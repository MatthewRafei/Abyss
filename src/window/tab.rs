//! One tab: a folder being browsed, with its own history, listing, filter,
//! selection and views. The window's toolbar, prompt, job and status bars
//! act on whichever tab is active (see `panes.rs`).
//!
//! A tab that isn't on screen (another tab of its pane, or a pane hidden
//! while the split is off) lets go of its views' models, so it builds no
//! tiles and loads no thumbnails; it keeps its listing and watches its
//! folder. A tab restored from the last session loads only when first
//! shown.

use super::*;
use crate::listing::{Listing, Notice};

pub struct Tab {
    pub(super) win: Weak<Win>,
    /// The tab's page in its pane: the views with the "Nothing here" label.
    pub(super) root: gtk::Overlay,
    /// Its entry in the pane's tab strip, and the name shown there.
    pub(super) chip: gtk::Box,
    pub(super) chip_label: gtk::Label,

    pub(super) view: gtk::ColumnView,
    /// Pages "list", "grid" and "groups" (grouped grid); carries the zoom class.
    pub(super) views: gtk::Stack,
    pub(super) grid: gtk::GridView,
    /// The grouped grid: one grid over `grouped_model`'s layout.
    pub(super) groups: gtk::GridView,
    pub(super) grouped_model: crate::section::GroupedSelection,
    /// The running column check of the grouped grid, if any.
    pub(super) group_tick: RefCell<Option<gtk::TickCallbackId>>,
    pub(super) group_sorter: gtk::NumericSorter,
    pub(super) folders_first: gtk::NumericSorter,
    /// Reports the file area's size (drives the grouped grid's columns).
    pub(super) size_probe: gtk::DrawingArea,
    /// The shown folder's listing; `filtered` reads it, or search results.
    pub(super) list: RefCell<Rc<Listing>>,
    /// Listings of folders visited lately, most recent first, kept loaded
    /// (and watched) so going back to one is instant.
    pub(super) recent: RefCell<VecDeque<Rc<Listing>>>,
    pub(super) filtered: gtk::FilterListModel,
    pub(super) filter: gtk::CustomFilter,
    /// The filter text as typed, and lowercased once per keystroke.
    pub(super) search_text: RefCell<String>,
    pub(super) needle: RefCell<String>,
    /// Shown files waiting for their full type, and whether a lookup runs.
    pub(super) untyped: RefCell<Vec<gio::FileInfo>>,
    pub(super) typing: Cell<bool>,
    /// The recursive search under the current folder, if one is running.
    pub(super) search_task: RefCell<Option<glib::JoinHandle<()>>>,
    pub(super) search_state: Cell<SearchState>,
    pub(super) sorted: gtk::SortListModel,
    pub(super) selection: gtk::MultiSelection,
    pub(super) empty: gtk::Label,
    pub(super) status_pending: Cell<bool>,
    /// Room left where this tab is (footer), once looked up.
    pub(super) free_space: Cell<Option<u64>>,

    pub(super) current: RefCell<gio::File>,
    pub(super) back: RefCell<Vec<gio::File>>,
    pub(super) forward: RefCell<Vec<gio::File>>,
    /// On screen (its views follow the file list).
    pub(super) shown: Cell<bool>,
    /// Restored but not loaded yet: loads on first showing.
    pub(super) pending: Cell<bool>,
    /// Where the view was scrolled when the tab was last hidden.
    pub(super) scroll: Cell<f64>,
    /// Whether the files are sorted by group (as of the last sort).
    pub(super) sorted_grouped: Cell<bool>,
}

impl Tab {
    /// A tab at `start`; with `load` false it reads the folder only when
    /// first shown.
    // Builds the tab's widget tree; splitting it would just scatter it.
    #[allow(clippy::too_many_lines)]
    pub(super) fn new(win: &Rc<Win>, start: gio::File, load: bool) -> Rc<Self> {
        // File list: Listing -> filter -> sort -> selection.
        let list = Listing::new();
        let filter = gtk::CustomFilter::new(|_| true);
        let filtered = gtk::FilterListModel::new(Some(list.model().clone()), Some(filter.clone()));
        let sorted = gtk::SortListModel::new(Some(filtered.clone()), None::<gtk::Sorter>);
        let selection = gtk::MultiSelection::new(Some(sorted.clone()));
        // Dragging across empty space draws a rubber band that selects what
        // it covers (Ctrl adds to the selection, Shift takes away).
        let view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        view.add_css_class("files");
        view.set_show_column_separators(false);
        view.set_enable_rubberband(true);
        let empty = gtk::Label::new(Some("Nothing here"));
        empty.add_css_class("empty");
        empty.set_visible(false);
        empty.set_can_target(false);
        let view_scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();
        let grid = gtk::GridView::new(None::<gtk::SelectionModel>, None::<gtk::ListItemFactory>);
        grid.add_css_class("files");
        grid.set_max_columns(24);
        grid.set_enable_rubberband(true);
        // No sideways scrolling: the grid wraps to the window's width.
        let grid_scroll = gtk::ScrolledWindow::builder()
            .child(&grid)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let grouped_model = crate::section::GroupedSelection::new(&selection, |item| {
            item.downcast_ref::<gio::FileInfo>()
                .map_or("Other", |info| views::category(info).1)
        });
        let groups = gtk::GridView::new(None::<gtk::SelectionModel>, None::<gtk::ListItemFactory>);
        groups.add_css_class("files");
        groups.add_css_class("groups");
        groups.set_enable_rubberband(true);
        // External: no sideways scrolling, and the grid's width doesn't
        // become the pane's minimum (it's held to an exact column count,
        // which would stop the pane, and the window, from shrinking).
        let groups_scroll = gtk::ScrolledWindow::builder()
            .child(&groups)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::External)
            .build();
        for scroll in [&view_scroll, &grid_scroll, &groups_scroll] {
            watch_scroll(&scroll.vadjustment());
        }
        let views = gtk::Stack::new();
        views.add_css_class("views");
        views.add_css_class(&format!("zoom-{}", ZOOM_LEVELS[win.zoom.get()]));
        views.add_named(&view_scroll, Some("list"));
        views.add_named(&grid_scroll, Some("grid"));
        views.add_named(&groups_scroll, Some("groups"));
        let g = win.grouped.clone();
        let group_sorter = gtk::NumericSorter::new(Some(info_key(move |info| {
            if g.get() {
                u32::from(views::category(info).0)
            } else {
                0
            }
        })));
        let folders_first =
            gtk::NumericSorter::new(Some(info_key(|info| u32::from(!is_dir(info)))));
        let root = gtk::Overlay::new();
        root.set_child(Some(&views));
        root.add_overlay(&empty);
        root.add_css_class("tab-page");
        // An invisible, click-through layer over the file area: GTK reports
        // its size on every allocation.
        let size_probe = gtk::DrawingArea::new();
        size_probe.set_can_target(false);
        size_probe.set_can_focus(false);
        root.add_overlay(&size_probe);

        // Its entry in the tab strip: the folder's name and a close button.
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        chip.add_css_class("tab-chip");
        let chip_label = gtk::Label::new(None);
        chip_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        chip_label.set_max_width_chars(20);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_tooltip_text(Some("Close tab (Ctrl+W)"));
        close.set_focusable(false);
        chip.append(&chip_label);
        chip.append(&close);

        let tab = Rc::new(Self {
            win: Rc::downgrade(win),
            root,
            chip,
            chip_label,
            view,
            views,
            grid,
            groups,
            grouped_model,
            group_tick: RefCell::default(),
            group_sorter,
            folders_first,
            size_probe,
            list: RefCell::new(list),
            recent: RefCell::default(),
            filtered,
            filter,
            search_text: RefCell::default(),
            needle: RefCell::default(),
            untyped: RefCell::default(),
            typing: Cell::new(false),
            search_task: RefCell::default(),
            search_state: Cell::new(SearchState::Off),
            sorted,
            selection,
            empty,
            status_pending: Cell::new(false),
            free_space: Cell::new(None),
            current: RefCell::new(start.clone()),
            back: RefCell::default(),
            forward: RefCell::default(),
            shown: Cell::new(false),
            pending: Cell::new(!load),
            scroll: Cell::new(0.0),
            sorted_grouped: Cell::new(false),
        });

        let w = Rc::downgrade(&tab);
        close.connect_clicked(move |_| {
            if let Some(tab) = w.upgrade() {
                if let Some(win) = tab.win() {
                    win.close_tab(&tab);
                }
            }
        });
        Self::setup_chip(&tab);
        Self::setup_columns(&tab);
        Self::setup_views(&tab);
        Self::setup_filter(&tab);
        Self::setup_dnd(&tab);
        Self::setup_background_menu(&tab);
        Self::setup_click_clear(&tab);
        Self::setup_scroll_zoom(&tab);
        Self::setup_local_shortcuts(&tab);
        Self::setup_activation(&tab);
        let first = tab.list.borrow().clone();
        tab.watch_list(&first);
        tab.update_chip();
        if load {
            tab.navigate(start, false);
        }
        tab
    }

    pub(super) fn win(&self) -> Option<Rc<Win>> {
        self.win.upgrade()
    }

    /// Whether this is the window's active tab (the toolbar acts on it).
    pub(super) fn is_active(&self) -> bool {
        self.win()
            .and_then(|w| w.tab())
            .is_some_and(|t| std::ptr::eq(Rc::as_ptr(&t), self))
    }

    /// Clicking, or focusing, anywhere in the tab makes it the active one.
    fn setup_activation(tab: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(tab);
        click.connect_pressed(move |_, _, _, _| {
            if let Some(tab) = w.upgrade() {
                if let (Some(win), false) = (tab.win(), tab.is_active()) {
                    win.select_tab(&tab);
                }
            }
        });
        tab.root.add_controller(click);
        let focus = gtk::EventControllerFocus::new();
        let w = Rc::downgrade(tab);
        focus.connect_enter(move |_| {
            if let Some(tab) = w.upgrade() {
                if let (Some(win), false) = (tab.win(), tab.is_active()) {
                    win.select_tab(&tab);
                }
            }
        });
        tab.root.add_controller(focus);
    }

    /// The tab strip entry: click selects, middle-click closes, and files
    /// dragged onto it go into its folder (hovering a moment switches to it).
    fn setup_chip(tab: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(0);
        let w = Rc::downgrade(tab);
        click.connect_pressed(move |g, _, _, _| {
            let Some(tab) = w.upgrade() else { return };
            let Some(win) = tab.win() else { return };
            match g.current_button() {
                gdk::BUTTON_MIDDLE => win.close_tab(&tab),
                gdk::BUTTON_PRIMARY => {
                    win.select_tab(&tab);
                    tab.focus_view();
                }
                _ => {}
            }
        });
        tab.chip.add_controller(click);
        let w = Rc::downgrade(tab);
        let folder = Rc::new(move || w.upgrade().map(|tab| tab.current.borrow().clone()));
        if let Some(win) = tab.win() {
            Win::attach_drop(
                &Rc::downgrade(&win),
                &tab.chip,
                folder,
                HoverOpen::Select(Rc::downgrade(tab)),
            );
        }
    }

    /// The strip shows the folder's name; its tooltip, the full location.
    pub(super) fn update_chip(&self) {
        let file = self.current.borrow().clone();
        let name = if file.parent().is_none() {
            root_label(&file)
        } else {
            ops::display_name(&file)
        };
        self.chip_label.set_text(&name);
        self.chip.set_tooltip_text(Some(&location_text(&file)));
    }

    /// Lets go of everything (the tab is being closed).
    pub(super) fn dispose(&self) {
        self.stop_search_quiet();
        if let Some(tick) = self.group_tick.take() {
            tick.remove();
        }
        self.view.set_model(None::<&gtk::SelectionModel>);
        self.grid.set_model(None::<&gtk::SelectionModel>);
        self.groups.set_model(None::<&gtk::SelectionModel>);
        self.grouped_model.set_columns(None);
        self.grouped_model.detach();
        self.list.borrow().load(None, "");
        self.recent.borrow_mut().clear();
    }

    /// Puts the tab on screen or takes it off: off screen its views let go
    /// of their models (no tiles, no thumbnails), keeping the scroll
    /// position for when it's back.
    pub(super) fn set_shown(self: &Rc<Self>, on: bool) {
        if self.shown.replace(on) == on {
            return;
        }
        if on {
            if self.pending.replace(false) {
                let start = self.current.borrow().clone();
                self.navigate(start, false);
            }
            self.apply_view();
            let value = self.scroll.get();
            if value > 0.0 {
                if let Some(adj) = self.active_adjustment() {
                    glib::idle_add_local_once(move || adj.set_value(value));
                }
            }
            self.schedule_status();
        } else {
            self.scroll
                .set(self.active_adjustment().map_or(0.0, |a| a.value()));
            if let Some(tick) = self.group_tick.take() {
                tick.remove();
            }
            self.view.set_model(None::<&gtk::SelectionModel>);
            self.grid.set_model(None::<&gtk::SelectionModel>);
            self.groups.set_model(None::<&gtk::SelectionModel>);
            self.grouped_model.set_columns(None);
        }
    }

    fn active_adjustment(&self) -> Option<gtk::Adjustment> {
        let win = self.win()?;
        match (win.mode.get(), win.grouped.get()) {
            (Mode::List, _) => self.view.vadjustment(),
            (Mode::Grid, false) => self.grid.vadjustment(),
            (Mode::Grid, true) => self.groups.vadjustment(),
        }
    }

    // ------------------------------------------------------------ columns --

    #[allow(clippy::too_many_lines)]
    fn setup_columns(tab: &Rc<Self>) {
        let w = Rc::downgrade(tab);

        let name_factory = gtk::SignalListItemFactory::new();
        name_factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let image = gtk::Image::new();
            image.add_css_class("file-icon");
            track_icon(&image);
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            row.append(&image);
            row.append(&label);
            item.set_child(Some(&row));
            Self::attach_item_menu(&w, item, &row, false);
            Self::attach_item_dnd(&w, item, &row, false);
        });
        name_factory.connect_bind(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(info) = item.item().and_downcast::<gio::FileInfo>() else {
                return;
            };
            let Some(row) = item.child().and_downcast::<gtk::Box>() else {
                return;
            };
            let (Some(image), Some(label)) = (
                row.first_child().and_downcast::<gtk::Image>(),
                row.last_child().and_downcast::<gtk::Label>(),
            ) else {
                return;
            };
            image.set_from_gicon(&crate::info::icon(&info));
            show_thumbnail(&image, None, &info);
            match hit_markup(&info) {
                Some(markup) => label.set_markup(&markup),
                None => label.set_text(&info.display_name()),
            }
            row.set_opacity(if is_hidden(&info) { 0.6 } else { 1.0 });
        });

        let text_factory = |w: Weak<Self>, f: fn(&gio::FileInfo) -> String| {
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.add_css_class("dim-label");
                item.set_child(Some(&label));
                Self::attach_item_menu(&w, item, &label, false);
                Self::attach_item_dnd(&w, item, &label, false);
            });
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(info) = item.item().and_downcast::<gio::FileInfo>() else {
                    return;
                };
                if let Some(label) = item.child().and_downcast::<gtk::Label>() {
                    label.set_text(&f(&info));
                }
            });
            factory
        };

        let size_text = |info: &gio::FileInfo| {
            // Folders, and entries like an SMB share list's, have no size.
            if is_dir(info) || !info.has_attribute("standard::size") {
                String::new()
            } else {
                glib::format_size(ops::size_of(info)).to_string()
            }
        };

        // Keyed sorters: GTK works out each file's key once per sort.
        let name_sorter = gtk::StringSorter::new(Some(info_key(|i| i.display_name().to_string())));
        name_sorter.set_collation(gtk::Collation::Filename);
        let size_sorter = gtk::NumericSorter::new(Some(info_key(ops::size_of)));
        let time_sorter = gtk::NumericSorter::new(Some(info_key(|i| {
            i.modification_date_time().map_or(0, |d| d.to_unix())
        })));
        let type_sorter = gtk::StringSorter::new(Some(info_key(type_text)));

        let name_col = gtk::ColumnViewColumn::new(Some("Name"), Some(name_factory));
        name_col.set_expand(true);
        name_col.set_resizable(true);
        name_col.set_sorter(Some(&name_sorter));
        let size_col = gtk::ColumnViewColumn::new(
            Some("Size"),
            Some(text_factory(Rc::downgrade(tab), size_text)),
        );
        size_col.set_fixed_width(100);
        size_col.set_sorter(Some(&size_sorter));
        let time_col = gtk::ColumnViewColumn::new(
            Some("Modified"),
            Some(text_factory(Rc::downgrade(tab), format_time)),
        );
        time_col.set_fixed_width(130);
        time_col.set_sorter(Some(&time_sorter));
        let type_col = gtk::ColumnViewColumn::new(
            Some("Type"),
            Some(text_factory(Rc::downgrade(tab), type_text)),
        );
        type_col.set_fixed_width(150);
        type_col.set_resizable(true);
        type_col.set_sorter(Some(&type_sorter));
        tab.view.append_column(&name_col);
        tab.view.append_column(&size_col);
        tab.view.append_column(&type_col);
        tab.view.append_column(&time_col);

        // The headers pick the column; `apply_sort` builds the model's sorter.
        if let Some(picked) = tab.view.sorter() {
            let w = Rc::downgrade(tab);
            picked.connect_changed(move |_, _| {
                if let Some(tab) = w.upgrade() {
                    tab.apply_sort();
                }
            });
        }
        tab.view
            .sort_by_column(Some(&name_col), gtk::SortType::Ascending);
        tab.apply_sort();

        let w = Rc::downgrade(tab);
        tab.view.connect_activate(move |_, pos| {
            if let Some(tab) = w.upgrade() {
                tab.activate(pos);
            }
        });

        let w = Rc::downgrade(tab);
        tab.selection.connect_selection_changed(move |_, _, _| {
            if let Some(tab) = w.upgrade() {
                tab.schedule_status();
            }
        });
        let w = Rc::downgrade(tab);
        tab.sorted.connect_items_changed(move |_, _, _, _| {
            if let Some(tab) = w.upgrade() {
                tab.schedule_status();
                tab.keep_top();
            }
        });
    }

    /// Sorts again from scratch. GTK keeps the sort keys it computed when
    /// told a sorter changed (as long as their kind is the same), so a file
    /// whose group changed would stay where it was: dropping the sorter
    /// first makes it work every key out again.
    pub(super) fn resort(&self) {
        self.sorted.set_sorter(None::<&gtk::Sorter>);
        self.apply_sort();
    }

    /// Sorts the model by group, folders first, then the column picked in
    /// the header. The column view's own sorter isn't put on the model: it
    /// gives GTK no sort keys, so every comparison would redo the work.
    fn apply_sort(&self) {
        let picked = self.view.sorter().and_downcast::<gtk::ColumnViewSorter>();
        let descending = picked
            .as_ref()
            .is_some_and(|p| p.primary_sort_order() == gtk::SortType::Descending);
        let by = picked
            .and_then(|p| p.primary_sort_column())
            .and_then(|c| c.sorter());
        let all = gtk::MultiSorter::new();
        all.append(self.group_sorter.clone());
        if crate::prefs::get().folders_first {
            all.append(self.folders_first.clone());
        }
        if let Some(by) = by {
            match by.downcast::<gtk::NumericSorter>() {
                Ok(numeric) => {
                    let keyed = gtk::NumericSorter::new(numeric.expression());
                    keyed.set_sort_order(if descending {
                        gtk::SortType::Descending
                    } else {
                        gtk::SortType::Ascending
                    });
                    all.append(keyed);
                }
                // Strings can't sort keyed in reverse; Z→A compares directly.
                Err(by) if descending => {
                    all.append(gtk::CustomSorter::new(move |a, b| by.compare(b, a)));
                }
                Err(by) => all.append(by),
            }
        }
        self.sorted.set_sorter(Some(&all));
    }

    // ------------------------------------------------------------- status --

    /// Updates the "Nothing here" label and, for the active tab, the footer,
    /// once the current burst of changes is over: a folder loads in many
    /// batches, and each would walk the selection.
    pub(super) fn schedule_status(self: &Rc<Self>) {
        if self.status_pending.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(tab) = w.upgrade() else { return };
            tab.status_pending.set(false);
            tab.refresh_empty();
            if tab.is_active() {
                if let Some(win) = tab.win() {
                    win.update_status();
                }
            }
        });
    }

    fn loading(&self) -> bool {
        self.list.borrow().is_loading() || self.search_state.get() == SearchState::Running
    }

    fn refresh_empty(&self) {
        let empty = self.sorted.n_items() == 0 && !self.loading() && !self.pending.get();
        self.empty.set_visible(empty);
        self.empty
            .set_text(if self.search_text.borrow().is_empty() {
                "Nothing here"
            } else {
                "No matches"
            });
    }

    /// The footer's text for this tab: how many items (or matches), and
    /// what's selected.
    pub(super) fn status_text(&self) -> String {
        let total = self.sorted.n_items();
        let state = self.search_state.get();
        let loading = self.loading();
        let matches = if total == 1 { "match" } else { "matches" };
        let mut text = match state {
            SearchState::Running => format!("{total} {matches} so far, searching subfolders…"),
            SearchState::Done if usize::try_from(total).unwrap_or(0) >= search::MAX_RESULTS => {
                format!("First {total} matches here and in subfolders; type more to narrow")
            }
            SearchState::Done => format!("{total} {matches} here and in subfolders"),
            SearchState::Off if loading && total == 0 => "Loading…".to_string(),
            SearchState::Off => format!("{total} item{}", if total == 1 { "" } else { "s" }),
        };
        // Count and size straight from the selection (no list of files).
        let set = self.selection.selection();
        if !set.is_empty() {
            let mut bytes = 0;
            if let Some((iter, first)) = gtk::BitsetIter::init_first(&set) {
                for pos in std::iter::once(first).chain(iter) {
                    if let Some(info) = self.item(pos).filter(|i| !is_dir(i)) {
                        bytes += ops::size_of(&info);
                    }
                }
            }
            let _ = write!(text, " · {} selected", set.size());
            if bytes > 0 {
                let _ = write!(text, " ({})", glib::format_size(bytes));
            }
        }
        if let Some(free) = self.free_space.get() {
            let _ = write!(text, " · {} free", glib::format_size(free));
        }
        text
    }

    /// Looks up how much room is left where this tab is, for the footer.
    pub(super) fn refresh_free_space(self: &Rc<Self>) {
        let here = self.current.borrow().clone();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let free = here
                .query_filesystem_info_future("filesystem::free", glib::Priority::LOW)
                .await
                .ok()
                .filter(|i| i.has_attribute("filesystem::free"))
                .map(|i| i.attribute_uint64("filesystem::free"));
            let Some(tab) = w.upgrade() else { return };
            // Moved on meanwhile: that folder's answer is coming.
            if !tab.current.borrow().equal(&here) {
                return;
            }
            tab.free_space.set(free);
            tab.schedule_status();
        });
    }

    // ------------------------------------------------------- items & menus --

    /// The position in the shared selection of a view's item: the grouped
    /// grid's positions include headings and fillers.
    pub(super) fn shared_pos(&self, grouped: bool, pos: u32) -> Option<u32> {
        if pos == gtk::INVALID_LIST_POSITION {
            None
        } else if grouped {
            self.grouped_model.file_at(pos)
        } else {
            Some(pos)
        }
    }

    /// Right-click on a row selects it (unless it already is) and shows the
    /// file menu; middle-click on a folder opens it in a new tab.
    pub(super) fn attach_item_menu(
        w: &Weak<Self>,
        item: &gtk::ListItem,
        widget: &impl IsA<gtk::Widget>,
        grouped: bool,
    ) {
        let click = gtk::GestureClick::new();
        click.set_button(0);
        let w = w.clone();
        let item = item.downgrade();
        let widget_ref = widget.as_ref().downgrade();
        click.connect_pressed(move |gesture, _, x, y| {
            let button = gesture.current_button();
            if button != gdk::BUTTON_SECONDARY && button != gdk::BUTTON_MIDDLE {
                return;
            }
            let (Some(tab), Some(item), Some(widget)) =
                (w.upgrade(), item.upgrade(), widget_ref.upgrade())
            else {
                return;
            };
            let Some(win) = tab.win() else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let Some(pos) = tab.shared_pos(grouped, item.position()) else {
                return;
            };
            if button == gdk::BUTTON_MIDDLE {
                let Some(info) = tab.item(pos) else { return };
                if let (true, Some(file)) = (is_dir(&info), info_file(info.upcast_ref())) {
                    win.open_tab_beside(&tab, file, false);
                }
                return;
            }
            win.select_tab(&tab);
            if !tab.selection.is_selected(pos) {
                tab.selection.select_item(pos, true);
            }
            let (x, y) = translate(&widget, &tab.views, x, y);
            win.show_file_menu(x, y);
        });
        widget.add_controller(click);
    }

    /// Ctrl+scroll zooms the file list.
    fn setup_scroll_zoom(tab: &Rc<Self>) {
        // Discrete: GTK adds up a touchpad's small movements into whole
        // notches, so each notch is exactly one zoom level. A short pause
        // after each step keeps a fast flick from skipping levels.
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(tab);
        scroll.connect_scroll(move |c, _, dy| {
            let ctrl = c
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            let Some(win) = w.upgrade().and_then(|t| t.win()) else {
                return glib::Propagation::Proceed;
            };
            if !ctrl || dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let settled = win
                .zoom_step_at
                .get()
                .is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(120));
            if settled {
                win.zoom_step_at.set(Some(std::time::Instant::now()));
                win.zoom_by(if dy < 0.0 { 1 } else { -1 });
            }
            glib::Propagation::Stop
        });
        tab.views.add_controller(scroll);
    }

    fn setup_background_menu(tab: &Rc<Self>) {
        let views: [gtk::Widget; 3] = [
            tab.view.clone().upcast(),
            tab.grid.clone().upcast(),
            tab.groups.clone().upcast(),
        ];
        for view in views {
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_SECONDARY);
            let w = Rc::downgrade(tab);
            let v = view.downgrade();
            click.connect_pressed(move |_, _, x, y| {
                let (Some(tab), Some(v)) = (w.upgrade(), v.upgrade()) else {
                    return;
                };
                let Some(win) = tab.win() else { return };
                win.select_tab(&tab);
                tab.selection.unselect_all();
                let (x, y) = translate(&v, &tab.views, x, y);
                win.show_folder_menu(x, y);
            });
            view.add_controller(click);
        }
    }

    /// A plain click on empty space (below the rows, between tiles, on a
    /// group heading) clears the selection, as in Nautilus. Watched before
    /// the views handle it; a drag from there is a rubber band instead, and
    /// Ctrl / Shift clicks keep the selection.
    fn setup_click_clear(tab: &Rc<Self>) {
        let views: [gtk::Widget; 3] = [
            tab.view.clone().upcast(),
            tab.grid.clone().upcast(),
            tab.groups.clone().upcast(),
        ];
        for view in views {
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_PRIMARY);
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let pressed_empty = Rc::new(Cell::new(None::<(f64, f64)>));
            let p = pressed_empty.clone();
            let v = view.downgrade();
            click.connect_pressed(move |g, n, x, y| {
                let modified = g
                    .current_event_state()
                    .intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
                let empty = n == 1 && !modified && v.upgrade().is_some_and(|v| !on_file(&v, x, y));
                p.set(empty.then_some((x, y)));
            });
            let w = Rc::downgrade(tab);
            let v = view.downgrade();
            click.connect_released(move |_, _, x, y| {
                let Some((sx, sy)) = pressed_empty.take() else {
                    return;
                };
                let (Some(tab), Some(v)) = (w.upgrade(), v.upgrade()) else {
                    return;
                };
                if !v.drag_check_threshold(px(sx), px(sy), px(x), px(y)) {
                    // After GTK's own handling: a grid cell is wider than its
                    // tile, and GTK selects the cell's file on any click in it.
                    glib::idle_add_local_once(move || {
                        tab.selection.unselect_all();
                    });
                }
            });
            view.add_controller(click);
        }
    }

    /// Keys for the file list itself, so text fields keep theirs.
    fn setup_local_shortcuts(tab: &Rc<Self>) {
        fn add(controller: &gtk::ShortcutController, w: &Weak<Tab>, accel: &str, f: fn(&Rc<Win>)) {
            let w = w.clone();
            let action = gtk::CallbackAction::new(move |_, _| {
                if let Some(win) = w.upgrade().and_then(|t| t.win()) {
                    f(&win);
                }
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(accel),
                Some(action),
            ));
        }
        let w = Rc::downgrade(tab);
        let local = gtk::ShortcutController::new();
        add(&local, &w, "Delete", |win| {
            let Some(tab) = win.tab() else { return };
            let files = tab.selected_files();
            if files.is_empty() {
                return;
            }
            if tab.current.borrow().uri_scheme().as_deref() == Some("trash") {
                win.confirm_delete(files);
            } else {
                win.trash(files);
            }
        });
        add(&local, &w, "<Shift>Delete", |win| {
            let files = win.tab().map(|t| t.selected_files()).unwrap_or_default();
            if !files.is_empty() {
                win.confirm_delete(files);
            }
        });
        add(&local, &w, "F2", Win::start_rename);
        add(&local, &w, "<Alt>Return", Win::show_properties);
        add(&local, &w, "BackSpace", |win| {
            if let Some(tab) = win.tab() {
                tab.go_up();
            }
        });
        add(&local, &w, "Menu", |win| win.show_file_menu(40.0, 40.0));
        tab.views.add_controller(local);
    }

    // ------------------------------------------------------- drag & drop --

    /// Rows drag their selection out, and folder rows take drops (and open
    /// after hovering), as in nautilus-list-base.c `setup_cell_common()`.
    pub(super) fn attach_item_dnd(
        w: &Weak<Self>,
        item: &gtk::ListItem,
        widget: &impl IsA<gtk::Widget>,
        grouped: bool,
    ) {
        let source = gtk::DragSource::new();
        source.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ww = w.clone();
        let it = item.downgrade();
        source.connect_prepare(move |source, _, _| {
            let (tab, item) = (ww.upgrade()?, it.upgrade()?);
            let pos = tab.shared_pos(grouped, item.position())?;
            tab.drag_prepare(source, pos)
        });
        widget.add_controller(source.clone());

        // GTK's drag source won't start until 100 ms after the press, but
        // the view's rubber band starts as soon as the pointer moves a few
        // pixels: a quick drag (or one while the app was busy, say loading
        // a big folder) became a rubber band selection instead. Moving off
        // a file is claimed here first, which keeps it for the drag; rubber
        // bands start from empty space.
        let hold = gtk::GestureDrag::new();
        hold.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ww = w.clone();
        let it = item.downgrade();
        hold.connect_drag_update(move |gesture, dx, dy| {
            let Some(widget) = gesture.widget() else {
                return;
            };
            let moved = widget.drag_check_threshold(0, 0, px(dx), px(dy));
            // Only on a file (a grouped grid's heading cell isn't one).
            let on_file = ww
                .upgrade()
                .zip(it.upgrade())
                .and_then(|(tab, item)| tab.shared_pos(grouped, item.position()))
                .is_some();
            if moved && on_file {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        });
        widget.add_controller(hold.clone());
        // Grouped with the drag source, so the claim keeps it alive (GTK
        // denies the sequence to a claim's ungrouped neighbours).
        hold.group_with(&source);

        let it = item.downgrade();
        let folder = Rc::new(move || {
            let info = it.upgrade()?.item().and_downcast::<gio::FileInfo>()?;
            is_dir(&info)
                .then(|| info_file(info.upcast_ref()))
                .flatten()
        });
        let win = w.upgrade().map(|t| t.win.clone()).unwrap_or_default();
        Win::attach_drop(&win, widget, folder, HoverOpen::Tab(w.clone()));
    }

    fn drag_prepare(&self, source: &gtk::DragSource, pos: u32) -> Option<gdk::ContentProvider> {
        if pos == gtk::INVALID_LIST_POSITION {
            return None;
        }
        // Dragging an unselected row drags just that row.
        if !self.selection.is_selected(pos) {
            self.selection.select_item(pos, true);
        }
        let selected = self.selected();
        if selected.is_empty() {
            return None;
        }
        let mut actions = gdk::DragAction::COPY | gdk::DragAction::MOVE | gdk::DragAction::LINK;
        if selected
            .iter()
            .any(|(i, _)| i.has_attribute("access::can-delete") && !i.boolean("access::can-delete"))
        {
            actions.remove(gdk::DragAction::MOVE);
        }
        source.set_actions(actions);
        let icon = crate::info::icon(&selected[0].0);
        let theme = gtk::IconTheme::for_display(&WidgetExt::display(&self.views));
        let paintable = theme.lookup_by_gicon(
            &icon,
            32,
            self.views.scale_factor(),
            gtk::TextDirection::None,
            gtk::IconLookupFlags::empty(),
        );
        source.set_icon(Some(&paintable), 0, 0);
        let files: Vec<gio::File> = selected.into_iter().map(|(_, f)| f).collect();
        Some(gdk::ContentProvider::for_value(
            &gdk::FileList::from_array(&files).to_value(),
        ))
    }

    fn setup_dnd(tab: &Rc<Self>) {
        // Background of the list: drop into this tab's folder.
        let w = Rc::downgrade(tab);
        let current = Rc::new(move || w.upgrade().map(|tab| tab.current.borrow().clone()));
        Win::attach_drop(&tab.win, &tab.views, current, HoverOpen::No);
    }

    // ------------------------------------------------------------- filter --

    fn setup_filter(tab: &Rc<Self>) {
        let w = Rc::downgrade(tab);
        tab.filter.set_filter_func(move |obj| {
            let Some(tab) = w.upgrade() else { return true };
            let Some(info) = obj.downcast_ref::<gio::FileInfo>() else {
                return false;
            };
            let hidden_shown = tab.win().is_some_and(|win| win.show_hidden.get());
            if !hidden_shown && is_hidden(info) {
                return false;
            }
            let shown = {
                let needle = tab.needle.borrow();
                needle.is_empty() || search::matches(&info.display_name(), &needle)
            };
            if shown && crate::info::needs_type(info) {
                tab.queue_typing(info);
            }
            shown
        });
    }

    /// Filters by `text` (the filter box's text for this tab). A longer term
    /// only hides more, a shorter one only shows more: GTK then rechecks
    /// just the files that could change. Subfolders are searched too.
    pub(super) fn set_search(self: &Rc<Self>, text: &str) {
        self.search_text.replace(text.to_owned());
        let needle = search::fold(text);
        let old = self.needle.replace(needle.clone());
        if needle != old {
            self.filter.changed(if needle.contains(old.as_str()) {
                gtk::FilterChange::MoreStrict
            } else if old.contains(needle.as_str()) {
                gtk::FilterChange::LessStrict
            } else {
                gtk::FilterChange::Different
            });
            self.start_search();
        }
        // Typing goes to the filter while focus stays on the list, so make
        // the first match the current, selected row: Enter then opens it,
        // and F2 / Menu / Delete act on it.
        if !text.is_empty() && self.selection.n_items() > 0 {
            self.selection.select_item(0, true);
            self.scroll_to_pos(0);
        }
        self.schedule_status();
    }

    /// Hidden files were shown or hidden (a window-wide setting).
    pub(super) fn hidden_changed(self: &Rc<Self>, show: bool) {
        self.filter.changed(if show {
            gtk::FilterChange::LessStrict
        } else {
            gtk::FilterChange::MoreStrict
        });
        // Hidden folders are skipped while searching, so search again.
        if self.search_state.get() != SearchState::Off {
            self.start_search();
        }
    }

    // -------------------------------------------------------------- types --

    /// Queues a shown file whose name didn't settle its type for a look at
    /// its contents (see `info::needs_type`).
    fn queue_typing(self: &Rc<Self>, info: &gio::FileInfo) {
        self.untyped.borrow_mut().push(info.clone());
        if self.typing.replace(true) {
            return;
        }
        // Not from inside the filter: the models are mid-update.
        let w = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(tab) = w.upgrade() {
                tab.type_queued();
            }
        });
    }

    /// Looks up the full types of queued files, a few at a time, and
    /// refreshes the rows whose type turned out to be something.
    fn type_queued(self: &Rc<Self>) {
        use futures_util::StreamExt;
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                let Some(batch) = w.upgrade().map(|tab| tab.untyped.take()) else {
                    return;
                };
                if batch.is_empty() {
                    break;
                }
                let typed: Vec<gio::FileInfo> = futures_util::stream::iter(batch)
                    .map(|info| async move {
                        let file = info_file(info.upcast_ref())?;
                        let found = file
                            .query_info_future(
                                "standard::content-type",
                                gio::FileQueryInfoFlags::NONE,
                                glib::Priority::LOW,
                            )
                            .await
                            .ok()?;
                        let ct = found.content_type()?;
                        (ct != "application/octet-stream").then(|| {
                            crate::info::set_full_type(&info, &ct);
                            info
                        })
                    })
                    .buffer_unordered(crate::par::in_flight())
                    .filter_map(std::future::ready)
                    .collect()
                    .await;
                let Some(tab) = w.upgrade() else { return };
                tab.refresh_rows(&typed);
            }
            if let Some(tab) = w.upgrade() {
                tab.typing.set(false);
                // Queued while the last batch was being looked up.
                if !tab.untyped.borrow().is_empty() && !tab.typing.replace(true) {
                    tab.type_queued();
                }
            }
        });
    }

    /// Re-shows the icon and thumbnail of `infos`' rows, whose type changed
    /// (GTK won't refill a row whose item is the same object), and regroups.
    fn refresh_rows(&self, infos: &[gio::FileInfo]) {
        if infos.is_empty() {
            return;
        }
        let by_uri: HashMap<String, &gio::FileInfo> = infos
            .iter()
            .filter_map(|i| Some((info_file(i.upcast_ref())?.uri().to_string(), i)))
            .collect();
        for image in shown_icons() {
            if let Some(info) = by_uri.get(image.widget_name().as_str()) {
                image.set_from_gicon(&crate::info::icon(info));
                let picture = image.next_sibling().and_downcast::<gtk::Picture>();
                show_thumbnail(&image, picture.as_ref(), info);
            }
        }
        // Their group (from the type) may have changed.
        if self.win().is_some_and(|w| w.grouped.get()) {
            self.resort();
        }
    }

    // ------------------------------------------------------------- search --

    /// Searches the subfolders for the current term (after the folder's own
    /// files were filtered instantly), or goes back to the folder listing
    /// when the term is too short or the folder can't be searched.
    fn start_search(self: &Rc<Self>) {
        use futures_util::StreamExt;
        self.stop_search();
        let needle = self.needle.borrow().clone();
        let dir = self.current.borrow().clone();
        if needle.chars().count() < SEARCH_MIN_CHARS || !search::can_search(&dir) {
            return;
        }
        self.search_state.set(SearchState::Running);
        self.schedule_status();
        let hidden = self.win().is_some_and(|w| w.show_hidden.get());
        let w = Rc::downgrade(self);
        let handle = glib::spawn_future_local(async move {
            let store = gio::ListStore::new::<gio::FileInfo>();
            let (tx, rx) = futures_channel::mpsc::unbounded::<Vec<PathBuf>>();
            let find = search::run(&dir, &needle, hidden, move |batch| {
                let _ = tx.unbounded_send(batch);
            });
            let seen = RefCell::new(HashSet::new());
            let show = rx.for_each(|batch| {
                let fresh: Vec<PathBuf> = batch
                    .into_iter()
                    .filter(|p| seen.borrow_mut().insert(p.clone()))
                    .collect();
                let (w, store, dir) = (w.clone(), store.clone(), dir.clone());
                async move {
                    let infos = hit_infos(&dir, fresh).await;
                    let Some(tab) = w.upgrade() else { return };
                    store.extend_from_slice(&infos);
                    // Swap the folder's listing for the results once some
                    // are in, so typing never flashes an empty list.
                    if store.n_items() > 0 && !tab.showing(&store) {
                        tab.filtered.set_model(Some(&store));
                    }
                }
            });
            futures_util::future::join(find, show).await;
            if let Some(tab) = w.upgrade() {
                if !tab.showing(&store) {
                    tab.filtered.set_model(Some(&store));
                }
                tab.search_state.set(SearchState::Done);
                tab.search_task.replace(None);
                tab.schedule_status();
            }
        });
        self.search_task.replace(Some(handle));
    }

    fn showing(&self, model: &impl IsA<gio::ListModel>) -> bool {
        self.filtered
            .model()
            .is_some_and(|m| &m == model.upcast_ref::<gio::ListModel>())
    }

    /// Stops a recursive search and shows the folder's own listing again.
    fn stop_search(self: &Rc<Self>) {
        if self.stop_search_quiet() {
            self.schedule_status();
        }
    }

    /// `stop_search` without the status update; true if a search was on.
    fn stop_search_quiet(&self) -> bool {
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
        if self.search_state.replace(SearchState::Off) == SearchState::Off {
            return false;
        }
        let list = self.list.borrow().clone();
        if !self.showing(list.model()) {
            self.filtered.set_model(Some(list.model()));
        }
        true
    }

    // --------------------------------------------------------- navigation --

    pub(super) fn parse_location(&self, text: &str) -> gio::File {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix('~') {
            return gio::File::for_path(format!("{}{}", glib::home_dir().display(), rest));
        }
        if text.starts_with('/') || text.contains("://") || text.ends_with(":///") {
            return gio::File::for_commandline_arg(text);
        }
        self.current.borrow().resolve_relative_path(text)
    }

    pub(super) fn navigate(self: &Rc<Self>, file: gio::File, record: bool) {
        if record {
            let current = self.current.borrow().clone();
            if !current.equal(&file) {
                self.back.borrow_mut().push(current);
                self.forward.borrow_mut().clear();
            }
        }
        // A tab not loaded yet just remembers where it is.
        *self.current.borrow_mut() = file;
        if self.pending.get() {
            self.update_chip();
            if let Some(win) = self.win() {
                win.tab_changed(self);
            }
            return;
        }
        // Set before reloading: the path bar and sidebar read `current`.
        self.set_search("");
        if self.is_active() {
            if let Some(win) = self.win() {
                win.search.set_text("");
            }
        }
        self.selection.unselect_all();
        let file = self.current.borrow().clone();
        self.reload_at(&file, false);
    }

    /// Reports a listing's errors and progress while it's the one shown.
    pub(super) fn watch_list(self: &Rc<Self>, list: &Listing) {
        let w = Rc::downgrade(self);
        list.watch(move |list, notice| {
            let Some(tab) = w
                .upgrade()
                .filter(|tab| Rc::ptr_eq(&tab.list.borrow(), list))
            else {
                return;
            };
            if notice == Notice::Error {
                let Some(error) = list.error() else {
                    return;
                };
                // Mount on demand (nautilus-window-slot.c: mount_not_mounted_cb).
                if error.matches(gio::IOErrorEnum::NotMounted) {
                    let file = tab.current.borrow().clone();
                    tab.mount_then(file, |tab, file| tab.reload_at(&file, true));
                } else if let Some(win) = tab.win() {
                    win.show_error(&error.to_string());
                }
                return;
            }
            tab.schedule_status();
            if !list.is_loading() {
                tab.reveal_top();
                tab.refresh_free_space();
            }
        });
    }

    /// Shows `file`'s listing: the one kept from a recent visit when it's
    /// still good (and `fresh` isn't asked for), otherwise read anew. The
    /// listing being left is kept for later.
    fn show_listing(self: &Rc<Self>, file: &gio::File, fresh: bool) {
        self.stop_search();
        if crate::starred::is_view(file) {
            self.show_starred();
            return;
        }
        let current = self.list.borrow().clone();
        let reread = |list: &Listing| list.load(Some(file), crate::info::attributes(file));
        // The same folder (Reload), or the very first listing.
        if current.file().is_none_or(|f| f.equal(file)) {
            reread(&current);
            return;
        }
        let kept = {
            let mut recent = self.recent.borrow_mut();
            recent
                .iter()
                .position(|l| l.file().is_some_and(|f| f.equal(file)))
                .and_then(|i| recent.remove(i))
        };
        let next = match kept {
            // Local folders only (one stat), so a deleted one isn't shown stale.
            Some(list)
                if !fresh
                    && list.error().is_none()
                    && keeps_fresh(file)
                    && file.query_exists(gio::Cancellable::NONE) =>
            {
                list
            }
            Some(list) => {
                reread(&list);
                list
            }
            None => {
                let list = Listing::new();
                self.watch_list(&list);
                reread(&list);
                list
            }
        };
        self.list.replace(next.clone());
        self.filtered.set_model(Some(next.model()));
        // A kept listing would otherwise open where it was scrolled.
        // Only views showing the list (a hidden one has no model).
        let has_items = |m: Option<gtk::SelectionModel>| m.is_some_and(|m| m.n_items() > 0);
        if next.n_items() > 0 && self.shown.get() {
            if has_items(self.view.model()) {
                self.view
                    .scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
            }
            if has_items(self.grid.model()) {
                self.grid.scroll_to(0, gtk::ListScrollFlags::NONE, None);
            }
        }
        if current.file().is_some_and(|f| keeps_fresh(&f)) && current.error().is_none() {
            let mut recent = self.recent.borrow_mut();
            recent.push_front(current);
            let mut files = 0;
            let mut keep = 0;
            for list in recent.iter() {
                files += list.n_items();
                if keep == RECENT_FOLDERS || (keep > 0 && files > RECENT_FILES) {
                    break;
                }
                keep += 1;
            }
            recent.truncate(keep);
        }
    }

    /// Starred isn't a folder: its files are looked up wherever they are and
    /// shown together, like search results. The folder listing stays as it
    /// was (it comes back when the tab goes to a folder again).
    fn show_starred(self: &Rc<Self>) {
        let store = gio::ListStore::new::<gio::FileInfo>();
        self.filtered.set_model(Some(&store));
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            use futures_util::StreamExt;
            let infos: Vec<gio::FileInfo> = futures_util::stream::iter(crate::starred::all())
                .map(|uri| async move {
                    let file = gio::File::for_uri(&uri);
                    let attrs = file.parent().map_or_else(
                        || crate::info::attributes(&file),
                        |p| crate::info::attributes(&p),
                    );
                    let info = file
                        .query_info_future(
                            attrs,
                            gio::FileQueryInfoFlags::NONE,
                            glib::Priority::LOW,
                        )
                        .await
                        .ok()?;
                    info.set_attribute_object("standard::file", &file);
                    // Where it is, under its name (as search results show).
                    if let Some(parent) = file.parent() {
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
                        info.set_attribute_string("abyss::where", &place);
                    }
                    Some(info)
                })
                .buffered(crate::par::in_flight())
                .filter_map(std::future::ready)
                .collect()
                .await;
            let Some(tab) = w.upgrade() else { return };
            // Gone elsewhere meanwhile.
            if !tab.showing(&store) {
                return;
            }
            store.extend_from_slice(&infos);
            tab.schedule_status();
        });
    }

    pub(super) fn reload_at(self: &Rc<Self>, file: &gio::File, fresh: bool) {
        self.show_listing(file, fresh);
        self.update_chip();
        if let Some(win) = self.win() {
            win.tab_changed(self);
        }
        self.schedule_status();
    }

    pub(super) fn reload(self: &Rc<Self>) {
        let file = self.current.borrow().clone();
        self.reload_at(&file, true);
    }

    pub(super) fn go_back(self: &Rc<Self>) {
        let Some(prev) = self.back.borrow_mut().pop() else {
            return;
        };
        let current = self.current.borrow().clone();
        self.forward.borrow_mut().push(current);
        self.navigate(prev, false);
    }

    pub(super) fn go_forward(self: &Rc<Self>) {
        let Some(next) = self.forward.borrow_mut().pop() else {
            return;
        };
        let current = self.current.borrow().clone();
        self.back.borrow_mut().push(current);
        self.navigate(next, false);
    }

    pub(super) fn go_up(self: &Rc<Self>) {
        let parent = parent_of(&self.current.borrow());
        if let Some(parent) = parent {
            self.navigate(parent, true);
        }
    }

    pub(super) fn mount_then(
        self: &Rc<Self>,
        file: gio::File,
        then: impl FnOnce(&Rc<Self>, gio::File) + 'static,
    ) {
        let Some(win) = self.win() else { return };
        let op = win.auth.operation(Some(&file.uri()));
        let tab = self.clone();
        win.show_message(&format!("Connecting to {}…", root_label(&file)));
        glib::spawn_future_local(async move {
            let result = file
                .mount_enclosing_volume_future(gio::MountMountFlags::NONE, Some(&op))
                .await;
            match result {
                Ok(()) => {
                    win.show_message("");
                    then(&tab, file);
                }
                Err(e) if e.matches(gio::IOErrorEnum::AlreadyMounted) => then(&tab, file),
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => win.show_message(""),
                Err(e) => win.show_error(&e.to_string()),
            }
        });
    }

    // ------------------------------------------------------------ opening --

    pub(super) fn item(&self, pos: u32) -> Option<gio::FileInfo> {
        self.selection.item(pos).and_downcast::<gio::FileInfo>()
    }

    pub(super) fn selected(&self) -> Vec<(gio::FileInfo, gio::File)> {
        let set = self.selection.selection();
        let mut out = Vec::new();
        if let Some((iter, first)) = gtk::BitsetIter::init_first(&set) {
            for pos in std::iter::once(first).chain(iter) {
                if let Some(info) = self.item(pos) {
                    if let Some(file) = info_file(info.upcast_ref()) {
                        out.push((info, file));
                    }
                }
            }
        }
        out
    }

    pub(super) fn selected_files(&self) -> Vec<gio::File> {
        self.selected().into_iter().map(|(_, f)| f).collect()
    }

    pub(super) fn activate(self: &Rc<Self>, pos: u32) {
        let targets = if self.selection.is_selected(pos) {
            self.selected()
        } else {
            self.item(pos)
                .and_then(|i| info_file(i.upcast_ref()).map(|f| (i, f)))
                .into_iter()
                .collect()
        };
        // A single folder navigates; files open with their default apps.
        if let [(info, file)] = targets.as_slice() {
            if self.open_location(info, file) {
                return;
            }
        }
        let Some(win) = self.win() else { return };
        for (info, file) in targets {
            if !is_dir(&info) {
                win.launch(&file, None);
            }
        }
    }

    /// Handles folders, network mountables and shortcuts; false for files.
    fn open_location(self: &Rc<Self>, info: &gio::FileInfo, file: &gio::File) -> bool {
        match info.file_type() {
            gio::FileType::Directory => {
                self.navigate(file.clone(), true);
                true
            }
            gio::FileType::Mountable => {
                let Some(win) = self.win() else { return true };
                // Share the login with the server the mountable points at.
                let target = info
                    .attribute_string("standard::target-uri")
                    .map_or_else(|| file.uri().to_string(), |t| t.to_string());
                let op = win.auth.operation(Some(&target));
                let tab = self.clone();
                let file = file.clone();
                glib::spawn_future_local(async move {
                    match file
                        .mount_mountable_future(gio::MountMountFlags::NONE, Some(&op))
                        .await
                    {
                        Ok(target) => tab.navigate(target, true),
                        Err(e) if e.matches(gio::IOErrorEnum::AlreadyMounted) => {
                            if let Some(uri) = info_target(&file).await {
                                tab.navigate(gio::File::for_uri(&uri), true);
                            }
                        }
                        Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => {}
                        Err(e) => win.show_error(&e.to_string()),
                    }
                });
                true
            }
            gio::FileType::Shortcut => {
                if let Some(uri) = info.attribute_string("standard::target-uri") {
                    self.navigate(gio::File::for_uri(&uri), true);
                }
                true
            }
            _ => false,
        }
    }
}

/// A location as typed: a path for local folders, else its URI.
pub(super) fn location_text(file: &gio::File) -> String {
    match file.path() {
        Some(p) if file.is_native() => p.display().to_string(),
        _ => file.uri().to_string(),
    }
}

/// Whether `(x, y)` in `view` is on a file: a list row, or a grid cell
/// showing a tile (headings and fillers have none).
fn on_file(view: &gtk::Widget, x: f64, y: f64) -> bool {
    let mut at = view.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(w) = at {
        if &w == view {
            return false;
        }
        if w.css_name() == "row" || (w.has_css_class("tile") && w.is_visible()) {
            return true;
        }
        at = w.parent();
    }
    false
}

/// What kind of file it is, in words: "Folder", "JPEG image".
pub(super) fn type_text(info: &gio::FileInfo) -> String {
    if is_dir(info) {
        return "Folder".to_owned();
    }
    crate::info::content_type(info)
        .map(|ct| gio::content_type_get_description(&ct).to_string())
        .unwrap_or_default()
}
