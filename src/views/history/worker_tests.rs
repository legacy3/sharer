//! Regression tests for the asynchronous, bounded history loader.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use sharer::{
    history::{HistoryEntry, HistoryFilter, HistorySort},
    storage::{HistoryLocalPathReconciliation, PlatformStorage},
};

use super::worker::{HistoryEvent, HistoryLoader, HistoryPage, Reply};

#[cfg(test)]
fn next_reply(loader: &mut HistoryLoader, deadline: Instant) -> Reply {
    loader
        .wait_for_reply(deadline.saturating_duration_since(Instant::now()))
        .expect("history worker did not reply before its deadline")
}

#[cfg(test)]
fn completion(loader: &mut HistoryLoader, generation: u64) -> HistoryPage {
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        let reply = next_reply(loader, deadline);

        if let HistoryEvent::Complete(result) = reply.event {
            assert_eq!(reply.generation, generation, "stale completion escaped");

            return result.unwrap();
        }
    }
}

#[cfg(test)]
fn entry(index: i64) -> HistoryEntry {
    HistoryEntry {
        id: 0,
        created_at: index + 1,
        filename: format!("capture-{index:02}.png"),
        size_bytes: 42,
        link: String::new(),
        delete_url: String::new(),
        local_path: String::new(),
        expires_at: String::new(),
    }
}

#[test]
fn worker_completes_pages_and_clamps_an_out_of_range_page() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for index in 0..61 {
        storage.insert_history(&entry(index)).unwrap();
    }

    let mut loader = HistoryLoader::new(&storage).unwrap();
    let generation = loader
        .submit(1, HistorySort::default(), String::new(), HistoryFilter::All)
        .unwrap();
    let page = completion(&mut loader, generation);

    assert_eq!(page.page, 1);
    assert_eq!(page.total, 61);
    assert_eq!(page.entries.len(), 25);
    assert_eq!(page.entries.first().unwrap().filename, "capture-35.png");
    assert_eq!(page.entries.last().unwrap().filename, "capture-11.png");
    assert_eq!(
        page.reconciliation,
        HistoryLocalPathReconciliation::default()
    );
    let generation = loader
        .submit(
            99,
            HistorySort::default(),
            String::new(),
            HistoryFilter::All,
        )
        .unwrap();
    let page = completion(&mut loader, generation);

    assert_eq!(page.page, 2);
    assert_eq!(page.total, 61);
    assert_eq!(page.entries.len(), 11);
    assert_eq!(page.entries.first().unwrap().filename, "capture-10.png");
    assert_eq!(page.entries.last().unwrap().filename, "capture-00.png");
}

#[test]
fn worker_reconciles_missing_path_search_in_bounded_batches() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut expected = BTreeMap::new();

    for index in 0..61 {
        let path = directory
            .path()
            .join("missing-batch")
            .join(format!("{index}.png"));
        let mut record = entry(index);

        record.local_path = path.to_string_lossy().into_owned();
        expected.insert(storage.insert_history(&record).unwrap(), path);
    }

    let mut loader = HistoryLoader::new(&storage).unwrap();
    let generation = loader
        .submit(
            0,
            HistorySort::default(),
            "missing-batch".to_owned(),
            HistoryFilter::LocalOnly,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = BTreeMap::new();
    let mut batches = 0;
    let page = loop {
        let reply = next_reply(&mut loader, deadline);

        assert_eq!(reply.generation, generation);

        match reply.event {
            HistoryEvent::Progress { cleared, checked } => {
                assert!(checked > 0);
                assert!(
                    cleared.len() <= 25,
                    "progress must stay bounded to one page"
                );
                batches += 1;

                for (id, path) in cleared {
                    assert!(observed.insert(id, path).is_none(), "duplicate clear event");
                }
            }

            HistoryEvent::Complete(result) => break result.unwrap(),
        }
    };

    assert!(batches >= 3);
    assert_eq!(observed, expected);
    assert_eq!(page.page, 0);
    assert_eq!(page.total, 0);
    assert!(page.entries.is_empty());
    assert_eq!(page.reconciliation.cleared, 61);
    assert_eq!(page.reconciliation.unavailable, 0);
    assert_eq!(page.reconciliation.not_regular, 0);
    assert_eq!(storage.history_count().unwrap(), 61);

    for id in expected.keys() {
        assert!(
            storage
                .history_entry(*id)
                .unwrap()
                .unwrap()
                .local_path
                .is_empty()
        );
    }
}

#[test]
fn rapid_requests_and_cancellation_deliver_only_the_latest_completion() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for index in 0..61 {
        storage.insert_history(&entry(index)).unwrap();
    }

    let mut loader = HistoryLoader::new(&storage).unwrap();
    let mut previous = 0;

    for index in 0..100 {
        let generation = loader
            .submit(
                0,
                HistorySort::default(),
                format!("capture-{index:02}"),
                HistoryFilter::All,
            )
            .unwrap();

        assert!(generation > previous);
        previous = generation;
    }

    let cancelled = loader.cancel().unwrap();

    assert!(cancelled > previous);
    let generation = loader
        .submit(
            0,
            HistorySort::default(),
            "capture-42".to_owned(),
            HistoryFilter::LocalOnly,
        )
        .unwrap();

    assert!(generation > cancelled);
    let page = completion(&mut loader, generation);

    assert_eq!(page.total, 1);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].filename, "capture-42.png");
    assert!(loader.poll().is_none());
}

#[test]
fn worker_keeps_its_original_profile_and_connection_alive() {
    let original = tempfile::tempdir().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(original.path()).unwrap();

    storage.insert_history(&entry(1)).unwrap();
    let mut loader = HistoryLoader::new(&storage).unwrap();

    drop(storage);
    let unrelated_storage = PlatformStorage::open_in(unrelated.path()).unwrap();

    unrelated_storage.insert_history(&entry(99)).unwrap();
    let reopened = PlatformStorage::open_in(original.path()).unwrap();

    reopened.insert_history(&entry(2)).unwrap();
    let generation = loader
        .submit(0, HistorySort::default(), String::new(), HistoryFilter::All)
        .unwrap();
    let page = completion(&mut loader, generation);

    assert_eq!(page.total, 2);
    assert_eq!(page.entries[0].filename, "capture-02.png");
    assert_eq!(page.entries[1].filename, "capture-01.png");
    assert_eq!(unrelated_storage.history_count().unwrap(), 1);
}

#[test]
fn worker_retries_a_transient_profile_open_failure() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let saved_profile = directory.path().join("saved-profile");
    let storage = PlatformStorage::open_in(&profile).unwrap();

    storage.insert_history(&entry(7)).unwrap();
    let mut loader = HistoryLoader::new(&storage).unwrap();

    // The worker opens lazily on its first request, so no connection races this fixture.
    drop(storage);
    std::fs::rename(&profile, &saved_profile).unwrap();
    std::fs::write(&profile, b"temporarily unavailable profile directory").unwrap();
    let failed_generation = loader
        .submit(0, HistorySort::default(), String::new(), HistoryFilter::All)
        .unwrap();
    let reply = next_reply(&mut loader, Instant::now() + Duration::from_secs(5));

    assert_eq!(reply.generation, failed_generation);
    let HistoryEvent::Complete(Err(error)) = reply.event else {
        panic!("an unavailable profile must report the failed open");
    };

    assert!(
        error
            .to_string()
            .contains("could not open the history worker database")
    );
    std::fs::remove_file(&profile).unwrap();
    std::fs::rename(&saved_profile, &profile).unwrap();
    let recovered_generation = loader
        .submit(0, HistorySort::default(), String::new(), HistoryFilter::All)
        .unwrap();
    let page = completion(&mut loader, recovered_generation);

    assert!(recovered_generation > failed_generation);
    assert_eq!(page.total, 1);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].filename, "capture-07.png");
}

#[test]
fn cancelling_a_scan_does_not_lose_committed_clear_notifications() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut expected = BTreeMap::<i64, PathBuf>::new();

    for index in 0..61 {
        let path = directory
            .path()
            .join("missing-batch")
            .join(format!("{index}.png"));
        let mut record = entry(index);

        record.local_path = path.to_string_lossy().into_owned();
        expected.insert(storage.insert_history(&record).unwrap(), path);
    }

    let mut loader = HistoryLoader::new(&storage).unwrap();

    loader
        .submit(
            0,
            HistorySort::default(),
            "missing-batch".to_owned(),
            HistoryFilter::All,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = next_reply(&mut loader, deadline);
    let HistoryEvent::Progress { cleared, .. } = first.event else {
        panic!("missing paths must be published before completion");
    };
    let mut observed = cleared.into_iter().collect::<BTreeMap<_, _>>();

    loader.cancel().unwrap();
    let generation = loader
        .submit(
            0,
            HistorySort::default(),
            "missing-batch".to_owned(),
            HistoryFilter::All,
        )
        .unwrap();

    loop {
        let reply = next_reply(&mut loader, deadline);

        match reply.event {
            HistoryEvent::Progress { cleared, .. } => {
                assert!(cleared.len() <= 25);

                for (id, path) in cleared {
                    assert!(observed.insert(id, path).is_none(), "duplicate clear event");
                }
            }

            HistoryEvent::Complete(result) => {
                assert_eq!(reply.generation, generation);
                assert_eq!(result.unwrap().total, 0);

                break;
            }
        }
    }

    assert_eq!(observed, expected);
    assert_eq!(storage.history_count().unwrap(), 61);
}
