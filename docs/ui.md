# UI module guide

The main window draws itself. There are no child controls: Direct2D and
DirectWrite paint each frame into an off-screen 32-bit bitmap, which is copied
to the window with its alpha intact, so the Windows Mica or Acrylic backdrop
shows through translucent surfaces. Application state stays in `UiModel`;
search work stays in `SearchController`; geometry, formatting, sorting, and
text editing are pure helpers. That split is what makes a new feature testable
without clicking through the app. The Filters, Settings, and Help dialogs are
drawn the same way by `ui/form.rs`, so they share the palette, backdrop, and
animations.

## Files

| File | Responsibility |
| --- | --- |
| `ui/mod.rs` | Window class, custom frame (`WM_NCCALCSIZE`, `WM_NCHITTEST`), message loop, mouse and keyboard routing, accelerators, settings/filter commands, rendering, screenshot/test entry points. |
| `ui/layout.rs` | Pure geometry: sidebar, top bar, search row, columns, list, details pane, sidebar entries. Unit-tested. |
| `ui/paint.rs` | Draws one `Frame` snapshot: sidebar, top bar, search field, results, details. Never touches the index. |
| `ui/gfx.rs` | Direct2D, DirectWrite, and WIC factories, Segoe UI Variable text formats, and `Painter` drawing helpers. |
| `ui/canvas.rs` | Presents a Direct2D frame to a window through an off-screen bitmap, and renders frames into PNGs. |
| `ui/anim.rs` | Tweens, hover fades, and the main window's `Motion` state. |
| `ui/form.rs` | Custom-drawn dialogs: the `Form` builder (switches, segmented choices, text fields, check lists, folder lists, help tables, footer buttons), its layout, painting, input, and modal window. |
| `ui/theme.rs` | `Palette` colors, light/dark resolution, and the DWM backdrop and dark-title-bar attributes. |
| `ui/icons.rs` | Windows file-type icons from the system image list, cached by extension as bitmaps. |
| `ui/textfield.rs` | Editing state for the search field and dialog fields: UTF-16 caret, selection, word motion. Unit-tested. |
| `ui/model.rs` | Testable GUI state: rows, generation, counts, warnings, searching/done/started flags. No Win32. |
| `ui/picker.rs` | Modern `IFileOpenDialog` folder picker with `FOS_PICKFOLDERS`. |
| `ui/context_menu.rs` | Explorer `IContextMenu`/`IContextMenu2`/`IContextMenu3` host, the native fallback menu, and clipboard, open, and reveal helpers. |
| `ui/sort.rs` | Pure column comparators, unit-tested without a window. |
| `ui/text.rs` | Pure path, size, and type formatting, unit-tested without a window. |
| `ui/screenshot.rs` | The PNG writer. |
| `ui/win.rs` | Small UTF-16 and window-size helpers. |
| `ui/filters.rs` | `FilterSet` state, category presets, query composition, summaries, validation, and the Filters form. |
| `ui/settings.rs` | `Settings` state, INI v2 parse/save, and the Settings form, including Appearance. |
| `ui/help.rs` | Help content and the Help form. |
| `ui/setup.rs` | The first-run setup form: places to search, theme, and accent. |
| `ui/hotkey.rs` | The global shortcut: parsing, display, registration. |
| `ui/tray.rs` | The notification-area icon, its menu, and the app icon loader. |
| `ui/indexer.rs` | In-app indexing: background scan into a staged library, the swap into place, drive and profile discovery. |
| `ui/frames.rs` | Animation frames paced by the display (`DwmFlush`) instead of a timer. |

Text uses Segoe UI Variable from the system font collection, with its weight
and optical-size axes, so no font is embedded in the exe. Windows 10 does not
ship it; there the window falls back to Segoe UI.

## Motion

`ui/anim.rs` holds the tweens: hover highlights fade, the selected row and the
selected sidebar entries slide to their new place, the list scrolls smoothly,
the details pane fades and rises when the selection changes, and switching the
theme cross-fades the colors. Values come from the clock at paint time, and a
16 ms timer (`TIMER_ANIM`) repaints only while something is moving. When the
Windows "Animation effects" setting is off, every change is instant.
Screenshots call `Motion::settle`, so they show where each animation ends.

To animate something new, give `Motion` a `Tween`, start it with
`animate_to` where the state changes, include it in `Motion::active`, and read
`value(now)` when building the frame.

## Adding something to the main window

1. Give it a rectangle in `layout::compute`, or a `Button` entry, with a unit
   test for the geometry.
2. Put what it shows into the frame in `Ui::build_frame`, computed from
   `UiModel` and settings.
3. Draw it in `paint.rs` from the frame only.
4. Handle input in `Ui::hit_at`, `Ui::press`, or `Ui::on_key`.
5. Put pure logic in `text.rs`, `sort.rs`, `layout.rs`, or `textfield.rs` with
   unit tests, not in the message handler.
6. Add a screenshot scenario in `run_scenarios` once it is visible.

Keep the Win32 message handlers short. Long work belongs in a controller with
a channel, as the search worker does.

## Themes and backdrop

`dark_mode` is `light`, `full` (dark), or `follow` (the Windows app theme,
re-read on `WM_SETTINGCHANGE`). `material` is `mica`, `acrylic`, or `solid`;
`accent` is `saffron`, `cobalt`, `teal`, or `iris` (the older names `green`,
`amber`, `blue`, and `plum` still load). All three live in
`settings.ini` and work with `feather config set`. The sun/moon button in the
top bar switches light and dark and saves the choice.

The window removes its caption in `WM_NCCALCSIZE`, extends the DWM frame over
the whole client area, and sets `DWMWA_SYSTEMBACKDROP_TYPE`. Windows draws the
minimize, maximize, and close buttons over the backdrop, and `DwmDefWindowProc`
hit-tests them, which keeps Snap Layouts. Builds older than 22621 get the solid
look. The off-screen bitmap is released after each paint to keep it out of the
idle working set.

## Adding a dialog

Dialogs are forms. A form is a list of rows grouped into cards by headings,
plus footer buttons; `ui/form.rs` lays it out, paints it, and handles the
mouse, keyboard, scrolling, focus ring, and animations.

1. Build a `Form`: `heading`, then rows such as `switch`, `segments`,
   `field`, `checks`, `paths`, `paragraph`, or `syntax`, each with an `Id`.
   Add footer buttons with `ButtonStyle::Primary` (Enter presses it),
   `Secondary`, or `Quiet` (left-aligned, for Reset).
2. Implement `FormDialog::on_button`. Read values with `form.is_on`,
   `form.selected`, `form.text`, `form.checked`, or `form.path_items`. On a
   validation error call `form.set_error` and `form.focus(field)` and return
   `Action::Stay`; otherwise store the result and return `Action::Accept`.
   `take_outcome` hands the result back.
3. Open it with `FormWindow::open(parent, title, width, height, look, form,
   dialog)`, then `run_modal()`, or `capture(path)` for screenshots. `look`
   comes from `Ui::look()` so the dialog matches the main window.
4. Route the command from `Ui::press` and apply the outcome to `Ui` state.
5. Add a scenario in `run_scenarios` so the dialog is rendered by `--ui-test`.

`FormDialog::look` lets a dialog restyle itself live; Settings uses it to
preview the theme, background, and accent before saving. Tab and Shift+Tab
move between controls, arrow keys change a segmented choice, Space toggles,
Enter presses the primary button, and Esc closes the window.

## Indexing and first-run setup

The app indexes the folders and drives in `index_roots`. The first time it
runs (until `setup_done` is saved), `ui/setup.rs` asks where to search,
offering the profile folder (checked), every fixed drive, and any other
folder, plus the theme and accent. Settings has the same list under Search
locations; saving a change re-indexes.

There is no Update button: the index keeps itself current. At startup the app
rebuilds it once (to catch up on changes made while it was closed), and
`ui/watch.rs` watches every root with `ReadDirectoryChangesW` for files and
folders being created, deleted, or renamed. Changes under Feather's own
folders, `%TEMP%`, cache folders, and excluded names do not count.
`Ui::maybe_refresh` starts a new build once changes have been quiet for 4
seconds (or after a minute of constant change), and never sooner than 20 times
the last build's length (at least 45 seconds) after the previous one.

Local NTFS drives, and folders on them, are read from the drive's master
file table (`feather-core/src/mft.rs`): one sequential read of `$MFT`, on all
cores, skipping unused records by `$MFT`'s bitmap. That needs admin rights.
When Feather has them it reads the table itself; otherwise `ui/helper.rs`
starts one elevated helper per session (`feather index serve --pipe NAME`, one
UAC prompt) and keeps it. The app owns a named pipe with a random name; the
helper connects, says hello, and answers one request per line with the tree
as a byte stream (`feather_core::scan::Tree::encode`). The helper never writes
files, and it exits when the pipe closes. A declined prompt is remembered for
the session, and any other file system uses the folder walk, which reads
folders on several threads and takes each file's details from the directory
listing. On a 2.5-million-file drive the table read takes
about 3 seconds and the walk of a 2-million-file folder about 9.

`Indexer` scans each root on a background thread into a new folder beside the
base path (`%LOCALAPPDATA%\Feather\index.g<time>`), one `vol-*` folder per
root, and marks it complete. The old index keeps answering searches. When the
build finishes, `Ui::install_index` opens the new folder next to the old one,
records it in `index.current`, and runs the current search on it; the list
keeps showing the old results until the new ones are in (`Ui::finish_swap`),
then switches with the same item selected and the same scroll position.
Folders that are no longer current are deleted once nothing has them open
(`indexer::cleanup`, also at startup). Nothing is renamed, so a mapped index
never blocks an update. The first build shows the root being read; later ones
only add "updating" to the item count. Nested and duplicate roots are
indexed once.

Settings lists every fixed drive (and any drive already searched) as a check
box, labeled the way Explorer names it, and other folders below. Whole drives
are stored as `C:\`; older files wrote `C:`, which is not a full path, and
are read correctly now.

## Searching by path

A word with a backslash (or slash) is about the path, like in Everything.
A full folder path on its own browses it: `C:\Users\` lists what is directly
inside `C:\Users`, and `C:\Users\ad` also lists the items in `C:\Users`
whose names contain `ad`. The worker resolves the folder once per volume
(`VolumeIndex::find_folder`, a binary search over the sorted names per path
part) and then compares parent references, so browsing is as fast as a name
search. With other words, the folder becomes a scope: `C:\Users\ report`
finds report anywhere under it. A partial path such as `users\me` matches
anywhere in the full path.

## Sidebar

The sidebar header has the app icon and the wordmark; the fold button sits at
the bottom with Settings and Help, drawn as a window outline whose side pane
is wide when the sidebar is open and a sliver when folded. Folding animates the sidebar to 64 px; tabs
keep their icons, labels fade out early (and back in late), and hovering a tab
shows its name beside it. Kinds use the drawn icons in `ui/glyphs.rs`, one
shape and color per kind, because Windows' own icons are a blank page for
many types. Kinds can be dragged to a new order
(they lift and the others slide aside), hidden or shown from the right-click
menu or Settings, and reset from the menu. The order, hidden tabs, and fold
state are saved (`sidebar_tabs`, `sidebar_collapsed`). Clicking the tab that
is already selected does nothing.

## Shortcut and tray

`ui/hotkey.rs` parses and formats shortcuts as any set of keys:
`Ctrl + Space`, `Ctrl + Shift + Right Shift`, `Caps Lock + F`, `Right Ctrl`, or
`None`. `Ctrl`, `Alt`, `Shift`, and `Win` mean either side; `Left ...` and
`Right ...` mean one side. A shortcut that is only a typing key (alone or with
Shift) is refused. Plain combinations go to `RegisterHotKey`, which also works
over admin windows; anything else is watched by `ui/keyhook.rs`, a low-level
keyboard hook on its own thread. Shortcuts with an ordinary key fire on that
key and keep it from the app in front; modifier-only ones fire when released,
if nothing else was pressed, so `Ctrl + Shift` does not break `Ctrl + Shift + T`.
After a shortcut using Win or Alt, an unassigned key press keeps Start or the
menu bar from opening.

In Settings the shortcut box listens while focused: the hook sends every key
to it (and keeps them from other apps), key caps pop in as keys go down, and
the combination is taken when all keys are let go. Esc cancels, Backspace
turns the shortcut off, and Tab moves on. The main window releases its
shortcut while Settings is open. If another app owns the shortcut, the
sidebar says so.

`WM_HOTKEY` toggles: it hides Feather when Feather is in front, otherwise
shows it with the search text selected (or brings an open dialog forward).
With `close_to_tray` on, `ui/tray.rs` keeps an icon in the notification area;
closing the window hides it there, a click on the icon shows it, and the icon
menu has Open, Settings, and Quit. The icon is re-added when Explorer
restarts (`TaskbarCreated`). One copy runs at a time: a second launch posts
`WM_SHOW_FEATHER` to the first and exits (`ui::activate_running`).

The app icon is `crates/feather-app/feather.ico`, embedded as resource 1.

The theme button sits beside the caption buttons with their height and width.
With the Solid background, the pixels under the caption buttons are left
transparent and `DWMWA_CAPTION_COLOR` is set to the background color, so
Windows' own buttons stay visible.

The app no longer generates demo data. With no index it starts empty and
says so; `--ui-test` and `--screenshot` still generate the demo index they
need.

## Settings window

Settings has pages (`Form::page`) listed on the left like the main window's
sidebar: Appearance (with its Custom theme sub-page, `Form::subpage`),
Indexes, Shortcut and tray, Sidebar, Window, and Sharing. Indexes holds the
drives and folders to search at the top, then the Results, Index, and What to
show settings that used to be the Search page.
`Ctrl+Tab` and `Ctrl+PgUp/PgDn` switch pages; the arrow keys do too when the
page list has focus. Dialogs have no Windows title bar: the title strip is
drawn by the app and drags the window, and Windows draws the close button
over it (left see-through with the Solid background).

Every change shows in the main window at once: the dialog posts the settings
as they stand (`WM_PREVIEW_SETTINGS`), and Cancel applies the old ones again.
What to search, the shortcut, and starting at sign-in wait for Save.

Places shows each drive as a card with its Explorer icon, name, and free
space; folder lists show the folder icon. The Sidebar page is a list to drag
into order (`Form::order`, or `Ctrl+Up/Down`) with a check box per tab.
Number boxes (`Form::number`) group digits by thousands with the separator
from the Windows region settings as you type. "Start Feather when you sign in"
reads and writes the per-user `Run` key (`ui/startup.rs`), which starts
`feather.exe --background` in the tray.

"Results to show" is a slider (`Form::slider`) from 1% to 100% of the indexed
items, saved as `results_percent`. `Settings::result_cap` turns it into the
cap for a search; 100% means no cap, so every match is listed. The caption
beside the slider shows the count the percentage comes to. "Reindex now" is an
in-page button (`Form::action`, handled by `FormDialog::on_action`): it posts
`WM_REINDEX` to the main window, which rebuilds the index in the background.
The dialog gets the preview and reindex callbacks and the item count through
`settings::Live`.

Searched drive cards and the "More folders to search" rows carry a round-arrow
button (`Part::Refresh`, reported through `FormDialog::on_item_action`). It
posts `WM_REINDEX` with the place as a boxed `String` in lparam; the main
window calls `Indexer::start` with `only` set, so just that root is read again
and every other root's files are hard-linked (or copied) from the library in
use. A place that is not saved yet asks to be saved first.

"Update the index" (`auto_update`: right away, every 15 minutes, every hour,
never) sets the least time between automatic rebuilds in `Ui::maybe_refresh`;
"never" leaves rebuilding to the buttons. Folded, the sidebar's Settings, Help,
and fold buttons slide down into the space the status text leaves.

Appearance has "Show file sizes in" (`size_format`: Auto, KB, MB, GB), used by
`text::size_in` for the Size column and the details pane. The toolbar's panel
button (`Button::Details`) slides the details pane out and back
(`Chrome::details_w`, saved as `details_pane`).

The Custom theme sub-page edits five colors with `Form::color` rows, which
open the Windows color picker (`ChooseColorW`). `Palette::custom` builds the
whole palette from them; light or dark follows the background's luminance.
"Choose..." copies a picture into `%APPDATA%\Feather\backgrounds`
(`ui/wallpaper.rs` checks Windows can decode it first) and the window draws
it under everything with the chosen fit and strength; pictures no longer used
are deleted when Settings closes. The theme button in the caption switches
back to the built-in colors.

## Sharing

`src/share/` runs a web server and an FTP server for the folders listed on the
Sharing page. Both are off by default and need a user name, a password of at
least ten characters (kept only as an Argon2id hash in `share_password`), and
at least one folder. `Ui::update_sharing` starts or restarts them on Save and
at launch; dropping `share::Sharing` stops them.

- **Access.** "This PC" listens on 127.0.0.1. "Local network" listens on all
  interfaces but drops any address that isn't private, link-local, or
  100.64/10 (Tailscale). "Internet" either runs `cloudflared` (a quick
  `trycloudflare.com` address, or a named tunnel with a token) or `ngrok`, both
  pointed at the web server on 127.0.0.1, or listens directly for a forwarded
  port. Tunnel tokens are DPAPI-encrypted in `tunnel_token` and passed to the
  tool through its environment. The tool runs in a job object so it never
  outlives Feather. FTP can't go through the tunnels and stays on the network.
- **Web.** HTTPS only (rustls with ring, TLS 1.2 and 1.3) with a self-signed
  certificate made once in `%APPDATA%\Feather\sharing` (key DPAPI-encrypted).
  Requests are parsed strictly (no chunked bodies, no folded or duplicate
  lengths, 16 KB heads within 15 s). Sessions are 256-bit random cookies,
  `HttpOnly; Secure; SameSite=Strict`, idle out after two hours; every change
  needs the session's CSRF token in `X-Feather-Csrf` and a matching `Origin`.
  Responses carry a strict CSP, `nosniff`, `X-Frame-Options: DENY`, HSTS, and
  `no-store`; downloads are always attachments. The page (`share/web/`) puts
  every name in as text, never HTML.
- **FTP.** Explicit FTPS (`AUTH TLS`) before `USER`, `PROT P` for data, and
  passive mode only. A data connection must come from the control
  connection's address. Plain FTP only with "Allow unencrypted FTP".
- **Files.** `share::files::Roots` turns a root number and a relative path
  into a file, checking each name (`..`, `:`, device names like `CON`,
  wildcards, trailing dots) and then the resolved path, so links and junctions
  can't leave the shared folder. Hidden and system items follow the "What to
  show" settings; folders to leave out and Feather's own folders are never
  served. Deleting moves to the Recycle Bin.
- **Passwords.** At least 4 characters, or with "Require a strong password"
  12 or more mixing three kinds of characters, not common, and not the user
  name. The hash is Argon2id (64 MiB, 3 passes) over HMAC-SHA256 of the
  password with a 32-byte pepper in `sharing\pepper.key`, encrypted with
  DPAPI; hashes start `feather2$`, and older hashes still verify. Only one
  hash is checked at a time. The password itself is also kept,
  DPAPI-encrypted, in `share_password_saved`, so Settings can show it again
  behind its eye button.
- **End to end** (`share_e2e`, opt-in). The password never crosses the wire:
  the page gets a salt and nonce from `/e2e/hello`, derives a key with
  PBKDF2-SHA256 (600,000 rounds), and proves it with HMAC; the server proves
  it back. Both derive a session key with HKDF, and every call after that
  goes to `/api/sealed`, `/api/sealed-download`, or `/api/sealed-upload`,
  sealed with AES-256-GCM. File data travels in 256 KB frames bound to their
  place, so frames can't be dropped, reordered, or cut short, and request
  nonces are remembered per session, so nothing can be replayed. A tunnel in
  the middle sees only opaque bytes.
- **Requests.** Static files carry an ETag and answer 304 when unchanged;
  text replies over 1 KB are gzipped when the browser accepts it (never in
  end-to-end mode). The session cookie is `__Host-feather`. Changes must come
  from the page: `Sec-Fetch-Site: same-origin`, or an `Origin` naming this
  site, or the tunnel's `X-Forwarded-Host`.
- **Tunnels.** When `cloudflared` or `ngrok` is missing, the tunnel thread
  fetches it once into `%LOCALAPPDATA%\Feather\tools`: cloudflared from
  Cloudflare's GitHub releases, used only with a valid Authenticode signature
  from Cloudflare; ngrok through winget (hash-checked) or ngrok's download
  with its signature checked. "Set up tunnel" in Settings starts that early,
  and for ngrok opens the page with the authtoken.
- **Lockout.** `share::auth::Guard` locks an address out after five wrong
  passwords, doubling from 30 seconds to an hour, and everyone for a while
  after sixty misses in ten minutes. Sign-ins and changes are logged to
  `sharing.log` (rotated at 5 MB), never with passwords.

`share::tests::the_servers_serve_only_what_is_shared_to_who_signs_in` runs
both servers on 127.0.0.1 and checks sign-in, browsing, ranges, uploads,
CSRF, path tricks, and an FTPS download end to end;
`end_to_end_encryption_never_sends_the_password` does the sealed sign-in,
listing, download, upload, and replay refusal with a Rust client doing what
`login.js` and `app.js` do. `serve_page_for_a_browser` (ignored) serves the
page over plain HTTP on 127.0.0.1, where browsers allow Web Crypto, to try it
by hand.

## Memory

Results are kept in `rows::Rows`: a search that matches everything on a
volume arrives from the worker as one `SearchEvent::Span`, so "everything" on
millions of files costs a few bytes; other lists pack each row into 4 bytes
(6 bits of volume, 26 of id) and only fall back to 8 if an id doesn't fit.
Finished lists give back spare capacity. Hidden or minimized, the window drops
its render target and bitmaps and trims its working set (`ui::memory`); 20
seconds in the background trims it too. `feather --memory --index DIR` prints
the memory after each step of a typical session.

## Searching and refreshing

A search with an `in:` scope (the sidebar's drives and folders) skips volumes
the scope rules out and, when the scope covers a whole volume, never builds
paths on it (`Query::volume_paths`). The watcher remembers which root changed
(`Changes::take_dirty`), and an automatic refresh reads only those roots again,
carrying the others over from the library in use.

## Linux

Without the Win32 window, `local/` serves the window as a page on 127.0.0.1
(`feather window`, the default on Linux). Each run makes a 256-bit key; the
browser gets it once in the address it opens (`/?key=`) and keeps it in an
HttpOnly SameSite cookie. Requests must name 127.0.0.1 or localhost in `Host`
(no DNS rebinding) and changes must come from the page. `local/desktop.rs`
opens the window in a Chromium-family app window when one is installed, opens
files with `xdg-open`, reveals them over the FileManager1 D-Bus interface,
and writes the `.desktop` menu and autostart entries. The index is built with
`library::build_walk`, the portable part of the indexer, on a timer.

## Settings and filters

`Ui::start_search` is the one place that turns UI state into a query:

1. `FilterSet::compose` appends `type:`, `ext:`, `size:`, and `match:` tokens to
   the search text.
2. `Ui` appends `in:"<scope>"` when the filter bar has a folder selected.
3. `Query::parse` validates the combined text and produces warnings.
4. `SearchController::submit` receives the query, result cap, and
   `SearchOptions`.
5. If the empty search box is configured to stay empty, the controller is
   invalidated instead and `UiModel::clear` shows the `Type to search` state.

The `match:` token carries the Filters window's position choice: `anywhere`,
`starts`, or `ends`. It applies to every plain word, not to `ext:`, `size:`, or
wildcards. `Ui` keeps the parsed `Query` beside the visible model, and
`build_frame` asks it for `match_spans` on each visible name. `paint.rs` passes
those ranges to `Painter::text_marked`, which sets a dim Direct2D brush as the
drawing effect on the parts of the name no word matched. Rows therefore show
the matching text normally and grey the rest; wildcard terms and non-ASCII
names stay plain rather than risk misaligned ranges. The span search is
ASCII-only and allocation-free.

`FilterSet` is session state. `Settings::remember_filters` decides whether it is
written to `%APPDATA%\Feather\settings.ini`; the same file stores the results
percentage, debounce delay, double-click target, empty-query behavior, remembered
window placement, always-on-top, hidden/system visibility, shell-menu choice,
excluded volumes, excluded folders, and the remembered scope. Writes go to a
process-specific temporary file and are renamed into place, so a crash cannot
leave a half-written file. Parsing clamps out-of-range values and reports the
bad key on stderr.

`SearchOptions` is built from the settings on every search: hidden and system
record flags, excluded folder prefixes, and a volume bitmask derived from
`IndexSet::volume_mask(&excluded_volumes)`. The worker never opens a volume that
is masked out.

## Sidebar and scope

The sidebar lists every `Category` preset, then `Look in`: `All locations`,
each volume prefix in the open `IndexSet`, the current custom scope, and
`Choose a folder...`, which opens `ui/picker.rs`. Choosing a preset rewrites
the kind/extension fields of `FilterSet`; choosing a location sets the scope.
No preset is highlighted when the Filters dialog produces a custom set.
Clicking the entry that is already selected does nothing. `Apps` matches
shortcuts (`.lnk`), programs, and ClickOnce apps.

The scope is UI state, not a filter field, so the Filters dialog stays simple.
`Ui::set_scope` rebuilds the sidebar and starts a search; `Esc` clears the
search text but keeps the scope. The top bar shows `Looking in <path>`.

## Context menu

Right-clicking a result selects the row and calls `ui/context_menu.rs`:

1. Resolve the full path and create the shell `IShellItem`/`IContextMenu` for it
   from the parent folder's `IShellFolder`.
2. Query the menu with `CMF_NORMAL | CMF_EXPLORE | CMF_EXTENDEDVERBS`, which
   produces the expanded classic menu rather than the compact Windows 11 menu.
3. Forward `WM_INITMENUPOPUP`, `WM_DRAWITEM`, `WM_MEASUREITEM`, and
   `WM_MENUCHAR` to `IContextMenu2`/`IContextMenu3` while the menu is open.
4. Invoke the chosen command through `IContextMenu::InvokeCommand`, converting
   the menu offset to a verb.

If shell initialization fails, the fallback menu offers Open, Open containing
folder, and Copy full path. `Settings::use_shell_context_menu` can force the
fallback. The shell menu is only created on right-click, never per row, so it
does not affect idle memory.

The status bar has four parts: state, filter summary, result count, and indexed
item count. The toolbar Filters label shows the number of active filter
categories.

## Search flow

1. `Ui::start_search` composes the filter set and scope with the search text,
   parses the combined query, and submits it with the configured result cap and
   `SearchOptions`. The controller increments the generation.
2. `SearchController` wakes its worker, which scans every unmasked
   `VolumeIndex` in the `IndexSet` by `iter_ids`.
3. Matches arrive in batches of up to 1,000 IDs.
4. `Ui::on_timer` applies each event through `UiModel::apply`.
5. A stale generation is dropped by the model and never reaches the ListView.
6. The ListView is virtual: `LVN_GETDISPINFO` fills only visible cells.

The worker checks the generation and stop flag every 1,024 entries, so typing
does not build a queue of old searches.

## Sorting

Clicking a header sends `LVN_COLUMNCLICK`. The first click sorts ascending;
clicking the same header again toggles descending.

- Name, size, modified, and type compare index records directly through
  `ui/sort.rs`.
- Path resolves each loaded row to its containing folder and sorts a transient
  key vector. The table shows the folder in the Path column and the file name
  in the Name column, like Everything.
- While a search is still streaming, selected sort order is re-applied when the
  search finishes.
- `ResultsPanel::set_sort_indicator` sets `HDF_SORTUP` or `HDF_SORTDOWN` on the
  header and repaints it. The Common Controls v6 manifest is required for the
  arrow to draw.

## Keyboard and mouse

| Input | Action |
| --- | --- |
| typing | edits the search field, which always has the keyboard |
| `Esc` | clear the search; with empty listings off, return to the empty state; with the search already empty after the shortcut opened Feather, hide it again |
| the global shortcut (`Ctrl+Space` by default) | from any app, show Feather with the search text selected; pressed while Feather is in front, hide it |
| `Ctrl+F` | select the search text |
| `Left`/`Right`/`Home`/`End`, with `Shift` or `Ctrl` | move the caret, extend the selection, jump by word |
| `Ctrl+A`/`C`/`X`/`V`, `Ctrl+Backspace` | edit the search text; `Ctrl+C` with no text selected copies the selected result's path |
| `Up`/`Down`/`PageUp`/`PageDown`, `Ctrl+Home`/`Ctrl+End` | move the result selection |
| `Enter` or double-click a row | open the item, or show a file in its folder when Settings says so |
| `Ctrl+Enter` | show the selected item in its folder |
| right-click a row, `Shift+F10`, or the menu key | the Windows context menu |
| click a column header | sort by that column; click again to reverse |
| `Tab` / `Shift+Tab` in a dialog | move through the dialog's controls |
| arrow keys, `Space` in a dialog | change a choice, flip a switch or check box |
| `Enter` in a dialog | run the primary button, including validation |
| `Esc` in a dialog | cancel without applying |

The accelerators live in a single `CreateAcceleratorTableW` table. Both message
loops call `TranslateAcceleratorW` before `TranslateMessage`. Dialogs use their
own modal loop without accelerators, so main-window shortcuts are inactive
while a dialog is open.

## Screenshot harness

`feather.exe --screenshot FILE --query TEXT` creates the normal window, waits
for the search to finish, renders the frame into a WIC bitmap over an opaque
base color (a screen capture cannot see the backdrop), then destroys the
window.

`feather.exe --ui-test --screenshots DIR` runs these scenarios:

| Scenario | Query | Expectation |
| --- | --- | --- |
| `01-all` | empty | rows exist |
| `02-match` | `file0000005` | exactly one row |
| `03-no-results` | `zzzzzz-no-such-file` | zero rows |
| `04-sorted` | `ext:pdf`, Size descending | rows exist, leading sizes are descending |
| `05-path` | `type:folder`, Path ascending | rows exist, leading folders are ascending |
| `06-filters` | dialog | Filters window renders |
| `07-filtered` | `file`, filter Files + `pdf` | every leading row ends in `.pdf`; the typed `file` is drawn brighter than the rest of each name |
| `08-settings` | dialog | Settings window renders |
| `09-help` | dialog | Help window renders |
| `10-quick-filter` | `file`, Images preset | rows exist, every leading row is an image |
| `11-scope` | `in:C:\FeatherBench\dir0001` | rows exist, every leading path is inside the scope |
| `12-light`, `13-dark` | `file0003` | both themes render, whatever the Windows theme is |
| `14-hover-filters`, `15-hover-settings` | hover | hover highlights stay inside their column |
| `16`–`18` | dialogs | Settings, Filters, and Help in the other theme |
| `19-setup` | dialog | first-run setup renders |
| `20-indexed` | `feather-setup-check` | a real folder is indexed in the app, swapped in, and found |
| `21-settings-places` | dialog, scrolled | drive check boxes, other folders, and the shortcut box render |
| `22-browse` | `C:\FeatherBench\dir0001\` | every row is directly inside that folder |
| `23-sidebar-folded` | folded, hover | the sidebar folds to icons and shows a tab's name |
| `24-sidebar-drag` | drag | a lifted tab drops into the expected slot |
| `25-shortcut-recording` | dialog | the shortcut box shows held keys as key caps |
| `26-sidebar-folding` | mid-fold | the sidebar halfway through folding |
| `27-settings-sidebar`, `28-settings-search` | dialog pages | the tab list with icons, and grouped numbers |

Each writes a PNG and `report.txt`. The ignored integration test runs the same
binary and checks that the PNGs exist and are not empty.

## Test layers

| Layer | Location | Needs a desktop |
| --- | --- | --- |
| Index round-trip, corruption, query parser, path helpers | `crates/feather-core/tests`, `feather-core/src/*.rs` | no |
| Directory scan builder and index set | `feather-core/src/scan.rs`, `feather-core/src/index_set.rs` | no |
| Search worker batching, cancellation, cap, mask | `crates/feather-app/tests/search_controller.rs` | no |
| UI state model, including the empty state | `ui/model.rs` unit tests | no |
| Filter composition and validation | `ui/filters.rs` unit tests | no |
| Settings INI parse, save, clamp, and round-trip | `ui/settings.rs` unit tests | no |
| Help content | `ui/help.rs` unit tests | no |
| Form values, layout, and keyboard stops | `ui/form.rs` unit tests | no |
| Tweens, hover fades, and selection motion | `ui/anim.rs` unit tests | no |
| Sort comparators | `ui/sort.rs` unit tests | no |
| Text formatting | `ui/text.rs` unit tests | no |
| CLI parsing and percentiles | `cli.rs` unit tests | no |
| Real window screenshots | `crates/feather-app/tests/ui_smoke.rs` | yes, ignored by default |

## Rules of thumb

- Text formats live in `Gfx`; each window owns one `Gfx`.
- Only visible cells are formatted; never build a string for every result.
- Reuse `Ui::scratch` for the UTF-16 conversion.
- Path resolution is cached by `IndexSet`.
- Shell menus and icons are created only on demand. A per-row shell icon would
  destroy the memory budget.
- Draw with the `Palette`, never with literal colors, so both themes and the
  theme cross-fade keep working.
- Settings writes go through a temporary file and a rename. Never write the
  final INI in place.
- Changing filters or matching settings starts a new search generation; do not
  mutate the loaded rows in place.
