//! "Open With Other Application": a port of nautilus-app-chooser.c and
//! nautilus-app-chooser-widget.c without libadwaita.

use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

/// Called with the chosen app and, if "always use" failed, why.
pub type OnChosen = Box<dyn Fn(gio::AppInfo, Option<String>)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Default,
    Recommended,
    Related,
    Other,
}

impl Group {
    const fn title(self) -> &'static str {
        match self {
            Self::Default => "Default App",
            Self::Recommended => "Recommended Apps",
            Self::Related => "Related Apps",
            Self::Other => "Other Apps",
        }
    }
}

/// Apps in Nautilus' order: default, recommended, fallback, then the rest,
/// each listed once.
fn collect(content_type: Option<&str>) -> Vec<(Group, gio::AppInfo)> {
    let mut out: Vec<(Group, gio::AppInfo)> = Vec::new();
    let mut push = |group, app: gio::AppInfo| {
        if !out.iter().any(|(_, seen)| seen.equal(&app)) {
            out.push((group, app));
        }
    };
    if let Some(ct) = content_type {
        if let Some(app) = gio::AppInfo::default_for_type(ct, false) {
            push(Group::Default, app);
        }
        for app in gio::AppInfo::recommended_for_type(ct) {
            push(Group::Recommended, app);
        }
        for app in gio::AppInfo::fallback_for_type(ct) {
            push(Group::Related, app);
        }
    }
    let mut others: Vec<gio::AppInfo> = gio::AppInfo::all()
        .into_iter()
        .filter(AppInfoExt::should_show)
        .collect();
    others.sort_by_key(|a| a.display_name().to_lowercase());
    for app in others {
        push(Group::Other, app);
    }
    out
}

fn app_row(group: Group, app: &gio::AppInfo) -> gtk::ListBoxRow {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = app.icon().map_or_else(
        || gtk::Image::from_icon_name("application-x-executable"),
        |i| gtk::Image::from_gicon(&i),
    );
    icon.set_pixel_size(28);
    let label = gtk::Label::new(Some(&app.display_name()));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    line.append(&icon);
    line.append(&label);
    let row = gtk::ListBoxRow::builder().child(&line).build();
    row.set_widget_name(group.title());
    row
}

fn section_headers(row: &gtk::ListBoxRow, before: Option<&gtk::ListBoxRow>) {
    let group = row.widget_name();
    if before.is_some_and(|b| b.widget_name() == group) {
        row.set_header(None::<&gtk::Widget>);
    } else {
        let label = gtk::Label::new(Some(&group));
        label.set_xalign(0.0);
        label.add_css_class("section");
        row.set_header(Some(&label));
    }
}

struct Chooser {
    window: gtk::Window,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    always: gtk::CheckButton,
    open: gtk::Button,
    cancel: gtk::Button,
    apps: Vec<(Group, gio::AppInfo)>,
    content_type: Option<String>,
}

impl Chooser {
    fn build(parent: &gtk::Window, content_type: Option<String>, subject: &str) -> Self {
        let title = if content_type.is_none() {
            "Open Items"
        } else {
            "Open File"
        };
        let window = gtk::Window::builder()
            .title(title)
            .transient_for(parent)
            .modal(true)
            .default_width(440)
            .default_height(560)
            .build();
        window.add_css_class("abyss");
        window.add_css_class("chooser");

        let description = gtk::Label::new(None);
        description.set_markup(&format!(
            "Choose an app to open <b>{}</b>",
            glib::markup_escape_text(subject)
        ));
        description.set_xalign(0.0);
        description.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search apps")
            .build();

        let apps = collect(content_type.as_deref());
        let list = gtk::ListBox::new();
        list.add_css_class("sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);
        for (group, app) in &apps {
            list.append(&app_row(*group, app));
        }
        list.set_header_func(section_headers);
        let empty = gtk::Label::new(Some("No apps found"));
        empty.add_css_class("empty");
        list.set_placeholder(Some(&empty));
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        // "Always use" only makes sense when all items share one type.
        let always = gtk::CheckButton::with_label("Always use for this file type");
        always.set_visible(content_type.is_some());
        let cancel = gtk::Button::with_label("Cancel");
        let open = gtk::Button::with_label("Open");
        open.add_css_class("suggested-action");
        open.set_sensitive(false);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&always);
        buttons.append(&spacer);
        buttons.append(&cancel);
        buttons.append(&open);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.add_css_class("chooser-body");
        content.append(&description);
        content.append(&search);
        content.append(&scroll);
        content.append(&buttons);
        window.set_child(Some(&content));

        Self {
            window,
            search,
            list,
            always,
            open,
            cancel,
            apps,
            content_type,
        }
    }

    fn app_at(&self, row: &gtk::ListBoxRow) -> Option<&gio::AppInfo> {
        let index = usize::try_from(row.index()).ok()?;
        self.apps.get(index).map(|(_, app)| app)
    }

    fn selected(&self) -> Option<&gio::AppInfo> {
        self.list.selected_row().and_then(|r| self.app_at(&r))
    }

    fn matches(&self, row: &gtk::ListBoxRow) -> bool {
        let needle = self.search.text().to_lowercase();
        needle.is_empty()
            || self.app_at(row).is_some_and(|app| {
                app.display_name().to_lowercase().contains(&needle)
                    || app
                        .id()
                        .is_some_and(|id| id.to_lowercase().contains(&needle))
            })
    }

    /// The default app can't be made default again (`on_application_selected`).
    fn update_buttons(&self) {
        let app = self.selected();
        self.open.set_sensitive(app.is_some());
        let is_default = match (app, &self.content_type) {
            (Some(app), Some(ct)) => {
                gio::AppInfo::default_for_type(ct, false).is_some_and(|d| d.equal(app))
            }
            _ => false,
        };
        self.always.set_active(is_default);
        self.always.set_sensitive(!is_default);
    }

    /// Opens with the selected app, first making it the default if asked
    /// (`app_chosen`).
    fn choose(&self, on_chosen: &OnChosen) {
        let Some(app) = self.selected().cloned() else {
            return;
        };
        let make_default =
            self.always.is_visible() && self.always.is_sensitive() && self.always.is_active();
        let mut note = None;
        if let (Some(ct), true) = (&self.content_type, make_default) {
            if let Err(e) = app.set_as_default_for_type(ct) {
                note = Some(format!(
                    "Couldn't make {} the default: {e}",
                    app.display_name()
                ));
            }
        }
        self.window.close();
        on_chosen(app, note);
    }

    /// Selects the first row the search leaves visible.
    fn select_first_visible(&self) -> bool {
        let mut i = 0;
        while let Some(row) = self.list.row_at_index(i) {
            if row.is_child_visible() {
                self.list.select_row(Some(&row));
                return true;
            }
            i += 1;
        }
        false
    }

    fn wire(self: &Rc<Self>, on_chosen: OnChosen) {
        let on_chosen = Rc::new(on_chosen);

        let me = Rc::downgrade(self);
        self.list
            .set_filter_func(move |row| me.upgrade().is_none_or(|c| c.matches(row)));
        let me = Rc::downgrade(self);
        self.search.connect_search_changed(move |_| {
            if let Some(c) = me.upgrade() {
                c.list.invalidate_filter();
            }
        });
        let me = Rc::downgrade(self);
        self.list.connect_row_selected(move |_, _| {
            if let Some(c) = me.upgrade() {
                c.update_buttons();
            }
        });

        let (me, chosen) = (Rc::downgrade(self), on_chosen.clone());
        self.open.connect_clicked(move |_| {
            if let Some(c) = me.upgrade() {
                c.choose(&chosen);
            }
        });
        let (me, chosen) = (Rc::downgrade(self), on_chosen.clone());
        self.list.connect_row_activated(move |_, _| {
            if let Some(c) = me.upgrade() {
                c.choose(&chosen);
            }
        });
        // Enter in the search box opens the first match.
        let (me, chosen) = (Rc::downgrade(self), on_chosen);
        self.search.connect_activate(move |_| {
            if let Some(c) = me.upgrade() {
                if c.select_first_visible() {
                    c.choose(&chosen);
                }
            }
        });

        let window = self.window.clone();
        self.cancel.connect_clicked(move |_| window.close());
        let window = self.window.clone();
        self.search.connect_stop_search(move |_| window.close());
        let keys = gtk::EventControllerKey::new();
        let window = self.window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);
        self.search.set_key_capture_widget(Some(&self.window));

        // The window owns the chooser until it closes.
        let keep = std::cell::RefCell::new(Some(self.clone()));
        self.window.connect_close_request(move |_| {
            keep.take();
            glib::Propagation::Proceed
        });
    }
}

/// Shows the chooser. `content_type` is the type all the items share, if
/// they do; `subject` names them in the description.
pub fn show(
    parent: &gtk::Window,
    content_type: Option<String>,
    subject: &str,
    on_chosen: OnChosen,
) {
    let chooser = Rc::new(Chooser::build(parent, content_type, subject));
    chooser.wire(on_chosen);
    if let Some(first) = chooser.list.row_at_index(0) {
        chooser.list.select_row(Some(&first));
    }
    chooser.window.present();
    chooser.search.grab_focus();
}
