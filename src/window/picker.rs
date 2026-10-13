//! Abyss as a file dialog. The file chooser portal (`crate::portal`) opens a
//! window in this mode: the usual sidebar, views and filter box, plus a bar
//! with the file name (when saving), the app's extra options, a file-type
//! filter and Cancel / accept. Behaviour follows GTK's file chooser
//! (gtkfilechooserwidget.c): a folder opens, a file is picked, and saving
//! over an existing file asks first. Nothing launches, and the browsing
//! session is left alone.

use super::*;

/// What the app asked for.
pub enum Purpose {
    /// Existing files, or folders with `directory`.
    Open { multiple: bool, directory: bool },
    /// One file to save, suggested name first.
    Save { name: String },
    /// A folder to save these files into.
    SaveFiles { names: Vec<String> },
}

/// A file-type filter as the portal sends it: (0, glob) or (1, mime type).
#[derive(Clone)]
pub struct Filter {
    pub name: String,
    pub rules: Vec<(u32, String)>,
}

impl Filter {
    fn matches(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        let guessed = std::cell::OnceCell::new();
        self.rules.iter().any(|(kind, rule)| match kind {
            0 => glob_match(&rule.to_lowercase(), &lower),
            1 => mime_matches(
                guessed.get_or_init(|| gio::content_type_guess(Some(name), None).0),
                rule,
            ),
            _ => false,
        })
    }
}

/// `*`, `?` and `[...]` (with ranges and `!`), as in GLib's patterns.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Backtracking over the last `*`: linear for the usual patterns.
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        let step = match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => class_match(&p[pi..], t[ti]),
            Some(&c) => (c == t[ti]).then_some(1),
            None => None,
        };
        if let Some(len) = step {
            pi += len;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Matches `c` against the class at the start of `p`; the class's length
/// if it does. An unclosed `[` is a plain character.
fn class_match(p: &[char], c: char) -> Option<usize> {
    let Some(end) = p.iter().skip(2).position(|&x| x == ']').map(|i| i + 2) else {
        return (c == '[').then_some(1);
    };
    let (negate, body) = match p[1] {
        '!' | '^' => (true, &p[2..end]),
        _ => (false, &p[1..end]),
    };
    let mut hit = false;
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            hit |= (body[i]..=body[i + 2]).contains(&c);
            i += 3;
        } else {
            hit |= body[i] == c;
            i += 1;
        }
    }
    (hit != negate).then_some(end + 1)
}

/// `image/*` matches any image; other types match their subtypes too.
fn mime_matches(content_type: &str, mime: &str) -> bool {
    mime.strip_suffix("/*").map_or_else(
        || gio::content_type_is_mime_type(content_type, mime),
        |group| {
            gio::content_type_get_mime_type(content_type)
                .is_some_and(|m| m.split('/').next() == Some(group))
        },
    )
}

/// An extra option: a checkbox without `options`, else a dropdown.
pub struct Choice {
    pub id: String,
    pub label: String,
    pub options: Vec<(String, String)>,
    pub initial: String,
}

pub struct Request {
    pub purpose: Purpose,
    pub title: String,
    pub accept_label: Option<String>,
    pub filters: Vec<Filter>,
    pub current_filter: Option<usize>,
    pub choices: Vec<Choice>,
    pub folder: Option<gio::File>,
    /// The app's window, as "wayland:<handle>".
    pub parent: Option<String>,
}

pub enum Outcome {
    Chosen {
        uris: Vec<String>,
        choices: Vec<(String, String)>,
        filter: Option<Filter>,
    },
    Cancelled,
}

enum ChoiceWidget {
    Check(gtk::CheckButton),
    Drop(gtk::DropDown, Vec<String>),
}

pub struct Picker {
    purpose: Purpose,
    filters: Vec<Filter>,
    /// The filter in use (an index into `filters`).
    filter: Cell<Option<usize>>,
    choices: Vec<(String, ChoiceWidget)>,
    bar: gtk::Box,
    name: gtk::Entry,
    accept: gtk::Button,
    cancel: gtk::Button,
    filter_drop: gtk::DropDown,
    done: RefCell<Option<Done>>,
}

/// Where the answer goes, once.
type Done = Box<dyn FnOnce(Outcome)>;

impl Picker {
    fn new(request: Request, done: Done) -> Self {
        let name = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("File name")
            .build();
        let name_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let name_label = gtk::Label::new(Some("Name"));
        name_label.add_css_class("dim-label");
        name_row.append(&name_label);
        name_row.append(&name);
        name_row.set_visible(matches!(request.purpose, Purpose::Save { .. }));
        if let Purpose::Save { name: suggested } = &request.purpose {
            name.set_text(suggested);
        }

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let choices: Vec<(String, ChoiceWidget)> = request
            .choices
            .into_iter()
            .map(|c| {
                let widget = if c.options.is_empty() {
                    let check = gtk::CheckButton::with_label(&c.label);
                    check.set_active(c.initial == "true");
                    row.append(&check);
                    ChoiceWidget::Check(check)
                } else {
                    let labels: Vec<&str> = c.options.iter().map(|(_, l)| l.as_str()).collect();
                    let drop = gtk::DropDown::from_strings(&labels);
                    let ids: Vec<String> = c.options.iter().map(|(id, _)| id.clone()).collect();
                    if let Some(i) = ids.iter().position(|id| *id == c.initial) {
                        drop.set_selected(u32::try_from(i).unwrap_or(0));
                    }
                    let label = gtk::Label::new(Some(&c.label));
                    label.add_css_class("dim-label");
                    row.append(&label);
                    row.append(&drop);
                    ChoiceWidget::Drop(drop, ids)
                };
                (c.id, widget)
            })
            .collect();

        // "All files" goes first only when the app didn't make one.
        let mut labels: Vec<&str> = request.filters.iter().map(|f| f.name.as_str()).collect();
        if labels.is_empty() {
            labels.push("All files");
        }
        let filter_drop = gtk::DropDown::from_strings(&labels);
        filter_drop.set_tooltip_text(Some("Show these files"));
        filter_drop.set_visible(request.filters.len() > 1);
        let current = request
            .current_filter
            .or_else(|| (!request.filters.is_empty()).then_some(0));
        if let Some(i) = current {
            filter_drop.set_selected(u32::try_from(i).unwrap_or(0));
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let cancel = gtk::Button::with_label("Cancel");
        let accept_label = request.accept_label.unwrap_or_else(|| {
            match request.purpose {
                Purpose::Open {
                    directory: true, ..
                } => "Select",
                Purpose::Open { .. } => "Open",
                Purpose::Save { .. } | Purpose::SaveFiles { .. } => "Save",
            }
            .to_owned()
        });
        // GTK marks mnemonics with "_"; the button shows them as such.
        let accept = gtk::Button::with_mnemonic(&accept_label);
        accept.add_css_class("suggested-action");
        row.append(&spacer);
        row.append(&filter_drop);
        row.append(&cancel);
        row.append(&accept);

        let bar = gtk::Box::new(gtk::Orientation::Vertical, 8);
        bar.add_css_class("prompt");
        bar.add_css_class("picker");
        bar.append(&name_row);
        bar.append(&row);
        Self {
            purpose: request.purpose,
            filters: request.filters,
            filter: Cell::new(current),
            choices,
            bar,
            name,
            accept,
            cancel,
            filter_drop,
            done: RefCell::new(Some(done)),
        }
    }

    /// Whether `info` is listed: folders always (only folders when picking
    /// folders), files when the filter in use takes them.
    fn shows(&self, info: &gio::FileInfo) -> bool {
        if is_dir(info) {
            return true;
        }
        if matches!(
            self.purpose,
            Purpose::Open {
                directory: true,
                ..
            }
        ) {
            return false;
        }
        self.filter
            .get()
            .and_then(|i| self.filters.get(i))
            .is_none_or(|f| f.matches(&info.display_name()))
    }

    fn chosen_options(&self) -> Vec<(String, String)> {
        self.choices
            .iter()
            .map(|(id, widget)| {
                let value = match widget {
                    ChoiceWidget::Check(check) => check.is_active().to_string(),
                    ChoiceWidget::Drop(drop, ids) => usize::try_from(drop.selected())
                        .ok()
                        .and_then(|i| ids.get(i).cloned())
                        .unwrap_or_default(),
                };
                (id.clone(), value)
            })
            .collect()
    }
}

/// The name without its extension, for preselecting in the name box.
fn stem_len(name: &str) -> i32 {
    let stem = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    i32::try_from(stem.chars().count()).unwrap_or(i32::MAX)
}

impl Win {
    /// Opens a window that picks files for another app and reports the
    /// choice (or Cancel) to `done`, once. Returned so the portal can close
    /// it when the app gives up.
    pub fn open_picker(
        app: &gtk::Application,
        request: Request,
        done: impl FnOnce(Outcome) + 'static,
    ) -> Rc<Self> {
        let title = request.title.clone();
        let parent = request.parent.clone();
        let start = request.folder.clone();
        let (win, adds) = Self::build(app);
        let picker = Rc::new(Picker::new(request, Box::new(done)));
        win.picker.replace(Some(picker.clone()));
        Self::wire(&win, adds);
        win.split_btn.set_visible(false);
        if let Some(main) = win.status.parent().and_then(|footer| footer.parent()) {
            if let Some(main) = main.downcast_ref::<gtk::Box>() {
                let footer = win.status.parent();
                main.insert_child_after(
                    &picker.bar,
                    footer.and_then(|f| f.prev_sibling()).as_ref(),
                );
            }
        }
        Self::wire_picker(&win, &picker);

        let start = start
            .filter(|f| {
                f.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                    == gio::FileType::Directory
            })
            .unwrap_or_else(|| gio::File::for_path(glib::home_dir()));
        win.open_tab(0, start, true);
        win.refresh_sidebar();
        win.refresh_peers();
        win.window.set_title(Some(&title));

        // Over the app that asked (niri floats it above that window).
        if let Some(handle) = parent.as_deref().and_then(|p| p.strip_prefix("wayland:")) {
            let handle = handle.to_owned();
            win.window.connect_realize(move |window| {
                if let Some(top) = window
                    .surface()
                    .and_then(|s| s.downcast::<gdk4_wayland::WaylandToplevel>().ok())
                {
                    top.set_transient_for_exported(&handle);
                }
            });
        }
        let keep = RefCell::new(Some(win.clone()));
        win.window.connect_destroy(move |_| {
            keep.take();
        });
        win.window.present();
        if matches!(picker.purpose, Purpose::Save { .. }) {
            picker.name.grab_focus();
            picker.name.select_region(0, stem_len(&picker.name.text()));
        } else {
            win.focus_view();
        }
        win
    }

    fn wire_picker(win: &Rc<Self>, picker: &Picker) {
        let w = Rc::downgrade(win);
        picker.accept.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.picker_accept();
            }
        });
        let w = Rc::downgrade(win);
        picker.name.connect_activate(move |_| {
            if let Some(win) = w.upgrade() {
                win.picker_accept();
            }
        });
        let w = Rc::downgrade(win);
        picker.cancel.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.window.close();
            }
        });
        let w = Rc::downgrade(win);
        picker.filter_drop.connect_selected_notify(move |drop| {
            let Some(win) = w.upgrade() else { return };
            let Some(picker) = win.picker() else { return };
            let i = usize::try_from(drop.selected()).ok();
            picker.filter.set(i.filter(|&i| i < picker.filters.len()));
            for tab in win.all_tabs() {
                tab.filter.changed(gtk::FilterChange::Different);
            }
        });
        // Closing the window any other way is Cancel.
        let w = Rc::downgrade(win);
        win.window.connect_close_request(move |_| {
            if let Some(picker) = w.upgrade().and_then(|win| win.picker()) {
                if let Some(done) = picker.done.take() {
                    done(Outcome::Cancelled);
                }
            }
            glib::Propagation::Proceed
        });
    }

    /// Closes the window (a file dialog: as Cancel).
    pub fn close(&self) {
        self.window.close();
    }

    pub(super) fn picker(&self) -> Option<Rc<Picker>> {
        self.picker.borrow().clone()
    }

    pub(super) fn picker_shows(&self, info: &gio::FileInfo) -> bool {
        self.picker.borrow().as_ref().is_none_or(|p| p.shows(info))
    }

    /// Double-click / Enter on files while picking: they're the answer (a
    /// folder has already opened). True while picking: nothing launches.
    pub(super) fn picker_activate(self: &Rc<Self>, targets: &[(gio::FileInfo, gio::File)]) -> bool {
        let Some(picker) = self.picker() else {
            return false;
        };
        // The activated files, which needn't be selected (Enter after
        // typing in the filter acts on the row under the cursor).
        let files: Vec<&(gio::FileInfo, gio::File)> =
            targets.iter().filter(|(info, _)| !is_dir(info)).collect();
        match &picker.purpose {
            Purpose::Open {
                multiple,
                directory: false,
            } if !files.is_empty() => {
                let take = if *multiple { files.len() } else { 1 };
                let chosen: Vec<gio::File> =
                    files.iter().take(take).map(|(_, f)| f.clone()).collect();
                self.picker_finish(&chosen);
            }
            Purpose::Save { .. } => {
                if let [(info, _)] = files.as_slice() {
                    picker.name.set_text(&info.display_name());
                    self.picker_accept();
                }
            }
            _ => {}
        }
        true
    }

    /// The accept button: check the choice, then answer.
    fn picker_accept(self: &Rc<Self>) {
        let (Some(picker), Some(tab)) = (self.picker(), self.tab()) else {
            return;
        };
        let here = tab.current.borrow().clone();
        let selected = tab.selected();
        let folders: Vec<gio::File> = selected
            .iter()
            .filter(|(info, _)| is_dir(info))
            .map(|(_, f)| f.clone())
            .collect();
        match &picker.purpose {
            Purpose::Open {
                multiple,
                directory: true,
            } => {
                let mut chosen = if folders.is_empty() {
                    vec![here]
                } else {
                    folders
                };
                if !multiple {
                    chosen.truncate(1);
                }
                self.picker_finish(&chosen);
            }
            Purpose::Open { multiple, .. } => {
                let mut files: Vec<gio::File> = selected
                    .iter()
                    .filter(|(info, _)| !is_dir(info))
                    .map(|(_, f)| f.clone())
                    .collect();
                if files.is_empty() {
                    // Only a folder selected: open it, as GTK does.
                    if let [folder] = folders.as_slice() {
                        tab.navigate(folder.clone(), true);
                    }
                    return;
                }
                if !multiple {
                    files.truncate(1);
                }
                self.picker_finish(&files);
            }
            Purpose::Save { .. } => {
                let text = picker.name.text().trim().to_owned();
                if text.is_empty() {
                    picker.name.grab_focus();
                    return;
                }
                let target = if text.starts_with('/') || text.starts_with('~') {
                    tab.parse_location(&text)
                } else {
                    here.resolve_relative_path(&text)
                };
                let kind =
                    target.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE);
                if kind == gio::FileType::Directory {
                    // A folder's name opens it; the name box waits for a file.
                    tab.navigate(target, true);
                    picker.name.set_text("");
                    picker.name.grab_focus();
                } else if kind == gio::FileType::Unknown {
                    self.picker_finish(&[target]);
                } else {
                    let name = ops::display_name(&target);
                    self.ask(
                        &format!("“{name}” already exists. Replace it?"),
                        None,
                        "Replace",
                        move |win, _| win.picker_finish(std::slice::from_ref(&target)),
                    );
                }
            }
            Purpose::SaveFiles { names } => {
                let folder = match folders.as_slice() {
                    [one] => one.clone(),
                    _ => here,
                };
                let files: Vec<gio::File> = names.iter().map(|n| folder.child(n)).collect();
                self.picker_finish(&files);
            }
        }
    }

    fn picker_finish(&self, files: &[gio::File]) {
        let Some(picker) = self.picker() else { return };
        let Some(done) = picker.done.take() else {
            return;
        };
        done(Outcome::Chosen {
            uris: files.iter().map(|f| f.uri().to_string()).collect(),
            choices: picker.chosen_options(),
            filter: picker
                .filter
                .get()
                .and_then(|i| picker.filters.get(i).cloned()),
        });
        self.window.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("*.png", "cat.png"));
        assert!(!glob_match("*.png", "cat.png.txt"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*.[jp]pg", "x.jpg"));
        assert!(!glob_match("*.[!jp]pg", "x.jpg"));
        assert!(glob_match("img[0-9].*", "img7.webp"));
        assert!(glob_match("*a*b*", "xxaxxbxx"));
        assert!(glob_match("[", "["));
    }

    #[test]
    fn filters() {
        let images = Filter {
            name: "Images".into(),
            rules: vec![(0, "*.PNG".into()), (1, "image/jpeg".into())],
        };
        assert!(images.matches("Shot.png"));
        assert!(images.matches("photo.JPG"));
        assert!(!images.matches("notes.txt"));
        let any_image = Filter {
            name: "Any".into(),
            rules: vec![(1, "image/*".into())],
        };
        assert!(any_image.matches("a.webp"));
        assert!(!any_image.matches("a.pdf"));
    }

    #[test]
    fn stems() {
        assert_eq!(stem_len("report.pdf"), 6);
        assert_eq!(stem_len(".bashrc"), 7);
        assert_eq!(stem_len("archive"), 7);
    }
}
