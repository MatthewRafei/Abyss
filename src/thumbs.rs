//! Thumbnails with Abyss' own cache and its own renderers per file type.
//!
//! Cache: thumbnails other programs could use too (photos, videos,
//! documents, fonts...) go in the shared freedesktop.org cache,
//! `~/.cache/thumbnails/<size>/<md5 of URI>.png`, tagged with the file's
//! modification time, and Abyss reads what other apps put there. Its own
//! looks (text pages, archive listings, waveforms) stay in
//! `~/.cache/abyss/thumbnails/<px>/<key>.png`, where the key hashes the
//! URI, modification time and size, so a changed file simply gets a new
//! entry. Failures leave an empty marker under `fail/` and aren't retried;
//! Abyss' own entries unused for 120 days are pruned.
//!
//! Renderers, each used only when its tool is installed: images (glycin,
//! gdk-pixbuf, ImageMagick), video frames and audio covers/waveforms
//! (ffmpeg), PDF/PostScript (pdftoppm or Ghostscript), office documents
//! (gsf-office), EPUB / comic covers (bsdtar), fonts (ImageMagick),
//! text and code (first lines drawn as a page), archives (their file list),
//! and any other installed `.thumbnailer`. Only images and video are
//! thumbnailed for now (`SHOWN`); other files show their type's icon.
//! Queueing follows
//! nautilus-thumbnails.c, newest request first, but several renders run at
//! once (one per two cores); decoding, scaling and cache lookups happen off
//! the UI thread. Only local files are thumbnailed.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_channel::oneshot;
use gtk::gdk;
use gtk::gdk_pixbuf;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

/// Images over this aren't thumbnailed (nautilus `thumbnail-limit`).
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
/// Memory for decoded thumbnails kept so scrolling doesn't reload them.
/// They're decoded at the size they're shown, so this holds a few
/// thousand at list sizes and several hundred at the largest grid zoom.
const TEXTURE_BYTES: usize = 32 * 1024 * 1024;
/// A renderer that takes longer than this is stopped.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Cache entries untouched this long are pruned.
const MAX_AGE: Duration = Duration::from_hours(2880); // 120 days

// Page look for text, code and archive listings.
const PAPER: &str = "#f2ecf5";
const INK: &str = "#2a1830";

/// Thumbnail edge in pixels for this display: 256 at 1x, 512 up to 2x,
/// 1024 above (as Nautilus picks large / x-large / xx-large).
fn edge() -> u32 {
    let scale = gdk::Display::default().map_or(1, |d| {
        d.monitors()
            .iter::<gdk::Monitor>()
            .filter_map(Result::ok)
            .map(|m| m.scale_factor())
            .max()
            .unwrap_or(1)
    });
    match scale {
        ..=1 => 256,
        2 => 512,
        _ => 1024,
    }
}

fn cache_root() -> PathBuf {
    glib::user_cache_dir().join("abyss").join("thumbnails")
}

/// Identity of a file version: same URI, mtime and size share a thumbnail.
fn key(uri: &str, mtime: i64, size: u64) -> String {
    glib::compute_checksum_for_string(glib::ChecksumType::Md5, format!("{uri}\n{mtime}\n{size}"))
        .map_or_else(String::new, |s| s.to_string())
}

fn cache_path(key: &str) -> PathBuf {
    cache_root()
        .join(edge().to_string())
        .join(format!("{key}.png"))
}

fn fail_path(key: &str) -> PathBuf {
    cache_root().join("fail").join(key)
}

// ------------------------------------------------------ shared cache ----

/// freedesktop.org size folders, smallest first, with their edge in pixels.
const SHARED_SIZES: [(&str, u32); 4] = [
    ("normal", 128),
    ("large", 256),
    ("x-large", 512),
    ("xx-large", 1024),
];

/// Where the thumbnail spec keeps a thumbnail of `uri` at size folder `dir`.
fn shared_path(dir: &str, uri: &str) -> PathBuf {
    let name = glib::compute_checksum_for_string(glib::ChecksumType::Md5, uri)
        .map_or_else(String::new, |s| s.to_string());
    glib::user_cache_dir()
        .join("thumbnails")
        .join(dir)
        .join(format!("{name}.png"))
}

/// Shared thumbnails of `uri` at least `edge` pixels big, best fit first.
fn shared_candidates(uri: &str, edge: u32) -> Vec<PathBuf> {
    SHARED_SIZES
        .iter()
        .filter(|(_, px)| *px >= edge)
        .map(|(dir, _)| shared_path(dir, uri))
        .collect()
}

/// The shared thumbnail a new render of this size goes to.
fn shared_target(uri: &str, edge: u32) -> PathBuf {
    let dir = SHARED_SIZES
        .iter()
        .find(|(_, px)| *px >= edge)
        .map_or("xx-large", |(dir, _)| dir);
    shared_path(dir, uri)
}

/// What a thumbnail must say about its file to still be current.
#[derive(Clone)]
struct Stamp {
    uri: String,
    mtime: i64,
    size: u64,
}

/// The `tEXt` chunks of a PNG, read up to the image data.
fn png_text(path: &Path) -> Option<HashMap<String, String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut sig = [0; 8];
    f.read_exact(&mut sig).ok()?;
    if sig != *b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut text = HashMap::new();
    loop {
        let mut head = [0; 8];
        f.read_exact(&mut head).ok()?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        match &head[4..] {
            b"IDAT" | b"IEND" => return Some(text),
            b"tEXt" if len <= 64 * 1024 => {
                let mut data = vec![0; usize::try_from(len).ok()?];
                f.read_exact(&mut data).ok()?;
                if let Some(nul) = data.iter().position(|&b| b == 0) {
                    let key = String::from_utf8_lossy(&data[..nul]).into_owned();
                    let value = String::from_utf8_lossy(&data[nul + 1..]).into_owned();
                    text.insert(key, value);
                }
                f.seek_relative(4).ok()?;
            }
            _ => {
                f.seek(SeekFrom::Current(i64::from(len) + 4)).ok()?;
            }
        }
    }
}

/// Whether a shared thumbnail is of this file as it is now: the spec's
/// `Thumb::URI` and `Thumb::MTime` must match, and `Thumb::Size` if given.
fn shared_is_current(path: &Path, stamp: &Stamp) -> bool {
    let Some(text) = png_text(path) else {
        return false;
    };
    let field = |k: &str| text.get(k).map(String::as_str);
    field("Thumb::URI") == Some(stamp.uri.as_str())
        && field("Thumb::MTime").and_then(|m| m.parse::<i64>().ok()) == Some(stamp.mtime)
        && field("Thumb::Size").is_none_or(|s| s.parse::<u64>().ok() == Some(stamp.size))
}

/// Whether `program` is on PATH; looked up once per program, since the
/// answer is needed for every row shown.
fn have(program: &str) -> bool {
    use std::sync::{Mutex, OnceLock, PoisonError};
    static SEEN: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some(&known) = seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(program)
    {
        return known;
    }
    let found = glib::find_program_in_path(program).is_some();
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(program.to_owned(), found);
    found
}

fn is_a(content_type: &str, supertype: &str) -> bool {
    content_type == supertype || gio::content_type_is_a(content_type, supertype)
}

// --------------------------------------------------------- renderers ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Image,
    Video,
    Audio,
    Pdf,
    PostScript,
    Ebook,
    Font,
    Text,
    Archive,
    /// A system `.thumbnailer` (office documents and anything else).
    System,
}

struct Thumbnailer {
    exec: String,
    mime_types: Vec<String>,
}

/// Installed `*.thumbnailer` programs, first definition wins.
fn system_thumbnailers() -> &'static [Thumbnailer] {
    use std::sync::OnceLock;
    static LIST: OnceLock<Vec<Thumbnailer>> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut dirs = vec![glib::user_data_dir()];
        dirs.extend(glib::system_data_dirs());
        let mut out = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir.join("thumbnailers")) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let value = |key: &str| {
                    text.lines()
                        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
                        .map(str::trim)
                };
                let (Some(exec), Some(types)) = (value("Exec"), value("MimeType")) else {
                    continue;
                };
                let program = value("TryExec")
                    .or_else(|| exec.split_whitespace().next())
                    .unwrap_or("");
                if !have(program) {
                    continue;
                }
                out.push(Thumbnailer {
                    exec: exec.to_owned(),
                    mime_types: types
                        .split(';')
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned)
                        .collect(),
                });
            }
        }
        out
    })
}

fn system_thumbnailer(content_type: &str) -> Option<&'static Thumbnailer> {
    system_thumbnailers()
        .iter()
        .find(|t| t.mime_types.iter().any(|m| is_a(content_type, m)))
}

fn pixbuf_loads(content_type: &str) -> bool {
    gdk_pixbuf::Pixbuf::formats()
        .iter()
        .flat_map(gdk_pixbuf::PixbufFormat::mime_types)
        .any(|m| m.as_str() == content_type)
}

/// Text-like types that read well as their first lines.
fn is_text(content_type: &str) -> bool {
    const TEXTY: &[&str] = &[
        "text/plain",
        "application/json",
        "application/xml",
        "application/javascript",
        "application/x-shellscript",
        "application/toml",
        "application/x-yaml",
        "application/sql",
        "application/x-desktop",
        "application/x-subrip",
    ];
    TEXTY.iter().any(|t| is_a(content_type, t))
}

/// Picks the renderer for a content type, if any tool for it is installed.
/// Remembered per type: working it out walks the MIME tree.
fn kind_for(content_type: &str) -> Option<Kind> {
    thread_local! {
        static KINDS: RefCell<HashMap<String, Option<Kind>>> = RefCell::default();
    }
    if let Some(kind) = KINDS.with_borrow(|k| k.get(content_type).copied()) {
        return kind;
    }
    let kind = find_kind(content_type);
    KINDS.with_borrow_mut(|k| k.insert(content_type.to_owned(), kind));
    kind
}

fn find_kind(content_type: &str) -> Option<Kind> {
    let ct = content_type;
    if ct.starts_with("image/") || pixbuf_loads(ct) {
        return Some(Kind::Image);
    }
    if ct.starts_with("video/") && have("ffmpeg") {
        return Some(Kind::Video);
    }
    if ct.starts_with("audio/") && have("ffmpeg") {
        return Some(Kind::Audio);
    }
    if is_a(ct, "application/pdf") && (have("pdftoppm") || have("gs")) {
        return Some(Kind::Pdf);
    }
    if is_a(ct, "application/postscript") && have("gs") {
        return Some(Kind::PostScript);
    }
    let ebook = [
        "application/epub+zip",
        "application/vnd.comicbook+zip",
        "application/vnd.comicbook-rar",
        "application/x-cbr",
        "application/x-cbz",
    ];
    if ebook.iter().any(|t| is_a(ct, t)) && have("bsdtar") {
        return Some(Kind::Ebook);
    }
    if (ct.starts_with("font/") || ct.starts_with("application/x-font")) && have("magick") {
        return Some(Kind::Font);
    }
    if system_thumbnailer(ct).is_some() {
        return Some(Kind::System);
    }
    if is_text(ct) && have("magick") {
        return Some(Kind::Text);
    }
    if crate::archive::is_archive_type(ct) && have("bsdtar") && have("magick") {
        return Some(Kind::Archive);
    }
    None
}

// ------------------------------------------------------------ public ----

/// Whether Abyss will try to thumbnail this file: local, regular, a type
/// it can render, and not an oversized image. Cheap: no disk access.
fn can_thumbnail(file: &gio::File, info: &gio::FileInfo) -> bool {
    // Empty files have nothing to show (and their names can claim any type).
    if !file.is_native()
        || info.file_type() != gio::FileType::Regular
        || crate::ops::size_of(info) == 0
    {
        return false;
    }
    let Some(ct) = crate::info::content_type(info) else {
        return false;
    };
    match kind_for(&ct) {
        Some(Kind::Image) => crate::ops::size_of(info) <= MAX_IMAGE_BYTES,
        Some(kind) => SHOWN.contains(&kind),
        None => false,
    }
}

/// The kinds that get thumbnails; everything else shows its type's icon.
/// The other renderers are kept: add a kind here to bring it back.
const SHOWN: [Kind; 2] = [Kind::Image, Kind::Video];

/// The file's thumbnail, from the cache or freshly made; None if it has
/// none. The cache is checked on a worker thread, not the UI thread.
pub async fn thumbnail(file: &gio::File, info: &gio::FileInfo) -> Option<PathBuf> {
    if !can_thumbnail(file, info) {
        return None;
    }
    let stamp = info_stamp(file, info);
    let key = key(&stamp.uri, stamp.mtime, stamp.size);
    let own = cache_path(&key);
    let failed = fail_path(&key);
    let shared = shared_candidates(&stamp.uri, edge());
    // A tile scrolled past before its turn came drops this future; the
    // flag then lets the queued lookup skip its disk work.
    let found = crate::par::blocking(move |stop| {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(());
        }
        if own.exists() {
            return Ok(Some(own));
        }
        if let Some(path) = shared.into_iter().find(|p| shared_is_current(p, &stamp)) {
            return Ok(Some(path));
        }
        // Err: a known failure, don't try again.
        if failed.exists() {
            Err(())
        } else {
            Ok(None)
        }
    })
    .await?;
    match found {
        Ok(Some(path)) => Some(path),
        Ok(None) => request(file, info).await,
        Err(()) => None,
    }
}

fn info_stamp(file: &gio::File, info: &gio::FileInfo) -> Stamp {
    Stamp {
        uri: file.uri().to_string(),
        mtime: info.modification_date_time().map_or(0, |d| d.to_unix()),
        size: crate::ops::size_of(info),
    }
}

/// Removes cache entries untouched for `MAX_AGE`, off the UI thread, at
/// most once a day (a stamp file remembers the last run). The installed
/// thumbnailers are found meanwhile so the first renders don't wait.
pub fn prune_in_background() {
    std::thread::spawn(|| {
        system_thumbnailers();
        let stamp = cache_root().join(".pruned");
        let recent = std::fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < Duration::from_hours(24));
        if recent {
            return;
        }
        let _ = std::fs::create_dir_all(cache_root());
        let _ = std::fs::write(&stamp, b"");
        let Ok(dirs) = std::fs::read_dir(cache_root()) else {
            return;
        };
        for dir in dirs.filter_map(Result::ok) {
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for f in files.filter_map(Result::ok) {
                let old = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > MAX_AGE);
                if old {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
    });
}

// ------------------------------------------------------------- queue ----

struct Job {
    key: String,
    uri: String,
    mtime: i64,
    size: u64,
    path: PathBuf,
    content_type: String,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Job>,
    waiters: HashMap<String, Vec<oneshot::Sender<Option<PathBuf>>>>,
    /// Workers currently draining `pending`.
    running: usize,
}

/// Renders at once. Each renderer (ffmpeg, ImageMagick...) is itself
/// multi-threaded, so one per two cores keeps the machine busy without
/// making the newest request wait behind a crowd.
fn workers() -> usize {
    (crate::par::threads() / 2).clamp(2, 8)
}

thread_local! {
    static QUEUE: RefCell<Queue> = RefCell::default();
    static TEXTURES: RefCell<Textures> = RefCell::default();
}

/// Asks for a thumbnail; resolves to its path, or None if it can't be made.
/// Requests for the same file version share one job.
async fn request(file: &gio::File, info: &gio::FileInfo) -> Option<PathBuf> {
    let stamp = info_stamp(file, info);
    let key = key(&stamp.uri, stamp.mtime, stamp.size);
    let job = Job {
        key: key.clone(),
        uri: stamp.uri,
        mtime: stamp.mtime,
        size: stamp.size,
        path: file.path()?,
        content_type: crate::info::content_type(info)?.to_string(),
    };
    let (tx, rx) = oneshot::channel();
    let start = QUEUE.with_borrow_mut(|q| {
        let waiting = q.waiters.entry(key).or_default();
        let first = waiting.is_empty();
        waiting.push(tx);
        if first {
            q.pending.push_back(job);
        }
        let start = q.running < workers() && !q.pending.is_empty();
        if start {
            q.running += 1;
        }
        start
    });
    if start {
        glib::spawn_future_local(run_queue());
    }
    rx.await.ok().flatten()
}

async fn run_queue() {
    loop {
        // Newest first: what was just scrolled into view.
        let Some(job) = QUEUE.with_borrow_mut(|q| loop {
            let Some(job) = q.pending.pop_back() else {
                q.running -= 1;
                return None;
            };
            // Every row that asked has scrolled away: don't make it.
            if abandoned(q, &job.key) {
                q.waiters.remove(&job.key);
                continue;
            }
            return Some(job);
        }) else {
            return;
        };
        let result = generate(&job).await;
        let waiters = QUEUE.with_borrow_mut(|q| q.waiters.remove(&job.key).unwrap_or_default());
        for tx in waiters {
            let _ = tx.send(result.clone());
        }
    }
}

/// Whether no one still waits for `key` (their rows were recycled).
fn abandoned(q: &Queue, key: &str) -> bool {
    q.waiters
        .get(key)
        .is_none_or(|w| w.iter().all(oneshot::Sender::is_canceled))
}

/// Resolves once no one waits for `key` any more.
async fn until_abandoned(key: &str) {
    loop {
        glib::timeout_future(Duration::from_millis(250)).await;
        if QUEUE.with_borrow(|q| abandoned(q, key)) {
            return;
        }
    }
}

// --------------------------------------------------------- rendering ----

/// Kills a renderer whose result is no longer wanted: dropping the render
/// (its row scrolled away) drops this. Harmless once the process has exited.
struct Reap<'a>(&'a gio::Subprocess);

impl Drop for Reap<'_> {
    fn drop(&mut self) {
        self.0.force_exit();
    }
}

fn os<S: AsRef<OsStr> + ?Sized>(s: &S) -> &OsStr {
    s.as_ref()
}

/// Runs a command, stopping it after `TIMEOUT`; true if it succeeded.
/// Starts a renderer in the background's place: lowered CPU priority
/// (`nice`), and one thread each for the tools that would otherwise use
/// every core per render, while several renders already run side by side.
fn spawn(argv: &[&OsStr], flags: gio::SubprocessFlags) -> Option<gio::Subprocess> {
    let launcher = gio::SubprocessLauncher::new(flags);
    launcher.setenv("MAGICK_THREAD_LIMIT", "1", true);
    launcher.setenv("OMP_NUM_THREADS", "1", true);
    let (program, rest) = argv.split_first()?;
    let mut full: Vec<&OsStr> = Vec::with_capacity(argv.len() + 5);
    if have("nice") {
        full.extend([os("nice"), os("-n"), os("10")]);
    }
    full.push(program);
    if *program == os("ffmpeg") {
        // As an input option: decode with one thread.
        full.extend([os("-threads"), os("1")]);
    }
    full.extend_from_slice(rest);
    launcher.spawn(&full).ok()
}

async fn run(argv: &[&OsStr]) -> bool {
    let flags = gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE;
    let Some(process) = spawn(argv, flags) else {
        return false;
    };
    let _reap = Reap(&process);
    let wait = process.wait_future();
    let timeout = glib::timeout_future(TIMEOUT);
    futures_util::pin_mut!(wait, timeout);
    if let futures_util::future::Either::Left((Ok(()), _)) =
        futures_util::future::select(wait, timeout).await
    {
        process.is_successful()
    } else {
        process.force_exit();
        false
    }
}

/// Runs a command and returns its standard output.
async fn output(argv: &[&OsStr]) -> Option<glib::Bytes> {
    let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE;
    let process = spawn(argv, flags)?;
    let _reap = Reap(&process);
    let (out, _) = process.communicate_future(None).await.ok()?;
    if process.is_successful() {
        out
    } else {
        None
    }
}

/// Seconds into a video to grab: 10% in, at most a minute.
async fn video_offset(path: &Path) -> f64 {
    let argv = [
        os("ffprobe"),
        os("-v"),
        os("error"),
        os("-show_entries"),
        os("format=duration"),
        os("-of"),
        os("csv=p=0"),
        path.as_os_str(),
    ];
    output(&argv)
        .await
        .and_then(|b| {
            std::str::from_utf8(&b)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
        })
        .map_or(0.0, |d| (d * 0.1).min(60.0))
}

async fn render_video(path: &Path, out: &Path, edge: u32) -> bool {
    let scale = format!("scale={edge}:{edge}:force_original_aspect_ratio=decrease");
    for at in [video_offset(path).await, 0.0] {
        let seek = format!("{at:.2}");
        let argv = [
            os("ffmpeg"),
            os("-nostdin"),
            os("-y"),
            os("-ss"),
            os(&seek),
            os("-i"),
            path.as_os_str(),
            os("-frames:v"),
            os("1"),
            os("-vf"),
            os(&scale),
            out.as_os_str(),
        ];
        if run(&argv).await && out.exists() {
            return true;
        }
    }
    false
}

async fn render_audio(path: &Path, out: &Path, edge: u32) -> bool {
    // Embedded cover art first.
    let cover = [
        os("ffmpeg"),
        os("-nostdin"),
        os("-y"),
        os("-i"),
        path.as_os_str(),
        os("-map"),
        os("0:v:0"),
        os("-frames:v"),
        os("1"),
        out.as_os_str(),
    ];
    if run(&cover).await && out.exists() {
        return true;
    }
    // Otherwise a waveform of the first two minutes, on the page colour.
    let h = edge / 2;
    let wave = format!(
        "showwavespic=s={edge}x{h}:colors=#c04fb0,pad={edge}:{edge}:0:{}:{PAPER}",
        (edge - h) / 2
    );
    let argv = [
        os("ffmpeg"),
        os("-nostdin"),
        os("-y"),
        os("-t"),
        os("120"),
        os("-i"),
        path.as_os_str(),
        os("-filter_complex"),
        os(&wave),
        os("-frames:v"),
        os("1"),
        out.as_os_str(),
    ];
    run(&argv).await && out.exists()
}

async fn render_pdf(path: &Path, out: &Path, edge: u32) -> bool {
    if have("pdftoppm") {
        // pdftoppm adds ".png" to the prefix it's given.
        let size = edge.to_string();
        let prefix = out.with_extension("");
        let argv = [
            os("pdftoppm"),
            os("-png"),
            os("-singlefile"),
            os("-f"),
            os("1"),
            os("-scale-to"),
            os(&size),
            path.as_os_str(),
            prefix.as_os_str(),
        ];
        return run(&argv).await && out.exists();
    }
    render_ghostscript(path, out).await
}

async fn render_ghostscript(path: &Path, out: &Path) -> bool {
    let target = format!("-sOutputFile={}", out.display());
    let argv = [
        os("gs"),
        os("-q"),
        os("-dSAFER"),
        os("-dBATCH"),
        os("-dNOPAUSE"),
        os("-sDEVICE=png16m"),
        os("-dFirstPage=1"),
        os("-dLastPage=1"),
        os("-r72"),
        os(&target),
        path.as_os_str(),
    ];
    run(&argv).await && out.exists()
}

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];

/// Cover of an EPUB or comic book: an entry named like a cover, else the
/// first image, extracted with bsdtar.
async fn render_ebook(path: &Path, out: &Path) -> bool {
    let Some(list) = output(&[os("bsdtar"), os("-tf"), path.as_os_str()]).await else {
        return false;
    };
    let list = String::from_utf8_lossy(&list);
    let mut images: Vec<&str> = list
        .lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            IMAGE_EXTS.iter().any(|e| lower.ends_with(&format!(".{e}")))
        })
        .collect();
    images.sort_unstable();
    let pick = images
        .iter()
        .find(|l| l.to_lowercase().contains("cover"))
        .or_else(|| images.first());
    let Some(entry) = pick else { return false };
    let Some(bytes) = output(&[os("bsdtar"), os("-xOf"), path.as_os_str(), os(entry)]).await else {
        return false;
    };
    std::fs::write(out, &bytes).is_ok()
}

fn monospace_font() -> Option<String> {
    use std::sync::OnceLock;
    static FONT: OnceLock<Option<String>> = OnceLock::new();
    FONT.get_or_init(|| {
        let out = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", "monospace"])
            .output()
            .ok()?;
        let path = String::from_utf8(out.stdout).ok()?;
        (!path.is_empty()).then_some(path)
    })
    .clone()
}

/// Draws lines of text on a page with ImageMagick.
async fn render_page(lines: &str, out: &Path, edge: u32) -> bool {
    let text_file = out.with_extension("txt");
    if std::fs::write(&text_file, lines).is_err() {
        return false;
    }
    let size = format!("{edge}x{edge}");
    let paper = format!("xc:{PAPER}");
    let point = (edge / 22).max(8).to_string();
    let label = format!("label:@{}", text_file.display());
    let margin = format!("+{m}+{m}", m = edge / 16);
    // Runs fc-match the first time; not on the UI thread.
    let font = gio::spawn_blocking(monospace_font).await.ok().flatten();
    let mut argv = vec![os("magick"), os("-size"), os(&size), os(&paper)];
    if let Some(font) = &font {
        argv.extend([os("-font"), os(font.as_str())]);
    }
    argv.extend([
        os("("),
        os("-background"),
        os("none"),
        os("-fill"),
        os(INK),
        os("-pointsize"),
        os(&point),
        os(&label),
        os(")"),
        os("-gravity"),
        os("northwest"),
        os("-geometry"),
        os(&margin),
        os("-composite"),
        out.as_os_str(),
    ]);
    let ok = run(&argv).await && out.exists();
    let _ = std::fs::remove_file(&text_file);
    ok
}

/// The first lines of a text file; None for binary content.
fn text_preview(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0; 4096];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    Some(page_lines(text.lines()))
}

/// At most 18 lines of 44 characters, tabs as four spaces.
fn page_lines<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    lines
        .take(18)
        .map(|l| l.replace('\t', "    ").chars().take(44).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn render_font(path: &Path, out: &Path, edge: u32) -> bool {
    let size = format!("{edge}x{edge}");
    let paper = format!("xc:{PAPER}");
    let point = (edge / 3).to_string();
    let argv = [
        os("magick"),
        os("-size"),
        os(&size),
        os(&paper),
        os("-font"),
        path.as_os_str(),
        os("-fill"),
        os(INK),
        os("-pointsize"),
        os(&point),
        os("-gravity"),
        os("center"),
        os("-annotate"),
        os("+0+0"),
        os("Aa"),
        out.as_os_str(),
    ];
    run(&argv).await && out.exists()
}

async fn render_system(job: &Job, out: &Path, edge: u32) -> bool {
    let Some(t) = system_thumbnailer(&job.content_type) else {
        return false;
    };
    let argv = command(&t.exec, job, out, edge);
    let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
    run(&argv).await && out.exists()
}

/// Splits a `.thumbnailer` Exec line and fills in %u %i %o %s.
fn command(exec: &str, job: &Job, out: &Path, edge: u32) -> Vec<String> {
    exec.split_whitespace()
        .map(|arg| {
            arg.replace("%u", &job.uri)
                .replace("%i", &job.path.to_string_lossy())
                .replace("%o", &out.to_string_lossy())
                .replace("%s", &edge.to_string())
                .replace("%%", "%")
        })
        .collect()
}

/// Renders the job to a temporary PNG (or picks the file itself for plain
/// images); None if nothing could be rendered.
async fn render(job: &Job, kind: Kind, tmp: &Path, edge: u32) -> Option<PathBuf> {
    let made = |ok: bool| ok.then(|| tmp.to_path_buf());
    match kind {
        // Images: the system thumbnailer (glycin) if there is one, else
        // gdk-pixbuf straight from the file in `save`, else ImageMagick.
        Kind::Image => {
            if render_system(job, tmp, edge).await {
                return Some(tmp.to_path_buf());
            }
            if pixbuf_loads(&job.content_type) || !have("magick") {
                return Some(job.path.clone());
            }
            let first_frame = format!("{}[0]", job.path.display());
            let geometry = format!("{edge}x{edge}>");
            let argv = [
                os("magick"),
                os(&first_frame),
                os("-thumbnail"),
                os(&geometry),
                tmp.as_os_str(),
            ];
            Some(if run(&argv).await {
                tmp.to_path_buf()
            } else {
                job.path.clone()
            })
        }
        Kind::Video => made(render_video(&job.path, tmp, edge).await),
        Kind::Audio => made(render_audio(&job.path, tmp, edge).await),
        Kind::Pdf => made(render_pdf(&job.path, tmp, edge).await),
        Kind::PostScript => made(render_ghostscript(&job.path, tmp).await),
        Kind::Ebook => made(render_ebook(&job.path, tmp).await),
        Kind::Font => made(render_font(&job.path, tmp, edge).await),
        Kind::System => made(render_system(job, tmp, edge).await),
        Kind::Text => {
            let path = job.path.clone();
            let lines = gio::spawn_blocking(move || text_preview(&path))
                .await
                .ok()??;
            made(render_page(&lines, tmp, edge).await)
        }
        Kind::Archive => {
            let list = output(&[os("bsdtar"), os("-tf"), job.path.as_os_str()]).await?;
            let list = String::from_utf8_lossy(&list).into_owned();
            made(render_page(&page_lines(list.lines()), tmp, edge).await)
        }
    }
}

/// Whether other programs would want this kind of thumbnail; Abyss' own
/// looks (text pages, archive listings, audio waveforms) stay private.
const fn shareable(kind: Kind) -> bool {
    !matches!(kind, Kind::Text | Kind::Archive | Kind::Audio)
}

async fn generate(job: &Job) -> Option<PathBuf> {
    let edge = edge();
    let kind = kind_for(&job.content_type)?;
    let tmp = std::env::temp_dir().join(format!(
        "abyss-thumb-{}-{}.png",
        std::process::id(),
        job.key
    ));
    let _ = std::fs::remove_file(&tmp);
    // Stop rendering (and kill the renderer) if the rows that asked are
    // gone. Only the render is cut short: a save in progress finishes, so
    // a cancelled render is never mistaken for a failed one.
    let rendered = {
        let render = render(job, kind, &tmp, edge);
        let gone = until_abandoned(&job.key);
        futures_util::pin_mut!(render, gone);
        match futures_util::future::select(render, gone).await {
            futures_util::future::Either::Left((rendered, _)) => rendered,
            futures_util::future::Either::Right(((), _)) => {
                let _ = std::fs::remove_file(&tmp);
                let _ = std::fs::remove_file(tmp.with_extension("txt"));
                return None;
            }
        }
    };
    // A render that produced nothing still records a failure below.
    let source = rendered.unwrap_or_else(|| tmp.clone());
    let out = if shareable(kind) {
        shared_target(&job.uri, edge)
    } else {
        cache_path(&job.key)
    };
    let fail = fail_path(&job.key);
    let stamp = Stamp {
        uri: job.uri.clone(),
        mtime: job.mtime,
        size: job.size,
    };
    let target = out.clone();
    let saved = gio::spawn_blocking(move || save(&source, &out, &fail, &stamp, edge))
        .await
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp);
    saved.then_some(target)
}

/// Scales `source` to fit `edge` and writes it to the cache (atomically,
/// private to the user as the thumbnail spec asks, with the spec's tags);
/// records a failure marker if it can't be read.
fn save(source: &Path, out: &Path, fail: &Path, stamp: &Stamp, edge: u32) -> bool {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let max_px = i32::try_from(edge).unwrap_or(256);
    // Read the size from the header and decode straight to the thumbnail's
    // size: the loader scales while decoding (JPEG decodes at a fraction of
    // full size), so a 12-megapixel photo is never held in memory whole.
    let pixbuf = gdk_pixbuf::Pixbuf::file_info(source).and_then(|(_, w, h)| {
        if w <= max_px && h <= max_px {
            gdk_pixbuf::Pixbuf::from_file(source).ok()
        } else {
            gdk_pixbuf::Pixbuf::from_file_at_scale(source, max_px, max_px, true).ok()
        }
    });
    let Some(pixbuf) = pixbuf else {
        if let Some(parent) = fail.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(fail, b"");
        }
        return false;
    };
    let Some(parent) = out.parent() else {
        return false;
    };
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .is_err()
    {
        return false;
    }
    let partial = out.with_extension(format!("png.{}.tmp", std::process::id()));
    let (mtime, size) = (stamp.mtime.to_string(), stamp.size.to_string());
    let tags = [
        ("tEXt::Thumb::URI", stamp.uri.as_str()),
        ("tEXt::Thumb::MTime", &mtime),
        ("tEXt::Thumb::Size", &size),
        ("tEXt::Software", "Abyss"),
    ];
    if pixbuf.savev(&partial, "png", &tags).is_err()
        || std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o600)).is_err()
    {
        let _ = std::fs::remove_file(&partial);
        return false;
    }
    std::fs::rename(&partial, out).is_ok()
}

/// Decoded thumbnails by file and size, oldest first out once over
/// `TEXTURE_BYTES`.
#[derive(Default)]
struct Textures {
    map: HashMap<(PathBuf, i32), gdk::Texture>,
    order: VecDeque<(PathBuf, i32)>,
    bytes: usize,
}

fn texture_bytes(t: &gdk::Texture) -> usize {
    let px = |n: i32| usize::try_from(n).unwrap_or(0);
    px(t.width()) * px(t.height()) * 4
}

/// Reads a thumbnail at most `px` device pixels on its longer side (the
/// cache holds 256 to 1024 px; a list icon needs a fraction of that).
fn decode(path: &Path, px: i32) -> Option<gdk::Texture> {
    let (_, w, h) = gdk_pixbuf::Pixbuf::file_info(path)?;
    let pixbuf = if w > px || h > px {
        gdk_pixbuf::Pixbuf::from_file_at_scale(path, px, px, true)
    } else {
        gdk_pixbuf::Pixbuf::from_file(path)
    }
    .ok()?;
    let format = if pixbuf.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    let stride = usize::try_from(pixbuf.rowstride()).ok()?;
    Some(
        gdk::MemoryTexture::new(
            pixbuf.width(),
            pixbuf.height(),
            format,
            &pixbuf.read_pixel_bytes(),
            stride,
        )
        .upcast(),
    )
}

/// Loads a thumbnail as a texture of at most `px` device pixels, from a
/// small in-memory cache when possible. Reading, decoding and scaling
/// happen on a worker thread.
pub async fn texture(path: PathBuf, px: i32) -> Option<gdk::Texture> {
    let px = px.clamp(16, 1024);
    let key = (path, px);
    if let Some(t) = TEXTURES.with_borrow(|c| c.map.get(&key).cloned()) {
        return Some(t);
    }
    let file = key.0.clone();
    let texture = crate::par::blocking(move |stop| {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            None
        } else {
            decode(&file, px)
        }
    })
    .await??;
    TEXTURES.with_borrow_mut(|c| {
        let size = texture_bytes(&texture);
        if let Some(old) = c.map.insert(key.clone(), texture.clone()) {
            c.bytes -= texture_bytes(&old);
        } else {
            c.order.push_back(key);
        }
        c.bytes += size;
        while c.bytes > TEXTURE_BYTES {
            let Some(old) = c.order.pop_front() else {
                break;
            };
            if let Some(t) = c.map.remove(&old) {
                c.bytes -= texture_bytes(&t);
            }
        }
    });
    Some(texture)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(path: &Path, content_type: &str) -> Job {
        let uri = format!("file://{}", path.display());
        Job {
            key: key(&uri, 1, 2),
            uri,
            mtime: 1,
            size: 2,
            path: path.to_path_buf(),
            content_type: content_type.to_owned(),
        }
    }

    fn run_local<T>(fut: impl std::future::Future<Output = T>) -> T {
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| ctx.block_on(fut)).unwrap()
    }

    #[test]
    fn keys_change_with_the_file() {
        assert_eq!(key("file:///a", 1, 2), key("file:///a", 1, 2));
        assert_ne!(key("file:///a", 1, 2), key("file:///a", 3, 2));
        assert_ne!(key("file:///a", 1, 2), key("file:///a", 1, 5));
    }

    #[test]
    fn exec_substitution() {
        let j = job(Path::new("/a b.png"), "image/png");
        let argv = command(
            "thumb --input %u --output %o --size %s -i %i",
            &j,
            Path::new("/tmp/o.png"),
            256,
        );
        assert_eq!(argv[2], "file:///a b.png");
        assert_eq!(
            &argv[3..],
            ["--output", "/tmp/o.png", "--size", "256", "-i", "/a b.png"]
        );
    }

    #[test]
    fn page_lines_are_trimmed() {
        let text = "a\tb\n".to_owned() + &"x".repeat(80) + "\n" + &"line\n".repeat(40);
        let page = page_lines(text.lines());
        let lines: Vec<&str> = page.lines().collect();
        assert_eq!(lines.len(), 18);
        assert_eq!(lines[0], "a    b");
        assert_eq!(lines[1].len(), 44);
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_for("image/png"), Some(Kind::Image));
        if have("magick") {
            assert_eq!(kind_for("text/x-rust"), Some(Kind::Text));
            assert_eq!(kind_for("application/json"), Some(Kind::Text));
        }
        if have("ffmpeg") {
            assert_eq!(kind_for("video/mp4"), Some(Kind::Video));
            assert_eq!(kind_for("audio/flac"), Some(Kind::Audio));
        }
        assert_eq!(kind_for("application/x-sharedlib"), None);
    }

    #[test]
    fn renders_each_kind() {
        let dir = std::env::temp_dir().join(format!("abyss-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = |name: &str| dir.join(format!("{name}.png"));
        let ffmpeg = |args: &[&str], file: &Path| {
            have("ffmpeg")
                && std::process::Command::new("ffmpeg")
                    .args(["-nostdin", "-loglevel", "error"])
                    .args(args)
                    .arg(file)
                    .status()
                    .is_ok_and(|s| s.success())
        };
        let video = dir.join("clip.mp4");
        let audio = dir.join("tone.wav");
        let have_video = ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10",
                "-t",
                "2",
            ],
            &video,
        );
        let have_audio = ffmpeg(
            &["-f", "lavfi", "-i", "sine=frequency=440:duration=1"],
            &audio,
        );
        run_local(async {
            if have("magick") {
                assert!(
                    render_page("fn main() {\n    println!(\"hi\");\n}", &out("page"), 256).await
                );
                let img = gdk_pixbuf::Pixbuf::from_file(out("page")).unwrap();
                assert_eq!((img.width(), img.height()), (256, 256));
            }
            if have_video {
                assert!(render_video(&video, &out("video"), 256).await);
            }
            if have_audio {
                assert!(render_audio(&audio, &out("audio"), 256).await);
            }
        });
        // Unreadable source: no thumbnail, a failure marker instead.
        std::fs::write(dir.join("bad.png"), b"nope").unwrap();
        let stamp = Stamp {
            uri: "file:///bad".into(),
            mtime: 5,
            size: 4,
        };
        assert!(!save(
            &dir.join("bad.png"),
            &out("x"),
            &dir.join("fail"),
            &stamp,
            256
        ));
        assert!(dir.join("fail").exists());
        // A good save carries the spec's tags, which make it current only
        // for that version of the file.
        if have("magick") {
            assert!(save(
                &out("page"),
                &out("tagged"),
                &dir.join("f2"),
                &stamp,
                256
            ));
            assert!(shared_is_current(&out("tagged"), &stamp));
            let newer = Stamp { mtime: 6, ..stamp };
            assert!(!shared_is_current(&out("tagged"), &newer));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
