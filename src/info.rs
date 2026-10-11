//! What Abyss asks about listed files, and how it reads the type and icon.
//!
//! Local folders are listed by name alone: `standard::fast-content-type`
//! and no `standard::icon`. Asking GIO for the full content type or for the
//! icon makes it open and read every file whose name doesn't settle its
//! type: every PNG, for one, which in a photo folder is thousands of reads
//! and three times the listing time. The icon comes from the type instead,
//! one shared per type. Files the name can't type (scripts without an
//! extension, say) are looked up afterwards; see `needs_type`.
//!
//! Remote locations keep the full set: their backends answer from what
//! they already know, and some of their icons (shares, servers) are their
//! own.

use std::cell::RefCell;
use std::collections::HashMap;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

macro_rules! common {
    () => {
        "standard::name,standard::display-name,standard::type,standard::size,\
         standard::is-hidden,standard::is-backup,standard::target-uri,\
         standard::is-symlink,time::modified,trash::orig-path,access::can-delete"
    };
}

const LOCAL: &str = concat!(common!(), ",standard::fast-content-type");
const REMOTE: &str = concat!(common!(), ",standard::content-type,standard::icon");

/// The full type, once looked up (see `needs_type`).
const FULL_TYPE: &str = "standard::content-type";
const FAST_TYPE: &str = "standard::fast-content-type";
/// Marks a file whose full type is being looked up.
const TYPING: &str = "abyss::typing";

/// The attributes to list `dir` with.
pub fn attributes(dir: &gio::File) -> &'static str {
    if dir.is_native() {
        LOCAL
    } else {
        REMOTE
    }
}

/// The file's content type: the full one when known, else the one its
/// name gives.
pub fn content_type(info: &gio::FileInfo) -> Option<glib::GString> {
    [FULL_TYPE, FAST_TYPE]
        .into_iter()
        .find(|key| info.has_attribute(key))
        .and_then(|key| info.attribute_string(key))
}

/// The file's icon: its own when the backend gave one, else its type's.
pub fn icon(info: &gio::FileInfo) -> gio::Icon {
    thread_local! {
        static BY_TYPE: RefCell<HashMap<String, gio::Icon>> = RefCell::default();
    }
    if info.has_attribute("standard::icon") {
        if let Some(icon) = info.icon() {
            return icon;
        }
    }
    let ct = content_type(info).map_or_else(String::new, |c| c.to_string());
    if let Some(icon) = BY_TYPE.with_borrow(|m| m.get(&ct).cloned()) {
        return icon;
    }
    let icon = if ct.is_empty() {
        gio::ThemedIcon::new("text-x-generic").upcast()
    } else {
        gio::content_type_get_icon(&ct)
    };
    BY_TYPE.with_borrow_mut(|m| m.insert(ct, icon.clone()));
    icon
}

/// Whether a listed file's name didn't settle its type and its contents
/// should be looked at: a non-empty regular file typed only as "some
/// bytes". True once per file; the caller then calls `set_full_type`.
pub fn needs_type(info: &gio::FileInfo) -> bool {
    let generic = !info.has_attribute(FULL_TYPE)
        && info.has_attribute(FAST_TYPE)
        && info.attribute_string(FAST_TYPE).as_deref() == Some("application/octet-stream")
        && info.file_type() == gio::FileType::Regular
        && crate::ops::size_of(info) > 0;
    if !generic || info.has_attribute(TYPING) {
        return false;
    }
    info.set_attribute_boolean(TYPING, true);
    true
}

/// Records the full type looked up for a file (and forgets its group,
/// which came from the type).
pub fn set_full_type(info: &gio::FileInfo, content_type: &str) {
    info.set_content_type(content_type);
    info.remove_attribute("abyss::group");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_icons() {
        let info = gio::FileInfo::new();
        info.set_file_type(gio::FileType::Regular);
        info.set_size(10);
        info.set_attribute_string(FAST_TYPE, "application/octet-stream");
        assert_eq!(
            content_type(&info).as_deref(),
            Some("application/octet-stream")
        );
        assert!(needs_type(&info));
        assert!(!needs_type(&info), "only asked once");
        set_full_type(&info, "application/x-shellscript");
        assert_eq!(
            content_type(&info).as_deref(),
            Some("application/x-shellscript")
        );
        assert!(!needs_type(&info));
        // Icons are shared per type.
        let a = gio::FileInfo::new();
        a.set_attribute_string(FAST_TYPE, "image/png");
        let b = gio::FileInfo::new();
        b.set_attribute_string(FAST_TYPE, "image/png");
        assert_eq!(icon(&a).as_ptr(), icon(&b).as_ptr());
        assert!(attributes(&gio::File::for_path("/")).contains(FAST_TYPE));
        assert!(attributes(&gio::File::for_uri("sftp://host/")).contains("standard::icon"));
    }
}
