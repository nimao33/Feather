# Index format, version 1

The index is a directory per volume. Both the synthetic generator and the
portable directory-walk scanner write this layout; it is the same one the MFT
builder will write.

## Library layout

A single index directory contains `manifest.bin`, the data files, and
`prefix.txt`. An `IndexSet` also accepts a library directory: one subdirectory
per volume, each a complete index directory, for example:

```
library/
  vol-c/                 <- C:\
  vol-d/                 <- D:\
  vol-1a2b3c4d5e6f7890/  <- C:\Users
```

`IndexSet::open` detects both forms. A library gives every volume a mask bit
(`1 << volume index`, at most 64 volumes). Entry references pack the volume
index in the high 32 bits and the entry ID in the low 32 bits. `prefix.txt` is
the display root used to resolve a path.

## Manifest

`manifest.bin` contains two 4 KB slots. Each slot has:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | magic `FTHRIDX1` |
| 8 | 4 | format version |
| 16 | 8 | generation |
| 24 | 4 | entry count |
| 32 | 8 | meta byte length |
| 40 | 8 | search blob byte length |
| 48 | 8 | display blob byte length |
| 56 | 8 | FRN index byte length |
| 64 | 8 | checkpoint byte length |
| 72 | 8 | slot sequence |
| 80 | 4 | CRC32 of bytes 0..80 |

The newest valid slot wins. A slot is written in place, flushed, and only then
used. A crash during a manifest write leaves the other slot intact.

## Data files

| File | Contents |
| --- | --- |
| `base-<gen>.meta` | 48-byte records, one per entry, in search-name order. |
| `base-<gen>.search` | Lowercase UTF-8 names, each followed by a NUL separator. |
| `base-<gen>.display` | Original UTF-16LE names, each followed by a NUL code unit. |
| `base-<gen>.frn` | 12-byte records: file reference number (u64), entry ID (u32), sorted by FRN then ID. |
| `base-<gen>.check` | Every 256th entry: search offset (u32), display offset (u32). |
| `prefix.txt` | Display prefix for path reconstruction, for example `C:\FeatherBench`. |
| `journal.bin` | Optional. Where the volume's NTFS change journal stood when this generation was read (see below). |

The meta record is:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | file reference number |
| 8 | 8 | parent file reference number |
| 16 | 8 | logical size |
| 24 | 8 | modified FILETIME, UTC |
| 32 | 4 | display name offset |
| 36 | 4 | search name offset |
| 40 | 2 | display name length in UTF-16 units |
| 42 | 2 | search name length in UTF-8 bytes |
| 44 | 2 | flags |
| 46 | 2 | reserved |

Flags: bit 0 directory, bit 1 hidden, bit 2 system, bit 3 read-only, bit 4
archive, bit 5 reparse point, bit 6 deleted, bit 7 compressed, bit 8 encrypted,
bit 9 sparse.

Paths are reconstructed from parent FRNs, not stored. Entry 0 is the volume
root, with an empty name and a parent FRN equal to its own. Every file and
directory therefore costs one fixed record plus its two name copies. The index
does not keep a 1M-entry heap structure at query time; the reader maps these
files read-only.

## Validation

On open, the reader checks magic, version, slot CRC, all table lengths, and
every name offset and length. It validates the FRN table independently: sorted
by `(FRN, id)`, every id in range, every FRN matching its entry record, and
every entry id appearing exactly once. The meta table is in name order, so its
FRNs are deliberately not assumed to be sorted. A corrupt generation returns an
error and the caller rebuilds.

## Journal position (`journal.bin`)

Written by the NTFS reader for whole drives, and by a fold, never changed
afterwards. 28 bytes:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | magic `FTHRUSN1` |
| 8 | 8 | USN journal id |
| 16 | 8 | next USN to read (i64) |
| 24 | 4 | CRC32 of bytes 0..24 |

A missing or damaged file means the volume is not followed through a journal.
For these volumes the FRNs in the meta and FRN tables are the real NTFS file
reference numbers (record number in the low 48 bits, sequence number in the
high 16), so they match what the journal reports. The directory-walk scanner
and the synthetic generator still make FRNs up from the entry order and write
no `journal.bin`.

## Live updates

Base files are immutable; live changes never touch them. Instead
(`feather-core/src/live.rs`):

- **Reading.** The elevated helper answers `usn<TAB>C:<TAB>journal id
  (hex)<TAB>next USN` with the changes since that position. It reads
  `FSCTL_READ_USN_JOURNAL` (USN_RECORD_V2/V3, parsed in `usn.rs`), keeps one
  change per file (its newest name and parent, or a delete), and asks each
  changed file for its size, time, and attributes with `OpenFileById`. At
  most 100,000 records are read per request; `more` says another request
  will find more. If the journal no longer reaches back to the position
  (deleted, recreated, or wrapped), the answer is `NotFound` and the app
  rebuilds that drive once.
- **Change stream.** The answer is status byte 0 followed by:
  `FUS1`, journal id (u64), next USN (i64), more (u8), change count (u32),
  then per change a kind byte: `2` delete + FRN (u64); or `1` upsert + FRN,
  parent FRN, size (u64, `u64::MAX` when unknown), mtime (u64), flags (u16),
  created (u8), name length (u16), UTF-8 name. All little-endian.
- **Overlay.** Each followed volume gets an in-memory `Overlay` next to its
  mapped base. New, renamed, moved, or resized entries are added to the
  overlay with entry ids from the base entry count upwards (`EntryRef` packs
  them like any id); the base entry they replace, and deleted base entries,
  are marked in a removed bit set. Overlay ids never move while the base is
  the same, so rows already on screen stay valid. Lookups by FRN, path
  resolution (overlay parents included), folder lookups, counts, and the
  name-ordered id iteration (`VolumeIndex::iter_ids`, `live_runs`) all see
  base and overlay together. A deleted folder takes everything below it
  along, since the journal does not list a folder's contents. A folder that
  appears without being created (moved in from outside the index) is listed
  from the disk. Applying the same changes twice changes nothing, so
  replaying from an older position is harmless.
- **Snapshots.** After each round with visible changes, the app publishes a
  new `IndexSet` sharing the same mapped bases with copies of the overlays
  (names are shared, not copied) and searches it like a new index.
- **Folding.** When an overlay holds 50,000 entries (added plus removed),
  holds 5,000 and the journals have been quiet for ten minutes, or still has
  changes six hours after following started, the volume is written out as a new base
  generation in a staged library (`live::fold` streams the name-sorted base
  and overlay together; other volumes are hard-linked over) with the
  overlay's journal position in its `journal.bin`, then swapped in like a
  rebuild. At startup a volume catches up from its stored position; only a
  lost journal, declined admin rights, or a manual Reindex read it again.
- **Folder totals hook.** `Overlay::folder_delta(frn)` returns the change in
  recursive size and item count below a folder since the base was written.
  Every applied change adds its contribution (size and one item, plus the
  folder's own delta for a folder) to each ancestor. A base that stores each
  folder's recursive total in its meta `size` field and an item count per
  entry adds these deltas when reading and when folding; until then the
  deltas are kept but unused.

Folders, network shares, and other non-NTFS roots keep the folder watcher and
rebuilds, as do the timed update modes; Linux refreshes on a timer.

## Current limitations

- The synthetic generator writes entries already sorted alphabetically.
- The directory-walk scanner synthesizes parent FRNs from the walk and rewrites
  them after the name sort; they are stable within one index generation but are
  not NTFS file IDs, so such volumes are not followed through a journal.
- Search supports literal terms, quoted phrases, `!` negation, `*`/`?`
  wildcards, `ext:`, `type:`, `size:`, and `in:`/`path:`/`folder:` scopes. Date
  filters are parsed but currently produce a warning.
- The UI caps loaded rows at 50,000. Empty-query listing is therefore the first
  50,000 rows, not every entry.
