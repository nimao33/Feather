use std::sync::Arc;
use std::time::{Duration, Instant};

use feather_app::search::{SearchController, SearchEvent, SearchOptions, RESULT_CAP};
use feather_core::synthetic::generate;
use feather_core::{IndexSet, Query};

fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feather-search-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open_set(dir: &std::path::Path) -> Arc<IndexSet> {
    Arc::new(IndexSet::open(dir).expect("open"))
}

#[test]
fn streams_matches_and_finishes() {
    let dir = test_dir("stream");
    generate(&dir, 5_000, r"C:\FeatherTest").expect("generate");
    let index = open_set(&dir);
    let controller = SearchController::new(index);

    let generation = controller.submit(
        Query::parse("file0000005"),
        RESULT_CAP,
        SearchOptions::default(),
    );
    let started = Instant::now();
    let mut ids = Vec::new();
    let mut done = false;
    while started.elapsed() < Duration::from_secs(5) {
        match controller.try_recv() {
            Ok(SearchEvent::Batch {
                generation: event_generation,
                ids: batch,
            }) if event_generation == generation => ids.extend(batch),
            Ok(SearchEvent::Done {
                generation: event_generation,
                total,
                capped,
                ..
            }) if event_generation == generation => {
                assert!(!capped);
                assert_eq!(total, ids.len() as u64);
                done = true;
                break;
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert!(done, "search did not finish");
    assert!(!ids.is_empty(), "expected the synthetic file to match");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_query_wins_over_a_running_one() {
    let dir = test_dir("cancel");
    generate(&dir, 100_000, r"C:\FeatherTest").expect("generate");
    let index = open_set(&dir);
    let controller = SearchController::new(index);

    let first = controller.submit(Query::parse("file"), RESULT_CAP, SearchOptions::default());
    let second = controller.submit(
        Query::parse("file0000005"),
        RESULT_CAP,
        SearchOptions::default(),
    );

    let started = Instant::now();
    let mut ids = Vec::new();
    let mut done_generation = 0;
    while started.elapsed() < Duration::from_secs(5) {
        match controller.try_recv() {
            Ok(SearchEvent::Batch {
                generation,
                ids: batch,
            }) => {
                if generation == second {
                    ids.extend(batch);
                }
            }
            Ok(SearchEvent::Done {
                generation, total, ..
            }) => {
                if generation == second {
                    assert_eq!(total, 1);
                    done_generation = generation;
                    break;
                }
                if generation == first {
                    panic!("the old query produced a completion event");
                }
            }
            Ok(SearchEvent::Span { .. }) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert_eq!(done_generation, second);
    assert_eq!(ids.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_result_cap_limits_rows_and_reports_the_cap() {
    let dir = test_dir("cap");
    generate(&dir, 5_000, r"C:\FeatherTest").expect("generate");
    let index = open_set(&dir);
    let controller = SearchController::new(index);

    let generation = controller.submit(Query::parse(""), 10, SearchOptions::default());
    let started = Instant::now();
    let mut ids = Vec::new();
    let mut done = false;
    while started.elapsed() < Duration::from_secs(5) {
        match controller.try_recv() {
            Ok(SearchEvent::Batch {
                generation: event_generation,
                ids: batch,
            }) if event_generation == generation => ids.extend(batch),
            // An empty search lists whole volumes as runs of ids.
            Ok(SearchEvent::Span {
                generation: event_generation,
                volume,
                first,
                count,
            }) if event_generation == generation => {
                ids.extend(
                    (first..first + count).map(|id| feather_core::EntryRef::new(volume, id)),
                );
            }
            Ok(SearchEvent::Done {
                generation: event_generation,
                total,
                capped,
                ..
            }) if event_generation == generation => {
                assert!(capped);
                assert_eq!(total, 10);
                done = true;
                break;
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert!(done, "search did not finish");
    assert_eq!(ids.len(), 10);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn volume_mask_and_excluded_paths_are_applied() {
    let root = test_dir("scope");
    let c = root.join("c");
    let d = root.join("d");
    generate(&c, 3_000, r"C:\FeatherTest").expect("generate c");
    generate(&d, 2_000, r"D:\FeatherTest").expect("generate d");
    let index = Arc::new(IndexSet::open(&root).expect("open library"));
    let controller = SearchController::new(Arc::clone(&index));

    let only_c = index.volume_mask(&["d:".into()]);
    let options = SearchOptions {
        volume_mask: only_c,
        ..SearchOptions::default()
    };
    let generation = controller.submit(Query::parse("file"), 10_000, options);
    let started = Instant::now();
    let mut volumes = std::collections::BTreeSet::new();
    while started.elapsed() < Duration::from_secs(5) {
        match controller.try_recv() {
            Ok(SearchEvent::Batch {
                generation: event_generation,
                ids,
            }) if event_generation == generation => {
                for id in ids {
                    volumes.insert(id.volume());
                }
            }
            Ok(SearchEvent::Done {
                generation: event_generation,
                ..
            }) if event_generation == generation => break,
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert_eq!(volumes.into_iter().collect::<Vec<_>>(), vec![0]);

    let generation = controller.submit(
        Query::parse(r#"file in:"D:\FeatherTest""#),
        10_000,
        SearchOptions::default(),
    );
    let mut paths = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match controller.try_recv() {
            Ok(SearchEvent::Batch {
                generation: event_generation,
                ids,
            }) if event_generation == generation => {
                for id in ids {
                    paths.push(index.resolve_path(id).expect("path"));
                }
            }
            Ok(SearchEvent::Done {
                generation: event_generation,
                ..
            }) if event_generation == generation => break,
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|path| path.starts_with(r"D:\FeatherTest")));
    let _ = std::fs::remove_dir_all(&root);
}
