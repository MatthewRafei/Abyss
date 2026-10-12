//! Add Server: a small form that turns a protocol, an address and an
//! optional folder into a location, connects to it and keeps it in the
//! sidebar's Network section.

use super::*;

/// The protocols offered, as (label, scheme, address hint, folder hint).
const KINDS: [(&str, &str, &str, &str); 5] = [
    (
        "Windows or NAS share (SMB)",
        "smb",
        "nas.local or 192.168.1.10",
        "share/folder (optional)",
    ),
    (
        "SSH server (SFTP)",
        "sftp",
        "user@host",
        "folder (optional)",
    ),
    (
        "Web storage (WebDAV)",
        "davs",
        "cloud.example.com",
        "folder (optional)",
    ),
    ("FTP server", "ftp", "ftp.example.com", "folder (optional)"),
    ("Unix share (NFS)", "nfs", "nas.local", "/export/path"),
];

/// The location for `address` (a host, `user@host:port`, a full URI, or a
/// Windows `\\host\share` path) and `folder` under `scheme`. None until
/// there's an address.
pub(super) fn server_uri(scheme: &str, address: &str, folder: &str) -> Option<String> {
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    if address.contains("://") {
        return Some(address.to_owned());
    }
    // Pasted from Windows: \\host\share\folder.
    let address = address.replace('\\', "/");
    let address = address.trim_start_matches('/');
    let (host, path) = address.split_once('/').unwrap_or((address, ""));
    let folder = [path, folder.trim()]
        .iter()
        .flat_map(|p| p.split('/'))
        .filter(|s| !s.is_empty())
        .map(|s| glib::uri_escape_string(s, None::<&str>, false).to_string())
        .collect::<Vec<_>>()
        .join("/");
    Some(format!("{scheme}://{host}/{folder}"))
}

/// Whether GIO here can open `scheme` locations (gvfs backends are
/// optional: a system's gvfs may come without NFS, for one).
fn supported(scheme: &str) -> bool {
    gio::Vfs::default()
        .supported_uri_schemes()
        .iter()
        .any(|s| s == scheme)
}

/// The protocol's name for messages: "NFS" for `nfs`.
fn protocol_name(scheme: &str) -> String {
    KINDS
        .iter()
        .find(|k| k.1 == scheme)
        .and_then(|k| {
            k.0.rsplit_once('(')
                .map(|(_, p)| p.trim_end_matches(')').to_owned())
        })
        .unwrap_or_else(|| scheme.to_uppercase())
}

/// What to do when `scheme` can't be opened here.
fn missing_note(scheme: &str) -> String {
    let name = protocol_name(scheme);
    format!(
        "{name} isn't available: this system's gvfs was built without it. \
         Install gvfs with {name} support, or mount the share with fstab or \
         autofs and it shows up in the sidebar."
    )
}

/// The scheme of what's typed: the full address's own, else the picked one.
fn scheme_of(uri: &str) -> &str {
    uri.split_once("://").map_or(uri, |(s, _)| s)
}

/// What the line under the fields says.
enum Status {
    Clear,
    Busy(String),
    Error(String),
}

/// The form's parts that the window wires up.
struct Form {
    grid: gtk::Grid,
    kind: gtk::DropDown,
    address: gtk::Entry,
    folder: gtk::Entry,
    preview: gtk::Label,
    spinner: gtk::Spinner,
    status: gtk::Label,
    cancel: gtk::Button,
    connect: gtk::Button,
    busy: Cell<bool>,
    /// The connection being made; dropping it stops it.
    task: RefCell<Option<glib::JoinHandle<()>>>,
    /// Follows the sign-in bar while connecting.
    asking: RefCell<Option<glib::SignalHandlerId>>,
}

impl Form {
    fn new() -> Self {
        let grid = gtk::Grid::builder()
            .row_spacing(8)
            .column_spacing(10)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let title = gtk::Label::new(Some("Add Server"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        grid.attach(&title, 0, 0, 2, 1);
        let caption = |text: &str| {
            let label = gtk::Label::new(Some(text));
            label.set_xalign(1.0);
            label.add_css_class("dim-label");
            label
        };
        // Protocols this system can't open stay listed, marked as such.
        let kinds: Vec<String> = KINDS
            .iter()
            .map(|k| {
                if supported(k.1) {
                    k.0.to_owned()
                } else {
                    format!("{} – not installed", k.0)
                }
            })
            .collect();
        let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
        let kind = gtk::DropDown::from_strings(&kinds);
        kind.set_hexpand(true);
        grid.attach(&caption("Type"), 0, 1, 1, 1);
        grid.attach(&kind, 1, 1, 1, 1);
        let address = gtk::Entry::builder().width_chars(26).build();
        grid.attach(&caption("Address"), 0, 2, 1, 1);
        grid.attach(&address, 1, 2, 1, 1);
        let folder = gtk::Entry::new();
        grid.attach(&caption("Folder"), 0, 3, 1, 1);
        grid.attach(&folder, 1, 3, 1, 1);
        // What will be opened, as it's typed.
        let preview = gtk::Label::new(None);
        preview.add_css_class("server-preview");
        preview.set_xalign(0.0);
        preview.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        preview.set_selectable(true);
        grid.attach(&preview, 0, 4, 2, 1);
        // Progress and problems, right where the address was typed.
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let status = gtk::Label::new(None);
        status.add_css_class("server-status");
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_max_width_chars(42);
        status.set_hexpand(true);
        line.append(&spinner);
        line.append(&status);
        line.set_visible(false);
        grid.attach(&line, 0, 5, 2, 1);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let connect = gtk::Button::with_label("Connect");
        connect.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&connect);
        grid.attach(&buttons, 0, 6, 2, 1);
        Self {
            grid,
            kind,
            address,
            folder,
            preview,
            spinner,
            status,
            cancel,
            connect,
            busy: Cell::new(false),
            task: RefCell::default(),
            asking: RefCell::default(),
        }
    }

    fn picked(&self) -> &'static (&'static str, &'static str, &'static str, &'static str) {
        KINDS
            .get(usize::try_from(self.kind.selected()).unwrap_or(0))
            .unwrap_or(&KINDS[0])
    }

    /// The location as typed so far.
    fn uri(&self) -> Option<String> {
        server_uri(self.picked().1, &self.address.text(), &self.folder.text())
    }

    fn set_status(&self, status: Status) {
        let (text, busy, error) = match status {
            Status::Clear => (String::new(), false, false),
            Status::Busy(text) => (text, true, false),
            Status::Error(text) => (text, false, true),
        };
        self.busy.set(busy);
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        self.status.set_text(&text);
        if error {
            self.status.add_css_class("error");
        } else {
            self.status.remove_css_class("error");
        }
        if let Some(line) = self.status.parent() {
            line.set_visible(!text.is_empty());
        }
        for field in [
            self.kind.upcast_ref::<gtk::Widget>(),
            self.address.upcast_ref(),
            self.folder.upcast_ref(),
        ] {
            field.set_sensitive(!busy);
        }
        self.cancel.set_label(if busy { "Stop" } else { "Cancel" });
    }

    /// Hints, preview, availability and Connect follow what's typed.
    fn update(&self) {
        if self.busy.get() {
            return;
        }
        let kind = self.picked();
        self.address.set_placeholder_text(Some(kind.2));
        self.folder.set_placeholder_text(Some(kind.3));
        // A full address already says where to go.
        self.folder
            .set_sensitive(!self.address.text().contains("://"));
        let uri = self.uri();
        // Shown as typed (spaces, not %20).
        let shown = uri.as_deref().map(|u| {
            glib::uri_unescape_string(u, None::<&str>)
                .map_or_else(|| u.to_owned(), |s| s.to_string())
        });
        self.preview.set_text(
            shown
                .as_deref()
                .unwrap_or("Type the server's name or address"),
        );
        let scheme = uri.as_deref().map_or(kind.1, scheme_of);
        let available = supported(scheme);
        self.set_status(if available {
            Status::Clear
        } else {
            Status::Error(missing_note(scheme))
        });
        self.connect.set_sensitive(uri.is_some() && available);
    }
}

impl Win {
    /// Opens the Add Server form next to `row`.
    pub(super) fn show_add_server(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        let form = Rc::new(Form::new());
        form.update();
        let refresh = {
            let form = form.clone();
            move || form.update()
        };
        let again = refresh.clone();
        form.kind.connect_selected_notify(move |_| again());
        let again = refresh.clone();
        form.address.connect_changed(move |_| again());
        form.folder.connect_changed(move |_| refresh());

        let popover = gtk::Popover::new();
        popover.add_css_class("add-server");
        // Stays open like a dialog (Cancel or Escape closes it): the
        // sign-in bar may need clicking while it connects.
        popover.set_autohide(false);
        // Cancel closes the form; while connecting it stops instead.
        {
            let w = Rc::downgrade(self);
            let weak = popover.downgrade();
            let f = Rc::downgrade(&form);
            form.cancel.connect_clicked(move |_| {
                let (Some(win), Some(form)) = (w.upgrade(), f.upgrade()) else {
                    return;
                };
                if form.busy.get() {
                    if let Some(task) = form.task.take() {
                        task.abort();
                    }
                    win.auth.dismiss();
                    win.end_connect(&form);
                } else if let Some(popover) = weak.upgrade() {
                    popover.popdown();
                }
            });
        }
        let go = {
            let w = Rc::downgrade(self);
            let weak = popover.downgrade();
            let form = Rc::downgrade(&form);
            move || {
                let (Some(win), Some(form), Some(popover)) =
                    (w.upgrade(), form.upgrade(), weak.upgrade())
                else {
                    return;
                };
                if form.busy.get() || !form.connect.is_sensitive() {
                    return;
                }
                if let Some(uri) = form.uri() {
                    win.connect_server(&form, &popover, uri);
                }
            }
        };
        let run = go.clone();
        form.connect.connect_clicked(move |_| run());
        let run = go.clone();
        form.address.connect_activate(move |_| run());
        form.folder.connect_activate(move |_| go());

        popover.set_child(Some(&form.grid));
        // On the sidebar box, not the row (see show_place_menu).
        let Some(anchor) = self.sidebar.parent() else {
            return;
        };
        let (right, middle) = translate(
            row,
            &anchor,
            f64::from(row.width()),
            f64::from(row.height()) / 2.0,
        );
        popover.set_parent(&anchor);
        popover.set_position(gtk::PositionType::Right);
        popover.set_pointing_to(Some(&spot(right, middle)));
        // The form lives as long as the popover does; closing it stops a
        // connection still being made.
        let keep = RefCell::new(Some(form.clone()));
        let w = Rc::downgrade(self);
        popover.connect_closed(move |popover| {
            if let Some(form) = keep.take() {
                if let Some(task) = form.task.take() {
                    task.abort();
                    if let Some(win) = w.upgrade() {
                        win.auth.dismiss();
                        win.end_connect(&form);
                    }
                }
            }
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
        form.address.grab_focus();
    }

    /// Connects to `uri` from the form, which shows how it goes: it closes
    /// once the server is open (and kept in the sidebar), or says why not.
    fn connect_server(self: &Rc<Self>, form: &Rc<Form>, popover: &gtk::Popover, uri: String) {
        let file = gio::File::for_uri(&uri);
        let host = places::server_name(&uri);
        form.set_status(Status::Busy(format!("Connecting to {host}…")));
        form.connect.set_sensitive(false);
        let op = self.auth.operation(Some(&uri));
        // Point at the sign-in bar while it asks.
        let asking = {
            let f = Rc::downgrade(form);
            let host = host.clone();
            self.auth.connect_asking(move |asking| {
                if let Some(form) = f.upgrade().filter(|f| f.busy.get()) {
                    form.set_status(Status::Busy(if asking {
                        format!("{host} asks you to sign in: see the bar below")
                    } else {
                        format!("Connecting to {host}…")
                    }));
                }
            })
        };
        let w = Rc::downgrade(self);
        let f = Rc::downgrade(form);
        let p = popover.downgrade();
        let task = glib::spawn_future_local(async move {
            let result = file
                .mount_enclosing_volume_future(gio::MountMountFlags::NONE, Some(&op))
                .await;
            let (Some(win), Some(form)) = (w.upgrade(), f.upgrade()) else {
                return;
            };
            win.end_connect(&form);
            let outcome = match result {
                Ok(()) => Ok(()),
                Err(e) if e.matches(gio::IOErrorEnum::AlreadyMounted) => Ok(()),
                Err(e) => Err(e),
            };
            match outcome {
                Ok(()) => {
                    if let Some(popover) = p.upgrade() {
                        popover.popdown();
                    }
                    if let Err(e) = places::save_server(&uri) {
                        win.show_error(&format!("Couldn't save the server: {e}"));
                    }
                    win.refresh_sidebar();
                    win.navigate(file);
                }
                // Sign-in was cancelled: back to the form as it was.
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => {}
                Err(e) => {
                    let text = if e.matches(gio::IOErrorEnum::NotMountableFile)
                        && !supported(scheme_of(&uri))
                    {
                        missing_note(scheme_of(&uri))
                    } else {
                        format!("Couldn't connect to {host}: {}", e.message())
                    };
                    form.set_status(Status::Error(text));
                }
            }
        });
        form.task.replace(Some(task));
        form.asking.replace(Some(asking));
    }

    /// The form is done connecting (it worked, failed, or was stopped):
    /// back to editable, following what's typed.
    fn end_connect(&self, form: &Form) {
        form.task.replace(None);
        if let Some(id) = form.asking.take() {
            self.auth.disconnect(id);
        }
        form.busy.set(false);
        form.update();
    }
}

#[cfg(test)]
mod tests {
    use super::server_uri;

    #[test]
    fn addresses() {
        assert_eq!(server_uri("smb", "  ", "x"), None);
        assert_eq!(server_uri("smb", "nas", "").as_deref(), Some("smb://nas/"));
        assert_eq!(
            server_uri("smb", "nas", "/Media/My Films/").as_deref(),
            Some("smb://nas/Media/My%20Films")
        );
        assert_eq!(
            server_uri("smb", r"\\nas\Media\tv", "").as_deref(),
            Some("smb://nas/Media/tv")
        );
        assert_eq!(
            server_uri("sftp", "me@box:2222", "srv").as_deref(),
            Some("sftp://me@box:2222/srv")
        );
        assert_eq!(
            server_uri("smb", "sftp://box/x", "ignored").as_deref(),
            Some("sftp://box/x")
        );
    }
}
