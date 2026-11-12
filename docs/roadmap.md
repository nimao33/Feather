# Roadmap

## Done (1.0.0)

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
- Linux window (a local page in a browser app window), CLI, per-user .msi
  installer with upgrades, CI on Windows and Linux.

## In progress

- **Live updates** from the NTFS change journal (USN): changes appear within
  about a second and whole-drive rebuilds become rare, removing the brief
  memory spike each rebuild causes.
- **Folder downloads** from the shared page as a streamed .zip.
- **Linux release** tarball built by a release workflow next to the .msi.

## Next

- **No UAC prompt**: read drives through a service installed by the .msi
  instead of an elevated helper started at each launch.
- Removable drives: index USB drives when they appear, drop them when they
  go.
- Code-signed installer, and a winget package.
- File watcher on Linux instead of timer rebuilds.

## Known gaps

- Date filters cover the modification date only; no calendar picker.
- On Linux the index refreshes on a timer and has no NTFS fast path.
- The installer is not code signed, so SmartScreen warns on first run.
- The UI test harness needs an interactive desktop; it occasionally fails
  with "window has no client area" and passes on a rerun.
