//! The file chooser portal backend: with `--portal`, Abyss serves
//! `org.freedesktop.impl.portal.FileChooser` as
//! `org.freedesktop.impl.portal.desktop.abyss`, so Open / Save dialogs of
//! apps that use the portal (Chrome, Firefox, GTK with `GDK_DEBUG=portals`,
//! sandboxed apps) are Abyss windows (`window/picker.rs`). D-Bus starts it
//! when a dialog is wanted; it exits a minute after the last one closes.
//!
//! Interface: xdg-desktop-portal's
//! data/org.freedesktop.impl.portal.FileChooser.xml.

use std::cell::Cell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::window::picker::{Choice, Filter, Outcome, Purpose, Request};
use crate::window::Win;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.abyss";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const APP_ID: &str = "io.github.MatthewRafei.Abyss.Portal";

const XML: &str = r#"
<node>
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
</node>
"#;

/// Portal response codes.
const SUCCESS: u32 = 0;
const CANCELLED: u32 = 1;
const ENDED: u32 = 2;

/// Runs the backend until it has been idle for a minute.
pub fn run() -> glib::ExitCode {
    // GTK asks the portal for settings as it starts, while the portal waits
    // for this backend to come up: neither would answer. Backends keep GTK
    // off the portal (as xdg-desktop-portal-gnome does).
    let debug = std::env::var("GDK_DEBUG")
        .map_or_else(|_| "no-portals".to_owned(), |d| format!("{d},no-portals"));
    std::env::set_var("GDK_DEBUG", debug);
    std::env::remove_var("GTK_USE_PORTAL");
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::IS_SERVICE)
        .build();
    app.set_inactivity_timeout(60_000);
    app.connect_startup(|app| {
        crate::load_css();
        if let Some(desktop) = follow_desktop() {
            // Kept for the app's life: dropping it stops the updates.
            app.connect_shutdown(move |_| {
                let _ = &desktop;
            });
        }
        if let Err(e) = serve(app) {
            eprintln!("abyss: can't serve the file chooser portal: {e}");
            app.quit();
        }
    });
    // A service has nothing to show until a dialog is asked for.
    app.connect_activate(|_| {});
    app.run_with_args(&["abyss"])
}

/// Off the portal, GTK only has `settings.ini`: no dark colour scheme, and
/// the plain theme. Takes the desktop's look from `GSettings` instead (what
/// the portal itself reads), and follows its changes.
fn follow_desktop() -> Option<gio::Settings> {
    let schema =
        gio::SettingsSchemaSource::default()?.lookup("org.gnome.desktop.interface", true)?;
    let desktop = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None);
    let apply = move |desktop: &gio::Settings| {
        let Some(gtk_settings) = gtk::Settings::default() else {
            return;
        };
        let has = |key: &str| schema.has_key(key);
        for (key, setting) in [
            ("gtk-theme", "gtk-theme-name"),
            ("icon-theme", "gtk-icon-theme-name"),
            ("cursor-theme", "gtk-cursor-theme-name"),
            ("font-name", "gtk-font-name"),
        ] {
            if has(key) {
                gtk_settings.set_property(setting, desktop.string(key).as_str());
            }
        }
        if has("cursor-size") {
            gtk_settings.set_property("gtk-cursor-theme-size", desktop.int("cursor-size"));
        }
        if has("color-scheme") {
            let dark = desktop.string("color-scheme") == "prefer-dark";
            gtk_settings.set_property("gtk-application-prefer-dark-theme", dark);
            // GTK 4.20+: GtkInterfaceColorScheme DARK (2) or LIGHT (3).
            set_enum(
                gtk_settings.upcast_ref(),
                "gtk-interface-color-scheme",
                if dark { 2 } else { 3 },
            );
        }
    };
    apply(&desktop);
    desktop.connect_changed(None, move |desktop, _| apply(desktop));
    Some(desktop)
}

/// Sets an enum property by number, when this GTK has it.
fn set_enum(object: &glib::Object, property: &str, value: i32) {
    let Some(spec) = object.find_property(property) else {
        return;
    };
    if let Some(v) = glib::EnumClass::with_type(spec.value_type()).and_then(|c| c.to_value(value)) {
        object.set_property_from_value(property, &v);
    }
}

fn serve(app: &gtk::Application) -> Result<(), glib::Error> {
    let conn = app
        .dbus_connection()
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::NotConnected, "no session bus"))?;
    let node = gio::DBusNodeInfo::for_xml(XML)?;
    let chooser = node
        .lookup_interface("org.freedesktop.impl.portal.FileChooser")
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "bad interface XML"))?;
    let request = node
        .lookup_interface("org.freedesktop.impl.portal.Request")
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "bad interface XML"))?;
    let a = app.clone();
    conn.register_object(OBJECT_PATH, &chooser)
        .method_call(move |conn, _, _, _, method, params, invocation| {
            handle(&a, &conn, &request, method, &params, invocation);
        })
        .build()?;
    let a = app.clone();
    // Ids live as long as the connection; nothing to keep.
    let _ = gio::bus_own_name_on_connection(
        &conn,
        BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        |_, _| {},
        move |_, _| a.quit(),
    );
    Ok(())
}

fn handle(
    app: &gtk::Application,
    conn: &gio::DBusConnection,
    request_iface: &gio::DBusInterfaceInfo,
    method: &str,
    params: &glib::Variant,
    invocation: gio::DBusMethodInvocation,
) {
    let handle = params.child_value(0).str().unwrap_or_default().to_owned();
    let parent = params.child_value(2).str().unwrap_or_default().to_owned();
    let title = params.child_value(3).str().unwrap_or_default().to_owned();
    let options = glib::VariantDict::new(Some(&params.child_value(4)));
    let Some(request) = parse(method, title, parent, &options) else {
        invocation.return_dbus_error(
            "org.freedesktop.DBus.Error.UnknownMethod",
            &format!("no method {method}"),
        );
        return;
    };

    // The app can give up on the dialog (Request.Close): the window goes,
    // and the answer says the request ended.
    let closed = Rc::new(Cell::new(false));
    let win_slot: Rc<std::cell::RefCell<Option<std::rc::Weak<Win>>>> = Rc::default();
    let (c, slot) = (closed.clone(), win_slot.clone());
    let registration = conn
        .register_object(&handle, request_iface)
        .method_call(move |_, _, _, _, method, _, invocation| {
            if method == "Close" {
                c.set(true);
                if let Some(win) = slot.borrow().as_ref().and_then(std::rc::Weak::upgrade) {
                    win.close();
                }
            }
            invocation.return_value(None);
        })
        .build()
        .ok();

    let conn = conn.clone();
    let win = Win::open_picker(app, request, move |outcome| {
        let results = glib::VariantDict::new(None);
        let code = match outcome {
            _ if closed.get() => ENDED,
            Outcome::Cancelled => CANCELLED,
            Outcome::Chosen {
                uris,
                choices,
                filter,
            } => {
                results.insert_value("uris", &uris.to_variant());
                if !choices.is_empty() {
                    results.insert_value("choices", &choices.to_variant());
                }
                if let Some(f) = filter {
                    results.insert_value("current_filter", &(f.name, f.rules).to_variant());
                }
                SUCCESS
            }
        };
        invocation.return_value(Some(&glib::Variant::tuple_from_iter([
            code.to_variant(),
            results.end(),
        ])));
        if let Some(id) = registration {
            let _ = conn.unregister_object(id);
        }
    });
    win_slot.replace(Some(Rc::downgrade(&win)));
}

/// A filter as sent: its name and (kind, pattern) rules.
type RawFilter = (String, Vec<(u32, String)>);

fn opt<T: FromVariant>(options: &glib::VariantDict, key: &str) -> Option<T> {
    options.lookup::<T>(key).ok().flatten()
}

/// A path the portal sends as a NUL-terminated byte string.
fn path_of(bytes: &[u8]) -> Option<std::path::PathBuf> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    (!bytes.is_empty()).then(|| OsStr::from_bytes(bytes).into())
}

fn parse(
    method: &str,
    title: String,
    parent: String,
    options: &glib::VariantDict,
) -> Option<Request> {
    let mut folder = opt::<Vec<u8>>(options, "current_folder")
        .as_deref()
        .and_then(path_of)
        .map(gio::File::for_path);
    let purpose = match method {
        "OpenFile" => Purpose::Open {
            multiple: opt(options, "multiple").unwrap_or(false),
            directory: opt(options, "directory").unwrap_or(false),
        },
        "SaveFile" => {
            let current = opt::<Vec<u8>>(options, "current_file")
                .as_deref()
                .and_then(path_of);
            let mut name: String = opt(options, "current_name").unwrap_or_default();
            if let Some(path) = current {
                if folder.is_none() {
                    folder = path.parent().map(gio::File::for_path);
                }
                if let Some(base) = path.file_name() {
                    name = base.to_string_lossy().into_owned();
                }
            }
            Purpose::Save { name }
        }
        "SaveFiles" => Purpose::SaveFiles {
            names: opt::<Vec<Vec<u8>>>(options, "files")
                .unwrap_or_default()
                .iter()
                .filter_map(|b| path_of(b))
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect(),
        },
        _ => return None,
    };

    let into = |(name, rules): RawFilter| Filter { name, rules };
    let mut filters: Vec<Filter> = opt::<Vec<RawFilter>>(options, "filters")
        .unwrap_or_default()
        .into_iter()
        .map(into)
        .collect();
    // The current filter may be one of the list or one of its own.
    let current_filter = opt::<RawFilter>(options, "current_filter").map(|(name, rules)| {
        filters
            .iter()
            .position(|f| f.name == name && f.rules == rules)
            .unwrap_or_else(|| {
                filters.push(Filter { name, rules });
                filters.len() - 1
            })
    });
    let choices = opt::<Vec<(String, String, Vec<(String, String)>, String)>>(options, "choices")
        .unwrap_or_default()
        .into_iter()
        .map(|(id, label, options, initial)| Choice {
            id,
            label,
            options,
            initial,
        })
        .collect();
    let accept_label = opt::<String>(options, "accept_label").filter(|l| !l.is_empty());
    Some(Request {
        purpose,
        title,
        accept_label,
        filters,
        current_filter,
        choices,
        folder,
        parent: Some(parent).filter(|p| !p.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, glib::Variant)]) -> glib::VariantDict {
        let d = glib::VariantDict::new(None);
        for (k, v) in pairs {
            d.insert_value(k, v);
        }
        d
    }

    #[test]
    fn save_file_takes_folder_and_name_from_current_file() {
        let d = dict(&[(
            "current_file",
            b"/tmp/reports/q3.pdf\0".to_vec().to_variant(),
        )]);
        let r = parse("SaveFile", "Save".into(), String::new(), &d).unwrap();
        assert!(matches!(r.purpose, Purpose::Save { ref name } if name == "q3.pdf"));
        assert_eq!(
            r.folder.unwrap().path().unwrap(),
            std::path::Path::new("/tmp/reports")
        );
        assert!(r.parent.is_none());
    }

    #[test]
    fn current_filter_outside_the_list_is_added() {
        let images = ("Images".to_owned(), vec![(0u32, "*.png".to_owned())]);
        let text = ("Text".to_owned(), vec![(1u32, "text/plain".to_owned())]);
        let d = dict(&[
            ("filters", vec![images.clone()].to_variant()),
            ("current_filter", text.to_variant()),
            ("multiple", true.to_variant()),
        ]);
        let r = parse("OpenFile", "Open".into(), "wayland:abc".into(), &d).unwrap();
        assert_eq!(r.filters.len(), 2);
        assert_eq!(r.current_filter, Some(1));
        assert!(matches!(
            r.purpose,
            Purpose::Open {
                multiple: true,
                directory: false
            }
        ));
        assert_eq!(r.parent.as_deref(), Some("wayland:abc"));
        let d = dict(&[
            ("filters", vec![images.clone()].to_variant()),
            ("current_filter", images.to_variant()),
        ]);
        let r = parse("OpenFile", "Open".into(), String::new(), &d).unwrap();
        assert_eq!((r.filters.len(), r.current_filter), (1, Some(0)));
    }

    #[test]
    fn save_files_keeps_base_names() {
        let d = dict(&[(
            "files",
            vec![b"/x/a.txt\0".to_vec(), b"b.txt\0".to_vec()].to_variant(),
        )]);
        let r = parse("SaveFiles", "Save".into(), String::new(), &d).unwrap();
        assert!(
            matches!(r.purpose, Purpose::SaveFiles { ref names } if names == &["a.txt", "b.txt"])
        );
        assert!(parse("Nope", String::new(), String::new(), &d).is_none());
    }
}
