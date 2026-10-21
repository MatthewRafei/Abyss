//! The freedesktop `FileManager1` D-Bus service, through which other apps
//! open folders, "Show in Folder" (`ShowItems`) and show Properties.
//! Ported from nautilus-freedesktop-dbus.c; install.sh installs the
//! service file that starts Abyss for it.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::window::Win;

const NAME: &str = "org.freedesktop.FileManager1";
const PATH: &str = "/org/freedesktop/FileManager1";
const XML: &str = r#"<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Serves `FileManager1` on the app's bus connection. Another file manager
/// holding the name (Nautilus) keeps it; Abyss waits in line behind it.
pub fn register(app: &gtk::Application) {
    let Some(connection) = app.dbus_connection() else {
        return;
    };
    let Some(interface) = gio::DBusNodeInfo::for_xml(XML)
        .ok()
        .and_then(|node| node.lookup_interface(NAME))
    else {
        return;
    };
    let app = app.downgrade();
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(move |_, _, _, _, method, params, invocation| {
            let Some(app) = app.upgrade() else {
                return invocation.return_value(None);
            };
            let Some((uris, startup_id)) = params.get::<(Vec<String>, String)>() else {
                return invocation.return_error(gio::DBusError::InvalidArgs, "Expected (as, s)");
            };
            let files = uris.iter().map(|uri| gio::File::for_uri(uri));
            match method {
                "ShowFolders" => {
                    for folder in files {
                        Win::show(&app, folder, Vec::new(), false, &startup_id);
                    }
                }
                "ShowItems" | "ShowItemProperties" => {
                    let properties = method == "ShowItemProperties";
                    for (folder, items) in by_folder(files) {
                        Win::show(&app, folder, items, properties, &startup_id);
                    }
                }
                _ => {
                    return invocation.return_error(gio::DBusError::UnknownMethod, method);
                }
            }
            invocation.return_value(None);
        })
        .build();
    if let Err(e) = registered {
        glib::g_warning!("abyss", "FileManager1 not served: {e}");
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

/// Files grouped by the folder they're in, in the order first named; a
/// file with no parent (a root) is shown as a folder of its own.
fn by_folder(files: impl Iterator<Item = gio::File>) -> Vec<(gio::File, Vec<gio::File>)> {
    let mut groups: Vec<(gio::File, Vec<gio::File>)> = Vec::new();
    for file in files {
        let Some(parent) = file.parent() else {
            groups.push((file, Vec::new()));
            continue;
        };
        match groups.iter_mut().find(|(folder, _)| folder.equal(&parent)) {
            Some((_, items)) => items.push(file),
            None => groups.push((parent, vec![file])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_items_by_folder() {
        let files = ["/a/x", "/b/y", "/a/z", "/"].map(gio::File::for_path);
        let groups = by_folder(files.into_iter());
        let paths: Vec<_> = groups
            .iter()
            .map(|(folder, items)| {
                let folder = folder.path().unwrap_or_default();
                let items: Vec<_> = items.iter().filter_map(gio::File::path).collect();
                (folder, items)
            })
            .collect();
        assert_eq!(
            paths,
            [
                ("/a".into(), vec!["/a/x".into(), "/a/z".into()]),
                ("/b".into(), vec!["/b/y".into()]),
                ("/".into(), vec![]),
            ]
        );
    }
}
