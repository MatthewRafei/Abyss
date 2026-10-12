//! Sidebar sources. The volume/mount grouping follows `update_places()` in
//! src/nautilus-sidebar.c; Tailscale and Syncthing are Abyss additions and
//! only appear when those tools are installed.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Places,
    Devices,
    Network,
    Synced,
}

impl Section {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Places => "Places",
            Self::Devices => "Devices",
            Self::Network => "Network",
            Self::Synced => "Synced",
        }
    }
}

#[derive(Clone)]
pub enum Target {
    /// Open this location.
    Uri(String),
    /// Mount first, then open the mount root.
    Volume(gio::Volume),
    /// Ask for a server's address (Add Server).
    AddServer,
}

#[derive(Clone)]
pub struct Place {
    pub section: Section,
    pub name: String,
    pub icon: String,
    pub target: Target,
    /// Mount that can be unmounted/ejected from the row.
    pub mount: Option<gio::Mount>,
    /// Extra locations for the row's menu, e.g. SMB for a Tailscale peer.
    pub alternates: Vec<(String, String)>,
    pub dim: bool,
    /// A Syncthing folder (badge on Places rows that are also synced).
    pub synced: bool,
    /// Pinned rows keep their place and can't be dragged.
    pub pin: Pin,
    /// A second, smaller line saying what the row is.
    pub detail: Option<String>,
    pub tooltip: Option<String>,
    /// A server saved in GTK's servers file (it can be forgotten).
    pub saved_server: bool,
}

/// Where a row sits in its section, before the saved order applies.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Pin {
    First,
    None,
    Last,
}

impl Place {
    fn new(section: Section, name: &str, icon: &str, target: Target) -> Self {
        Self {
            section,
            name: name.to_owned(),
            icon: icon.to_owned(),
            target,
            mount: None,
            alternates: Vec::new(),
            dim: false,
            synced: false,
            pin: Pin::None,
            detail: None,
            tooltip: None,
            saved_server: false,
        }
    }

    fn detail(mut self, detail: &str) -> Self {
        self.detail = Some(detail.to_owned());
        self
    }

    fn tooltip(mut self, tooltip: &str) -> Self {
        self.tooltip = Some(tooltip.to_owned());
        self
    }

    const fn pinned(mut self) -> Self {
        self.pin = Pin::First;
        self
    }

    const fn pinned_last(mut self) -> Self {
        self.pin = Pin::Last;
        self
    }

    /// Stable identity for saving the sidebar order.
    pub fn key(&self) -> String {
        match &self.target {
            Target::Uri(uri) => uri.trim_end_matches('/').to_owned(),
            Target::Volume(v) => format!(
                "volume:{}",
                v.identifier("uuid")
                    .map_or_else(|| v.name().to_string(), |u| u.to_string())
            ),
            Target::AddServer => "add-server".to_owned(),
        }
    }
}

fn home() -> std::path::PathBuf {
    glib::home_dir()
}

fn uri_of(path: &std::path::Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

pub fn fixed_places() -> Vec<Place> {
    let mut out = vec![Place::new(
        Section::Places,
        "Home",
        "user-home-symbolic",
        Target::Uri(uri_of(&home())),
    )
    .pinned()];
    out.push(
        Place::new(
            Section::Places,
            "Recent",
            "document-open-recent-symbolic",
            Target::Uri("recent:///".into()),
        )
        .pinned(),
    );
    out.push(
        Place::new(
            Section::Places,
            "Starred",
            "starred-symbolic",
            Target::Uri(crate::starred::VIEW.into()),
        )
        .pinned(),
    );
    out.extend(bookmarks());
    out.push(
        Place::new(
            Section::Places,
            "Trash",
            "user-trash-symbolic",
            Target::Uri("trash:///".into()),
        )
        .pinned_last(),
    );
    out
}

/// GTK bookmarks ("uri [label]" per line), the same file Nautilus and the
/// GTK file chooser share (nautilus-bookmark-list.c).
pub fn bookmarks_file() -> std::path::PathBuf {
    glib::user_config_dir().join("gtk-3.0").join("bookmarks")
}

fn bookmarks() -> Vec<Place> {
    let Ok(text) = std::fs::read_to_string(bookmarks_file()) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (uri, label) = match line.split_once(' ') {
                Some((u, l)) => (u.to_owned(), l.to_owned()),
                None => (line.to_owned(), String::new()),
            };
            let file = gio::File::for_uri(&uri);
            let name = if label.is_empty() {
                file.basename()
                    .map_or_else(|| uri.clone(), |b| b.to_string_lossy().into_owned())
            } else {
                label
            };
            let icon = if file.is_native() {
                "folder-symbolic"
            } else {
                "folder-remote-symbolic"
            };
            Place::new(Section::Places, &name, icon, Target::Uri(uri))
        })
        .collect()
}

pub fn add_bookmark(file: &gio::File) -> std::io::Result<()> {
    use std::io::Write;
    let path = bookmarks_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let uri = file.uri();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.split(' ').next() == Some(uri.as_str()))
    {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "{uri}")
}

pub fn remove_bookmark(uri: &str) -> std::io::Result<()> {
    let path = bookmarks_file();
    let text = std::fs::read_to_string(&path)?;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| l.split(' ').next() != Some(uri))
        .collect();
    std::fs::write(path, kept.join("\n") + "\n")
}

pub fn is_bookmark(uri: &str) -> bool {
    std::fs::read_to_string(bookmarks_file())
        .is_ok_and(|t| t.lines().any(|l| l.split(' ').next() == Some(uri)))
}

fn icon_name(icon: Option<gio::Icon>, fallback: &str) -> String {
    icon.and_then(|i| i.downcast::<gio::ThemedIcon>().ok())
        .and_then(|t| t.names().first().map(ToString::to_string))
        .unwrap_or_else(|| fallback.to_owned())
}

fn is_network_volume(volume: &gio::Volume) -> bool {
    volume.identifier("class").as_deref() == Some("network")
}

/// Drives, volumes and mounts, split into local devices and network shares.
pub fn volumes(monitor: &gio::VolumeMonitor) -> Vec<Place> {
    let mut out = vec![Place::new(
        Section::Devices,
        "Filesystem",
        "drive-harddisk-symbolic",
        Target::Uri("file:///".into()),
    )
    .pinned()];

    let mut seen_volumes = Vec::new();
    for mount in monitor.mounts() {
        if mount.is_shadowed() {
            continue;
        }
        let root = mount.root();
        if let Some(path) = root.path() {
            // "/" already has its own Filesystem row.
            if path == std::path::Path::new("/") {
                continue;
            }
        }
        let volume = mount.volume();
        let network = !root.is_native() || volume.as_ref().is_some_and(is_network_volume);
        if let Some(v) = volume {
            seen_volumes.push(v);
        }
        let section = if network {
            Section::Network
        } else {
            Section::Devices
        };
        let fallback = if network {
            "folder-remote-symbolic"
        } else {
            "drive-removable-media-symbolic"
        };
        let mut place = Place::new(
            section,
            &mount.name(),
            &icon_name(Some(mount.symbolic_icon()), fallback),
            Target::Uri(root.uri().to_string()),
        );
        if mount.can_unmount() || mount.can_eject() {
            place.mount = Some(mount);
        }
        out.push(place);
    }

    // Volumes that are not mounted yet: clicking mounts them.
    for volume in monitor.volumes() {
        if volume.get_mount().is_some() || seen_volumes.contains(&volume) {
            continue;
        }
        let section = if is_network_volume(&volume) {
            Section::Network
        } else {
            Section::Devices
        };
        out.push(Place::new(
            section,
            &volume.name(),
            &icon_name(
                Some(volume.symbolic_icon()),
                "drive-removable-media-symbolic",
            ),
            Target::Volume(volume),
        ));
    }

    out
}

/// The Network section's own rows, after the servers and shares: what's
/// discoverable nearby, and adding a server by its address.
pub fn network_actions() -> Vec<Place> {
    vec![
        Place::new(
            Section::Network,
            "Nearby",
            "network-workgroup-symbolic",
            Target::Uri("network:///".into()),
        )
        .detail("Computers on this network")
        .tooltip("Computers and shares that announce themselves on your local network")
        .pinned_last(),
        Place::new(
            Section::Network,
            "Add Server…",
            "list-add-symbolic",
            Target::AddServer,
        )
        .detail("By address: SMB, SFTP…")
        .tooltip(
            "Connect to a server by its address (SMB, SFTP, WebDAV, FTP or NFS) and keep it here",
        )
        .pinned_last(),
    ]
}

// ------------------------------------------------------ saved servers ----

/// Servers the user connected to, in the file GTK's file chooser (and
/// Nautilus) keep them in.
pub fn servers_file() -> std::path::PathBuf {
    glib::user_config_dir().join("gtk-4.0").join("servers")
}

fn load_servers(path: &std::path::Path) -> glib::BookmarkFile {
    let mut file = glib::BookmarkFile::new();
    let _ = file.load_from_file(path);
    file
}

/// A server's row name: its host, and the folder if one was given.
pub fn server_name(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    // No user name in the sidebar.
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let rest = rest.trim_end_matches('/');
    glib::uri_unescape_string(rest, None::<&str>).map_or_else(|| rest.to_owned(), |s| s.to_string())
}

/// Saved servers, most recently added last.
pub fn saved_servers() -> Vec<Place> {
    servers_in(&servers_file())
}

fn servers_in(path: &std::path::Path) -> Vec<Place> {
    let file = load_servers(path);
    let mut uris: Vec<_> = file.uris().into_iter().map(|u| u.to_string()).collect();
    uris.sort_by_key(|u| file.added_date_time(u).map_or(0, |d| d.to_unix()));
    uris.into_iter()
        .map(|uri| {
            let mut place = Place::new(
                Section::Network,
                &server_name(&uri),
                "network-server-symbolic",
                Target::Uri(uri.clone()),
            )
            .tooltip(&uri);
            place.saved_server = true;
            place
        })
        .collect()
}

/// Remembers a server the user connected to (marks it visited if known).
pub fn save_server(uri: &str) -> Result<(), glib::Error> {
    save_server_in(&servers_file(), uri)
}

fn save_server_in(path: &std::path::Path, uri: &str) -> Result<(), glib::Error> {
    let mut file = load_servers(path);
    let now = glib::DateTime::now_utc()
        .map_err(|e| glib::Error::new(glib::FileError::Failed, &e.message))?;
    if !file.has_item(uri) {
        file.set_added_date_time(uri, &now);
        file.set_title(Some(uri), &server_name(uri));
    }
    file.set_visited_date_time(uri, &now);
    // An entry no application claims isn't written out at all.
    file.add_application(uri, Some("Abyss"), Some("abyss %u"));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    file.to_file(path)
}

pub fn forget_server(uri: &str) -> Result<(), glib::Error> {
    forget_server_in(&servers_file(), uri)
}

fn forget_server_in(path: &std::path::Path, uri: &str) -> Result<(), glib::Error> {
    let mut file = load_servers(path);
    file.remove_item(uri)?;
    file.to_file(path)
}

// ---------------------------------------------------- mounted shares ----

/// A network share the system mounts into the file tree (fstab, autofs).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Share {
    host: String,
    at: std::path::PathBuf,
}

const NETWORK_FS: [&str; 9] = [
    "cifs",
    "smb3",
    "nfs",
    "nfs4",
    "sshfs",
    "fuse.sshfs",
    "davfs",
    "fuse.rclone",
    "9p",
];

/// Unescapes the octal escapes (`\040` for a space) of /proc/mounts.
fn unescape_mount(field: &str) -> String {
    let mut out = Vec::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        if let (b'\\', Some(digits)) = (bytes[i], octal) {
            out.push(
                digits
                    .iter()
                    .fold(0u8, |v, d| v.wrapping_mul(8).wrapping_add(d - b'0')),
            );
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The server in a mount source: `//host/share`, `host:/path`,
/// `user@host:path`.
fn source_host(source: &str) -> Option<String> {
    let host = if let Some(rest) = source.strip_prefix("//") {
        rest.split('/').next()?
    } else {
        let (before, _) = source.split_once(':')?;
        before.rsplit_once('@').map_or(before, |(_, h)| h)
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// Network shares from /proc/mounts: mounted ones, and autofs mount points
/// (which unmount when idle) looked up in their direct map.
fn shares_from(mounts: &str, read_map: impl Fn(&str) -> Option<String>) -> Vec<Share> {
    let mut out: Vec<Share> = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(at), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let at = std::path::PathBuf::from(unescape_mount(at));
        let host = if NETWORK_FS.contains(&fstype) {
            source_host(&unescape_mount(source))
        } else if fstype == "autofs" {
            // Direct maps: "/mount/point -options source".
            read_map(source).and_then(|map| {
                map.lines().find_map(|l| {
                    let mut f = l.split_whitespace();
                    let key = f.next()?;
                    if std::path::Path::new(key) != at {
                        return None;
                    }
                    let rest: Vec<&str> = f.collect();
                    let network = rest.iter().any(|o| {
                        o.starts_with('-')
                            && NETWORK_FS
                                .iter()
                                .any(|fs| o.contains(&format!("fstype={fs}")))
                    });
                    let source = rest.last()?.trim_start_matches(':');
                    network.then(|| source_host(source)).flatten()
                })
            })
        } else {
            None
        };
        if let Some(host) = host {
            if !out.iter().any(|s| s.at == at) {
                out.push(Share { host, at });
            }
        }
    }
    out
}

/// Rows for system-mounted shares: one per server when its shares sit
/// together in one folder (it opens that folder), else one per share.
fn share_places(shares: &[Share], home: &std::path::Path) -> Vec<Place> {
    let show = |p: &std::path::Path| {
        p.strip_prefix(home).map_or_else(
            |_| p.display().to_string(),
            |r| format!("~/{}", r.display()),
        )
    };
    let mut hosts: Vec<&str> = shares.iter().map(|s| s.host.as_str()).collect();
    hosts.dedup();
    let mut seen = std::collections::HashSet::new();
    hosts.retain(|h| seen.insert(*h));
    let mut out = Vec::new();
    for host in hosts {
        let mine: Vec<&Share> = shares.iter().filter(|s| s.host == host).collect();
        let parent = mine[0].at.parent().map(std::path::Path::to_path_buf);
        let together = mine.len() > 1
            && parent.as_ref().is_some_and(|p| {
                p != std::path::Path::new("/")
                    && p != home
                    && mine.iter().all(|s| s.at.parent() == Some(p.as_path()))
            });
        if let (true, Some(parent)) = (together, parent) {
            let detail = format!("{} shares in {}", mine.len(), show(&parent));
            out.push(
                Place::new(
                    Section::Network,
                    host,
                    "network-server-symbolic",
                    Target::Uri(uri_of(&parent)),
                )
                .detail(&detail)
                .tooltip(&format!("Shares from {host}, mounted in {}", show(&parent))),
            );
        } else {
            for share in mine {
                let name = share
                    .at
                    .file_name()
                    .map_or_else(|| show(&share.at), |n| n.to_string_lossy().into_owned());
                out.push(
                    Place::new(
                        Section::Network,
                        &name,
                        "folder-remote-symbolic",
                        Target::Uri(uri_of(&share.at)),
                    )
                    .detail(&format!("on {host}"))
                    .tooltip(&format!(
                        "Share from {host}, mounted at {}",
                        show(&share.at)
                    )),
                );
            }
        }
    }
    out
}

/// Network shares the system mounts into the file tree, as sidebar rows.
pub fn mounted_shares() -> Vec<Place> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let read_map = |source: &str| {
        let path = std::path::Path::new(source);
        path.is_absolute()
            .then(|| std::fs::read_to_string(path).ok())
            .flatten()
    };
    share_places(&shares_from(&mounts, read_map), &home())
}

/// Number of items in the trash (0 when it can't be read).
pub fn trash_count() -> u32 {
    gio::File::for_uri("trash:///")
        .query_info(
            "trash::item-count",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .map_or(0, |i| i.attribute_uint32("trash::item-count"))
}

fn order_file() -> std::path::PathBuf {
    glib::user_config_dir().join("abyss").join("sidebar-order")
}

/// Saved order: (section title, key) pairs, one "section<TAB>key" per line.
pub fn load_order() -> Vec<(String, String)> {
    std::fs::read_to_string(order_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(s, k)| (s.to_owned(), k.to_owned()))
        .collect()
}

/// Replaces the saved order of one section.
pub fn save_order(section: Section, keys: &[String]) -> std::io::Result<()> {
    let mut lines: Vec<String> = load_order()
        .into_iter()
        .filter(|(s, _)| s != section.title())
        .map(|(s, k)| format!("{s}\t{k}"))
        .collect();
    lines.extend(keys.iter().map(|k| format!("{}\t{k}", section.title())));
    let path = order_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, lines.join("\n") + "\n")
}

/// Sorts by section, then pinned rows, then the saved order; rows with no
/// saved position keep their natural order after the saved ones.
pub fn apply_order(all: &mut [Place], order: &[(String, String)]) {
    let sections = [
        Section::Places,
        Section::Synced,
        Section::Devices,
        Section::Network,
    ];
    let rank = |p: &Place| {
        let saved = order
            .iter()
            .filter(|(s, _)| s == p.section.title())
            .position(|(_, k)| *k == p.key())
            .unwrap_or(usize::MAX);
        // Pinned rows ignore the saved order.
        let saved = if p.pin == Pin::None { saved } else { 0 };
        (sections.iter().position(|s| *s == p.section), p.pin, saved)
    };
    all.sort_by_key(rank);
}

/// Rewrites the GTK bookmarks file in the given order (Nautilus does the
/// same when bookmarks are dragged), keeping labels and unknown lines.
pub fn reorder_bookmarks(uris: &[String]) -> std::io::Result<()> {
    let path = bookmarks_file();
    let text = std::fs::read_to_string(&path)?;
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let pos = |l: &&str| {
        let uri = l.split(' ').next().unwrap_or("").trim_end_matches('/');
        uris.iter()
            .position(|u| u.trim_end_matches('/') == uri)
            .unwrap_or(usize::MAX)
    };
    lines.sort_by_key(pos);
    std::fs::write(path, lines.join("\n") + "\n")
}

pub fn have_command(name: &str) -> bool {
    glib::find_program_in_path(name).is_some()
}

/// Peers from `tailscale status --json`, online first. Windows hosts open
/// over SMB, everything else over SFTP; the other protocol is in the menu.
pub async fn tailscale_peers() -> Vec<Place> {
    if !have_command("tailscale") {
        return Vec::new();
    }
    let Ok(proc) = gio::Subprocess::newv(
        &["tailscale".as_ref(), "status".as_ref(), "--json".as_ref()],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    ) else {
        return Vec::new();
    };
    let Ok((Some(stdout), _)) = proc.communicate_utf8_future(None).await else {
        return Vec::new();
    };
    parse_tailscale(&stdout)
}

fn parse_tailscale(json: &str) -> Vec<Place> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    if v["BackendState"].as_str() != Some("Running") {
        return Vec::new();
    }
    let Some(peers) = v["Peer"].as_object() else {
        return Vec::new();
    };
    let mut out: Vec<Place> = peers
        .values()
        .filter_map(|p| {
            let host = p["DNSName"].as_str()?.trim_end_matches('.');
            if host.is_empty() {
                return None;
            }
            let name = p["HostName"].as_str().unwrap_or(host);
            let online = p["Online"].as_bool().unwrap_or(false);
            let windows = p["OS"].as_str() == Some("windows");
            let sftp = format!("sftp://{host}/");
            let smb = format!("smb://{host}/");
            let (main, alt) = if windows { (smb, sftp) } else { (sftp, smb) };
            let alt_label = if windows {
                "Open over SFTP"
            } else {
                "Open over SMB"
            };
            let mut place = Place::new(
                Section::Network,
                name,
                "network-server-symbolic",
                Target::Uri(main),
            );
            place.alternates.push((alt_label.into(), alt));
            place.dim = !online;
            Some(place)
        })
        .collect();
    out.sort_by(|a, b| {
        a.dim
            .cmp(&b.dim)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// Folders from Syncthing's config.xml.
pub fn syncthing_folders() -> Vec<Place> {
    if !have_command("syncthing") {
        return Vec::new();
    }
    let candidates = [
        glib::user_state_dir().join("syncthing/config.xml"),
        glib::user_config_dir().join("syncthing/config.xml"),
    ];
    let Some(text) = candidates
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
    else {
        return Vec::new();
    };
    parse_syncthing(&text)
}

fn parse_syncthing(xml: &str) -> Vec<Place> {
    xml.split("<folder ")
        .skip(1)
        .filter_map(|chunk| {
            let tag = &chunk[..chunk.find('>')?];
            // The <defaults> template folder has an empty path.
            let path = xml_attr(tag, "path").filter(|p| !p.is_empty())?;
            let label = xml_attr(tag, "label").filter(|l| !l.is_empty());
            let path = path
                .strip_prefix("~/")
                .map_or_else(|| std::path::PathBuf::from(&path), |rest| home().join(rest));
            let name = label.unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            Some(Place::new(
                Section::Synced,
                &name,
                "emblem-synchronizing-symbolic",
                Target::Uri(uri_of(&path)),
            ))
        })
        .collect()
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = format!(" {tag}").find(&needle)? + needle.len() - 1;
    let rest = &tag[start..];
    let value = &rest[..rest.find('"')?];
    Some(
        value
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncthing() {
        let xml = r#"<configuration><folder id="a" label="Docs &amp; Notes" path="~/Documents" type="x"><device/></folder><folder id="b" label="" path="/srv/cam"></folder></configuration>"#;
        let places = parse_syncthing(xml);
        assert_eq!(places.len(), 2);
        assert_eq!(places[0].name, "Docs & Notes");
        assert_eq!(places[1].name, "cam");
        assert!(matches!(&places[1].target, Target::Uri(u) if u == "file:///srv/cam"));
    }

    #[test]
    fn ordering() {
        let uri = |u: &str| Target::Uri(u.into());
        let mut all = vec![
            Place::new(Section::Network, "peer", "x", uri("sftp://peer/")),
            Place::new(Section::Places, "Home", "x", uri("file:///h")).pinned(),
            Place::new(Section::Places, "a", "x", uri("file:///a")),
            Place::new(Section::Places, "b", "x", uri("file:///b")),
            Place::new(Section::Places, "Trash", "x", uri("trash:///")).pinned_last(),
            Place::new(Section::Places, "new", "x", uri("file:///new")),
            Place::new(Section::Network, "Nearby", "x", uri("network:///")).pinned_last(),
        ];
        let order = vec![
            ("Places".to_string(), "file:///b".to_string()),
            ("Places".to_string(), "file:///a".to_string()),
            // A pinned row in the saved order still stays first.
            ("Places".to_string(), "file:///h".to_string()),
            ("Places".to_string(), "trash:///".to_string()),
        ];
        apply_order(&mut all, &order);
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Home", "b", "a", "new", "Trash", "peer", "Nearby"]);
    }

    #[test]
    fn tailscale() {
        let json = r#"{"BackendState":"Running","Peer":{
            "k1":{"HostName":"box","DNSName":"box.ts.net.","Online":false,"OS":"linux"},
            "k2":{"HostName":"Win","DNSName":"win.ts.net.","Online":true,"OS":"windows"}}}"#;
        let peers = parse_tailscale(json);
        assert_eq!(peers[0].name, "Win");
        assert!(matches!(&peers[0].target, Target::Uri(u) if u == "smb://win.ts.net/"));
        assert!(peers[1].dim);
        assert!(parse_tailscale(r#"{"BackendState":"Stopped"}"#).is_empty());
    }

    #[test]
    fn network_shares() {
        let mounts = "\
/etc/auto.nas /home/u/Nas/Media autofs rw,fd=7 0 0
//nas/Media /home/u/Nas/Media cifs rw,vers=3.1.1 0 0
box:/srv/x /mnt/x nfs4 rw 0 0
me@pi:/home/me /home/u/pi fuse.sshfs rw 0 0
/dev/sda1 /data ext4 rw 0 0
/etc/auto.misc /misc autofs rw 0 0
";
        let map = "\
# direct map
/home/u/Nas/Media -fstype=cifs,credentials=/x ://nas/Media
/home/u/local -fstype=ext4 :/dev/sdb1
";
        let read = |src: &str| (src == "/etc/auto.nas").then(|| map.to_owned());
        let shares = shares_from(mounts, read);
        let at: Vec<_> = shares
            .iter()
            .map(|s| (s.host.as_str(), s.at.to_str().unwrap()))
            .collect();
        assert_eq!(
            at,
            [
                ("nas", "/home/u/Nas/Media"),
                ("box", "/mnt/x"),
                ("pi", "/home/u/pi"),
            ]
        );
        // Two shares of one server in one folder: a single row for it.
        let two = [
            Share {
                host: "nas".into(),
                at: "/home/u/Nas/a".into(),
            },
            Share {
                host: "nas".into(),
                at: "/home/u/Nas/b".into(),
            },
            Share {
                host: "box".into(),
                at: "/mnt/x".into(),
            },
        ];
        let rows = share_places(&two, std::path::Path::new("/home/u"));
        let names: Vec<_> = rows
            .iter()
            .map(|p| (p.name.as_str(), p.detail.as_deref()))
            .collect();
        assert_eq!(
            names,
            [("nas", Some("2 shares in ~/Nas")), ("x", Some("on box"))]
        );
        assert!(matches!(&rows[0].target, Target::Uri(u) if u == "file:///home/u/Nas"));
    }

    #[test]
    fn saved_servers_file() {
        let dir = std::env::temp_dir().join(format!("abyss-servers-{}", std::process::id()));
        let path = dir.join("gtk-4.0/servers");
        save_server_in(&path, "smb://nas/").unwrap();
        save_server_in(&path, "sftp://me@box/srv/").unwrap();
        // Saving again only marks it visited.
        save_server_in(&path, "smb://nas/").unwrap();
        let names: Vec<_> = servers_in(&path).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["nas", "box/srv"]);
        assert!(servers_in(&path).iter().all(|p| p.saved_server));
        forget_server_in(&path, "smb://nas/").unwrap();
        let names: Vec<_> = servers_in(&path).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["box/srv"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn server_names() {
        assert_eq!(server_name("smb://10.0.90.210/"), "10.0.90.210");
        assert_eq!(
            server_name("sftp://me@box:22/srv/My%20Files/"),
            "box:22/srv/My Files"
        );
        assert_eq!(unescape_mount(r"/a\040b\\c"), r"/a b\\c");
    }
}
