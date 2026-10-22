//! Pick mode: the window as another app's Open or Save dialog, for the
//! `FileChooser` portal (`portal.rs`). It browses as usual; a bar above the
//! footer holds the name to save as, the app's file type filters and the
//! accept button. Opening a file picks it instead of launching it.

use super::*;
use crate::portal::{Chosen, Filter, Kind, Reply, Request};

pub struct Picker {
    kind: Kind,
    title: String,
    filters: Vec<Rc<Filter>>,
    /// The filter shown now (None: everything).
    filter: RefCell<Option<Rc<Filter>>>,
    /// Save: the name to save as.
    name: gtk::Entry,
    accept: gtk::Button,
    accept_label: String,
    /// Save: the existing file the user was asked about replacing.
    replace: RefCell<Option<gio::File>>,
    reply: Reply,
}

impl Win {
    /// A window picking files for another app; `reply` gets the choice, or
    /// None when the window closes without one.
    pub fn pick(app: &gtk::Application, req: Request, reply: Reply) -> gtk::ApplicationWindow {
        let start = req
            .folder
            .clone()
            .unwrap_or_else(|| gio::File::for_path(glib::home_dir()));
        let win = Self::open_window(app, Some(start), "", move |win| {
            Self::start_pick(win, req, reply);
        });
        win.window.clone()
    }

    pub(super) fn picking(&self) -> bool {
        self.picker.borrow().is_some()
    }

    /// The picker's window title, when picking.
    pub(super) fn pick_title(&self) -> Option<String> {
        self.picker.borrow().as_ref().map(|p| p.title.clone())
    }

    /// Sets the window up for `req`, before it's shown.
    // Builds the bar and wires it; splitting it would just scatter it.
    #[allow(clippy::too_many_lines)]
    fn start_pick(win: &Rc<Self>, req: Request, reply: Reply) {
        let default_label = match req.kind {
            Kind::Open {
                directory: true, ..
            } => "_Select",
            Kind::Open { .. } => "_Open",
            Kind::Save { .. } | Kind::SaveFiles { .. } => "_Save",
        };
        let accept_label = req
            .accept_label
            .clone()
            .unwrap_or_else(|| default_label.to_owned());
        let title = if req.title.is_empty() {
            accept_label.replace('_', "")
        } else {
            req.title.clone()
        };

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("prompt");
        bar.add_css_class("pick");
        let name = gtk::Entry::builder().hexpand(true).build();
        match &req.kind {
            Kind::Save { name: start } => {
                bar.append(&gtk::Label::new(Some("Name")));
                name.set_text(start);
                bar.append(&name);
            }
            Kind::SaveFiles { names } => {
                let what = match names.as_slice() {
                    [one] => format!("Saving “{one}” in this folder"),
                    _ => format!("Saving {} files in this folder", names.len()),
                };
                let label = gtk::Label::new(Some(&what));
                label.set_xalign(0.0);
                label.set_hexpand(true);
                label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                bar.append(&label);
            }
            Kind::Open { .. } => {
                let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                spacer.set_hexpand(true);
                bar.append(&spacer);
            }
        }
        let filter = req.filters.get(req.current_filter).cloned();
        if !req.filters.is_empty() {
            let names: Vec<&str> = req.filters.iter().map(|f| f.name.as_str()).collect();
            let types = gtk::DropDown::from_strings(&names);
            types.set_selected(u32::try_from(req.current_filter).unwrap_or(0));
            types.set_tooltip_text(Some("File types shown"));
            let w = Rc::downgrade(win);
            types.connect_selected_notify(move |types| {
                if let Some(win) = w.upgrade() {
                    win.set_pick_filter(types.selected() as usize);
                }
            });
            bar.append(&types);
        }
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_mnemonic(&accept_label);
        accept.add_css_class("suggested-action");
        bar.append(&cancel);
        bar.append(&accept);
        // Above the footer.
        if let Some(footer) = win.status.parent() {
            if let Some(main) = footer.parent().and_downcast::<gtk::Box>() {
                main.insert_child_after(&bar, footer.prev_sibling().as_ref());
            }
        }

        let close_reply = reply.clone();
        win.picker.replace(Some(Rc::new(Picker {
            kind: req.kind.clone(),
            title: title.clone(),
            filters: req.filters,
            filter: RefCell::new(filter),
            name: name.clone(),
            accept: accept.clone(),
            accept_label,
            replace: RefCell::default(),
            reply,
        })));
        win.window.set_title(Some(&title));
        win.window.set_default_size(900, 600);
        // Closing without choosing cancels.
        win.window.connect_close_request(move |_| {
            close_reply(None);
            glib::Propagation::Proceed
        });
        // The asking app's window becomes the parent, so the compositor
        // shows this as its dialog (niri floats it over the app).
        if let Some(handle) = req.parent.strip_prefix("wayland:") {
            let handle = handle.to_owned();
            win.window.connect_realize(move |window| {
                if let Some(top) = window
                    .surface()
                    .and_downcast::<gdk4_wayland::WaylandToplevel>()
                {
                    top.set_transient_for_exported(&handle);
                }
            });
        }

        let w = Rc::downgrade(win);
        cancel.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.window.close();
            }
        });
        let w = Rc::downgrade(win);
        accept.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.pick_accept();
            }
        });
        let w = Rc::downgrade(win);
        name.connect_activate(move |_| {
            if let Some(win) = w.upgrade() {
                win.pick_accept();
            }
        });
        let w = Rc::downgrade(win);
        name.connect_changed(move |_| {
            if let Some(win) = w.upgrade() {
                win.forget_replace();
                win.update_pick();
            }
        });
        // Save starts in the name, its stem selected (as GTK does), once
        // the window has focused its file list.
        if matches!(req.kind, Kind::Save { .. }) {
            let stem = name.text().rfind('.').filter(|&dot| dot > 0).map_or_else(
                || char_offset(&name.text()),
                |dot| char_offset(&name.text()[..dot]),
            );
            glib::idle_add_local_once(move || {
                name.grab_focus();
                name.select_region(0, stem);
            });
        }
        // The file list follows the app's filter.
        for tab in win.all_tabs() {
            tab.filter.changed(gtk::FilterChange::MoreStrict);
        }
        win.update_pick();
    }

    /// Whether the picker's filter lets this file show; folders always do.
    pub(super) fn pick_shows(&self, info: &gio::FileInfo) -> bool {
        let picker = self.picker.borrow();
        let Some(filter) = picker.as_ref().and_then(|p| p.filter.borrow().clone()) else {
            return true;
        };
        is_dir(info)
            || filter.matches(
                &info.display_name(),
                crate::info::content_type(info).as_deref(),
            )
    }

    fn set_pick_filter(&self, index: usize) {
        let Some(picker) = self.picker.borrow().clone() else {
            return;
        };
        picker.filter.replace(picker.filters.get(index).cloned());
        for tab in self.all_tabs() {
            tab.filter.changed(gtk::FilterChange::Different);
            tab.schedule_status();
        }
    }

    /// Whether the accept button can act, after the selection, folder or
    /// name changed.
    pub(super) fn update_pick(&self) {
        let Some(picker) = self.picker.borrow().clone() else {
            return;
        };
        let here = self.tab().map(|t| t.current.borrow().clone());
        let ready = match &picker.kind {
            Kind::Open {
                directory: false, ..
            } => self.tab().is_some_and(|t| !t.selected().is_empty()),
            Kind::Open { .. } | Kind::SaveFiles { .. } => true,
            Kind::Save { .. } => {
                // A replace asked about elsewhere no longer applies.
                let asked = picker.replace.borrow().clone();
                if asked
                    .is_some_and(|f| here.is_none_or(|h| f.parent().is_none_or(|p| !p.equal(&h))))
                {
                    self.forget_replace();
                }
                !picker.name.text().trim().is_empty()
            }
        };
        picker.accept.set_sensitive(ready);
    }

    /// Back from "Replace" to the plain accept button.
    fn forget_replace(&self) {
        let Some(picker) = self.picker.borrow().clone() else {
            return;
        };
        if picker.replace.take().is_some() {
            picker.accept.set_label(&picker.accept_label);
            picker.accept.remove_css_class("destructive-action");
            picker.accept.add_css_class("suggested-action");
        }
    }

    /// Files opened (double-click, Enter) while picking: folders were
    /// entered already; files are picked.
    pub(super) fn pick_activate(self: &Rc<Self>, targets: Vec<(gio::FileInfo, gio::File)>) {
        let Some(picker) = self.picker.borrow().clone() else {
            return;
        };
        let files: Vec<(gio::FileInfo, gio::File)> =
            targets.into_iter().filter(|(i, _)| !is_dir(i)).collect();
        match &picker.kind {
            Kind::Open {
                multiple,
                directory: false,
            } => {
                let take = if *multiple { files.len() } else { 1 };
                let files: Vec<gio::File> = files.into_iter().take(take).map(|(_, f)| f).collect();
                if !files.is_empty() {
                    self.pick_done(files);
                }
            }
            Kind::Save { .. } => {
                if let [(info, _)] = files.as_slice() {
                    picker.name.set_text(&info.display_name());
                    self.pick_accept();
                }
            }
            Kind::Open { .. } | Kind::SaveFiles { .. } => {}
        }
    }

    /// The accept button: pick what's selected, or the name typed.
    fn pick_accept(self: &Rc<Self>) {
        let (Some(picker), Some(tab)) = (self.picker.borrow().clone(), self.tab()) else {
            return;
        };
        let here = tab.current.borrow().clone();
        match &picker.kind {
            Kind::Open {
                multiple,
                directory,
            } => {
                let selected = tab.selected();
                if *directory {
                    let chosen = match selected.as_slice() {
                        [(info, folder)] if is_dir(info) => folder.clone(),
                        _ => here,
                    };
                    self.pick_done(vec![chosen]);
                    return;
                }
                // A folder alone is entered, as a double-click would.
                if let [(info, folder)] = selected.as_slice() {
                    if is_dir(info) {
                        tab.navigate(folder.clone(), true);
                        return;
                    }
                }
                let take = if *multiple { usize::MAX } else { 1 };
                let files: Vec<gio::File> = selected
                    .into_iter()
                    .filter(|(i, _)| !is_dir(i))
                    .take(take)
                    .map(|(_, f)| f)
                    .collect();
                if !files.is_empty() {
                    self.pick_done(files);
                }
            }
            Kind::SaveFiles { names } => {
                let files = names.iter().map(|n| here.child(n)).collect();
                self.pick_done(files);
            }
            Kind::Save { .. } => {
                let name = picker.name.text().trim().to_owned();
                if name.is_empty() {
                    return;
                }
                // "sub/x.png" and absolute paths work too.
                let target = here.resolve_relative_path(&name);
                let win = self.clone();
                glib::spawn_future_local(async move {
                    let found = target
                        .query_info_future(
                            "standard::type,standard::display-name",
                            gio::FileQueryInfoFlags::NONE,
                            glib::Priority::DEFAULT,
                        )
                        .await
                        .ok();
                    match found {
                        // A folder's name: go into it.
                        Some(info) if is_dir(&info) => {
                            if let Some(tab) = win.tab() {
                                tab.navigate(target, true);
                            }
                            picker.name.set_text("");
                        }
                        // Ask once before replacing a file.
                        Some(info)
                            if !picker
                                .replace
                                .borrow()
                                .as_ref()
                                .is_some_and(|f| f.equal(&target)) =>
                        {
                            picker.replace.replace(Some(target));
                            picker.accept.set_label("_Replace");
                            picker.accept.remove_css_class("suggested-action");
                            picker.accept.add_css_class("destructive-action");
                            win.show_message(&format!(
                                "“{}” already exists; Replace overwrites it",
                                info.display_name()
                            ));
                        }
                        _ => win.pick_done(vec![target]),
                    }
                });
            }
        }
    }

    /// Hands `files` to the asking app and closes.
    fn pick_done(&self, files: Vec<gio::File>) {
        let Some(picker) = self.picker.borrow().clone() else {
            return;
        };
        let filter = picker.filter.borrow().clone();
        (picker.reply)(Some(Chosen { files, filter }));
        self.window.close();
    }
}
