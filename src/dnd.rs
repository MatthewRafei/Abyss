//! Drag-and-drop rules, from src/nautilus-dnd.c and nautilus-list-base.c.

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

/// Hover time before a folder under a drag opens (nautilus-dnd.h).
pub const HOVER_TIMEOUT_MS: u64 = 500;

fn is_trash(file: &gio::File) -> bool {
    file.has_uri_scheme("trash")
}

fn query(file: &gio::File, attrs: &str) -> Option<gio::FileInfo> {
    // Only local files are queried synchronously; drag callbacks can't wait.
    file.is_native()
        .then(|| {
            file.query_info(attrs, gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                .ok()
        })
        .flatten()
}

/// Whether `target` accepts drops at all, and what it would prefer.
/// Port of `nautilus_dnd_get_preferred_action()`; `sources` is None while
/// the dragged value hasn't arrived yet.
pub fn preferred_action(target: &gio::File, sources: Option<&[gio::File]>) -> gdk::DragAction {
    if let Some(sources) = sources {
        // Onto itself, or a folder into its own subtree.
        if sources
            .iter()
            .any(|s| target.equal(s) || target.has_prefix(s))
        {
            return gdk::DragAction::empty();
        }
    }
    if is_trash(target) {
        return gdk::DragAction::MOVE;
    }
    if target.has_uri_scheme("recent") || target.has_uri_scheme("network") {
        return gdk::DragAction::empty();
    }
    let target_info = query(target, "standard::type,access::can-write,id::filesystem");
    if let Some(info) = &target_info {
        let writable =
            !info.has_attribute("access::can-write") || info.boolean("access::can-write");
        if info.file_type() != gio::FileType::Directory || !writable {
            return gdk::DragAction::empty();
        }
    }
    let Some(first) = sources.and_then(|s| s.first()) else {
        return gdk::DragAction::COPY;
    };
    if is_trash(first) {
        return gdk::DragAction::MOVE;
    }
    // Same filesystem and deletable source: move, otherwise copy.
    let source_info = query(first, "id::filesystem,access::can-delete");
    let same_fs = match (&target_info, &source_info) {
        (Some(t), Some(s)) => {
            let a = t.attribute_string("id::filesystem");
            let b = s.attribute_string("id::filesystem");
            a.is_some() && a == b
        }
        _ => false,
    };
    let deletable = source_info
        .is_some_and(|s| !s.has_attribute("access::can-delete") || s.boolean("access::can-delete"));
    if same_fs && deletable {
        gdk::DragAction::MOVE
    } else {
        gdk::DragAction::COPY
    }
}

/// The action to carry out once dropped: what the compositor settled on
/// (modifier keys), falling back to our preference if it's ambiguous.
pub fn chosen_action(drop: Option<gdk::Drop>, preferred: gdk::DragAction) -> gdk::DragAction {
    let offered = drop.map_or(preferred, |d| d.actions());
    for action in [
        gdk::DragAction::MOVE,
        gdk::DragAction::COPY,
        gdk::DragAction::LINK,
    ] {
        if offered == action {
            return action;
        }
    }
    if offered.contains(preferred) {
        return preferred;
    }
    [
        gdk::DragAction::COPY,
        gdk::DragAction::MOVE,
        gdk::DragAction::LINK,
    ]
    .into_iter()
    .find(|a| offered.contains(*a))
    .unwrap_or(gdk::DragAction::COPY)
}

/// Value types every Abyss drop target takes.
pub fn drop_types() -> [glib::Type; 3] {
    [
        gdk::FileList::static_type(),
        gdk::Texture::static_type(),
        glib::Type::STRING,
    ]
}

/// Files carried by a drop value, if it is a file list.
pub fn value_files(value: &glib::Value) -> Option<Vec<gio::File>> {
    value.get::<gdk::FileList>().ok().map(|l| l.files())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let dir = std::env::temp_dir().join(format!("abyss-dnd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        let f = |p: &str| gio::File::for_path(dir.join(p));

        // Same filesystem: move. Onto itself / into own subtree: nothing.
        assert_eq!(
            preferred_action(&f("a"), Some(&[f("f.txt")])),
            gdk::DragAction::MOVE
        );
        assert!(preferred_action(&f("a"), Some(&[f("a")])).is_empty());
        assert!(preferred_action(&f("a/b"), Some(&[f("a")])).is_empty());
        // Files don't take drops; unknown payload copies; trash moves.
        assert!(preferred_action(&f("f.txt"), Some(&[f("a")])).is_empty());
        assert_eq!(preferred_action(&f("a"), None), gdk::DragAction::COPY);
        let trash = gio::File::for_uri("trash:///");
        assert_eq!(
            preferred_action(&trash, Some(&[f("f.txt")])),
            gdk::DragAction::MOVE
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn choosing() {
        use gdk::DragAction as A;
        assert_eq!(chosen_action(None, A::MOVE), A::MOVE);
    }
}
