//! A folder's files as a list model, kept current as they change.
//!
//! Does what `gtk::DirectoryList` does, but applies changes in batches and
//! finds files by name: GTK walks its whole list for every file that
//! appears, changes or goes, so a few thousand new files took seconds.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};

/// How long changes gather before they're applied together.
const BATCH: Duration = Duration::from_millis(30);

/// Past this many changed files a batch reads the folder again, in a few
/// big requests, instead of asking about each file.
const REREAD_OVER: usize = 32;

/// What changed about a listing, for its watcher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notice {
    Loading,
    Error,
}

type Watcher = Rc<dyn Fn(&Rc<Listing>, Notice)>;

pub struct Listing {
    store: gio::ListStore,
    file: RefCell<Option<gio::File>>,
    attributes: RefCell<String>,
    loading: Cell<bool>,
    error: RefCell<Option<glib::Error>>,
    /// The listing read in progress, and the change monitor.
    task: RefCell<Option<glib::JoinHandle<()>>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    /// Each shown file by name.
    by_name: RefCell<HashMap<PathBuf, gio::FileInfo>>,
    /// Changes waiting to be applied, by name: the file to look at again,
    /// or None if it's gone.
    pending: RefCell<HashMap<PathBuf, Option<gio::File>>>,
    /// A batch is scheduled or being applied.
    flushing: Cell<bool>,
    /// Bumped by each `load`, so a late result from before is dropped.
    generation: Cell<u64>,
    watcher: RefCell<Option<Watcher>>,
    this: RefCell<Weak<Self>>,
}

impl Listing {
    pub fn new() -> Rc<Self> {
        let listing = Rc::new(Self {
            store: gio::ListStore::new::<gio::FileInfo>(),
            file: RefCell::default(),
            attributes: RefCell::default(),
            loading: Cell::new(false),
            error: RefCell::default(),
            task: RefCell::default(),
            monitor: RefCell::default(),
            by_name: RefCell::default(),
            pending: RefCell::default(),
            flushing: Cell::new(false),
            generation: Cell::new(0),
            watcher: RefCell::default(),
            this: RefCell::default(),
        });
        listing.this.replace(Rc::downgrade(&listing));
        listing
    }

    /// The files, as `gio::FileInfo`s carrying their `standard::file`.
    pub const fn model(&self) -> &gio::ListStore {
        &self.store
    }

    pub fn file(&self) -> Option<gio::File> {
        self.file.borrow().clone()
    }

    pub const fn is_loading(&self) -> bool {
        self.loading.get()
    }

    pub fn error(&self) -> Option<glib::Error> {
        self.error.borrow().clone()
    }

    pub fn n_items(&self) -> u32 {
        self.store.n_items()
    }

    /// Calls `watch` when loading starts or ends, or an error comes in.
    pub fn watch(&self, watch: impl Fn(&Rc<Self>, Notice) + 'static) {
        self.watcher.replace(Some(Rc::new(watch)));
    }

    fn notify(&self, notice: Notice) {
        let watcher = self.watcher.borrow().clone();
        if let (Some(watch), Some(this)) = (watcher, self.this.borrow().upgrade()) {
            watch(&this, notice);
        }
    }

    /// Reads `file` (empties the listing for None) with `attributes`, and
    /// follows its changes from then on.
    pub fn load(&self, file: Option<&gio::File>, attributes: &str) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(monitor) = self.monitor.take() {
            monitor.cancel();
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.store.remove_all();
        self.by_name.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        self.flushing.set(false);
        self.error.replace(None);
        self.file.replace(file.cloned());
        attributes.clone_into(&mut self.attributes.borrow_mut());
        let Some(file) = file.cloned() else {
            if self.loading.replace(false) {
                self.notify(Notice::Loading);
            }
            return;
        };
        // Watched from the start, so nothing that changes during the read
        // is missed; changes and read entries meet by name.
        if let Ok(monitor) =
            file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            let this = self.this.borrow().clone();
            monitor.connect_changed(move |_, file, other, event| {
                if let Some(listing) = this.upgrade() {
                    listing.changed(file, other, event);
                }
            });
            self.monitor.replace(Some(monitor));
        }
        self.loading.set(true);
        self.notify(Notice::Loading);
        let this = self.this.borrow().clone();
        let attributes = attributes.to_owned();
        let task = glib::spawn_future_local(async move {
            let result = read(&file, &attributes, |infos| {
                if let Some(listing) = this.upgrade() {
                    listing.add_read(infos);
                }
            })
            .await;
            let Some(listing) = this.upgrade() else {
                return;
            };
            if listing.generation.get() != generation {
                return;
            }
            listing.task.replace(None);
            listing.loading.set(false);
            if let Err(error) = result {
                listing.error.replace(Some(error));
                listing.notify(Notice::Error);
            }
            listing.notify(Notice::Loading);
        });
        self.task.replace(Some(task));
    }

    /// Appends entries just read, unless a change already brought them in.
    fn add_read(&self, infos: Vec<gio::FileInfo>) {
        let mut by_name = self.by_name.borrow_mut();
        let fresh: Vec<gio::FileInfo> = infos
            .into_iter()
            .filter(|info| {
                let name = info.name();
                if by_name.contains_key(&name) {
                    return false;
                }
                by_name.insert(name, info.clone());
                true
            })
            .collect();
        drop(by_name);
        self.store.splice(self.store.n_items(), 0, &fresh);
    }

    fn changed(&self, file: &gio::File, other: Option<&gio::File>, event: gio::FileMonitorEvent) {
        use gio::FileMonitorEvent as E;
        let name = |f: &gio::File| f.basename();
        {
            let mut pending = self.pending.borrow_mut();
            match event {
                E::Created | E::MovedIn | E::AttributeChanged | E::ChangesDoneHint => {
                    if let Some(n) = name(file) {
                        pending.insert(n, Some(file.clone()));
                    }
                }
                E::Deleted | E::MovedOut => {
                    if let Some(n) = name(file) {
                        pending.insert(n, None);
                    }
                }
                E::Renamed => {
                    if let Some(n) = name(file) {
                        pending.insert(n, None);
                    }
                    if let Some((other, n)) = other.and_then(|o| Some((o, name(o)?))) {
                        pending.insert(n, Some(other.clone()));
                    }
                }
                _ => return,
            }
        }
        self.schedule_flush();
    }

    fn schedule_flush(&self) {
        if self.flushing.replace(true) {
            return;
        }
        let this = self.this.borrow().clone();
        let generation = self.generation.get();
        glib::spawn_future_local(async move {
            glib::timeout_future(BATCH).await;
            let Some(listing) = this.upgrade() else {
                return;
            };
            if listing.generation.get() == generation {
                listing.flush(generation).await;
            }
        });
    }

    /// Applies the gathered changes: looks again at the files that came or
    /// changed, then updates the list in one pass.
    async fn flush(self: Rc<Self>, generation: u64) {
        let batch = std::mem::take(&mut *self.pending.borrow_mut());
        let Some(folder) = self.file() else {
            self.flushing.set(false);
            return;
        };
        let attributes = self.attributes.borrow().clone();
        let wanted: Vec<(PathBuf, gio::File)> = batch
            .iter()
            .filter_map(|(name, file)| Some((name.clone(), file.clone()?)))
            .collect();
        let mut found: HashMap<PathBuf, gio::FileInfo> = HashMap::with_capacity(wanted.len());
        if wanted.len() > REREAD_OVER {
            let _ = read(&folder, &attributes, |infos| {
                found.extend(
                    infos
                        .into_iter()
                        .map(|info| (info.name(), info))
                        .filter(|(name, _)| matches!(batch.get(name), Some(Some(_)))),
                );
            })
            .await;
        } else {
            for (name, file) in wanted {
                if let Ok(info) = file
                    .query_info_future(
                        &attributes,
                        gio::FileQueryInfoFlags::NONE,
                        glib::Priority::LOW,
                    )
                    .await
                {
                    info.set_attribute_object("standard::file", &file);
                    found.insert(name, info);
                }
            }
        }
        if self.generation.get() != generation {
            return;
        }
        self.apply(&batch, found);
        self.flushing.set(false);
        if !self.pending.borrow().is_empty() {
            self.schedule_flush();
        }
    }

    fn apply(
        &self,
        batch: &HashMap<PathBuf, Option<gio::File>>,
        mut found: HashMap<PathBuf, gio::FileInfo>,
    ) {
        // Files that went, or vanished again before they could be read.
        let mut gone: Vec<PathBuf> = Vec::new();
        let mut by_name = self.by_name.borrow_mut();
        for name in batch.keys() {
            if !found.contains_key(name) && by_name.contains_key(name) {
                gone.push(name.clone());
            }
        }
        // Changed files are swapped in place, new ones go at the end.
        let mut replace: HashMap<gio::FileInfo, gio::FileInfo> = HashMap::new();
        let mut added: Vec<gio::FileInfo> = Vec::new();
        for (name, info) in found.drain() {
            match by_name.insert(name, info.clone()) {
                Some(old) => {
                    replace.insert(old, info);
                }
                None => added.push(info),
            }
        }
        let removed: std::collections::HashSet<gio::FileInfo> = gone
            .iter()
            .filter_map(|name| by_name.remove(name))
            .collect();
        drop(by_name);
        if !removed.is_empty() || !replace.is_empty() {
            self.rewrite(&removed, &replace);
        }
        if !added.is_empty() {
            self.store.splice(self.store.n_items(), 0, &added);
        }
    }

    /// One pass over the list: drops `removed`, swaps in `replace`, as few
    /// runs of changes as possible so the views redo little.
    fn rewrite(
        &self,
        removed: &std::collections::HashSet<gio::FileInfo>,
        replace: &HashMap<gio::FileInfo, gio::FileInfo>,
    ) {
        let n = self.store.n_items();
        // Collect runs first: the store can't change while it's read.
        let mut runs: Vec<(u32, u32, Vec<gio::FileInfo>)> = Vec::new();
        let mut i = 0;
        while i < n {
            let Some(item) = self.store.item(i).and_downcast::<gio::FileInfo>() else {
                i += 1;
                continue;
            };
            if !removed.contains(&item) && !replace.contains_key(&item) {
                i += 1;
                continue;
            }
            let start = i;
            let mut with = Vec::new();
            while i < n {
                let Some(item) = self.store.item(i).and_downcast::<gio::FileInfo>() else {
                    break;
                };
                if removed.contains(&item) {
                } else if let Some(new) = replace.get(&item) {
                    with.push(new.clone());
                } else {
                    break;
                }
                i += 1;
            }
            runs.push((start, i - start, with));
        }
        // Back to front, so earlier positions still hold.
        for (start, len, with) in runs.into_iter().rev() {
            self.store.splice(start, len, &with);
        }
    }
}

impl Drop for Listing {
    /// A listing let go (an old one past the recent limit) stops reading
    /// and watching at once.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(monitor) = self.monitor.take() {
            monitor.cancel();
        }
    }
}

/// Reads `folder`'s entries in big requests, handing each batch to `got`.
async fn read(
    folder: &gio::File,
    attributes: &str,
    mut got: impl FnMut(Vec<gio::FileInfo>),
) -> Result<(), glib::Error> {
    let entries = folder
        .enumerate_children_future(
            attributes,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::LOW,
        )
        .await?;
    // Local folders answer fast, so ask for many at once (as GTK does).
    let per_request = if folder.is_native() { 3200 } else { 64 };
    // The next batch is asked for before this one is handed over, so the
    // views take it in while the next one is read (on a network share,
    // each takes about as long as the other).
    // (The callback form starts at once; a future would wait to be polled.)
    let ask = || {
        let (tx, rx) = futures_channel::oneshot::channel();
        entries.next_files_async(
            per_request,
            glib::Priority::LOW,
            gio::Cancellable::NONE,
            move |result| {
                let _ = tx.send(result);
            },
        );
        rx
    };
    let mut next = ask();
    loop {
        let infos = next
            .await
            .map_err(|_| glib::Error::new(gio::IOErrorEnum::Cancelled, "Listing stopped"))??;
        if infos.is_empty() {
            break;
        }
        next = ask();
        for info in &infos {
            info.set_attribute_object("standard::file", &entries.child(info));
        }
        got(infos);
    }
    let _ = entries.close_future(glib::Priority::LOW).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names and sizes shown, sorted.
    fn shown(listing: &Listing) -> Vec<(String, i64)> {
        let mut all: Vec<(String, i64)> = (0..listing.n_items())
            .filter_map(|i| listing.model().item(i).and_downcast::<gio::FileInfo>())
            .map(|info| (info.name().to_string_lossy().into_owned(), info.size()))
            .collect();
        all.sort();
        all
    }

    /// Names and sizes on disk, sorted.
    fn on_disk(dir: &std::path::Path) -> Vec<(String, i64)> {
        let mut all: Vec<(String, i64)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let size = i64::try_from(e.metadata().unwrap().len()).unwrap();
                (e.file_name().to_string_lossy().into_owned(), size)
            })
            .collect();
        all.sort();
        all
    }

    /// Waits (up to a few seconds) for the listing to show what's on disk.
    async fn settles(listing: &Listing, dir: &std::path::Path) -> bool {
        for _ in 0..300 {
            glib::timeout_future(Duration::from_millis(10)).await;
            if !listing.is_loading() && shown(listing) == on_disk(dir) {
                return true;
            }
        }
        false
    }

    #[test]
    fn follows_changes() {
        let dir = std::env::temp_dir().join(format!("abyss-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            std::fs::write(dir.join(format!("start{i}")), "hi").unwrap();
        }
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            ctx.block_on(async {
                let listing = Listing::new();
                let attributes = "standard::name,standard::size,standard::type";
                listing.load(Some(&gio::File::for_path(&dir)), attributes);
                assert!(settles(&listing, &dir).await, "first read");
                // Many at once (read again in one go), half gone again.
                for i in 0..500 {
                    std::fs::write(dir.join(format!("f{i:03}")), "").unwrap();
                }
                for i in (0..500).step_by(2) {
                    std::fs::remove_file(dir.join(format!("f{i:03}"))).unwrap();
                }
                assert!(settles(&listing, &dir).await, "burst");
                // A few (asked about one by one): renamed, grown, removed.
                std::fs::rename(dir.join("f001"), dir.join("renamed")).unwrap();
                std::fs::write(dir.join("start0"), "a longer text").unwrap();
                std::fs::remove_file(dir.join("start1")).unwrap();
                assert!(settles(&listing, &dir).await, "small changes");
                listing.load(None, "");
                assert_eq!(listing.n_items(), 0);
            });
        })
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
