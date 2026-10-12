//! Starred files: a list of locations in `~/.config/abyss/starred`, one URI
//! per line, shown together under Starred (`starred:///`) wherever they are.

use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

/// Where Starred is browsed.
pub const VIEW: &str = "starred:///";

fn file() -> PathBuf {
    glib::user_config_dir().join("abyss").join("starred")
}

/// Whether `location` is the Starred view.
pub fn is_view(location: &gio::File) -> bool {
    location.has_uri_scheme("starred")
}

fn read(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn write(path: &Path, uris: &[String]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = uris.join("\n");
    text.push('\n');
    std::fs::write(path, text)
}

/// Starred locations, first starred first.
pub fn all() -> Vec<String> {
    read(&file())
}

pub fn is_starred(location: &gio::File) -> bool {
    let uri = location.uri();
    all().iter().any(|u| *u == uri.as_str())
}

/// Stars `files` (those not starred already).
pub fn star(files: &[gio::File]) -> std::io::Result<()> {
    star_in(&file(), files)
}

fn star_in(path: &Path, files: &[gio::File]) -> std::io::Result<()> {
    let mut uris = read(path);
    for f in files {
        let uri = f.uri().to_string();
        if !uris.contains(&uri) {
            uris.push(uri);
        }
    }
    write(path, &uris)
}

pub fn unstar(files: &[gio::File]) -> std::io::Result<()> {
    unstar_in(&file(), files)
}

fn unstar_in(path: &Path, files: &[gio::File]) -> std::io::Result<()> {
    let gone: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
    let uris: Vec<String> = read(path)
        .into_iter()
        .filter(|u| !gone.contains(u))
        .collect();
    write(path, &uris)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_unstar() {
        let dir = std::env::temp_dir().join(format!("abyss-starred-{}", std::process::id()));
        let path = dir.join("starred");
        let f = |p: &str| gio::File::for_path(format!("/tmp/{p}"));
        star_in(&path, &[f("a"), f("b")]).unwrap();
        star_in(&path, &[f("b"), f("c")]).unwrap();
        assert_eq!(
            read(&path),
            ["file:///tmp/a", "file:///tmp/b", "file:///tmp/c"]
        );
        unstar_in(&path, &[f("b")]).unwrap();
        assert_eq!(read(&path), ["file:///tmp/a", "file:///tmp/c"]);
        assert!(is_view(&gio::File::for_uri(VIEW)));
        assert!(!is_view(&f("a")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
