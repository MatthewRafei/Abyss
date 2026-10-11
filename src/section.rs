//! The grouped grid's model: the file list laid out as rows of a grid with
//! a heading row per file type.
//!
//! GTK's grid has no section headers, and one grid per group can't scroll
//! as a whole: stacked in one scrolled area, each grid thinks all its rows
//! are on screen and builds a tile for every file (thousands, in a photo
//! folder). So one grid shows everything, and `GroupedSelection` lays the
//! shared `MultiSelection` out for it: each group starts on a row of its
//! own whose first cell is the heading, then its files, with empty cells
//! filling out the last row. Grid rows are as tall as their tallest cell,
//! so heading rows stay short.
//!
//! Headings and fillers are `StringObject`s ("name\tcount", or empty);
//! files pass straight through, and so do selection state and changes, so
//! selecting in the grid is selecting in the window. The layout is redone
//! as soon as the shared model changes: a grid must never see a layout
//! that points at files which have moved.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// One cell of the grouped grid.
#[derive(Clone)]
pub enum Slot {
    /// Position in the shared model.
    File(u32),
    /// A group heading.
    Heading(gtk::StringObject),
    /// Padding to the end of a row.
    Gap(gtk::StringObject),
}

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::gio;
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    use super::Slot;

    /// Names an item's group.
    pub type Group = Box<dyn Fn(&glib::Object) -> &'static str>;

    #[derive(Default)]
    pub struct GroupedSelection {
        pub inner: RefCell<Option<gtk::MultiSelection>>,
        pub group: RefCell<Option<Group>>,
        pub slots: RefCell<Vec<Slot>>,
        /// Grid position of each position in the shared model.
        pub cell_of: RefCell<Vec<u32>>,
        /// Columns laid out for; 0 while the grouped grid isn't shown.
        pub columns: Cell<u32>,
        pub handlers: RefCell<Vec<glib::SignalHandlerId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GroupedSelection {
        const NAME: &'static str = "AbyssGroupedSelection";
        type Type = super::GroupedSelection;
        type Interfaces = (gio::ListModel, gtk::SelectionModel);
    }

    impl ObjectImpl for GroupedSelection {}

    impl GroupedSelection {
        pub fn inner(&self) -> Option<gtk::MultiSelection> {
            self.inner.borrow().clone()
        }

        /// The shared positions of the files among grid positions
        /// `position..position + n`: one contiguous run, since the grid
        /// keeps the shared order.
        fn files_in(&self, position: u32, n: u32) -> Option<(u32, u32)> {
            let slots = self.slots.borrow();
            let end = usize::try_from(position.saturating_add(n))
                .unwrap_or(usize::MAX)
                .min(slots.len());
            let start = usize::try_from(position).ok()?.min(end);
            let mut files = slots[start..end].iter().filter_map(|s| match s {
                Slot::File(p) => Some(*p),
                _ => None,
            });
            let first = files.next()?;
            let last = files.next_back().unwrap_or(first);
            Some((first, last - first + 1))
        }

        pub fn file_at(&self, position: u32) -> Option<u32> {
            match self.slots.borrow().get(usize::try_from(position).ok()?)? {
                Slot::File(p) => Some(*p),
                _ => None,
            }
        }
    }

    impl ListModelImpl for GroupedSelection {
        fn item_type(&self) -> glib::Type {
            glib::Object::static_type()
        }

        fn n_items(&self) -> u32 {
            u32::try_from(self.slots.borrow().len()).unwrap_or(u32::MAX)
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            let slot = self
                .slots
                .borrow()
                .get(usize::try_from(position).ok()?)?
                .clone();
            match slot {
                Slot::File(p) => self.inner()?.item(p),
                Slot::Heading(o) | Slot::Gap(o) => Some(o.upcast()),
            }
        }
    }

    impl SelectionModelImpl for GroupedSelection {
        fn selection_in_range(&self, position: u32, n_items: u32) -> gtk::Bitset {
            let out = gtk::Bitset::new_empty();
            let (Some(inner), Some((first, n))) = (self.inner(), self.files_in(position, n_items))
            else {
                return out;
            };
            let set = inner.selection_in_range(first, n);
            if !set.is_empty() {
                let cell_of = self.cell_of.borrow();
                for p in first..first + n {
                    if set.contains(p) {
                        if let Some(&cell) = cell_of.get(p as usize) {
                            out.add(cell);
                        }
                    }
                }
            }
            out
        }

        fn is_selected(&self, position: u32) -> bool {
            matches!((self.inner(), self.file_at(position)), (Some(inner), Some(p)) if inner.is_selected(p))
        }

        fn select_item(&self, position: u32, unselect_rest: bool) -> bool {
            matches!((self.inner(), self.file_at(position)), (Some(inner), Some(p)) if inner.select_item(p, unselect_rest))
        }

        fn unselect_item(&self, position: u32) -> bool {
            matches!((self.inner(), self.file_at(position)), (Some(inner), Some(p)) if inner.unselect_item(p))
        }

        fn select_range(&self, position: u32, n_items: u32, unselect_rest: bool) -> bool {
            let Some(inner) = self.inner() else {
                return false;
            };
            match self.files_in(position, n_items) {
                Some((first, n)) => inner.select_range(first, n, unselect_rest),
                None if unselect_rest => inner.unselect_all(),
                None => false,
            }
        }

        fn unselect_range(&self, position: u32, n_items: u32) -> bool {
            match (self.inner(), self.files_in(position, n_items)) {
                (Some(inner), Some((first, n))) => inner.unselect_range(first, n),
                _ => false,
            }
        }

        fn select_all(&self) -> bool {
            self.inner().is_some_and(|inner| inner.select_all())
        }

        fn unselect_all(&self) -> bool {
            self.inner().is_some_and(|inner| inner.unselect_all())
        }
    }
}

glib::wrapper! {
    pub struct GroupedSelection(ObjectSubclass<imp::GroupedSelection>)
        @implements gio::ListModel, gtk::SelectionModel;
}

impl GroupedSelection {
    /// A grid layout of `inner`, whose items must be in group order;
    /// `group` names an item's group. Empty until `set_columns`.
    pub fn new(
        inner: &gtk::MultiSelection,
        group: impl Fn(&glib::Object) -> &'static str + 'static,
    ) -> Self {
        let this: Self = glib::Object::new();
        let imp = this.imp();
        imp.inner.replace(Some(inner.clone()));
        imp.group.replace(Some(Box::new(group)));
        let weak = this.downgrade();
        let follow = inner.connect_items_changed(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.relayout();
            }
        });
        // Forward selection changes, as grid positions.
        let weak = this.downgrade();
        let handler = inner.connect_selection_changed(move |_, position, n_items| {
            let Some(this) = weak.upgrade() else { return };
            let (lo, hi) = {
                let cell_of = this.imp().cell_of.borrow();
                let at = |p: u32| cell_of.get(p as usize).copied();
                let last = position.saturating_add(n_items.max(1)) - 1;
                let last = last.min(u32::try_from(cell_of.len()).unwrap_or(0).saturating_sub(1));
                match (at(position), at(last)) {
                    (Some(lo), Some(hi)) => (lo, hi),
                    _ => return,
                }
            };
            this.selection_changed(lo, hi - lo + 1);
        });
        imp.handlers.replace(vec![follow, handler]);
        this
    }

    /// Lays out for `columns` columns, or empties the grid (and stops
    /// following changes) with None.
    pub fn set_columns(&self, columns: Option<u32>) {
        self.imp().columns.set(columns.map_or(0, |c| c.max(1)));
        self.relayout();
    }

    /// Recomputes the layout from the shared model's current items,
    /// grouping consecutive items in the same group.
    fn relayout(&self) {
        let imp = self.imp();
        let Some(inner) = imp.inner() else { return };
        let columns = imp.columns.get();
        let guard = imp.group.borrow();
        let (Some(group), true) = (guard.as_ref(), columns > 0) else {
            drop(guard);
            self.replace(Vec::new(), Vec::new());
            return;
        };
        let n = inner.n_items();
        let mut runs: Vec<(u32, u32, &'static str)> = Vec::new();
        for i in 0..n {
            let Some(item) = inner.item(i) else { continue };
            let name = group(&item);
            match runs.last_mut() {
                Some((_, len, last)) if *last == name => *len += 1,
                _ => runs.push((i, 1, name)),
            }
        }
        let gap = || Slot::Gap(gtk::StringObject::new(""));
        let mut slots = Vec::with_capacity(n as usize + runs.len() * columns as usize * 2);
        let mut cell_of = vec![0; n as usize];
        for (start, len, name) in runs {
            slots.push(Slot::Heading(gtk::StringObject::new(&format!(
                "{name}\t{len}"
            ))));
            slots.extend(std::iter::repeat_with(gap).take(columns as usize - 1));
            for p in start..start + len {
                cell_of[p as usize] = u32::try_from(slots.len()).unwrap_or(u32::MAX);
                slots.push(Slot::File(p));
            }
            let pad = (columns - len % columns) % columns;
            slots.extend(std::iter::repeat_with(gap).take(pad as usize));
        }
        drop(guard);
        self.replace(slots, cell_of);
    }

    fn replace(&self, slots: Vec<Slot>, cell_of: Vec<u32>) {
        let imp = self.imp();
        let new = slots.len();
        let old = imp.slots.replace(slots).len();
        imp.cell_of.replace(cell_of);
        if old == 0 && new == 0 {
            return;
        }
        self.items_changed(
            0,
            u32::try_from(old).unwrap_or(u32::MAX),
            u32::try_from(new).unwrap_or(u32::MAX),
        );
    }

    pub fn columns(&self) -> u32 {
        self.imp().columns.get()
    }

    /// The shared-model position shown at grid position `position`, if
    /// it's a file (not a heading or filler).
    pub fn file_at(&self, position: u32) -> Option<u32> {
        self.imp().file_at(position)
    }

    /// Whether grid position `position` is a group heading.
    pub fn is_heading(&self, position: u32) -> bool {
        usize::try_from(position)
            .is_ok_and(|p| matches!(self.imp().slots.borrow().get(p), Some(Slot::Heading(_))))
    }

    /// The grid position showing shared-model position `position`.
    pub fn cell_of(&self, position: u32) -> Option<u32> {
        self.imp().cell_of.borrow().get(position as usize).copied()
    }

    /// Stops listening to the shared model (call before dropping it).
    pub fn detach(&self) {
        let imp = self.imp();
        if let Some(inner) = imp.inner.take() {
            for handler in imp.handlers.take() {
                inner.disconnect(handler);
            }
        }
    }
}

/// A heading cell's group name and count, from its `StringObject`.
pub fn heading(item: &glib::Object) -> Option<(String, u32)> {
    let text = item.downcast_ref::<gtk::StringObject>()?.string();
    let (name, count) = text.split_once('\t')?;
    Some((name.to_owned(), count.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> gio::FileInfo {
        let i = gio::FileInfo::new();
        i.set_name(name);
        i
    }

    #[test]
    fn lays_out_groups_and_maps_selection() {
        if gtk::init().is_err() {
            return; // no display to initialise GTK on
        }
        // Groups by first letter: a a a | b | c c
        let store = gio::ListStore::new::<gio::FileInfo>();
        for n in ["a1", "a2", "a3", "b1", "c1", "c2"] {
            store.append(&info(n));
        }
        let main = gtk::MultiSelection::new(Some(store.clone()));
        let group = |o: &glib::Object| -> &'static str {
            let name = o
                .downcast_ref::<gio::FileInfo>()
                .map(|i| i.name().display().to_string())
                .unwrap_or_default();
            match name.chars().next() {
                Some('a') => "A",
                Some('b') => "B",
                _ => "C",
            }
        };
        let grid = GroupedSelection::new(&main, group);
        grid.set_columns(Some(2));
        // [H gap] [a1 a2] [a3 gap] [H gap] [b1 gap] [H gap] [c1 c2]
        assert_eq!(grid.n_items(), 14);
        let kind = |p: u32| match grid.item(p) {
            Some(o) if o.is::<gio::FileInfo>() => o
                .downcast::<gio::FileInfo>()
                .unwrap()
                .name()
                .display()
                .to_string(),
            Some(o) => heading(&o).map_or_else(|| "-".into(), |(n, c)| format!("{n}{c}")),
            None => "?".into(),
        };
        let all: Vec<String> = (0..14).map(kind).collect();
        assert_eq!(
            all,
            ["A3", "-", "a1", "a2", "a3", "-", "B1", "-", "b1", "-", "C2", "-", "c1", "c2"]
        );
        assert_eq!(grid.file_at(8), Some(3));
        assert_eq!(grid.cell_of(5), Some(13));
        assert_eq!(grid.file_at(0), None);

        // Selecting in the grid selects in the shared model, and back.
        grid.select_item(8, true);
        assert!(main.is_selected(3));
        main.select_item(0, false);
        assert!(grid.is_selected(2));
        assert!(!grid.select_item(1, false)); // a filler can't be selected
                                              // A range over headings and fillers selects just the files.
        grid.select_range(3, 6, true); // a2 .. b1
        let set = main.selection();
        assert!(!set.contains(0) && set.contains(1) && set.contains(2) && set.contains(3));
        assert_eq!(set.size(), 3);
        let shown = grid.selection();
        assert!(shown.contains(3) && shown.contains(4) && shown.contains(8));
        assert_eq!(shown.size(), 3);
        // The layout follows the shared model at once.
        store.append(&info("c3"));
        assert_eq!(grid.n_items(), 16);
        assert_eq!(kind(14), "c3");
        grid.set_columns(None);
        assert_eq!(grid.n_items(), 0);
        grid.detach();
    }
}
