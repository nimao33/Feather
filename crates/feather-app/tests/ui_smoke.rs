use std::path::PathBuf;
use std::process::Command;

fn test_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "feather-ui-smoke-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

#[test]
#[ignore = "requires an interactive desktop; run scripts/ui-test.ps1"]
fn scenarios_write_screenshots() {
    let root = test_root();
    let index = root.join("index");
    let screenshots = root.join("screenshots");
    let status = Command::new(env!("CARGO_BIN_EXE_feather"))
        .arg("--index")
        .arg(&index)
        .arg("--entries")
        .arg("5000")
        .arg("--ui-test")
        .arg("--screenshots")
        .arg(&screenshots)
        .status()
        .expect("start feather --ui-test");
    assert!(status.success(), "ui test exited with {status}");

    for name in [
        "01-all.png",
        "02-match.png",
        "03-no-results.png",
        "04-sorted.png",
        "05-path.png",
        "06-filters.png",
        "07-filtered.png",
        "08-settings.png",
        "09-help.png",
        "10-quick-filter.png",
        "11-scope.png",
        "38-folder-sizes.png",
    ] {
        let path = screenshots.join(name);
        let metadata = std::fs::metadata(&path).expect("screenshot missing");
        assert!(
            metadata.len() > 1_000,
            "screenshot {name} is suspiciously small"
        );
    }
    let report = std::fs::read_to_string(screenshots.join("report.txt")).expect("report");
    assert!(report.contains("02-match: rows=1"));
    assert!(report.contains("04-sorted: rows="));
    assert!(report.contains("05-path: rows="));
    assert!(report.contains("06-filters: screenshot="));
    assert!(report.contains("07-filtered: rows="));
    assert!(report.contains("08-settings: screenshot="));
    assert!(report.contains("09-help: screenshot="));
    assert!(report.contains("10-quick-filter: rows="));
    assert!(report.contains("11-scope: rows="));
    assert!(report.contains("38-folder-sizes: rows="));
    println!("screenshots: {}", screenshots.display());
}
