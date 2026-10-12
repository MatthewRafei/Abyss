//! Batch rename (F2 with several items selected): a bar that slides up from
//! the bottom, like the sign-in bar, with a few simple rules (replace text,
//! add text, number, change case) and a preview of every name before and
//! after. Extensions are kept; nothing is renamed while a name has a problem.

use std::collections::{HashMap, HashSet};

use super::*;

/// Rows the preview shows; the summary still counts them all.
const PREVIEW_ROWS: usize = 200;

/// How names change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Rule {
    Replace {
        find: String,
        with: String,
        match_case: bool,
    },
    Add {
        before: String,
        after: String,
    },
    Number {
        /// The new name; empty means just the number.
        name: String,
        start: u32,
        digits: usize,
        /// Keep each name and add the number after it.
        keep: bool,
    },
    Case(Case),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Case {
    Lower,
    Upper,
    Title,
}

/// Each name's stem (what rules change) and extension (kept). Folders
/// have no extension.
fn split(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        (name, "")
    } else {
        name.split_at(crate::filename::extension_start(name))
    }
}

/// `text` with every `find` replaced, ignoring case unless `match_case`.
fn replace(text: &str, find: &str, with: &str, match_case: bool) -> String {
    if find.is_empty() {
        return text.to_owned();
    }
    if match_case {
        return text.replace(find, with);
    }
    let lower = text.to_lowercase();
    let needle = find.to_lowercase();
    // Lowercasing can change byte lengths (rarely): then match exactly.
    if lower.len() != text.len() {
        return text.replace(find, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find(&needle) {
        out.push_str(&text[at..at + i]);
        out.push_str(with);
        at += i + needle.len();
    }
    out.push_str(&text[at..]);
    out
}

fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = true;
    for c in text.chars() {
        if start {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start = !c.is_alphanumeric() && c != '\'';
    }
    out
}

/// The new names for `items` (name, is folder), in order.
pub(super) fn plan(items: &[(String, bool)], rule: &Rule) -> Vec<String> {
    items
        .iter()
        .zip(0_u32..)
        .map(|((name, is_dir), i)| {
            let (stem, ext) = split(name, *is_dir);
            let stem = match rule {
                Rule::Replace {
                    find,
                    with,
                    match_case,
                } => replace(stem, find, with, *match_case),
                Rule::Add { before, after } => format!("{before}{stem}{after}"),
                Rule::Number {
                    name,
                    start,
                    digits,
                    keep,
                } => {
                    let n = format!("{:0digits$}", start.saturating_add(i));
                    let base = if *keep { stem } else { name.trim() };
                    if base.is_empty() {
                        n
                    } else {
                        format!("{base} {n}")
                    }
                }
                Rule::Case(Case::Lower) => stem.to_lowercase(),
                Rule::Case(Case::Upper) => stem.to_uppercase(),
                Rule::Case(Case::Title) => title_case(stem),
            };
            format!("{stem}{ext}")
        })
        .collect()
}

/// What's wrong with each new name, if anything: invalid, the same as
/// another new name, or taken by a file that isn't being renamed
/// (`taken`, names in the folder).
pub(super) fn problems(
    old: &[String],
    new: &[String],
    taken: &HashSet<String>,
) -> Vec<Option<String>> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for name in new {
        *seen.entry(name.as_str()).or_default() += 1;
    }
    let renamed: HashSet<&str> = old.iter().map(String::as_str).collect();
    old.iter()
        .zip(new)
        .map(|(before, after)| {
            if let Err(e) = crate::filename::validate(after) {
                return Some(e.to_owned());
            }
            if seen.get(after.as_str()).copied().unwrap_or(0) > 1 {
                return Some("Another file gets this name too".to_owned());
            }
            if after != before && taken.contains(after) && !renamed.contains(after.as_str()) {
                return Some("A file with this name is already there".to_owned());
            }
            None
        })
        .collect()
}

/// The bar's widgets and what it's renaming.
pub(super) struct Bar {
    revealer: gtk::Revealer,
    head: gtk::Box,
    title: gtk::Label,
    modes: [gtk::ToggleButton; 4],
    pages: gtk::Stack,
    find: gtk::Entry,
    with: gtk::Entry,
    match_case: gtk::CheckButton,
    before: gtk::Entry,
    after: gtk::Entry,
    name: gtk::Entry,
    start: gtk::SpinButton,
    digits: gtk::SpinButton,
    keep: gtk::CheckButton,
    cases: [gtk::CheckButton; 3],
    preview: gtk::ListBox,
    summary: gtk::Label,
    rename: gtk::Button,
    cancel: gtk::Button,
    /// The files, their names now, and whether each is a folder.
    files: RefCell<Vec<(gio::File, String, bool)>>,
    /// Names already in the folder that aren't being renamed.
    taken: RefCell<HashSet<String>>,
}

fn entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .placeholder_text(placeholder)
        .hexpand(true)
        .build()
}

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label
}

impl Bar {
    #[allow(clippy::too_many_lines)]
    pub(super) fn new() -> Rc<Self> {
        let title = gtk::Label::new(None);
        title.add_css_class("conflict-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);

        // The rule: one of four, as linked buttons.
        let labels = ["Replace", "Add text", "Number", "Change case"];
        let modes = labels.map(gtk::ToggleButton::with_label);
        for m in &modes[1..] {
            m.set_group(Some(&modes[0]));
        }
        modes[0].set_active(true);
        let switch = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        switch.add_css_class("linked");
        for m in &modes {
            switch.append(m);
        }
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        head.append(&title);
        head.append(&switch);

        // Each rule's fields.
        let row = || {
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            b.add_css_class("rename-fields");
            b
        };
        let find = entry("Text to find");
        let with = entry("Replace with (empty removes it)");
        let match_case = gtk::CheckButton::with_label("Match case");
        let replace_row = row();
        replace_row.append(&caption("Find"));
        replace_row.append(&find);
        replace_row.append(&caption("Replace with"));
        replace_row.append(&with);
        replace_row.append(&match_case);

        let before = entry("Text before each name");
        let after = entry("Text after (before the extension)");
        let add_row = row();
        add_row.append(&caption("Before"));
        add_row.append(&before);
        add_row.append(&caption("After"));
        add_row.append(&after);

        let name = entry("New name, e.g. Holiday");
        let start = gtk::SpinButton::with_range(0.0, 999_999.0, 1.0);
        start.set_value(1.0);
        let digits = gtk::SpinButton::with_range(1.0, 6.0, 1.0);
        digits.set_value(2.0);
        let keep = gtk::CheckButton::with_label("Keep names, add the number after them");
        let number_row = row();
        number_row.append(&caption("Name"));
        number_row.append(&name);
        number_row.append(&caption("Start at"));
        number_row.append(&start);
        number_row.append(&caption("Digits"));
        number_row.append(&digits);
        number_row.append(&keep);

        let lower = gtk::CheckButton::with_label("lowercase");
        let upper = gtk::CheckButton::with_label("UPPERCASE");
        let title_btn = gtk::CheckButton::with_label("Title Case");
        upper.set_group(Some(&lower));
        title_btn.set_group(Some(&lower));
        lower.set_active(true);
        let case_row = row();
        case_row.append(&lower);
        case_row.append(&upper);
        case_row.append(&title_btn);

        let pages = gtk::Stack::new();
        pages.set_transition_type(gtk::StackTransitionType::Crossfade);
        pages.add_named(&replace_row, Some("Replace"));
        pages.add_named(&add_row, Some("Add text"));
        pages.add_named(&number_row, Some("Number"));
        pages.add_named(&case_row, Some("Change case"));

        // Before → after, for every name.
        let preview = gtk::ListBox::new();
        preview.add_css_class("rename-preview");
        preview.set_selection_mode(gtk::SelectionMode::None);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&preview)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(220)
            .propagate_natural_height(true)
            .build();

        let summary = gtk::Label::new(None);
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        summary.set_wrap(true);
        summary.add_css_class("dim-label");
        let cancel = gtk::Button::with_label("Cancel");
        let rename = gtk::Button::with_label("Rename");
        rename.add_css_class("suggested-action");
        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        foot.append(&summary);
        foot.append(&cancel);
        foot.append(&rename);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.add_css_class("prompt");
        body.add_css_class("batch-rename");
        body.append(&head);
        body.append(&pages);
        body.append(&scroll);
        body.append(&foot);
        let revealer = gtk::Revealer::builder()
            .child(&body)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        let bar = Rc::new(Self {
            revealer,
            head,
            title,
            modes,
            pages,
            find,
            with,
            match_case,
            before,
            after,
            name,
            start,
            digits,
            keep,
            cases: [lower, upper, title_btn],
            preview,
            summary,
            rename,
            cancel,
            files: RefCell::default(),
            taken: RefCell::default(),
        });
        bar.wire();
        bar
    }

    pub(super) const fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    /// Narrow window: the mode switch goes under the title, and each rule's
    /// fields stack instead of sharing a row.
    pub(super) fn set_compact(&self, compact: bool) {
        let o = if compact {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        };
        self.head.set_orientation(o);
        let mut page = self.pages.first_child();
        while let Some(row) = page {
            if let Some(row) = row.downcast_ref::<gtk::Box>() {
                row.set_orientation(o);
            }
            page = row.next_sibling();
        }
    }

    /// Every field updates the preview as it changes.
    fn wire(self: &Rc<Self>) {
        let update = {
            let me = Rc::downgrade(self);
            move || {
                if let Some(bar) = me.upgrade() {
                    bar.update();
                }
            }
        };
        for (mode, label) in self
            .modes
            .iter()
            .zip(["Replace", "Add text", "Number", "Change case"])
        {
            let (update, pages) = (update.clone(), self.pages.clone());
            mode.connect_toggled(move |m| {
                if m.is_active() {
                    pages.set_visible_child_name(label);
                    update();
                }
            });
        }
        for e in [
            &self.find,
            &self.with,
            &self.before,
            &self.after,
            &self.name,
        ] {
            let update = update.clone();
            e.connect_changed(move |_| update());
        }
        for s in [&self.start, &self.digits] {
            let update = update.clone();
            s.connect_value_changed(move |_| update());
        }
        for c in self.cases.iter().chain([&self.match_case, &self.keep]) {
            let update = update.clone();
            c.connect_toggled(move |_| update());
        }
        let me = Rc::downgrade(self);
        let keep_update = update;
        self.keep.connect_toggled(move |k| {
            if let Some(bar) = me.upgrade() {
                bar.name.set_sensitive(!k.is_active());
            }
            keep_update();
        });
        let me = Rc::downgrade(self);
        self.cancel.connect_clicked(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.close();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let me = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(bar) = me.upgrade() {
                    bar.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.revealer.add_controller(keys);
    }

    /// The rule the fields describe.
    fn rule(&self) -> Rule {
        let mode = self
            .modes
            .iter()
            .position(gtk::ToggleButton::is_active)
            .unwrap_or(0);
        match mode {
            1 => Rule::Add {
                before: self.before.text().to_string(),
                after: self.after.text().to_string(),
            },
            2 => Rule::Number {
                name: self.name.text().to_string(),
                start: u32::try_from(self.start.value_as_int()).unwrap_or(1),
                digits: usize::try_from(self.digits.value_as_int()).unwrap_or(2),
                keep: self.keep.is_active(),
            },
            3 => Rule::Case(
                match self.cases.iter().position(gtk::CheckButton::is_active) {
                    Some(1) => Case::Upper,
                    Some(2) => Case::Title,
                    _ => Case::Lower,
                },
            ),
            _ => Rule::Replace {
                find: self.find.text().to_string(),
                with: self.with.text().to_string(),
                match_case: self.match_case.is_active(),
            },
        }
    }

    /// The new names and their problems, for the files being renamed.
    fn outcome(&self) -> (Vec<String>, Vec<String>, Vec<Option<String>>) {
        let files = self.files.borrow();
        let items: Vec<(String, bool)> = files.iter().map(|(_, n, d)| (n.clone(), *d)).collect();
        let old: Vec<String> = items.iter().map(|(n, _)| n.clone()).collect();
        let new = plan(&items, &self.rule());
        let issues = problems(&old, &new, &self.taken.borrow());
        (old, new, issues)
    }

    fn update(&self) {
        while let Some(row) = self.preview.first_child() {
            self.preview.remove(&row);
        }
        let (old, new, issues) = self.outcome();
        for ((before, after), issue) in old.iter().zip(&new).zip(&issues).take(PREVIEW_ROWS) {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let from = gtk::Label::new(Some(before));
            from.set_xalign(0.0);
            from.set_hexpand(true);
            from.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            from.set_width_chars(10);
            let arrow = gtk::Label::new(Some("→"));
            arrow.add_css_class("dim-label");
            let to = gtk::Label::new(Some(after));
            to.set_xalign(0.0);
            to.set_hexpand(true);
            to.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            to.set_width_chars(10);
            if let Some(issue) = issue {
                to.add_css_class("rename-problem");
                line.set_tooltip_text(Some(issue));
            } else if before == after {
                to.add_css_class("dim-label");
            } else {
                to.add_css_class("rename-changed");
            }
            line.append(&from);
            line.append(&arrow);
            line.append(&to);
            self.preview.append(&line);
        }
        if old.len() > PREVIEW_ROWS {
            let more = gtk::Label::new(Some(&format!("…and {} more", old.len() - PREVIEW_ROWS)));
            more.add_css_class("dim-label");
            more.set_xalign(0.0);
            self.preview.append(&more);
        }
        let changing = old.iter().zip(&new).filter(|(a, b)| a != b).count();
        let bad = issues.iter().filter(|i| i.is_some()).count();
        let mut text = match changing {
            0 => "No names change yet".to_owned(),
            1 => "1 name changes".to_owned(),
            n => format!("{n} names change"),
        };
        if bad > 0 {
            let first = issues.iter().flatten().next().cloned().unwrap_or_default();
            text = format!("{text} · {bad} to fix: {first}");
            self.summary.add_css_class("rename-problem");
        } else {
            self.summary.remove_css_class("rename-problem");
        }
        self.summary.set_text(&text);
        self.rename.set_sensitive(changing > 0 && bad == 0);
    }

    /// Opens the bar for `files` (name, whether a folder); `taken` are the
    /// other names in their folder.
    pub(super) fn open(&self, files: Vec<(gio::File, String, bool)>, taken: HashSet<String>) {
        self.title
            .set_text(&format!("Rename {} items", files.len()));
        self.files.replace(files);
        self.taken.replace(taken);
        self.update();
        self.revealer.set_reveal_child(true);
        let first = match self.pages.visible_child_name().as_deref() {
            Some("Add text") => self.before.clone(),
            Some("Number") => self.name.clone(),
            _ => self.find.clone(),
        };
        first.grab_focus();
    }

    pub(super) fn close(&self) {
        self.revealer.set_reveal_child(false);
        self.files.replace(Vec::new());
    }

    /// The renames to make (only names that change), when there are no
    /// problems.
    fn renames(&self) -> Option<Vec<(gio::File, String)>> {
        let (old, new, issues) = self.outcome();
        if issues.iter().any(Option::is_some) {
            return None;
        }
        let files = self.files.borrow();
        Some(
            files
                .iter()
                .zip(old.iter().zip(new))
                .filter(|(_, (a, b))| *a != b)
                .map(|((file, _, _), (_, b))| (file.clone(), b))
                .collect(),
        )
    }
}

impl Win {
    /// Batch rename for the selection (several items).
    pub(super) fn start_batch_rename(self: &Rc<Self>) {
        let Some(tab) = self.tab() else { return };
        let selected = tab.selected();
        if selected.len() < 2 {
            return;
        }
        let files: Vec<(gio::File, String, bool)> = selected
            .iter()
            .map(|(info, file)| (file.clone(), info.display_name().to_string(), is_dir(info)))
            .collect();
        // Names already in the folder, to catch a rename onto one of them.
        let chosen: HashSet<String> = files.iter().map(|(_, n, _)| n.clone()).collect();
        let model = tab.list.borrow().model().clone();
        let taken: HashSet<String> = (0..model.n_items())
            .filter_map(|i| model.item(i).and_downcast::<gio::FileInfo>())
            .map(|i| i.display_name().to_string())
            .filter(|n| !chosen.contains(n))
            .collect();
        self.close_prompt();
        self.batch.open(files, taken);
    }

    /// Wires the bar's Rename button and Enter in its fields.
    pub(super) fn setup_batch_rename(win: &Rc<Self>) {
        let go = {
            let w = Rc::downgrade(win);
            move || {
                let Some(win) = w.upgrade() else { return };
                let Some(renames) = win.batch.renames() else {
                    return;
                };
                if renames.is_empty() {
                    return;
                }
                win.batch.close();
                win.focus_view();
                let n = renames.len();
                let olds: Vec<String> = renames.iter().map(|(f, _)| ops::display_name(f)).collect();
                let w = Rc::downgrade(&win);
                win.run_job(async move {
                    let done = ops::rename_many(renames).await?;
                    if let Some(win) = w.upgrade() {
                        win.show_message(&format!(
                            "Renamed {n} item{}",
                            if n == 1 { "" } else { "s" }
                        ));
                        let back = done.into_iter().zip(olds).collect();
                        win.record("Rename", undo::Change::Renamed(back));
                    }
                    Ok(())
                });
                win.job_label.set_text(&format!("Renaming {n} items…"));
            }
        };
        let run = go.clone();
        win.batch.rename.connect_clicked(move |_| run());
        for e in [
            &win.batch.find,
            &win.batch.with,
            &win.batch.before,
            &win.batch.after,
            &win.batch.name,
        ] {
            let run = go.clone();
            e.connect_activate(move |_| run());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<(String, bool)> {
        list.iter().map(|n| ((*n).to_owned(), false)).collect()
    }

    #[test]
    fn rules() {
        let items = names(&["IMG_001.JPG", "IMG_002.JPG", "notes.txt"]);
        let replace = Rule::Replace {
            find: "img_".into(),
            with: "Holiday ".into(),
            match_case: false,
        };
        assert_eq!(
            plan(&items, &replace),
            ["Holiday 001.JPG", "Holiday 002.JPG", "notes.txt"]
        );
        let exact = Rule::Replace {
            find: "img_".into(),
            with: "x".into(),
            match_case: true,
        };
        assert_eq!(plan(&items, &exact)[0], "IMG_001.JPG");
        let add = Rule::Add {
            before: "2024 ".into(),
            after: " (edit)".into(),
        };
        assert_eq!(plan(&items, &add)[2], "2024 notes (edit).txt");
        let number = Rule::Number {
            name: "Photo".into(),
            start: 9,
            digits: 2,
            keep: false,
        };
        assert_eq!(
            plan(&items, &number),
            ["Photo 09.JPG", "Photo 10.JPG", "Photo 11.txt"]
        );
        let keep = Rule::Number {
            name: String::new(),
            start: 1,
            digits: 1,
            keep: true,
        };
        assert_eq!(plan(&items, &keep)[1], "IMG_002 2.JPG");
        assert_eq!(plan(&items, &Rule::Case(Case::Lower))[0], "img_001.JPG");
        assert_eq!(
            plan(
                &names(&["my holiday's photo.png"]),
                &Rule::Case(Case::Title)
            )[0],
            "My Holiday's Photo.png"
        );
        // Folders have no extension: the whole name changes.
        let dirs = vec![("a.b".to_owned(), true)];
        assert_eq!(plan(&dirs, &Rule::Case(Case::Upper))[0], "A.B");
    }

    #[test]
    fn problems_found() {
        let old: Vec<String> = ["a.txt", "b.txt", "c.txt"].map(str::to_owned).to_vec();
        let taken: HashSet<String> = ["z.txt".to_owned()].into();
        // Two files to one name, a name taken by another file, a bad name.
        let new: Vec<String> = ["x.txt", "x.txt", "z.txt"].map(str::to_owned).to_vec();
        let found = problems(&old, &new, &taken);
        assert!(found[0].is_some() && found[1].is_some() && found[2].is_some());
        // Swapping names within the batch is fine.
        let swap: Vec<String> = ["b.txt", "a.txt", "c.txt"].map(str::to_owned).to_vec();
        assert!(problems(&old, &swap, &taken).iter().all(Option::is_none));
        let bad: Vec<String> = ["", "b/c.txt", "c.txt"].map(str::to_owned).to_vec();
        let found = problems(&old, &bad, &taken);
        assert!(found[0].is_some() && found[1].is_some() && found[2].is_none());
    }
}
