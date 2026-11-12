# Changelog

## 1.1.0

### New

- **Live updates.** Feather follows the NTFS change journal, so new,
  renamed and deleted files show up within seconds without reindexing.
  Results refresh calmly, at most every few seconds, and never while you
  rename something.
- **Update check.** Once a day Feather asks GitHub whether a newer version
  is out and shows a Windows notification; clicking it downloads the
  installer. Settings → Window → Updates has a switch to turn this off and a
  Check now button. Nothing is downloaded or installed on its own.
- **Date filters.** `dm:today`, `dm:lastweek`, `dm:2026-11`, ranges, and a
  Date modified section in Filters.
- **Folder totals.** Folders show their size and item count in the Size
  column, the details pane and `size:` filters.
- **Shared page:** download whole folders as .zip; rename, move, copy and
  delete from a long-press menu that fits any screen; upload any files,
  whole folders or camera photos; sort; install it as an app with Feather's
  icon.
- **Photo and video viewer:** pinch and double-tap zoom, rotate with a
  threshold so zooming doesn't turn the picture, double-tap the sides of a
  video to skip 10 seconds, instant seeking while dragging the bar.
- **Linux release:** a static tarball next to the Windows installer.

### Changed

- **Settings → Sharing** has a short status line with a colored dot and
  one row per address, with info, copy and QR icon buttons. The QR popup
  says what it's waiting for instead of showing a blank square.
- Right-click menus follow the dark theme.
- Del warns before deleting for good.
- The global shortcut stays out of the way while a game is full screen;
  maximized windows are no longer mistaken for games.

### Fixed

- Choosing a command in the full Windows context menu (such as Copy as
  path) ran it instead of opening a second menu.
- Results no longer refilter by themselves while the drive is busy.
- Long names on the shared page wrap instead of overlapping.
- Sharing: renaming or deleting a folder can't expose a folder that was
  left out; shortcuts can't preview or receive uploads; hidden files stay
  hidden; upload conflicts and errors stay on screen.
- Live updates keep junctions and symbolic links without walking into
  them, and leave no ghost results when a folder goes away.

## 1.0.0

The first release: a fast, low-memory file search for Windows with an
NTFS `$MFT` reader, Direct2D UI, sharing over HTTPS and FTPS, a per-user
.msi installer, and a Linux build.
