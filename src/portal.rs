//! The xdg-desktop-portal `FileChooser` backend: other apps' Open and Save
//! dialogs (browsers, sandboxed apps) become an Abyss window in pick mode
//! (`window/pick.rs`). Follows xdg-desktop-portal-gtk's filechooser.c.
//! install.sh installs the `.portal` and service files that start Abyss for
//! it; the portal frontend's config chooses it for `FileChooser`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::window::Win;

const NAME: &str = "org.freedesktop.impl.portal.desktop.abyss";
const PATH: &str = "/org/freedesktop/portal/desktop";
const CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";
const REQUEST: &str = "org.freedesktop.impl.portal.Request";
const XML: &str = r#"<node>
  <interface name="org.freedesktop.impl.portal.FileChooser">
    <method name="OpenFile">
      <arg type="o" name="handle" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/>
      <arg type="a{sv}" name="results" direction="out"/>
    </method>
    <method name="SaveFile">
      <arg type="o" name="handle" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/>
      <arg type="a{sv}" name="results" direction="out"/>
    </method>
    <method name="SaveFiles">
      <arg type="o" name="handle" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/>
      <arg type="a{sv}" name="results" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.impl.portal.Request">
    <method name="Close"/>
  </interface>
</node>"#;

/// The portal's response codes.
const SUCCESS: u32 = 0;
const CANCELLED: u32 = 1;
const OTHER: u32 = 2;

/// What the asking app wants picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Files to open, or with `directory` a folder.
    Open { multiple: bool, directory: bool },
    /// A name to save one file under.
    Save { name: String },
    /// A folder to save these files into.
    SaveFiles { names: Vec<String> },
}

/// One of the asking app's file type filters: shell globs and MIME types.
pub struct Filter {
    pub name: String,
    /// Lowercased.
    globs: Vec<String>,
    mimes: Vec<String>,
    /// As the app sent it, to report back which one was in use.
    raw: glib::Variant,
}

impl Filter {
    /// A filter from its `(sa(us))` form; type 0 is a glob, 1 a MIME type.
    fn from_variant(raw: &glib::Variant) -> Option<Self> {
        let (name, rules) = raw.get::<(String, Vec<(u32, String)>)>()?;
        let mut globs = Vec::new();
        let mut mimes = Vec::new();
        for (kind, pattern) in rules {
            match kind {
                0 => globs.push(pattern.to_lowercase()),
                1 => mimes.push(pattern),
                _ => {}
            }
        }
        Some(Self {
            name,
            globs,
            mimes,
            raw: raw.clone(),
        })
    }

    /// Whether a file of this name and content type passes. Globs ignore
    /// case, as GTK's file chooser does.
    pub fn matches(&self, name: &str, content_type: Option<&str>) -> bool {
        let lower = name.to_lowercase();
        self.globs.iter().any(|g| glob_matches(g, &lower))
            || content_type.is_some_and(|ct| {
                self.mimes
                    .iter()
                    .any(|m| gio::content_type_is_mime_type(ct, m))
            })
    }
}

/// Shell glob match with `*` and `?`, the patterns file choosers get
/// (`*.png`, `*.tar.gz`); anything else matches itself.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and how much of the name it has taken.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi += 1;
            }
            Some(&c) if c == '?' || c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                // Let the `*` take one more character and try again.
                Some((sp, sn)) => {
                    star = Some((sp, sn + 1));
                    pi = sp + 1;
                    ni = sn + 1;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// A request from another app, as the pick window needs it.
pub struct Request {
    pub kind: Kind,
    pub title: String,
    /// The accept button's label, when the app names one.
    pub accept_label: Option<String>,
    pub folder: Option<gio::File>,
    pub filters: Vec<Rc<Filter>>,
    /// Index into `filters` of the one to start with.
    pub current_filter: usize,
    /// The app's window ("wayland:HANDLE"), to open as its dialog.
    pub parent: String,
}

/// What the user chose: the files, and the filter shown at the time.
pub struct Chosen {
    pub files: Vec<gio::File>,
    pub filter: Option<Rc<Filter>>,
}

/// Hands the chosen files (None: cancelled) back to the asking app; only
/// the first call counts.
pub type Reply = Rc<dyn Fn(Option<Chosen>)>;

/// Serves the `FileChooser` backend on the app's bus connection.
pub fn register(app: &gtk::Application) {
    let Some(connection) = app.dbus_connection() else {
        return;
    };
    let Some(node) = gio::DBusNodeInfo::for_xml(XML).ok() else {
        return;
    };
    let (Some(chooser), Some(request)) = (
        node.lookup_interface(CHOOSER),
        node.lookup_interface(REQUEST),
    ) else {
        return;
    };
    let app = app.downgrade();
    let conn = connection.clone();
    let registered = connection
        .register_object(PATH, &chooser)
        .method_call(move |_, _, _, _, method, params, invocation| {
            let Some(app) = app.upgrade() else {
                return invocation.return_value(Some(&response(OTHER, None)));
            };
            let Some(req) = parse(method, &params) else {
                return invocation.return_error(gio::DBusError::InvalidArgs, "Bad arguments");
            };
            let handle = params.child_value(0).str().unwrap_or_default().to_owned();
            serve(&app, &conn, &request, &handle, req, invocation);
        })
        .build();
    if let Err(e) = registered {
        glib::g_warning!("abyss", "FileChooser portal not served: {e}");
        return;
    }
    // Owned for as long as the app runs.
    let _owner = gio::bus_own_name_on_connection(
        &connection,
        NAME,
        gio::BusNameOwnerFlags::NONE,
        |_, _| {},
        |_, _| {},
    );
}

/// Opens the pick window for `req` and answers `invocation` once the user
/// is done, or once the frontend closes the request at `handle`.
fn serve(
    app: &gtk::Application,
    connection: &gio::DBusConnection,
    request_iface: &gio::DBusInterfaceInfo,
    handle: &str,
    req: Request,
    invocation: gio::DBusMethodInvocation,
) {
    // Kept until answered; the app stays up meanwhile.
    let pending = Rc::new(RefCell::new(Some((invocation, app.hold()))));
    let close_id: Rc<RefCell<Option<gio::RegistrationId>>> = Rc::default();
    let conn = connection.clone();
    let id = close_id.clone();
    let reply: Reply = Rc::new(move |chosen: Option<Chosen>| {
        let Some((invocation, _hold)) = pending.take() else {
            return;
        };
        if let Some(id) = id.take() {
            let _ = conn.unregister_object(id);
        }
        let answer = chosen.map_or_else(
            || response(CANCELLED, None),
            |chosen| response(SUCCESS, Some(&chosen)),
        );
        invocation.return_value(Some(&answer));
    });
    let window = Win::pick(app, req, reply.clone());
    // The frontend's Close (the app gave up): close the window, which
    // answers "cancelled" if nothing was chosen yet.
    let window = window.downgrade();
    let registered = connection
        .register_object(handle, request_iface)
        .method_call(move |_, _, _, _, _, _, invocation| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
            invocation.return_value(None);
        })
        .build();
    match registered {
        Ok(reg) => {
            close_id.replace(Some(reg));
        }
        Err(e) => glib::g_warning!("abyss", "Request {handle} not served: {e}"),
    }
}

/// `(u a{sv})`: the response code, and the chosen URIs and filter.
fn response(code: u32, chosen: Option<&Chosen>) -> glib::Variant {
    let results = glib::VariantDict::new(None);
    if let Some(chosen) = chosen {
        let uris: Vec<String> = chosen.files.iter().map(local_uri).collect();
        results.insert_value("uris", &uris.to_variant());
        if let Some(filter) = &chosen.filter {
            results.insert_value("current_filter", &filter.raw);
        }
    }
    // Child by child: a Variant inside a Rust tuple would be boxed (`v`).
    glib::Variant::tuple_from_iter([code.to_variant(), results.end()])
}

/// A `file://` URI where the file has a local path (GVfs shares have one
/// through FUSE), so apps that only take paths can use it.
fn local_uri(file: &gio::File) -> String {
    file.path().map_or_else(
        || file.uri().to_string(),
        |path| gio::File::for_path(path).uri().to_string(),
    )
}

/// The request from a method's `(o s s s a{sv})` arguments.
fn parse(method: &str, params: &glib::Variant) -> Option<Request> {
    if params.n_children() != 5 {
        return None;
    }
    let parent = params.child_value(2).str()?.to_owned();
    let title = params.child_value(3).str()?.to_owned();
    // Checked first: the dictionary asserts its type, and a panic aborts.
    let options = params.child_value(4);
    if options.type_() != glib::VariantDict::static_variant_type() {
        return None;
    }
    let options = glib::VariantDict::new(Some(&options));
    let flag = |key: &str| options.lookup::<bool>(key).ok().flatten().unwrap_or(false);
    let text = |key: &str| options.lookup::<String>(key).ok().flatten();
    let path = |key: &str| {
        options
            .lookup::<Vec<u8>>(key)
            .ok()
            .flatten()
            .map(bytes_path)
    };

    let current_file = path("current_file");
    let kind = match method {
        "OpenFile" => Kind::Open {
            multiple: flag("multiple"),
            directory: flag("directory"),
        },
        "SaveFile" => Kind::Save {
            name: text("current_name")
                .or_else(|| {
                    current_file
                        .as_ref()
                        .and_then(|f| f.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
        },
        "SaveFiles" => Kind::SaveFiles {
            names: options
                .lookup::<Vec<Vec<u8>>>("files")
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|b| {
                    let p = bytes_path(b);
                    p.file_name().map(|n| n.to_string_lossy().into_owned())
                })
                .collect(),
        },
        _ => return None,
    };
    // The folder: as given, else the current file's.
    let folder = path("current_folder")
        .or_else(|| current_file.and_then(|f| f.parent().map(PathBuf::from)))
        .filter(|p| p.is_dir())
        .map(gio::File::for_path);

    let filters: Vec<Rc<Filter>> = options
        .lookup_value("filters", None)
        .map(|list| {
            list.iter()
                .filter_map(|f| Filter::from_variant(&f).map(Rc::new))
                .collect()
        })
        .unwrap_or_default();
    // The current filter is matched by name; one that isn't in the list
    // (allowed by the spec) is added to it.
    let mut filters = filters;
    let current_filter = options
        .lookup_value("current_filter", None)
        .and_then(|v| Filter::from_variant(&v))
        .map_or(0, |current| {
            filters
                .iter()
                .position(|f| f.name == current.name)
                .unwrap_or_else(|| {
                    filters.push(Rc::new(current));
                    filters.len() - 1
                })
        });
    Some(Request {
        kind,
        title,
        accept_label: text("accept_label").filter(|l| !l.is_empty()),
        folder,
        filters,
        current_filter,
        parent,
    })
}

/// A path sent as bytes, which the portal ends with a NUL.
fn bytes_path(mut bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(name: &str, rules: &[(u32, &str)]) -> glib::Variant {
        let rules: Vec<(u32, String)> = rules.iter().map(|(k, p)| (*k, (*p).to_owned())).collect();
        (name.to_owned(), rules).to_variant()
    }

    fn params(options: &glib::VariantDict) -> glib::Variant {
        let handle = glib::variant::ObjectPath::try_from("/org/freedesktop/portal/desktop/t")
            .unwrap_or_else(|_| unreachable!());
        // Built child by child: a Variant inside a Rust tuple would be boxed.
        glib::Variant::tuple_from_iter([
            handle.to_variant(),
            "firefox".to_variant(),
            "wayland:abc".to_variant(),
            "Save Image".to_variant(),
            options.end(),
        ])
    }

    #[test]
    fn filters_match_globs_and_types() {
        let images = Filter::from_variant(&filter("Images", &[(0, "*.PNG"), (1, "image/*")]));
        let Some(images) = images else {
            panic!("filter not parsed");
        };
        assert!(images.matches("shot.png", None));
        assert!(images.matches("SHOT.Png", None));
        assert!(images.matches("photo", Some("image/jpeg")));
        assert!(!images.matches("notes.txt", Some("text/plain")));
    }

    #[test]
    fn parses_a_save_request() {
        let options = glib::VariantDict::new(None);
        options.insert_value("current_name", &"cat.png".to_variant());
        options.insert_value("current_folder", &b"/tmp\0".to_vec().to_variant());
        options.insert_value("accept_label", &"_Save".to_variant());
        let png = filter("PNG", &[(0, "*.png")]);
        let all = filter("All files", &[(0, "*")]);
        let filters =
            glib::Variant::array_from_iter_with_type(png.type_(), [png.clone(), all.clone()]);
        options.insert_value("filters", &filters);
        options.insert_value("current_filter", &all);
        let Some(req) = parse("SaveFile", &params(&options)) else {
            panic!("request not parsed");
        };
        assert_eq!(
            req.kind,
            Kind::Save {
                name: "cat.png".into()
            }
        );
        assert_eq!(req.title, "Save Image");
        assert_eq!(req.parent, "wayland:abc");
        assert_eq!(req.accept_label.as_deref(), Some("_Save"));
        assert_eq!(
            req.folder.and_then(|f| f.path()),
            Some(PathBuf::from("/tmp"))
        );
        assert_eq!(req.filters.len(), 2);
        assert_eq!(req.current_filter, 1);
    }

    #[test]
    fn parses_open_flags_and_answers() {
        let options = glib::VariantDict::new(None);
        options.insert_value("multiple", &true.to_variant());
        let Some(req) = parse("OpenFile", &params(&options)) else {
            panic!("request not parsed");
        };
        assert_eq!(
            req.kind,
            Kind::Open {
                multiple: true,
                directory: false
            }
        );
        assert!(parse("Frobnicate", &params(&options)).is_none());

        let chosen = Chosen {
            files: vec![gio::File::for_path("/tmp/a b.png")],
            filter: None,
        };
        let answer = response(SUCCESS, Some(&chosen));
        assert_eq!(answer.type_().as_str(), "(ua{sv})");
        assert_eq!(answer.child_value(0).get::<u32>(), Some(SUCCESS));
        let results = glib::VariantDict::new(Some(&answer.child_value(1)));
        assert_eq!(
            results.lookup::<Vec<String>>("uris").ok().flatten(),
            Some(vec!["file:///tmp/a%20b.png".to_owned()])
        );
        let cancelled = response(CANCELLED, None);
        assert_eq!(cancelled.child_value(0).get::<u32>(), Some(CANCELLED));
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*.png", "a.png"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("*.tar.gz", "x.y.tar.gz"));
        assert!(glob_matches("img_??.jpg", "img_01.jpg"));
        assert!(!glob_matches("*.png", "a.png.txt"));
        assert!(!glob_matches("img_?.jpg", "img_01.jpg"));
    }

    #[test]
    fn strips_the_trailing_nul() {
        assert_eq!(bytes_path(b"/home/x\0".to_vec()), PathBuf::from("/home/x"));
    }
}
