//! A zip archive written as it goes, for downloading a whole folder.
//!
//! Nothing is held back: each file's header goes out, then its bytes as
//! they are read, then a data descriptor with the CRC worked out on the
//! way. Only the central directory (a name and a few numbers per entry)
//! stays in memory until the end. Files are stored, not compressed: most
//! of what people share is photos and videos, which don't shrink, and
//! storing keeps the download as fast as the disk and the network.
//!
//! ZIP64 records are added where sizes, offsets, or the number of entries
//! don't fit the classic fields, and names are UTF-8 (flag bit 11).

use std::io::{self, Read, Write};

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const END: u32 = 0x0605_4b50;
const END64: u32 = 0x0606_4b50;
const LOCATOR64: u32 = 0x0706_4b50;
/// Names are UTF-8.
const UTF8: u16 = 1 << 11;
/// Sizes and CRC follow the data in a descriptor.
const STREAMED: u16 = 1 << 3;
/// Made on "Unix" so unpackers use the permissions given; spec 4.5.
const MADE_BY: u16 = (3 << 8) | 45;
const ZIP64_EXTRA: u16 = 0x0001;
const TIME_EXTRA: u16 = 0x5455;

struct Entry {
    name: Vec<u8>,
    dir: bool,
    crc: u32,
    size: u64,
    offset: u64,
    modified: u64,
    /// The local header announced ZIP64 sizes.
    zip64_local: bool,
}

/// Writes a zip to `out` one entry at a time.
pub struct ZipWriter<W: Write> {
    out: W,
    written: u64,
    entries: Vec<Entry>,
    /// Values at or above these need ZIP64; lowered in tests.
    max32: u64,
    max16: u64,
}

impl<W: Write> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        Self::with_limits(out, u64::from(u32::MAX), u64::from(u16::MAX))
    }

    /// Like `new`, switching to ZIP64 at smaller values, so tests can
    /// reach that path without writing gigabytes.
    pub fn with_limits(out: W, max32: u64, max16: u64) -> Self {
        Self {
            out,
            written: 0,
            entries: Vec::new(),
            max32: max32.min(u64::from(u32::MAX)),
            max16: max16.min(u64::from(u16::MAX)),
        }
    }

    /// Entries so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// A folder, named like `Photos/2024/` (the slash is added if missing).
    pub fn add_dir(&mut self, name: &str, modified: u64) -> io::Result<()> {
        let mut name = name.as_bytes().to_vec();
        if !name.ends_with(b"/") {
            name.push(b'/');
        }
        let offset = self.written;
        let header = local_header(&name, modified, false, false);
        self.put(&header)?;
        self.entries.push(Entry {
            name,
            dir: true,
            crc: 0,
            size: 0,
            offset,
            modified,
            zip64_local: false,
        });
        Ok(())
    }

    /// A file of `expected` bytes read from `data`. At most `expected`
    /// bytes are stored. A read that fails partway ends the entry where it
    /// is, so the archive stays whole; the answer is then `false`. Errors
    /// writing `out` are returned.
    pub fn add_file(
        &mut self,
        name: &str,
        modified: u64,
        expected: u64,
        data: &mut dyn Read,
    ) -> io::Result<bool> {
        let name = name.as_bytes().to_vec();
        let offset = self.written;
        let zip64_local = expected >= self.max32;
        let header = local_header(&name, modified, true, zip64_local);
        self.put(&header)?;
        let mut crc = crc32fast::Hasher::new();
        let mut size = 0u64;
        let mut whole = true;
        let mut chunk = vec![0u8; 256 * 1024];
        while size < expected {
            let want = (chunk.len() as u64).min(expected - size) as usize;
            match data.read(&mut chunk[..want]) {
                // The file shrank since it was measured.
                Ok(0) => {
                    whole = false;
                    break;
                }
                Ok(read) => {
                    crc.update(&chunk[..read]);
                    self.put(&chunk[..read])?;
                    size += read as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    whole = false;
                    break;
                }
            }
        }
        let crc = crc.finalize();
        let mut descriptor = Vec::with_capacity(24);
        put32(&mut descriptor, DESCRIPTOR);
        put32(&mut descriptor, crc);
        if zip64_local {
            put64(&mut descriptor, size);
            put64(&mut descriptor, size);
        } else {
            put32(&mut descriptor, size as u32);
            put32(&mut descriptor, size as u32);
        }
        self.put(&descriptor)?;
        self.entries.push(Entry {
            name,
            dir: false,
            crc,
            size,
            offset,
            modified,
            zip64_local,
        });
        Ok(whole)
    }

    /// The central directory and the end records. Returns `out`.
    pub fn finish(mut self) -> io::Result<W> {
        let start = self.written;
        let entries = std::mem::take(&mut self.entries);
        for entry in &entries {
            let record = self.central(entry);
            self.put(&record)?;
        }
        let size = self.written - start;
        let count = entries.len() as u64;
        let need64 = count >= self.max16 || start >= self.max32 || size >= self.max32;
        if need64 {
            let at = self.written;
            let mut record = Vec::with_capacity(56 + 20);
            put32(&mut record, END64);
            put64(&mut record, 44);
            put16(&mut record, MADE_BY);
            put16(&mut record, 45);
            put32(&mut record, 0);
            put32(&mut record, 0);
            put64(&mut record, count);
            put64(&mut record, count);
            put64(&mut record, size);
            put64(&mut record, start);
            put32(&mut record, LOCATOR64);
            put32(&mut record, 0);
            put64(&mut record, at);
            put32(&mut record, 1);
            self.put(&record)?;
        }
        let small16 = |value: u64| {
            if value >= self.max16 {
                u16::MAX
            } else {
                value as u16
            }
        };
        let small32 = |value: u64| {
            if value >= self.max32 {
                u32::MAX
            } else {
                value as u32
            }
        };
        let mut end = Vec::with_capacity(22);
        put32(&mut end, END);
        put16(&mut end, 0);
        put16(&mut end, 0);
        put16(&mut end, small16(count));
        put16(&mut end, small16(count));
        put32(&mut end, small32(size));
        put32(&mut end, small32(start));
        put16(&mut end, 0);
        self.put(&end)?;
        self.out.flush()?;
        Ok(self.out)
    }

    fn central(&self, entry: &Entry) -> Vec<u8> {
        let big_size = entry.zip64_local || entry.size >= self.max32;
        let big_offset = entry.offset >= self.max32;
        let mut zip64 = Vec::new();
        if big_size {
            put64(&mut zip64, entry.size);
            put64(&mut zip64, entry.size);
        }
        if big_offset {
            put64(&mut zip64, entry.offset);
        }
        let mut extra = Vec::new();
        if !zip64.is_empty() {
            put16(&mut extra, ZIP64_EXTRA);
            put16(&mut extra, zip64.len() as u16);
            extra.extend_from_slice(&zip64);
        }
        put16(&mut extra, TIME_EXTRA);
        put16(&mut extra, 5);
        extra.push(1);
        put32(&mut extra, unix32(entry.modified));
        let (time, date) = dos_time(entry.modified);
        let mut record = Vec::with_capacity(46 + entry.name.len() + extra.len());
        put32(&mut record, CENTRAL);
        put16(&mut record, MADE_BY);
        put16(&mut record, if zip64.is_empty() { 20 } else { 45 });
        put16(&mut record, if entry.dir { UTF8 } else { UTF8 | STREAMED });
        put16(&mut record, 0);
        put16(&mut record, time);
        put16(&mut record, date);
        put32(&mut record, entry.crc);
        let size = if big_size {
            u32::MAX
        } else {
            entry.size as u32
        };
        put32(&mut record, size);
        put32(&mut record, size);
        put16(&mut record, entry.name.len() as u16);
        put16(&mut record, extra.len() as u16);
        put16(&mut record, 0);
        put16(&mut record, 0);
        put16(&mut record, 0);
        // Unix permissions above, the DOS folder flag below.
        let attributes = if entry.dir {
            (0o40755u32 << 16) | 0x10
        } else {
            0o100644u32 << 16
        };
        put32(&mut record, attributes);
        put32(
            &mut record,
            if big_offset {
                u32::MAX
            } else {
                entry.offset as u32
            },
        );
        record.extend_from_slice(&entry.name);
        record.extend_from_slice(&extra);
        record
    }
}

fn local_header(name: &[u8], modified: u64, streamed: bool, zip64: bool) -> Vec<u8> {
    let (time, date) = dos_time(modified);
    let mut extra = Vec::new();
    if zip64 {
        // Real sizes come in the descriptor; this only says they are 64-bit.
        put16(&mut extra, ZIP64_EXTRA);
        put16(&mut extra, 16);
        put64(&mut extra, 0);
        put64(&mut extra, 0);
    }
    put16(&mut extra, TIME_EXTRA);
    put16(&mut extra, 5);
    extra.push(1);
    put32(&mut extra, unix32(modified));
    let mut header = Vec::with_capacity(30 + name.len() + extra.len());
    put32(&mut header, LOCAL);
    put16(&mut header, if zip64 { 45 } else { 20 });
    put16(&mut header, if streamed { UTF8 | STREAMED } else { UTF8 });
    put16(&mut header, 0);
    put16(&mut header, time);
    put16(&mut header, date);
    put32(&mut header, 0);
    let size = if zip64 { u32::MAX } else { 0 };
    put32(&mut header, size);
    put32(&mut header, size);
    put16(&mut header, name.len() as u16);
    put16(&mut header, extra.len() as u16);
    header.extend_from_slice(name);
    header.extend_from_slice(&extra);
    header
}

fn unix32(seconds: u64) -> u32 {
    seconds.min(u64::from(u32::MAX)) as u32
}

/// MS-DOS time and date (UTC; the extended timestamp has the exact time).
fn dos_time(seconds: u64) -> (u16, u16) {
    let (year, month, day) = super::civil(seconds / 86_400);
    if year < 1980 {
        return (0, (1 << 5) | 1);
    }
    let year = (year - 1980).min(127) as u16;
    let rest = seconds % 86_400;
    let time =
        ((rest / 3600) as u16) << 11 | ((rest / 60 % 60) as u16) << 5 | (rest % 60 / 2) as u16;
    let date = year << 9 | (month as u16) << 5 | day as u16;
    (time, date)
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A small zip reader for tests: follows the end records and central
/// directory, then checks every local header, data, and descriptor.
#[cfg(test)]
pub(crate) mod read {
    pub struct Read {
        pub name: String,
        pub dir: bool,
        pub data: Vec<u8>,
        pub modified: u64,
    }

    fn u16_at(data: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(data[at..at + 2].try_into().unwrap())
    }

    fn u32_at(data: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
    }

    fn u64_at(data: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
    }

    /// Every entry, in order; panics on anything malformed.
    pub fn entries(zip: &[u8]) -> Vec<Read> {
        let end = zip
            .windows(4)
            .rposition(|window| window == super::END.to_le_bytes())
            .expect("an end record");
        assert_eq!(end + 22, zip.len(), "nothing after the end record");
        let mut count = u64::from(u16_at(zip, end + 10));
        let mut size = u64::from(u32_at(zip, end + 12));
        let mut start = u64::from(u32_at(zip, end + 16));
        if count == 0xFFFF || size == 0xFFFF_FFFF || start == 0xFFFF_FFFF {
            let locator = end - 20;
            assert_eq!(u32_at(zip, locator), super::LOCATOR64);
            let record = u64_at(zip, locator + 8) as usize;
            assert_eq!(u32_at(zip, record), super::END64);
            count = u64_at(zip, record + 32);
            size = u64_at(zip, record + 40);
            start = u64_at(zip, record + 48);
        }
        let mut at = start as usize;
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(zip, at), super::CENTRAL, "central header");
            let flags = u16_at(zip, at + 8);
            assert_ne!(flags & super::UTF8, 0, "UTF-8 names");
            assert_eq!(u16_at(zip, at + 10), 0, "stored");
            let crc = u32_at(zip, at + 16);
            let mut length = u64::from(u32_at(zip, at + 24));
            let name_len = usize::from(u16_at(zip, at + 28));
            let extra_len = usize::from(u16_at(zip, at + 30));
            let comment_len = usize::from(u16_at(zip, at + 32));
            let mut offset = u64::from(u32_at(zip, at + 42));
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let extra = &zip[at + 46 + name_len..at + 46 + name_len + extra_len];
            let mut modified = 0;
            let mut field = 0;
            while field < extra.len() {
                let id = u16_at(extra, field);
                let len = usize::from(u16_at(extra, field + 2));
                let body = &extra[field + 4..field + 4 + len];
                if id == super::ZIP64_EXTRA {
                    let mut next = 0;
                    if length == 0xFFFF_FFFF {
                        length = u64_at(body, 0);
                        assert_eq!(u64_at(body, 8), length);
                        next = 16;
                    }
                    if offset == 0xFFFF_FFFF {
                        offset = u64_at(body, next);
                    }
                }
                if id == super::TIME_EXTRA {
                    modified = u64::from(u32_at(body, 1));
                }
                field += 4 + len;
            }
            at += 46 + name_len + extra_len + comment_len;

            let local = offset as usize;
            assert_eq!(u32_at(zip, local), super::LOCAL, "local header of {name}");
            assert_eq!(u16_at(zip, local + 6), flags);
            let local_name_len = usize::from(u16_at(zip, local + 26));
            let local_extra_len = usize::from(u16_at(zip, local + 28));
            assert_eq!(
                &zip[local + 30..local + 30 + local_name_len],
                name.as_bytes()
            );
            let local_extra = &zip[local + 30 + local_name_len..][..local_extra_len];
            let zip64_local =
                local_extra.len() >= 2 && u16_at(local_extra, 0) == super::ZIP64_EXTRA;
            let data_at = local + 30 + local_name_len + local_extra_len;
            let data = zip[data_at..data_at + length as usize].to_vec();
            assert_eq!(crc32fast::hash(&data), crc, "CRC of {name}");
            if flags & super::STREAMED != 0 {
                let descriptor = data_at + length as usize;
                assert_eq!(u32_at(zip, descriptor), super::DESCRIPTOR);
                assert_eq!(u32_at(zip, descriptor + 4), crc);
                if zip64_local {
                    assert_eq!(u64_at(zip, descriptor + 8), length);
                    assert_eq!(u64_at(zip, descriptor + 16), length);
                } else {
                    assert_eq!(u64::from(u32_at(zip, descriptor + 8)), length);
                    assert_eq!(u64::from(u32_at(zip, descriptor + 12)), length);
                }
            }
            out.push(Read {
                dir: name.ends_with('/'),
                name,
                data,
                modified,
            });
        }
        assert_eq!(at as u64, start + size, "central directory size");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Failing(usize);

    impl Read for Failing {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("disk gone"));
            }
            let n = self.0.min(buffer.len()).min(3);
            buffer[..n].fill(b'z');
            self.0 -= n;
            Ok(n)
        }
    }

    fn sample(max32: u64, max16: u64) -> Vec<u8> {
        let mut zip = ZipWriter::with_limits(Vec::new(), max32, max16);
        zip.add_dir("Fotos", 1_700_000_000).unwrap();
        let text = b"hello, zip".repeat(50);
        assert!(zip
            .add_file(
                "Fotos/a.txt",
                1_700_000_100,
                text.len() as u64,
                &mut &text[..]
            )
            .unwrap());
        assert!(zip
            .add_file("Fotos/Ünïcödé 日本.txt", 0, 3, &mut &b"abc"[..])
            .unwrap());
        assert!(zip.add_file("Fotos/empty", 5, 0, &mut io::empty()).unwrap());
        // Fails after 5 bytes: kept as what was read.
        assert!(!zip
            .add_file("Fotos/broken.bin", 7, 100, &mut Failing(5))
            .unwrap());
        // Shrank: 2 of the 10 bytes promised.
        assert!(!zip.add_file("Fotos/short", 7, 10, &mut &b"ab"[..]).unwrap());
        // Grew: only what was measured is stored.
        assert!(zip
            .add_file("Fotos/long", 7, 2, &mut &b"abcdef"[..])
            .unwrap());
        zip.finish().unwrap()
    }

    fn check(zip: &[u8]) {
        let entries = read::entries(zip);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Fotos/",
                "Fotos/a.txt",
                "Fotos/Ünïcödé 日本.txt",
                "Fotos/empty",
                "Fotos/broken.bin",
                "Fotos/short",
                "Fotos/long"
            ]
        );
        assert!(entries[0].dir && entries[0].data.is_empty());
        assert_eq!(entries[1].data, b"hello, zip".repeat(50));
        assert_eq!(entries[1].modified, 1_700_000_100);
        assert_eq!(entries[2].data, b"abc");
        assert_eq!(entries[4].data, b"zzzzz");
        assert_eq!(entries[5].data, b"ab");
        assert_eq!(entries[6].data, b"ab");
    }

    #[test]
    fn a_zip_reads_back() {
        let zip = sample(u64::MAX, u64::MAX);
        check(&zip);
        // No ZIP64 records when nothing needs them.
        assert!(!zip.windows(4).any(|window| window == END64.to_le_bytes()));
    }

    #[test]
    fn zip64_records_when_values_do_not_fit() {
        // Every size and offset past 100 bytes and more than 3 entries count
        // as too big, so all the ZIP64 fields are written and read.
        let zip = sample(100, 3);
        check(&zip);
        assert!(zip.windows(4).any(|window| window == END64.to_le_bytes()));
        let end = &zip[zip.len() - 22..];
        assert_eq!(&end[8..10], &[0xFF, 0xFF]);
        assert_eq!(&end[16..20], &[0xFF; 4]);
    }

    #[test]
    fn more_entries_than_the_classic_count_holds() {
        let mut zip = ZipWriter::new(Vec::new());
        for index in 0..70_000u32 {
            zip.add_file(&format!("f{index}"), 0, 1, &mut &[index as u8][..])
                .unwrap();
        }
        assert_eq!(zip.len(), 70_000);
        let bytes = zip.finish().unwrap();
        let entries = read::entries(&bytes);
        assert_eq!(entries.len(), 70_000);
        assert_eq!(entries[69_999].name, "f69999");
        assert_eq!(entries[69_999].data, [(69_999u32 as u8)]);
    }

    #[test]
    fn dos_times() {
        // 2023-11-14 22:13:20 UTC.
        let (time, date) = dos_time(1_700_000_000);
        assert_eq!(date >> 9, 2023 - 1980);
        assert_eq!((date >> 5) & 0xF, 11);
        assert_eq!(date & 0x1F, 14);
        assert_eq!(time >> 11, 22);
        assert_eq!((time >> 5) & 0x3F, 13);
        assert_eq!(time & 0x1F, 10);
        assert_eq!(dos_time(0), (0, 33));
    }
}
