//! Tabs and the split view, vifm style: one or two side-by-side panes,
//! each with its own row of tabs. The toolbar, path bar, filter box and
//! footer act on the active tab (the one last clicked or focused, in the
//! active pane, which carries an accent outline while split).
//!
//! The left pane always exists; the right one shows while the split is on
//! and keeps its tabs while hidden. Tabs, their locations and the split are
//! remembered in `~/.config/abyss/session` and come back when Abyss starts
//! without a location to open.

use super::tab::Tab;
use super::*;

/// A pane: its tab strip (shown when it has several tabs, or while split)
/// over the active tab's page.
pub struct Pane {
    pub(super) root: gtk::Box,
    header: gtk::Box,
    strip: gtk::Box,
    /// The pane's location, at the end of its strip (while split).
    path: gtk::Label,
    stack: gtk::Stack,
    pub(super) tabs: RefCell<Vec<Rc<Tab>>>,
    pub(super) current: Cell<usize>,
}

impl Pane {
    pub(super) fn new() -> (Self, gtk::Button) {
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        strip.add_css_class("tab-strip");
        let strip_scroll = gtk::ScrolledWindow::builder()
            .child(&strip)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(true)
            .build();
        let add = icon_button("list-add-symbolic", "New tab (Ctrl+T)");
        let path = gtk::Label::new(None);
        path.add_css_class("pane-path");
        path.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        path.set_hexpand(true);
        path.set_xalign(1.0);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        header.add_css_class("pane-header");
        header.append(&strip_scroll);
        header.append(&add);
        header.append(&path);
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("pane");
        root.append(&header);
        root.append(&stack);
        let pane = Self {
            root,
            header,
            strip,
            path,
            stack,
            tabs: RefCell::default(),
            current: Cell::new(0),
        };
        (pane, add)
    }

    fn current_tab(&self) -> Option<Rc<Tab>> {
        self.tabs.borrow().get(self.current.get()).cloned()
    }
}

/// Closed tabs remembered for Ctrl+Shift+T.
const CLOSED_KEPT: usize = 20;

fn session_file() -> PathBuf {
    glib::user_config_dir().join("abyss").join("session")
}

impl Win {
    /// The active tab (the toolbar, menus and keys act on it).
    pub(super) fn tab(&self) -> Option<Rc<Tab>> {
        self.panes[self.active_pane.get().min(1)].current_tab()
    }

    /// Every tab, in both panes.
    pub(super) fn all_tabs(&self) -> Vec<Rc<Tab>> {
        self.panes
            .iter()
            .flat_map(|p| p.tabs.borrow().clone())
            .collect()
    }

    /// Which pane holds `tab`, and where in it.
    fn place_of(&self, tab: &Tab) -> Option<(usize, usize)> {
        self.panes.iter().enumerate().find_map(|(p, pane)| {
            pane.tabs
                .borrow()
                .iter()
                .position(|t| std::ptr::eq(Rc::as_ptr(t), tab))
                .map(|i| (p, i))
        })
    }

    pub(super) fn setup_panes(win: &Rc<Self>, adds: [gtk::Button; 2]) {
        for (p, add) in adds.into_iter().enumerate() {
            let w = Rc::downgrade(win);
            add.connect_clicked(move |_| {
                if let Some(win) = w.upgrade() {
                    let here = win.panes[p]
                        .current_tab()
                        .map(|t| t.current.borrow().clone());
                    let file = here.unwrap_or_else(|| gio::File::for_path(glib::home_dir()));
                    win.open_tab(p, file, true);
                }
            });
            // Double-click on the strip's empty space opens a tab too.
            let click = gtk::GestureClick::new();
            let w = Rc::downgrade(win);
            click.connect_released(move |g, n, _, _| {
                if n != 2 {
                    return;
                }
                if let Some(win) = w.upgrade() {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    let here = win.panes[p]
                        .current_tab()
                        .map(|t| t.current.borrow().clone());
                    let file = here.unwrap_or_else(|| gio::File::for_path(glib::home_dir()));
                    win.open_tab(p, file, true);
                }
            });
            win.panes[p].header.add_controller(click);
        }
        // Save the session as the window closes.
        let w = Rc::downgrade(win);
        win.window.connect_close_request(move |_| {
            if let Some(win) = w.upgrade() {
                win.save_session();
            }
            glib::Propagation::Proceed
        });
    }

    /// Opens a tab at `file` in pane `p`, after its current tab; `show`
    /// switches to it, otherwise it waits in the background (and only
    /// reads its folder once shown).
    pub(super) fn open_tab(self: &Rc<Self>, p: usize, file: gio::File, show: bool) -> Rc<Tab> {
        let p = p.min(1);
        let tab = Tab::new(self, file, show);
        let pane = &self.panes[p];
        let at = if pane.tabs.borrow().is_empty() {
            0
        } else {
            pane.current.get() + 1
        };
        // The strip's children follow the tab order.
        let before = pane.tabs.borrow().get(at.saturating_sub(1)).cloned();
        match before.filter(|_| at > 0) {
            Some(prev) => pane.strip.insert_child_after(&tab.chip, Some(&prev.chip)),
            None => pane.strip.prepend(&tab.chip),
        }
        pane.stack.add_child(&tab.root);
        pane.tabs.borrow_mut().insert(at, tab.clone());
        if show || pane.tabs.borrow().len() == 1 {
            pane.current.set(at);
            self.select_tab(&tab);
        } else {
            // Keep the current tab current (its index moved).
            if at <= pane.current.get() {
                pane.current.set(pane.current.get() + 1);
            }
            self.refresh_panes();
            self.schedule_session();
        }
        tab
    }

    /// Opens `file` in a new tab next to `tab`, in the same pane.
    pub(super) fn open_tab_beside(self: &Rc<Self>, tab: &Tab, file: gio::File, show: bool) {
        let p = self
            .place_of(tab)
            .map_or_else(|| self.active_pane.get(), |(p, _)| p);
        self.open_tab(p, file, show);
    }

    /// Makes `tab` the current tab of its pane and the active tab.
    pub(super) fn select_tab(self: &Rc<Self>, tab: &Rc<Tab>) {
        let Some((p, i)) = self.place_of(tab) else {
            return;
        };
        let was = self.tab();
        let was_pane = self.active_pane.get();
        self.panes[p].current.set(i);
        self.panes[p].stack.set_visible_child(&tab.root);
        self.active_pane.set(p);
        self.refresh_panes();
        if let Some(was) = was.filter(|t| !Rc::ptr_eq(t, tab)) {
            // One selection on screen at a time: moving to the other pane
            // clears what was selected in the one left behind (tabs of the
            // same pane are never on screen together, so they keep theirs).
            if was_pane != p {
                was.selection.unselect_all();
            }
            self.active_tab_changed();
        }
        self.schedule_session();
    }

    /// Shows the right tabs and panes and marks the active ones.
    fn refresh_panes(self: &Rc<Self>) {
        let split = self.split.get();
        for (p, pane) in self.panes.iter().enumerate() {
            let visible = p == 0 || split;
            pane.root.set_visible(visible);
            let tabs = pane.tabs.borrow().clone();
            pane.header.set_visible(split || tabs.len() > 1);
            pane.path.set_visible(split);
            if split && p == self.active_pane.get() {
                pane.root.add_css_class("active-pane");
            } else {
                pane.root.remove_css_class("active-pane");
            }
            for (i, tab) in tabs.iter().enumerate() {
                let current = i == pane.current.get();
                tab.set_shown(visible && current);
                if current {
                    tab.chip.add_css_class("current");
                    pane.stack.set_visible_child(&tab.root);
                    pane.path
                        .set_text(&tab::location_text(&tab.current.borrow()));
                } else {
                    tab.chip.remove_css_class("current");
                }
            }
        }
    }

    /// A tab's location changed: its strip entry and, for the active tab,
    /// the toolbar, title and sidebar follow.
    pub(super) fn tab_changed(self: &Rc<Self>, tab: &Tab) {
        if let Some((p, i)) = self.place_of(tab) {
            if self.panes[p].current.get() == i {
                self.panes[p]
                    .path
                    .set_text(&tab::location_text(&tab.current.borrow()));
            }
        }
        if tab.is_active() {
            self.active_tab_changed();
        }
        self.schedule_session();
    }

    /// The toolbar and footer show the active tab.
    fn active_tab_changed(self: &Rc<Self>) {
        let Some(tab) = self.tab() else { return };
        let file = tab.current.borrow().clone();
        self.rebuild_pathbar();
        self.back_btn.set_sensitive(!tab.back.borrow().is_empty());
        self.forward_btn
            .set_sensitive(!tab.forward.borrow().is_empty());
        self.up_btn.set_sensitive(parent_of(&file).is_some());
        let name = if file.parent().is_none() {
            root_label(&file)
        } else {
            ops::display_name(&file)
        };
        self.window.set_title(Some(&format!("{name} — Abyss")));
        self.select_sidebar_row();
        // The filter box shows this tab's filter (its handler sees no change).
        let text = tab.search_text.borrow().clone();
        if self.search.text() != text {
            self.search.set_text(&text);
        }
        self.update_status();
    }

    /// Closes `tab`; the last tab of a pane closes the split, and the last
    /// tab of all closes the window.
    pub(super) fn close_tab(self: &Rc<Self>, tab: &Rc<Tab>) {
        let Some((p, i)) = self.place_of(tab) else {
            return;
        };
        if self.all_tabs().len() == 1 {
            self.window.close();
            return;
        }
        let pane = &self.panes[p];
        pane.tabs.borrow_mut().remove(i);
        pane.strip.remove(&tab.chip);
        pane.stack.remove(&tab.root);
        {
            let mut closed = self.closed.borrow_mut();
            closed.push((p, tab.current.borrow().clone()));
            let extra = closed.len().saturating_sub(CLOSED_KEPT);
            closed.drain(..extra);
        }
        tab.dispose();
        let left = pane.tabs.borrow().len();
        if left == 0 {
            if p == 0 {
                // The right pane's tabs move over to the left.
                let moved = self.panes[1].tabs.take();
                for t in &moved {
                    self.panes[1].strip.remove(&t.chip);
                    self.panes[1].stack.remove(&t.root);
                    self.panes[0].strip.append(&t.chip);
                    self.panes[0].stack.add_child(&t.root);
                }
                self.panes[0].current.set(
                    self.panes[1]
                        .current
                        .get()
                        .min(moved.len().saturating_sub(1)),
                );
                *self.panes[0].tabs.borrow_mut() = moved;
                self.panes[1].current.set(0);
            }
            self.active_pane.set(0);
            self.set_split(false);
        } else if pane.current.get() >= i && pane.current.get() > 0 {
            pane.current.set(pane.current.get() - 1);
        }
        if p == self.active_pane.get() || left == 0 {
            if let Some(next) = self.panes[self.active_pane.get()].current_tab() {
                self.select_tab(&next);
                next.focus_view();
            }
        } else {
            self.refresh_panes();
        }
        self.schedule_session();
    }

    /// Ctrl+Shift+T: opens the last closed tab again, in its pane.
    pub(super) fn reopen_closed_tab(self: &Rc<Self>) {
        let Some((p, file)) = self.closed.borrow_mut().pop() else {
            return;
        };
        let p = if p == 1 && !self.split.get() { 0 } else { p };
        self.open_tab(p, file, true);
    }

    /// Ctrl+PgDown / Ctrl+PgUp (Ctrl+Tab): the next / previous tab of the
    /// active pane, wrapping around.
    pub(super) fn cycle_tab(self: &Rc<Self>, step: isize) {
        let pane = &self.panes[self.active_pane.get()];
        let n = pane.tabs.borrow().len();
        if n < 2 {
            return;
        }
        let i = pane
            .current
            .get()
            .cast_signed()
            .wrapping_add(step)
            .rem_euclid(n.cast_signed())
            .cast_unsigned();
        let next = pane.tabs.borrow().get(i).cloned();
        if let Some(next) = next {
            self.select_tab(&next);
            next.focus_view();
        }
    }

    /// Alt+1…9: the n-th tab of the active pane (Alt+9 the last).
    pub(super) fn goto_tab(self: &Rc<Self>, n: usize) {
        let pane = &self.panes[self.active_pane.get()];
        let tabs = pane.tabs.borrow().clone();
        let tab = if n >= 9 { tabs.last() } else { tabs.get(n - 1) };
        if let Some(tab) = tab {
            self.select_tab(tab);
            tab.focus_view();
        }
    }

    /// Turns the split view on or off. Turning it on shows the right pane
    /// (opening it at the active tab's folder the first time); off hides it
    /// with its tabs kept.
    pub(super) fn set_split(self: &Rc<Self>, on: bool) {
        if self.split_btn.is_active() != on {
            // Its handler calls back here.
            self.split_btn.set_active(on);
            return;
        }
        if self.split.replace(on) == on {
            return;
        }
        if on && self.panes[1].tabs.borrow().is_empty() {
            let here = self.tab().map_or_else(
                || gio::File::for_path(glib::home_dir()),
                |t| t.current.borrow().clone(),
            );
            let tab = Tab::new(self, here, true);
            self.panes[1].strip.append(&tab.chip);
            self.panes[1].stack.add_child(&tab.root);
            self.panes[1].tabs.borrow_mut().push(tab);
            self.panes[1].current.set(0);
        }
        if !on {
            self.active_pane.set(0);
        }
        self.refresh_panes();
        if on {
            self.center_divider();
        }
        self.active_tab_changed();
        self.focus_view();
        self.schedule_session();
    }

    /// Puts the divider half way. Right after the right pane appears, GTK
    /// can briefly report the left one as wanting the whole width and clamp
    /// the position to it, so this keeps setting it for a few frames until
    /// it holds.
    fn center_divider(&self) {
        let (frames, held) = (Cell::new(0), Cell::new(0));
        self.panes_paned.add_tick_callback(move |paned, _| {
            frames.set(frames.get() + 1);
            let width = paned.width();
            if width > 0 {
                let target = width / 2;
                if (paned.position() - target).abs() <= 4 {
                    held.set(held.get() + 1);
                } else {
                    held.set(0);
                    paned.set_position(target);
                }
            }
            if held.get() >= 3 || frames.get() > 60 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    /// Tab (in split view): switches to the other pane.
    pub(super) fn switch_pane(self: &Rc<Self>) {
        if !self.split.get() {
            return;
        }
        let other = 1 - self.active_pane.get().min(1);
        if let Some(tab) = self.panes[other].current_tab() {
            self.select_tab(&tab);
            tab.focus_view();
        }
    }

    /// The other pane's current tab (split view only).
    pub(super) fn other_tab(&self) -> Option<Rc<Tab>> {
        if !self.split.get() {
            return None;
        }
        self.panes[1 - self.active_pane.get().min(1)].current_tab()
    }

    /// F5 / F6: copies or moves the selection into the other pane's folder.
    pub(super) fn to_other_pane(self: &Rc<Self>, kind: Kind) {
        let (Some(tab), Some(other)) = (self.tab(), self.other_tab()) else {
            return;
        };
        let files = tab.selected_files();
        if files.is_empty() {
            self.show_message("Nothing selected");
            return;
        }
        let dest = other.current.borrow().clone();
        self.run_transfer(files, dest, kind);
    }

    /// Ctrl+U: the two panes trade folders.
    pub(super) fn swap_panes(self: &Rc<Self>) {
        let (Some(tab), Some(other)) = (self.tab(), self.other_tab()) else {
            return;
        };
        let (a, b) = (tab.current.borrow().clone(), other.current.borrow().clone());
        tab.navigate(b, true);
        other.navigate(a, true);
    }

    /// Opens `file` in the other pane (turning the split on if needed).
    pub(super) fn open_in_other_pane(self: &Rc<Self>, file: gio::File) {
        if !self.split.get() {
            self.set_split(true);
        }
        if let Some(other) = self.other_tab() {
            other.navigate(file, true);
        }
    }

    // ------------------------------------------------------------ session --

    /// Saves the session soon (changes come in bursts).
    pub(super) fn schedule_session(self: &Rc<Self>) {
        if self.restoring.get() || self.session_pending.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
            if let Some(win) = w.upgrade() {
                win.session_pending.set(false);
                win.save_session();
            }
        });
    }

    fn save_session(&self) {
        let panes: Vec<serde_json::Value> = self
            .panes
            .iter()
            .map(|pane| {
                let tabs: Vec<String> = pane
                    .tabs
                    .borrow()
                    .iter()
                    .map(|t| t.current.borrow().uri().to_string())
                    .collect();
                serde_json::json!({ "current": pane.current.get(), "tabs": tabs })
            })
            .collect();
        let session = serde_json::json!({
            "split": self.split.get(),
            "active": self.active_pane.get(),
            "panes": panes,
        });
        let path = session_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, session.to_string());
    }

    /// Opens the last session's tabs; false if there was none. Only the
    /// current tab of each pane loads now, the rest when first shown.
    pub(super) fn restore_session(self: &Rc<Self>) -> bool {
        let Ok(text) = std::fs::read_to_string(session_file()) else {
            return false;
        };
        let Ok(session) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        let Some(panes) = session["panes"].as_array() else {
            return false;
        };
        self.restoring.set(true);
        for (p, pane) in panes.iter().take(2).enumerate() {
            let uris: Vec<&str> = pane["tabs"]
                .as_array()
                .map(|a| a.iter().filter_map(|u| u.as_str()).collect())
                .unwrap_or_default();
            let current = pane["current"]
                .as_u64()
                .and_then(|c| usize::try_from(c).ok())
                .unwrap_or(0)
                .min(uris.len().saturating_sub(1));
            for (i, uri) in uris.iter().enumerate() {
                let tab = Tab::new(self, gio::File::for_uri(uri), false);
                self.panes[p].strip.append(&tab.chip);
                self.panes[p].stack.add_child(&tab.root);
                self.panes[p].tabs.borrow_mut().push(tab);
                if i == current {
                    self.panes[p].current.set(i);
                }
            }
        }
        self.restoring.set(false);
        if self.panes[0].tabs.borrow().is_empty() {
            // Nothing usable: start over from the right pane's tabs, if any.
            for tab in self.panes[1].tabs.take() {
                self.panes[1].strip.remove(&tab.chip);
                self.panes[1].stack.remove(&tab.root);
                tab.dispose();
            }
            return false;
        }
        let split =
            session["split"].as_bool().unwrap_or(false) && !self.panes[1].tabs.borrow().is_empty();
        let active = session["active"]
            .as_u64()
            .and_then(|a| usize::try_from(a).ok())
            .filter(|a| *a == 0 || split)
            .unwrap_or(0);
        self.split.set(split);
        self.split_btn.set_active(split);
        self.active_pane.set(active);
        if let Some(tab) = self.tab() {
            self.select_tab(&tab);
        }
        if split {
            self.center_divider();
        }
        true
    }
}
