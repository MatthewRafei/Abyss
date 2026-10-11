//! Repro: cancel an extraction mid-way the way the Cancel button does.
//! Destination from `ABYSS_CANCEL_DEST` (defaults to a temp dir).

#[cfg(test)]
mod tests {
    use crate::archive;
    use crate::ops;
    use gtk::gio;
    use gtk::glib;
    use std::rc::Rc;

    #[test]
    #[ignore = "manual repro; set ABYSS_CANCEL_DEST to test on a share"]
    fn cancel_mid_extract() {
        let work = std::env::temp_dir().join(format!("abyss-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(work.join("src")).unwrap();
        // Many files so extraction takes a while.
        for i in 0..4000 {
            std::fs::write(work.join(format!("src/f{i}.bin")), vec![7u8; 64 * 1024]).unwrap();
        }
        let tar = work.join("big.tar");
        assert!(std::process::Command::new("bsdtar")
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(work.join("src"))
            .arg(".")
            .status()
            .unwrap()
            .success());
        let dest = std::env::var("ABYSS_CANCEL_DEST")
            .map_or_else(|_| work.join("out"), std::path::PathBuf::from);
        std::fs::create_dir_all(&dest).unwrap();

        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            let report: ops::Report =
                Rc::new(|p| eprintln!("progress {}/{}", p.done_files, p.total_files));
            let archive_file = gio::File::for_path(&tar);
            let dest_file = gio::File::for_path(&dest);
            let handle = glib::spawn_future_local(async move {
                let r = archive::extract(&archive_file, &dest_file, None, &report).await;
                eprintln!("finished: ok={}", r.is_ok());
            });
            // Let it run, then cancel like Win::cancel_job.
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(
                    std::env::var("ABYSS_CANCEL_MS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(150),
                );
            while std::time::Instant::now() < deadline {
                ctx.iteration(false);
            }
            eprintln!("aborting");
            let t = std::time::Instant::now();
            handle.abort();
            eprintln!("abort blocked for {:?}", t.elapsed());
            let t = std::time::Instant::now();
            for _ in 0..200 {
                ctx.iteration(false);
            }
            eprintln!("main loop after abort took {:?}", t.elapsed());
        })
        .unwrap();
        let t = std::time::Instant::now();
        while std::fs::read_dir(&dest).map_or(0, Iterator::count) > 0 && t.elapsed().as_secs() < 120
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        eprintln!("background cleanup took {:?}", t.elapsed());
        let leftovers: Vec<_> = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        eprintln!("left in dest: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&work);
    }
}
