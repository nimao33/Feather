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

// Volumes named by drive letter: Windows paths.
#[cfg(windows)]
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

/// Every row a search sends, spans spelled out, once it is done.
fn collect(controller: &SearchController, generation: u64) -> (Vec<feather_core::EntryRef>, u64) {
    let started = Instant::now();
    let mut ids = Vec::new();
    while started.elapsed() < Duration::from_secs(20) {
        match controller.try_recv() {
            Ok(SearchEvent::Batch {
                generation: event,
                ids: batch,
            }) if event == generation => ids.extend(batch),
            Ok(SearchEvent::Span {
                generation: event,
                volume,
                first,
                count,
            }) if event == generation => {
                ids.extend((first..first + count).map(|id| feather_core::EntryRef::new(volume, id)))
            }
            Ok(SearchEvent::Done {
                generation: event,
                total,
                ..
            }) if event == generation => return (ids, total),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    panic!("search did not finish");
}

/// Every id the search for `query` returns, spans expanded.
fn collect_query(controller: &SearchController, query: Query) -> Vec<feather_core::EntryRef> {
    let generation = controller.submit(query, RESULT_CAP, SearchOptions::default());
    collect(controller, generation).0
}

#[test]
fn date_filters_are_applied_by_the_worker_and_when_narrowing() {
    use feather_core::date::Clock;
    const DAY: u64 = 86_400 * 10_000_000;

    // Big enough that the worker splits the volume across threads, small
    // enough that every match fits under the result cap.
    let dir = test_dir("dates");
    generate(&dir, 210_001, r"C:\FeatherTest").expect("generate");
    let index = open_set(&dir);
    let controller = SearchController::new(Arc::clone(&index));
    let volume = &index.volumes()[0];
    let now = volume.entry(0).mtime;
    let clock = Clock::fixed(now, 0);
    let recent =
        |reference: &feather_core::EntryRef| (now - volume.entry_mtime(reference.id())) / DAY <= 6;

    // A date filter alone must not take the list-everything shortcut.
    let dated = collect_query(&controller, Query::parse_at("dm:last7days", &clock));
    let expected: Vec<_> = volume
        .iter_ids()
        .map(|id| feather_core::EntryRef::new(0, id))
        .filter(recent)
        .collect();
    assert!(!dated.is_empty() && dated.len() < volume.iter_ids().count());
    assert_eq!(dated, expected);

    // Narrowing earlier rows applies the date filter too.
    let files = collect_query(&controller, Query::parse_at("file0000", &clock));
    let previous = feather_app::rows::Rows::from_refs(files.clone());
    let query = Query::parse_at("file0000 dm:last7days", &clock);
    assert!(query.narrows(&Query::parse_at("file0000", &clock)));
    let narrowed =
        feather_app::search::narrow(&index, &previous, &query, RESULT_CAP).expect("under the cap");
    let expected: Vec<_> = files.iter().copied().filter(recent).collect();
    assert!(!expected.is_empty());
    assert_eq!(narrowed.to_vec(), expected);
    assert_eq!(collect_query(&controller, query), expected);
    drop(controller);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_changes_are_searched_in_name_order() {
    use feather_core::live::{Change, EntryState, JournalCursor, Overlay, Rules};
    let dir = test_dir("live");
    // Big enough to be scanned in parallel parts.
    generate(&dir, 250_000, r"C:\FeatherTest").expect("generate");
    let base = open_set(&dir);
    let volume = &base.volumes()[0];
    let root_frn = volume.entry(0).frn;
    let named = |name: &str| {
        volume
            .iter_ids()
            .find(|&id| volume.entry(id).name_search == name)
            .expect(name)
    };
    let gone = named("file0000005.log");
    let gone_frn = volume.entry(gone).frn;
    let renamed_frn = volume.entry(named("file0000006.bin")).frn;
    let file = |frn: u64, name: &str| {
        Change::Upsert(EntryState {
            frn,
            parent_frn: root_frn,
            name: name.into(),
            size: Some(1),
            mtime: 1,
            flags: 0,
            created: true,
        })
    };
    let mut overlay = Overlay::new(volume, JournalCursor::default());
    let report = overlay.apply(
        volume,
        &[
            Change::Remove(gone_frn),
            file(u64::MAX - 1, "file0000005 copy.txt"),
            file(renamed_frn, "file0000005b.txt"),
            file(u64::MAX - 2, "aaa-first.txt"),
        ],
        &Rules::default(),
    );
    assert!(report.changed);
    let live = Arc::new(base.with_overlays(&[Some(Arc::new(overlay))]));
    let controller = SearchController::new(Arc::clone(&live));
    let name_of = |reference: feather_core::EntryRef| live.entry(reference).name_search.to_string();

    let generation = controller.submit(
        Query::parse("file0000005"),
        RESULT_CAP,
        SearchOptions::default(),
    );
    let (ids, total) = collect(&controller, generation);
    let names: Vec<String> = ids.iter().map(|&id| name_of(id)).collect();
    assert_eq!(total, names.len() as u64);
    assert!(!names.contains(&"file0000005.log".to_string()));
    let at = |name: &str| names.iter().position(|found| found == name).expect(name);
    assert!(at("file0000005 copy.txt") < at("file0000005b.txt"));
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "overlay matches sit in name order");
    let path = live
        .resolve_path(ids[at("file0000005b.txt")])
        .expect("path");
    assert!(
        path.starts_with("C:") && path.ends_with("file0000005b.txt"),
        "{path}"
    );

    // Matching everything: runs around the deleted entry, new ones in place.
    let generation = controller.submit(Query::parse(""), RESULT_CAP * 10, SearchOptions::default());
    let (ids, total) = collect(&controller, generation);
    assert_eq!(total, live.item_count());
    assert_eq!(ids.len() as u64, live.item_count());
    assert_eq!(name_of(ids[0]), "aaa-first.txt");
    assert!(ids.iter().all(|&id| !live.volumes()[0].is_removed(id.id())));
    assert!(ids.windows(2).all(|pair| live.volumes()[0]
        .compare_names(pair[0].id(), pair[1].id())
        .is_lt()));
    drop(controller);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_changes_match_date_and_size_filters() {
    use feather_core::date::Clock;
    use feather_core::live::{Change, EntryState, JournalCursor, Overlay, Rules};
    const DAY: u64 = 86_400 * 10_000_000;
    let dir = test_dir("live-filters");
    // Big enough to be scanned in parallel parts.
    generate(&dir, 210_001, r"C:\FeatherTest").expect("generate");
    let base = open_set(&dir);
    let volume = &base.volumes()[0];
    assert!(volume.has_folder_totals());
    let root_frn = volume.entry(0).frn;
    // Well after every generated time, so no base entry is recent.
    let now = volume
        .iter_ids()
        .map(|id| volume.entry_mtime(id))
        .max()
        .unwrap_or(0)
        + 30 * DAY;
    let clock = Clock::fixed(now, 0);
    let upsert = |frn: u64, parent: u64, name: &str, size: u64, mtime: u64, flags: u16| {
        Change::Upsert(EntryState {
            frn,
            parent_frn: parent,
            name: name.into(),
            size: Some(size),
            mtime,
            flags,
            created: true,
        })
    };
    let (folder, big, old, fresh) = (u64::MAX - 10, u64::MAX - 11, u64::MAX - 12, u64::MAX - 13);
    let mut overlay = Overlay::new(volume, JournalCursor::default());
    overlay.apply(
        volume,
        &[
            upsert(folder, root_frn, "zz-live-folder", 0, now - 40 * DAY, 1),
            upsert(big, folder, "zz-live-big.bin", 5 << 20, now - 40 * DAY, 0),
            upsert(old, root_frn, "zz-live-old.txt", 1, now - 40 * DAY, 0),
            upsert(fresh, root_frn, "zz-live-new.txt", 1, now - DAY, 0),
        ],
        &Rules::default(),
    );
    let live = Arc::new(base.with_overlays(&[Some(Arc::new(overlay))]));
    let controller = SearchController::new(Arc::clone(&live));
    let names = |ids: &[feather_core::EntryRef]| -> Vec<String> {
        ids.iter()
            .map(|&id| live.entry(id).name_search.to_string())
            .collect()
    };

    // Dates: the worker's scan sees overlay entries by their own time.
    let recent = collect_query(&controller, Query::parse_at("dm:last7days", &clock));
    assert_eq!(names(&recent), vec!["zz-live-new.txt"]);
    let all = collect_query(&controller, Query::parse_at("zz-live", &clock));
    assert_eq!(all.len(), 4);
    // Narrowing earlier rows applies the date filter to them too.
    let previous = feather_app::rows::Rows::from_refs(all.clone());
    let query = Query::parse_at("zz-live dm:last7days", &clock);
    let narrowed =
        feather_app::search::narrow(&live, &previous, &query, RESULT_CAP).expect("narrowed");
    assert_eq!(names(&narrowed.to_vec()), vec!["zz-live-new.txt"]);

    // Sizes: the new folder's total is what was added below it.
    let folder_ref = all
        .iter()
        .copied()
        .find(|&id| live.entry(id).name_search == "zz-live-folder")
        .expect("folder");
    assert_eq!(live.folder_totals(folder_ref), Some((5 << 20, 1)));
    let big_ones = collect_query(&controller, Query::parse_at("zz-live size:>1mb", &clock));
    assert_eq!(names(&big_ones), vec!["zz-live-big.bin", "zz-live-folder"]);
    // And the root's total grew by everything added.
    let root = feather_core::EntryRef::new(0, 0);
    let (base_size, base_items) = base.folder_totals(root).expect("root");
    assert_eq!(
        live.folder_totals(root),
        Some((base_size + (5 << 20) + 2, base_items + 4))
    );
    drop(controller);
    let _ = std::fs::remove_dir_all(&dir);
}
