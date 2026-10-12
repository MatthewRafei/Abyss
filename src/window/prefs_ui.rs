//! The main menu, the Preferences window and the list of keyboard
//! shortcuts.

use super::*;
use crate::prefs::{self, Prefs};

/// Every shortcut, by group, as (keys, what it does).
const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("Alt+Left", "Back"),
            ("Alt+Right", "Forward"),
            ("Alt+Up, Backspace", "Up to the parent folder"),
            ("Alt+Home", "Home folder"),
            ("Ctrl+L", "Type a location"),
            ("Ctrl+F", "Filter, then search subfolders"),
            ("Ctrl+R", "Reload"),
        ],
    ),
    (
        "Files",
        &[
            ("Ctrl+C, Ctrl+X, Ctrl+V", "Copy, cut, paste"),
            ("Ctrl+Z", "Undo the last operation"),
            ("F2", "Rename (several selected: batch rename)"),
            ("Alt+Enter", "Properties"),
            ("Delete", "Move to the trash"),
            ("Shift+Delete", "Delete forever"),
            ("Ctrl+Shift+N", "New folder"),
            ("Menu", "File menu"),
        ],
    ),
    (
        "View",
        &[
            ("Ctrl+1, Ctrl+2", "List, grid"),
            ("Ctrl+plus, Ctrl+minus", "Bigger, smaller icons"),
            ("Ctrl+0", "Normal icon size"),
            ("Ctrl+H", "Show or hide hidden files"),
        ],
    ),
    (
        "Tabs and panes",
        &[
            ("Ctrl+T", "New tab"),
            ("Ctrl+W", "Close tab"),
            ("Ctrl+Shift+T", "Reopen closed tab"),
            ("Ctrl+Tab, Ctrl+Page Down", "Next tab"),
            ("Ctrl+Shift+Tab, Ctrl+Page Up", "Previous tab"),
            ("Alt+1 … Alt+9", "Go to tab"),
            ("F3", "Split view"),
            ("Tab", "Other pane (split view)"),
            ("F5, F6", "Copy, move to the other pane (split view)"),
            ("Ctrl+U", "Swap panes"),
        ],
    ),
    (
        "Window",
        &[
            ("Ctrl+N", "New window"),
            ("Ctrl+Shift+W", "Close window"),
            ("Ctrl+,", "Preferences"),
            ("F1, Ctrl+?", "This list"),
        ],
    ),
];

/// A modal window over `parent` with a title, the CSS hooks the app's other
/// windows use, and Esc to close.
pub(super) fn dialog(
    parent: &gtk::ApplicationWindow,
    title: &str,
    width: i32,
    height: i32,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(title)
        .transient_for(parent)
        .modal(true)
        .default_width(width)
        .default_height(height)
        .build();
    window.add_css_class("abyss");
    window.add_css_class("dialog-window");
    let keys = gtk::EventControllerKey::new();
    let w = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = w.upgrade() {
                w.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    window
}

/// A row with a title, a smaller explanation under it, and `control` at
/// the end.
fn setting_row(title: &str, detail: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let name = gtk::Label::new(Some(title));
    name.set_xalign(0.0);
    let more = gtk::Label::new(Some(detail));
    more.set_xalign(0.0);
    more.set_wrap(true);
    more.add_css_class("dim-label");
    more.add_css_class("setting-detail");
    text.append(&name);
    text.append(&more);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("setting");
    row.append(&text);
    control.as_ref().set_valign(gtk::Align::Center);
    row.append(control);
    row
}

impl Win {
    /// The main menu, on the toolbar: undo, preferences, shortcuts.
    pub(super) fn main_menu_button(win: &Rc<Self>) -> gtk::MenuButton {
        let button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Menu")
            .build();
        button.add_css_class("flat");
        let w = Rc::downgrade(win);
        // Built when opened, so Undo names the step it would take back.
        button.set_create_popup_func(move |button| {
            let Some(win) = w.upgrade() else { return };
            let popover = gtk::Popover::new();
            popover.add_css_class("menu");
            popover.set_has_arrow(false);
            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let entry = |label: &str, enabled: bool, run: fn(&Rc<Self>)| {
                let item = gtk::Button::with_label(label);
                item.add_css_class("flat");
                item.set_sensitive(enabled);
                if let Some(l) = item.child().and_downcast::<gtk::Label>() {
                    l.set_xalign(0.0);
                }
                let w = Rc::downgrade(&win);
                let p = popover.downgrade();
                item.connect_clicked(move |_| {
                    if let Some(p) = p.upgrade() {
                        p.popdown();
                    }
                    if let Some(win) = w.upgrade() {
                        run(&win);
                    }
                });
                column.append(&item);
            };
            let undo = win.last_step();
            let label = undo.map_or_else(|| "Undo".to_owned(), |l| format!("Undo {l}"));
            entry(&label, win.last_step().is_some(), Self::undo);
            column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            entry("Preferences", true, Self::show_preferences);
            entry("Keyboard Shortcuts", true, Self::show_shortcuts);
            popover.set_child(Some(&column));
            button.set_popover(Some(&popover));
        });
        button
    }

    pub(super) fn show_preferences(self: &Rc<Self>) {
        let window = dialog(&self.window, "Preferences", 480, 420);
        let current = prefs::get();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.add_css_class("dialog-body");

        let switch = |on: bool| {
            let s = gtk::Switch::new();
            s.set_active(on);
            s
        };
        let hidden = switch(current.show_hidden);
        let folders = switch(current.folders_first);
        let thumbs = switch(current.thumbnails);
        let restore = switch(current.restore_tabs);
        let terminal = gtk::Entry::builder()
            .text(current.terminal.as_str())
            .placeholder_text("Automatic")
            .width_chars(16)
            .build();
        body.append(&setting_row(
            "Show hidden files",
            "Names starting with a dot, and backups ending in ~ (Ctrl+H).",
            &hidden,
        ));
        body.append(&setting_row(
            "Folders before files",
            "Keep folders together at the top, whatever the sort order.",
            &folders,
        ));
        body.append(&setting_row(
            "Thumbnails",
            "Previews of images and videos instead of their icons.",
            &thumbs,
        ));
        body.append(&setting_row(
            "Reopen tabs",
            "Start with the tabs and folders that were open last time.",
            &restore,
        ));
        body.append(&setting_row(
            "Terminal",
            "Command for Open in Terminal. Automatic tries $TERMINAL, then alacritty, foot, kitty, wezterm.",
            &terminal,
        ));
        window.set_child(Some(&body));

        // Each change applies at once and is saved.
        let save = {
            let w = Rc::downgrade(self);
            let (hidden, folders, thumbs, restore, terminal) = (
                hidden.clone(),
                folders.clone(),
                thumbs.clone(),
                restore.clone(),
                terminal.clone(),
            );
            move || {
                let Some(win) = w.upgrade() else { return };
                let before = prefs::get();
                let now = Prefs {
                    show_hidden: hidden.is_active(),
                    folders_first: folders.is_active(),
                    thumbnails: thumbs.is_active(),
                    restore_tabs: restore.is_active(),
                    terminal: terminal.text().trim().to_owned(),
                };
                if now == before {
                    return;
                }
                if let Err(e) = prefs::set(now.clone()) {
                    win.show_error(&format!("Couldn't save preferences: {e}"));
                }
                win.apply_prefs(&before, &now);
            }
        };
        for s in [&hidden, &folders, &thumbs, &restore] {
            let save = save.clone();
            s.connect_active_notify(move |_| save());
        }
        terminal.connect_changed(move |_| save());
        window.present();
    }

    /// Makes changed preferences take effect in every tab.
    fn apply_prefs(self: &Rc<Self>, before: &Prefs, now: &Prefs) {
        if before.show_hidden != now.show_hidden {
            self.set_show_hidden(now.show_hidden);
        }
        if before.folders_first != now.folders_first {
            for tab in self.all_tabs() {
                tab.resort();
            }
        }
        if before.thumbnails != now.thumbnails {
            // Tiles fetch thumbnails as they're bound: bind them all again.
            for tab in self.all_tabs() {
                tab.refresh_tiles();
                tab.rebind_list();
            }
        }
    }

    pub(super) fn show_shortcuts(self: &Rc<Self>) {
        let window = dialog(&self.window, "Keyboard Shortcuts", 520, 640);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.add_css_class("dialog-body");
        for (group, keys) in SHORTCUTS {
            let title = gtk::Label::new(Some(group));
            title.set_xalign(0.0);
            title.add_css_class("section");
            body.append(&title);
            let grid = gtk::Grid::builder()
                .row_spacing(6)
                .column_spacing(18)
                .margin_bottom(8)
                .build();
            for (row, (accel, what)) in (0..).zip(keys.iter()) {
                let key = gtk::Label::new(Some(accel));
                key.set_xalign(0.0);
                key.add_css_class("shortcut-keys");
                let label = gtk::Label::new(Some(what));
                label.set_xalign(0.0);
                label.set_hexpand(true);
                grid.attach(&key, 0, row, 1, 1);
                grid.attach(&label, 1, row, 1, 1);
            }
            body.append(&grid);
        }
        let scroll = gtk::ScrolledWindow::builder()
            .child(&body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        window.set_child(Some(&scroll));
        window.present();
    }
}

impl Win {
    /// The sort menu on the toolbar: by what, and which way. It sets the
    /// list's column headers, which every view sorts by.
    pub(super) fn sort_menu_button(win: &Rc<Self>) -> gtk::MenuButton {
        let button = gtk::MenuButton::builder()
            .icon_name("view-sort-ascending-symbolic")
            .tooltip_text("Sort")
            .build();
        button.add_css_class("view-switch");
        let w = Rc::downgrade(win);
        button.set_create_popup_func(move |button| {
            let Some(win) = w.upgrade() else { return };
            let Some(tab) = win.tab() else { return };
            let (by, descending) = tab.sort_state();
            let popover = gtk::Popover::new();
            popover.add_css_class("menu");
            popover.add_css_class("sort-menu");
            popover.set_has_arrow(false);
            let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let choice = |label: &str, active: bool, group: Option<&gtk::CheckButton>| {
                let check = gtk::CheckButton::with_label(label);
                check.set_group(group);
                check.set_active(active);
                column.append(&check);
                check
            };
            let mut first: Option<gtk::CheckButton> = None;
            let mut fields = Vec::new();
            for title in ["Name", "Size", "Modified", "Type"] {
                let check = choice(title, by == title, first.as_ref());
                first.get_or_insert_with(|| check.clone());
                fields.push((title, check));
            }
            column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let ascending = choice("Ascending", !descending, None);
            let down = choice("Descending", descending, Some(&ascending));
            let apply = {
                let w = Rc::downgrade(&win);
                let fields = fields.clone();
                let down = down.clone();
                move || {
                    let Some(tab) = w.upgrade().and_then(|win| win.tab()) else {
                        return;
                    };
                    let title = fields
                        .iter()
                        .find(|(_, c)| c.is_active())
                        .map_or("Name", |(t, _)| *t);
                    tab.sort_by(title, down.is_active());
                }
            };
            for check in fields.iter().map(|(_, c)| c).chain([&ascending, &down]) {
                let apply = apply.clone();
                check.connect_toggled(move |c| {
                    if c.is_active() {
                        apply();
                    }
                });
            }
            popover.set_child(Some(&column));
            button.set_popover(Some(&popover));
        });
        button
    }
}
