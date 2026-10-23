use std::{
    sync::{atomic::AtomicUsize, mpsc},
    task::Poll,
    thread,
    time::{Duration, Instant},
};

use super::*;

fn addresses() -> Vec<SocketAddr> {
    vec![SocketAddr::from(([127, 0, 0, 1], 0))]
}

fn wait_until_released(in_flight: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(2);

    while in_flight.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }

    assert!(!in_flight.load(Ordering::Acquire));
}

#[test]
fn cancelled_running_lookup_retains_admission_until_the_os_call_returns() {
    let in_flight = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_gate = Arc::clone(&in_flight);
    let worker_calls = Arc::clone(&calls);
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = super::super::run_download(async move {
            let mut lookup = resolve_with("fixture.invalid".to_owned(), worker_gate, move |_| {
                worker_calls.fetch_add(1, Ordering::AcqRel);
                started_sender.send(()).unwrap();
                let _ = release_receiver.recv_timeout(Duration::from_secs(10));

                Ok(addresses())
            });

            assert!(matches!(
                futures_util::poll!(lookup.as_mut()),
                Poll::Pending
            ));
            started_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            drop(lookup);

            Ok(())
        });

        result_sender.send(result).unwrap();
    });
    let result = result_receiver.recv_timeout(Duration::from_secs(2));
    let retry_result = super::super::run_download(async {
        for _ in 0..100 {
            let retry_calls = Arc::clone(&calls);
            let result = resolve_with(
                "fixture.invalid".to_owned(),
                Arc::clone(&in_flight),
                move |_| {
                    retry_calls.fetch_add(1, Ordering::AcqRel);

                    Ok(addresses())
                },
            )
            .await;

            let error = result
                .err()
                .ok_or_else(|| anyhow::anyhow!("a still-running lookup must reject retries"))?;

            anyhow::ensure!(
                error
                    .to_string()
                    .contains("previous FFmpeg network address lookup")
            );
        }

        Ok(())
    });

    // Unblock the synthetic OS lookup before assertions, including failures.
    let _ = release_sender.send(());

    worker.join().unwrap();
    result
        .expect("Cancel must return before the lookup is released")
        .unwrap();
    retry_result.unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    wait_until_released(&in_flight);
    super::super::run_download(async {
        let found = resolve_with("fixture.invalid".to_owned(), in_flight, |_| Ok(addresses()))
            .await
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(found, addresses());

        Ok(())
    })
    .unwrap();
}

#[test]
fn lookup_error_and_panic_release_admission() {
    let in_flight = Arc::new(AtomicBool::new(false));

    super::super::run_download(async {
        let error = resolve_with("fixture.invalid".to_owned(), Arc::clone(&in_flight), |_| {
            Err(io::Error::other("synthetic lookup failure"))
        })
        .await;

        assert!(error.is_err());
        assert!(!in_flight.load(Ordering::Acquire));
        let panic = resolve_with("fixture.invalid".to_owned(), Arc::clone(&in_flight), |_| {
            panic!("synthetic lookup panic")
        })
        .await;

        assert!(panic.is_err());
        assert!(!in_flight.load(Ordering::Acquire));

        Ok(())
    })
    .unwrap();
}

#[test]
fn dropping_a_queued_lookup_prevents_its_os_call_and_releases_admission() {
    let in_flight = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let lookup_calls = Arc::clone(&calls);
    let lookup_gate = Arc::clone(&in_flight);
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();

    super::super::run_download(async move {
        let blocker = tokio::task::spawn_blocking(move || {
            started_sender.send(()).unwrap();
            let _ = release_receiver.recv_timeout(Duration::from_secs(5));
        });

        started_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let mut lookup = resolve_with("fixture.invalid".to_owned(), lookup_gate, move |_| {
            lookup_calls.fetch_add(1, Ordering::AcqRel);

            Ok(addresses())
        });

        assert!(matches!(
            futures_util::poll!(lookup.as_mut()),
            Poll::Pending
        ));
        drop(lookup);
        release_sender.send(()).unwrap();
        blocker.await.unwrap();

        Ok(())
    })
    .unwrap();
    wait_until_released(&in_flight);
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn unpolled_lookup_creates_no_work_and_numeric_addresses_need_no_runtime() {
    let in_flight = Arc::new(AtomicBool::new(false));
    let lookup = resolve_with("fixture.invalid".to_owned(), Arc::clone(&in_flight), |_| {
        panic!("an unpolled lookup must never run")
    });

    drop(lookup);
    assert!(!in_flight.load(Ordering::Acquire));

    for (name, address) in [("127.0.0.1", "127.0.0.1:0"), ("::1", "[::1]:0")] {
        let mut lookup = InstallerResolver.resolve(name.parse().unwrap());
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        let Poll::Ready(Ok(found)) = lookup.as_mut().poll(&mut context) else {
            panic!("numeric addresses must resolve immediately without an OS call");
        };

        assert_eq!(found.collect::<Vec<_>>(), vec![address.parse().unwrap()]);
    }
}
