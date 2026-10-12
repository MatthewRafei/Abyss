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
//! Headings and fillers are `StringObject`s ("name\tcount", or empty).
//! The layout holds the files themselves, not their positions in the shared
//! model, so it stays valid while that model changes: a burst of changes
//! (an archive unpacking into the folder, a git checkout) is laid out once
//! per `RELAYOUT_MS` instead of once per file. Selection state and changes
//! pass through to the shared model, so selecting in the grid is selecting
//! in the window; the files' current positions there are looked up again
//! after the model changed, when first needed.

use std::collections::HashMap;
use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// How long changes to the file list gather before the grid is laid out
/// again.
const RELAYOUT_MS: u64 = 60;

/// One cell of the grouped grid.
#[derive(Clone)]
pub enum Slot {
    /// A file (an item of the shared model).
    File(glib::Object),
    /// A group heading.
    Heading(gtk::StringObject),
    /// Padding to the end of a row.
    Gap(gtk::StringObject),
}

/// An object's identity, for maps keyed by object.
fn key(o: &glib::Object) -> usize {
    o.as_ptr() as usize
}

mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use gtk::gio;
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    use super::{key, Slot};

    /// Names an item's group.
    pub type Group = Box<dyn Fn(&glib::Object) -> &'static str>;

    #[derive(Default)]
    pub struct GroupedSelection {
        pub inner: RefCell<Option<gtk::MultiSelection>>,
        pub group: RefCell<Option<Group>>,
        pub slots: RefCell<Vec<Slot>>,
        /// Grid position of each file shown.
        pub cell_by_file: RefCell<HashMap<usize, u32>>,
        /// Each file's position in the shared model; None once that model
        /// changed, until next needed.
        pub positions: RefCell<Option<HashMap<usize, u32>>>,
        /// Columns laid out for; 0 while the grouped grid isn't shown.
        pub columns: Cell<u32>,
        /// A re-layout is waiting for the current burst of changes.
        pub pending: Cell<bool>,
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

        /// The shared-model position of `file`, if it's still there.
        pub fn position(&self, file: &glib::Object) -> Option<u32> {
            if self.positions.borrow().is_none() {
                let inner = self.inner()?;
                let map = (0..inner.n_items())
                    .filter_map(|p| Some((key(&inner.item(p)?), p)))
                    .collect();
                self.positions.replace(Some(map));
            }
            self.positions.borrow().as_ref()?.get(&key(file)).copied()
        }

        pub fn file_at(&self, cell: u32) -> Option<u32> {
            let slot = self
                .slots
                .borrow()
                .get(usize::try_from(cell).ok()?)
                .cloned()?;
            match slot {
                Slot::File(file) => self.position(&file),
                _ => None,
            }
        }

        /// Bitsets over the shared model for grid cells: the files among
        /// `cells` (all of them in `mask`, those in `selected` too).
        fn files_of(
            &self,
            selected: &gtk::Bitset,
            mask: &gtk::Bitset,
        ) -> (gtk::Bitset, gtk::Bitset) {
            let (files_selected, files_mask) = (gtk::Bitset::new_empty(), gtk::Bitset::new_empty());
            if let Some((rest, first)) = gtk::BitsetIter::init_first(mask) {
                for cell in std::iter::once(first).chain(rest) {
                    if let Some(p) = self.file_at(cell) {
                        files_mask.add(p);
                        if selected.contains(cell) {
                            files_selected.add(p);
                        }
                    }
                }
            }
            (files_selected, files_mask)
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
            let slots = self.slots.borrow();
            Some(match slots.get(usize::try_from(position).ok()?)? {
                Slot::File(o) => o.clone(),
                Slot::Heading(o) | Slot::Gap(o) => o.clone().upcast(),
            })
        }
    }

    impl SelectionModelImpl for GroupedSelection {
        fn selection_in_range(&self, position: u32, n_items: u32) -> gtk::Bitset {
            let out = gtk::Bitset::new_empty();
            let Some(inner) = self.inner() else {
                return out;
            };
            for cell in position..position.saturating_add(n_items) {
                if self.file_at(cell).is_some_and(|p| inner.is_selected(p)) {
                    out.add(cell);
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
            let cells = gtk::Bitset::new_range(position, n_items);
            let (selected, mut mask) = self.files_of(&cells, &cells);
            if unselect_rest {
                mask = gtk::Bitset::new_range(0, inner.n_items());
            }
            inner.set_selection(&selected, &mask)
        }

        fn unselect_range(&self, position: u32, n_items: u32) -> bool {
            let Some(inner) = self.inner() else {
                return false;
            };
            let cells = gtk::Bitset::new_range(position, n_items);
            let (_, mask) = self.files_of(&cells, &cells);
            inner.set_selection(&gtk::Bitset::new_empty(), &mask)
        }

        fn select_all(&self) -> bool {
            self.inner().is_some_and(|inner| inner.select_all())
        }

        fn unselect_all(&self) -> bool {
            self.inner().is_some_and(|inner| inner.unselect_all())
        }

        /// The grid's rubber band (and GTK's other bulk changes): the cells
        /// in `mask` take their state from `selected`. Headings and fillers
        /// in either are skipped; the files are changed in the shared model.
        fn set_selection(&self, selected: &gtk::Bitset, mask: &gtk::Bitset) -> bool {
            let Some(inner) = self.inner() else {
                return false;
            };
            let (files_selected, files_mask) = self.files_of(selected, mask);
            inner.set_selection(&files_selected, &files_mask)
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
                this.imp().positions.replace(None);
                this.schedule_relayout();
            }
        });
        // Forward selection changes, as grid cells.
        let weak = this.downgrade();
        let handler = inner.connect_selection_changed(move |inner, position, n_items| {
            let Some(this) = weak.upgrade() else { return };
            let range = {
                let cells = this.imp().cell_by_file.borrow();
                let mut range: Option<(u32, u32)> = None;
                for p in position..position.saturating_add(n_items) {
                    let Some(cell) = inner.item(p).and_then(|o| cells.get(&key(&o)).copied())
                    else {
                        continue;
                    };
                    range =
                        Some(range.map_or((cell, cell), |(lo, hi)| (lo.min(cell), hi.max(cell))));
                }
                range
            };
            if let Some((lo, hi)) = range {
                this.selection_changed(lo, hi - lo + 1);
            }
        });
        imp.handlers.replace(vec![follow, handler]);
        this
    }

    /// Lays out for `columns` columns, or empties the grid with None.
    pub fn set_columns(&self, columns: Option<u32>) {
        self.imp().columns.set(columns.map_or(0, |c| c.max(1)));
        self.relayout();
    }

    /// Lays out again once the current burst of changes has gathered: the
    /// first change starts the wait, later ones join it.
    fn schedule_relayout(&self) {
        let imp = self.imp();
        if imp.columns.get() == 0 || imp.pending.replace(true) {
            return;
        }
        let weak = self.downgrade();
        // Below drawing in priority: a frame due meanwhile is painted first.
        glib::timeout_add_local_full(
            Duration::from_millis(RELAYOUT_MS),
            glib::Priority::DEFAULT_IDLE,
            move || {
                if let Some(this) = weak.upgrade() {
                    this.flush();
                }
                glib::ControlFlow::Break
            },
        );
    }

    /// Lays out now if changes are waiting.
    pub fn flush(&self) {
        if self.imp().pending.replace(false) {
            self.relayout();
        }
    }

    /// Recomputes the layout from the shared model's current items,
    /// grouping consecutive items in the same group.
    fn relayout(&self) {
        let imp = self.imp();
        imp.pending.set(false);
        let Some(inner) = imp.inner() else { return };
        let columns = imp.columns.get();
        let guard = imp.group.borrow();
        let (Some(group), true) = (guard.as_ref(), columns > 0) else {
            drop(guard);
            self.replace(Vec::new(), HashMap::new());
            return;
        };
        let items: Vec<glib::Object> = (0..inner.n_items()).filter_map(|p| inner.item(p)).collect();
        let mut runs: Vec<(usize, usize, &'static str)> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let name = group(item);
            match runs.last_mut() {
                Some((_, len, last)) if *last == name => *len += 1,
                _ => runs.push((i, 1, name)),
            }
        }
        drop(guard);
        let columns = columns as usize;
        let gap = || Slot::Gap(gtk::StringObject::new(""));
        let mut slots = Vec::with_capacity(items.len() + runs.len() * columns * 2);
        let mut cells = HashMap::with_capacity(items.len());
        let mut positions = HashMap::with_capacity(items.len());
        for (start, len, name) in runs {
            slots.push(Slot::Heading(gtk::StringObject::new(&format!(
                "{name}\t{len}"
            ))));
            slots.extend(std::iter::repeat_with(gap).take(columns - 1));
            for (offset, item) in items[start..start + len].iter().enumerate() {
                let k = key(item);
                cells.insert(k, u32::try_from(slots.len()).unwrap_or(u32::MAX));
                positions.insert(k, u32::try_from(start + offset).unwrap_or(u32::MAX));
                slots.push(Slot::File(item.clone()));
            }
            let pad = (columns - len % columns) % columns;
            slots.extend(std::iter::repeat_with(gap).take(pad));
        }
        imp.positions.replace(Some(positions));
        self.replace(slots, cells);
    }

    fn replace(&self, slots: Vec<Slot>, cells: HashMap<usize, u32>) {
        let imp = self.imp();
        let new = slots.len();
        let old = imp.slots.replace(slots).len();
        imp.cell_by_file.replace(cells);
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

    /// The shared-model position of the file shown at grid position
    /// `position`, if it's a file (not a heading or filler).
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
        let imp = self.imp();
        let item = imp.inner()?.item(position)?;
        imp.cell_by_file.borrow().get(&key(&item)).copied()
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
        // A rubber band over cells 2..=8 (a1, a2, a3, filler, heading,
        // filler, b1) with nothing else kept: those files only.
        let band = gtk::Bitset::new_range(2, 7);
        let all = gtk::Bitset::new_range(0, grid.n_items());
        assert!(grid.set_selection(&band, &all));
        let set = main.selection();
        assert_eq!(set.size(), 4);
        assert!(set.contains(0) && set.contains(1) && set.contains(2) && set.contains(3));
        // The layout follows the shared model after a burst of changes.
        store.append(&info("c3"));
        grid.flush();
        assert_eq!(grid.n_items(), 16);
        assert_eq!(kind(14), "c3");
        grid.set_columns(None);
        assert_eq!(grid.n_items(), 0);
        grid.detach();
    }
}
