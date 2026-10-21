//! Abyss: a minimal GTK4 file browser whose logic is ported from Nautilus,
//! with network shares, Tailscale peers and Syncthing folders built in.

mod archive;
mod archive_cancel_test;
mod auth;
mod chooser;
mod dnd;
mod filename;
mod fm1;
mod info;
mod listing;
mod ops;
mod par;
mod places;
mod prefs;
mod search;
mod section;
mod starred;
mod thumbs;
mod window;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const APP_ID: &str = "io.github.MatthewRafei.Abyss";

fn load_css() {
    // GTK has already failed to start without a display; nothing to style.
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../data/style.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|app| {
        load_css();
        fm1::register(app);
        thumbs::prune_in_background();
    });
    app.connect_activate(|app| window::Win::open(app, None));
    app.connect_open(|app, files, _hint| {
        for file in files {
            window::Win::open(app, Some(file.clone()));
        }
    });
    app.run()
}
