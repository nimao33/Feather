use std::path::PathBuf;

use feather_core::synthetic::generate;
#[cfg(windows)]
use feather_core::Query;
use feather_core::VolumeIndex;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("feather-core-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// Uses drive letters and backslashes: Windows paths.
#[cfg(windows)]
#[test]
fn roundtrip_and_search() {
    let dir = test_dir("roundtrip");
    let report = generate(&dir, 1_001, r"C:\FeatherTest").expect("generate");
    assert_eq!(report.entries, 1_001);

    let index = VolumeIndex::open(&dir).expect("open");
    assert_eq!(index.entry_count(), 1_001);
    assert!(index.entry(0).is_dir());
    assert_eq!(index.entry(0).name_search, "");

    let query = Query::parse("file0000005");
    let matches: Vec<u32> = index
        .iter_ids()
        .filter(|id| {
            let entry = index.entry(*id);
            query.matches(entry.name_search, &entry)
        })
        .collect();
    assert_eq!(matches.len(), 1);
    let path = index.resolve_path(matches[0]).expect("path");
    assert!(path.starts_with(r"C:\FeatherTest\dir"));
    assert!(path.contains("file0000005"));

    let pdf = Query::parse("ext:pdf");
    let pdf_count = index
        .iter_ids()
        .filter(|id| {
            let entry = index.entry(*id);
            pdf.matches(entry.name_search, &entry)
        })
        .count();
    assert!(pdf_count > 0);

    let dirs = Query::parse("type:folder");
    let dir_count = index
        .iter_ids()
        .filter(|id| {
            let entry = index.entry(*id);
            dirs.matches(entry.name_search, &entry)
        })
        .count();
    assert!(dir_count > 0);

    let wildcard = Query::parse("*.txt");
    assert!(index.iter_ids().any(|id| {
        let entry = index.entry(id);
        wildcard.matches(entry.name_search, &entry)
    }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_truncated_index() {
    let dir = test_dir("truncated");
    generate(&dir, 20, r"C:\FeatherTest").expect("generate");
    let meta = dir.join("base-0000000000000001.meta");
    let data = std::fs::read(&meta).expect("read meta");
    std::fs::write(&meta, &data[..data.len() - 1]).expect("truncate");
    assert!(VolumeIndex::open(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_corrupt_manifest_slot() {
    let dir = test_dir("bad-manifest");
    generate(&dir, 20, r"C:\FeatherTest").expect("generate");
    let manifest = dir.join("manifest.bin");
    let mut data = std::fs::read(&manifest).expect("read manifest");
    // Sequence 1 writes the second 4 KB slot.
    data[4096 + 24] ^= 0xff;
    std::fs::write(&manifest, &data).expect("corrupt manifest");
    assert!(VolumeIndex::open(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_unsorted_frn_table() {
    let dir = test_dir("bad-frn");
    generate(&dir, 30, r"C:\FeatherTest").expect("generate");
    let frn = dir.join("base-0000000000000001.frn");
    let mut data = std::fs::read(&frn).expect("read frn");
    let first = data[0..12].to_vec();
    let second = data[12..24].to_vec();
    data[0..12].copy_from_slice(&second);
    data[12..24].copy_from_slice(&first);
    std::fs::write(&frn, &data).expect("corrupt frn");
    assert!(VolumeIndex::open(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_out_of_bounds_name_offset() {
    let dir = test_dir("bad-offset");
    generate(&dir, 30, r"C:\FeatherTest").expect("generate");
    let meta = dir.join("base-0000000000000001.meta");
    let mut data = std::fs::read(&meta).expect("read meta");
    // Entry 1, display name offset is 48 bytes into its record.
    data[48 + 32..48 + 36].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&meta, &data).expect("corrupt meta");
    assert!(VolumeIndex::open(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
