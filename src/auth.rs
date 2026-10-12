//! Sign-in bar for network mounts, replacing GTK's stock mount dialog.
//!
//! GVfs asks separately for a server's share list (`smb://host/`) and for
//! each share (`smb://host/share`). Abyss remembers what was entered for a
//! server for as long as it runs and answers those follow-up prompts itself,
//! falling back to the bar if the server rejects them.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

#[derive(Clone)]
struct Creds {
    anonymous: bool,
    user: String,
    domain: String,
    password: String,
    save: gio::PasswordSave,
}

impl Creds {
    fn apply(&self, op: &gio::MountOperation) {
        op.set_anonymous(self.anonymous);
        if !self.anonymous {
            op.set_username(Some(&self.user));
            op.set_domain(Some(&self.domain));
            op.set_password(Some(&self.password));
        }
        op.set_password_save(self.save);
    }
}

/// `scheme://host` of a location, the key credentials are shared under.
pub fn server_of(uri: &str) -> Option<String> {
    let parsed = glib::Uri::parse(uri, glib::UriFlags::NONE).ok()?;
    let host = parsed.host().filter(|h| !h.is_empty())?;
    Some(format!("{}://{}", parsed.scheme(), host.to_lowercase()))
}

/// Whether a Secret Service (gnome-keyring, KeePassXC, …) is running, which
/// is what GVfs needs to remember passwords "forever".
fn have_keyring() -> bool {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return false;
    };
    bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&("org.freedesktop.secrets",).to_variant()),
        glib::VariantTy::new("(b)").ok(),
        gio::DBusCallFlags::NONE,
        500,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|v| v.get::<(bool,)>())
    .is_some_and(|(owned,)| owned)
}

pub struct AuthBar {
    revealer: gtk::Revealer,
    title: gtk::Label,
    detail: gtk::Label,
    fields: gtk::Box,
    buttons: gtk::Box,
    user: gtk::Entry,
    domain: gtk::Entry,
    password: gtk::PasswordEntry,
    options: gtk::Box,
    anonymous: gtk::CheckButton,
    remember: gtk::DropDown,
    choices: gtk::Box,
    connect: gtk::Button,
    /// The operation waiting on the bar, and the server it's for.
    pending: RefCell<Option<(gio::MountOperation, Option<String>)>>,
    creds: RefCell<HashMap<String, Creds>>,
    keyring: bool,
}

/// Who a password prompt is for, and whether it's asked again because the
/// last answer was refused.
struct Asking {
    server: Option<String>,
    refused: bool,
}

const REMEMBER: [(&str, gio::PasswordSave); 3] = [
    ("Don't remember", gio::PasswordSave::Never),
    ("Until logout", gio::PasswordSave::ForSession),
    ("Always", gio::PasswordSave::Permanently),
];

impl AuthBar {
    pub fn new() -> Rc<Self> {
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.add_css_class("conflict-title");
        title.set_wrap(true);
        let detail = gtk::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_wrap(true);
        detail.add_css_class("dim-label");

        let user = gtk::Entry::builder()
            .placeholder_text("Username")
            .hexpand(true)
            .build();
        let domain = gtk::Entry::builder()
            .placeholder_text("Domain")
            .width_chars(12)
            .build();
        let password = gtk::PasswordEntry::builder()
            .placeholder_text("Password")
            .show_peek_icon(true)
            .hexpand(true)
            .build();
        let fields = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        fields.append(&user);
        fields.append(&domain);
        fields.append(&password);

        let keyring = have_keyring();
        let labels: Vec<&str> = REMEMBER
            .iter()
            .filter(|(_, save)| keyring || *save != gio::PasswordSave::Permanently)
            .map(|(label, _)| *label)
            .collect();
        let remember = gtk::DropDown::from_strings(&labels);
        remember.set_selected(1);
        remember.set_tooltip_text(Some("Remember this password"));
        let anonymous = gtk::CheckButton::with_label("Connect anonymously");
        let cancel = gtk::Button::with_label("Cancel");
        let connect = gtk::Button::with_label("Connect");
        connect.add_css_class("suggested-action");
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let options = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        options.append(&anonymous);
        options.append(&remember);
        let left = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        left.set_hexpand(true);
        left.append(&options);
        left.append(&spacer);
        let choices = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        actions.append(&choices);
        actions.append(&cancel);
        actions.append(&connect);
        // The buttons go under the options when the window is narrow.
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&left);
        buttons.append(&actions);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.add_css_class("prompt");
        body.add_css_class("auth");
        body.append(&title);
        body.append(&detail);
        body.append(&fields);
        body.append(&buttons);
        let revealer = gtk::Revealer::builder()
            .child(&body)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        let bar = Rc::new(Self {
            revealer,
            title,
            detail,
            fields,
            buttons,
            user,
            domain,
            password,
            options,
            anonymous,
            remember,
            choices,
            connect,
            pending: RefCell::default(),
            creds: RefCell::default(),
            keyring,
        });
        bar.wire(&cancel);
        bar
    }

    pub const fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    /// Narrow window: the fields stack, and the buttons go under the options.
    pub fn set_compact(&self, compact: bool) {
        let o = if compact {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        };
        self.fields.set_orientation(o);
        self.buttons.set_orientation(o);
    }

    fn wire(self: &Rc<Self>, cancel: &gtk::Button) {
        let me = Rc::downgrade(self);
        self.connect.connect_clicked(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.submit();
            }
        });
        let me = Rc::downgrade(self);
        self.password.connect_activate(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.submit();
            }
        });
        let me = Rc::downgrade(self);
        self.user.connect_activate(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.password.grab_focus();
            }
        });
        let me = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.finish(gio::MountOperationResult::Aborted);
            }
        });
        let me = Rc::downgrade(self);
        self.anonymous.connect_toggled(move |check| {
            if let Some(bar) = me.upgrade() {
                bar.fields.set_sensitive(!check.is_active());
            }
        });
        let keys = gtk::EventControllerKey::new();
        let me = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(bar) = me.upgrade() {
                    bar.finish(gio::MountOperationResult::Aborted);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.revealer.add_controller(keys);
    }

    /// Takes down a prompt that's no longer wanted (its connection was
    /// stopped), answering it as cancelled.
    pub fn dismiss(&self) {
        if self.pending.borrow().is_some() {
            self.finish(gio::MountOperationResult::Aborted);
        }
    }

    /// Calls `f` with whether the bar is asking, as that changes.
    pub fn connect_asking(&self, f: impl Fn(bool) + 'static) -> glib::SignalHandlerId {
        self.revealer
            .connect_reveal_child_notify(move |r| f(r.reveals_child()))
    }

    pub fn disconnect(&self, id: glib::SignalHandlerId) {
        self.revealer.disconnect(id);
    }

    /// A mount operation for `target` (a location URI, if known) whose
    /// prompts go to this bar.
    pub fn operation(self: &Rc<Self>, target: Option<&str>) -> gio::MountOperation {
        let op = gio::MountOperation::new();
        let server = target.and_then(server_of);
        // Set once this operation has been answered from the cache, so a
        // second prompt means the server rejected those credentials.
        let used_cache = Rc::new(Cell::new(false));
        // Prompts so far: asked again means the last answer was refused.
        let asked = Cell::new(0_u32);

        let me = Rc::downgrade(self);
        op.connect_ask_password(move |op, message, user, domain, flags| {
            // Ours to answer: GIO's own handler would otherwise reply
            // "unhandled" from an idle while the bar waits, and the server
            // gives up ("Permission denied") before anyone can sign in.
            op.stop_signal_emission_by_name("ask-password");
            let Some(bar) = me.upgrade() else {
                op.reply(gio::MountOperationResult::Unhandled);
                return;
            };
            if let Some(server) = &server {
                if used_cache.get() {
                    bar.creds.borrow_mut().remove(server);
                } else if let Some(creds) = bar.creds.borrow().get(server).cloned() {
                    used_cache.set(true);
                    creds.apply(op);
                    op.reply(gio::MountOperationResult::Handled);
                    return;
                }
            }
            let refused = asked.replace(asked.get() + 1) > 0 || used_cache.get();
            let to = Asking {
                server: server.clone(),
                refused,
            };
            bar.ask_password(op, message, user, domain, flags, to);
        });
        let me = Rc::downgrade(self);
        // gio-rs has no typed binding for this signal.
        op.connect_local("ask-question", false, move |args| {
            let op = args.first()?.get::<gio::MountOperation>().ok()?;
            // As for passwords: keep GIO's handler from answering too.
            op.stop_signal_emission_by_name("ask-question");
            let message = args.get(1)?.get::<String>().unwrap_or_default();
            let choices: Vec<String> = args
                .get(2)
                .and_then(|v| v.get::<glib::StrV>().ok())
                .map(|v| v.iter().map(ToString::to_string).collect())
                .unwrap_or_default();
            match me.upgrade() {
                Some(bar) => bar.ask_question(&op, &message, &choices),
                None => op.reply(gio::MountOperationResult::Unhandled),
            }
            None
        });
        let me = Rc::downgrade(self);
        op.connect_aborted(move |op| {
            if let Some(bar) = me.upgrade() {
                let ours = bar.pending.borrow().as_ref().is_some_and(|(p, _)| p == op);
                if ours {
                    bar.pending.replace(None);
                    bar.revealer.set_reveal_child(false);
                }
            }
        });
        op
    }

    /// Takes over the bar for `op`, aborting any earlier prompt.
    fn take_bar(&self, op: &gio::MountOperation, server: Option<String>) {
        if let Some((old, _)) = self.pending.replace(Some((op.clone(), server))) {
            if &old != op {
                old.reply(gio::MountOperationResult::Aborted);
            }
        }
    }

    fn ask_password(
        &self,
        op: &gio::MountOperation,
        message: &str,
        user: &str,
        domain: &str,
        flags: gio::AskPasswordFlags,
        to: Asking,
    ) {
        let Asking { server, refused } = to;
        self.take_bar(op, server);
        let mut lines = message.lines();
        let first = lines.next().unwrap_or("Sign in");
        // GVfs sends "Authentication Required\nEnter user and password…:".
        let (title, detail) = if first == "Authentication Required" {
            (first, lines.collect::<Vec<_>>().join(" "))
        } else {
            ("Authentication Required", message.to_owned())
        };
        // Asked again: say so, or it looks like nothing happened.
        if refused {
            self.title.set_text("Sign-in failed");
            self.title.add_css_class("refused");
            self.detail.set_text(
                "The server didn't accept that user name and password. Check them and try again.",
            );
            self.detail.set_visible(true);
        } else {
            self.title.set_text(title);
            self.title.remove_css_class("refused");
            self.detail.set_text(detail.trim_end_matches(':'));
            self.detail.set_visible(!detail.is_empty());
        }

        // Keep the user name just tried over the server's suggestion.
        if !refused || self.user.text().is_empty() {
            self.user.set_text(user);
        }
        self.domain.set_text(domain);
        self.password.set_text("");
        self.user
            .set_visible(flags.contains(gio::AskPasswordFlags::NEED_USERNAME));
        self.domain
            .set_visible(flags.contains(gio::AskPasswordFlags::NEED_DOMAIN));
        self.password
            .set_visible(flags.contains(gio::AskPasswordFlags::NEED_PASSWORD));
        self.fields.set_visible(true);
        self.fields.set_sensitive(true);
        self.anonymous.set_active(false);
        self.anonymous
            .set_visible(flags.contains(gio::AskPasswordFlags::ANONYMOUS_SUPPORTED));
        self.remember
            .set_visible(flags.contains(gio::AskPasswordFlags::SAVING_SUPPORTED));
        self.options.set_visible(true);
        self.clear_choices();
        self.connect.set_visible(true);
        self.revealer.set_reveal_child(true);

        let focus_user = WidgetExt::is_visible(&self.user) && user.is_empty();
        if focus_user {
            self.user.grab_focus();
        } else if self.password.is_visible() {
            self.password.grab_focus();
        } else {
            self.connect.grab_focus();
        }
    }

    fn ask_question(self: &Rc<Self>, op: &gio::MountOperation, message: &str, choices: &[String]) {
        self.take_bar(op, None);
        let mut lines = message.lines();
        self.title.set_text(lines.next().unwrap_or("Question"));
        let detail = lines.collect::<Vec<_>>().join("\n");
        self.detail.set_text(&detail);
        self.detail.set_visible(!detail.is_empty());
        self.fields.set_visible(false);
        self.options.set_visible(false);
        self.connect.set_visible(false);
        self.clear_choices();
        for (i, choice) in choices.iter().enumerate() {
            let button = gtk::Button::with_label(choice);
            let me = Rc::downgrade(self);
            let index = i32::try_from(i).unwrap_or(i32::MAX);
            button.connect_clicked(move |_| {
                if let Some(bar) = me.upgrade() {
                    if let Some((op, _)) = bar.pending.borrow().as_ref() {
                        op.set_choice(index);
                    }
                    bar.finish(gio::MountOperationResult::Handled);
                }
            });
            self.choices.append(&button);
        }
        self.revealer.set_reveal_child(true);
    }

    fn clear_choices(&self) {
        while let Some(child) = self.choices.first_child() {
            self.choices.remove(&child);
        }
    }

    fn selected_save(&self) -> gio::PasswordSave {
        let options: Vec<gio::PasswordSave> = REMEMBER
            .iter()
            .map(|(_, save)| *save)
            .filter(|save| self.keyring || *save != gio::PasswordSave::Permanently)
            .collect();
        usize::try_from(self.remember.selected())
            .ok()
            .and_then(|i| options.get(i).copied())
            .unwrap_or(gio::PasswordSave::Never)
    }

    fn submit(&self) {
        let Some((op, server)) = self.pending.borrow().clone() else {
            return;
        };
        let creds = Creds {
            anonymous: self.anonymous.is_visible() && self.anonymous.is_active(),
            user: self.user.text().to_string(),
            domain: self.domain.text().to_string(),
            password: self.password.text().to_string(),
            save: self.selected_save(),
        };
        creds.apply(&op);
        if let Some(server) = server {
            self.creds.borrow_mut().insert(server, creds);
        }
        self.finish(gio::MountOperationResult::Handled);
    }

    fn finish(&self, result: gio::MountOperationResult) {
        self.password.set_text("");
        self.revealer.set_reveal_child(false);
        if let Some((op, _)) = self.pending.take() {
            op.reply(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::server_of;

    #[test]
    fn servers() {
        assert_eq!(
            server_of("smb://TrueNAS/batocera/x").as_deref(),
            Some("smb://truenas")
        );
        assert_eq!(
            server_of("smb://truenas/").as_deref(),
            Some("smb://truenas")
        );
        assert_eq!(
            server_of("sftp://user@box:2222/home").as_deref(),
            Some("sftp://box")
        );
        assert_eq!(server_of("file:///home"), None);
    }
}
