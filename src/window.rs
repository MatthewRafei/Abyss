//! The browser window: sidebar, path bar, file list, prompt bar and footer.
//! Behaviour borrowed from nautilus-window-slot.c (history, mounting on
//! demand), nautilus-pathbar.c and nautilus-files-view.c (menus, clipboard).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive;
use crate::auth;
use crate::chooser;
use crate::dnd;
use crate::ops::{self, Kind};
use crate::places::{self, Pin, Place, Section, Target};
use crate::search;
use crate::thumbs;

mod views;
use views::Mode;

type PromptAction = Box<dyn Fn(&Rc<Win>, String)>;

pub struct Win {
    window: gtk::ApplicationWindow,
    sidebar: gtk::ListBox,
    places: RefCell<Vec<Place>>,
    peers: RefCell<Vec<Place>>,
    peers_checked: Cell<Option<std::time::Instant>>,
    monitor: gio::VolumeMonitor,

    path_stack: gtk::Stack,
    pathbar: gtk::Box,
    location: gtk::Entry,
    search: gtk::SearchEntry,
    back_btn: gtk::Button,
    forward_btn: gtk::Button,
    up_btn: gtk::Button,

    view: gtk::ColumnView,
    /// Pages "list", "grid" and "groups" (grouped grid); carries the zoom class.
    views: gtk::Stack,
    grid: gtk::GridView,
    /// The grouped grid: one grid over `grouped_model`'s layout.
    groups: gtk::GridView,
    grouped_model: crate::section::GroupedSelection,
    /// The running column check of the grouped grid, if any.
    group_tick: RefCell<Option<gtk::TickCallbackId>>,
    list_btn: gtk::ToggleButton,
    grid_btn: gtk::ToggleButton,
    group_btn: gtk::ToggleButton,
    mode: Cell<Mode>,
    /// Shared with the group sorter, which only sorts while grouped.
    grouped: Rc<Cell<bool>>,
    group_sorter: gtk::NumericSorter,
    folders_first: gtk::NumericSorter,
    toolbar_row1: gtk::Box,
    toolbar_row2: gtk::Box,
    controls: gtk::Box,
    compact: Cell<bool>,
    size_probe: gtk::DrawingArea,
    /// The shown folder's listing; `filtered` reads it, or search results.
    list: RefCell<gtk::DirectoryList>,
    /// Listings of folders visited lately, most recent first, kept loaded
    /// (and watched) so going back to one is instant.
    recent: RefCell<VecDeque<gtk::DirectoryList>>,
    filtered: gtk::FilterListModel,
    filter: gtk::CustomFilter,
    /// The search term, lowercased once per keystroke.
    needle: RefCell<String>,
    /// Shown files waiting for their full type, and whether a lookup runs.
    untyped: RefCell<Vec<gio::FileInfo>>,
    typing: Cell<bool>,
    /// The recursive search under the current folder, if one is running.
    search_task: RefCell<Option<glib::JoinHandle<()>>>,
    search_state: Cell<SearchState>,
    sorted: gtk::SortListModel,
    selection: gtk::MultiSelection,
    empty: gtk::Label,

    prompt: gtk::Revealer,
    prompt_label: gtk::Label,
    prompt_entry: gtk::Entry,
    prompt_ok: gtk::Button,
    prompt_action: RefCell<Option<PromptAction>>,

    job: gtk::Revealer,
    job_label: gtk::Label,
    /// "1.2 GB / 4.0 GB · 30%" next to the bar.
    job_amount: gtk::Label,
    job_bar: gtk::ProgressBar,
    /// The running job, so Cancel can stop it.
    job_handle: RefCell<Option<glib::JoinHandle<()>>>,

    conflict: gtk::Revealer,
    conflict_title: gtk::Label,
    conflict_detail: gtk::Label,
    conflict_entry: gtk::Entry,
    conflict_all: gtk::CheckButton,
    conflict_replace: gtk::Button,
    conflict_rename: gtk::Button,
    conflict_reply: RefCell<Option<futures_channel::oneshot::Sender<(ops::Choice, bool)>>>,
    /// Button to focus once the conflict bar has slid in.
    conflict_focus: RefCell<Option<gtk::Widget>>,
    /// Sign-in bar for network mounts; also remembers logins per server.
    auth: Rc<auth::AuthBar>,
    busy: Cell<bool>,

    status: gtk::Label,
    status_pending: Cell<bool>,
    message: gtk::Label,
    message_seq: Rc<Cell<u32>>,

    current: RefCell<gio::File>,
    back: RefCell<Vec<gio::File>>,
    forward: RefCell<Vec<gio::File>>,
    show_hidden: Cell<bool>,
    /// Index into `ZOOM_LEVELS` for the file list's icon size.
    zoom: Cell<usize>,
    /// Folder-under-drag timer (open after `HOVER_TIMEOUT_MS`).
    hover_timer: RefCell<Option<glib::SourceId>>,
    hover_start: Cell<(f64, f64)>,
    /// Sidebar row between the bookmarks and Trash, shown only while
    /// files are dragged over the sidebar.
    bookmark_drop: gtk::ListBoxRow,
    bookmark_reveal: gtk::Revealer,
    /// Bumped on every sidebar drag enter/leave, so a slow folder check
    /// from an earlier drag can't reveal the strip.
    sidebar_drag: Cell<u32>,
    /// Files put on the clipboard by this process, and whether it was a cut.
    clip: RefCell<Option<(Vec<gio::File>, bool)>>,
}

fn info_file(obj: &glib::Object) -> Option<gio::File> {
    obj.downcast_ref::<gio::FileInfo>()?
        .attribute_object("standard::file")?
        .downcast::<gio::File>()
        .ok()
}

/// A menu entry's action.
type WinAction = Box<dyn Fn(&Rc<Win>)>;

/// Where the recursive search under the current folder stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchState {
    /// Not searching subfolders; the list is the folder's own.
    Off,
    Running,
    Done,
}

/// Recent folder listings kept loaded, and the most files they may hold
/// together (a listing costs roughly 1 KB per file).
const RECENT_FOLDERS: usize = 8;
const RECENT_FILES: u32 = 25_000;

/// A recursive search starts from this many characters.
const SEARCH_MIN_CHARS: usize = 2;

fn new_list() -> gtk::DirectoryList {
    // Attributes are set per folder (`info::attributes`) before listing.
    let list = gtk::DirectoryList::new(None, None::<&gio::File>);
    list.set_io_priority(glib::Priority::LOW);
    list
}

/// Whether a folder's listing stays correct while kept in memory: local
/// folders, whose changes inotify reports. Remote ones, and gvfs's FUSE
/// mirror of them, get no change events, so they're always read afresh.
fn keeps_fresh(file: &gio::File) -> bool {
    let Some(path) = file.path().filter(|_| file.is_native()) else {
        return false;
    };
    !path.starts_with(glib::user_runtime_dir().join("gvfs"))
}

/// An expression for GTK's keyed sorters, which evaluate it once per file
/// and sort on the stored results instead of redoing work per comparison.
fn info_key<R>(f: impl Fn(&gio::FileInfo) -> R + 'static) -> gtk::ClosureExpression
where
    R: ValueType + Default,
{
    gtk::ClosureExpression::with_callback(std::iter::empty::<gtk::Expression>(), move |values| {
        values
            .first()
            .and_then(|v| v.get::<glib::Object>().ok())
            .and_downcast::<gio::FileInfo>()
            .map_or_else(R::default, |info| f(&info))
    })
}

/// Name shown for a search hit: the name, then where it is under the
/// folder searched (Pango markup), or None for the folder's own files.
fn hit_markup(info: &gio::FileInfo) -> Option<String> {
    const WHERE: &str = "abyss::where";
    if !info.has_attribute(WHERE) {
        return None;
    }
    let place = info.attribute_string(WHERE)?;
    Some(format!(
        "{}  <span alpha=\"55%\">{}</span>",
        glib::markup_escape_text(&info.display_name()),
        glib::markup_escape_text(&place)
    ))
}

/// File infos for search hits under `dir`, read a few at a time, each
/// tagged with its file (as `DirectoryList` does) and with where it is
/// under `dir` when that's a subfolder.
async fn hit_infos(dir: &gio::File, paths: Vec<PathBuf>) -> Vec<gio::FileInfo> {
    use futures_util::StreamExt;
    let base = dir.path();
    let attrs = crate::info::attributes(dir);
    futures_util::stream::iter(paths)
        .map(|path| {
            let place = base
                .as_ref()
                .and_then(|b| path.parent()?.strip_prefix(b).ok())
                .map(|p| p.display().to_string())
                .filter(|p| !p.is_empty());
            let file = gio::File::for_path(&path);
            async move {
                let info = file
                    .query_info_future(attrs, gio::FileQueryInfoFlags::NONE, glib::Priority::LOW)
                    .await
                    .ok()?;
                info.set_attribute_object("standard::file", &file);
                if let Some(place) = place {
                    info.set_attribute_string("abyss::where", &place);
                }
                Some(info)
            }
        })
        .buffer_unordered(crate::par::in_flight())
        .filter_map(std::future::ready)
        .collect()
        .await
}

/// Pointer positions are small pixel values, so narrowing them is exact
/// enough for placing popovers and comparing drag distances.
#[allow(clippy::cast_possible_truncation)]
fn point(x: f64, y: f64) -> gtk::graphene::Point {
    gtk::graphene::Point::new(x as f32, y as f32)
}

#[allow(clippy::cast_possible_truncation)]
const fn px(v: f64) -> i32 {
    v as i32
}

/// `(x, y)` in `from`'s coordinates, translated into `to`'s.
fn translate(
    from: &impl IsA<gtk::Widget>,
    to: &impl IsA<gtk::Widget>,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let p = point(x, y);
    let p = from.compute_point(to, &p).unwrap_or(p);
    (f64::from(p.x()), f64::from(p.y()))
}

/// A 1×1 rectangle for `Popover::set_pointing_to`.
fn spot(x: f64, y: f64) -> gdk::Rectangle {
    gdk::Rectangle::new(px(x), px(y), 1, 1)
}

/// "1.2 GB / 4.0 GB · 30%", or "30%" without a size. The percentage
/// rounds down, so it shows 100% only when everything is done.
// Display only: a percentage from a 0..=1 fraction.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn progress_text(done: u64, total: u64, fraction: f64) -> String {
    let percent = (fraction.clamp(0.0, 1.0) * 100.0).floor() as u8;
    if total == 0 {
        format!("{percent}%")
    } else {
        format!(
            "{} / {} · {percent}%",
            glib::format_size(done.min(total)),
            glib::format_size(total)
        )
    }
}

/// Character count as a GTK text offset.
fn char_offset(text: &str) -> i32 {
    i32::try_from(text.chars().count()).unwrap_or(i32::MAX)
}

thread_local! {
    /// Every file row's and tile's icon, to refresh one when its file's
    /// type turns out different (see `Win::refresh_rows`).
    static ICONS: RefCell<Vec<glib::WeakRef<gtk::Image>>> = RefCell::default();
}

/// Registers a row's or tile's icon (once, when it's made).
fn track_icon(image: &gtk::Image) {
    ICONS.with_borrow_mut(|icons| {
        icons.retain(|w| w.upgrade().is_some());
        icons.push(image.downgrade());
    });
}

/// The row and tile icons that still exist; each is tagged with its file's
/// URI (see `show_thumbnail`).
fn shown_icons() -> Vec<gtk::Image> {
    ICONS.with_borrow(|icons| icons.iter().filter_map(glib::WeakRef::upgrade).collect())
}

/// The thumbnail scaled to fit a `width` × `height` slot, as a paintable
/// of exactly that display size. The GPU scales the full texture when
/// drawing, so it stays sharp on any display scale; a plain texture would
/// ask for its own pixel size and stretch the tile.
fn fitted(texture: &gdk::Texture, width: i32, height: i32) -> gdk::Paintable {
    let (w, h) = (
        f64::from(texture.width().max(1)),
        f64::from(texture.height().max(1)),
    );
    let scale = (f64::from(width.max(1)) / w).min(f64::from(height.max(1)) / h);
    // Slot sizes are a few hundred pixels at most.
    #[allow(clippy::cast_possible_truncation)]
    let (fw, fh) = ((w * scale).round() as f32, (h * scale).round() as f32);
    let snapshot = gtk::Snapshot::new();
    snapshot.append_texture(texture, &gtk::graphene::Rect::new(0.0, 0.0, fw, fh));
    snapshot
        .to_paintable(Some(&gtk::graphene::Size::new(fw, fh)))
        .unwrap_or_else(|| texture.clone().upcast())
}

/// Shows the file's thumbnail once it's available, generating it if
/// needed: in `picture` when given (grid tiles, where it fills the wide
/// slot that is the picture's parent), else in place of `image`'s icon.
/// Rows are recycled while scrolling, so the image is tagged with the
/// file it shows and stale results are dropped.
fn show_thumbnail(image: &gtk::Image, picture: Option<&gtk::Picture>, info: &gio::FileInfo) {
    thread_local! {
        /// The thumbnail each image is waiting for, by image: a recycled
        /// row drops its old wait, which lets the queue skip or stop that
        /// render. The number tells a later wait from an earlier one.
        static WAITING: RefCell<HashMap<usize, (u64, glib::JoinHandle<()>)>> =
            RefCell::default();
        static NEXT: Cell<u64> = const { Cell::new(0) };
    }
    let slot = image.as_ptr() as usize;
    if let Some((_, old)) = WAITING.with_borrow_mut(|w| w.remove(&slot)) {
        old.abort();
    }
    // A recycled tile starts with the icon until its own thumbnail is in.
    if let Some(picture) = picture {
        picture.set_paintable(None::<&gdk::Paintable>);
        picture.set_visible(false);
        image.set_visible(true);
    }
    // Decode at the size it's shown (device pixels), not the cache's 256+.
    let shown = picture.map_or_else(
        || image.measure(gtk::Orientation::Horizontal, -1).1,
        |p| {
            p.parent().map_or(128, |slot| {
                let (w, h) = slot.size_request();
                w.max(h)
            })
        },
    );
    let px = shown.max(16) * image.scale_factor();
    let picture = picture.map(ObjectExt::downgrade);
    let Some(file) = info_file(info.upcast_ref()) else {
        image.set_widget_name("");
        return;
    };
    let uri = file.uri().to_string();
    image.set_widget_name(&uri);
    let image = image.downgrade();
    let info = info.clone();
    let id = NEXT.replace(NEXT.get() + 1);
    let handle = glib::spawn_future_local(async move {
        let found = thumbs::thumbnail(&file, &info).await;
        WAITING.with_borrow_mut(|w| {
            if w.get(&slot).is_some_and(|(n, _)| *n == id) {
                w.remove(&slot);
            }
        });
        let Some(path) = found else {
            return;
        };
        let Some(texture) = thumbs::texture(path, px).await else {
            return;
        };
        let Some(image) = image.upgrade().filter(|i| i.widget_name() == uri) else {
            return;
        };
        match picture.and_then(|p| p.upgrade()) {
            Some(picture) => {
                // The slot around it is sized for the zoom level.
                let (w, h) = picture.parent().map_or((96, 96), |s| s.size_request());
                picture.set_paintable(Some(&fitted(&texture, w, h)));
                picture.set_visible(true);
                image.set_visible(false);
            }
            None => image.set_paintable(Some(&texture)),
        }
    });
    WAITING.with_borrow_mut(|w| w.insert(slot, (id, handle)));
}

/// File list icon sizes (Nautilus-style zoom); thumbnails show at all of
/// them. The default is the second, big enough to see a thumbnail.
const ZOOM_LEVELS: [u32; 6] = [20, 32, 48, 64, 96, 128];
const ZOOM_DEFAULT: usize = 1;

fn zoom_file() -> PathBuf {
    glib::user_config_dir().join("abyss").join("zoom")
}

fn load_zoom() -> usize {
    std::fs::read_to_string(zoom_file())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .and_then(|px| ZOOM_LEVELS.iter().position(|z| *z == px))
        .unwrap_or(ZOOM_DEFAULT)
}

/// Hidden or backup file. Some backends (e.g. an SMB share list) leave
/// these attributes out, and GIO logs a critical if they're read anyway.
fn is_hidden(info: &gio::FileInfo) -> bool {
    let flag = |attr| info.has_attribute(attr) && info.boolean(attr);
    flag("standard::is-hidden") || flag("standard::is-backup")
}

fn is_dir(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Directory
}

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("flat");
    b
}

fn format_time(info: &gio::FileInfo) -> String {
    let Some(dt) = info.modification_date_time() else {
        return String::new();
    };
    let Ok(dt) = dt.to_local() else {
        return String::new();
    };
    let now = glib::DateTime::now_local().ok();
    let fmt = match now {
        Some(n) if n.ymd() == dt.ymd() => "%H:%M",
        Some(n) if n.year() == dt.year() => "%e %b",
        _ => "%e %b %Y",
    };
    dt.format(fmt)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// "~" for home, the host for remote roots, nice names for special schemes.
/// The folder above `file`. GVfs gives an SMB share's top level no parent;
/// Abyss treats the server's share list (`smb://host/`) as its parent.
fn parent_of(file: &gio::File) -> Option<gio::File> {
    file.parent().or_else(|| {
        if !file.has_uri_scheme("smb") {
            return None;
        }
        let uri = file.uri();
        let server = auth::server_of(&uri)?;
        (uri.trim_end_matches('/') != server).then(|| gio::File::for_uri(&format!("{server}/")))
    })
}

/// Last segment of a location's address, unescaped ("batocera" for
/// `smb://truenas/batocera/`).
fn share_name(file: &gio::File) -> String {
    let uri = file.uri();
    let leaf = uri
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    glib::Uri::unescape_string(leaf, None::<&str>)
        .map_or_else(|| leaf.to_owned(), |s| s.to_string())
}

/// Name for a location with no parent: the mount's own name for a share
/// ("batocera on truenas"), the host for a server, or the scheme's place.
fn root_label(file: &gio::File) -> String {
    let uri = file.uri();
    let mounted = gio::VolumeMonitor::get()
        .mounts()
        .into_iter()
        .find(|m| m.root().equal(file))
        .map(|m| m.name().to_string());
    if let (Some(name), false) = (mounted, file.is_native()) {
        return name;
    }
    match file.uri_scheme().as_deref() {
        Some("trash") => "Trash".into(),
        Some("network") => "Network".into(),
        Some("recent") => "Recent".into(),
        Some("file") => "Filesystem".into(),
        _ => glib::Uri::parse(&uri, glib::UriFlags::NONE)
            .ok()
            .and_then(|u| u.host().map(|h| h.to_string()))
            .unwrap_or_else(|| uri.to_string()),
    }
}

fn terminal() -> Option<String> {
    std::env::var("TERMINAL")
        .ok()
        .filter(|t| places::have_command(t))
        .or_else(|| {
            ["alacritty", "foot", "kitty", "wezterm", "gnome-terminal"]
                .into_iter()
                .find(|t| places::have_command(t))
                .map(str::to_owned)
        })
}

impl Win {
    pub fn open(app: &gtk::Application, start: Option<gio::File>) {
        let win = Self::build(app);
        Self::wire(&win);
        let start = start.unwrap_or_else(|| gio::File::for_path(glib::home_dir()));
        win.navigate(start, false);
        win.refresh_sidebar();
        win.refresh_peers();
        // Callbacks only hold weak refs; the window owns the state.
        let keep = RefCell::new(Some(win.clone()));
        win.window.connect_destroy(move |_| {
            keep.take();
        });
        win.window.present();
    }

    // Builds the whole widget tree; splitting it would just scatter it.
    #[allow(clippy::too_many_lines)]
    fn build(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Abyss")
            .default_width(1040)
            .default_height(660)
            .build();
        window.add_css_class("abyss");

        // Header: navigation, path bar / location entry, search.
        let back_btn = icon_button("go-previous-symbolic", "Back (Alt+Left)");
        let forward_btn = icon_button("go-next-symbolic", "Forward (Alt+Right)");
        let up_btn = icon_button("go-up-symbolic", "Up (Alt+Up)");
        let pathbar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        pathbar.add_css_class("pathbar");
        let path_scroll = gtk::ScrolledWindow::builder()
            .child(&pathbar)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let location = gtk::Entry::builder()
            .placeholder_text("Path or smb:// sftp:// ftp:// dav:// address")
            .hexpand(true)
            .build();
        let path_stack = gtk::Stack::new();
        path_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        path_stack.add_named(&path_scroll, Some("bar"));
        path_stack.add_named(&location, Some("entry"));
        path_stack.set_hexpand(true);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter")
            .width_chars(16)
            .build();

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        header.append(&back_btn);
        header.append(&forward_btn);
        header.append(&up_btn);
        header.append(&path_stack);
        let list_btn = gtk::ToggleButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text("List (Ctrl+1)")
            .build();
        let grid_btn = gtk::ToggleButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Grid (Ctrl+2)")
            .build();
        grid_btn.set_group(Some(&list_btn));
        let switch = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        switch.add_css_class("linked");
        switch.add_css_class("view-switch");
        switch.append(&list_btn);
        switch.append(&grid_btn);
        let group_btn = gtk::ToggleButton::builder()
            .icon_name("view-paged-symbolic")
            .tooltip_text("Group by type")
            .build();
        group_btn.add_css_class("view-switch");
        // View controls and filter: on the path's row, or a row of their own
        // under it when the window is narrow (see update_toolbar_layout).
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        controls.append(&switch);
        controls.append(&group_btn);
        controls.append(&search);
        header.append(&controls);
        let toolbar_row2 = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        toolbar_row2.set_visible(false);
        let toolbar = gtk::Box::new(gtk::Orientation::Vertical, 6);
        toolbar.add_css_class("toolbar");
        toolbar.append(&header);
        toolbar.append(&toolbar_row2);

        // Sidebar.
        let sidebar = gtk::ListBox::new();
        sidebar.add_css_class("sidebar");
        sidebar.set_selection_mode(gtk::SelectionMode::Single);
        let bookmark_label = gtk::Label::new(Some("Drop folders here to bookmark"));
        bookmark_label.add_css_class("bookmark-drop");
        let bookmark_reveal = gtk::Revealer::builder()
            .child(&bookmark_label)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(220)
            .build();
        let bookmark_drop = gtk::ListBoxRow::builder()
            .child(&bookmark_reveal)
            .activatable(false)
            .selectable(false)
            .name(Section::Places.title())
            .build();
        bookmark_drop.add_css_class("bookmark-drop-row");
        let side_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        side_box.append(&sidebar);
        // The octopus, tinted by CSS, at the bottom of the sidebar's empty
        // space; after the last row when the sidebar scrolls. Only shown
        // when install.sh has installed the symbolic icon.
        let logo_name = "io.github.MatthewRafei.Abyss-symbolic";
        let logo = gtk::Image::from_icon_name(logo_name);
        logo.set_pixel_size(150);
        logo.add_css_class("sidebar-logo");
        logo.set_vexpand(true);
        logo.set_valign(gtk::Align::End);
        logo.set_visible(
            gdk::Display::default()
                .is_some_and(|d| gtk::IconTheme::for_display(&d).has_icon(logo_name)),
        );
        side_box.append(&logo);
        let side_scroll = gtk::ScrolledWindow::builder()
            .child(&side_box)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(210)
            .build();

        // File list: DirectoryList -> filter -> sort -> selection.
        let list = new_list();
        let filter = gtk::CustomFilter::new(|_| true);
        let filtered = gtk::FilterListModel::new(Some(list.clone()), Some(filter.clone()));
        let sorted = gtk::SortListModel::new(Some(filtered.clone()), None::<gtk::Sorter>);
        let selection = gtk::MultiSelection::new(Some(sorted.clone()));
        let view = gtk::ColumnView::new(Some(selection.clone()));
        view.add_css_class("files");
        view.set_show_column_separators(false);
        let empty = gtk::Label::new(Some("Nothing here"));
        empty.add_css_class("empty");
        empty.set_visible(false);
        let view_scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();
        let grid = gtk::GridView::new(Some(selection.clone()), None::<gtk::ListItemFactory>);
        grid.add_css_class("files");
        grid.set_max_columns(24);
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
        let groups = gtk::GridView::new(Some(grouped_model.clone()), None::<gtk::ListItemFactory>);
        groups.add_css_class("files");
        groups.add_css_class("groups");
        let groups_scroll = gtk::ScrolledWindow::builder()
            .child(&groups)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let views = gtk::Stack::new();
        views.add_css_class("views");
        views.add_css_class(&format!("zoom-{}", ZOOM_LEVELS[load_zoom()]));
        views.add_named(&view_scroll, Some("list"));
        views.add_named(&grid_scroll, Some("grid"));
        views.add_named(&groups_scroll, Some("groups"));
        let (start_mode, start_grouped) = views::load_view();
        let grouped = Rc::new(Cell::new(start_grouped));
        let g = grouped.clone();
        let group_sorter = gtk::NumericSorter::new(Some(info_key(move |info| {
            if g.get() {
                u32::from(views::category(info).0)
            } else {
                0
            }
        })));
        let folders_first =
            gtk::NumericSorter::new(Some(info_key(|info| u32::from(!is_dir(info)))));
        let view_overlay = gtk::Overlay::new();
        view_overlay.set_child(Some(&views));
        view_overlay.add_overlay(&empty);
        // An invisible, click-through layer over the file area: GTK reports
        // its size on every allocation, which drives the toolbar layout.
        let size_probe = gtk::DrawingArea::new();
        size_probe.set_can_target(false);
        size_probe.set_can_focus(false);
        view_overlay.add_overlay(&size_probe);
        empty.set_can_target(false);

        // Prompt bar (rename, new folder, confirmations).
        let prompt_label = gtk::Label::new(None);
        let prompt_entry = gtk::Entry::builder().hexpand(true).build();
        let prompt_ok = gtk::Button::with_label("OK");
        prompt_ok.add_css_class("suggested-action");
        let prompt_cancel = gtk::Button::with_label("Cancel");
        let prompt_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        prompt_box.add_css_class("prompt");
        prompt_box.append(&prompt_label);
        prompt_box.append(&prompt_entry);
        prompt_box.append(&prompt_cancel);
        prompt_box.append(&prompt_ok);
        let prompt = gtk::Revealer::builder()
            .child(&prompt_box)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        // Job bar.
        let job_label = gtk::Label::new(None);
        job_label.set_xalign(0.0);
        job_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        job_label.set_hexpand(true);
        let job_amount = gtk::Label::new(None);
        job_amount.add_css_class("job-amount");
        job_amount.set_xalign(1.0);
        let job_bar = gtk::ProgressBar::new();
        job_bar.set_valign(gtk::Align::Center);
        job_bar.set_width_request(220);
        let job_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        job_box.add_css_class("job");
        job_box.append(&job_label);
        job_box.append(&job_amount);
        job_box.append(&job_bar);
        let job_cancel = gtk::Button::from_icon_name("process-stop-symbolic");
        job_cancel.set_tooltip_text(Some("Cancel"));
        job_cancel.add_css_class("job-cancel");
        job_cancel.set_valign(gtk::Align::Center);
        job_box.append(&job_cancel);

        // Conflict bar (nautilus-file-conflict-dialog, inline).
        let conflict_title = gtk::Label::new(None);
        conflict_title.set_xalign(0.0);
        conflict_title.add_css_class("conflict-title");
        conflict_title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let conflict_detail = gtk::Label::new(None);
        conflict_detail.set_xalign(0.0);
        conflict_detail.set_wrap(true);
        conflict_detail.add_css_class("dim-label");
        let conflict_entry = gtk::Entry::builder().hexpand(true).visible(false).build();
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let conflict_all = gtk::CheckButton::with_label("Apply to all");
        let conflict_cancel = gtk::Button::with_label("Cancel");
        let conflict_skip = gtk::Button::with_label("Skip");
        let conflict_rename = gtk::Button::with_label("Rename…");
        let conflict_replace = gtk::Button::with_label("Replace");
        conflict_replace.add_css_class("suggested-action");
        let conflict_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        conflict_buttons.append(&conflict_entry);
        conflict_buttons.append(&spacer);
        conflict_buttons.append(&conflict_all);
        conflict_buttons.append(&conflict_cancel);
        conflict_buttons.append(&conflict_skip);
        conflict_buttons.append(&conflict_rename);
        conflict_buttons.append(&conflict_replace);
        let conflict_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        conflict_box.add_css_class("prompt");
        conflict_box.add_css_class("conflict");
        conflict_box.append(&conflict_title);
        conflict_box.append(&conflict_detail);
        conflict_box.append(&conflict_buttons);
        let conflict = gtk::Revealer::builder()
            .child(&conflict_box)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();
        let job = gtk::Revealer::builder()
            .child(&job_box)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        // Footer.
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        let message = gtk::Label::new(None);
        message.add_css_class("message");
        message.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        footer.add_css_class("footer");
        footer.append(&status);
        footer.append(&message);

        let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main.append(&toolbar);
        main.append(&view_overlay);
        main.append(&prompt);
        let auth = auth::AuthBar::new();
        main.append(auth.widget());
        main.append(&conflict);
        main.append(&job);
        main.append(&footer);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&side_scroll));
        paned.set_end_child(Some(&main));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_position(220);
        window.set_child(Some(&paned));

        let home = gio::File::for_path(glib::home_dir());
        let win = Rc::new(Self {
            window,
            sidebar,
            places: RefCell::default(),
            peers: RefCell::default(),
            peers_checked: Cell::new(None),
            monitor: gio::VolumeMonitor::get(),
            path_stack,
            pathbar,
            location,
            search,
            back_btn,
            forward_btn,
            up_btn,
            view,
            views,
            grid,
            groups,
            grouped_model,
            group_tick: RefCell::default(),
            list_btn,
            grid_btn,
            group_btn,
            mode: Cell::new(start_mode),
            grouped,
            group_sorter,
            folders_first,
            toolbar_row1: header,
            toolbar_row2,
            controls,
            compact: Cell::new(false),
            size_probe,
            list: RefCell::new(list),
            recent: RefCell::default(),
            filtered,
            filter,
            needle: RefCell::default(),
            untyped: RefCell::default(),
            typing: Cell::new(false),
            search_task: RefCell::default(),
            search_state: Cell::new(SearchState::Off),
            sorted,
            selection,
            empty,
            prompt,
            prompt_label,
            prompt_entry,
            prompt_ok,
            prompt_action: RefCell::default(),
            job,
            job_label,
            job_amount,
            job_bar,
            job_handle: RefCell::default(),
            conflict,
            conflict_title,
            conflict_detail,
            conflict_entry,
            conflict_all,
            conflict_replace,
            conflict_rename,
            conflict_reply: RefCell::default(),
            conflict_focus: RefCell::default(),
            auth,
            busy: Cell::new(false),
            status,
            status_pending: Cell::new(false),
            message,
            message_seq: Rc::default(),
            current: RefCell::new(home),
            back: RefCell::default(),
            forward: RefCell::default(),
            show_hidden: Cell::new(false),
            zoom: Cell::new(load_zoom()),
            hover_timer: RefCell::default(),
            hover_start: Cell::new((0.0, 0.0)),
            bookmark_drop,
            bookmark_reveal,
            sidebar_drag: Cell::new(0),
            clip: RefCell::default(),
        });

        let w = Rc::downgrade(&win);
        prompt_cancel.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.close_prompt();
            }
        });
        let w = Rc::downgrade(&win);
        job_cancel.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.cancel_job();
            }
        });
        // GTK ignores focus requests for widgets that aren't on screen yet,
        // so focus the conflict bar's button once it has fully slid in.
        let w = Rc::downgrade(&win);
        win.conflict.connect_child_revealed_notify(move |bar| {
            if !bar.is_child_revealed() {
                return;
            }
            if let Some(target) = w.upgrade().and_then(|win| win.conflict_focus.take()) {
                target.grab_focus();
            }
        });
        let w = Rc::downgrade(&win);
        conflict_cancel.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.answer_conflict(ops::Choice::Cancel);
            }
        });
        let w = Rc::downgrade(&win);
        conflict_skip.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.answer_conflict(ops::Choice::Skip);
            }
        });
        let w = Rc::downgrade(&win);
        win.conflict_replace.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.answer_conflict(ops::Choice::Replace);
            }
        });
        let w = Rc::downgrade(&win);
        win.conflict_rename.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.rename_conflict();
            }
        });
        let w = Rc::downgrade(&win);
        win.conflict_entry.connect_activate(move |_| {
            if let Some(win) = w.upgrade() {
                win.rename_conflict();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&win);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(win) = w.upgrade() {
                    win.answer_conflict(ops::Choice::Cancel);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        win.conflict.add_controller(keys);
        win
    }

    fn wire(win: &Rc<Self>) {
        Self::setup_columns(win);
        Self::setup_views(win);
        Self::setup_filter(win);
        Self::setup_navigation(win);
        Self::setup_sidebar(win);
        Self::setup_prompt(win);
        Self::setup_shortcuts(win);
        Self::setup_background_menu(win);
        Self::setup_scroll_zoom(win);
        Self::setup_dnd(win);
    }

    // ------------------------------------------------------------ columns --

    #[allow(clippy::too_many_lines)]
    fn setup_columns(win: &Rc<Self>) {
        let w = Rc::downgrade(win);

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

        // Per-column sorters; the view's sorter is chained after folders-first.
        // Keyed sorters: GTK works out each file's key once per sort.
        let name_sorter = gtk::StringSorter::new(Some(info_key(|i| i.display_name().to_string())));
        name_sorter.set_collation(gtk::Collation::Filename);
        let size_sorter = gtk::NumericSorter::new(Some(info_key(ops::size_of)));
        let time_sorter = gtk::NumericSorter::new(Some(info_key(|i| {
            i.modification_date_time().map_or(0, |d| d.to_unix())
        })));

        let name_col = gtk::ColumnViewColumn::new(Some("Name"), Some(name_factory));
        name_col.set_expand(true);
        name_col.set_resizable(true);
        name_col.set_sorter(Some(&name_sorter));
        let size_col = gtk::ColumnViewColumn::new(
            Some("Size"),
            Some(text_factory(Rc::downgrade(win), size_text)),
        );
        size_col.set_fixed_width(100);
        size_col.set_sorter(Some(&size_sorter));
        let time_col = gtk::ColumnViewColumn::new(
            Some("Modified"),
            Some(text_factory(Rc::downgrade(win), format_time)),
        );
        time_col.set_fixed_width(130);
        time_col.set_sorter(Some(&time_sorter));
        win.view.append_column(&name_col);
        win.view.append_column(&size_col);
        win.view.append_column(&time_col);

        // The headers pick the column; `apply_sort` builds the model's sorter.
        if let Some(picked) = win.view.sorter() {
            let w = Rc::downgrade(win);
            picked.connect_changed(move |_, _| {
                if let Some(win) = w.upgrade() {
                    win.apply_sort();
                }
            });
        }
        win.view
            .sort_by_column(Some(&name_col), gtk::SortType::Ascending);
        win.apply_sort();

        let w = Rc::downgrade(win);
        win.view.connect_activate(move |_, pos| {
            if let Some(win) = w.upgrade() {
                win.activate(pos);
            }
        });

        let w = Rc::downgrade(win);
        win.selection.connect_selection_changed(move |_, _, _| {
            if let Some(win) = w.upgrade() {
                win.schedule_status();
            }
        });
        let w = Rc::downgrade(win);
        win.sorted.connect_items_changed(move |_, _, _, _| {
            if let Some(win) = w.upgrade() {
                win.schedule_status();
                win.keep_top();
            }
        });
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
        all.append(self.folders_first.clone());
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

    /// Updates the footer once the current burst of changes is over: a
    /// folder loads in many batches, and each would walk the selection.
    fn schedule_status(self: &Rc<Self>) {
        if self.status_pending.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(win) = w.upgrade() {
                win.status_pending.set(false);
                win.update_status();
            }
        });
    }

    /// The position in the shared selection of a view's item: the grouped
    /// grid's positions include headings and fillers.
    fn shared_pos(&self, grouped: bool, pos: u32) -> Option<u32> {
        if pos == gtk::INVALID_LIST_POSITION {
            None
        } else if grouped {
            self.grouped_model.file_at(pos)
        } else {
            Some(pos)
        }
    }

    /// Right-click on a row selects it (unless it already is) and shows the
    /// file menu.
    fn attach_item_menu(
        w: &Weak<Self>,
        item: &gtk::ListItem,
        widget: &impl IsA<gtk::Widget>,
        grouped: bool,
    ) {
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        let w = w.clone();
        let item = item.downgrade();
        let widget_ref = widget.as_ref().downgrade();
        click.connect_pressed(move |gesture, _, x, y| {
            let (Some(win), Some(item), Some(widget)) =
                (w.upgrade(), item.upgrade(), widget_ref.upgrade())
            else {
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let Some(pos) = win.shared_pos(grouped, item.position()) else {
                return;
            };
            if !win.selection.is_selected(pos) {
                win.selection.select_item(pos, true);
            }
            let (x, y) = translate(&widget, &win.views, x, y);
            win.show_file_menu(x, y);
        });
        widget.add_controller(click);
    }

    /// Ctrl+scroll zooms the file list.
    fn setup_scroll_zoom(win: &Rc<Self>) {
        // Discrete: GTK adds up a touchpad's small movements into whole
        // notches, so each notch is exactly one zoom level. A short pause
        // after each step keeps a fast flick from skipping levels.
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(win);
        let last = Cell::new(None::<std::time::Instant>);
        scroll.connect_scroll(move |c, _, dy| {
            let ctrl = c
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            let Some(win) = w.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if !ctrl || dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let settled = last
                .get()
                .is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(120));
            if settled {
                last.set(Some(std::time::Instant::now()));
                win.zoom_by(if dy < 0.0 { 1 } else { -1 });
            }
            glib::Propagation::Stop
        });
        win.views.add_controller(scroll);
    }

    fn setup_background_menu(win: &Rc<Self>) {
        let views: [gtk::Widget; 3] = [
            win.view.clone().upcast(),
            win.grid.clone().upcast(),
            win.groups.clone().upcast(),
        ];
        for view in views {
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_SECONDARY);
            let w = Rc::downgrade(win);
            let v = view.downgrade();
            click.connect_pressed(move |_, _, x, y| {
                let (Some(win), Some(v)) = (w.upgrade(), v.upgrade()) else {
                    return;
                };
                win.selection.unselect_all();
                let (x, y) = translate(&v, &win.views, x, y);
                win.show_folder_menu(x, y);
            });
            view.add_controller(click);
        }
    }

    // ------------------------------------------------------- drag & drop --

    /// Rows drag their selection out, and folder rows take drops (and open
    /// after hovering), as in nautilus-list-base.c `setup_cell_common()`.
    fn attach_item_dnd(
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
            let (win, item) = (ww.upgrade()?, it.upgrade()?);
            let pos = win.shared_pos(grouped, item.position())?;
            win.drag_prepare(source, pos)
        });
        widget.add_controller(source);

        let it = item.downgrade();
        let folder = Rc::new(move || {
            let info = it.upgrade()?.item().and_downcast::<gio::FileInfo>()?;
            is_dir(&info)
                .then(|| info_file(info.upcast_ref()))
                .flatten()
        });
        Self::attach_drop(w, widget, folder, true);
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
        {
            let icon = crate::info::icon(&selected[0].0);
            let theme = gtk::IconTheme::for_display(&WidgetExt::display(&self.window));
            let paintable = theme.lookup_by_gicon(
                &icon,
                32,
                self.window.scale_factor(),
                gtk::TextDirection::None,
                gtk::IconLookupFlags::empty(),
            );
            source.set_icon(Some(&paintable), 0, 0);
        }
        let files: Vec<gio::File> = selected.into_iter().map(|(_, f)| f).collect();
        Some(gdk::ContentProvider::for_value(
            &gdk::FileList::from_array(&files).to_value(),
        ))
    }

    /// A drop target onto whatever folder `folder()` names right now. With
    /// `hover_open`, holding a drag over it for a moment opens it.
    fn attach_drop(
        w: &Weak<Self>,
        widget: &impl IsA<gtk::Widget>,
        folder: Rc<dyn Fn() -> Option<gio::File>>,
        hover_open: bool,
    ) {
        let target = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::all());
        target.set_types(&dnd::drop_types());
        target.set_preload(true);
        let action = Rc::new(Cell::new(gdk::DragAction::empty()));

        let compute = {
            let folder = folder.clone();
            let action = action.clone();
            move |t: &gtk::DropTarget| {
                let a = folder().map_or(gdk::DragAction::empty(), |dest| match t.value() {
                    Some(v) if v.type_() == gdk::FileList::static_type() => {
                        dnd::preferred_action(&dest, dnd::value_files(&v).as_deref())
                    }
                    // Text and images are saved as new files.
                    Some(_) => dnd::preferred_action(&dest, None) & gdk::DragAction::COPY,
                    None => dnd::preferred_action(&dest, None),
                });
                action.set(a);
                a
            }
        };
        let c = compute.clone();
        target.connect_enter(move |t, _, _| c(t));
        let c = compute;
        target.connect_value_notify(move |t| {
            c(t);
        });
        let a = action.clone();
        target.connect_motion(move |_, _, _| a.get());
        let ww = w.clone();
        let f = folder.clone();
        let a = action.clone();
        target.connect_drop(move |t, value, _, _| {
            let (Some(win), Some(dest)) = (ww.upgrade(), f()) else {
                return false;
            };
            win.cancel_hover();
            if a.get().is_empty() {
                return false;
            }
            let chosen = dnd::chosen_action(t.current_drop(), a.get());
            win.perform_drop(value, chosen, dest)
        });
        widget.add_controller(target);

        if !hover_open {
            return;
        }
        let motion = gtk::DropControllerMotion::new();
        let ww = w.clone();
        motion.connect_enter(move |_, x, y| {
            if let Some(win) = ww.upgrade() {
                win.hover_start.set((x, y));
            }
        });
        let ww = w.clone();
        motion.connect_leave(move |_| {
            if let Some(win) = ww.upgrade() {
                win.cancel_hover();
            }
        });
        let ww = w.clone();
        let wid = widget.as_ref().downgrade();
        motion.connect_motion(move |_, x, y| {
            let (Some(win), Some(wid)) = (ww.upgrade(), wid.upgrade()) else {
                return;
            };
            // Restart the timer only after real movement, so brushing past
            // a folder doesn't open it (on_item_drag_hover_motion).
            let (sx, sy) = win.hover_start.get();
            if !wid.drag_check_threshold(px(sx), px(sy), px(x), px(y))
                && win.hover_timer.borrow().is_some()
            {
                return;
            }
            win.hover_start.set((x, y));
            win.cancel_hover();
            let w2 = Rc::downgrade(&win);
            let folder = folder.clone();
            let action = action.clone();
            let id = glib::timeout_add_local_once(
                std::time::Duration::from_millis(dnd::HOVER_TIMEOUT_MS),
                move || {
                    let Some(win) = w2.upgrade() else { return };
                    win.hover_timer.replace(None);
                    if action.get().is_empty() {
                        return;
                    }
                    if let Some(dest) = folder() {
                        let here = win.current.borrow().clone();
                        if !dest.equal(&here) {
                            win.navigate(dest, true);
                        }
                    }
                },
            );
            win.hover_timer.replace(Some(id));
        });
        widget.add_controller(motion);
    }

    fn cancel_hover(&self) {
        if let Some(id) = self.hover_timer.take() {
            id.remove();
        }
    }

    /// `nautilus_dnd_perform_drop()` + `drop_proxy_received_uris()`.
    fn perform_drop(
        self: &Rc<Self>,
        value: &glib::Value,
        action: gdk::DragAction,
        dest: gio::File,
    ) -> bool {
        if let Some(files) = dnd::value_files(value) {
            let Some(first) = files.first() else {
                return false;
            };
            // A move into the folder it's already in is a no-op; Ctrl
            // (copy) still duplicates.
            if action == gdk::DragAction::MOVE && first.parent().is_some_and(|p| p.equal(&dest)) {
                return false;
            }
            if dest.has_uri_scheme("trash") {
                self.trash(files);
            } else if action == gdk::DragAction::LINK {
                self.run_job(ops::link(files, dest));
            } else if action == gdk::DragAction::MOVE {
                self.run_transfer(files, dest, Kind::Move);
            } else {
                self.run_transfer(files, dest, Kind::Copy);
            }
            return true;
        }
        let (name, bytes) = if let Ok(texture) = value.get::<gdk::Texture>() {
            (
                "Dropped Image.png".to_string(),
                texture.save_to_png_bytes().to_vec(),
            )
        } else if let Ok(text) = value.get::<String>() {
            (crate::filename::for_dropped_text(&text), text.into_bytes())
        } else {
            return false;
        };
        let win = self.clone();
        glib::spawn_future_local(async move {
            if let Err(e) = ops::new_file_with_contents(&dest, &name, bytes).await {
                win.show_message(&e.to_string());
            }
        });
        true
    }

    fn setup_dnd(win: &Rc<Self>) {
        // Background of the list: drop into the current folder.
        let w = Rc::downgrade(win);
        let current = Rc::new(move || w.upgrade().map(|win| win.current.borrow().clone()));
        Self::attach_drop(&Rc::downgrade(win), &win.views, current, false);

        // Show the bookmark strip while files are dragged over the sidebar.
        let Some(side) = win.sidebar.parent() else {
            return;
        };
        let motion = gtk::DropControllerMotion::new();
        let w = Rc::downgrade(win);
        motion.connect_enter(move |m, _, _| {
            let Some(win) = w.upgrade() else { return };
            let drag = win.sidebar_drag.get().wrapping_add(1);
            win.sidebar_drag.set(drag);
            let Some(drop) = m.drop() else { return };
            if !drop.formats().contains_type(gdk::FileList::static_type()) {
                return;
            }
            // Only folders can be bookmarks: read the dragged list first.
            glib::spawn_future_local(async move {
                let value = drop
                    .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                    .await;
                let Some(files) = value.ok().and_then(|v| dnd::value_files(&v)) else {
                    return;
                };
                let has_folder = files.iter().any(|f| {
                    f.is_native()
                        && f.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                            == gio::FileType::Directory
                });
                if has_folder && win.sidebar_drag.get() == drag {
                    win.bookmark_reveal.set_reveal_child(true);
                }
            });
        });
        let w = Rc::downgrade(win);
        motion.connect_leave(move |_| {
            if let Some(win) = w.upgrade() {
                win.sidebar_drag.set(win.sidebar_drag.get().wrapping_add(1));
                win.bookmark_reveal.set_reveal_child(false);
            }
        });
        side.add_controller(motion);

        // Folders dropped on the strip become bookmarks (drop_files_as_bookmarks).
        let target = gtk::DropTarget::new(
            gdk::FileList::static_type(),
            gdk::DragAction::COPY | gdk::DragAction::LINK | gdk::DragAction::MOVE,
        );
        let w = Rc::downgrade(win);
        target.connect_drop(move |_, value, _, _| {
            let Some(win) = w.upgrade() else { return false };
            win.bookmark_reveal.set_reveal_child(false);
            let Some(files) = dnd::value_files(value) else {
                return false;
            };
            for file in files {
                let kind =
                    file.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE);
                if matches!(
                    kind,
                    gio::FileType::Directory | gio::FileType::Mountable | gio::FileType::Shortcut
                ) {
                    let _ = places::add_bookmark(&file);
                }
            }
            win.refresh_sidebar();
            true
        });
        win.bookmark_drop.add_controller(target);
    }

    // ------------------------------------------------------------- filter --

    fn setup_filter(win: &Rc<Self>) {
        let w = Rc::downgrade(win);
        win.filter.set_filter_func(move |obj| {
            let Some(win) = w.upgrade() else { return true };
            let Some(info) = obj.downcast_ref::<gio::FileInfo>() else {
                return false;
            };
            if !win.show_hidden.get() && is_hidden(info) {
                return false;
            }
            let shown = {
                let needle = win.needle.borrow();
                needle.is_empty() || search::matches(&info.display_name(), &needle)
            };
            if shown && crate::info::needs_type(info) {
                win.queue_typing(info);
            }
            shown
        });

        win.search.set_key_capture_widget(Some(&win.window));
        let w = Rc::downgrade(win);
        win.search.connect_search_changed(move |search| {
            if let Some(win) = w.upgrade() {
                // A longer term only hides more, a shorter one only shows
                // more: GTK then rechecks just the files that could change.
                let needle = search::fold(&search.text());
                let old = win.needle.replace(needle.clone());
                if needle != old {
                    win.filter.changed(if needle.contains(old.as_str()) {
                        gtk::FilterChange::MoreStrict
                    } else if old.contains(needle.as_str()) {
                        gtk::FilterChange::LessStrict
                    } else {
                        gtk::FilterChange::Different
                    });
                    win.start_search();
                }
                // Typing goes to the filter while focus stays on the list, so
                // make the first match the current, selected row: Enter then
                // opens it, and F2 / Menu / Delete act on it.
                if !search.text().is_empty() && win.selection.n_items() > 0 {
                    win.selection.select_item(0, true);
                    win.scroll_to_pos(0);
                }
            }
        });
        let w = Rc::downgrade(win);
        win.search.connect_activate(move |_| {
            if let Some(win) = w.upgrade() {
                if win.selection.n_items() > 0 {
                    win.selection.select_item(0, true);
                    win.scroll_to_pos(0);
                }
                win.focus_view();
            }
        });
        let w = Rc::downgrade(win);
        win.search.connect_stop_search(move |s| {
            s.set_text("");
            if let Some(win) = w.upgrade() {
                win.focus_view();
            }
        });
    }

    fn set_show_hidden(self: &Rc<Self>, show: bool) {
        self.show_hidden.set(show);
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
            if let Some(win) = w.upgrade() {
                win.type_queued();
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
                let Some(batch) = w.upgrade().map(|win| win.untyped.take()) else {
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
                let Some(win) = w.upgrade() else { return };
                win.refresh_rows(&typed);
            }
            if let Some(win) = w.upgrade() {
                win.typing.set(false);
                // Queued while the last batch was being looked up.
                if !win.untyped.borrow().is_empty() && !win.typing.replace(true) {
                    win.type_queued();
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
        if self.grouped.get() {
            self.group_sorter.changed(gtk::SorterChange::Different);
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
        let hidden = self.show_hidden.get();
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
                    let Some(win) = w.upgrade() else { return };
                    store.extend_from_slice(&infos);
                    // Swap the folder's listing for the results once some
                    // are in, so typing never flashes an empty list.
                    if store.n_items() > 0 && !win.showing(&store) {
                        win.filtered.set_model(Some(&store));
                    }
                }
            });
            futures_util::future::join(find, show).await;
            if let Some(win) = w.upgrade() {
                if !win.showing(&store) {
                    win.filtered.set_model(Some(&store));
                }
                win.search_state.set(SearchState::Done);
                win.search_task.replace(None);
                win.schedule_status();
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
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
        if self.search_state.replace(SearchState::Off) != SearchState::Off {
            let list = self.list.borrow().clone();
            if !self.showing(&list) {
                self.filtered.set_model(Some(&list));
            }
            self.schedule_status();
        }
    }

    // --------------------------------------------------------- navigation --

    fn setup_navigation(win: &Rc<Self>) {
        let w = Rc::downgrade(win);
        win.back_btn.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.go_back();
            }
        });
        let w = Rc::downgrade(win);
        win.forward_btn.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.go_forward();
            }
        });
        let w = Rc::downgrade(win);
        win.up_btn.connect_clicked(move |_| {
            if let Some(win) = w.upgrade() {
                win.go_up();
            }
        });

        let w = Rc::downgrade(win);
        win.location.connect_activate(move |entry| {
            if let Some(win) = w.upgrade() {
                let file = win.parse_location(&entry.text());
                win.hide_location();
                win.navigate(file, true);
            }
        });
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(win);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(win) = w.upgrade() {
                    win.hide_location();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        win.location.add_controller(keys);

        let first = win.list.borrow().clone();
        win.watch_list(&first);

        // Refresh Tailscale peers when the window comes back into focus.
        let w = Rc::downgrade(win);
        win.window.connect_is_active_notify(move |window| {
            if let (Some(win), true) = (w.upgrade(), window.is_active()) {
                win.refresh_peers();
            }
        });
    }

    fn parse_location(&self, text: &str) -> gio::File {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix('~') {
            return gio::File::for_path(format!("{}{}", glib::home_dir().display(), rest));
        }
        if text.starts_with('/') || text.contains("://") || text.ends_with(":///") {
            return gio::File::for_commandline_arg(text);
        }
        self.current.borrow().resolve_relative_path(text)
    }

    pub fn navigate(self: &Rc<Self>, file: gio::File, record: bool) {
        if record {
            let current = self.current.borrow().clone();
            if !current.equal(&file) {
                self.back.borrow_mut().push(current);
                self.forward.borrow_mut().clear();
            }
        }
        // Set before reloading: the path bar and sidebar read `current`.
        *self.current.borrow_mut() = file;
        self.search.set_text("");
        self.selection.unselect_all();
        let file = self.current.borrow().clone();
        self.reload_at(&file, false);
    }

    /// Reports a listing's errors and progress while it's the one shown.
    fn watch_list(self: &Rc<Self>, list: &gtk::DirectoryList) {
        // Mount on demand (nautilus-window-slot.c: mount_not_mounted_cb).
        let w = Rc::downgrade(self);
        list.connect_error_notify(move |list| {
            let Some(win) = w.upgrade().filter(|win| *win.list.borrow() == *list) else {
                return;
            };
            let Some(error) = list.error() else {
                return;
            };
            if error.matches(gio::IOErrorEnum::NotMounted) {
                let file = win.current.borrow().clone();
                win.mount_then(file, |win, file| win.reload_at(&file, true));
            } else {
                win.show_message(&error.to_string());
            }
        });
        let w = Rc::downgrade(self);
        list.connect_loading_notify(move |list| {
            let Some(win) = w.upgrade().filter(|win| *win.list.borrow() == *list) else {
                return;
            };
            win.schedule_status();
            if !list.is_loading() {
                win.reveal_top();
            }
        });
    }

    /// Shows `file`'s listing: the one kept from a recent visit when it's
    /// still good (and `fresh` isn't asked for), otherwise read anew. The
    /// listing being left is kept for later.
    fn show_listing(self: &Rc<Self>, file: &gio::File, fresh: bool) {
        self.stop_search();
        let current = self.list.borrow().clone();
        let reread = |list: &gtk::DirectoryList| {
            list.set_file(None::<&gio::File>);
            list.set_attributes(Some(crate::info::attributes(file)));
            list.set_file(Some(file));
        };
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
                let list = new_list();
                self.watch_list(&list);
                list.set_attributes(Some(crate::info::attributes(file)));
                list.set_file(Some(file));
                list
            }
        };
        self.list.replace(next.clone());
        self.filtered.set_model(Some(&next));
        // A kept listing would otherwise open where it was scrolled.
        if next.n_items() > 0 {
            self.view
                .scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
            self.grid.scroll_to(0, gtk::ListScrollFlags::NONE, None);
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

    fn reload_at(self: &Rc<Self>, file: &gio::File, fresh: bool) {
        self.show_listing(file, fresh);
        self.rebuild_pathbar();
        self.back_btn.set_sensitive(!self.back.borrow().is_empty());
        self.forward_btn
            .set_sensitive(!self.forward.borrow().is_empty());
        self.up_btn.set_sensitive(parent_of(file).is_some());
        let name = if file.parent().is_none() {
            root_label(file)
        } else {
            ops::display_name(file)
        };
        self.window.set_title(Some(&format!("{name} — Abyss")));
        self.select_sidebar_row();
        self.schedule_status();
    }

    fn reload(self: &Rc<Self>) {
        let file = self.current.borrow().clone();
        self.reload_at(&file, true);
    }

    fn go_back(self: &Rc<Self>) {
        let Some(prev) = self.back.borrow_mut().pop() else {
            return;
        };
        let current = self.current.borrow().clone();
        self.forward.borrow_mut().push(current);
        self.navigate(prev, false);
    }

    fn go_forward(self: &Rc<Self>) {
        let Some(next) = self.forward.borrow_mut().pop() else {
            return;
        };
        let current = self.current.borrow().clone();
        self.back.borrow_mut().push(current);
        self.navigate(next, false);
    }

    fn go_up(self: &Rc<Self>) {
        let parent = parent_of(&self.current.borrow());
        if let Some(parent) = parent {
            self.navigate(parent, true);
        }
    }

    fn show_location(&self) {
        let file = self.current.borrow().clone();
        let text = match file.path() {
            Some(p) if file.is_native() => p.display().to_string(),
            _ => file.uri().to_string(),
        };
        self.location.set_text(&text);
        self.path_stack.set_visible_child_name("entry");
        self.location.grab_focus();
        self.location.select_region(0, -1);
    }

    fn prompt_location(&self, prefix: &str) {
        self.path_stack.set_visible_child_name("entry");
        self.location.set_text(prefix);
        self.location.grab_focus();
        self.location.set_position(-1);
    }

    fn hide_location(&self) {
        self.path_stack.set_visible_child_name("bar");
        self.focus_view();
    }

    /// Breadcrumbs from the root (or ~) to the current folder.
    fn rebuild_pathbar(self: &Rc<Self>) {
        while let Some(child) = self.pathbar.first_child() {
            self.pathbar.remove(&child);
        }
        let home = gio::File::for_path(glib::home_dir());
        let mut chain = Vec::new();
        let mut cursor = Some(self.current.borrow().clone());
        while let Some(f) = cursor {
            let at_home = f.equal(&home);
            cursor = if at_home { None } else { parent_of(&f) };
            chain.push((f, at_home));
        }
        chain.reverse();
        let last = chain.len().saturating_sub(1);
        for (i, (file, at_home)) in chain.into_iter().enumerate() {
            let label = if at_home {
                "Home".to_string()
            } else if parent_of(&file).is_none() {
                root_label(&file)
            } else if file.parent().is_none() {
                // An SMB share under its server crumb: GVfs names its root
                // "/", so use the last part of the address.
                share_name(&file)
            } else {
                ops::display_name(&file)
            };
            if i > 0 {
                let sep = gtk::Label::new(Some("/"));
                sep.add_css_class("dim-label");
                self.pathbar.append(&sep);
            }
            let button = gtk::Button::with_label(&label);
            button.add_css_class("flat");
            if i == last {
                button.add_css_class("current");
            }
            if i != last {
                let f = file.clone();
                Self::attach_drop(
                    &Rc::downgrade(self),
                    &button,
                    Rc::new(move || Some(f.clone())),
                    true,
                );
            }
            let w = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(win) = w.upgrade() {
                    win.navigate(file.clone(), true);
                }
            });
            self.pathbar.append(&button);
        }
        let click = gtk::GestureClick::new();
        // Clicking empty path bar space switches to the editable location.
        let w = Rc::downgrade(self);
        click.connect_released(move |g, n, _, _| {
            if n == 2 {
                if let Some(win) = w.upgrade() {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    win.show_location();
                }
            }
        });
        self.pathbar.add_controller(click);

        // Keep the current folder in view when the path is wider than the
        // bar: scroll to the end once the new crumbs have been laid out.
        if let Some(scroll) = self
            .pathbar
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
        {
            let adj = scroll.hadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper()));
        }
    }

    fn mount_then(
        self: &Rc<Self>,
        file: gio::File,
        then: impl FnOnce(&Rc<Self>, gio::File) + 'static,
    ) {
        let op = self.auth.operation(Some(&file.uri()));
        let win = self.clone();
        self.show_message(&format!("Connecting to {}…", root_label(&file)));
        glib::spawn_future_local(async move {
            match file
                .mount_enclosing_volume_future(gio::MountMountFlags::NONE, Some(&op))
                .await
            {
                Ok(()) => {
                    win.show_message("");
                    then(&win, file);
                }
                Err(e) if e.matches(gio::IOErrorEnum::AlreadyMounted) => then(&win, file),
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => win.show_message(""),
                Err(e) => win.show_message(&e.to_string()),
            }
        });
    }

    // ------------------------------------------------------------ opening --

    fn item(&self, pos: u32) -> Option<gio::FileInfo> {
        self.selection.item(pos).and_downcast::<gio::FileInfo>()
    }

    fn selected(&self) -> Vec<(gio::FileInfo, gio::File)> {
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

    fn selected_files(&self) -> Vec<gio::File> {
        self.selected().into_iter().map(|(_, f)| f).collect()
    }

    fn activate(self: &Rc<Self>, pos: u32) {
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
        for (info, file) in targets {
            if !is_dir(&info) {
                self.launch(&file, None);
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
                // Share the login with the server the mountable points at.
                let target = info
                    .attribute_string("standard::target-uri")
                    .map_or_else(|| file.uri().to_string(), |t| t.to_string());
                let op = self.auth.operation(Some(&target));
                let win = self.clone();
                let file = file.clone();
                glib::spawn_future_local(async move {
                    match file
                        .mount_mountable_future(gio::MountMountFlags::NONE, Some(&op))
                        .await
                    {
                        Ok(target) => win.navigate(target, true),
                        Err(e) if e.matches(gio::IOErrorEnum::AlreadyMounted) => {
                            if let Some(uri) = info_target(&file).await {
                                win.navigate(gio::File::for_uri(&uri), true);
                            }
                        }
                        Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => {}
                        Err(e) => win.show_message(&e.to_string()),
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

    /// Opens several files with one app (GIO starts one instance per file
    /// if the app can't take them all).
    fn launch_files(&self, files: &[gio::File], app: &gio::AppInfo) {
        let ctx = WidgetExt::display(&self.window).app_launch_context();
        if let Err(e) = app.launch(files, Some(&ctx)) {
            self.show_message(&e.to_string());
        }
    }

    /// The app chooser for the selection (`action_open_with_other_application`).
    fn open_with_other(self: &Rc<Self>) {
        let selected = self.selected();
        let Some((first, _)) = selected.first() else {
            return;
        };
        // A shared content type enables recommendations and "always use".
        let first_type = crate::info::content_type(first);
        let shared = selected
            .iter()
            .all(|(i, _)| crate::info::content_type(i) == first_type);
        let content_type = if shared {
            first_type.map(|t| t.to_string())
        } else {
            None
        };
        let subject = if let [(info, _)] = selected.as_slice() {
            info.display_name().to_string()
        } else {
            format!("{} items", selected.len())
        };
        let files: Vec<gio::File> = selected.into_iter().map(|(_, f)| f).collect();
        self.choose_app(content_type, &subject, files);
    }

    fn choose_app(
        self: &Rc<Self>,
        content_type: Option<String>,
        subject: &str,
        files: Vec<gio::File>,
    ) {
        let w = Rc::downgrade(self);
        let on_chosen: chooser::OnChosen = Box::new(move |app, note| {
            let Some(win) = w.upgrade() else { return };
            if let Some(note) = note {
                win.show_message(&note);
            }
            win.launch_files(&files, &app);
        });
        chooser::show(self.window.upcast_ref(), content_type, subject, on_chosen);
    }

    fn launch(self: &Rc<Self>, file: &gio::File, app: Option<gio::AppInfo>) {
        let ctx = WidgetExt::display(&self.window).app_launch_context();
        let uri = file.uri().to_string();
        let win = self.clone();
        glib::spawn_future_local(async move {
            let result = match app {
                Some(app) => app.launch_uris(&[&uri], Some(&ctx)),
                None => gio::AppInfo::launch_default_for_uri_future(&uri, Some(&ctx)).await,
            };
            if let Err(e) = result {
                win.show_message(&e.to_string());
            }
        });
    }

    fn open_terminal(&self, dir: &gio::File) {
        let (Some(term), Some(path)) = (terminal(), dir.path()) else {
            self.show_message("No terminal found (set $TERMINAL)");
            return;
        };
        let mut cmd = std::process::Command::new(&term);
        cmd.current_dir(&path);
        if term == "alacritty" || term == "wezterm" {
            cmd.arg("--working-directory").arg(&path);
        }
        if let Err(e) = cmd.spawn() {
            self.show_message(&e.to_string());
        }
    }

    // -------------------------------------------------------------- menus --

    fn popover_at(&self, x: f64, y: f64, items: Vec<MenuItem>) {
        let popover = gtk::Popover::new();
        popover.add_css_class("menu");
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for item in items {
            match item {
                MenuItem::Separator => {
                    column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                }
                MenuItem::Action(label, run) => {
                    let button = gtk::Button::with_label(&label);
                    button.add_css_class("flat");
                    if let Some(l) = button.child().and_downcast::<gtk::Label>() {
                        l.set_xalign(0.0);
                    }
                    let pop = popover.downgrade();
                    let run = RefCell::new(Some(run));
                    button.connect_clicked(move |_| {
                        if let Some(p) = pop.upgrade() {
                            p.popdown();
                        }
                        if let Some(run) = run.borrow_mut().take() {
                            run();
                        }
                    });
                    column.append(&button);
                }
            }
        }
        popover.set_child(Some(&column));
        popover.set_parent(&self.views);
        popover.set_pointing_to(Some(&spot(x, y)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// A menu entry that runs `f` on this window if it still exists.
    fn menu_action(self: &Rc<Self>, label: &str, f: impl Fn(&Rc<Self>) + 'static) -> MenuItem {
        let w = Rc::downgrade(self);
        MenuItem::Action(
            label.to_owned(),
            Box::new(move || {
                if let Some(win) = w.upgrade() {
                    f(&win);
                }
            }),
        )
    }

    /// Entries that only make sense for a single selected item: other apps
    /// for a file; terminal and bookmark for a local folder.
    fn single_item_actions(
        self: &Rc<Self>,
        info: &gio::FileInfo,
        file: &gio::File,
    ) -> Vec<MenuItem> {
        let mut items = Vec::new();
        if !is_dir(info) {
            let Some(ct) = crate::info::content_type(info) else {
                return items;
            };
            let default = gio::AppInfo::default_for_type(&ct, false);
            for app in gio::AppInfo::all_for_type(&ct).into_iter().take(6) {
                if default.as_ref().is_some_and(|d| d.equal(&app)) {
                    continue;
                }
                let file = file.clone();
                let label = format!("Open With {}", app.display_name());
                items.push(
                    self.menu_action(&label, move |win| win.launch(&file, Some(app.clone()))),
                );
            }
            return items;
        }
        if !file.is_native() {
            return items;
        }
        if terminal().is_some() {
            let f = file.clone();
            items.push(self.menu_action("Open in Terminal", move |win| win.open_terminal(&f)));
        }
        let uri = file.uri().to_string();
        if places::is_bookmark(&uri) {
            items.push(self.menu_action("Remove Bookmark", move |win| {
                let _ = places::remove_bookmark(&uri);
                win.refresh_sidebar();
            }));
        } else {
            let file = file.clone();
            items.push(self.menu_action("Add Bookmark", move |win| {
                let _ = places::add_bookmark(&file);
                win.refresh_sidebar();
            }));
        }
        items
    }

    fn show_file_menu(self: &Rc<Self>, x: f64, y: f64) {
        let selected = self.selected();
        if selected.is_empty() {
            return;
        }
        let files: Vec<gio::File> = selected.iter().map(|(_, f)| f.clone()).collect();
        let in_trash = self.current.borrow().uri_scheme().as_deref() == Some("trash");

        let mut items = vec![self.menu_action("Open", |win: &Rc<Self>| {
            if let Some(pos) = first_selected(&win.selection) {
                win.activate(pos);
            }
        })];

        if let [(info, file)] = selected.as_slice() {
            items.extend(self.single_item_actions(info, file));
        }
        // Files only: folders open in Abyss.
        if !in_trash && !selected.iter().any(|(i, _)| is_dir(i)) {
            items.push(self.menu_action("Open With Other Application…", |win| {
                win.open_with_other();
            }));
        }
        // can_extract_all: every selected item is an archive bsdtar reads.
        if !in_trash
            && archive::have_bsdtar()
            && selected.iter().all(|(i, _)| archive::is_archive(i))
        {
            let f = files.clone();
            items.push(self.menu_action("Extract Here", move |win| {
                let dest = win.current.borrow().clone();
                win.extract(f.clone(), dest);
            }));
            let f = files.clone();
            items.push(self.menu_action("Extract To…", move |win| win.extract_to(f.clone())));
        }
        items.push(MenuItem::Separator);

        if in_trash {
            let restore: Vec<_> = selected
                .iter()
                .filter_map(|(i, f)| {
                    let orig = i.attribute_byte_string("trash::orig-path")?;
                    Some((f.clone(), gio::File::for_path(orig)))
                })
                .collect();
            items.push(self.menu_action("Restore", move |win: &Rc<Self>| {
                win.restore(restore.clone());
            }));
            let f = files;
            items.push(self.menu_action("Delete Forever", move |win: &Rc<Self>| {
                win.confirm_delete(f.clone());
            }));
            self.popover_at(x, y, items);
            return;
        }

        items.push(self.menu_action("Cut", |win: &Rc<Self>| win.copy_selection(true)));
        items.push(self.menu_action("Copy", |win: &Rc<Self>| win.copy_selection(false)));
        let f = files.clone();
        items.push(self.menu_action("Duplicate", move |win: &Rc<Self>| {
            let dest = win.current.borrow().clone();
            win.run_transfer(f.clone(), dest, Kind::Copy);
        }));
        if selected.len() == 1 {
            items.push(self.menu_action("Rename…", |win: &Rc<Self>| win.start_rename()));
        }
        items.push(self.menu_action("Copy Path", |win: &Rc<Self>| win.copy_paths()));
        items.push(MenuItem::Separator);
        let f = files.clone();
        items.push(self.menu_action("Move to Trash", move |win: &Rc<Self>| win.trash(f.clone())));
        let f = files;
        items.push(self.menu_action("Delete Forever…", move |win: &Rc<Self>| {
            win.confirm_delete(f.clone());
        }));
        self.popover_at(x, y, items);
    }

    fn show_folder_menu(self: &Rc<Self>, x: f64, y: f64) {
        let dir = self.current.borrow().clone();
        let mut items = Vec::new();
        if dir.uri_scheme().as_deref() == Some("trash") {
            items.push(self.menu_action("Restore All", Self::restore_all));
            items.push(self.menu_action("Empty Trash…", Self::confirm_empty_trash));
        } else {
            items.push(self.menu_action("New Folder…", Self::start_new_folder));
            items.push(self.menu_action("Paste", Self::paste));
            if dir.is_native() && terminal().is_some() {
                items
                    .push(self.menu_action("Open in Terminal", move |win| win.open_terminal(&dir)));
            }
        }
        items.push(MenuItem::Separator);
        let label = if self.show_hidden.get() {
            "Hide Hidden Files"
        } else {
            "Show Hidden Files"
        };
        items.push(self.menu_action(label, |win| win.set_show_hidden(!win.show_hidden.get())));
        items.push(self.menu_action("Reload", Self::reload));
        self.popover_at(x, y, items);
    }

    // ---------------------------------------------------------- clipboard --

    fn copy_selection(&self, cut: bool) {
        let files = self.selected_files();
        if files.is_empty() {
            return;
        }
        let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
        // Nautilus' own format, so a cut here moves when pasted there too.
        let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
        let providers = [
            gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()),
            gdk::ContentProvider::for_bytes(
                "x-special/gnome-copied-files",
                &glib::Bytes::from_owned(gnome.into_bytes()),
            ),
            gdk::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from_owned((uris.join("\r\n") + "\r\n").into_bytes()),
            ),
        ];
        let provider = gdk::ContentProvider::new_union(&providers);
        if let Err(e) = self.window.clipboard().set_content(Some(&provider)) {
            self.show_message(&e.to_string());
            return;
        }
        let n = files.len();
        *self.clip.borrow_mut() = Some((files, cut));
        self.show_message(&format!(
            "{} {n} item{}",
            if cut { "Cut" } else { "Copied" },
            if n == 1 { "" } else { "s" }
        ));
    }

    fn copy_paths(&self) {
        let text: Vec<String> = self
            .selected_files()
            .iter()
            .map(|f| {
                f.path()
                    .map_or_else(|| f.uri().to_string(), |p| p.display().to_string())
            })
            .collect();
        self.window.clipboard().set_text(&text.join("\n"));
    }

    fn paste(self: &Rc<Self>) {
        let dest = self.current.borrow().clone();
        let clipboard = self.window.clipboard();
        if clipboard.is_local() {
            let clip = self.clip.borrow().clone();
            if let Some((files, cut)) = clip {
                if cut {
                    self.clip.replace(None);
                }
                self.run_transfer(files, dest, if cut { Kind::Move } else { Kind::Copy });
                return;
            }
        }
        let win = self.clone();
        glib::spawn_future_local(async move {
            // Another app (or another Abyss/Nautilus): honour its cut marker.
            let gnome = clipboard
                .read_future(&["x-special/gnome-copied-files"], glib::Priority::DEFAULT)
                .await;
            if let Ok((stream, _)) = gnome {
                let mut text = Vec::new();
                while let Ok(chunk) = stream
                    .read_bytes_future(65536, glib::Priority::DEFAULT)
                    .await
                {
                    if chunk.is_empty() {
                        break;
                    }
                    text.extend_from_slice(&chunk);
                }
                let text = String::from_utf8_lossy(&text);
                let mut lines = text.lines();
                let kind = if lines.next() == Some("cut") {
                    Kind::Move
                } else {
                    Kind::Copy
                };
                let files: Vec<_> = lines
                    .filter(|l| !l.is_empty())
                    .map(gio::File::for_uri)
                    .collect();
                if !files.is_empty() {
                    win.run_transfer(files, dest, kind);
                    return;
                }
            }
            match clipboard
                .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await
            {
                Ok(value) => {
                    if let Ok(list) = value.get::<gdk::FileList>() {
                        win.run_transfer(list.files(), dest, Kind::Copy);
                    }
                }
                Err(_) => win.show_message("Nothing to paste"),
            }
        });
    }

    // ----------------------------------------------------------- file ops --

    fn run_transfer(self: &Rc<Self>, files: Vec<gio::File>, dest: gio::File, kind: Kind) {
        if files.is_empty() {
            return;
        }
        let verb = if kind == Kind::Move {
            "Moving"
        } else {
            "Copying"
        };
        let w = Rc::downgrade(self);
        let report: ops::Report = Rc::new(move |p: &ops::Progress| {
            if let Some(win) = w.upgrade() {
                win.job_label.set_text(&format!(
                    "{verb} {} of {} · {}",
                    (p.done_files + 1).min(p.total_files),
                    p.total_files,
                    p.current
                ));
                win.show_progress(p);
            }
        });
        let w = Rc::downgrade(self);
        let resolve: ops::Resolver = Rc::new(move |conflict| {
            let w = w.clone();
            Box::pin(async move {
                let Some(win) = w.upgrade() else {
                    return (ops::Choice::Cancel, false);
                };
                let (tx, rx) = futures_channel::oneshot::channel();
                win.show_conflict(&conflict, tx);
                drop(win);
                rx.await.unwrap_or((ops::Choice::Cancel, false))
            })
        });
        self.run_job(async move {
            ops::transfer(files, dest, kind, report, resolve)
                .await
                .map(|_| ())
        });
    }

    /// Runs one job at a time with the job bar shown.
    fn run_job(
        self: &Rc<Self>,
        job: impl std::future::Future<Output = Result<(), glib::Error>> + 'static,
    ) {
        if self.busy.get() {
            self.show_message("Another operation is still running");
            return;
        }
        self.busy.set(true);
        self.job_label.set_text("Working…");
        self.job_amount.set_text("");
        self.job_bar.set_fraction(0.0);
        self.job.set_reveal_child(true);
        let w = Rc::downgrade(self);
        let handle = glib::spawn_future_local(async move {
            let result = job.await;
            let Some(win) = w.upgrade() else { return };
            win.job_handle.replace(None);
            win.finish_job();
            match result {
                Err(e) if e.matches(gio::IOErrorEnum::Cancelled) => win.show_message("Cancelled"),
                Err(e) => win.show_message(&e.to_string()),
                Ok(()) => {}
            }
        });
        self.job_handle.replace(Some(handle));
    }

    /// The bar, and how far along: "1.2 GB / 4.0 GB · 30%" (just the
    /// percentage when sizes aren't known).
    fn show_progress(&self, p: &ops::Progress) {
        let fraction = p.fraction();
        self.job_bar.set_fraction(fraction);
        let text = progress_text(p.done_bytes, p.total_bytes, fraction);
        if self.job_amount.text() != text {
            self.job_amount.set_text(&text);
        }
    }

    fn finish_job(&self) {
        self.busy.set(false);
        self.job.set_reveal_child(false);
        self.close_conflict();
    }

    /// Stops the running job; dropping its future cancels the GIO call in
    /// flight (and any conflict question it was waiting on).
    fn cancel_job(&self) {
        if let Some(handle) = self.job_handle.take() {
            handle.abort();
            self.finish_job();
            self.show_message("Cancelled");
        }
    }

    // ----------------------------------------------------------- conflict --

    /// Texts follow nautilus-operations-ui-manager.c.
    fn show_conflict(
        &self,
        c: &ops::Conflict,
        reply: futures_channel::oneshot::Sender<(ops::Choice, bool)>,
    ) {
        let name = ops::display_name(&c.dest);
        let folder = c
            .dest
            .parent()
            .map(|p| ops::display_name(&p))
            .unwrap_or_default();
        let kind = if c.dest_dir { "folder" } else { "file" };
        let title = if c.merge {
            format!("Merge folder “{name}”?")
        } else if !c.can_replace {
            format!("“{name}” is already a folder here")
        } else {
            format!("Replace {kind} “{name}”?")
        };
        let age = match c.dest_newer {
            Some(true) => "A newer",
            Some(false) => "An older",
            None => "Another",
        };
        let extra = if c.merge {
            " Merging asks before replacing any files inside."
        } else if !c.can_replace {
            " Rename what you're copying, or skip it."
        } else {
            " Replacing it will overwrite its content."
        };
        self.conflict_title.set_text(&title);
        self.conflict_detail.set_text(&format!(
            "{age} {kind} with the same name is already in “{folder}”.{extra}"
        ));
        self.conflict_replace
            .set_label(if c.merge { "Merge" } else { "Replace" });
        self.conflict_replace.set_visible(c.can_replace);
        self.conflict_rename.set_label("Rename…");
        self.conflict_entry.set_visible(false);
        self.conflict_entry.set_text(&c.suggested);
        self.conflict_all.set_visible(c.more);
        self.conflict_all.set_active(false);
        self.conflict_reply.replace(Some(reply));
        self.conflict.set_reveal_child(true);
        let target: gtk::Widget = if c.can_replace {
            self.conflict_replace.clone().upcast()
        } else {
            self.conflict_rename.clone().upcast()
        };
        // Already showing (back-to-back conflicts): focus now; otherwise the
        // child-revealed handler does it once the bar is on screen.
        if self.conflict.is_child_revealed() {
            target.grab_focus();
        } else {
            self.conflict_focus.replace(Some(target));
        }
    }

    fn answer_conflict(&self, choice: ops::Choice) {
        let all = self.conflict_all.is_active() && !matches!(choice, ops::Choice::Rename(_));
        if let Some(reply) = self.conflict_reply.take() {
            let _ = reply.send((choice, all));
        }
        self.conflict.set_reveal_child(false);
        self.focus_view();
    }

    /// First click shows the name entry; the second (or Enter) renames.
    fn rename_conflict(&self) {
        if !WidgetExt::is_visible(&self.conflict_entry) {
            let name = self.conflict_entry.text().to_string();
            let stem = crate::filename::extension_start(&name);
            self.conflict_entry.set_visible(true);
            self.conflict_rename.set_label("Rename");
            self.conflict_entry.grab_focus();
            self.conflict_entry
                .select_region(0, char_offset(&name[..stem]));
            return;
        }
        let name = self.conflict_entry.text().to_string();
        if let Err(e) = crate::filename::validate(&name) {
            self.show_message(e);
            return;
        }
        self.answer_conflict(ops::Choice::Rename(name));
    }

    fn close_conflict(&self) {
        self.conflict_reply.replace(None);
        self.conflict.set_reveal_child(false);
    }

    fn trash(self: &Rc<Self>, files: Vec<gio::File>) {
        let win = self.clone();
        let n = files.len();
        glib::spawn_future_local(async move {
            match ops::trash(files.clone()).await {
                Ok(()) => win.show_message(&format!(
                    "Moved {n} item{} to the trash",
                    if n == 1 { "" } else { "s" }
                )),
                // Remote locations often have no trash (nautilus offers the same).
                Err(e) if e.matches(gio::IOErrorEnum::NotSupported) => {
                    win.show_message("This location has no trash");
                    win.confirm_delete(files);
                }
                Err(e) => win.show_message(&e.to_string()),
            }
        });
    }

    fn restore(self: &Rc<Self>, items: Vec<(gio::File, gio::File)>) {
        let win = self.clone();
        glib::spawn_future_local(async move {
            for (item, orig) in items {
                if let Some(parent) = orig.parent() {
                    // Both block on disk (or gvfsd-trash); keep them off the UI thread.
                    let (dir, it) = (parent.clone(), item.clone());
                    let is_dir = gio::spawn_blocking(move || {
                        let _ = dir.make_directory_with_parents(gio::Cancellable::NONE);
                        it.query_file_type(
                            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                            gio::Cancellable::NONE,
                        ) == gio::FileType::Directory
                    })
                    .await
                    .unwrap_or(false);
                    let target =
                        ops::free_target(&parent, &ops::display_name(&orig), is_dir, false).await;
                    let (mv, _) = item.move_future(
                        &target,
                        gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
                        glib::Priority::DEFAULT,
                    );
                    if let Err(e) = mv.await {
                        win.show_message(&e.to_string());
                        return;
                    }
                }
            }
        });
    }

    /// Puts everything in the trash back where it came from.
    fn restore_all(self: &Rc<Self>) {
        let win = self.clone();
        glib::spawn_future_local(async move {
            let trash = gio::File::for_uri("trash:///");
            let items = match ops::trash_items(&trash).await {
                Ok(items) => items,
                Err(e) => return win.show_message(&e.to_string()),
            };
            let n = items.len();
            win.restore(items);
            win.show_message(&format!(
                "Restored {n} item{}",
                if n == 1 { "" } else { "s" }
            ));
        });
    }

    /// Asks for an archive's passphrase in the prompt bar; None if cancelled.
    async fn ask_passphrase(self: &Rc<Self>, name: &str, retry: bool) -> Option<String> {
        let (tx, rx) = futures_channel::oneshot::channel();
        let tx = RefCell::new(Some(tx));
        let label = if retry {
            format!("Wrong password for “{name}”, try again")
        } else {
            format!("“{name}” is password-protected")
        };
        self.ask(&label, Some(""), "Extract", move |_, text| {
            if let Some(tx) = tx.take() {
                let _ = tx.send(text);
            }
        });
        self.prompt_entry.set_visibility(false);
        // Cancel drops the sender, which ends the wait with None.
        rx.await.ok()
    }

    /// Extracts each archive into `dest` (`nautilus_file_operations_extract_files`).
    fn extract(self: &Rc<Self>, archives: Vec<gio::File>, dest: gio::File) {
        let w = Rc::downgrade(self);
        let report: ops::Report = Rc::new(move |p: &ops::Progress| {
            if let Some(win) = w.upgrade() {
                win.job_label.set_text(&format!(
                    "Extracting {} of {} · {}",
                    p.done_files.min(p.total_files),
                    p.total_files,
                    p.current
                ));
                win.show_progress(p);
            }
        });
        let win = self.clone();
        self.run_job(async move {
            for archive in archives {
                let name = ops::display_name(&archive);
                let mut passphrase: Option<String> = None;
                loop {
                    match archive::extract(&archive, &dest, passphrase.as_deref(), &report).await {
                        Ok(_) => break,
                        Err(archive::Error::NeedsPassphrase) => {
                            let retry = passphrase.is_some();
                            passphrase = Some(
                                win.ask_passphrase(&name, retry)
                                    .await
                                    .ok_or_else(ops::cancelled)?,
                            );
                        }
                        Err(archive::Error::Failed(e)) => return Err(e),
                    }
                }
            }
            Ok(())
        });
    }

    /// "Extract To…": the destination is typed in the prompt bar (a path,
    /// `~/…` or a URI), prefilled with the current folder.
    fn extract_to(self: &Rc<Self>, archives: Vec<gio::File>) {
        let here = self.current.borrow().clone();
        let text = here
            .path()
            .map_or_else(|| here.uri().to_string(), |p| p.display().to_string());
        let archives = RefCell::new(archives);
        self.ask("Extract to", Some(&text), "Extract", move |win, text| {
            let dest = win.parse_location(&text);
            win.extract(archives.take(), dest);
        });
        self.prompt_entry.set_position(-1);
    }

    fn confirm_delete(self: &Rc<Self>, files: Vec<gio::File>) {
        let n = files.len();
        let what = if n == 1 {
            format!("“{}”", ops::display_name(&files[0]))
        } else {
            format!("{n} items")
        };
        let files = RefCell::new(files);
        self.ask(
            &format!("Permanently delete {what}?"),
            None,
            "Delete",
            move |win, _| {
                let files = files.take();
                win.run_job(ops::delete(files));
            },
        );
    }

    fn confirm_empty_trash(self: &Rc<Self>) {
        self.ask(
            "Permanently delete everything in the trash?",
            None,
            "Empty",
            |win, _| {
                win.run_job(ops::empty_trash());
            },
        );
    }

    fn start_rename(self: &Rc<Self>) {
        let Ok::<[_; 1], _>([(info, file)]) = self.selected().try_into() else {
            return;
        };
        let name = info.display_name().to_string();
        let stem = if is_dir(&info) {
            name.len()
        } else {
            crate::filename::extension_start(&name)
        };
        let stem_chars = char_offset(&name[..stem]);
        let initial = name.clone();
        self.ask("Rename to", Some(&initial), "Rename", move |win, new| {
            if new == name {
                return;
            }
            let win = win.clone();
            let file = file.clone();
            glib::spawn_future_local(async move {
                if let Err(e) = ops::rename(&file, &new).await {
                    win.show_message(&e.to_string());
                }
            });
        });
        self.prompt_entry.select_region(0, stem_chars);
    }

    fn start_new_folder(self: &Rc<Self>) {
        self.ask("New folder", Some("New Folder"), "Create", |win, name| {
            if let Err(e) = crate::filename::validate(&name) {
                win.show_message(e);
                return;
            }
            let dir = win.current.borrow().clone();
            let win = win.clone();
            glib::spawn_future_local(async move {
                if let Err(e) = ops::new_folder(&dir, &name).await {
                    win.show_message(&e.to_string());
                }
            });
        });
        self.prompt_entry.select_region(0, -1);
    }

    // ------------------------------------------------------------- prompt --

    fn setup_prompt(win: &Rc<Self>) {
        let w = Rc::downgrade(win);
        let accept = move || {
            let Some(win) = w.upgrade() else { return };
            let text = win.prompt_entry.text().to_string();
            let action = win.prompt_action.take();
            win.close_prompt();
            if let Some(action) = action {
                action(&win, text);
            }
        };
        let a = accept.clone();
        win.prompt_ok.connect_clicked(move |_| a());
        win.prompt_entry.connect_activate(move |_| accept());

        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(win);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(win) = w.upgrade() {
                    win.close_prompt();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        win.prompt.add_controller(keys);
    }

    fn ask(
        &self,
        label: &str,
        text: Option<&str>,
        ok: &str,
        action: impl Fn(&Rc<Self>, String) + 'static,
    ) {
        self.prompt_label.set_text(label);
        self.prompt_ok.set_label(ok);
        self.prompt_entry.set_visible(text.is_some());
        self.prompt_entry.set_visibility(true);
        self.prompt_entry.set_text(text.unwrap_or(""));
        if ok == "Delete" || ok == "Empty" {
            self.prompt_ok.add_css_class("destructive-action");
        } else {
            self.prompt_ok.remove_css_class("destructive-action");
        }
        *self.prompt_action.borrow_mut() = Some(Box::new(action));
        self.prompt.set_reveal_child(true);
        if text.is_some() {
            self.prompt_entry.grab_focus();
        } else {
            self.prompt_ok.grab_focus();
        }
    }

    fn close_prompt(&self) {
        self.prompt.set_reveal_child(false);
        self.prompt_action.replace(None);
        self.focus_view();
    }

    // ---------------------------------------------------------- shortcuts --

    /// Steps the icon size by `step` levels (0 resets), restyling the list
    /// and remembering the choice.
    fn zoom_by(self: &Rc<Self>, step: isize) {
        let old = self.zoom.get();
        let new = if step == 0 {
            ZOOM_DEFAULT
        } else {
            old.saturating_add_signed(step).min(ZOOM_LEVELS.len() - 1)
        };
        if new == old {
            return;
        }
        self.views
            .remove_css_class(&format!("zoom-{}", ZOOM_LEVELS[old]));
        self.views
            .add_css_class(&format!("zoom-{}", ZOOM_LEVELS[new]));
        self.zoom.set(new);
        let path = zoom_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, ZOOM_LEVELS[new].to_string());
        // Grid tiles size their thumbnail slot when they're made.
        self.refresh_tiles();
    }

    /// Window-level keys that must work whatever has focus.
    fn capture_key(self: &Rc<Self>, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        let mods = mods & gtk::accelerator_get_default_mod_mask();
        // Typing in a text field (location, rename, sign-in, filter box) keeps
        // its own Esc; anywhere else, including an emptied list, Esc clears
        // an active filter.
        let in_text = RootExt::focus(&self.window).is_some_and(|f| f.is::<gtk::Text>());
        let in_list = !in_text;
        if key == gdk::Key::Escape && mods.is_empty() && in_list && !self.search.text().is_empty() {
            self.search.set_text("");
            return glib::Propagation::Stop;
        }
        // Copy / cut / paste files unless a text field has focus (then they
        // are text clipboard keys). After typing a location the list doesn't
        // always get focus back, so these can't live on the list.
        if !in_text && mods == gdk::ModifierType::CONTROL_MASK {
            match key.to_lower() {
                gdk::Key::c => self.copy_selection(false),
                gdk::Key::x => self.copy_selection(true),
                gdk::Key::v => self.paste(),
                _ => return glib::Propagation::Proceed,
            }
            return glib::Propagation::Stop;
        }
        // Ctrl+1 / Ctrl+2: list / grid view.
        if mods == gdk::ModifierType::CONTROL_MASK && !in_text {
            let view = match key {
                gdk::Key::_1 | gdk::Key::KP_1 => Some(Mode::List),
                gdk::Key::_2 | gdk::Key::KP_2 => Some(Mode::Grid),
                _ => None,
            };
            if let Some(view) = view {
                self.set_view(view, self.grouped.get());
                return glib::Propagation::Stop;
            }
        }
        // Zoom: Ctrl+plus / Ctrl+minus / Ctrl+0 (with or without Shift for +).
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK)
            && (mods - gdk::ModifierType::CONTROL_MASK - gdk::ModifierType::SHIFT_MASK).is_empty();
        if ctrl && !in_text {
            let step = match key {
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => Some(1),
                gdk::Key::minus | gdk::Key::underscore | gdk::Key::KP_Subtract => Some(-1),
                gdk::Key::_0 | gdk::Key::KP_0 => Some(0),
                _ => None,
            };
            if let Some(step) = step {
                self.zoom_by(step);
                return glib::Propagation::Stop;
            }
        }
        // Ctrl+Shift+N: new folder (keyval may arrive as n or N).
        if key.to_lower() == gdk::Key::n
            && mods == gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
        {
            self.start_new_folder();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }

    fn setup_shortcuts(win: &Rc<Self>) {
        fn add(controller: &gtk::ShortcutController, w: &Weak<Win>, accel: &str, f: fn(&Rc<Win>)) {
            let w = w.clone();
            let action = gtk::CallbackAction::new(move |_, _| {
                if let Some(win) = w.upgrade() {
                    f(&win);
                }
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(accel),
                Some(action),
            ));
        }
        let w = Rc::downgrade(win);

        // Window-wide.
        let global = gtk::ShortcutController::new();
        add(&global, &w, "<Alt>Left", Self::go_back);
        add(&global, &w, "<Alt>Right", Self::go_forward);
        add(&global, &w, "<Alt>Up", Self::go_up);
        add(&global, &w, "<Alt>Home", |win| {
            win.navigate(gio::File::for_path(glib::home_dir()), true);
        });
        add(&global, &w, "<Control>l", |win| win.show_location());
        add(&global, &w, "<Control>h", |win| {
            win.set_show_hidden(!win.show_hidden.get());
        });
        add(&global, &w, "<Control><Shift>n", Self::start_new_folder);
        add(&global, &w, "F5", Self::reload);
        add(&global, &w, "<Control>r", Self::reload);
        add(&global, &w, "<Control>f", |win| {
            win.search.grab_focus();
        });
        add(&global, &w, "<Control>w", |win| win.window.close());
        add(&global, &w, "<Control>n", |win| {
            if let Some(app) = win.window.application() {
                Self::open(&app, Some(win.current.borrow().clone()));
            }
        });
        win.window.add_controller(global);

        // Only while the file list has focus, so entries keep Ctrl+C etc.
        let local = gtk::ShortcutController::new();
        add(&local, &w, "Delete", |win| {
            let files = win.selected_files();
            if files.is_empty() {
                return;
            }
            if win.current.borrow().uri_scheme().as_deref() == Some("trash") {
                win.confirm_delete(files);
            } else {
                win.trash(files);
            }
        });
        add(&local, &w, "<Shift>Delete", |win| {
            let files = win.selected_files();
            if !files.is_empty() {
                win.confirm_delete(files);
            }
        });
        add(&local, &w, "F2", Self::start_rename);
        add(&local, &w, "BackSpace", Self::go_up);
        add(&local, &w, "Menu", |win| win.show_file_menu(40.0, 40.0));
        win.views.add_controller(local);

        // Esc and Ctrl+Shift+N never reached the shortcut controllers above
        // (the filter's key capture and the list's own key handling get them
        // first), so catch them on the window in the capture phase.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ww = w;
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(win) = ww.upgrade() else {
                return glib::Propagation::Proceed;
            };
            win.capture_key(key, mods)
        });
        win.window.add_controller(keys);
    }

    // ------------------------------------------------------------ sidebar --

    fn setup_sidebar(win: &Rc<Self>) {
        win.sidebar.set_header_func(|row, before| {
            let section = row.widget_name();
            let same = before.is_some_and(|b| b.widget_name() == section);
            if same {
                row.set_header(None::<&gtk::Widget>);
            } else {
                let label = gtk::Label::new(Some(&section));
                label.set_xalign(0.0);
                label.add_css_class("section");
                row.set_header(Some(&label));
            }
        });

        let w = Rc::downgrade(win);
        win.sidebar.connect_row_activated(move |_, row| {
            let Some(win) = w.upgrade() else { return };
            let Some(index) = win.place_index(row.index()) else {
                return;
            };
            let place = win.places.borrow().get(index).cloned();
            if let Some(place) = place {
                win.open_place(&place);
            }
        });

        let w = Rc::downgrade(win);
        let refresh = move || {
            if let Some(win) = w.upgrade() {
                win.refresh_sidebar();
            }
        };
        let r = refresh.clone();
        win.monitor.connect_mount_added(move |_, _| r());
        let r = refresh.clone();
        win.monitor.connect_mount_removed(move |_, _| r());
        let r = refresh.clone();
        win.monitor.connect_mount_changed(move |_, _| r());
        let r = refresh.clone();
        win.monitor.connect_volume_added(move |_, _| r());
        let r = refresh;
        win.monitor.connect_volume_removed(move |_, _| r());
    }

    fn open_place(self: &Rc<Self>, place: &Place) {
        match &place.target {
            Target::Uri(uri) => self.navigate(gio::File::for_uri(uri), true),
            Target::Prompt(prefix) => self.prompt_location(prefix),
            Target::Volume(volume) => {
                let op = self.auth.operation(None);
                let win = self.clone();
                let volume = volume.clone();
                glib::spawn_future_local(async move {
                    match volume
                        .mount_future(gio::MountMountFlags::NONE, Some(&op))
                        .await
                    {
                        Ok(()) => {
                            if let Some(root) = volume.get_mount().map(|m| m.root()) {
                                win.navigate(root, true);
                            }
                        }
                        Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => {}
                        Err(e) => win.show_message(&e.to_string()),
                    }
                });
            }
        }
    }

    pub fn refresh_sidebar(self: &Rc<Self>) {
        let mut all = places::fixed_places();
        all.extend(places::volumes(&self.monitor));
        all.extend(self.peers.borrow().iter().cloned());
        // A synced folder that is already a place gets a badge there
        // instead of a second row under Synced.
        for mut synced in places::syncthing_folders() {
            let Target::Uri(uri) = &synced.target else {
                continue;
            };
            let existing = all.iter_mut().find(|p| {
                p.section == Section::Places
                    && matches!(&p.target, Target::Uri(u) if u.trim_end_matches('/') == uri.trim_end_matches('/'))
            });
            if let Some(place) = existing {
                place.synced = true;
            } else {
                synced.synced = true;
                all.push(synced);
            }
        }
        places::apply_order(&mut all, &places::load_order());

        while let Some(child) = self.sidebar.first_child() {
            self.sidebar.remove(&child);
        }
        for place in &all {
            // The bookmark drop strip sits right above Trash.
            if matches!(&place.target, Target::Uri(u) if u == "trash:///") {
                self.sidebar.append(&self.bookmark_drop);
            }
            self.sidebar.append(&self.sidebar_row(place));
        }
        *self.places.borrow_mut() = all;
        self.select_sidebar_row();
    }

    fn sidebar_row(self: &Rc<Self>, place: &Place) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(place.section.title());
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.append(&gtk::Image::from_icon_name(&place.icon));
        let label = gtk::Label::new(Some(&place.name));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&label);
        if place.synced && place.section != Section::Synced {
            let badge = gtk::Image::from_icon_name("emblem-synchronizing-symbolic");
            badge.add_css_class("synced-badge");
            badge.set_tooltip_text(Some("Synced by Syncthing"));
            line.append(&badge);
        }
        if place.dim {
            line.add_css_class("offline");
            row.set_tooltip_text(Some("Offline"));
        }
        if let Some(mount) = &place.mount {
            let eject = icon_button(
                "media-eject-symbolic",
                if mount.can_eject() {
                    "Eject"
                } else {
                    "Unmount"
                },
            );
            let mount = mount.clone();
            let w = Rc::downgrade(self);
            eject.connect_clicked(move |_| {
                if let Some(win) = w.upgrade() {
                    win.unmount(mount.clone());
                }
            });
            line.append(&eject);
        }
        row.set_child(Some(&line));
        if let Target::Uri(uri) = &place.target {
            let uri = uri.clone();
            Self::attach_drop(
                &Rc::downgrade(self),
                &row,
                Rc::new(move || Some(gio::File::for_uri(&uri))),
                false,
            );
        }

        if place.pin == Pin::None {
            self.attach_reorder(&row, place);
        }

        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        let w = Rc::downgrade(self);
        let r = row.downgrade();
        let place = place.clone();
        click.connect_pressed(move |_, _, x, y| {
            let (Some(win), Some(row)) = (w.upgrade(), r.upgrade()) else {
                return;
            };
            let items = Self::place_menu_items(&place);
            if !items.is_empty() {
                win.show_place_menu(&row, x, y, items);
            }
        });
        row.add_controller(click);
        row
    }

    /// Drag a row within its section to reorder it. The payload is a
    /// `GtkStringObject`, which only these targets accept, so it can't be
    /// mistaken for a file or text drop.
    fn attach_reorder(self: &Rc<Self>, row: &gtk::ListBoxRow, place: &Place) {
        let payload = format!("{}\u{1f}{}", place.section.title(), place.key());
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::MOVE);
        let r = row.downgrade();
        source.connect_prepare(move |source, _, _| {
            if let Some(row) = r.upgrade() {
                source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row))), 0, 0);
            }
            let item = gtk::StringObject::new(&payload);
            Some(gdk::ContentProvider::for_value(&item.to_value()))
        });
        row.add_controller(source);

        let target = gtk::DropTarget::new(gtk::StringObject::static_type(), gdk::DragAction::MOVE);
        target.set_preload(true);
        let section = place.section;
        let key = place.key();
        // Whether the drop lands below this row, and whether it's allowed.
        let below = Rc::new(Cell::new(false));
        let mark = {
            let below = below.clone();
            let key = key.clone();
            move |t: &gtk::DropTarget, y: f64| -> gdk::DragAction {
                let Some(row) = t.widget() else {
                    return gdk::DragAction::empty();
                };
                let dragged = t
                    .value()
                    .and_then(|v| v.get::<gtk::StringObject>().ok())
                    .map(|o| o.string().to_string());
                let ok = dragged
                    .as_deref()
                    .and_then(|d| d.split_once('\u{1f}'))
                    .is_some_and(|(s, k)| s == section.title() && k != key);
                row.remove_css_class("drop-above");
                row.remove_css_class("drop-below");
                if !ok {
                    return gdk::DragAction::empty();
                }
                below.set(y > f64::from(row.height()) / 2.0);
                row.add_css_class(if below.get() {
                    "drop-below"
                } else {
                    "drop-above"
                });
                gdk::DragAction::MOVE
            }
        };
        let m = mark.clone();
        target.connect_enter(move |t, _, y| m(t, y));
        target.connect_motion(move |t, _, y| mark(t, y));
        target.connect_leave(|t| {
            if let Some(row) = t.widget() {
                row.remove_css_class("drop-above");
                row.remove_css_class("drop-below");
            }
        });
        let w = Rc::downgrade(self);
        target.connect_drop(move |t, value, _, _| {
            if let Some(row) = t.widget() {
                row.remove_css_class("drop-above");
                row.remove_css_class("drop-below");
            }
            let Some(win) = w.upgrade() else { return false };
            let Ok(item) = value.get::<gtk::StringObject>() else {
                return false;
            };
            let item = item.string().to_string();
            let Some((s, dragged)) = item.split_once('\u{1f}') else {
                return false;
            };
            if s != section.title() || dragged == key {
                return false;
            }
            let (dragged, key, below) = (dragged.to_owned(), key.clone(), below.get());
            // Rebuilding the sidebar destroys this row; do it after the drop.
            glib::idle_add_local_once(move || win.move_place(section, &dragged, &key, below));
            true
        });
        row.add_controller(target);
    }

    /// Moves `dragged` next to `anchor` within `section` and saves the order.
    fn move_place(self: &Rc<Self>, section: Section, dragged: &str, anchor: &str, below: bool) {
        let mut keys: Vec<String> = self
            .places
            .borrow()
            .iter()
            .filter(|p| p.section == section && p.pin == Pin::None)
            .map(Place::key)
            .collect();
        let Some(from) = keys.iter().position(|k| k == dragged) else {
            return;
        };
        let item = keys.remove(from);
        let Some(to) = keys.iter().position(|k| k == anchor) else {
            return;
        };
        keys.insert(if below { to + 1 } else { to }, item);
        if let Err(e) = places::save_order(section, &keys) {
            self.show_message(&format!("Couldn't save sidebar order: {e}"));
        }
        if section == Section::Places {
            let uris: Vec<String> = self
                .places
                .borrow()
                .iter()
                .filter_map(|p| match &p.target {
                    Target::Uri(u) => Some(u.clone()),
                    _ => None,
                })
                .collect();
            let ordered: Vec<String> = keys
                .iter()
                .filter_map(|k| uris.iter().find(|u| u.trim_end_matches('/') == k).cloned())
                .collect();
            let _ = places::reorder_bookmarks(&ordered);
        }
        self.refresh_sidebar();
    }

    /// Right-click entries for a sidebar row, built when the menu opens.
    fn place_menu_items(place: &Place) -> Vec<PlaceItem> {
        let mut items: Vec<PlaceItem> = place
            .alternates
            .iter()
            .map(|(label, uri)| {
                let uri = uri.clone();
                PlaceItem::new(label, true, move |win| {
                    win.navigate(gio::File::for_uri(&uri), true);
                })
            })
            .collect();
        let Target::Uri(uri) = &place.target else {
            return items;
        };
        if uri == "trash:///" {
            let full = places::trash_count() > 0;
            items.push(PlaceItem::new("Restore All", full, Self::restore_all));
            items.push(PlaceItem::new(
                "Empty Trash…",
                full,
                Self::confirm_empty_trash,
            ));
        } else if place.section == Section::Places && places::is_bookmark(uri) {
            let uri = uri.clone();
            items.push(PlaceItem::new("Remove Bookmark", true, move |win| {
                let _ = places::remove_bookmark(&uri);
                win.refresh_sidebar();
            }));
        }
        items
    }

    fn show_place_menu(
        self: &Rc<Self>,
        row: &gtk::ListBoxRow,
        x: f64,
        y: f64,
        items: Vec<PlaceItem>,
    ) {
        let popover = gtk::Popover::new();
        popover.add_css_class("menu");
        popover.set_has_arrow(false);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for item in items {
            let button = gtk::Button::with_label(&item.label);
            button.add_css_class("flat");
            button.set_sensitive(item.enabled);
            if let Some(l) = button.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            let w = Rc::downgrade(self);
            let pop = popover.downgrade();
            let run = item.run;
            button.connect_clicked(move |_| {
                if let Some(p) = pop.upgrade() {
                    p.popdown();
                }
                if let Some(win) = w.upgrade() {
                    run(&win);
                }
            });
            column.append(&button);
        }
        popover.set_child(Some(&column));
        // Parent to the sidebar box, not the row: refresh_sidebar() rebuilds
        // rows (e.g. on focus), and a row finalized with a popover still
        // attached crashes GTK.
        let Some(anchor) = self.sidebar.parent() else {
            return;
        };
        let (x, y) = translate(row, &anchor, x, y);
        popover.set_parent(&anchor);
        popover.set_pointing_to(Some(&spot(x, y)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn unmount(self: &Rc<Self>, mount: gio::Mount) {
        // Leave the mount first if we're inside it.
        let root = mount.root();
        let inside = {
            let cur = self.current.borrow();
            cur.equal(&root) || cur.has_prefix(&root)
        };
        if inside {
            self.navigate(gio::File::for_path(glib::home_dir()), true);
        }
        let op = self.auth.operation(Some(&root.uri()));
        let win = self.clone();
        glib::spawn_future_local(async move {
            let result = if mount.can_eject() {
                mount
                    .eject_with_operation_future(gio::MountUnmountFlags::NONE, Some(&op))
                    .await
            } else {
                mount
                    .unmount_with_operation_future(gio::MountUnmountFlags::NONE, Some(&op))
                    .await
            };
            if let Err(e) = result {
                if !e.matches(gio::IOErrorEnum::FailedHandled) {
                    win.show_message(&e.to_string());
                }
            }
        });
    }

    fn refresh_peers(self: &Rc<Self>) {
        let fresh = self
            .peers_checked
            .get()
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(30));
        if fresh || !places::have_command("tailscale") {
            return;
        }
        self.peers_checked.set(Some(std::time::Instant::now()));
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let peers = places::tailscale_peers().await;
            if let Some(win) = w.upgrade() {
                // The sidebar is rebuilt only when something changed.
                let key = |p: &Place| {
                    let uri = match &p.target {
                        Target::Uri(u) | Target::Prompt(u) => u.clone(),
                        Target::Volume(_) => String::new(),
                    };
                    (p.name.clone(), uri, p.dim, p.alternates.clone())
                };
                let same = win.peers.borrow().iter().map(key).eq(peers.iter().map(key));
                if same {
                    return;
                }
                *win.peers.borrow_mut() = peers;
                win.refresh_sidebar();
            }
        });
    }

    /// Highlights the sidebar row for the current location, if any.
    /// Index into `places` for a sidebar row, skipping the bookmark strip.
    fn place_index(&self, row_index: i32) -> Option<usize> {
        let strip = self.bookmark_drop.index();
        match row_index {
            i if i == strip => None,
            i if strip >= 0 && i > strip => usize::try_from(i - 1).ok(),
            i => usize::try_from(i).ok(),
        }
    }

    fn select_sidebar_row(&self) {
        let current = self.current.borrow().uri().to_string();
        let index = self.places.borrow().iter().position(|p| match &p.target {
            Target::Uri(u) => u.trim_end_matches('/') == current.trim_end_matches('/'),
            _ => false,
        });
        let strip = self.bookmark_drop.index();
        let row_index = index.and_then(|i| i32::try_from(i).ok()).map(|i| {
            if strip >= 0 && i >= strip {
                i + 1
            } else {
                i
            }
        });
        match row_index.and_then(|i| self.sidebar.row_at_index(i)) {
            Some(row) => self.sidebar.select_row(Some(&row)),
            None => self.sidebar.unselect_all(),
        }
    }

    // ------------------------------------------------------------- footer --

    fn update_status(&self) {
        let total = self.sorted.n_items();
        let state = self.search_state.get();
        let loading = self.list.borrow().is_loading() || state == SearchState::Running;
        self.empty.set_visible(total == 0 && !loading);
        self.empty.set_text(if self.search.text().is_empty() {
            "Nothing here"
        } else {
            "No matches"
        });
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
        self.status.set_text(&text);
    }

    fn show_message(&self, text: &str) {
        self.message.set_text(text);
        let seq = self.message_seq.get().wrapping_add(1);
        self.message_seq.set(seq);
        if text.is_empty() {
            return;
        }
        let label = self.message.downgrade();
        let latest = self.message_seq.clone();
        glib::timeout_add_seconds_local_once(6, move || {
            // Only clear if no newer message replaced this one.
            if let (Some(l), true) = (label.upgrade(), latest.get() == seq) {
                l.set_text("");
            }
        });
    }
}

struct PlaceItem {
    label: String,
    enabled: bool,
    run: WinAction,
}

impl PlaceItem {
    fn new(label: &str, enabled: bool, run: impl Fn(&Rc<Win>) + 'static) -> Self {
        Self {
            label: label.to_owned(),
            enabled,
            run: Box::new(run),
        }
    }
}

enum MenuItem {
    Separator,
    Action(String, Box<dyn FnOnce()>),
}

fn first_selected(selection: &gtk::MultiSelection) -> Option<u32> {
    let set = selection.selection();
    (!set.is_empty()).then(|| set.nth(0))
}

async fn info_target(file: &gio::File) -> Option<String> {
    file.query_info_future(
        "standard::target-uri",
        gio::FileQueryInfoFlags::NONE,
        glib::Priority::DEFAULT,
    )
    .await
    .ok()?
    .attribute_string("standard::target-uri")
    .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_text_reads_naturally() {
        assert_eq!(
            progress_text(1_200_000_000, 4_000_000_000, 0.3),
            "1.2\u{a0}GB / 4.0\u{a0}GB · 30%"
        );
        // Rounds down: 100% only when done.
        assert_eq!(
            progress_text(999, 1000, 0.999),
            "999 bytes / 1.0\u{a0}kB · 99%"
        );
        assert_eq!(progress_text(5, 0, 0.5), "50%");
    }
}
