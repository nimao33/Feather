//! The NTFS change journal (USN journal): every create, delete, rename,
//! and write on a volume is logged there with the file's reference number.
//!
//! Parsing records and turning them into [`Change`]s is plain byte work and
//! runs anywhere, so it is tested from fixtures. Reading the journal itself
//! (`query`, `read_changes`) opens the raw volume and needs admin rights;
//! the app does it through its elevated helper.

use std::collections::HashMap;

use crate::live::{entry_flags, Change, EntryState, FILE_ATTRIBUTE_DIRECTORY};

pub const REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
pub const REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
pub const REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
pub const REASON_CLOSE: u32 = 0x8000_0000;

/// The reasons that can change what the index shows: names, places,
/// sizes, times, and attributes. Security, extended attributes, object
/// ids and the like are left to the kernel to filter out.
pub const REASON_MASK: u32 = REASON_DATA_OVERWRITE
    | REASON_DATA_EXTEND
    | REASON_DATA_TRUNCATION
    | REASON_FILE_CREATE
    | REASON_FILE_DELETE
    | REASON_RENAME_OLD_NAME
    | REASON_RENAME_NEW_NAME
    | REASON_BASIC_INFO_CHANGE
    | REASON_HARD_LINK_CHANGE
    | REASON_REPARSE_POINT_CHANGE;

/// Records below this are NTFS's own metadata files.
const FIRST_USER_RECORD: u64 = 16;
const RECORD_MASK: u64 = 0x0000_ffff_ffff_ffff;

/// One journal record, version 2 or 3.
#[derive(Clone, Debug, PartialEq)]
pub struct UsnRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    /// When the change happened, as a FILETIME.
    pub timestamp: u64,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
}

impl UsnRecord {
    /// A record written just before a rename, naming the old place.
    fn is_old_name(&self) -> bool {
        self.reason & REASON_RENAME_OLD_NAME != 0 && self.reason & REASON_RENAME_NEW_NAME == 0
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// Parses one record at the start of `data`; `None` for a record that is
/// damaged or of a version without a name (v4 range records).
fn parse_record(data: &[u8]) -> Option<UsnRecord> {
    let major = u16_at(data, 4)?;
    // (frn, parent, usn, time, reason, attributes, name length, offset)
    let (frn, parent, fields, name_at) = match major {
        2 => (u64_at(data, 8)?, u64_at(data, 16)?, 24, 56),
        3 => {
            // 128-bit ids; NTFS leaves the high half zero. Anything else
            // (ReFS) cannot be matched to an NTFS index.
            if u64_at(data, 16)? != 0 || u64_at(data, 32)? != 0 {
                return None;
            }
            (u64_at(data, 8)?, u64_at(data, 24)?, 40, 72)
        }
        _ => return None,
    };
    let usn = u64_at(data, fields)? as i64;
    let timestamp = u64_at(data, fields + 8)?;
    let reason = u32_at(data, fields + 16)?;
    let attributes = u32_at(data, fields + 28)?;
    let name_len = u16_at(data, name_at)? as usize;
    let name_offset = u16_at(data, name_at + 2)? as usize;
    let bytes = data.get(name_offset..name_offset + name_len)?;
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    Some(UsnRecord {
        frn,
        parent_frn: parent,
        usn,
        timestamp,
        reason,
        attributes,
        name: String::from_utf16_lossy(&units),
    })
}

/// Parses the records packed one after another in `data`, as the journal
/// returns them. Stops at the first record whose length is impossible.
pub fn parse_records(data: &[u8]) -> Vec<UsnRecord> {
    let mut records = Vec::new();
    let mut at = 0usize;
    while at + 8 <= data.len() {
        let Some(length) = u32_at(data, at) else {
            break;
        };
        let length = length as usize;
        if length < 8 || at + length > data.len() {
            break;
        }
        if let Some(record) = parse_record(&data[at..at + length]) {
            records.push(record);
        }
        at += length;
    }
    records
}

/// Splits what `FSCTL_READ_USN_JOURNAL` returned: the next USN to read
/// from, then the records.
pub fn parse_read_output(data: &[u8]) -> Option<(i64, Vec<UsnRecord>)> {
    let next = u64_at(data, 0)? as i64;
    Some((next, parse_records(&data[8..])))
}

/// What the file system says about a file now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileFacts {
    pub attributes: u32,
    pub size: u64,
    pub mtime: u64,
}

/// The answer to "what is this file now".
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lookup {
    Found(FileFacts),
    /// No file has this reference any more.
    Gone,
    /// It could not be asked (access denied, for instance).
    Unknown,
}

/// Turns journal records into one change per file, in the order files
/// first appear. A file's latest name and folder come from its newest
/// record that names its new place; its size, time, and attributes from
/// `lookup`, asked once per file, since records do not carry sizes.
pub fn collect_changes(
    records: &[UsnRecord],
    mut lookup: impl FnMut(u64) -> Lookup,
) -> Vec<Change> {
    struct Seen {
        named: Option<usize>,
        deleted: bool,
        created: bool,
    }
    let mut order: Vec<u64> = Vec::new();
    let mut seen: HashMap<u64, Seen> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if record.frn & RECORD_MASK < FIRST_USER_RECORD {
            continue;
        }
        let entry = seen.entry(record.frn).or_insert_with(|| {
            order.push(record.frn);
            Seen {
                named: None,
                deleted: false,
                created: false,
            }
        });
        entry.created |= record.reason & REASON_FILE_CREATE != 0;
        if record.reason & REASON_FILE_DELETE != 0 {
            entry.deleted = true;
        } else if !record.is_old_name()
            // A link added or removed names that link, which may not be
            // the one the index shows.
            && record.reason & !(REASON_HARD_LINK_CHANGE | REASON_CLOSE) != 0
        {
            entry.named = Some(index);
        }
    }
    let mut changes = Vec::with_capacity(order.len());
    for frn in order {
        let entry = &seen[&frn];
        if entry.deleted {
            changes.push(Change::Remove(frn));
            continue;
        }
        let Some(named) = entry.named else {
            continue;
        };
        let record = &records[named];
        let state = |attributes: u32, size: Option<u64>, mtime: u64| EntryState {
            frn,
            parent_frn: record.parent_frn,
            name: record.name.clone(),
            size,
            mtime,
            flags: entry_flags(attributes & FILE_ATTRIBUTE_DIRECTORY != 0, attributes),
            created: entry.created,
        };
        changes.push(match lookup(frn) {
            Lookup::Found(facts) => {
                let is_dir = facts.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                Change::Upsert(state(
                    facts.attributes,
                    Some(if is_dir { 0 } else { facts.size }),
                    facts.mtime,
                ))
            }
            Lookup::Gone => Change::Remove(frn),
            Lookup::Unknown => Change::Upsert(state(record.attributes, None, record.timestamp)),
        });
    }
    changes
}

/// A volume's journal as `FSCTL_QUERY_USN_JOURNAL` describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
}

impl JournalInfo {
    pub fn cursor(&self) -> crate::live::JournalCursor {
        crate::live::JournalCursor {
            journal_id: self.journal_id,
            next_usn: self.next_usn,
        }
    }

    /// Whether reading from `cursor` would see every change since it.
    pub fn covers(&self, cursor: crate::live::JournalCursor) -> bool {
        cursor.journal_id == self.journal_id
            && cursor.next_usn >= self.first_usn.max(self.lowest_valid_usn)
            && cursor.next_usn <= self.next_usn
    }
}

/// The error for a journal that no longer holds every change since the
/// cursor: deleted, recreated, or wrapped around. The index has to be
/// read again.
pub fn journal_gone(detail: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("the change journal no longer covers the index: {detail}"),
    )
}

#[cfg(windows)]
pub use imp::{list_dir, query, read_changes, DirItem};

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        FileStandardInfo, GetFileInformationByHandleEx, OpenFileById, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_ID_DESCRIPTOR, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{collect_changes, journal_gone, parse_read_output, FileFacts, JournalInfo, Lookup};
    use crate::live::{Batch, JournalCursor};

    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ERROR_HANDLE_EOF: i32 = 38;
    const ERROR_NO_MORE_FILES: i32 = 18;
    const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
    const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
    const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
    const READ_BUFFER: usize = 256 * 1024;

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `C:`, `c:\`, or `C:\Users` -> `C:`.
    fn drive_of(root: &str) -> io::Result<String> {
        let bytes = root.as_bytes();
        if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "only drive letters have a change journal",
            ));
        }
        Ok(root[..2].to_ascii_uppercase())
    }

    fn open_volume(drive: &str) -> io::Result<Handle> {
        let path = wide(&format!(r"\\.\{}", drive_of(drive)?));
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Handle(handle))
    }

    fn journal_error(error: io::Error) -> io::Error {
        match error.raw_os_error() {
            Some(ERROR_JOURNAL_NOT_ACTIVE) => io::Error::new(
                io::ErrorKind::Unsupported,
                "the drive has no change journal",
            ),
            Some(ERROR_JOURNAL_DELETE_IN_PROGRESS) | Some(ERROR_JOURNAL_ENTRY_DELETED) => {
                journal_gone(&error.to_string())
            }
            _ => error,
        }
    }

    fn query_handle(volume: &Handle) -> io::Result<JournalInfo> {
        let mut out = [0u8; 80];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                volume.0,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(journal_error(io::Error::last_os_error()));
        }
        let field = |at: usize| u64::from_le_bytes(out[at..at + 8].try_into().unwrap_or([0; 8]));
        Ok(JournalInfo {
            journal_id: field(0),
            first_usn: field(8) as i64,
            next_usn: field(16) as i64,
            lowest_valid_usn: field(24) as i64,
        })
    }

    /// Describes the journal of the drive holding `drive`. Needs admin.
    pub fn query(drive: &str) -> io::Result<JournalInfo> {
        query_handle(&open_volume(drive)?)
    }

    /// What a file is now, asked by reference through the volume.
    fn facts(volume: &Handle, frn: u64) -> Lookup {
        let mut descriptor: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
        descriptor.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
        descriptor.Type = 0; // FileIdType
        descriptor.Anonymous.FileId = frn as i64;
        let handle = unsafe {
            OpenFileById(
                volume.0,
                &descriptor,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_INVALID_PARAMETER) => Lookup::Gone,
                _ => Lookup::Unknown,
            };
        }
        let handle = Handle(handle);
        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) != 0
                && GetFileInformationByHandleEx(
                    handle.0,
                    FileStandardInfo,
                    (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                    std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
                ) != 0
        };
        if !ok {
            return Lookup::Unknown;
        }
        Lookup::Found(FileFacts {
            attributes: basic.FileAttributes,
            size: standard.EndOfFile.max(0) as u64,
            mtime: basic.LastWriteTime.max(0) as u64,
        })
    }

    /// Reads what changed on `drive` since `cursor`, at most about
    /// `max_records` journal records, and asks each changed file for its
    /// size and time. Fails with `NotFound` (see [`journal_gone`]) when
    /// the journal no longer reaches back to the cursor. Needs admin.
    pub fn read_changes(
        drive: &str,
        cursor: JournalCursor,
        max_records: usize,
    ) -> io::Result<Batch> {
        let volume = open_volume(drive)?;
        let info = query_handle(&volume)?;
        if !info.covers(cursor) {
            return Err(journal_gone(&format!(
                "journal {:x} from {} to {}, wanted {:x} at {}",
                info.journal_id, info.first_usn, info.next_usn, cursor.journal_id, cursor.next_usn
            )));
        }
        let mut records = Vec::new();
        let mut next = cursor.next_usn;
        let mut buffer = vec![0u8; READ_BUFFER];
        while next < info.next_usn && records.len() < max_records {
            // READ_USN_JOURNAL_DATA_V0: start, reasons, only on close,
            // timeout, bytes to wait for, journal id.
            let mut input = [0u8; 40];
            input[0..8].copy_from_slice(&next.to_le_bytes());
            input[8..12].copy_from_slice(&super::REASON_MASK.to_le_bytes());
            input[32..40].copy_from_slice(&info.journal_id.to_le_bytes());
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    volume.0,
                    FSCTL_READ_USN_JOURNAL,
                    input.as_ptr().cast(),
                    input.len() as u32,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_HANDLE_EOF) {
                    break;
                }
                return Err(journal_error(error));
            }
            let Some((after, mut found)) = parse_read_output(&buffer[..returned as usize]) else {
                break;
            };
            records.append(&mut found);
            if after <= next {
                break;
            }
            next = after;
        }
        let changes = collect_changes(&records, |frn| facts(&volume, frn));
        Ok(Batch {
            cursor: JournalCursor {
                journal_id: info.journal_id,
                next_usn: next,
            },
            changes,
            more: next < info.next_usn,
        })
    }

    /// One item of a folder listing, with its file reference number.
    #[derive(Clone, Debug, PartialEq)]
    pub struct DirItem {
        pub frn: u64,
        pub name: String,
        pub attributes: u32,
        pub size: u64,
        pub mtime: u64,
    }

    /// Lists a folder with each item's file reference number, the way the
    /// NTFS index knows it. No admin rights needed.
    pub fn list_dir(path: &Path) -> io::Result<Vec<DirItem>> {
        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(handle);
        // u64 storage keeps the records 8-byte aligned.
        let mut buffer = vec![0u64; 64 * 1024 / 8];
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let class = if first {
                FileIdBothDirectoryRestartInfo
            } else {
                FileIdBothDirectoryInfo
            };
            first = false;
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    class,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                    break;
                }
                return Err(error);
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8)
            };
            let mut at = 0usize;
            loop {
                if at + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > bytes.len() {
                    break;
                }
                let info = unsafe { &*(bytes.as_ptr().add(at) as *const FILE_ID_BOTH_DIR_INFO) };
                let name_at = at + std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
                let name_len = info.FileNameLength as usize;
                if let Some(name_bytes) = bytes.get(name_at..name_at + name_len) {
                    let units: Vec<u16> = name_bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_le_bytes(*pair))
                        .collect();
                    let name = String::from_utf16_lossy(&units);
                    if name != "." && name != ".." {
                        items.push(DirItem {
                            frn: info.FileId as u64,
                            name,
                            attributes: info.FileAttributes,
                            size: info.EndOfFile.max(0) as u64,
                            mtime: info.LastWriteTime.max(0) as u64,
                        });
                    }
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                at += info.NextEntryOffset as usize;
            }
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A USN_RECORD_V2 as the journal lays it out, padded to 8 bytes.
    fn v2(frn: u64, parent: u64, usn: i64, reason: u32, attributes: u32, name: &str) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let length = (60 + name.len()).div_ceil(8) * 8;
        let mut record = vec![0u8; length];
        record[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        record[4..6].copy_from_slice(&2u16.to_le_bytes());
        record[8..16].copy_from_slice(&frn.to_le_bytes());
        record[16..24].copy_from_slice(&parent.to_le_bytes());
        record[24..32].copy_from_slice(&usn.to_le_bytes());
        record[32..40].copy_from_slice(&(usn as u64 * 10).to_le_bytes());
        record[40..44].copy_from_slice(&reason.to_le_bytes());
        record[52..56].copy_from_slice(&attributes.to_le_bytes());
        record[56..58].copy_from_slice(&(name.len() as u16).to_le_bytes());
        record[58..60].copy_from_slice(&60u16.to_le_bytes());
        record[60..60 + name.len()].copy_from_slice(&name);
        record
    }

    /// A USN_RECORD_V3: 128-bit references.
    fn v3(frn: u128, parent: u128, usn: i64, reason: u32, name: &str) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let length = (76 + name.len()).div_ceil(8) * 8;
        let mut record = vec![0u8; length];
        record[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        record[4..6].copy_from_slice(&3u16.to_le_bytes());
        record[8..24].copy_from_slice(&frn.to_le_bytes());
        record[24..40].copy_from_slice(&parent.to_le_bytes());
        record[40..48].copy_from_slice(&usn.to_le_bytes());
        record[48..56].copy_from_slice(&77u64.to_le_bytes());
        record[56..60].copy_from_slice(&reason.to_le_bytes());
        record[68..72].copy_from_slice(&0x20u32.to_le_bytes());
        record[72..74].copy_from_slice(&(name.len() as u16).to_le_bytes());
        record[74..76].copy_from_slice(&76u16.to_le_bytes());
        record[76..76 + name.len()].copy_from_slice(&name);
        record
    }

    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
    const ROOT: u64 = 5 | (5 << 48);
    const A: u64 = 40 | (2 << 48);
    const B: u64 = 41 | (1 << 48);
    const D: u64 = 42 | (3 << 48);

    fn found(attributes: u32, size: u64) -> Lookup {
        Lookup::Found(FileFacts {
            attributes,
            size,
            mtime: 99,
        })
    }

    #[test]
    fn parses_version_2_and_3_records() {
        let mut data = v2(A, ROOT, 100, REASON_FILE_CREATE, 0x20, "naïve.txt");
        data.extend(v3(
            u128::from(B),
            u128::from(ROOT),
            200,
            REASON_DATA_EXTEND,
            "b.bin",
        ));
        // A v3 record with a real 128-bit id (ReFS) is skipped; a v4 range
        // record has no name and is skipped too.
        data.extend(v3(
            1 << 70,
            u128::from(ROOT),
            300,
            REASON_DATA_EXTEND,
            "refs",
        ));
        let mut v4 = vec![0u8; 80];
        v4[0..4].copy_from_slice(&80u32.to_le_bytes());
        v4[4..6].copy_from_slice(&4u16.to_le_bytes());
        data.extend(v4);
        let records = parse_records(&data);
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0],
            UsnRecord {
                frn: A,
                parent_frn: ROOT,
                usn: 100,
                timestamp: 1000,
                reason: REASON_FILE_CREATE,
                attributes: 0x20,
                name: "naïve.txt".into(),
            }
        );
        assert_eq!(records[1].frn, B);
        assert_eq!(records[1].usn, 200);
        assert_eq!(records[1].timestamp, 77);
        assert_eq!(records[1].name, "b.bin");

        // The read output starts with the next USN.
        let mut output = 4242u64.to_le_bytes().to_vec();
        output.extend(v2(A, ROOT, 1, REASON_CLOSE, 0, "x"));
        let (next, records) = parse_read_output(&output).unwrap();
        assert_eq!(next, 4242);
        assert_eq!(records.len(), 1);
        assert!(parse_read_output(&[1, 2]).is_none());
    }

    #[test]
    fn damaged_lengths_stop_parsing() {
        let mut data = v2(A, ROOT, 100, REASON_FILE_CREATE, 0, "ok");
        let mut bad = v2(B, ROOT, 101, REASON_FILE_CREATE, 0, "bad");
        bad[0..4].copy_from_slice(&5000u32.to_le_bytes());
        data.extend(bad);
        assert_eq!(parse_records(&data).len(), 1);
        let mut zero = v2(A, ROOT, 100, 0, 0, "zero");
        zero[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_records(&zero).is_empty());
        // A name pointing past the record is refused.
        let mut wild = v2(A, ROOT, 100, 0, 0, "wild");
        wild[58..60].copy_from_slice(&500u16.to_le_bytes());
        assert!(parse_records(&wild).is_empty());
    }

    #[test]
    fn records_become_one_change_per_file() {
        let records = parse_records(
            &[
                // A is created, written, closed.
                v2(A, ROOT, 1, REASON_FILE_CREATE, 0x20, "new.txt"),
                v2(
                    A,
                    ROOT,
                    2,
                    REASON_FILE_CREATE | REASON_DATA_EXTEND,
                    0x20,
                    "new.txt",
                ),
                // B is renamed and moved into D.
                v2(B, ROOT, 3, REASON_RENAME_OLD_NAME, 0x20, "old name.txt"),
                v2(B, D, 4, REASON_RENAME_NEW_NAME, 0x20, "new name.txt"),
                v2(
                    B,
                    D,
                    5,
                    REASON_RENAME_OLD_NAME | REASON_RENAME_NEW_NAME | REASON_CLOSE,
                    0x20,
                    "new name.txt",
                ),
                // D is a folder whose times changed.
                v2(D, ROOT, 6, REASON_BASIC_INFO_CHANGE, DIR, "folder"),
                v2(
                    A,
                    ROOT,
                    7,
                    REASON_CLOSE | REASON_DATA_EXTEND | REASON_FILE_CREATE,
                    0x20,
                    "new.txt",
                ),
                // NTFS's own files are not indexed.
                v2(11 | (11 << 48), ROOT, 8, REASON_DATA_EXTEND, 0, "$Extend"),
            ]
            .concat(),
        );
        let mut asked = Vec::new();
        let changes = collect_changes(&records, |frn| {
            asked.push(frn);
            if frn == D {
                found(DIR | 0x2, 4096)
            } else {
                found(0x20, 123)
            }
        });
        assert_eq!(asked, vec![A, B, D], "each file asked once");
        assert_eq!(
            changes,
            vec![
                Change::Upsert(EntryState {
                    frn: A,
                    parent_frn: ROOT,
                    name: "new.txt".into(),
                    size: Some(123),
                    mtime: 99,
                    flags: 0,
                    created: true,
                }),
                Change::Upsert(EntryState {
                    frn: B,
                    parent_frn: D,
                    name: "new name.txt".into(),
                    size: Some(123),
                    mtime: 99,
                    flags: 0,
                    created: false,
                }),
                Change::Upsert(EntryState {
                    frn: D,
                    parent_frn: ROOT,
                    name: "folder".into(),
                    size: Some(0),
                    mtime: 99,
                    flags: 1 | 2,
                    created: false,
                }),
            ]
        );
    }

    #[test]
    fn deletes_win_and_unknown_files_keep_what_the_record_says() {
        let records = parse_records(
            &[
                v2(A, ROOT, 1, REASON_FILE_CREATE, 0x20, "temp.tmp"),
                v2(
                    A,
                    ROOT,
                    2,
                    REASON_FILE_DELETE | REASON_CLOSE,
                    0x20,
                    "temp.tmp",
                ),
                v2(B, ROOT, 3, REASON_DATA_OVERWRITE, 0x2, "locked.db"),
                v2(D, ROOT, 4, REASON_DATA_EXTEND, 0x20, "vanished.txt"),
                // Only the old name so far: the new one comes next time.
                v2(
                    42 | (9 << 48),
                    ROOT,
                    5,
                    REASON_RENAME_OLD_NAME,
                    0x20,
                    "half.txt",
                ),
                // Only a link change: which name it is cannot be told.
                v2(
                    43 | (9 << 48),
                    ROOT,
                    6,
                    REASON_HARD_LINK_CHANGE,
                    0x20,
                    "link.txt",
                ),
            ]
            .concat(),
        );
        let changes = collect_changes(&records, |frn| match frn {
            B => Lookup::Unknown,
            D => Lookup::Gone,
            _ => panic!("deleted and unnamed files are not asked about"),
        });
        assert_eq!(
            changes,
            vec![
                Change::Remove(A),
                Change::Upsert(EntryState {
                    frn: B,
                    parent_frn: ROOT,
                    name: "locked.db".into(),
                    size: None,
                    mtime: 30,
                    flags: 2,
                    created: false,
                }),
                Change::Remove(D),
            ]
        );
    }

    #[test]
    fn a_journal_covers_only_its_own_range() {
        let info = JournalInfo {
            journal_id: 7,
            first_usn: 1000,
            next_usn: 5000,
            lowest_valid_usn: 900,
        };
        let at = |journal_id, next_usn| crate::live::JournalCursor {
            journal_id,
            next_usn,
        };
        assert!(info.covers(at(7, 1000)));
        assert!(info.covers(at(7, 5000)));
        assert!(!info.covers(at(7, 999)), "wrapped around");
        assert!(!info.covers(at(8, 2000)), "recreated");
        assert!(!info.covers(at(7, 6000)), "from the future");
        assert_eq!(info.cursor(), at(7, 5000));
        assert_eq!(journal_gone("x").kind(), std::io::ErrorKind::NotFound);
    }

    /// Lists a real folder with file references; no admin needed.
    #[cfg(windows)]
    #[test]
    fn folders_list_with_their_references() {
        let dir = std::env::temp_dir().join(format!("feather-usn-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("one.txt"), b"hello").unwrap();
        let mut items = list_dir(&dir).expect("list");
        items.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "inner");
        assert!(items[0].attributes & FILE_ATTRIBUTE_DIRECTORY != 0);
        assert_eq!(items[1].name, "one.txt");
        assert_eq!(items[1].size, 5);
        assert!(items[1].mtime > 0);
        assert!(items[0].frn != 0 && items[0].frn != items[1].frn);
        // The same reference again on a second listing.
        let again = list_dir(&dir).expect("list again");
        assert!(again.iter().any(|item| item.frn == items[1].frn));
        assert!(list_dir(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without admin rights the journal cannot be opened; that must say so
    /// plainly so the app falls back to its helper.
    #[cfg(windows)]
    #[test]
    fn reading_the_journal_needs_admin_or_says_why_not() {
        if let Err(error) = query("C:") {
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ));
        }
        assert_eq!(
            query(r"\\server\share").err().map(|error| error.kind()),
            Some(std::io::ErrorKind::Unsupported)
        );
    }
}
