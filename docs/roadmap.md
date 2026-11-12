# Roadmap

This repository follows `../build-plan-feather.md`.

## Done in this slice

- Step 0 scaffold: Cargo workspace, static CRT, Common Controls v6 manifest,
  release profile, scripts, CI.
- Step 1: index format, mmap reader, double-slot manifest, synthetic 1M-capable
  generator, query parser, wildcard matcher, filter subset, 1M benchmark
  command, memory proof, corrupt-index tests.
- Step 2 partial: native Win32 shell, virtual ListView, search worker with
  cancellation and generation tags, four-part status bar, toolbar, sortable
  columns with header arrows, folder-only Path column, double-click open,
  `Esc` and `Ctrl+F` accelerators, screenshots, UI scenarios, state model,
  tests.
- Step 2 dialogs: shared modal framework, Filters window with kind/extension/
  size validation, Settings window with per-user INI persistence, Help window,
  filter summaries in the status bar and toolbar, configurable result cap,
  debounce delay, double-click target, empty-query behavior, window placement,
  and always-on-top. Nine screenshot scenarios and parser/model tests.
- Step 2 filters and scope: one-click `Category` presets (Everything, Files,
  Folders, Documents, Images, Video, Audio, Archives, Executables, Code,
  Custom), a filter bar with a Type list and an In-location list, a modern
  `IFileOpenDialog` folder picker, `in:` query scopes with negation, remembered
  scope, volume include/exclude lists, and excluded-folder subtrees. Two more
  screenshot scenarios.
- Step 2 shell integration: Windows Explorer `IContextMenu` on right-click with
  the expanded verb set, `IContextMenu2`/`IContextMenu3` message forwarding, and
  a native fallback menu.
- Real-index groundwork: a portable `std::fs` scan builder with reparse/skip/
  error accounting, a multi-volume `IndexSet` and `EntryRef`, and path helpers
  for normalization and containment.
- CLI: `search` with text/JSON and folder scope, `index scan` (single-volume or
  multi-root library), `index synthetic`, `index status`, and `config
  list/get/set/path`.

## Next

1. Step 2 polish: icons, selection preservation, loading/empty banners, DPI
   scaling, hotkey, tray, and start at login.
2. Step 3: external sort, NTFS file IDs in the index, VHDX test. (The raw
   `$MFT` reader landed: `feather-core/src/mft.rs`.)
3. Step 4: SYSTEM service, installer, status file, real UI handoff.
4. Step 5 and later: USN deltas, USB, packaging.

## Immediate known gaps

- Fast indexing reads `$MFT` and needs admin rights: one UAC prompt per
  session starts the admin helper. A SYSTEM service would remove the prompt.
- Whole NTFS drives follow the USN journal in Live mode (changes show within
  about a second, see `docs/index-format.md`). Folders, shares, and the timed
  modes still use whole rebuilds triggered by the folder watcher, so there a
  new file shows up within about a minute and size or date changes alone do
  not trigger an update.
- Settings do not include start at login or an index-location picker yet.
- The screenshot harness shows a visible window. On a locked or session-0
  desktop it will produce a blank or fail; it is a developer/desktop tool.
- The empty-query list is capped at the configured result limit (50,000 by
  default) instead of mapping all entries virtually.
- Date filters are parsed but not applied. The Filters window has no date
  pickers.
- No icons yet; the columns are text only.
- The UI opens at 1,100x700 by default and restores a saved placement when
  Remember window is enabled. There is no DPI scaling yet.
- Keyboard accelerators and dialog commands have no automated window-level
  test; the screenshot scenarios render the windows, but the handlers are
  exercised manually.
- The shell context menu is verified manually with a desktop screenshot; it has
  no automated test.
