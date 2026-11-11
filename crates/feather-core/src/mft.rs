//! Reads an NTFS volume's master file table directly.
//!
//! Every file and folder on NTFS has a record in `$MFT`, with its name,
//! parent folder, size, times, and attributes. Reading that table front to
//! back is one long sequential read of the volume, which is why tools that
//! do it index a whole drive in seconds while a folder walk takes minutes.
//! It needs admin rights, because it opens the raw volume (`\\.\C:`).
//!
//! The table is streamed in large chunks along the `$MFT` file's own extent
//! list, each record's update-sequence fixups are applied, and the fields
//! are kept in flat arrays indexed by record number. Extension records (for
//! files with many attributes) add their names and sizes to their base
//! record. Finally the folder tree under the requested root is turned into
//! the same entries the folder walk produces and written with the shared
//! writer.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::scan::{display_root, Entries, ScanOptions, ScanReport};

const ROOT_RECORD: usize = 5;
/// Records below this are NTFS's own metadata files ($MFT, $Bitmap, ...).
const FIRST_USER_RECORD: usize = 16;
/// The update-sequence stride is 512 bytes whatever the sector size.
const FIXUP_STRIDE: usize = 512;
const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const NO_RECORD: u32 = u32::MAX;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_BITMAP: u32 = 0xb0;
/// A run of this many unused records ends a read slice, so empty parts of
/// the table are not read.
const SKIP_GAP: usize = 64;
const ATTR_END: u32 = 0xffff_ffff;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn unsupported(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message.to_string())
}

struct Boot {
    cluster: u64,
    mft_offset: u64,
    record_size: usize,
}

fn parse_boot(sector: &[u8]) -> io::Result<Boot> {
    if sector.get(3..11) != Some(b"NTFS    ".as_slice()) {
        return Err(unsupported("not an NTFS volume"));
    }
    let bytes_per_sector = u64::from(u16_at(sector, 0x0b).unwrap_or(0));
    let raw_spc = sector[0x0d];
    // Clusters above 64 KiB store the power of two as a negative number.
    let sectors_per_cluster = if raw_spc > 0x80 {
        1u64 << (256 - u32::from(raw_spc))
    } else {
        u64::from(raw_spc)
    };
    let cluster = bytes_per_sector * sectors_per_cluster;
    let mft_lcn = u64_at(sector, 0x30).unwrap_or(0);
    let raw_record = sector[0x40] as i8;
    let record_size = if raw_record > 0 {
        raw_record as u64 * cluster
    } else {
        1u64 << (-(raw_record as i32)) as u32
    };
    if cluster == 0 || !(256..=65_536).contains(&record_size) || bytes_per_sector < 512 {
        return Err(unsupported("unexpected NTFS boot sector"));
    }
    Ok(Boot {
        cluster,
        mft_offset: mft_lcn * cluster,
        record_size: record_size as usize,
    })
}

/// Checks the record signature and restores the bytes the update sequence
/// array replaced at the end of each 512-byte stride.
fn apply_fixup(record: &mut [u8]) -> bool {
    if record.get(0..4) != Some(b"FILE".as_slice()) {
        return false;
    }
    let (Some(offset), Some(count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let (offset, count) = (offset as usize, count as usize);
    if count == 0 || offset + count * 2 > record.len() {
        return false;
    }
    let usn = [record[offset], record[offset + 1]];
    for stride in 1..count {
        let end = stride * FIXUP_STRIDE;
        if end > record.len() {
            break;
        }
        if record[end - 2..end] != usn {
            return false;
        }
        record[end - 2] = record[offset + stride * 2];
        record[end - 1] = record[offset + stride * 2 + 1];
    }
    true
}

/// Calls `visit(type, offset)` for each attribute header in a record.
fn for_each_attribute(record: &[u8], mut visit: impl FnMut(u32, usize)) {
    let used = u32_at(record, 0x18)
        .map(|used| (used as usize).min(record.len()))
        .unwrap_or(record.len());
    let mut offset = u16_at(record, 0x14).unwrap_or(0) as usize;
    while offset + 16 <= used {
        let Some(kind) = u32_at(record, offset) else {
            break;
        };
        if kind == ATTR_END {
            break;
        }
        let Some(length) = u32_at(record, offset + 4) else {
            break;
        };
        let length = length as usize;
        if length < 16 || offset + length > used {
            break;
        }
        visit(kind, offset);
        offset += length;
    }
}

/// The content of a resident attribute.
fn resident(record: &[u8], offset: usize) -> Option<&[u8]> {
    if record.get(offset + 8) != Some(&0) {
        return None;
    }
    let length = u32_at(record, offset + 0x10)? as usize;
    let start = offset + u16_at(record, offset + 0x14)? as usize;
    record.get(start..start + length)
}

/// Decodes a non-resident attribute's run list into (cluster, count) pairs.
/// Sparse runs have no cluster.
fn runs(record: &[u8], offset: usize, length: usize) -> Vec<(Option<u64>, u64)> {
    let mut out = Vec::new();
    let Some(start) = u16_at(record, offset + 0x20) else {
        return out;
    };
    let end = (offset + length).min(record.len());
    let mut at = offset + start as usize;
    let mut lcn: i64 = 0;
    while at < end {
        let header = record[at];
        if header == 0 {
            break;
        }
        let length_size = (header & 0x0f) as usize;
        let offset_size = (header >> 4) as usize;
        at += 1;
        if length_size == 0
            || length_size > 8
            || offset_size > 8
            || at + length_size + offset_size > end
        {
            break;
        }
        let mut count = 0u64;
        for byte in 0..length_size {
            count |= u64::from(record[at + byte]) << (8 * byte);
        }
        at += length_size;
        if offset_size == 0 {
            out.push((None, count));
            continue;
        }
        let mut delta = 0i64;
        for byte in 0..offset_size {
            delta |= i64::from(record[at + byte]) << (8 * byte);
        }
        // Sign-extend the variable-width delta.
        let shift = 64 - 8 * offset_size as u32;
        delta = (delta << shift) >> shift;
        at += offset_size;
        lcn += delta;
        if lcn < 0 {
            break;
        }
        out.push((Some(lcn as u64), count));
    }
    out
}

/// A name or size found in an extension record, applied to its base record
/// after all records are read.
struct Extension {
    base: usize,
    rank: u8,
    parent: u32,
    name: Vec<u16>,
    size: Option<u64>,
}

/// Everything kept per MFT record, in flat arrays, for the records from
/// `first` on. Worker threads each fill one for a slice of the table; the
/// slices are then copied into one table covering every record.
struct Table {
    first: usize,
    /// Number of records in the whole volume, for range checks.
    total: usize,
    /// Bit 0: in use and a base record. Bit 1: a folder.
    state: Vec<u8>,
    parent: Vec<u32>,
    /// Preference of the stored name: Win32 over POSIX over DOS 8.3.
    name_rank: Vec<u8>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    size: Vec<u64>,
    mtime: Vec<u64>,
    attributes: Vec<u32>,
    names: Vec<u16>,
    extensions: Vec<Extension>,
}

fn name_rank(space: u8) -> u8 {
    match space {
        1 | 3 => 3,
        0 => 2,
        _ => 1,
    }
}

impl Table {
    fn new(first: usize, count: usize, total: usize) -> Self {
        Self {
            first,
            total,
            state: vec![0; count],
            parent: vec![NO_RECORD; count],
            name_rank: vec![0; count],
            name_start: vec![0; count],
            name_len: vec![0; count],
            size: vec![0; count],
            mtime: vec![0; count],
            attributes: vec![0; count],
            names: Vec::with_capacity(count * 12),
            extensions: Vec::new(),
        }
    }

    fn name(&self, record: usize) -> &[u16] {
        let start = self.name_start[record] as usize;
        &self.names[start..start + self.name_len[record] as usize]
    }

    fn parent_of(&self, raw: u64) -> u32 {
        let parent = (raw & 0xffff_ffff_ffff) as usize;
        if parent < self.total {
            parent as u32
        } else {
            NO_RECORD
        }
    }

    fn add(&mut self, number: usize, record: &mut [u8]) {
        if !apply_fixup(record) {
            return;
        }
        let flags = u16_at(record, 0x16).unwrap_or(0);
        if flags & 1 == 0 {
            return;
        }
        let base = (u64_at(record, 0x20).unwrap_or(0) & 0xffff_ffff_ffff) as usize;
        let record: &[u8] = record;
        if base != 0 {
            self.add_extension(base, record);
            return;
        }
        let slot = number - self.first;
        self.state[slot] |= 1 | if flags & 2 != 0 { 2 } else { 0 };
        for_each_attribute(record, |kind, offset| match kind {
            ATTR_STANDARD_INFORMATION => {
                if let Some(content) = resident(record, offset) {
                    self.mtime[slot] = u64_at(content, 8).unwrap_or(0);
                    self.attributes[slot] = u32_at(content, 32).unwrap_or(0);
                }
            }
            ATTR_FILE_NAME => {
                let Some((parent, space, bytes)) = file_name(record, offset) else {
                    return;
                };
                let rank = name_rank(space);
                if rank <= self.name_rank[slot] {
                    return;
                }
                self.parent[slot] = self.parent_of(parent);
                self.name_rank[slot] = rank;
                self.name_start[slot] = self.names.len() as u32;
                self.name_len[slot] = (bytes.len() / 2) as u16;
                self.names.extend(
                    bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_le_bytes(*pair)),
                );
            }
            ATTR_DATA => {
                if let Some(size) = data_size(record, offset) {
                    self.size[slot] = size;
                }
            }
            _ => {}
        });
    }

    fn add_extension(&mut self, base: usize, record: &[u8]) {
        if base >= self.total {
            return;
        }
        let mut extension = Extension {
            base,
            rank: 0,
            parent: NO_RECORD,
            name: Vec::new(),
            size: None,
        };
        for_each_attribute(record, |kind, offset| match kind {
            ATTR_FILE_NAME => {
                if let Some((parent, space, bytes)) = file_name(record, offset) {
                    let rank = name_rank(space);
                    if rank > extension.rank {
                        extension.rank = rank;
                        extension.parent = self.parent_of(parent);
                        extension.name = bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| u16::from_le_bytes(*pair))
                            .collect();
                    }
                }
            }
            ATTR_DATA => {
                if let Some(size) = data_size(record, offset) {
                    extension.size = Some(size);
                }
            }
            _ => {}
        });
        if extension.rank > 0 || extension.size.is_some() {
            self.extensions.push(extension);
        }
    }

    /// Copies a slice table into this whole-volume table.
    fn absorb(&mut self, mut part: Table) {
        let start = part.first;
        let end = start + part.state.len();
        let names_base = self.names.len() as u32;
        self.state[start..end].copy_from_slice(&part.state);
        self.parent[start..end].copy_from_slice(&part.parent);
        self.name_rank[start..end].copy_from_slice(&part.name_rank);
        self.name_len[start..end].copy_from_slice(&part.name_len);
        self.size[start..end].copy_from_slice(&part.size);
        self.mtime[start..end].copy_from_slice(&part.mtime);
        self.attributes[start..end].copy_from_slice(&part.attributes);
        for (slot, value) in self.name_start[start..end].iter_mut().zip(&part.name_start) {
            *slot = value + names_base;
        }
        self.names.append(&mut part.names);
        self.extensions.append(&mut part.extensions);
    }

    fn apply_extensions(&mut self) {
        for extension in std::mem::take(&mut self.extensions) {
            let base = extension.base;
            if extension.rank > self.name_rank[base] {
                self.name_rank[base] = extension.rank;
                self.parent[base] = extension.parent;
                self.name_start[base] = self.names.len() as u32;
                self.name_len[base] = extension.name.len() as u16;
                self.names.extend_from_slice(&extension.name);
            }
            if let Some(size) = extension.size {
                self.size[base] = size;
            }
        }
    }
}

/// A resident `$FILE_NAME`: parent reference, namespace, and UTF-16 bytes.
fn file_name(record: &[u8], offset: usize) -> Option<(u64, u8, &[u8])> {
    let content = resident(record, offset)?;
    let parent = u64_at(content, 0)?;
    let length = *content.get(0x40)? as usize;
    let space = *content.get(0x41)?;
    let bytes = content.get(0x42..0x42 + length * 2)?;
    Some((parent, space, bytes))
}

/// The size of the unnamed `$DATA` stream, from its first piece.
fn data_size(record: &[u8], offset: usize) -> Option<u64> {
    if record.get(offset + 9) != Some(&0) {
        return None;
    }
    if record.get(offset + 8) == Some(&0) {
        u32_at(record, offset + 0x10).map(u64::from)
    } else if u64_at(record, offset + 0x10) == Some(0) {
        u64_at(record, offset + 0x30)
    } else {
        None
    }
}

/// Reads at an absolute volume offset without moving a shared position, so
/// several threads can read one handle.
fn read_at(volume: &File, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buffer.len() {
        let read = volume.seek_read(&mut buffer[done..], offset + done as u64)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the volume ended inside the master file table",
            ));
        }
        done += read;
    }
    Ok(())
}

/// Unbuffered reads need sector-aligned offsets, lengths, and memory; 4 KiB
/// covers both 512-byte and 4 KiB sectors.
const ALIGN: usize = 4096;

/// Opens the volume so reads skip the file cache: the table is read once,
/// so caching it would only add a copy of every byte.
fn open_unbuffered(volume_path: &str) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(volume_path)
}

/// Reads `length` bytes at `offset` through an unbuffered handle, widening
/// the request to aligned bounds inside `buffer`, and returns just the
/// requested bytes.
fn read_aligned<'a>(
    volume: &File,
    offset: u64,
    length: usize,
    buffer: &'a mut [u8],
) -> io::Result<&'a mut [u8]> {
    let start = offset & !(ALIGN as u64 - 1);
    let skip = (offset - start) as usize;
    let total = (skip + length).div_ceil(ALIGN) * ALIGN;
    let pad = buffer.as_ptr().align_offset(ALIGN);
    if pad + total > buffer.len() {
        return Err(io::Error::other("read buffer too small"));
    }
    let window = &mut buffer[pad..pad + total];
    read_at(volume, start, window)?;
    Ok(&mut window[skip..skip + length])
}

/// A contiguous run of records on disk, read and parsed by one worker.
struct Slice {
    first: usize,
    count: usize,
    /// `None` for a sparse run, which holds no records.
    offset: Option<u64>,
}

fn cancelled(options: &ScanOptions) -> bool {
    options
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

fn read_table(volume_path: &str, options: &ScanOptions) -> io::Result<Table> {
    let volume = &File::open(volume_path)?;
    let mut sector = vec![0u8; 512];
    read_at(volume, 0, &mut sector)?;
    let boot = parse_boot(&sector)?;

    // Record 0 describes $MFT itself: its size and where its pieces are.
    let mut first = vec![0u8; boot.record_size.max(512)];
    read_at(volume, boot.mft_offset, &mut first)?;
    let first = &mut first[..boot.record_size];
    if !apply_fixup(first) {
        return Err(unsupported("the $MFT record is damaged"));
    }
    let mut extents = Vec::new();
    let mut mft_size = 0u64;
    // $MFT's $BITMAP has one bit per record: set when the record is in use.
    let mut bitmap = Vec::new();
    let mut bitmap_runs = Vec::new();
    let mut bitmap_size = 0u64;
    let first_ref: &[u8] = first;
    for_each_attribute(first_ref, |kind, offset| {
        let unnamed = first_ref.get(offset + 9) == Some(&0);
        let non_resident = first_ref.get(offset + 8) == Some(&1);
        let length = u32_at(first_ref, offset + 4).unwrap_or(0) as usize;
        if kind == ATTR_DATA && non_resident && unnamed {
            if u64_at(first_ref, offset + 0x10) == Some(0) {
                mft_size = u64_at(first_ref, offset + 0x30).unwrap_or(0);
            }
            extents.extend(runs(first_ref, offset, length));
        } else if kind == ATTR_BITMAP && unnamed {
            if non_resident {
                bitmap_size = u64_at(first_ref, offset + 0x30).unwrap_or(0);
                bitmap_runs.extend(runs(first_ref, offset, length));
            } else if let Some(content) = resident(first_ref, offset) {
                bitmap = content.to_vec();
            }
        }
    });
    for (lcn, clusters) in bitmap_runs {
        if bitmap.len() as u64 >= bitmap_size {
            break;
        }
        let mut piece = vec![0u8; (clusters * boot.cluster) as usize];
        if let Some(lcn) = lcn {
            read_at(volume, lcn * boot.cluster, &mut piece)?;
        }
        bitmap.extend_from_slice(&piece);
    }
    if bitmap_size > 0 {
        bitmap.truncate(bitmap_size as usize);
    }
    if extents.is_empty() || mft_size == 0 {
        return Err(unsupported("could not find the $MFT data"));
    }
    let records = (mft_size / boot.record_size as u64) as usize;
    let record_size = boot.record_size as u64;
    if boot.cluster % record_size != 0 {
        // Records can straddle pieces of $MFT when clusters are smaller
        // than records; such volumes are rare, so they use the folder walk.
        return Err(unsupported("clusters smaller than MFT records"));
    }

    // Cut the table into slices of at most CHUNK_BYTES along its extents,
    // leaving out stretches of unused records. Without a bitmap, every
    // record is read.
    let in_use = |record: usize| {
        bitmap
            .get(record / 8)
            .is_none_or(|byte| byte & (1 << (record % 8)) != 0)
    };
    let per_slice = (CHUNK_BYTES / record_size) as usize;
    let mut slices = Vec::new();
    let mut next = 0usize;
    for (lcn, clusters) in extents {
        let in_extent = (clusters * boot.cluster / record_size) as usize;
        let extent_start = next;
        let extent_end = (next + in_extent).min(records);
        let Some(lcn) = lcn else {
            next = extent_end;
            continue;
        };
        while next < extent_end {
            while next < extent_end && !in_use(next) {
                next += 1;
            }
            if next >= extent_end {
                break;
            }
            let start = next;
            let mut last_used = next;
            while next < extent_end && next - start < per_slice {
                if in_use(next) {
                    last_used = next;
                } else if next - last_used >= SKIP_GAP {
                    break;
                }
                next += 1;
            }
            slices.push(Slice {
                first: start,
                count: last_used - start + 1,
                offset: Some(lcn * boot.cluster + (start - extent_start) as u64 * record_size),
            });
        }
        next = extent_end;
    }
    if std::env::var_os("FEATHER_SCAN_TIMING").is_some() {
        let read: usize = slices.iter().map(|slice| slice.count).sum();
        eprintln!(
            "mft: {records} records, {} MB of {} MB read in {} slices",
            read * boot.record_size / 1_048_576,
            records * boot.record_size / 1_048_576,
            slices.len()
        );
    }

    // Readers hand over each slice as soon as it is parsed and this thread
    // copies it into the whole table, so slices never pile up: only the
    // table and the few slices in flight are held at once.
    let mut table = Table::new(0, records, records);
    let claimed = std::sync::atomic::AtomicUsize::new(0);
    let parsed = std::sync::atomic::AtomicU32::new(0);
    std::thread::scope(|scope| {
        let (sender, parts) = std::sync::mpsc::sync_channel::<io::Result<Table>>(16);
        for _ in 0..crate::scan::workers().min(slices.len()).max(1) {
            let (slices, claimed, parsed) = (&slices, &claimed, &parsed);
            let sender = sender.clone();
            scope.spawn(move || {
                let read = || -> io::Result<()> {
                    // Each reader has its own handle: Windows runs one
                    // request at a time on a synchronous handle.
                    let volume = &open_unbuffered(volume_path)?;
                    // Room for a slice plus alignment on both ends.
                    let mut buffer = vec![0u8; per_slice * boot.record_size + 3 * ALIGN];
                    loop {
                        let index = claimed.fetch_add(1, Ordering::Relaxed);
                        let Some(slice) = slices.get(index) else {
                            return Ok(());
                        };
                        if cancelled(options) {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "scan cancelled",
                            ));
                        }
                        let mut part = Table::new(slice.first, slice.count, records);
                        if let Some(offset) = slice.offset {
                            let bytes = read_aligned(
                                volume,
                                offset,
                                slice.count * boot.record_size,
                                &mut buffer,
                            )?;
                            for (index, record) in
                                bytes.chunks_exact_mut(boot.record_size).enumerate()
                            {
                                part.add(slice.first + index, record);
                            }
                        }
                        if sender.send(Ok(part)).is_err() {
                            return Ok(());
                        }
                        let total = parsed.fetch_add(slice.count as u32, Ordering::Relaxed);
                        if let Some(progress) = &options.progress {
                            progress.store(total + slice.count as u32, Ordering::Relaxed);
                        }
                    }
                };
                if let Err(error) = read() {
                    let _ = sender.send(Err(error));
                }
            });
        }
        drop(sender);
        for part in parts {
            // Dropping the receiver on an error stops the other readers.
            table.absorb(part?);
        }
        Ok::<_, io::Error>(())
    })?;
    table.names.shrink_to_fit();

    table.apply_extensions();
    Ok(table)
}

/// `C:\Users\Me` -> ("C:", ["Users", "Me"]). `None` for anything that is
/// not on a drive letter.
fn split_root(root: &str) -> Option<(String, Vec<String>)> {
    let bytes = root.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let parts = root[2..]
        .split(['\\', '/'])
        .filter(|part| !part.is_empty() && *part != ".")
        .map(str::to_string)
        .collect();
    Some((root[..2].to_ascii_uppercase(), parts))
}

/// Case-insensitive comparison of a UTF-16 name with a lowercase string.
fn name_is(name: &[u16], lowercase: &str) -> bool {
    char::decode_utf16(name.iter().copied())
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .flat_map(char::to_lowercase)
        .eq(lowercase.chars())
}

/// Indexes `root` (a drive, or a folder on it) by reading the drive's master
/// file table, and writes the index to `out`.
///
/// Fails with `PermissionDenied` without admin rights and with `Unsupported`
/// for anything that is not a local NTFS volume; callers fall back to
/// [`crate::scan::scan`] then.
pub fn scan(root: &Path, out: &Path, options: &ScanOptions) -> io::Result<ScanReport> {
    let started = Instant::now();
    let mut options = options.clone();
    // Never index the index output itself.
    options
        .excluded_paths
        .push(crate::scan::absolute_display(out));
    let tree = read(root, &options)?;
    crate::scan::write_tree(out, tree, started)
}

/// Reads `root` from the drive's master file table into memory, without
/// writing anything. The elevated helper sends this to the app, which
/// writes the index itself.
pub fn read(root: &Path, options: &ScanOptions) -> io::Result<crate::scan::Tree> {
    let started = Instant::now();
    let root_text = root.to_string_lossy().to_string();
    let Some((drive, parts)) = split_root(&root_text) else {
        return Err(unsupported(
            "only drive-letter paths can use the NTFS reader",
        ));
    };
    let table = read_table(&format!(r"\\.\{drive}"), options)?;
    crate::scan::phase("read table", started);
    let phase_start = Instant::now();

    let records = table.state.len();
    // Child lists, so the tree can be walked from any folder.
    let mut first_child = vec![NO_RECORD; records];
    let mut next_sibling = vec![NO_RECORD; records];
    for record in (FIRST_USER_RECORD..records).rev() {
        if table.state[record] & 1 == 0 || table.name_rank[record] == 0 {
            continue;
        }
        let parent = table.parent[record];
        if parent == NO_RECORD || table.state[parent as usize] & 1 == 0 {
            continue;
        }
        let parent = parent as usize;
        // The root's metadata children ($MFT, $Extend, ...) are below 16.
        if parent < FIRST_USER_RECORD && parent != ROOT_RECORD {
            continue;
        }
        next_sibling[record] = first_child[parent];
        first_child[parent] = record as u32;
    }
    let (first_child, next_sibling) = (&first_child, &next_sibling);
    let children = |record: usize| {
        let mut child = first_child[record];
        std::iter::from_fn(move || {
            if child == NO_RECORD {
                return None;
            }
            let current = child as usize;
            child = next_sibling[current];
            Some(current)
        })
    };
    // Finds the folder record for a path on this drive.
    let find = |parts: &[String]| -> Option<usize> {
        let mut at = ROOT_RECORD;
        for part in parts {
            let wanted = part.to_lowercase();
            at = children(at).find(|&child| {
                table.state[child] & 2 != 0 && name_is(table.name(child), &wanted)
            })?;
        }
        Some(at)
    };

    let Some(start) = find(&parts) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("folder not found in the NTFS table: {root_text}"),
        ));
    };
    // Excluded folders and the index output are skipped by record number,
    // so no path text is built while walking the tree.
    let mut skip_records = std::collections::HashSet::new();
    for path in options.excluded_paths.iter() {
        if let Some((path_drive, path_parts)) = split_root(path) {
            if path_drive == drive && !path_parts.is_empty() {
                if let Some(record) = find(&path_parts) {
                    skip_records.insert(record);
                }
            }
        }
    }
    // (lowercase name, its UTF-16 length) so most names are rejected on
    // length alone.
    let skip_names: Vec<(String, usize)> =
        ["$recycle.bin", "system volume information", "$winreagent"]
            .iter()
            .map(|name| name.to_string())
            .chain(
                options
                    .excluded_names
                    .iter()
                    .map(|name| name.to_lowercase()),
            )
            .map(|name| {
                let units = name.encode_utf16().count();
                (name, units)
            })
            .collect();

    let max_entries = if options.max_entries == 0 {
        ScanOptions::default().max_entries
    } else {
        options.max_entries
    } as usize;
    let root_display = display_root(root);
    let mut report = ScanReport {
        root: root_display.clone(),
        ..ScanReport::default()
    };
    // The tree in walk order: (record, index of its parent entry). Names are
    // converted afterwards, on all cores.
    let mut order: Vec<(u32, u32)> = vec![(start as u32, 0)];
    let mut stack: Vec<(usize, u32)> = vec![(start, 0)];
    while let Some((record, index)) = stack.pop() {
        for child in children(record) {
            if order.len() >= max_entries {
                report.truncated = true;
                break;
            }
            let name = table.name(child);
            if skip_records.contains(&child)
                || skip_names
                    .iter()
                    .any(|(skip, units)| name.len() == *units && name_is(name, skip))
            {
                report.skipped += 1;
                continue;
            }
            let entry = order.len() as u32;
            order.push((child as u32, index));
            if table.state[child] & 2 != 0 {
                stack.push((child, entry));
            }
        }
        if report.truncated {
            break;
        }
    }

    // Names go straight from the table into one buffer: no string per
    // entry, and the table is let go as soon as they are copied.
    let mut entries = Entries::with_capacity(order.len());
    let mut name = String::new();
    for (index, &(record, parent)) in order.iter().enumerate() {
        let record = record as usize;
        let is_dir = table.state[record] & 2 != 0;
        let attributes = table.attributes[record];
        let flags = u16::from(is_dir)
            | if attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
                2
            } else {
                0
            }
            | if attributes & FILE_ATTRIBUTE_SYSTEM != 0 {
                4
            } else {
                0
            };
        name.clear();
        if index == 0 {
            // The root entry is the scanned folder itself: no name, always
            // a folder.
            entries.push("", 0, 0, table.mtime[record], 1);
            continue;
        }
        name.extend(
            char::decode_utf16(table.name(record).iter().copied())
                .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER)),
        );
        let size = if is_dir { 0 } else { table.size[record] };
        entries.push(&name, parent, size, table.mtime[record], flags);
    }
    entries.shrink_to_fit();
    if let Some(progress) = &options.progress {
        progress.store(entries.len() as u32, Ordering::Relaxed);
    }
    drop(order);
    drop(table);
    crate::scan::phase("build tree", phase_start);
    report.elapsed = started.elapsed();
    Ok(crate::scan::Tree {
        root_display,
        entries,
        report,
    })
}

/// Uses the NTFS reader when it can and the folder walk otherwise.
pub fn scan_any(root: &Path, out: &Path, options: &ScanOptions) -> io::Result<ScanReport> {
    match scan(root, out, options) {
        Ok(report) => Ok(report),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) =>
        {
            crate::scan::scan(root, out, options)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_with_fixup() -> Vec<u8> {
        let mut record = vec![0u8; 1024];
        record[0..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&48u16.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        // Update sequence number, then the two saved stride endings.
        record[48..50].copy_from_slice(&[0xab, 0xcd]);
        record[50..52].copy_from_slice(&[1, 2]);
        record[52..54].copy_from_slice(&[3, 4]);
        record[510..512].copy_from_slice(&[0xab, 0xcd]);
        record[1022..1024].copy_from_slice(&[0xab, 0xcd]);
        record
    }

    #[test]
    fn fixups_restore_stride_endings() {
        let mut record = record_with_fixup();
        assert!(apply_fixup(&mut record));
        assert_eq!(&record[510..512], &[1, 2]);
        assert_eq!(&record[1022..1024], &[3, 4]);

        let mut torn = record_with_fixup();
        torn[1022] = 0;
        assert!(!apply_fixup(&mut torn), "a torn write is rejected");
        let mut other = record_with_fixup();
        other[0] = b'B';
        assert!(!apply_fixup(&mut other));
    }

    #[test]
    fn run_lists_decode_relative_signed_offsets() {
        // Attribute at 0 with its run list at 0x40: 0x21 = 1-byte length,
        // 2-byte offset. Second run moves back by 0x10 clusters. Third is
        // sparse.
        let mut record = vec![0u8; 0x60];
        record[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        let list = [
            0x21, 0x08, 0x00, 0x10, // 8 clusters at 0x1000
            0x21, 0x04, 0xf0, 0xff, // 4 clusters at 0x1000 - 0x10
            0x01, 0x02, // 2 sparse clusters
            0x00,
        ];
        record[0x40..0x40 + list.len()].copy_from_slice(&list);
        assert_eq!(
            runs(&record, 0, 0x60),
            vec![(Some(0x1000), 8), (Some(0x0ff0), 4), (None, 2)]
        );
    }

    #[test]
    fn boot_sectors_give_cluster_and_record_sizes() {
        let mut sector = vec![0u8; 512];
        sector[3..11].copy_from_slice(b"NTFS    ");
        sector[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
        sector[0x0d] = 8;
        sector[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        sector[0x40] = 0xf6; // -10: 1024-byte records
        let boot = parse_boot(&sector).expect("boot");
        assert_eq!(boot.cluster, 4096);
        assert_eq!(boot.record_size, 1024);
        assert_eq!(boot.mft_offset, 786_432 * 4096);
        sector[3] = b'X';
        assert_eq!(
            parse_boot(&sector).err().map(|error| error.kind()),
            Some(io::ErrorKind::Unsupported)
        );
    }

    #[test]
    fn roots_split_into_drive_and_folders() {
        assert_eq!(split_root(r"c:\"), Some(("C:".into(), vec![])));
        assert_eq!(
            split_root(r"C:\Users\Me\"),
            Some(("C:".into(), vec!["Users".into(), "Me".into()]))
        );
        assert_eq!(split_root(r"\\server\share"), None);
    }
}
