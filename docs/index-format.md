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
| 12 | 4 | feature bits (see below) |
| 16 | 8 | generation |
| 24 | 4 | entry count |
| 32 | 8 | meta byte length |
| 40 | 8 | search blob byte length |
| 48 | 8 | display blob byte length |
| 56 | 8 | FRN index byte length |
| 64 | 8 | checkpoint byte length |
| 72 | 8 | slot sequence |
| 80 | 4 | CRC32 of bytes 0..80 |

Feature bits: bit 0 (`FEATURE_FOLDER_TOTALS`) means folder sizes are totals
and the `.count` table exists. The word sits in bytes that format version 1
always wrote as zero, so indexes from before folder totals open with no
features, and older readers, which ignore the word, still open new indexes.

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
| `base-<gen>.count` | With folder totals only: u32 per entry, in meta order. A folder's number of files and folders anywhere below it; 0 for files. |
| `prefix.txt` | Display prefix for path reconstruction, for example `C:\FeatherBench`. |

The meta record is:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | file reference number |
| 8 | 8 | parent file reference number |
| 16 | 8 | logical size; for a folder, the total below it (see Folder totals) |
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

## Folder totals

With feature bit 0 set, a folder's `size` is the sum of the logical sizes of
every file anywhere below it, and its `.count` entry is the number of files
and folders anywhere below it (not counting itself). The root entry holds the
totals of the whole index. Both are computed by the writer from the parent
links, never by reading folders again (`feather_core::totals::
fold_folder_totals`), so they are as fresh as the index: a rebuild updates
them. Hard links are counted once per name, as they are listed. The live
update overlay keeps them right by adding each change to every ancestor.

Because the total lives in `size`, everything that reads sizes sees it:
sorting by size orders folders by their totals, and `size:` filters match
folders by their totals too (`size:>1gb` finds folders holding more than
1 GB; `type:file size:>1gb` keeps to files). This is how Everything treats
folder sizes.

Indexes written before folder totals have no feature bit and folders of size
0. They open as before: the app shows no size for their folders, and they
gain totals on their next rebuild.

## Validation

On open, the reader checks magic, version, slot CRC, all table lengths
(including the `.count` table when the feature bit says it exists), and
every name offset and length. It validates the FRN table independently: sorted
by `(FRN, id)`, every id in range, every FRN matching its entry record, and
every entry id appearing exactly once. The meta table is in name order, so its
FRNs are deliberately not assumed to be sorted. A corrupt generation returns an
error and the caller rebuilds.

## Current limitations

- Only a base generation is implemented. Delta runs and compaction are next.
- The synthetic generator writes entries already sorted alphabetically.
- The directory-walk scanner synthesizes parent FRNs from the walk and rewrites
  them after the name sort; they are stable within one index generation but are
  not the NTFS file IDs that the MFT reader will produce.
- Search supports literal terms, quoted phrases, `!` negation, `*`/`?`
  wildcards, `ext:`, `type:`, `size:`, and `in:`/`path:`/`folder:` scopes. Date
  filters are parsed but currently produce a warning.
- The UI caps loaded rows at 50,000. Empty-query listing is therefore the first
  50,000 rows, not every entry.
