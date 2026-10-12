//! Copy, move, trash and delete. A much smaller take on
//! src/nautilus-file-operations.c: no conflict dialogs (targets get a
//! "(2)" or "(Copy)" name instead), one job at a time, progress by bytes.
//!
//! Inside a job the work runs in parallel: local trees are measured and
//! deleted with plain syscalls on every core, and copies, trashing and
//! remote operations keep several GIO requests in flight. Files with no
//! name clash go at once while clashes are asked about one by one.

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use futures_util::{stream, StreamExt, TryStreamExt};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::filename;
use crate::par;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Copy,
    Move,
}

#[derive(Default, Clone)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub done_files: u64,
    pub total_files: u64,
    pub current: String,
}

impl Progress {
    // Byte counts only lose precision past 2^52 (4 PiB); fine for a bar.
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            if self.total_files == 0 {
                0.0
            } else {
                self.done_files as f64 / self.total_files as f64
            }
        } else {
            (self.done_bytes as f64 / self.total_bytes as f64).min(1.0)
        }
    }
}

pub type Report = Rc<dyn Fn(&Progress)>;

const PRIO: glib::Priority = glib::Priority::DEFAULT;

/// Files this big copy one at a time; several at once only makes the disk
/// seek between them.
const BIG_FILE: u64 = 64 * 1024 * 1024;

/// `G_FILE_COPY_TARGET_DEFAULT_MODIFIED_TIME` (GLib 2.80; older versions
/// ignore it and set the time themselves). Copies leave the time alone and
/// get it at the end of the job: setting it right after writing makes the
/// file system send the data off first, which on a network share is a
/// round trip per file (2,000 small files to SMB: 27 s one by one).
const TARGET_DEFAULT_MODIFIED_TIME: gio::FileCopyFlags =
    gio::FileCopyFlags::from_bits_retain(1 << 6);

/// Copies waiting for their modified time at most.
const STAMP_BATCH: usize = 4096;

fn err(msg: impl AsRef<str>) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Failed, msg.as_ref())
}

/// GIO reports sizes as i64; anything negative means "unknown".
pub fn bytes(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

/// Size in bytes, 0 when the backend didn't report one (reading a missing
/// attribute makes GIO log a critical).
pub fn size_of(info: &gio::FileInfo) -> u64 {
    if info.has_attribute("standard::size") {
        bytes(info.size())
    } else {
        0
    }
}

pub fn display_name(file: &gio::File) -> String {
    file.basename().map_or_else(
        || file.uri().to_string(),
        |b| b.to_string_lossy().into_owned(),
    )
}

/// The path of a local file, which plain syscalls on worker threads can
/// handle; None for anything GIO reaches through a backend.
fn local(file: &gio::File) -> Option<PathBuf> {
    file.is_native().then(|| file.path()).flatten()
}

/// An `io::Error` with the path in its message, worded like GIO's.
fn at_path(e: &io::Error, what: &str, path: &Path) -> io::Error {
    let text = e.to_string();
    let reason = text.split(" (os error").next().unwrap_or(&text);
    io::Error::new(
        e.kind(),
        format!("Error {what} {}: {reason}", path.display()),
    )
}

fn to_glib(e: &io::Error) -> glib::Error {
    let code = match e.kind() {
        io::ErrorKind::NotFound => gio::IOErrorEnum::NotFound,
        io::ErrorKind::PermissionDenied => gio::IOErrorEnum::PermissionDenied,
        io::ErrorKind::DirectoryNotEmpty => gio::IOErrorEnum::NotEmpty,
        io::ErrorKind::ReadOnlyFilesystem => gio::IOErrorEnum::ReadOnly,
        io::ErrorKind::Interrupted => gio::IOErrorEnum::Cancelled,
        _ => gio::IOErrorEnum::Failed,
    };
    glib::Error::new(code, &e.to_string())
}

async fn file_type(file: &gio::File) -> Result<gio::FileType, glib::Error> {
    let info = file
        .query_info_future(
            "standard::type",
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            PRIO,
        )
        .await?;
    Ok(info.file_type())
}

async fn exists(file: &gio::File) -> bool {
    file_type(file).await.is_ok()
}

async fn children(dir: &gio::File, attrs: &str) -> Result<Vec<gio::FileInfo>, glib::Error> {
    let en = dir
        .enumerate_children_future(attrs, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, PRIO)
        .await?;
    let mut all = Vec::new();
    loop {
        let batch = en.next_files_future(256, PRIO).await?;
        if batch.is_empty() {
            break;
        }
        all.extend(batch);
    }
    Ok(all)
}

/// Runs `f` over `items` with a few in flight; the first error stops the rest.
async fn concurrently<T, F, Fut>(items: Vec<T>, f: F) -> Result<(), glib::Error>
where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = Result<(), glib::Error>>,
{
    stream::iter(items)
        .map(Ok)
        .try_for_each_concurrent(par::in_flight(), f)
        .await
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Total bytes and entries under local `roots` (counting each root), walked
/// on all cores.
async fn measure_local(roots: Vec<PathBuf>) -> (u64, u64) {
    if roots.is_empty() {
        return (0, 0);
    }
    par::blocking(move |stop| {
        let (size, count) = (AtomicU64::new(0), AtomicU64::new(0));
        // Unreadable entries are skipped; the total only drives a bar.
        let _ = par::walk(roots, false, stop, |_, md| {
            count.fetch_add(1, Ordering::Relaxed);
            if !md.is_dir() {
                size.fetch_add(md.len(), Ordering::Relaxed);
            }
            Ok(true)
        });
        (size.into_inner(), count.into_inner())
    })
    .await
    .unwrap_or((0, 0))
}

/// Total bytes and entries under a file reached through a GIO backend.
fn measure_remote(file: gio::File, limit: Rc<par::Limiter>) -> Boxed<'static, (u64, u64)> {
    Box::pin(async move {
        let permit = limit.take(1).await;
        let Ok(info) = file
            .query_info_future(
                "standard::type,standard::size",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                PRIO,
            )
            .await
        else {
            return (0, 1);
        };
        if info.file_type() != gio::FileType::Directory {
            return (size_of(&info), 1);
        }
        let Ok(kids) = children(&file, "standard::name").await else {
            return (0, 1);
        };
        drop(permit);
        stream::iter(kids)
            .map(|kid| measure_remote(file.child(kid.name()), limit.clone()))
            .buffer_unordered(par::in_flight())
            .fold((0, 1), |(b, n), (kb, kn)| async move { (b + kb, n + kn) })
            .await
    })
}

/// Total bytes and entries under all of `files` (counting themselves).
async fn measure(files: &[gio::File]) -> (u64, u64) {
    let mut roots = Vec::new();
    let mut remote = Vec::new();
    for f in files {
        match local(f) {
            Some(path) => roots.push(path),
            None => remote.push(f.clone()),
        }
    }
    let limit = par::Limiter::new(par::in_flight());
    let remote = stream::iter(remote)
        .map(|f| measure_remote(f, limit.clone()))
        .buffer_unordered(par::in_flight())
        .fold((0, 0), |(b, n), (kb, kn)| async move { (b + kb, n + kn) });
    let ((lb, ln), (rb, rn)) = futures_util::future::join(measure_local(roots), remote).await;
    (lb + rb, ln + rn)
}

/// A free name for `name` in `dir`: "a (2).txt", or "a (Copy).txt" when
/// duplicating in place.
pub async fn free_target(dir: &gio::File, name: &str, is_dir: bool, duplicate: bool) -> gio::File {
    free_target_except(dir, name, is_dir, duplicate, &HashSet::new()).await
}

/// `free_target` that also steers clear of names already picked for files
/// that don't exist yet.
async fn free_target_except(
    dir: &gio::File,
    name: &str,
    is_dir: bool,
    duplicate: bool,
    picked: &HashSet<String>,
) -> gio::File {
    let mut candidate = dir.child(name);
    let mut current = name.to_owned();
    while picked.contains(&current) || exists(&candidate).await {
        current = if duplicate {
            filename::for_copy(&current, 1, is_dir)
        } else {
            filename::for_conflict(&current, 1, is_dir)
        };
        candidate = dir.child(&current);
    }
    candidate
}

/// What the user picked for a name clash (nautilus-file-conflict-dialog.h).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Replace a file, or merge into a folder.
    Replace,
    Skip,
    Rename(String),
    Cancel,
}

/// A name clash to ask about (nautilus-operations-ui-manager.c).
// Plain facts handed to the UI, not a state machine.
#[allow(clippy::struct_excessive_bools)]
pub struct Conflict {
    pub dest: gio::File,
    /// Whether what's already there is a folder.
    pub dest_dir: bool,
    /// Folder onto folder: offered as "Merge" instead of "Replace".
    pub merge: bool,
    /// False for a file or link onto a folder, which would wipe the folder.
    pub can_replace: bool,
    /// Whether what's already there is newer (Some(true)) or older.
    pub dest_newer: Option<bool>,
    /// A free "(2)" name to suggest for Rename.
    pub suggested: String,
    /// More items follow, so "apply to all" makes sense.
    pub more: bool,
}

/// Asks the user about a conflict; returns their choice and "apply to all".
pub type Resolver = Rc<dyn Fn(Conflict) -> Boxed<'static, (Choice, bool)>>;

pub fn cancelled() -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Cancelled, "Cancelled")
}

struct Job {
    progress: RefCell<Progress>,
    report: Report,
    /// Bytes of finished files; copies in flight add theirs on top.
    settled: Cell<u64>,
    copying: RefCell<HashMap<u64, u64>>,
    next_copy: Cell<u64>,
    /// GIO requests this job may have in flight.
    limit: Rc<par::Limiter>,
    resolve: Resolver,
    replace_all: Cell<bool>,
    merge_all: Cell<bool>,
    skip_all: Cell<bool>,
    /// Copies whose modified time is still the source's to give them
    /// (see `TARGET_DEFAULT_MODIFIED_TIME`).
    stamps: RefCell<Vec<(gio::File, gio::FileInfo)>>,
    /// What this job made, for undoing it (see `Placed`).
    placed: RefCell<Placed>,
}

/// What a copy or move made: each item that didn't exist before, with the
/// item it came from (inside merged folders too), and how many existing
/// files were replaced (those can't be put back).
#[derive(Default, Debug)]
pub struct Placed {
    pub fresh: Vec<(gio::File, gio::File)>,
    pub replaced: usize,
}

impl Job {
    fn new(total: Progress, report: Report, resolve: Resolver) -> Rc<Self> {
        Rc::new(Self {
            progress: total.into(),
            report,
            settled: Cell::new(0),
            copying: RefCell::default(),
            next_copy: Cell::new(0),
            limit: par::Limiter::new(par::in_flight()),
            resolve,
            replace_all: Cell::new(false),
            merge_all: Cell::new(false),
            skip_all: Cell::new(false),
            stamps: RefCell::default(),
            placed: RefCell::default(),
        })
    }

    /// Gives the copies made so far their source's modified time, all at
    /// once now that their contents are written.
    async fn stamp(&self) -> Result<(), glib::Error> {
        let stamps = std::mem::take(&mut *self.stamps.borrow_mut());
        concurrently(stamps, |(file, info)| async move {
            file.set_attributes_future(&info, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, PRIO)
                .await
                .map(|_| ())
        })
        .await
    }

    fn emit(&self) {
        (self.report)(&self.progress.borrow());
    }

    fn refresh(&self) {
        let live: u64 = self.copying.borrow().values().sum();
        self.progress.borrow_mut().done_bytes = self.settled.get() + live;
        self.emit();
    }

    fn advance(&self, files: u64, bytes: u64) {
        self.settled.set(self.settled.get() + bytes);
        self.progress.borrow_mut().done_files += files;
        self.refresh();
    }

    fn file_done(&self, bytes: u64) {
        self.advance(1, bytes);
    }

    /// Counts a skipped item as done so the bar still reaches the end.
    async fn skipped(&self, file: &gio::File) {
        let (b, n) = measure(std::slice::from_ref(file)).await;
        self.advance(n, b);
    }
}

fn copy_tree(
    job: Rc<Job>,
    src: gio::File,
    dst: gio::File,
    overwrite: bool,
) -> Boxed<'static, Result<(), glib::Error>> {
    Box::pin(async move {
        let permit = job.limit.take(1).await;
        let info = src
            .query_info_future(
                "standard::type,standard::size,standard::name,time::modified,time::modified-usec",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                PRIO,
            )
            .await?;
        job.progress.borrow_mut().current = display_name(&src);
        if info.file_type() == gio::FileType::Directory {
            dst.make_directory_future(PRIO).await?;
            let kids = children(&src, "standard::name").await?;
            // Held only for this folder's own requests: its children wait
            // for permits of their own.
            drop(permit);
            job.file_done(0);
            return concurrently(kids, |kid| {
                copy_tree(
                    job.clone(),
                    src.child(kid.name()),
                    dst.child(kid.name()),
                    false,
                )
            })
            .await;
        }

        let size = size_of(&info);
        let permit = if size >= BIG_FILE {
            drop(permit);
            job.limit.take_all().await
        } else {
            permit
        };
        // Not ALL_METADATA: it adds the access time, which is set the same
        // slow way. Permissions and extended attributes still come along;
        // the owner only could for root.
        let mut flags = gio::FileCopyFlags::NOFOLLOW_SYMLINKS | TARGET_DEFAULT_MODIFIED_TIME;
        if overwrite {
            flags |= gio::FileCopyFlags::OVERWRITE;
        }
        let (copy, mut progress) = src.copy_future(&dst, flags, PRIO);
        let id = job.next_copy.get();
        job.next_copy.set(id + 1);
        let watcher = job.clone();
        let ticker = glib::spawn_future_local(async move {
            while let Some((current, _total)) = progress.next().await {
                watcher.copying.borrow_mut().insert(id, bytes(current));
                watcher.refresh();
            }
        });
        let result = copy.await;
        ticker.abort();
        job.copying.borrow_mut().remove(&id);
        drop(permit);
        result?;
        if info.file_type() == gio::FileType::Regular {
            let stamp = gio::FileInfo::new();
            stamp.set_attribute_uint64("time::modified", info.attribute_uint64("time::modified"));
            stamp.set_attribute_uint32(
                "time::modified-usec",
                info.attribute_uint32("time::modified-usec"),
            );
            job.stamps.borrow_mut().push((dst, stamp));
            // A huge copy stamps as it goes, so the list stays small.
            if job.stamps.borrow().len() >= STAMP_BATCH {
                job.stamp().await?;
            }
        }
        job.file_done(size);
        Ok(())
    })
}

/// Deletes local trees on all cores: files as they're found, then folders
/// deepest first (folders at one depth don't contain each other).
async fn delete_local(roots: Vec<PathBuf>) -> Result<(), glib::Error> {
    if roots.is_empty() {
        return Ok(());
    }
    par::blocking(move |stop| {
        let dirs = Mutex::new(Vec::new());
        par::walk(roots, true, stop, |path, md| {
            if md.is_dir() {
                dirs.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(path.to_path_buf());
                Ok(true)
            } else {
                std::fs::remove_file(path)
                    .map(|()| false)
                    .map_err(|e| at_path(&e, "removing file", path))
            }
        })?;
        let mut dirs = dirs.into_inner().unwrap_or_else(PoisonError::into_inner);
        dirs.sort_unstable_by_key(|d| Reverse(d.components().count()));
        for level in dirs.chunk_by(|a, b| a.components().count() == b.components().count()) {
            par::each(level, stop, |d| {
                std::fs::remove_dir(d).map_err(|e| at_path(&e, "removing file", d))
            })?;
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|| Err(io::Error::other("Delete stopped unexpectedly")))
    .map_err(|e| to_glib(&e))
}

/// Deletes a tree reached through a GIO backend, a few requests at a time.
fn delete_remote(
    file: gio::File,
    limit: Rc<par::Limiter>,
) -> Boxed<'static, Result<(), glib::Error>> {
    Box::pin(async move {
        let permit = limit.take(1).await;
        if file_type(&file).await? == gio::FileType::Directory {
            let kids = children(&file, "standard::name").await?;
            drop(permit);
            concurrently(kids, |kid| {
                delete_remote(file.child(kid.name()), limit.clone())
            })
            .await?;
            let _permit = limit.take(1).await;
            return file.delete_future(PRIO).await;
        }
        file.delete_future(PRIO).await
    })
}

/// Deletes `files` and everything in them.
async fn delete_trees(files: Vec<gio::File>) -> Result<(), glib::Error> {
    let mut roots = Vec::new();
    let mut remote = Vec::new();
    for f in files {
        match local(&f) {
            Some(path) => roots.push(path),
            None => remote.push(f),
        }
    }
    let limit = par::Limiter::new(par::in_flight());
    let remote = concurrently(remote, |f| delete_remote(f, limit.clone()));
    futures_util::future::try_join(delete_local(roots), remote)
        .await
        .map(|_| ())
}

/// Copies or moves `src` to `target`, which is free (or, with `overwrite`,
/// may be replaced).
async fn put(
    job: &Rc<Job>,
    src: &gio::File,
    target: &gio::File,
    kind: Kind,
    overwrite: bool,
) -> Result<(), glib::Error> {
    let src_dir = file_type(src).await? == gio::FileType::Directory;
    if overwrite && src_dir {
        // A folder replacing a file: remove the file first.
        target.delete_future(PRIO).await?;
    }
    let overwrite_file = overwrite && !src_dir;
    if kind == Kind::Move {
        let mut flags = gio::FileCopyFlags::NOFOLLOW_SYMLINKS;
        if overwrite_file {
            flags |= gio::FileCopyFlags::OVERWRITE;
        }
        let permit = job.limit.take(1).await;
        let (mv, _progress) = src.move_future(target, flags, PRIO);
        let moved = mv.await;
        drop(permit);
        match moved {
            Ok(()) => {
                let (b, n) = measure(std::slice::from_ref(target)).await;
                job.advance(n, b);
                return Ok(());
            }
            // Different filesystem with a directory: copy, then delete.
            Err(e) if e.matches(gio::IOErrorEnum::WouldRecurse) => {}
            Err(e) => return Err(e),
        }
        copy_tree(job.clone(), src.clone(), target.clone(), overwrite_file).await?;
        delete_trees(vec![src.clone()]).await
    } else {
        copy_tree(job.clone(), src.clone(), target.clone(), overwrite_file).await
    }
}

/// Puts each source at its target. Free targets are filled in parallel
/// while clashes are asked about one at a time, in order. Returns what each
/// pair created, in order (None if skipped).
async fn place_all(
    job: &Rc<Job>,
    pairs: Vec<(gio::File, gio::File)>,
    kind: Kind,
) -> Result<Vec<Option<gio::File>>, glib::Error> {
    let taken: Vec<bool> = stream::iter(pairs.iter().map(|(_, t)| exists(t)))
        .buffered(par::in_flight())
        .collect()
        .await;
    let mut claimed = HashSet::new();
    let (mut free, mut clash, mut later) = (Vec::new(), Vec::new(), Vec::new());
    for (i, ((src, target), taken)) in pairs.into_iter().zip(taken).enumerate() {
        if taken {
            clash.push((i, src, target));
        } else if claimed.insert(target.uri()) {
            free.push((i, src, target));
        } else {
            // Two sources with one name: the second clashes with the first
            // once that one exists.
            later.push((i, src, target));
        }
    }
    let out = RefCell::new(vec![None; free.len() + clash.len() + later.len()]);
    let out = &out;
    let fast = concurrently(free, |(i, src, target)| async move {
        put(job, &src, &target, kind, false).await?;
        job.placed.borrow_mut().fresh.push((src, target.clone()));
        out.borrow_mut()[i] = Some(target);
        Ok(())
    });
    let ask = |items: Vec<(usize, gio::File, gio::File)>| async move {
        for (i, src, target) in items {
            let made = place(job.clone(), src, target, kind).await?;
            out.borrow_mut()[i] = made;
        }
        Ok::<(), glib::Error>(())
    };
    futures_util::future::try_join(fast, ask(clash)).await?;
    ask(later).await?;
    Ok(out.take())
}

/// Puts `src` at `target`, asking what to do if something is already there
/// (`copy_move_file` in nautilus-file-operations.c). Returns the file created,
/// or None if skipped.
fn place(
    job: Rc<Job>,
    src: gio::File,
    target: gio::File,
    kind: Kind,
) -> Boxed<'static, Result<Option<gio::File>, glib::Error>> {
    Box::pin(async move {
        let attrs = "standard::type,time::modified";
        let nofollow = gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS;
        let src_info = src.query_info_future(attrs, nofollow, PRIO).await?;
        let src_dir = src_info.file_type() == gio::FileType::Directory;
        let mut target = target;
        loop {
            let Ok(dest_info) = target.query_info_future(attrs, nofollow, PRIO).await else {
                put(&job, &src, &target, kind, false).await?;
                job.placed.borrow_mut().fresh.push((src, target.clone()));
                return Ok(Some(target));
            };
            let dest_dir = dest_info.file_type() == gio::FileType::Directory;
            let merge = src_dir && dest_dir;
            let can_replace = merge || !dest_dir;

            let auto =
                (merge && job.merge_all.get()) || (!merge && can_replace && job.replace_all.get());
            let choice = if auto {
                Choice::Replace
            } else if job.skip_all.get() {
                Choice::Skip
            } else {
                let parent = target.parent().ok_or_else(|| err("No parent folder"))?;
                let suggested = display_name(
                    &free_target(&parent, &display_name(&target), dest_dir, false).await,
                );
                let dest_newer = match (
                    dest_info.modification_date_time(),
                    src_info.modification_date_time(),
                ) {
                    (Some(d), Some(s)) if d != s => Some(d > s),
                    _ => None,
                };
                let more = job.progress.borrow().total_files > 1;
                let conflict = Conflict {
                    dest: target.clone(),
                    dest_dir,
                    merge,
                    can_replace,
                    dest_newer,
                    suggested,
                    more,
                };
                let (choice, all) = (job.resolve)(conflict).await;
                if all {
                    match choice {
                        Choice::Replace if merge => job.merge_all.set(true),
                        Choice::Replace => job.replace_all.set(true),
                        Choice::Skip => job.skip_all.set(true),
                        _ => {}
                    }
                }
                choice
            };

            match choice {
                Choice::Cancel => return Err(cancelled()),
                Choice::Skip => {
                    job.skipped(&src).await;
                    return Ok(None);
                }
                Choice::Rename(name) => {
                    filename::validate(&name).map_err(err)?;
                    let parent = target.parent().ok_or_else(|| err("No parent folder"))?;
                    target = parent.child(name);
                }
                Choice::Replace if merge => {
                    // Fill the existing folder, asking about each clash inside.
                    job.file_done(0);
                    let pairs = children(&src, "standard::name")
                        .await?
                        .into_iter()
                        .map(|kid| (src.child(kid.name()), target.child(kid.name())))
                        .collect();
                    place_all(&job, pairs, kind).await?;
                    if kind == Kind::Move {
                        // Only succeeds once everything was moved out.
                        let _ = src.delete_future(PRIO).await;
                    }
                    return Ok(Some(target));
                }
                Choice::Replace if can_replace => {
                    put(&job, &src, &target, kind, true).await?;
                    job.placed.borrow_mut().replaced += 1;
                    return Ok(Some(target));
                }
                // Replace isn't offered here; ask again.
                Choice::Replace => {}
            }
        }
    })
}

/// Copies or moves `sources` into `dest`, asking `resolve` about name
/// clashes. Returns the files created. (The app uses `transfer_logged`.)
#[cfg(test)]
pub async fn transfer(
    sources: Vec<gio::File>,
    dest: gio::File,
    kind: Kind,
    report: Report,
    resolve: Resolver,
) -> Result<Vec<gio::File>, glib::Error> {
    transfer_logged(sources, dest, kind, report, resolve)
        .await
        .0
}

/// `transfer`, also saying what it placed (for undo), even when it stopped
/// part way: what was done by then still is.
pub async fn transfer_logged(
    sources: Vec<gio::File>,
    dest: gio::File,
    kind: Kind,
    report: Report,
    resolve: Resolver,
) -> (Result<Vec<gio::File>, glib::Error>, Placed) {
    let job = Rc::new(RefCell::new(None::<Rc<Job>>));
    let result = transfer_in(sources, dest, kind, report, resolve, &job).await;
    let placed = job
        .borrow()
        .as_ref()
        .map(|j| std::mem::take(&mut *j.placed.borrow_mut()))
        .unwrap_or_default();
    (result, placed)
}

async fn transfer_in(
    sources: Vec<gio::File>,
    dest: gio::File,
    kind: Kind,
    report: Report,
    resolve: Resolver,
    job_out: &RefCell<Option<Rc<Job>>>,
) -> Result<Vec<gio::File>, glib::Error> {
    for src in &sources {
        if dest.equal(src) || dest.has_prefix(src) {
            return Err(err(format!(
                "Can't put “{}” inside itself",
                display_name(src)
            )));
        }
    }
    let sources: Vec<_> = sources
        .into_iter()
        .filter(|s| !(kind == Kind::Move && s.parent().is_some_and(|p| p.equal(&dest))))
        .collect();

    let (total_bytes, total_files) = measure(&sources).await;
    let job = Job::new(
        Progress {
            total_bytes,
            total_files,
            ..Progress::default()
        },
        report,
        resolve,
    );
    job_out.replace(Some(job.clone()));
    job.emit();

    let mut pairs = Vec::new();
    let mut picked = HashSet::new();
    for src in sources {
        let name = display_name(&src);
        // Duplicating in place never asks: it picks "name (Copy)", and no
        // two duplicates pick the same name.
        let target = if kind == Kind::Copy && src.parent().is_some_and(|p| p.equal(&dest)) {
            let is_dir = file_type(&src).await? == gio::FileType::Directory;
            let target = free_target_except(&dest, &name, is_dir, true, &picked).await;
            picked.insert(display_name(&target));
            target
        } else {
            dest.child(&name)
        };
        pairs.push((src, target));
    }
    // Stamped even when the job stops early: what was copied is complete.
    let placed = place_all(&job, pairs, kind).await;
    job.stamp().await?;
    Ok(placed?.into_iter().flatten().collect())
}

pub async fn trash(files: Vec<gio::File>) -> Result<(), glib::Error> {
    concurrently(files, |f| async move { f.trash_future(PRIO).await }).await
}

pub async fn delete(files: Vec<gio::File>) -> Result<(), glib::Error> {
    delete_trees(files).await
}

/// Empties trash:/// (`nautilus_file_operations_empty_trash`).
pub async fn empty_trash() -> Result<(), glib::Error> {
    let trash = gio::File::for_uri("trash:///");
    let kids = children(&trash, "standard::name").await?;
    // Items in trash:/// can be deleted directly, folders included.
    concurrently(kids, |kid| {
        let item = trash.child(kid.name());
        async move { item.delete_future(PRIO).await }
    })
    .await
}

/// Puts items back from the trash to where they were (`origs`), taking the
/// one trashed last when a place was trashed more than once. A place that's
/// taken again gets a free name beside it.
pub async fn untrash(origs: &[gio::File]) -> Result<(), glib::Error> {
    let trash = gio::File::for_uri("trash:///");
    let items = children(
        &trash,
        "standard::name,standard::type,trash::orig-path,trash::deletion-date",
    )
    .await?;
    for orig in origs {
        let Some(path) = orig.path() else { continue };
        let found = items
            .iter()
            .filter(|i| {
                i.attribute_byte_string("trash::orig-path")
                    .is_some_and(|p| Path::new(p.as_str()) == path)
            })
            .max_by_key(|i| {
                i.attribute_string("trash::deletion-date")
                    .unwrap_or_default()
            });
        let Some(info) = found else {
            return Err(err(format!(
                "“{}” is no longer in the trash",
                display_name(orig)
            )));
        };
        let parent = orig.parent().ok_or_else(|| err("No parent folder"))?;
        let dir = parent.clone();
        let _ =
            gio::spawn_blocking(move || dir.make_directory_with_parents(gio::Cancellable::NONE))
                .await;
        let is_dir = info.file_type() == gio::FileType::Directory;
        let target = if exists(orig).await {
            free_target(&parent, &display_name(orig), is_dir, false).await
        } else {
            orig.clone()
        };
        let (mv, _) = trash.child(info.name()).move_future(
            &target,
            gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
            PRIO,
        );
        mv.await?;
    }
    Ok(())
}

/// Moves each `(from, to)` back: what's at `to` goes to `from`, last first,
/// remaking `from`'s folder if a merge removed it. Refuses to replace
/// anything that's at `from` now.
pub async fn move_back(pairs: &[(gio::File, gio::File)]) -> Result<(), glib::Error> {
    for (from, to) in pairs.iter().rev() {
        if exists(from).await {
            return Err(err(format!(
                "“{}” can't go back: something else is there now",
                display_name(from)
            )));
        }
        if let Some(parent) = from.parent() {
            let _ = gio::spawn_blocking(move || {
                parent.make_directory_with_parents(gio::Cancellable::NONE)
            })
            .await;
        }
        let (mv, _) = to.move_future(from, gio::FileCopyFlags::NOFOLLOW_SYMLINKS, PRIO);
        mv.await?;
    }
    Ok(())
}

/// Every item in `trash` paired with the location it was trashed from.
pub async fn trash_items(trash: &gio::File) -> Result<Vec<(gio::File, gio::File)>, glib::Error> {
    Ok(children(trash, "standard::name,trash::orig-path")
        .await?
        .into_iter()
        .filter_map(|info| {
            let orig = info.attribute_byte_string("trash::orig-path")?;
            Some((trash.child(info.name()), gio::File::for_path(orig.as_str())))
        })
        .collect())
}

pub async fn new_folder(dir: &gio::File, name: &str) -> Result<gio::File, glib::Error> {
    let target = free_target(dir, name, true, false).await;
    target.make_directory_future(PRIO).await?;
    Ok(target)
}

/// Symlinks to `sources` in `dest` (`nautilus_file_operations_link`).
pub async fn link(sources: Vec<gio::File>, dest: gio::File) -> Result<(), glib::Error> {
    for src in sources {
        let Some(target_path) = src.path() else {
            return Err(err(format!(
                "Can't link to “{}”: not a local file",
                display_name(&src)
            )));
        };
        let name = display_name(&src);
        let same_dir = src.parent().is_some_and(|p| p.equal(&dest));
        let mut count = usize::from(same_dir);
        let mut link = dest.child(filename::for_link(&name, count));
        while exists(&link).await {
            count += 1;
            link = dest.child(filename::for_link(&name, count));
        }
        // Links are local-only, so the blocking call is quick.
        link.make_symbolic_link(&target_path, gio::Cancellable::NONE)?;
    }
    Ok(())
}

/// Creates `name` (or a free variant) in `dir` holding `contents`.
pub async fn new_file_with_contents(
    dir: &gio::File,
    name: &str,
    contents: Vec<u8>,
) -> Result<gio::File, glib::Error> {
    let target = free_target(dir, name, false, false).await;
    target
        .replace_contents_future(contents, None, false, gio::FileCreateFlags::NONE)
        .await
        .map_err(|(_, e)| e)?;
    Ok(target)
}

/// Total bytes and entries under `files` (each counted too), for sizes and
/// progress bars.
pub async fn count(files: &[gio::File]) -> (u64, u64) {
    measure(files).await
}

/// Renames each file to its new name, as one step: through temporary names
/// first, so names can be swapped or shifted along (a→b while b→c) without
/// one rename landing on a file still to go. On a failure, what was renamed
/// goes back. Returns the renamed files, in order.
pub async fn rename_many(renames: Vec<(gio::File, String)>) -> Result<Vec<gio::File>, glib::Error> {
    for (_, name) in &renames {
        filename::validate(name).map_err(err)?;
    }
    let stamp = format!(
        ".abyss-rename-{}-{}",
        std::process::id(),
        glib::monotonic_time()
    );
    // Phase one: everything to a temporary name beside it.
    let mut parked: Vec<(gio::File, gio::File, String)> = Vec::with_capacity(renames.len());
    for (i, (file, name)) in renames.into_iter().enumerate() {
        match file
            .set_display_name_future(&format!("{stamp}-{i}"), PRIO)
            .await
        {
            Ok(tmp) => parked.push((tmp, file, name)),
            Err(e) => {
                for (tmp, _, _) in parked.iter().rev() {
                    let _ = tmp
                        .set_display_name_future(&old_name_of(tmp, &parked), PRIO)
                        .await;
                }
                return Err(e);
            }
        }
    }
    // Phase two: temporary names to the new ones, refusing to replace a
    // file that was already there.
    let mut done = Vec::with_capacity(parked.len());
    for (k, (tmp, _, name)) in parked.iter().enumerate() {
        let target = tmp.parent().map(|p| p.child(name));
        if target
            .as_ref()
            .is_some_and(|t| t.query_exists(gio::Cancellable::NONE))
        {
            let e = err(format!("“{name}” already exists"));
            rollback(&parked[..k], &done, &parked[k..]).await;
            return Err(e);
        }
        match tmp.set_display_name_future(name, PRIO).await {
            Ok(new) => done.push(new),
            Err(e) => {
                rollback(&parked[..k], &done, &parked[k..]).await;
                return Err(e);
            }
        }
    }
    Ok(done)
}

/// The name a parked file had before `rename_many` (its original file's).
fn old_name_of(tmp: &gio::File, parked: &[(gio::File, gio::File, String)]) -> String {
    parked.iter().find(|(t, _, _)| t.equal(tmp)).map_or_else(
        || display_name(tmp),
        |(_, original, _)| display_name(original),
    )
}

/// Undoes a `rename_many` that failed part way: renamed files and still
/// parked ones all get their original names back.
async fn rollback(
    renamed: &[(gio::File, gio::File, String)],
    now: &[gio::File],
    parked: &[(gio::File, gio::File, String)],
) {
    for ((_, original, _), file) in renamed.iter().zip(now) {
        let _ = file
            .set_display_name_future(&display_name(original), PRIO)
            .await;
    }
    for (tmp, original, _) in parked {
        let _ = tmp
            .set_display_name_future(&display_name(original), PRIO)
            .await;
    }
}

pub async fn rename(file: &gio::File, name: &str) -> Result<gio::File, glib::Error> {
    filename::validate(name).map_err(err)?;
    file.set_display_name_future(name, PRIO).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abyss-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "hello").unwrap();
        std::fs::write(dir.join("src/sub/b.txt"), "world").unwrap();
        // An old time, so a copy that didn't keep it would show.
        let old =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_micros(1_000_000_123_456);
        for f in ["src/a.txt", "src/sub/b.txt"] {
            std::fs::File::options()
                .write(true)
                .open(dir.join(f))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        dir
    }

    /// Runs a future on a private main context, so tests can run in parallel.
    fn run(fut: impl Future<Output = ()>) {
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| ctx.block_on(fut)).unwrap();
    }

    /// Answers every conflict the same way.
    fn answer(f: impl Fn(&Conflict) -> Choice + 'static) -> Resolver {
        Rc::new(move |c| {
            let choice = f(&c);
            Box::pin(async move { (choice, false) })
        })
    }

    #[test]
    fn copy_move_delete() {
        let dir = tmpdir("ops");
        let f = |p: &str| gio::File::for_path(dir.join(p));
        let quiet: Report = Rc::new(|_| {});
        let rename = answer(|c| Choice::Rename(c.suggested.clone()));
        run(async {
            // A file and a folder tree.
            transfer(
                vec![f("src/a.txt"), f("src/sub")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                rename.clone(),
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("dst/sub/b.txt")).unwrap(),
                "world"
            );
            // Copies keep their source's modified time (set at the end).
            let mtime = |p: &str| std::fs::metadata(dir.join(p)).unwrap().modified().unwrap();
            assert_eq!(mtime("dst/sub/b.txt"), mtime("src/sub/b.txt"));
            assert_eq!(mtime("dst/a.txt"), mtime("src/a.txt"));
            // Renaming on a clash suggests "(2)".
            transfer(
                vec![f("src/a.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                rename.clone(),
            )
            .await
            .unwrap();
            assert!(dir.join("dst/a (2).txt").exists());
            // Duplicating in place gets "(Copy)".
            transfer(
                vec![f("dst/a.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                rename.clone(),
            )
            .await
            .unwrap();
            assert!(dir.join("dst/a (Copy).txt").exists());
            // A folder into itself is refused.
            assert!(transfer(
                vec![f("src")],
                f("src/sub"),
                Kind::Copy,
                quiet.clone(),
                rename.clone()
            )
            .await
            .is_err());
            // Move onto a clash, then delete a tree.
            transfer(
                vec![f("src/sub")],
                f("dst"),
                Kind::Move,
                quiet.clone(),
                rename.clone(),
            )
            .await
            .unwrap();
            assert!(!dir.join("src/sub").exists());
            assert!(dir.join("dst/sub (2)/b.txt").exists());
            delete(vec![f("dst")]).await.unwrap();
            assert!(!dir.join("dst").exists());
            let made = new_folder(&f(""), "src").await.unwrap();
            assert_eq!(display_name(&made), "src (2)");
            // Links: same name elsewhere, "Link to" next to the target.
            link(vec![f("src/a.txt")], f("src (2)")).await.unwrap();
            assert_eq!(
                std::fs::read_link(dir.join("src (2)/a.txt")).unwrap(),
                dir.join("src/a.txt")
            );
            link(vec![f("src/a.txt")], f("src")).await.unwrap();
            assert!(dir.join("src/Link to a.txt").is_symlink());
            // Dropped text.
            let t = new_file_with_contents(&f("src"), "a.txt", b"x".to_vec())
                .await
                .unwrap();
            assert_eq!(display_name(&t), "a (2).txt");
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parallel_trees() {
        let dir = tmpdir("par");
        let f = |p: &str| gio::File::for_path(dir.join(p));
        let quiet: Report = Rc::new(|_| {});
        let rename = answer(|c| Choice::Rename(c.suggested.clone()));
        for d in 0..12 {
            let sub = dir.join(format!("src/tree/d{d}/deeper"));
            std::fs::create_dir_all(&sub).unwrap();
            for i in 0..40 {
                std::fs::write(sub.join(format!("{i}.txt")), format!("{d}-{i}")).unwrap();
            }
        }
        // Files bigger than BIG_FILE run alone; make sure that still works.
        let big = vec![7u8; usize::try_from(BIG_FILE).unwrap() + 1];
        std::fs::write(dir.join("src/tree/big.bin"), &big).unwrap();
        let seen = Rc::new(RefCell::new(Progress::default()));
        let last = seen.clone();
        let report: Report = Rc::new(move |p| *last.borrow_mut() = p.clone());
        run(async {
            let (size, count) = measure(&[f("src/tree")]).await;
            assert_eq!(count, 1 + 12 * 2 + 12 * 40 + 1);
            transfer(
                vec![f("src/tree")],
                f("dst"),
                Kind::Copy,
                report,
                rename.clone(),
            )
            .await
            .unwrap();
            let done = seen.borrow().clone();
            assert_eq!((done.done_files, done.total_files), (count, count));
            assert_eq!((done.done_bytes, done.total_bytes), (size, size));
            for d in [0, 11] {
                assert_eq!(
                    std::fs::read_to_string(dir.join(format!("dst/tree/d{d}/deeper/39.txt")))
                        .unwrap(),
                    format!("{d}-39")
                );
            }
            assert_eq!(std::fs::read(dir.join("dst/tree/big.bin")).unwrap(), big);
            // Duplicating "x" and "x (Copy)" together: no two picks collide.
            std::fs::write(dir.join("src/x.txt"), "1").unwrap();
            std::fs::write(dir.join("src/x (Copy).txt"), "2").unwrap();
            let made = transfer(
                vec![f("src/x.txt"), f("src/x (Copy).txt")],
                f("src"),
                Kind::Copy,
                quiet.clone(),
                rename.clone(),
            )
            .await
            .unwrap();
            let names: HashSet<_> = made.iter().map(display_name).collect();
            assert_eq!(names.len(), 2);
            // Deleting: everything goes, and a failure is reported.
            delete(vec![f("dst/tree"), f("src/x.txt")]).await.unwrap();
            assert!(!dir.join("dst/tree").exists() && !dir.join("src/x.txt").exists());
            let e = delete(vec![f("missing")]).await.unwrap_err();
            assert!(e.matches(gio::IOErrorEnum::NotFound), "{e}");
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    // One scenario, step by step; each step builds on the files left by
    // the one before.
    #[allow(clippy::too_many_lines)]
    fn conflicts() {
        let dir = tmpdir("conflicts");
        let f = |p: &str| gio::File::for_path(dir.join(p));
        let quiet: Report = Rc::new(|_| {});
        std::fs::create_dir_all(dir.join("dst/sub")).unwrap();
        std::fs::write(dir.join("dst/a.txt"), "old").unwrap();
        std::fs::write(dir.join("dst/sub/b.txt"), "old b").unwrap();
        std::fs::write(dir.join("dst/sub/keep.txt"), "keep").unwrap();
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
        run(async {
            // Skip leaves what's there.
            transfer(
                vec![f("src/a.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                answer(|_| Choice::Skip),
            )
            .await
            .unwrap();
            assert_eq!(read("dst/a.txt"), "old");
            // Replace overwrites a file.
            transfer(
                vec![f("src/a.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                answer(|_| Choice::Replace),
            )
            .await
            .unwrap();
            assert_eq!(read("dst/a.txt"), "hello");
            // Folder onto folder is a merge: asked once for the folder, then
            // per clashing file inside; other files are kept.
            let asked = Rc::new(RefCell::new(Vec::new()));
            let log = asked.clone();
            let merge = answer(move |c| {
                log.borrow_mut().push((display_name(&c.dest), c.merge));
                Choice::Replace
            });
            transfer(
                vec![f("src/sub")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                merge,
            )
            .await
            .unwrap();
            assert_eq!(
                *asked.borrow(),
                [("sub".to_string(), true), ("b.txt".to_string(), false)]
            );
            assert_eq!(read("dst/sub/b.txt"), "world");
            assert_eq!(read("dst/sub/keep.txt"), "keep");
            // A file can't replace a folder.
            std::fs::write(dir.join("sub"), "file").unwrap();
            let offered = Rc::new(Cell::new(true));
            let o = offered.clone();
            transfer(
                vec![f("sub")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                answer(move |c| {
                    o.set(c.can_replace);
                    Choice::Skip
                }),
            )
            .await
            .unwrap();
            assert!(!offered.get());
            // "Apply to all" asks only once.
            std::fs::write(dir.join("src/c.txt"), "c").unwrap();
            std::fs::write(dir.join("dst/c.txt"), "old c").unwrap();
            let count = Rc::new(Cell::new(0));
            let n = count.clone();
            let all: Resolver = Rc::new(move |_| {
                n.set(n.get() + 1);
                Box::pin(async { (Choice::Replace, true) })
            });
            transfer(
                vec![f("src/a.txt"), f("src/c.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                all,
            )
            .await
            .unwrap();
            assert_eq!(count.get(), 1);
            assert_eq!(read("dst/c.txt"), "c");
            // Cancel stops the job with a Cancelled error.
            let e = transfer(
                vec![f("src/a.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                answer(|_| Choice::Cancel),
            )
            .await
            .unwrap_err();
            assert!(e.matches(gio::IOErrorEnum::Cancelled));
            // Moving with merge empties and removes the source folder.
            transfer(
                vec![f("src/sub")],
                f("dst"),
                Kind::Move,
                quiet.clone(),
                answer(|_| Choice::Replace),
            )
            .await
            .unwrap();
            assert!(!dir.join("src/sub").exists());
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_many_swaps_and_rolls_back() {
        let dir = tmpdir("rename-many");
        let f = |p: &str| gio::File::for_path(dir.join(p));
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
        for (name, text) in [("a", "A"), ("b", "B"), ("x", "X"), ("y", "Y")] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        run(async {
            // A swap, and a chain where each name is the next one's old name.
            rename_many(vec![(f("a"), "b".into()), (f("b"), "a".into())])
                .await
                .unwrap();
            assert_eq!((read("a"), read("b")), ("B".into(), "A".into()));
            rename_many(vec![(f("x"), "y".into()), (f("y"), "z".into())])
                .await
                .unwrap();
            assert_eq!((read("y"), read("z")), ("X".into(), "Y".into()));
            // Onto a file that stays: refused, and everything as it was.
            assert!(
                rename_many(vec![(f("a"), "q".into()), (f("b"), "z".into())])
                    .await
                    .is_err()
            );
            assert_eq!(
                (read("a"), read("b"), read("z")),
                ("B".into(), "A".into(), "Y".into())
            );
            assert!(!dir.join("q").exists());
            let leftovers = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().starts_with(".abyss-rename"));
            assert!(!leftovers);
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn placed_and_moved_back() {
        let dir = tmpdir("undo");
        let f = |p: &str| gio::File::for_path(dir.join(p));
        let quiet: Report = Rc::new(|_| {});
        let replace = answer(|_| Choice::Replace);
        run(async {
            // A move notes each item; moving back undoes it.
            let (done, placed) = transfer_logged(
                vec![f("src/a.txt"), f("src/sub")],
                f("dst"),
                Kind::Move,
                quiet.clone(),
                replace.clone(),
            )
            .await;
            done.unwrap();
            assert_eq!(placed.fresh.len(), 2);
            assert_eq!(placed.replaced, 0);
            assert!(!dir.join("src/a.txt").exists());
            move_back(&placed.fresh).await.unwrap();
            assert!(dir.join("src/a.txt").exists() && dir.join("src/sub/b.txt").exists());
            assert!(!dir.join("dst/a.txt").exists() && !dir.join("dst/sub").exists());
            // Replacing an existing file is counted, not noted as new.
            std::fs::write(dir.join("dst/a.txt"), "old").unwrap();
            let (done, placed) = transfer_logged(
                vec![f("src/a.txt")],
                f("dst"),
                Kind::Copy,
                quiet.clone(),
                replace.clone(),
            )
            .await;
            done.unwrap();
            assert_eq!((placed.fresh.len(), placed.replaced), (0, 1));
            // Merging into a folder notes only what went in, so undoing
            // leaves what was there before.
            std::fs::create_dir_all(dir.join("dst/sub")).unwrap();
            std::fs::write(dir.join("dst/sub/mine.txt"), "keep").unwrap();
            let (done, placed) = transfer_logged(
                vec![f("src/sub")],
                f("dst"),
                Kind::Move,
                quiet.clone(),
                replace.clone(),
            )
            .await;
            done.unwrap();
            assert_eq!(placed.fresh.len(), 1);
            move_back(&placed.fresh).await.unwrap();
            assert!(dir.join("dst/sub/mine.txt").exists());
            assert!(!dir.join("dst/sub/b.txt").exists());
            assert!(dir.join("src/sub/b.txt").exists());
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
