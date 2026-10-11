<p align="center">
  <img src="docs/banner.png" alt="Abyss — a small file browser from the deep" width="100%">
</p>

<p align="center">
  <img src="docs/overview.png" alt="The Abyss window: the home folder, a sidebar with bookmarks, a synced folder, devices and Tailscale peers, and the octopus logo at the bottom" width="88%">
</p>

> [!WARNING]
> **Vibe coded.** Abyss was written almost entirely by an AI coding assistant
> (Claude), steered, reviewed and tested by a human on one machine. The logic
> is ported from Nautilus and the code is tested and linted strictly, but it
> hasn't had the years of real-world use a file manager deserves. Keep backups,
> and read the code before trusting it with anything you can't lose.

**Abyss** is a minimal GTK4 file browser written in Rust. It started as a fork
of GNOME Files (Nautilus) and ports its logic from the Nautilus sources (see
[below](#where-the-logic-came-from)), but none of the Nautilus C code is
shipped here. It's built for a tiling Wayland desktop (niri), follows the
wallpaper-driven theme of the rice it was made for, and puts network shares,
Tailscale peers and Syncthing folders right in the sidebar.

## Tour

**Browsing:** type to filter (the first match is selected, Enter opens it);
in a local folder, two or more letters also search its subfolders. Alt+Up to go up, Ctrl+L to type a location, Alt+Left to go back.

<p align="center">
  <img src="docs/browse.gif" alt="Filtering the home folder, opening Documents, going up, typing ~/Music/Synthwave, going back" width="88%">
</p>

**File operations:** F2 renames in a bar at the bottom (the extension stays
put), Ctrl+Shift+N makes a folder, and pasting onto an existing name asks what
to do instead of overwriting.

<p align="center">
  <img src="docs/file-ops.gif" alt="Renaming resume.pdf to cv.pdf, creating a Receipts folder, pasting notes.txt onto an existing notes.txt and choosing Replace" width="88%">
</p>

**Archives:** Extract Here puts an archive with several items in its own
folder, and one with a single folder straight next to it.

<p align="center">
  <img src="docs/extract.gif" alt="Extracting tidepool.tar.gz into a tidepool folder and wallpaper-pack.zip into its Wallpapers folder" width="88%">
</p>

<sub>Recorded in a sandbox with demo files and made-up Tailscale peers.</sub>

## Features

**Browsing**
- Local folders, the trash, and network locations through GVfs: SMB, SFTP,
  FTP, WebDAV, MTP, `network:///`. Unmounted locations mount on demand.
- Path bar with breadcrumbs (Ctrl+L to type a path, `~/…` or `smb://…`),
  back / forward / up, type-anywhere filtering, sort by name, size or date.
  Recently visited local folders stay loaded (and up to date), so going back
  is instant.
- Search through subfolders as you type: hits from the LocalSearch index show
  at once, and a walk over the folder on all cores finds the rest. Hits say
  which subfolder they're in.
- Thumbnails for nearly every kind of file: images, video frames, audio cover
  art or a waveform, PDF pages, office documents, EPUB and comic covers, font
  samples, the first lines of text and code, and archive listings. Several
  render at once, and renders for rows scrolled past are stopped. Thumbnails
  other apps could use go in the shared `~/.cache/thumbnails`, so Abyss
  also shows the ones other file managers already made; its own looks (text
  pages, archive listings, waveforms) stay in `~/.cache/abyss`. Local files
  only. Zoom from 20 to 128 px with Ctrl+plus / Ctrl+minus or Ctrl+scroll.
- Copying, moving, deleting and trashing work on many files at once; local
  folders are measured and deleted using every core.
- List or grid view, each also **grouped by file type** (Folders, Images,
  Video, Audio, Documents, Code & Text, Archives, Other) with a heading per
  group. The switch sits next to the filter; in a narrow window the path
  keeps the top row and the controls move to a row of their own.
- SMB shares sit under their server: `truenas / batocera`, and Up from a
  share goes to the server's share list.

**Sidebar**
- Home, GTK bookmarks (shared with the file chooser), drives with eject,
  network mounts, Browse Network, Connect to Server.
- **Tailscale** peers (online first, SFTP or SMB) and **Syncthing** folders,
  shown only when those tools are installed. Synced folders that are already
  bookmarks get a sync badge instead of a second row.
- Drag rows to reorder them; Home, Trash, Filesystem and the network entries
  stay put. Drop folders on the sidebar to bookmark them.

**Files**
- Copy, cut, paste (interoperable with Nautilus' clipboard), duplicate,
  rename, new folder, trash, restore, delete, empty trash.
- Drag and drop with Nautilus' rules: move on the same filesystem, copy
  elsewhere, Ctrl/Shift for copy/move/link, hover a folder to open it.
- Name clashes ask: **Replace**, **Merge** folders, **Skip**, **Rename** or
  **Cancel**, with "apply to all". Every job can be cancelled.
- **Extract Here / Extract To…** for zip, 7z, rar, tar.*, iso and more
  (via bsdtar), locally and on network shares, password-protected archives
  included. Nothing is overwritten and cancelling leaves nothing behind.
- **Open With Other Application…** with recommended apps, search, and
  "always use for this file type".

**Network logins**
- An inline sign-in bar instead of a dialog. GVfs asks separately for a
  server's share list and each share; Abyss remembers what you typed per
  server for the session and answers the follow-up prompts itself, so you
  sign in once per server.

## Keys

| Key | Action |
| --- | --- |
| Enter / double-click | Open |
| Alt+Left / Alt+Right / Alt+Up, Backspace | Back / forward / up |
| Alt+Home | Home |
| Ctrl+L | Type a location (`~/x`, `/x`, `smb://host/share`) |
| Ctrl+F, or just type | Filter (2+ letters also search subfolders) |
| Ctrl+H | Show hidden files |
| Ctrl+1 / Ctrl+2 | List / grid view |
| Ctrl+plus / Ctrl+minus / Ctrl+0, Ctrl+scroll | Zoom in / out / reset |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste |
| F2 | Rename |
| Delete / Shift+Delete | Trash / delete forever |
| Ctrl+Shift+N | New folder |
| F5, Ctrl+R | Reload |
| Ctrl+N / Ctrl+W | New window / close |

## Install

Needs Rust, GTK 4.12+, GVfs (`gvfs-smb` for SMB) and, for archives,
`bsdtar` (libarchive). Thumbnails use whatever of these is installed:
`ffmpeg` (video, audio), `imagemagick` (fonts, text, archive listings,
unusual images), `poppler` or `ghostscript` (PDF), `glycin` and
`libgsf` thumbnailers (images, office documents). On Chimera Linux:

```sh
doas apk add rust cargo gtk4-devel gvfs gvfs-smb gvfs-mtp
doas apk add ffmpeg imagemagick poppler   # optional, for thumbnails
git clone https://github.com/MatthewRafei/Abyss && cd Abyss
./install.sh
```

`install.sh` builds a release binary into `~/.local/bin/abyss` and installs the
icon and a desktop entry so launchers like fuzzel list it. To make Abyss the default for
folders:

```sh
xdg-mime default io.github.MatthewRafei.Abyss.desktop inode/directory
```

The release build is tuned for the CPU it's built on (`target-cpu=native`),
so build it on the machine that runs it. Developer notes are in
[`DEVELOPING.md`](DEVELOPING.md).

## Where the logic came from

Abyss ports behaviour from [Nautilus](https://gitlab.gnome.org/GNOME/nautilus)
rather than reinventing it. The files below are in the Nautilus source tree:

| Abyss | Nautilus |
| --- | --- |
| `filename.rs` | `nautilus-filename-utilities.c`: "(2)" / "(Copy)" names, extensions |
| `places.rs` | `nautilus-sidebar.c`, `nautilus-bookmark-list.c` |
| `ops.rs` | `nautilus-file-operations.c`: copy/move, conflicts, trash |
| `dnd.rs` | `nautilus-dnd.c`, `nautilus-files-view-dnd.c`, `nautilus-list-base.c` |
| `archive.rs` | `nautilus-file-operations.c` extract job |
| `chooser.rs` | `nautilus-app-chooser.c`, `nautilus-app-chooser-widget.c` |
| `window.rs` | `nautilus-window-slot.c`, `nautilus-pathbar.c`, `nautilus-files-view.c` |

## Licence and credits

Abyss is GPL-3.0-or-later, like the Nautilus code it derives from. Thanks to
the [GNOME Files](https://gitlab.gnome.org/GNOME/nautilus) developers.

The octopus and tentacle artwork (the app icon, banner and footer) is by
[VectorWhiz](https://www.etsy.com/shop/VectorWhiz) and
[CreativeSweetArt](https://www.etsy.com/shop/CreativeSweetArt) on Etsy, used
here under a commercial licence. It is **not** covered by the GPL; please don't
reuse it without your own licence from the artists.

<p align="center">
  <img src="docs/footer.png" alt="" width="100%">
</p>
