//! The error bar: slides up from the bottom of the window (like the
//! sign-in bar) when something fails, so a failure isn't just small text in
//! the footer. It goes by itself after a while, but not while the pointer is
//! on it (so it can be read, or its text selected and copied).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;

/// How long an error stays up.
const SHOWN: Duration = Duration::from_secs(12);
/// And after the pointer leaves it.
const AFTER_HOVER: Duration = Duration::from_secs(4);

pub struct ErrorBar {
    revealer: gtk::Revealer,
    label: gtk::Label,
    /// Bumped by each error, so an old timer can't hide a newer one.
    seq: Cell<u32>,
    hovered: Cell<bool>,
}

impl ErrorBar {
    pub fn new() -> Rc<Self> {
        let icon = gtk::Image::from_icon_name("dialog-error-symbolic");
        icon.add_css_class("error-icon");
        icon.set_valign(gtk::Align::Start);
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_selectable(true);
        label.set_hexpand(true);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("error-close");
        close.set_tooltip_text(Some("Dismiss"));
        close.set_valign(gtk::Align::Start);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        body.add_css_class("prompt");
        body.add_css_class("error-bar");
        body.append(&icon);
        body.append(&label);
        body.append(&close);
        let revealer = gtk::Revealer::builder()
            .child(&body)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();
        let bar = Rc::new(Self {
            revealer,
            label,
            seq: Cell::new(0),
            hovered: Cell::new(false),
        });
        let me = Rc::downgrade(&bar);
        close.connect_clicked(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.hide();
            }
        });
        let hover = gtk::EventControllerMotion::new();
        let me = Rc::downgrade(&bar);
        hover.connect_enter(move |_, _, _| {
            if let Some(bar) = me.upgrade() {
                bar.hovered.set(true);
            }
        });
        let me = Rc::downgrade(&bar);
        hover.connect_leave(move |_| {
            if let Some(bar) = me.upgrade() {
                bar.hovered.set(false);
                bar.hide_after(AFTER_HOVER);
            }
        });
        body.add_controller(hover);
        bar
    }

    pub const fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    /// Shows `text`, replacing any error already up.
    pub fn show(self: &Rc<Self>, text: &str) {
        self.label.set_text(text);
        self.revealer.set_reveal_child(true);
        self.hide_after(SHOWN);
    }

    pub fn hide(&self) {
        self.seq.set(self.seq.get().wrapping_add(1));
        self.revealer.set_reveal_child(false);
    }

    /// Hides the bar in `delay`, unless another error comes first or the
    /// pointer is on it then (leaving starts a new, shorter wait).
    fn hide_after(self: &Rc<Self>, delay: Duration) {
        let seq = self.seq.get().wrapping_add(1);
        self.seq.set(seq);
        let me = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(bar) = me.upgrade() {
                if bar.seq.get() == seq && !bar.hovered.get() {
                    bar.hide();
                }
            }
        });
    }
}
