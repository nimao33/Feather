# Roadmap

## Done (1.1.0)

- Memory-mapped index with a versioned, validated on-disk format; NTFS
  `$MFT` reader through a small elevated helper; folder walk for everything
  else; per-drive libraries with generation swaps.
- Search: words, phrases, negation, wildcards, `ext:`, `type:`, `size:`,
  `match:`, folder paths (`docs\report`), `in:` scopes; parallel scans,
  narrowing of previous results while typing, highlighted matches.
- Windows UI drawn with Direct2D: sidebar categories and places, details
  pane, Explorer context menu, Filters, Settings, Help, themes (Mica,
  Acrylic, solid, custom colors and background), color picker with screen
  pick, global shortcut, tray, start at sign-in, first-run guide.
- Sharing over HTTPS and FTPS with sign-in, LAN QR code, Cloudflare/ngrok
  tunnels, photo and video viewing.
- Date filters: `dm:today`, `dm:lastweek`, `dm:2026-11`, ranges, and a
  Date modified section in Filters.
- Folder totals: folder sizes and item counts in the Size column, sorting,
  the details pane, and `size:` filters.
- **Live updates** from the NTFS change journal (USN) for whole drives:
  changes appear within about a second, folder totals included, and the
  changes are folded into a new index when idle instead of rebuilding.
- Shared page: folder .zip downloads, file actions (rename, move, copy,
  delete to the Recycle Bin), folder sizes, sorting, gestures in the viewer,
  installable on phones.
- Linux release tarball built next to the .msi by a release workflow.
- Update check: once a day (optional), a notification offers the new
  installer; Settings → Window has Check now.
- Linux window (a local page in a browser app window), CLI, per-user .msi
  installer with upgrades, CI on Windows and Linux.

## Next

- **No UAC prompt**: read drives through a service installed by the .msi
  instead of an elevated helper started at each launch.
- Removable drives: index USB drives when they appear, drop them when they
  go.
- Code-signed installer, and a winget package.
- File watcher on Linux instead of timer rebuilds.

## Known gaps

- Date filters cover the modification date only; no calendar picker.
- Only whole NTFS drives are followed live. Folders, network shares, and the
  timed update modes are still rebuilt when the folder watcher reports
  changes, so there a new file shows up within about a minute.
- On Linux the index refreshes on a timer and has no NTFS fast path.
- The installer is not code signed, so SmartScreen warns on first run.
- The UI test harness needs an interactive desktop; it occasionally fails
  with "window has no client area" and passes on a rerun.
