//! List and grid views, optionally grouped by file type.
//!
//! - List: the `ColumnView`; grouped, it uses GTK's section headers
//!   (`SortListModel` section sorter + header factory).
//! - Grid: a `GridView` on the same selection model.
//! - Grouped grid: GTK's grid has no section headers, so one `GridView`
//!   shows a `GroupedSelection` layout of the shared model, with a heading
//!   row per group (see `section.rs`); selection and file operations stay
//!   shared.
//!
//! The choice is remembered in `~/.config/abyss/view`.

use super::*;
use crate::section;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    List,
    Grid,
}

fn view_file() -> PathBuf {
    glib::user_config_dir().join("abyss").join("view")
}

/// Saved view: "list" / "grid", optionally followed by "grouped".
pub fn load_view() -> (Mode, bool) {
    let text = std::fs::read_to_string(view_file()).unwrap_or_default();
    let mode = if text.split_whitespace().any(|w| w == "grid") {
        Mode::Grid
    } else {
        Mode::List
    };
    (mode, text.split_whitespace().any(|w| w == "grouped"))
}

fn save_view(mode: Mode, grouped: bool) {
    let path = view_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mode = if mode == Mode::Grid { "grid" } else { "list" };
    let _ = std::fs::write(
        path,
        if grouped {
            format!("{mode} grouped\n")
        } else {
            format!("{mode}\n")
        },
    );
}

/// A grid tile's fixed geometry at one zoom level. Every tile at a level is
/// the same width, so names wrap the same way everywhere and columns line
/// up; each level is chosen so ordinary names fit.
pub struct TileSize {
    /// Thumbnail slot: as tall as the grid icons (`style.css`), a third
    /// wider.
    pub slot: (i32, i32),
    /// Width of the name in characters (up to two lines).
    pub name_chars: i32,
}

/// The tile for a zoom level (`ZOOM_LEVELS`).
const fn tile_size(zoom: u32) -> TileSize {
    let (icon, name_chars) = match zoom {
        0..=20 => (48, 12),
        21..=32 => (64, 12),
        33..=48 => (96, 14),
        49..=64 => (128, 16),
        65..=96 => (160, 18),
        _ => (192, 20),
    };
    TileSize {
        slot: (icon * 4 / 3, icon),
        name_chars,
    }
}

/// Group headings, in sort order.
const GROUPS: [&str; 8] = [
    "Folders",
    "Images",
    "Video",
    "Audio",
    "Documents",
    "Code & Text",
    "Archives",
    "Other",
];

/// File-type group: sort order and heading. Worked out once per file (it
/// walks the MIME tree) and kept on the `FileInfo`.
pub fn category(info: &gio::FileInfo) -> (u8, &'static str) {
    const KEY: &str = "abyss::group";
    let order = if info.has_attribute(KEY) {
        u8::try_from(info.attribute_uint32(KEY)).unwrap_or(7)
    } else {
        let order = find_category(info);
        info.set_attribute_uint32(KEY, u32::from(order));
        order
    };
    (order, GROUPS[usize::from(order).min(GROUPS.len() - 1)])
}

fn find_category(info: &gio::FileInfo) -> u8 {
    if is_dir(info) {
        return 0;
    }
    let ct = crate::info::content_type(info)
        .map(|c| c.to_string())
        .unwrap_or_default();
    let is_a = |t: &str| ct == t || gio::content_type_is_a(&ct, t);
    if ct.starts_with("image/") {
        1
    } else if ct.starts_with("video/") {
        2
    } else if ct.starts_with("audio/") {
        3
    } else if is_a("application/pdf")
        || is_a("application/epub+zip")
        || is_a("application/postscript")
        || is_a("application/rtf")
        || ct.contains("opendocument")
        || ct.contains("officedocument")
        || ct.contains("msword")
        || ct.contains("ms-excel")
        || ct.contains("ms-powerpoint")
    {
        4
    } else if archive::is_archive_type(&ct) {
        6
    } else if is_a("text/plain")
        || is_a("application/json")
        || is_a("application/xml")
        || is_a("application/javascript")
    {
        5
    } else {
        7
    }
}

impl Win {
    pub(super) fn setup_views(win: &Rc<Self>) {
        win.grid.set_factory(Some(&Self::tile_factory(win, false)));
        let w = Rc::downgrade(win);
        win.grid.connect_activate(move |_, pos| {
            if let Some(win) = w.upgrade() {
                win.activate(pos);
            }
        });
        win.groups.set_factory(Some(&Self::tile_factory(win, true)));
        // A new layout may land on a different number of columns.
        let w = Rc::downgrade(win);
        win.grouped_model.connect_items_changed(move |_, _, _, _| {
            if let Some(win) = w.upgrade() {
                win.sync_group_columns();
            }
        });
        let w = Rc::downgrade(win);
        win.groups.connect_activate(move |_, pos| {
            if let Some(win) = w.upgrade() {
                if let Some(pos) = win.grouped_model.file_at(pos) {
                    win.activate(pos);
                }
            }
        });

        let w = Rc::downgrade(win);
        win.list_btn.connect_toggled(move |b| {
            if let (Some(win), true) = (w.upgrade(), b.is_active()) {
                win.set_view(Mode::List, win.grouped.get());
            }
        });
        let w = Rc::downgrade(win);
        win.grid_btn.connect_toggled(move |b| {
            if let (Some(win), true) = (w.upgrade(), b.is_active()) {
                win.set_view(Mode::Grid, win.grouped.get());
            }
        });
        let w = Rc::downgrade(win);
        win.group_btn.connect_toggled(move |b| {
            if let Some(win) = w.upgrade() {
                win.set_view(win.mode.get(), b.is_active());
            }
        });

        win.apply_view();

        // Re-check the toolbar layout whenever the file area changes size
        // (window resize or sidebar drag). Moving widgets during allocation
        // isn't allowed, so do it right after.
        let w = Rc::downgrade(win);
        win.size_probe.connect_resize(move |_, width, _| {
            let w = w.clone();
            glib::idle_add_local_once(move || {
                if let Some(win) = w.upgrade() {
                    win.update_toolbar_layout(width);
                    // The grouped grid's rows depend on how many columns fit.
                    if win.mode.get() == Mode::Grid && win.grouped.get() {
                        win.sync_group_columns();
                    }
                }
            });
        });
    }

    /// Narrow file area: the path keeps the top row and the view controls
    /// and filter move to a second row under it.
    fn update_toolbar_layout(&self, width: i32) {
        /// Below this width (logical px) the path would be squeezed.
        const COMPACT_BELOW: i32 = 820;
        if width <= 0 {
            return;
        }
        let compact = width < COMPACT_BELOW;
        if compact == self.compact.replace(compact) {
            return;
        }
        let (from, to) = if compact {
            (&self.toolbar_row1, &self.toolbar_row2)
        } else {
            (&self.toolbar_row2, &self.toolbar_row1)
        };
        from.remove(&self.controls);
        to.append(&self.controls);
        self.toolbar_row2.set_visible(compact);
        // On its own row the filter takes the spare width.
        self.controls.set_hexpand(compact);
        self.search.set_hexpand(compact);
    }

    /// Remakes the grid tiles (after a zoom change, which resizes their
    /// thumbnail slots).
    pub(super) fn refresh_tiles(self: &Rc<Self>) {
        self.grid
            .set_factory(Some(&Self::tile_factory(self, false)));
        self.groups
            .set_factory(Some(&Self::tile_factory(self, true)));
        if self.mode.get() == Mode::Grid && self.grouped.get() {
            self.rebuild_groups();
        }
    }

    /// A tile (icon or thumbnail over the name) for grid views. In the
    /// `grouped` grid a cell can also be a group heading or an empty filler,
    /// and positions map to the shared selection through `grouped_model`.
    // One cell type for both grids; setup and bind read best side by side.
    #[allow(clippy::too_many_lines)]
    fn tile_factory(win: &Rc<Self>, grouped: bool) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let w = Rc::downgrade(win);
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            // Centred at its own width rather than stretched over the cell:
            // GridView sizes rows by measuring tiles at their narrowest, so a
            // stretched name fits on one line but has two lines' room kept.
            let tile = gtk::Box::new(gtk::Orientation::Vertical, 6);
            tile.add_css_class("tile");
            tile.set_halign(gtk::Align::Center);
            // Top of the cell: icons line up across a row, and every name
            // starts right under its icon (one-line names don't sink).
            tile.set_valign(gtk::Align::Start);
            // Top of the cell: icons line up across a row, and every name
            // starts right under its icon (one-line names don't sink).
            tile.set_valign(gtk::Align::Start);
            // A slot as tall as the icons and a third wider: photos and
            // videos fill it instead of a square, sitting on their name.
            let size = tile_size(w.upgrade().map_or(48, |win| ZOOM_LEVELS[win.zoom.get()]));
            let (slot_w, slot_h) = size.slot;
            let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
            slot.set_size_request(slot_w, slot_h);
            let image = gtk::Image::new();
            image.add_css_class("file-icon");
            image.set_vexpand(true);
            track_icon(&image);
            let picture = gtk::Picture::new();
            picture.set_can_shrink(false);
            picture.set_valign(gtk::Align::End);
            picture.set_halign(gtk::Align::Center);
            picture.set_vexpand(true);
            picture.set_visible(false);
            slot.append(&image);
            slot.append(&picture);
            let label = gtk::Label::new(None);
            label.set_wrap(true);
            label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            label.set_justify(gtk::Justification::Center);
            // At least ~12 characters wide even when the icon is small, so
            // ordinary names ("Development") fit on a line instead of
            // breaking mid-word to the icon's width.
            label.set_width_chars(12);
            // A fixed width per zoom level (see `tile_size`).
            label.set_width_chars(size.name_chars);
            label.set_max_width_chars(size.name_chars);
            label.set_lines(2);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            // Break long names without adding hyphens ("j-pg").
            let attrs = gtk::pango::AttrList::new();
            attrs.insert(gtk::pango::AttrInt::new_insert_hyphens(false));
            label.set_attributes(Some(&attrs));
            tile.append(&slot);
            tile.append(&label);
            Self::attach_item_menu(&w, item, &tile, grouped);
            Self::attach_item_dnd(&w, item, &tile, grouped);
            // Cell: the heading (grouped grid only) or the tile.
            let cell = gtk::Box::new(gtk::Orientation::Vertical, 0);
            cell.set_valign(gtk::Align::Start);
            cell.set_valign(gtk::Align::Start);
            let heading = group_heading();
            heading.set_visible(false);
            // A heading may shorten to a cell's width, so no heading is wider
            // than a tile: GTK sizes columns by the widest cell shown, and a
            // wide heading scrolling into view would change the column count.
            if let Some(name) = heading.first_child().and_downcast::<gtk::Label>() {
                name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            cell.append(&heading);
            cell.append(&tile);
            item.set_child(Some(&cell));
        });
        factory.connect_bind(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(cell) = item.child().and_downcast::<gtk::Box>() else {
                return;
            };
            let (Some(heading), Some(tile)) = (
                cell.first_child().and_downcast::<gtk::Box>(),
                cell.last_child().and_downcast::<gtk::Box>(),
            ) else {
                return;
            };
            let object = item.item();
            let info = object.clone().and_downcast::<gio::FileInfo>();
            // Headings and fillers: not selectable and no tile (the tile
            // carries the hover and selection look, see style.css). GTK's
            // own cell widget is left alone: restyling it while binding
            // corrupts the grid's bookkeeping.
            let filler = info.is_none();
            item.set_selectable(!filler);
            item.set_activatable(!filler);
            item.set_focusable(!filler);
            tile.set_visible(!filler);
            let named = object.as_ref().and_then(section::heading);
            heading.set_visible(named.is_some());
            if let Some((name, count)) = named {
                fill_heading(&heading, &name, count);
            }
            let Some(info) = info else {
                return;
            };
            let image = tile
                .first_child()
                .and_then(|slot| slot.first_child())
                .and_downcast::<gtk::Image>();
            let picture = image
                .as_ref()
                .and_then(WidgetExt::next_sibling)
                .and_downcast::<gtk::Picture>();
            let label = tile.last_child().and_downcast::<gtk::Label>();
            let (Some(image), Some(picture), Some(label)) = (image, picture, label) else {
                return;
            };
            image.set_from_gicon(&crate::info::icon(&info));
            show_thumbnail(&image, Some(&picture), &info);
            label.set_text(&info.display_name());
            // Search hits from subfolders say where they are.
            let place = info
                .has_attribute("abyss::where")
                .then(|| info.attribute_string("abyss::where"))
                .flatten();
            match place {
                Some(place) => {
                    tile.set_tooltip_text(Some(&format!("{}\n{place}", info.display_name())));
                }
                None => tile.set_tooltip_text(Some(&info.display_name())),
            }
            tile.set_opacity(if is_hidden(&info) { 0.6 } else { 1.0 });
        });
        factory
    }

    /// Section headers for the grouped list: the group's name and size.
    fn header_factory() -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, header| {
            let Some(header) = header.downcast_ref::<gtk::ListHeader>() else {
                return;
            };
            header.set_child(Some(&group_heading()));
        });
        factory.connect_bind(|_, header| {
            let Some(header) = header.downcast_ref::<gtk::ListHeader>() else {
                return;
            };
            let (Some(info), Some(row)) = (
                header.item().and_downcast::<gio::FileInfo>(),
                header.child().and_downcast::<gtk::Box>(),
            ) else {
                return;
            };
            fill_heading(&row, category(&info).1, header.n_items());
        });
        factory
    }

    pub(super) fn set_view(self: &Rc<Self>, mode: Mode, grouped: bool) {
        if self.mode.get() == mode && self.grouped.get() == grouped {
            return;
        }
        self.mode.set(mode);
        self.grouped.set(grouped);
        save_view(mode, grouped);
        self.apply_view();
    }

    fn apply_view(self: &Rc<Self>) {
        let (mode, grouped) = (self.mode.get(), self.grouped.get());
        // The buttons reflect the state (their handlers ignore no-op changes).
        self.list_btn.set_active(mode == Mode::List);
        self.grid_btn.set_active(mode == Mode::Grid);
        self.group_btn.set_active(grouped);

        // Group order first when grouped (the sorter reads `grouped`).
        self.group_sorter.changed(gtk::SorterChange::Different);
        let sections = grouped && mode == Mode::List;
        self.sorted
            .set_section_sorter(sections.then_some(&self.group_sorter));
        self.view
            .set_header_factory(sections.then(Self::header_factory).as_ref());

        let page = match (mode, grouped) {
            (Mode::List, _) => "list",
            (Mode::Grid, false) => "grid",
            (Mode::Grid, true) => "groups",
        };
        // Only the view on screen follows the file list: hidden ones would
        // keep making and filling tiles (and loading thumbnails) unseen.
        self.view
            .set_model((page == "list").then_some(&self.selection));
        self.grid
            .set_model((page == "grid").then_some(&self.selection));
        self.groups
            .set_model((page == "groups").then_some(&self.grouped_model));
        if page == "groups" {
            self.rebuild_groups();
        } else {
            self.clear_groups();
        }
        self.views.set_visible_child_name(page);
        self.reveal_top();
    }

    /// Keeps a view that's scrolled to the very top there as files arrive.
    /// GTK holds on to the first item shown, so files sorting in above it
    /// (a folder loads in batches) would push the real first rows out of
    /// sight. Runs as the list changes, before GTK lays the view out again,
    /// so the scroll position read here is still the one the user left.
    pub(super) fn keep_top(&self) {
        let (adj, n) = match (self.mode.get(), self.grouped.get()) {
            (Mode::List, _) => (self.view.vadjustment(), self.sorted.n_items()),
            (Mode::Grid, false) => (self.grid.vadjustment(), self.sorted.n_items()),
            (Mode::Grid, true) => (self.groups.vadjustment(), self.grouped_model.n_items()),
        };
        if n == 0 || adj.is_none_or(|a| a.value() > 0.5) {
            return;
        }
        let flags = gtk::ListScrollFlags::NONE;
        match (self.mode.get(), self.grouped.get()) {
            (Mode::List, _) => self.view.scroll_to(0, None, flags, None),
            (Mode::Grid, false) => self.grid.scroll_to(0, flags, None),
            (Mode::Grid, true) => self.groups.scroll_to(0, flags, None),
        }
    }

    /// GTK anchors the list to its first item, which leaves the first group
    /// heading just above the view; scroll to the very top so it shows.
    pub(super) fn reveal_top(&self) {
        if self.mode.get() != Mode::List || !self.grouped.get() {
            return;
        }
        if let Some(adj) = self.view.vadjustment() {
            glib::idle_add_local_once(move || adj.set_value(0.0));
        }
    }

    /// Empties the grouped grid while another view shows.
    fn clear_groups(&self) {
        self.grouped_model.set_columns(None);
    }

    /// A first estimate of the grouped grid's columns for a file area
    /// `width` wide: each cell is a thumbnail slot plus the tile's padding
    /// and margin (`style.css`), inside the grid's own padding.
    pub(super) fn group_columns(&self, width: i32) -> u32 {
        let size = tile_size(ZOOM_LEVELS[self.zoom.get()]);
        // About 7 px per character of name.
        let cell = size.slot.0.max(size.name_chars * 7) + 6 + 6 + 2 + 2;
        let usable = (width - 2 * 8).max(cell);
        u32::try_from(usable / cell).unwrap_or(1).clamp(1, 24)
    }

    /// Lays the grouped grid out (it then follows the folder's contents by
    /// itself), for an estimate of the columns until a tile can be
    /// measured (see `sync_group_columns`).
    fn rebuild_groups(self: &Rc<Self>) {
        if self.grouped_model.columns() == 0 {
            let width = [
                self.groups.width(),
                self.views.width(),
                self.size_probe.width(),
            ]
            .into_iter()
            .find(|w| *w > 0)
            .unwrap_or(800);
            self.set_group_columns(self.group_columns(width));
        }
        self.sync_group_columns();
    }

    /// Holds the grouped grid to `columns` columns and lays the groups out
    /// for exactly that many: GTK's own count depends on whichever tiles
    /// exist at the moment (during a zoom, old and new sizes mixed), so it
    /// isn't left to choose.
    fn set_group_columns(&self, columns: u32) {
        let columns = columns.clamp(1, 24);
        self.groups
            .set_max_columns(columns.max(self.groups.min_columns()));
        self.groups.set_min_columns(columns);
        self.groups.set_max_columns(columns);
        if columns != self.grouped_model.columns() {
            self.grouped_model.set_columns(Some(columns));
        }
    }

    /// Works out the grouped grid's columns from a real tile: every file
    /// tile at a zoom level is the same width, so as many as fit the grid's
    /// width. Checked on the next frames (a fresh grid has no tile to
    /// measure yet) until the count holds for two of them. Each request (a
    /// new layout, a resize, a zoom, showing the grid) replaces any check
    /// still running, so one stalled by the grid being hidden can't block
    /// later ones.
    pub(super) fn sync_group_columns(self: &Rc<Self>) {
        /// Frames to wait for a tile to measure (a big folder takes a while).
        const MAX_FRAMES: u32 = 120;
        /// Frames the count must hold to stop.
        const SETTLED: u32 = 2;
        if let Some(old) = self.group_tick.take() {
            old.remove();
        }
        if self.mode.get() != Mode::Grid || !self.grouped.get() {
            return;
        }
        let w = Rc::downgrade(self);
        let (frames, held) = (Cell::new(0), Cell::new(0));
        let id = self.groups.add_tick_callback(move |grid, _| {
            let Some(win) = w.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Ending by itself: forget the handle (it's no longer valid).
            let done = |win: &Self| {
                drop(win.group_tick.take());
                glib::ControlFlow::Break
            };
            frames.set(frames.get() + 1);
            if frames.get() > MAX_FRAMES || win.mode.get() != Mode::Grid || !win.grouped.get() {
                return done(&win);
            }
            let (Some(cell), width) = (file_cell_width(grid), grid.width()) else {
                return glib::ControlFlow::Continue;
            };
            if width <= 0 {
                return glib::ControlFlow::Continue;
            }
            let columns = u32::try_from(width / cell.max(1)).unwrap_or(1).clamp(1, 24);
            if columns == win.grouped_model.columns() && grid.max_columns() == columns {
                held.set(held.get() + 1);
                if held.get() >= SETTLED {
                    return done(&win);
                }
                return glib::ControlFlow::Continue;
            }
            held.set(0);
            if columns == win.grouped_model.columns() {
                // Same layout; only the grid's limits change.
                win.set_group_columns(columns);
                return glib::ControlFlow::Continue;
            }
            // Laying out again requests a new check, which replaces this
            // one; let go of it first so that one doesn't remove it.
            drop(win.group_tick.take());
            win.set_group_columns(columns);
            glib::ControlFlow::Break
        });
        self.group_tick.replace(Some(id));
    }

    /// The widget showing files right now (for focus).
    pub(super) fn active_view(&self) -> gtk::Widget {
        match (self.mode.get(), self.grouped.get()) {
            (Mode::List, _) => self.view.clone().upcast(),
            (Mode::Grid, false) => self.grid.clone().upcast(),
            (Mode::Grid, true) => self.groups.clone().upcast(),
        }
    }

    pub(super) fn focus_view(&self) {
        self.active_view().grab_focus();
    }

    /// Scrolls to and focuses the item at `pos` of the shared model.
    pub(super) fn scroll_to_pos(&self, pos: u32) {
        let flags = gtk::ListScrollFlags::FOCUS;
        match (self.mode.get(), self.grouped.get()) {
            (Mode::List, _) => self.view.scroll_to(pos, None, flags, None),
            (Mode::Grid, false) => self.grid.scroll_to(pos, flags, None),
            (Mode::Grid, true) => {
                let Some(cell) = self.grouped_model.cell_of(pos) else {
                    return;
                };
                // A file in its group's first row brings the heading above
                // it into view too.
                let columns = self.grouped_model.columns().max(1);
                let row = cell - cell % columns;
                if row >= columns && self.grouped_model.is_heading(row - columns) {
                    self.groups
                        .scroll_to(row - columns, gtk::ListScrollFlags::NONE, None);
                }
                self.groups.scroll_to(cell, flags, None);
            }
        }
    }
}

/// The width GTK gives a file tile's cell in `grid` (its minimum, which is
/// what GTK sizes columns by), from the first file tile there; None before
/// any exists. Headings and fillers are skipped: their tile is hidden.
fn file_cell_width(grid: &gtk::GridView) -> Option<i32> {
    let mut cell = grid.first_child();
    while let Some(c) = cell {
        let tile_shown = c
            .first_child()
            .and_then(|b| b.last_child())
            .is_some_and(|t| t.is_visible());
        if tile_shown {
            let (min, ..) = c.measure(gtk::Orientation::Horizontal, -1);
            if min > 0 {
                return Some(min);
            }
        }
        cell = c.next_sibling();
    }
    None
}

fn group_heading() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("group-heading");
    let name = gtk::Label::new(None);
    name.add_css_class("group-name");
    let count = gtk::Label::new(None);
    count.add_css_class("group-count");
    row.append(&name);
    row.append(&count);
    row
}

fn fill_heading(row: &gtk::Box, name: &str, count: u32) {
    if let Some(label) = row.first_child().and_downcast::<gtk::Label>() {
        label.set_text(name);
    }
    if let Some(label) = row.last_child().and_downcast::<gtk::Label>() {
        label.set_text(&count.to_string());
    }
}
