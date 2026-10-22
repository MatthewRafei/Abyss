# Abyss — developer notes

The project page is the [README](README.md). This file covers
working on the code.

## Layout

| File | What |
| --- | --- |
| `src/main.rs` | App setup, CSS |
| `src/fm1.rs` | `org.freedesktop.FileManager1` D-Bus service: other apps' "Show in Folder" and Properties |
| `src/portal.rs` | xdg-desktop-portal `FileChooser` backend: other apps' Open and Save dialogs |
| `src/window/pick.rs` | Pick mode: the window as such a dialog (name, type filter, Open/Save bar) |
| `src/window.rs` | The window: sidebar, toolbar and path bar, menus, bottom bars |
| `src/window/tab.rs` | A tab: one folder's listing, history, filter, selection and views |
| `src/window/panes.rs` | Panes of tabs, the split view, the saved session |
| `src/places.rs` | Sidebar sources (incl. mounted shares, saved servers) and saved order |
| `src/window/servers.rs` | Add Server form: protocol, address and folder into a location |
| `src/window/undo.rs` | Undo: what each operation changed, and taking it back |
| `src/window/batch_rename.rs` | Batch rename bar: rules, preview, problems |
| `src/window/properties.rs` | Properties window: sizes, times, permissions, default app, free space |
| `src/window/prefs_ui.rs` | Main menu, sort menu, Preferences and Keyboard Shortcuts windows |
| `src/window/error_bar.rs` | The bar failures slide up in |
| `src/prefs.rs` | Preferences, saved in `~/.config/abyss/settings` |
| `src/starred.rs` | Starred files, saved in `~/.config/abyss/starred` |
| `src/ops.rs` | Copy / move / trash / delete engine, conflicts |
| `src/par.rs` | Parallel tree walk, cancellable blocking work, per-job limiter |
| `src/search.rs` | Name matching and recursive search (LocalSearch, then a walk) |
| `src/thumbs.rs` | Thumbnail renderers, shared and own caches, render queue |
| `src/info.rs` | Listing attributes (name-only types locally), shared per-type icons |
| `src/listing.rs` | A folder's files as a list model, following changes in batches |
| `src/section.rs` | Grouped grid layout: heading rows and fillers over the shared selection |
| `src/window/views.rs` | List / grid / grouped views, grid tiles, view switching |
| `src/dnd.rs` | Drag-and-drop rules |
| `src/archive.rs` | Extraction and compression via bsdtar |
| `src/chooser.rs` | Open With chooser |
| `src/auth.rs` | Sign-in bar and per-server login cache |
| `src/filename.rs` | Name helpers ("(2)", "(Copy)", extensions) |
| `data/style.css` | Styling; follows the rice's `--hud-*` CSS variables |

## Checks

The build is strict; all of these must pass:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # pedantic + nursery, no unwrap
cargo test
```

`cargo test cancel_mid_extract -- --ignored --nocapture` cancels a real
extraction mid-way; set `ABYSS_CANCEL_DEST` to a mounted share to test there.

## File chooser portal

`install.sh` sets Abyss up as the portal's `FileChooser` backend; pick it in
`~/.config/xdg-desktop-portal/<desktop>-portals.conf` with
`org.freedesktop.impl.portal.FileChooser=abyss;gtk;` and re-run
`install.sh` (it links that file into the portal folder). Things learned:

- The portal starts its backends and waits for each; GTK asks the portal for
  settings while it starts, so the service file runs Abyss with
  `GDK_DEBUG=no-portals` (as xdg-desktop-portal-gtk does), else both wait
  25 s and the portal comes up without `FileChooser`.
- Test off-screen: `Xvfb :77`, then `GDK_BACKEND=x11 DISPLAY=:77` and
  `XDG_DATA_HOME` (holding a service file for the debug build) set before
  `dbus-run-session`. Call the backend with `gdbus call`, but give paths as
  `[byte 0x…]` arrays: gdbus corrupts `b'…'` byte strings. A client of the
  real frontend must stay connected for its `Response`, so `gdbus call`
  can't stand in for one there.

## Build flags

`Cargo.toml` sets a speed-first release profile (fat LTO, one codegen unit,
`panic = "abort"`, stripped). `.cargo/config.toml` adds `target-cpu=native`
and full RELRO. With `panic = "abort"` a panic in a GTK callback kills the
window, so app code never unwraps.

## Porting from Nautilus

Abyss ports behaviour from Nautilus rather than reinventing it. The Nautilus
sources are no longer in this repository; read them upstream at
<https://gitlab.gnome.org/GNOME/nautilus> (the fork point was commit
`8f94be5b0`, Nautilus 51). The README's "Where the logic came from" table
maps each Abyss module to the Nautilus files it follows. Read the matching
`src/nautilus-*.c` before adding a feature.

## Icon and artwork

`data/io.github.MatthewRafei.Abyss.svg` is the app icon; `install.sh`
installs it. `data/make-icon.py` renders it from the original octopus
artwork (light fill, dark outline); set `ABYSS_ICON_SRC` to regenerate it at
install time. `docs/banner.png` and `docs/footer.png` are renders for the
project page. The artwork is by VectorWhiz and CreativeSweetArt on Etsy, used
under a commercial licence and not covered by the GPL.
