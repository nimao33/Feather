use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::format::{
    write_manifest, Manifest, CHECK_STRIDE, COUNT_SIZE, FEATURE_FOLDER_TOTALS, FORMAT_VERSION,
    FRN_SIZE, META_SIZE,
};

#[derive(Clone, Debug)]
pub struct SyntheticReport {
    pub entries: u32,
    pub dirs: u32,
    pub files: u32,
    pub bytes: u64,
    pub elapsed: Duration,
}

/// Generates a sorted, deterministic index for UI and search benchmarking.
///
/// Entry 0 is the volume root. Directories sort first (`dir0000`), then files
/// (`file0000000.ext`). File reference numbers are `entry_id + 1`.
pub fn generate(dir: &Path, entries: u32, prefix: &str) -> io::Result<SyntheticReport> {
    let started = Instant::now();
    let entries = entries.max(1);
    fs::create_dir_all(dir)?;
    cleanup(dir)?;

    let item_count = entries.saturating_sub(1);
    let dirs = if item_count == 0 {
        0
    } else {
        (item_count / 200).clamp(1, 2000).min(item_count)
    };
    let files = item_count.saturating_sub(dirs);

    let mut meta = Vec::with_capacity(entries as usize * META_SIZE);
    let mut search = Vec::new();
    let mut display = Vec::new();
    let mut frn = Vec::with_capacity(entries as usize * FRN_SIZE);
    let mut check = Vec::new();
    let mut counts = Vec::with_capacity(entries as usize * COUNT_SIZE);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let now_filetime = (now + 11_644_473_600) * 10_000_000;
    let extensions = ["pdf", "docx", "txt", "png", "rs", "log", "bin", "md"];
    let file_size = |index: u32| match index % 10 {
        0 => 0,
        1..=6 => (index as u64).wrapping_mul(131) % 2_000_000,
        7..=8 => (index as u64).wrapping_mul(104_729) % 200_000_000,
        _ => (index as u64).wrapping_mul(7_919) % 8_000_000_000,
    };
    // Folder totals: file `index` lives in folder `index % dirs`.
    let mut dir_bytes = vec![0u64; dirs as usize];
    let mut dir_items = vec![0u32; dirs as usize];
    for index in 0..files {
        let folder = (index % dirs) as usize;
        dir_bytes[folder] += file_size(index);
        dir_items[folder] += 1;
    }
    let all_bytes: u64 = dir_bytes.iter().sum();

    for id in 0..entries {
        let (name, parent_frn, size, mtime, flags) = if id == 0 {
            counts.extend_from_slice(&item_count.to_le_bytes());
            (String::new(), 1u64, all_bytes, now_filetime, 1u16)
        } else if id <= dirs {
            let index = id - 1;
            counts.extend_from_slice(&dir_items[index as usize].to_le_bytes());
            (
                format!("dir{:04}", index),
                1u64,
                dir_bytes[index as usize],
                now_filetime.saturating_sub((index as u64 % 3650) * 86_400 * 10_000_000),
                1u16,
            )
        } else {
            let index = id - dirs - 1;
            let extension = extensions[(index as usize) % extensions.len()];
            let parent = 2u64 + (index % dirs) as u64;
            let size = file_size(index);
            counts.extend_from_slice(&0u32.to_le_bytes());
            let mut flags = 0u16;
            if index % 1000 == 0 {
                flags |= 2;
            }
            if index % 5000 == 0 {
                flags |= 4;
            }
            (
                format!("file{:07}.{}", index, extension),
                parent,
                size,
                now_filetime.saturating_sub((index as u64 % 3650) * 86_400 * 10_000_000),
                flags,
            )
        };

        let search_name = name.to_lowercase();
        let display_units: Vec<u16> = name.encode_utf16().collect();
        let search_offset = search.len() as u32;
        let display_offset = display.len() as u32;
        let search_len = search_name.len() as u16;
        let display_len = display_units.len() as u16;

        if (id as usize).is_multiple_of(CHECK_STRIDE) {
            check.extend_from_slice(&search_offset.to_le_bytes());
            check.extend_from_slice(&display_offset.to_le_bytes());
        }

        search.extend_from_slice(search_name.as_bytes());
        search.push(0);
        for unit in &display_units {
            display.extend_from_slice(&unit.to_le_bytes());
        }
        display.extend_from_slice(&0u16.to_le_bytes());

        let file_reference = id as u64 + 1;
        meta.extend_from_slice(&file_reference.to_le_bytes());
        meta.extend_from_slice(&parent_frn.to_le_bytes());
        meta.extend_from_slice(&size.to_le_bytes());
        meta.extend_from_slice(&mtime.to_le_bytes());
        meta.extend_from_slice(&display_offset.to_le_bytes());
        meta.extend_from_slice(&search_offset.to_le_bytes());
        meta.extend_from_slice(&display_len.to_le_bytes());
        meta.extend_from_slice(&search_len.to_le_bytes());
        meta.extend_from_slice(&flags.to_le_bytes());
        meta.extend_from_slice(&0u16.to_le_bytes());

        frn.extend_from_slice(&file_reference.to_le_bytes());
        frn.extend_from_slice(&id.to_le_bytes());
    }

    let generation = 1u64;
    let manifest = Manifest {
        generation,
        entry_count: entries,
        meta_len: meta.len() as u64,
        search_len: search.len() as u64,
        display_len: display.len() as u64,
        frn_len: frn.len() as u64,
        check_len: check.len() as u64,
        seq: 0,
        features: FEATURE_FOLDER_TOTALS,
    };
    if manifest.entry_count == 0
        || manifest.meta_len != manifest.entry_count as u64 * META_SIZE as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "synthetic index shape is invalid",
        ));
    }

    write_file(&dir.join(format!("base-{:016}.meta", generation)), &meta)?;
    write_file(
        &dir.join(format!("base-{:016}.search", generation)),
        &search,
    )?;
    write_file(
        &dir.join(format!("base-{:016}.display", generation)),
        &display,
    )?;
    write_file(&dir.join(format!("base-{:016}.frn", generation)), &frn)?;
    write_file(&dir.join(format!("base-{:016}.check", generation)), &check)?;
    write_file(&dir.join(manifest.count_name()), &counts)?;
    fs::write(dir.join("prefix.txt"), prefix.as_bytes())?;
    write_manifest(dir, &manifest)?;

    Ok(SyntheticReport {
        entries,
        dirs,
        files,
        bytes: (meta.len() + search.len() + display.len() + frn.len() + check.len()) as u64,
        elapsed: started.elapsed(),
    })
}

fn cleanup(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with("base-") || name == "manifest.bin" || name == "prefix.txt")
            && entry.path().is_file()
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file = fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn manifest_version() -> u32 {
    FORMAT_VERSION
}
