//! Extracting archives with bsdtar (libarchive), following Nautilus'
//! extract job (nautilus-file-operations.c, `extract_job_on_decide_destination`):
//! an archive with one top-level item puts that item next to the archive,
//! anything else goes into a folder named after the archive. Nothing is
//! overwritten; clashes get "(2)" names.
//!
//! bsdtar's defaults keep entries inside the target (no absolute paths, no
//! `..`, no writing through symlinks), and extraction happens in a hidden
//! temporary folder that is removed if the job fails or is cancelled.

use std::path::PathBuf;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::filename;
use crate::ops;

/// Types libarchive reads (as gnome-autoar's supported list), matched with
/// `content_type_is_a` so aliases count too.
const TYPES: &[&str] = &[
    "application/zip",
    "application/x-7z-compressed",
    "application/vnd.rar",
    "application/x-rar",
    "application/x-tar",
    "application/x-compressed-tar",
    "application/x-bzip-compressed-tar",
    "application/x-bzip2-compressed-tar",
    "application/x-xz-compressed-tar",
    "application/x-zstd-compressed-tar",
    "application/x-lzma-compressed-tar",
    "application/x-lz4-compressed-tar",
    "application/x-lzip-compressed-tar",
    "application/x-tarz",
    "application/x-cpio",
    "application/x-iso9660-image",
    "application/x-cd-image",
    "application/x-archive",
    "application/vnd.debian.binary-package",
    "application/x-rpm",
    "application/x-xar",
    "application/vnd.ms-cab-compressed",
    "application/java-archive",
];

pub fn is_archive(info: &gio::FileInfo) -> bool {
    crate::info::content_type(info).is_some_and(|ct| is_archive_type(&ct))
}

/// Whether a content type is one of the archive types bsdtar reads.
pub fn is_archive_type(content_type: &str) -> bool {
    TYPES
        .iter()
        .any(|t| gio::content_type_is_a(content_type, t))
}

pub fn have_bsdtar() -> bool {
    glib::find_program_in_path("bsdtar").is_some()
}

pub enum Error {
    /// The archive is encrypted and needs (another) passphrase.
    NeedsPassphrase,
    Failed(glib::Error),
}

impl From<glib::Error> for Error {
    fn from(e: glib::Error) -> Self {
        Self::Failed(e)
    }
}

fn fail(msg: impl AsRef<str>) -> Error {
    Error::Failed(glib::Error::new(gio::IOErrorEnum::Failed, msg.as_ref()))
}

/// Kills bsdtar and removes the temporary folder unless disarmed, so a
/// cancelled job (its future dropped) leaves nothing behind.
struct Cleanup {
    process: Option<gio::Subprocess>,
    tmp: PathBuf,
    armed: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let pid = self.process.take().and_then(|p| {
            p.force_exit();
            p.identifier().and_then(|id| id.parse::<u32>().ok())
        });
        if !self.armed {
            return;
        }
        // Deleting what was extracted can take minutes on a network share
        // (one round trip per file); doing it here froze the window. A
        // thread removes it once bsdtar is gone, so it isn't still writing.
        let tmp = std::mem::take(&mut self.tmp);
        std::thread::spawn(move || {
            if let Some(pid) = pid {
                wait_for_exit(pid);
            }
            let _ = std::fs::remove_dir_all(&tmp);
        });
    }
}

/// Waits (up to 5 s) until `pid` has exited. A zombie counts as exited:
/// GLib reaps it on the main loop, which this thread doesn't wait for.
fn wait_for_exit(pid: u32) {
    for _ in 0..50 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // Field 3 (after the parenthesised name) is the state.
        let state = stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next());
        if matches!(state, None | Some("Z" | "X")) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Runs bsdtar with `args`, calling `line` for each stderr line; returns
/// the whole stderr on failure.
async fn bsdtar(
    args: &[&std::ffi::OsStr],
    cleanup: &mut Cleanup,
    mut line: impl FnMut(&str),
) -> Result<(), String> {
    let mut command: Vec<&std::ffi::OsStr> = vec!["bsdtar".as_ref()];
    command.extend_from_slice(args);
    let process = gio::Subprocess::newv(
        &command,
        gio::SubprocessFlags::STDERR_PIPE
            | gio::SubprocessFlags::STDOUT_SILENCE
            | gio::SubprocessFlags::STDIN_PIPE,
    )
    .map_err(|e| e.to_string())?;
    cleanup.process = Some(process.clone());
    let mut errors = String::new();
    if let Some(pipe) = process.stderr_pipe() {
        let reader = gio::DataInputStream::new(&pipe);
        while let Ok(Some(text)) = reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
            line(&text);
            if !text.starts_with("x ") {
                errors.push_str(&text);
                errors.push('\n');
            }
        }
    }
    let ok = process.wait_future().await.is_ok() && process.is_successful();
    cleanup.process = None;
    if ok {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Number of entries, for the progress bar. Listing also fails early,
/// with a clear message, on an encrypted archive without a passphrase.
async fn count(
    archive: &std::path::Path,
    passphrase: Option<&str>,
    cleanup: &mut Cleanup,
) -> Result<u64, Error> {
    let mut argv: Vec<&std::ffi::OsStr> = vec![
        "bsdtar".as_ref(),
        "-t".as_ref(),
        "-f".as_ref(),
        archive.as_os_str(),
    ];
    if let Some(p) = passphrase {
        argv.extend([
            std::ffi::OsStr::new("--passphrase"),
            std::ffi::OsStr::new(p),
        ]);
    }
    // stdin is a pipe so bsdtar can't prompt on a terminal for a password.
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE
            | gio::SubprocessFlags::STDERR_PIPE
            | gio::SubprocessFlags::STDIN_PIPE,
    )?;
    cleanup.process = Some(process.clone());
    let (out, err) = process.communicate_utf8_future(None).await?;
    cleanup.process = None;
    if !process.is_successful() {
        return Err(classify(err.as_deref().unwrap_or_default()));
    }
    Ok(out.map_or(0, |o| o.lines().count() as u64))
}

fn classify(stderr: &str) -> Error {
    let lower = stderr.to_lowercase();
    if lower.contains("passphrase") || lower.contains("encrypted") || lower.contains("password") {
        Error::NeedsPassphrase
    } else {
        fail(
            stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Extraction failed")
                .trim(),
        )
    }
}

/// Extracts `archive` into `dest`, returning what was created.
pub async fn extract(
    archive: &gio::File,
    dest: &gio::File,
    passphrase: Option<&str>,
    report: &ops::Report,
) -> Result<gio::File, Error> {
    let Some(archive_path) = archive.path() else {
        return Err(fail(
            "Extracting needs the archive on a local or mounted path",
        ));
    };
    let Some(dest_path) = dest.path() else {
        return Err(fail("Extracting needs a local or mounted destination"));
    };
    let name = ops::display_name(archive);
    let tmp = dest_path.join(format!(
        ".abyss-extract-{}-{}",
        std::process::id(),
        glib::monotonic_time()
    ));
    std::fs::create_dir(&tmp)
        .map_err(|e| fail(format!("Couldn't create a temporary folder: {e}")))?;
    let mut cleanup = Cleanup {
        process: None,
        tmp: tmp.clone(),
        armed: true,
    };

    let total = count(&archive_path, passphrase, &mut cleanup).await?;
    let mut progress = ops::Progress {
        total_files: total,
        current: name.clone(),
        ..Default::default()
    };
    report(&progress);

    let mut args: Vec<&std::ffi::OsStr> = vec![
        "-x".as_ref(),
        "-v".as_ref(),
        "--no-same-owner".as_ref(),
        "-f".as_ref(),
        archive_path.as_os_str(),
        "-C".as_ref(),
        tmp.as_os_str(),
    ];
    if let Some(p) = passphrase {
        args.extend([
            std::ffi::OsStr::new("--passphrase"),
            std::ffi::OsStr::new(p),
        ]);
    }
    let result = bsdtar(&args, &mut cleanup, |line| {
        if let Some(entry) = line.strip_prefix("x ") {
            progress.done_files += 1;
            entry
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(entry)
                .clone_into(&mut progress.current);
            report(&progress);
        }
    })
    .await;
    if let Err(stderr) = result {
        return Err(classify(&stderr));
    }

    settle(&tmp, dest, &name, &mut cleanup).await
}

/// Moves what was extracted into `tmp` to its place in `dest`
/// (`extract_job_on_decide_destination`): a single top-level item goes
/// there directly, several go into a folder named after the archive.
async fn settle(
    tmp: &std::path::Path,
    dest: &gio::File,
    name: &str,
    cleanup: &mut Cleanup,
) -> Result<gio::File, Error> {
    let tmp_file = gio::File::for_path(tmp);
    let mut entries = std::fs::read_dir(tmp)
        .map_err(|e| fail(e.to_string()))?
        .filter_map(Result::ok)
        .map(|e| e.file_name());
    let first = entries.next();
    let single = entries.next().is_none();
    let (item, target) = match (first, single) {
        (None, _) => return Err(fail(format!("“{name}” is empty"))),
        (Some(only), true) => {
            let item = tmp_file.child(&only);
            let is_dir = item.query_file_type(
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                gio::Cancellable::NONE,
            ) == gio::FileType::Directory;
            let target = ops::free_target(dest, &only.to_string_lossy(), is_dir, false).await;
            (item, target)
        }
        (Some(_), false) => {
            let stem = &name[..filename::extension_start(name)];
            let stem = if stem.is_empty() { name } else { stem };
            (tmp_file, ops::free_target(dest, stem, true, false).await)
        }
    };
    let (mv, _) = item.move_future(
        &target,
        gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
        glib::Priority::DEFAULT,
    );
    mv.await?;
    // Either the folder itself moved, or its only item did and it's empty.
    let _ = std::fs::remove_dir(tmp);
    cleanup.armed = false;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn run(fut: impl std::future::Future<Output = ()>) {
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| ctx.block_on(fut)).unwrap();
    }

    #[test]
    fn extract_rules() {
        if !have_bsdtar() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("abyss-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/top")).unwrap();
        std::fs::write(dir.join("src/top/a.txt"), "a").unwrap();
        std::fs::write(dir.join("src/b.txt"), "b").unwrap();
        let tar = |name: &str, items: &[&str]| {
            let mut cmd = std::process::Command::new("bsdtar");
            cmd.arg("-a")
                .arg("-cf")
                .arg(dir.join(name))
                .arg("-C")
                .arg(dir.join("src"))
                .args(items);
            assert!(cmd.status().unwrap().success());
        };
        tar("one.zip", &["top"]);
        tar("many.tar.gz", &["top", "b.txt"]);
        let quiet: ops::Report = Rc::new(|_| {});
        let f = |p: &str| gio::File::for_path(dir.join(p));
        run(async {
            // One top-level item lands directly.
            let made = extract(&f("one.zip"), &f(""), None, &quiet)
                .await
                .ok()
                .unwrap();
            assert_eq!(ops::display_name(&made), "top");
            assert_eq!(std::fs::read_to_string(dir.join("top/a.txt")).unwrap(), "a");
            // Again: no overwrite, "(2)".
            let made = extract(&f("one.zip"), &f(""), None, &quiet)
                .await
                .ok()
                .unwrap();
            assert_eq!(ops::display_name(&made), "top (2)");
            // Several items go into a folder named after the archive.
            let made = extract(&f("many.tar.gz"), &f(""), None, &quiet)
                .await
                .ok()
                .unwrap();
            assert_eq!(ops::display_name(&made), "many");
            assert!(dir.join("many/b.txt").exists() && dir.join("many/top/a.txt").exists());
            // No temporary folders left behind.
            let leftovers = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".abyss-extract")
                });
            assert!(!leftovers);
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
