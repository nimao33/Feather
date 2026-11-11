//! On-disk format constants and the double-buffered manifest.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const MAGIC: [u8; 8] = *b"FTHRIDX1";
pub const FORMAT_VERSION: u32 = 1;
pub const SLOT_SIZE: usize = 4096;
pub const META_SIZE: usize = 48;
pub const FRN_SIZE: usize = 12;
pub const CHECK_STRIDE: usize = 256;

#[derive(Clone, Debug)]
pub struct Manifest {
    pub generation: u64,
    pub entry_count: u32,
    pub meta_len: u64,
    pub search_len: u64,
    pub display_len: u64,
    pub frn_len: u64,
    pub check_len: u64,
    pub seq: u64,
}

impl Manifest {
    pub fn meta_name(&self) -> String {
        format!("base-{:016}.meta", self.generation)
    }

    pub fn search_name(&self) -> String {
        format!("base-{:016}.search", self.generation)
    }

    pub fn display_name(&self) -> String {
        format!("base-{:016}.display", self.generation)
    }

    pub fn frn_name(&self) -> String {
        format!("base-{:016}.frn", self.generation)
    }

    pub fn check_name(&self) -> String {
        format!("base-{:016}.check", self.generation)
    }

    pub fn check_count(&self) -> usize {
        (self.entry_count as usize).div_ceil(CHECK_STRIDE)
    }

    pub fn validate_lengths(&self) -> bool {
        self.entry_count > 0
            && self.meta_len == self.entry_count as u64 * META_SIZE as u64
            && self.frn_len == self.entry_count as u64 * FRN_SIZE as u64
            && self.check_len == self.check_count() as u64 * 8
    }
}

fn encode_slot(manifest: &Manifest, seq: u64) -> [u8; SLOT_SIZE] {
    let mut slot = [0u8; SLOT_SIZE];
    slot[0..8].copy_from_slice(&MAGIC);
    slot[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    slot[16..24].copy_from_slice(&manifest.generation.to_le_bytes());
    slot[24..28].copy_from_slice(&manifest.entry_count.to_le_bytes());
    slot[32..40].copy_from_slice(&manifest.meta_len.to_le_bytes());
    slot[40..48].copy_from_slice(&manifest.search_len.to_le_bytes());
    slot[48..56].copy_from_slice(&manifest.display_len.to_le_bytes());
    slot[56..64].copy_from_slice(&manifest.frn_len.to_le_bytes());
    slot[64..72].copy_from_slice(&manifest.check_len.to_le_bytes());
    slot[72..80].copy_from_slice(&seq.to_le_bytes());
    let crc = crc32fast::hash(&slot[0..80]);
    slot[80..84].copy_from_slice(&crc.to_le_bytes());
    slot
}

fn decode_slot(slot: &[u8]) -> Option<Manifest> {
    if slot.len() < 84 || slot[0..8] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(slot[8..12].try_into().ok()?);
    if version != FORMAT_VERSION {
        return None;
    }
    let stored_crc = u32::from_le_bytes(slot[80..84].try_into().ok()?);
    if stored_crc != crc32fast::hash(&slot[0..80]) {
        return None;
    }
    Some(Manifest {
        generation: u64::from_le_bytes(slot[16..24].try_into().ok()?),
        entry_count: u32::from_le_bytes(slot[24..28].try_into().ok()?),
        meta_len: u64::from_le_bytes(slot[32..40].try_into().ok()?),
        search_len: u64::from_le_bytes(slot[40..48].try_into().ok()?),
        display_len: u64::from_le_bytes(slot[48..56].try_into().ok()?),
        frn_len: u64::from_le_bytes(slot[56..64].try_into().ok()?),
        check_len: u64::from_le_bytes(slot[64..72].try_into().ok()?),
        seq: u64::from_le_bytes(slot[72..80].try_into().ok()?),
    })
}

pub fn read_manifest(dir: &Path) -> io::Result<Manifest> {
    let path = dir.join("manifest.bin");
    let mut file = OpenOptions::new().read(true).open(&path)?;
    let mut data = Vec::with_capacity(SLOT_SIZE * 2);
    file.read_to_end(&mut data)?;
    let mut best: Option<Manifest> = None;
    for index in 0..2 {
        let start = index * SLOT_SIZE;
        if data.len() < start + 84 {
            continue;
        }
        if let Some(manifest) = decode_slot(&data[start..start + SLOT_SIZE]) {
            if manifest.validate_lengths()
                && best.as_ref().map(|b| manifest.seq > b.seq).unwrap_or(true)
            {
                best = Some(manifest);
            }
        }
    }
    best.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no valid manifest slot"))
}

pub fn write_manifest(dir: &Path, manifest: &Manifest) -> io::Result<Manifest> {
    fs::create_dir_all(dir)?;
    let path = dir.join("manifest.bin");
    let mut max_seq = 0u64;
    if path.exists() {
        if let Ok(existing) = read_manifest(dir) {
            max_seq = existing.seq;
        }
    }
    let seq = max_seq + 1;
    let slot_index = (seq % 2) as usize;
    let slot = encode_slot(manifest, seq);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.seek(SeekFrom::Start((slot_index * SLOT_SIZE) as u64))?;
    file.write_all(&slot)?;
    file.sync_all()?;
    let mut committed = manifest.clone();
    committed.seq = seq;
    Ok(committed)
}
