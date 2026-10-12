# Abyss — developer notes

The project page is the [README](README.md). This file covers
working on the code.

## Layout

| File | What |
| --- | --- |
| `src/main.rs` | App setup, CSS |
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
