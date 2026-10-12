//! Properties (Alt+Enter): what a file or folder is, how big, where, when,
//! whose, who may do what with it, what opens it, and how much room the
//! disk it's on has left. Several items at once show what they share.

use super::*;

/// What's asked about each item.
const ATTRS: &str = "standard::display-name,standard::type,standard::size,standard::icon,\
standard::content-type,standard::fast-content-type,standard::is-symlink,\
standard::symlink-target,time::modified,time::access,time::created,unix::mode,\
owner::user,owner::group";

/// The permission bits, as (who, what, bit).
const BITS: [(&str, &str, u32); 9] = [
    ("Owner", "Read", 0o400),
    ("Owner", "Write", 0o200),
    ("Owner", "Access", 0o100),
    ("Group", "Read", 0o040),
    ("Group", "Write", 0o020),
    ("Group", "Access", 0o010),
    ("Others", "Read", 0o004),
    ("Others", "Write", 0o002),
    ("Others", "Access", 0o001),
];

fn when(info: &gio::FileInfo, key: &str) -> Option<String> {
    if !info.has_attribute(key) {
        return None;
    }
    let secs = i64::try_from(info.attribute_uint64(key)).ok()?;
    let time = glib::DateTime::from_unix_local(secs).ok()?;
    time.format("%-d %B %Y, %H:%M").ok().map(|t| t.to_string())
}

/// "1,234": digits grouped by thousands.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "3.4 MB (3,412,345 bytes)".
fn size_words(bytes: u64) -> String {
    if bytes < 1000 {
        format!("{bytes} bytes")
    } else {
        format!("{} ({} bytes)", glib::format_size(bytes), grouped(bytes))
    }
}

/// The two-column table: a dim caption, then its value.
struct Table {
    grid: gtk::Grid,
    row: Cell<i32>,
}

impl Table {
    fn new() -> Self {
        let grid = gtk::Grid::builder()
            .row_spacing(8)
            .column_spacing(16)
            .build();
        Self {
            grid,
            row: Cell::new(0),
        }
    }

    fn add(&self, caption: &str, value: &impl IsA<gtk::Widget>) {
        let label = gtk::Label::new(Some(caption));
        label.set_xalign(1.0);
        label.set_valign(gtk::Align::Start);
        label.add_css_class("dim-label");
        let row = self.row.get();
        self.grid.attach(&label, 0, row, 1, 1);
        self.grid.attach(value, 1, row, 1, 1);
        self.row.set(row + 1);
    }

    fn text(&self, caption: &str, text: &str) -> gtk::Label {
        let value = gtk::Label::new(Some(text));
        value.set_xalign(0.0);
        value.set_wrap(true);
        value.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        value.set_selectable(true);
        value.set_hexpand(true);
        self.add(caption, &value);
        value
    }
}

impl Win {
    /// Properties of the selection, or of the folder shown when nothing is
    /// selected.
    pub(super) fn show_properties(self: &Rc<Self>) {
        let Some(tab) = self.tab() else { return };
        let mut files = tab.selected_files();
        if files.is_empty() {
            files.push(tab.current.borrow().clone());
        }
        let win = self.clone();
        glib::spawn_future_local(async move {
            let mut items = Vec::with_capacity(files.len());
            for file in files {
                match file
                    .query_info_future(
                        ATTRS,
                        gio::FileQueryInfoFlags::NONE,
                        glib::Priority::DEFAULT,
                    )
                    .await
                {
                    Ok(info) => items.push((file, info)),
                    Err(e) => return win.show_error(&e.to_string()),
                }
            }
            win.properties_window(&items);
        });
    }

    #[allow(clippy::too_many_lines)]
    fn properties_window(self: &Rc<Self>, items: &[(gio::File, gio::FileInfo)]) {
        let Some((first_file, first)) = items.first().cloned() else {
            return;
        };
        let several = items.len() > 1;
        let title = if several {
            format!("{} Items", items.len())
        } else {
            first.display_name().to_string()
        };
        let window = prefs_ui::dialog(&self.window, &format!("{title} – Properties"), 460, 560);

        // Header: icon, name, kind.
        let icon = gtk::Image::from_gicon(&crate::info::icon(&first));
        icon.set_pixel_size(64);
        let name = gtk::Label::new(Some(&title));
        name.add_css_class("title-2");
        name.set_xalign(0.0);
        name.set_wrap(true);
        name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        let types: Vec<String> = items.iter().map(|(_, i)| tab::type_text(i)).collect();
        let kind = if types.iter().all(|t| *t == types[0]) {
            types[0].clone()
        } else {
            "Mixed types".to_owned()
        };
        let kind_label = gtk::Label::new(Some(&kind));
        kind_label.set_xalign(0.0);
        kind_label.add_css_class("dim-label");
        let names = gtk::Box::new(gtk::Orientation::Vertical, 4);
        names.set_valign(gtk::Align::Center);
        names.append(&name);
        names.append(&kind_label);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        header.append(&icon);
        header.append(&names);

        let table = Table::new();
        // Size: files at once; folders are counted in the background.
        let any_dir = items.iter().any(|(_, i)| is_dir(i));
        let direct: u64 = items.iter().map(|(_, i)| ops::size_of(i)).sum();
        let size = table.text(
            "Size",
            &if any_dir {
                "Counting…".to_owned()
            } else {
                size_words(direct)
            },
        );
        let mut counting = None;
        if any_dir {
            let files: Vec<gio::File> = items.iter().map(|(f, _)| f.clone()).collect();
            let roots = u64::try_from(items.iter().filter(|(_, i)| is_dir(i)).count()).unwrap_or(0);
            let size = size.downgrade();
            counting = Some(glib::spawn_future_local(async move {
                let (bytes, entries) = ops::count(&files).await;
                if let Some(size) = size.upgrade() {
                    // The folders themselves aren't items "in" them.
                    let inside = entries.saturating_sub(roots);
                    let noun = if inside == 1 { "item" } else { "items" };
                    size.set_text(&format!(
                        "{} {noun}, {}",
                        grouped(inside),
                        size_words(bytes)
                    ));
                }
            }));
        }

        // Where: the folder they're in, when they share one.
        let parents: Vec<Option<gio::File>> = items.iter().map(|(f, _)| f.parent()).collect();
        if let Some(Some(parent)) = parents.first() {
            if parents
                .iter()
                .all(|p| p.as_ref().is_some_and(|p| p.equal(parent)))
            {
                // "~/Pictures" for places under home.
                let home = glib::home_dir();
                let place = parent.path().map_or_else(
                    || parent.uri().to_string(),
                    |p| {
                        p.strip_prefix(&home).map_or_else(
                            |_| p.display().to_string(),
                            |r| format!("~/{}", r.display()),
                        )
                    },
                );
                table.text("Location", &place);
            }
        }
        if !several && first.is_symlink() {
            if let Some(target) = first.symlink_target() {
                table.text("Link to", &target.display().to_string());
            }
        }
        if !several {
            for (caption, key) in [
                ("Modified", "time::modified"),
                ("Accessed", "time::access"),
                ("Created", "time::created"),
            ] {
                if let Some(t) = when(&first, key) {
                    table.text(caption, &t);
                }
            }
        }
        let owner = first.attribute_string("owner::user");
        if let Some(user) = &owner {
            let same = items
                .iter()
                .all(|(_, i)| i.attribute_string("owner::user").as_ref() == Some(user));
            table.text("Owner", if same { user } else { "Several" });
        }
        if let Some(group) = first.attribute_string("owner::group") {
            table.text("Group", &group);
        }

        // Permissions, when the file system has them (not on every share).
        if items.iter().all(|(_, i)| i.has_attribute("unix::mode")) {
            let mine = items.iter().all(|(_, i)| {
                i.attribute_string("owner::user")
                    .is_some_and(|u| u.as_str() == glib::user_name().to_string_lossy())
            });
            table.add("Permissions", &self.permissions(items, mine));
        }

        // Open with: files that all have one type.
        let content_types: Vec<Option<glib::GString>> = items
            .iter()
            .map(|(_, i)| crate::info::content_type(i))
            .collect();
        if !any_dir {
            if let Some(Some(ct)) = content_types.first() {
                if content_types.iter().all(|c| c.as_ref() == Some(ct)) {
                    if let Some(choice) = self.default_app_choice(ct) {
                        table.add("Open with", &choice);
                    }
                }
            }
        }

        // Disk: room left where it is.
        let disk = table.text("Disk", "…");
        let where_ = if is_dir(&first) && !several {
            first_file
        } else {
            first_file.parent().unwrap_or(first_file)
        };
        let d = disk.downgrade();
        glib::spawn_future_local(async move {
            let info = where_
                .query_filesystem_info_future(
                    "filesystem::free,filesystem::size",
                    glib::Priority::LOW,
                )
                .await
                .ok();
            let Some(disk) = d.upgrade() else { return };
            match info.filter(|i| i.has_attribute("filesystem::free")) {
                Some(i) => {
                    let free = i.attribute_uint64("filesystem::free");
                    let size = i.attribute_uint64("filesystem::size");
                    disk.set_text(&if size > 0 {
                        format!(
                            "{} free of {}",
                            glib::format_size(free),
                            glib::format_size(size)
                        )
                    } else {
                        format!("{} free", glib::format_size(free))
                    });
                }
                None => disk.set_text("Unknown"),
            }
        });

        let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
        body.add_css_class("dialog-body");
        body.append(&header);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&table.grid);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .build();
        window.set_child(Some(&scroll));
        // A count still going stops with the window.
        let counting = RefCell::new(counting);
        window.connect_close_request(move |_| {
            if let Some(job) = counting.take() {
                job.abort();
            }
            glib::Propagation::Proceed
        });
        window.present();
        // Nothing focused: a selectable label taking focus shows selected.
        GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
    }

    /// Read / write / access for owner, group and others, as checkboxes that
    /// change the files at once. A box is half-ticked when the items differ.
    fn permissions(
        self: &Rc<Self>,
        items: &[(gio::File, gio::FileInfo)],
        editable: bool,
    ) -> gtk::Grid {
        let grid = gtk::Grid::builder()
            .row_spacing(4)
            .column_spacing(10)
            .build();
        for (col, what) in (1..).zip(["Read", "Write", "Access"]) {
            let head = gtk::Label::new(Some(what));
            head.add_css_class("dim-label");
            grid.attach(&head, col, 0, 1, 1);
        }
        let modes: Rc<RefCell<Vec<(gio::File, u32)>>> = Rc::new(RefCell::new(
            items
                .iter()
                .map(|(f, i)| (f.clone(), i.attribute_uint32("unix::mode") & 0o7777))
                .collect(),
        ));
        for (i, (who, what, bit)) in BITS.iter().enumerate() {
            let row = i32::try_from(i / 3).unwrap_or(0) + 1;
            let col = i32::try_from(i % 3).unwrap_or(0) + 1;
            if col == 1 {
                let label = gtk::Label::new(Some(who));
                label.set_xalign(0.0);
                grid.attach(&label, 0, row, 1, 1);
            }
            let check = gtk::CheckButton::new();
            check.set_tooltip_text(Some(&format!("{who}: {what}")));
            let set = modes.borrow().iter().filter(|(_, m)| m & bit != 0).count();
            check.set_active(set == modes.borrow().len());
            check.set_inconsistent(set != 0 && set != modes.borrow().len());
            check.set_sensitive(editable);
            check.set_halign(gtk::Align::Center);
            let (w, modes, bit) = (Rc::downgrade(self), modes.clone(), *bit);
            check.connect_toggled(move |check| {
                check.set_inconsistent(false);
                let on = check.is_active();
                let Some(win) = w.upgrade() else { return };
                let changes: Vec<(gio::File, u32)> = modes
                    .borrow_mut()
                    .iter_mut()
                    .map(|(f, m)| {
                        *m = if on { *m | bit } else { *m & !bit };
                        (f.clone(), *m)
                    })
                    .collect();
                glib::spawn_future_local(async move {
                    for (file, mode) in changes {
                        let info = gio::FileInfo::new();
                        info.set_attribute_uint32("unix::mode", mode);
                        if let Err(e) = file
                            .set_attributes_future(
                                &info,
                                gio::FileQueryInfoFlags::NONE,
                                glib::Priority::DEFAULT,
                            )
                            .await
                        {
                            win.show_error(&format!(
                                "Couldn't change permissions of “{}”: {}",
                                ops::display_name(&file),
                                e.message()
                            ));
                            return;
                        }
                    }
                });
            });
            grid.attach(&check, col, row, 1, 1);
        }
        if !editable {
            grid.set_tooltip_text(Some("Only the owner can change these"));
        }
        grid
    }

    /// The app that opens this type, as a list to pick another default from.
    fn default_app_choice(self: &Rc<Self>, content_type: &str) -> Option<gtk::DropDown> {
        let apps = gio::AppInfo::all_for_type(content_type);
        if apps.is_empty() {
            return None;
        }
        let default = gio::AppInfo::default_for_type(content_type, false);
        let names: Vec<String> = apps.iter().map(|a| a.display_name().to_string()).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let choice = gtk::DropDown::from_strings(&names);
        let current = default
            .as_ref()
            .and_then(|d| apps.iter().position(|a| a.equal(d)))
            .and_then(|i| u32::try_from(i).ok());
        choice.set_selected(current.unwrap_or(gtk::INVALID_LIST_POSITION));
        choice.set_tooltip_text(Some("The app files of this type open with"));
        let (w, content_type) = (Rc::downgrade(self), content_type.to_owned());
        choice.connect_selected_notify(move |choice| {
            let Some(app) = usize::try_from(choice.selected())
                .ok()
                .and_then(|i| apps.get(i))
            else {
                return;
            };
            if let (Err(e), Some(win)) = (app.set_as_default_for_type(&content_type), w.upgrade()) {
                win.show_error(&format!("Couldn't change the default app: {}", e.message()));
            }
        });
        Some(choice)
    }
}

#[cfg(test)]
mod tests {
    use super::{grouped, size_words};

    #[test]
    fn numbers_read_naturally() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1234), "1,234");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(size_words(512), "512 bytes");
        assert!(size_words(3_412_345).ends_with("(3,412,345 bytes)"));
    }
}
