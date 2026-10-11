//! Searching by name: the folder filter's matching, and a recursive search
//! under a folder. The recursive search asks the `LocalSearch` index first,
//! which answers at once for indexed folders, then walks the tree on all
//! cores, so files the index doesn't cover (or hasn't reached yet) still
//! turn up. Both stream their hits; the caller drops duplicates.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::par;

/// Hits a recursive search stops at; past this, typing more narrows better
/// than scrolling.
pub const MAX_RESULTS: usize = 5000;

/// How a search term is matched: lowercased once per keystroke.
pub fn fold(term: &str) -> String {
    term.to_lowercase()
}

/// Whether `name` contains `needle` (from `fold`), ignoring case. ASCII
/// names, by far the most common, are compared without allocating.
pub fn matches(name: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if name.is_ascii() {
        // A needle with non-ASCII bytes simply never matches here, which is
        // right: no ASCII name lowercases to it.
        let (hay, needle) = (name.as_bytes(), needle.as_bytes());
        return hay
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle));
    }
    name.to_lowercase().contains(needle)
}

/// Whether a recursive search can run here (it walks local folders only).
pub fn can_search(dir: &gio::File) -> bool {
    dir.is_native() && dir.path().is_some()
}

/// Files under `dir` (not `dir` itself) whose names contain `needle`, in
/// batches as they're found. `hidden` also searches hidden files and goes
/// into hidden folders. Other filesystems mounted below `dir` are skipped.
/// Dropping the future stops the search.
pub async fn run(dir: &gio::File, needle: &str, hidden: bool, mut found: impl FnMut(Vec<PathBuf>)) {
    let Some(root) = dir.path().filter(|_| dir.is_native()) else {
        return;
    };
    if needle.is_empty() {
        return;
    }
    let indexed = from_index(dir, needle, hidden).await;
    if !indexed.is_empty() {
        found(indexed);
    }

    let (tx, rx) = futures_channel::mpsc::unbounded::<PathBuf>();
    let needle = needle.to_owned();
    let walk = par::blocking(move |stop| {
        let Ok(dev) = std::fs::metadata(&root).map(|m| m.dev()) else {
            return;
        };
        let hits = AtomicUsize::new(0);
        // Only stops early (limit reached or cancelled); errors are skipped.
        let _ = par::walk(vec![root.clone()], false, stop, |path, md| {
            if path == root {
                return Ok(true);
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !hidden && (name.starts_with('.') || name.ends_with('~')) {
                return Ok(false);
            }
            if matches(&name, &needle) {
                if hits.fetch_add(1, Ordering::Relaxed) >= MAX_RESULTS {
                    stop.store(true, Ordering::Relaxed);
                    return Ok(false);
                }
                let _ = tx.unbounded_send(path.to_path_buf());
            }
            Ok(md.dev() == dev)
        });
    });
    let receive = rx.ready_chunks(512).for_each(|batch| {
        found(batch);
        std::future::ready(())
    });
    futures_util::future::join(walk, receive).await;
}

/// Hits from the `LocalSearch` index (empty when it isn't running, isn't
/// installed or doesn't cover `dir`). Runs `tinysparql`, passing the search
/// term as a query parameter rather than splicing it into the query.
async fn from_index(dir: &gio::File, needle: &str, hidden: bool) -> Vec<PathBuf> {
    if glib::find_program_in_path("tinysparql").is_none() {
        return Vec::new();
    }
    let prefix = format!("{}/", dir.uri().trim_end_matches('/'));
    let query = format!(
        "SELECT ?u WHERE {{ ?f a nfo:FileDataObject ; nie:url ?u ; nfo:fileName ?n . \
         FILTER(STRSTARTS(?u, ~prefix) && CONTAINS(LCASE(?n), ~needle)) }} LIMIT {MAX_RESULTS}"
    );
    let prefix_arg = format!("prefix:s:{prefix}");
    let needle_arg = format!("needle:s:{needle}");
    let argv = [
        "tinysparql",
        "query",
        "-b",
        "org.freedesktop.LocalSearch3",
        "-a",
        &prefix_arg,
        "-a",
        &needle_arg,
        "-q",
        &query,
    ];
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE;
    let Ok(process) = gio::Subprocess::newv(&argv, flags) else {
        return Vec::new();
    };
    let ask = process.communicate_utf8_future(None);
    let timeout = glib::timeout_future(std::time::Duration::from_secs(2));
    futures_util::pin_mut!(ask, timeout);
    let futures_util::future::Either::Left((Ok((Some(out), _)), _)) =
        futures_util::future::select(ask, timeout).await
    else {
        process.force_exit();
        return Vec::new();
    };
    parse_index(&out, needle, hidden)
}

/// File paths from `tinysparql query` output: a "Results:" line, then one
/// URI per line. The index lowercases differently from Rust in rare cases,
/// so every hit is checked again here.
fn parse_index(out: &str, needle: &str, hidden: bool) -> Vec<PathBuf> {
    out.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("file://"))
        .filter_map(|uri| glib::filename_from_uri(uri).ok().map(|(p, _)| p))
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            let shown = hidden
                || !p.iter().any(|c| {
                    let c = c.to_string_lossy();
                    c.starts_with('.') || c.ends_with('~')
                });
            shown && matches(&name, needle)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let n = fold("ReAd");
        assert!(matches("README.md", &n));
        assert!(matches("thread", &n));
        assert!(!matches("rea", &n));
        assert!(matches("anything", ""));
        // Non-ASCII on either side.
        assert!(matches("Über.txt", &fold("üb")));
        assert!(!matches("uber.txt", &fold("üb")));
        assert!(matches("ÄRGER", &fold("ärg")));
    }

    #[test]
    fn index_output() {
        let out = "Results:\n  file:///home/u/Notes/read%20me.txt\n  file:///home/u/.cache/readme\n  file:///home/u/other\n";
        let hits = parse_index(out, "read", false);
        assert_eq!(hits, [PathBuf::from("/home/u/Notes/read me.txt")]);
        assert_eq!(parse_index(out, "read", true).len(), 2);
        assert!(parse_index("Results:\n  None\n", "x", true).is_empty());
    }

    #[test]
    fn walks_and_streams() {
        let dir = std::env::temp_dir().join(format!("abyss-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b/.hid")).unwrap();
        std::fs::write(dir.join("a/b/Report.txt"), "").unwrap();
        std::fs::write(dir.join("a/report-2.txt"), "").unwrap();
        std::fs::write(dir.join("a/b/.hid/report-3.txt"), "").unwrap();
        std::fs::write(dir.join("a/other.txt"), "").unwrap();
        let ctx = glib::MainContext::new();
        let found = |hidden| {
            let mut all = Vec::new();
            ctx.with_thread_default(|| {
                ctx.block_on(run(&gio::File::for_path(&dir), "report", hidden, |b| {
                    all.extend(b);
                }));
            })
            .unwrap();
            all.sort();
            all
        };
        assert_eq!(
            found(false),
            [dir.join("a/b/Report.txt"), dir.join("a/report-2.txt")]
        );
        assert_eq!(found(true).len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
