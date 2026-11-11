# Benchmarks

Measured on 2026-11-09 with the release build, static CRT, 1,000,000-entry
synthetic index, after compiled name terms, match modes, and the slim metadata
read used by the search worker.

## Machine

| Component | Value |
| --- | --- |
| CPU | AMD Ryzen 5 5600G |
| RAM | 16 GB |
| OS | Windows 11 Pro Insider Preview, x64 |
| Build | `cargo build --release`, `lto = "thin"`, `codegen-units = 1` |

## Index

| Metric | Value |
| --- | --- |
| Entries | 1,000,000 (999,999 items plus the volume root) |
| Folders | 2,000 |
| Files | 997,999 |
| Size on disk | 102.6 MB |
| Generation time | 483 to 578 ms |

The index was generated once and reused for all measurements.

## Search scan

Command:

```powershell
.\target\release\feather.exe --bench --index target\index-1m --iterations 5
```

The benchmark runs one warm-up pass, then five measured passes per query. It
scans the mapped index exactly like the search worker, including the 50,000
result cap, but excludes UI formatting and batching.

| Query | Matches | p50 | p95 | max |
| --- | ---: | ---: | ---: | ---: |
| empty, capped | 50,000+ | 0.20 ms | 0.26 ms | 0.26 ms |
| `file0000005` | 1 | 7.61 ms | 8.21 ms | 8.21 ms |
| `match:starts file0000005` | 1 | 7.10 ms | 7.51 ms | 7.51 ms |
| `match:ends .pdf`, capped | 50,000+ | 2.70 ms | 3.11 ms | 3.11 ms |
| `ext:pdf`, capped | 50,000+ | 1.94 ms | 2.11 ms | 2.11 ms |
| `type:folder` | 2,000 | 2.57 ms | 3.20 ms | 3.20 ms |

`+` means the scan stopped at the 50,000 result cap.

The single-match case scans the entire 1M-entry name table. Each plain name
term is compiled once per query into a Boyer–Moore–Horspool matcher, longest
term first, so the per-name work is a short skip-table search instead of
preparing a pattern a million times. The worker also reads only the metadata a
query needs (name, parent, size, flags) and skips the display name and file
reference number unless path resolution actually asks for them. Against the
previous scan this cut the single-match case from about 25 ms to 7.6 ms, the
extension cap from 7.7 ms to 1.9 ms, and folders from 15.7 ms to 2.6 ms.

## UI

At 1M entries, the UI scenarios complete in a few seconds including process
startup, window creation, searches, sorting, dialog captures, and PNG capture.
Scenario `01-all` loads the 50,000-row result cap, `07-filtered` loads the same
cap of PDFs and highlights the typed `file` in every name, and `11-scope` loads
500 rows inside `C:\FeatherBench\dir0001`.

The UI search time is larger than the raw scan because it includes worker
batching, generation checks, ListView updates, and status updates. It is a
whole-window number, not a controlled benchmark.

## Memory

Method: use a fresh profile with `setup_done=1`, `results_percent=5` (the
50,000-row cap at 1M), and no index roots, so the app neither indexes nor shows
first-run setup. Start `feather.exe` with the 1M index, wait five seconds for
the first search to finish, then sample the process's `Working Set - Private`
counter once per second.

| State | Metric | Value |
| --- | --- | ---: |
| Window open, 50,000 rows | Private working set | 10.94 MB |
| Window open, 50,000 rows | Private commit | 17.36 MB |
| Hidden in the tray | Private working set | 3.07 MB |
| Hidden in the tray | Private commit | 4.08 MB |

The visible figure is the whole UI: Direct2D, DirectWrite, the window, and the
50,000 result references. Hidden in the tray, Windows trims the same process to
about 3 MB private. Total working set is much larger (200 to 210 MB) because it
counts the read-only index mapping and other clean, shareable pages; those are
not private committed memory and are evicted under memory pressure. The shell
context menu adds no idle cost because `IContextMenu` is created only on
right-click and released after the menu closes.

The target is at most 15 MB combined private working set for the UI and service
with 1,000,000 entries. Only the UI exists so far: it uses 10.94 MB with 50,000
rows visible and 3.07 MB hidden, leaving room for a small service. The older
2.8 MB figure in earlier notes was a hidden-to-tray sample, not an open window.

## Reproduce

```powershell
.\scripts\build.ps1
.\target\release\feather.exe --gen-index --index target\index-1m --entries 1000000
.\target\release\feather.exe --bench --index target\index-1m --iterations 5
.\target\release\feather.exe --index target\index-1m --ui-test --screenshots target\shots\ui-test-1m
```

To measure a real scan instead of the synthetic generator:

```powershell
.\target\release\feather.exe index scan --root C:\Users --index target\real-index
.\target\release\feather.exe index status --index target\real-index
.\target\release\feather.exe search --index target\real-index --limit 20 report
```

Or run the whole pipeline:

```powershell
.\scripts\verify.ps1
```

## Caveats

- The index is synthetic and deterministic. Its names and directory fan-out do
  not match a real volume.
- The benchmark is a warm-cache scan. A cold first search pays for page faults
  from disk.
- UI numbers include screenshot capture and window startup in the scenario
  total, and only the status-bar search time for the individual query.
- These numbers should be re-measured after the real `$MFT` builder lands.
