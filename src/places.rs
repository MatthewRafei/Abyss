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
    /// Prefill the location entry (Connect to Server).
    Prompt(String),
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
        }
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
            Target::Prompt(p) => format!("prompt:{p}"),
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

    out.push(
        Place::new(
            Section::Network,
            "Browse Network",
            "network-workgroup-symbolic",
            Target::Uri("network:///".into()),
        )
        .pinned(),
    );
    out.push(
        Place::new(
            Section::Network,
            "Connect to Server…",
            "network-server-symbolic",
            Target::Prompt("smb://".into()),
        )
        .pinned(),
    );
    out
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
            Place::new(Section::Network, "Browse", "x", uri("network:///")).pinned(),
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
        assert_eq!(names, ["Home", "b", "a", "new", "Trash", "Browse", "peer"]);
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
}
