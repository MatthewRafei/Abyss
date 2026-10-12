//! Preferences, kept as `key=value` lines in `~/.config/abyss/settings`
//! and read once at startup (the Preferences window writes them back).

use std::cell::RefCell;
use std::path::PathBuf;

use gtk::glib;

// Independent on/off settings: a bool each is what they are.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Prefs {
    pub show_hidden: bool,
    pub folders_first: bool,
    pub thumbnails: bool,
    pub restore_tabs: bool,
    /// Command for "Open in Terminal"; empty picks one ($TERMINAL first).
    pub terminal: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            show_hidden: false,
            folders_first: true,
            thumbnails: true,
            restore_tabs: true,
            terminal: String::new(),
        }
    }
}

fn file() -> PathBuf {
    glib::user_config_dir().join("abyss").join("settings")
}

thread_local! {
    static CURRENT: RefCell<Option<Prefs>> = const { RefCell::new(None) };
}

fn parse(text: &str) -> Prefs {
    let mut prefs = Prefs::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let on = value == "true";
        match key {
            "show_hidden" => prefs.show_hidden = on,
            "folders_first" => prefs.folders_first = on,
            "thumbnails" => prefs.thumbnails = on,
            "restore_tabs" => prefs.restore_tabs = on,
            "terminal" => value.clone_into(&mut prefs.terminal),
            _ => {}
        }
    }
    prefs
}

fn render(prefs: &Prefs) -> String {
    format!(
        "show_hidden={}\nfolders_first={}\nthumbnails={}\nrestore_tabs={}\nterminal={}\n",
        prefs.show_hidden,
        prefs.folders_first,
        prefs.thumbnails,
        prefs.restore_tabs,
        prefs.terminal
    )
}

/// The preferences in effect.
pub fn get() -> Prefs {
    CURRENT.with_borrow_mut(|current| {
        current
            .get_or_insert_with(|| {
                std::fs::read_to_string(file()).map_or_else(|_| Prefs::default(), |t| parse(&t))
            })
            .clone()
    })
}

/// Changes and saves the preferences.
pub fn set(prefs: Prefs) -> std::io::Result<()> {
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, render(&prefs))?;
    CURRENT.with_borrow_mut(|current| *current = Some(prefs));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let prefs = Prefs {
            show_hidden: true,
            folders_first: false,
            thumbnails: false,
            restore_tabs: true,
            terminal: "foot -e".into(),
        };
        assert_eq!(parse(&render(&prefs)), prefs);
        // Unknown and missing keys fall back to the defaults.
        assert_eq!(parse("junk\nnope=1\n"), Prefs::default());
    }
}
