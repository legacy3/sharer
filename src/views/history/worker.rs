//! One bounded background connection for history queries and local-path reconciliation.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use sharer::{
    history::{HistoryEntry, HistoryFilter, HistorySort},
    storage::{HistoryLocalPathReconciliation, PlatformStorage},
};

use super::{PAGE_SIZE, history_search_matches, page_count};

struct Request {
    generation: u64,
    page: usize,
    sort: HistorySort,
    query: String,
    filter: HistoryFilter,
}

pub(crate) struct Reply {
    pub generation: u64,
    pub event: HistoryEvent,
}

pub(crate) enum HistoryEvent {
    Progress {
        cleared: Vec<(i64, PathBuf)>,
        checked: u64,
    },
    Complete(Result<HistoryPage>),
}

pub(crate) struct HistoryPage {
    pub page: usize,
    pub total: u64,
    pub entries: Vec<HistoryEntry>,
    pub reconciliation: HistoryLocalPathReconciliation,
}

pub(crate) struct HistoryLoader {
    requests: SyncSender<Request>,
    replies: Receiver<Reply>,
    generation: Arc<AtomicU64>,
    pending: Option<Request>,
    awaiting: bool,
    disconnected: bool,
}

impl HistoryLoader {
    pub fn new(storage: &PlatformStorage) -> Result<Self> {
        let directory = storage.directory().to_path_buf();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        let (request_tx, request_rx) = sync_channel(1);
        let (reply_tx, reply_rx) = sync_channel(1);

        thread::Builder::new()
            .name("sharer-history".to_owned())
            .spawn(move || run_worker(&directory, &request_rx, &reply_tx, &worker_generation))
            .context("failed to start the history worker")?;

        Ok(Self {
            requests: request_tx,
            replies: reply_rx,
            generation,
            pending: None,
            awaiting: false,
            disconnected: false,
        })
    }

    pub fn submit(
        &mut self,
        page: usize,
        sort: HistorySort,
        query: String,
        filter: HistoryFilter,
    ) -> Result<u64> {
        anyhow::ensure!(!self.disconnected, "the history worker has stopped");
        let generation = self.advance_generation()?;

        self.pending = Some(Request {
            generation,
            page,
            sort,
            query,
            filter,
        });
        self.awaiting = true;
        self.flush_pending()?;

        Ok(generation)
    }

    pub fn cancel(&mut self) -> Result<u64> {
        let generation = self.advance_generation()?;

        self.pending = None;
        self.awaiting = false;

        Ok(generation)
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    pub fn poll(&mut self) -> Option<Reply> {
        let send_error = self.flush_pending().err();

        // Drain committed notifications before reporting a disconnected worker.
        match self.replies.try_recv() {
            Ok(reply) => self.accept_reply(reply),

            Err(TryRecvError::Empty) if send_error.is_none() => None,

            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                self.disconnected = true;

                if !self.awaiting {
                    return None;
                }

                self.awaiting = false;

                Some(Reply {
                    generation: self.generation.load(Ordering::Acquire),
                    event: HistoryEvent::Complete(Err(
                        send_error.unwrap_or_else(|| anyhow!("the history worker has stopped"))
                    )),
                })
            }
        }
    }

    #[cfg(test)]
    pub(super) fn wait_for_reply(&mut self, timeout: Duration) -> Result<Reply> {
        let deadline = std::time::Instant::now() + timeout;

        loop {
            if let Some(reply) = self.poll() {
                return Ok(reply);
            }

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());

            anyhow::ensure!(!remaining.is_zero(), "history worker reply timed out");

            match self
                .replies
                .recv_timeout(remaining.min(Duration::from_millis(10)))
            {
                Ok(reply) => {
                    if let Some(reply) = self.accept_reply(reply) {
                        return Ok(reply);
                    }
                }

                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}

                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("the history worker has stopped"));
                }
            }
        }
    }

    fn accept_reply(&mut self, reply: Reply) -> Option<Reply> {
        if matches!(reply.event, HistoryEvent::Complete(_)) {
            if reply.generation != self.generation.load(Ordering::Acquire) {
                return None;
            }

            self.awaiting = false;
        }

        Some(reply)
    }

    fn advance_generation(&self) -> Result<u64> {
        self.generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map(|previous| previous + 1)
            .map_err(|value| anyhow!("history request generation exhausted at {value}"))
    }

    fn flush_pending(&mut self) -> Result<()> {
        let Some(request) = self.pending.take() else {
            return Ok(());
        };

        match self.requests.try_send(request) {
            Ok(()) => Ok(()),

            Err(TrySendError::Full(request)) => {
                self.pending = Some(request);

                Ok(())
            }

            Err(TrySendError::Disconnected(_)) => {
                self.disconnected = true;

                Err(anyhow!("the history worker has stopped"))
            }
        }
    }
}

impl Drop for HistoryLoader {
    fn drop(&mut self) {
        // Channel receivers are dropped immediately afterward, waking any blocked send.
        self.generation.store(u64::MAX, Ordering::Release);
    }
}

fn run_worker(
    directory: &Path,
    requests: &Receiver<Request>,
    replies: &SyncSender<Reply>,
    generation: &AtomicU64,
) {
    let mut storage = None;

    while let Ok(request) = requests.recv() {
        if is_cancelled(&request, generation) {
            continue;
        }

        let result = worker_connection(directory, &mut storage)
            .and_then(|storage| load_page(storage, &request, replies, generation));

        if is_cancelled(&request, generation) {
            continue;
        }

        let complete = match result {
            Ok(Some(page)) => Ok(page),
            Ok(None) => continue,
            Err(error) => Err(error),
        };

        if replies
            .send(Reply {
                generation: request.generation,
                event: HistoryEvent::Complete(complete),
            })
            .is_err()
        {
            break;
        }
    }
}

fn worker_connection<'a>(
    directory: &Path,
    storage: &'a mut Option<PlatformStorage>,
) -> Result<&'a PlatformStorage> {
    if storage.is_none() {
        let connection = PlatformStorage::open_in(directory)
            .context("could not open the history worker database")?;

        connection.set_lock_wait_timeout(Duration::from_millis(50))?;
        *storage = Some(connection);
    }

    // A failed open leaves the slot empty so a later request can recover.
    storage
        .as_ref()
        .context("history worker connection unavailable")
}

fn is_cancelled(request: &Request, generation: &AtomicU64) -> bool {
    request.generation != generation.load(Ordering::Acquire)
}

fn load_page(
    storage: &PlatformStorage,
    request: &Request,
    replies: &SyncSender<Reply>,
    generation: &AtomicU64,
) -> Result<Option<HistoryPage>> {
    let mut requested_page = request.page;
    let mut reconciliation = HistoryLocalPathReconciliation::default();
    let mut checked = 0;

    loop {
        if is_cancelled(request, generation) {
            return Ok(None);
        }

        let mut total = storage.history_count_filtered(&request.query, request.filter)?;

        if is_cancelled(request, generation) {
            return Ok(None);
        }

        let page = requested_page.min(page_count(total).saturating_sub(1));
        let offset = page
            .checked_mul(PAGE_SIZE)
            .context("history page offset overflowed")?;
        let entries = storage.history_page_filtered(
            offset,
            PAGE_SIZE,
            request.sort,
            &request.query,
            request.filter,
        )?;

        if is_cancelled(request, generation) {
            return Ok(None);
        }

        let Some((entries, report)) =
            fill_page(storage, entries, request, replies, generation, &mut checked)?
        else {
            return Ok(None);
        };

        reconciliation.cleared += report.cleared;
        reconciliation.unavailable += report.unavailable;
        reconciliation.not_regular += report.not_regular;

        if report.cleared != 0 && !request.query.trim().is_empty() {
            if is_cancelled(request, generation) {
                return Ok(None);
            }

            total = storage.history_count_filtered(&request.query, request.filter)?;
        }

        if is_cancelled(request, generation) {
            return Ok(None);
        }

        if page < page_count(total) {
            return Ok(Some(HistoryPage {
                page,
                total,
                entries,
                reconciliation,
            }));
        }

        // Reconciliation may remove the selected filtered page; restart once at the first page.
        requested_page = 0;
    }
}

fn fill_page(
    storage: &PlatformStorage,
    mut entries: Vec<HistoryEntry>,
    request: &Request,
    replies: &SyncSender<Reply>,
    generation: &AtomicU64,
    checked: &mut u64,
) -> Result<Option<(Vec<HistoryEntry>, HistoryLocalPathReconciliation)>> {
    let mut retained = Vec::with_capacity(PAGE_SIZE);
    let mut report = HistoryLocalPathReconciliation::default();

    loop {
        if is_cancelled(request, generation) {
            return Ok(None);
        }

        let Some(cursor) = entries.last().cloned() else {
            break;
        };
        let reached_end = entries.len() < PAGE_SIZE - retained.len();
        let original_paths = entries
            .iter()
            .filter(|entry| !entry.local_path.is_empty())
            .map(|entry| (entry.id, PathBuf::from(&entry.local_path)))
            .collect::<Vec<_>>();
        let outcome = storage
            .reconcile_missing_history_local_paths_cancellable(&mut entries, || {
                is_cancelled(request, generation)
            })?;
        let cleared = original_paths
            .into_iter()
            .filter(|(id, _)| {
                entries
                    .iter()
                    .any(|entry| entry.id == *id && entry.local_path.is_empty())
            })
            .collect::<Vec<_>>();

        // A stale request must still report every path that its transaction actually cleared.
        if !cleared.is_empty() || !is_cancelled(request, generation) {
            *checked += u64::try_from(entries.len()).unwrap_or_default();
            replies
                .send(Reply {
                    generation: request.generation,
                    event: HistoryEvent::Progress {
                        cleared,
                        checked: *checked,
                    },
                })
                .context("the history progress receiver has closed")?;
        }

        if is_cancelled(request, generation) {
            return Ok(None);
        }

        report.cleared += outcome.cleared;
        report.unavailable += outcome.unavailable;
        report.not_regular += outcome.not_regular;
        retained.extend(
            entries
                .drain(..)
                .filter(|entry| history_search_matches(entry, &request.query)),
        );

        if retained.len() == PAGE_SIZE || reached_end {
            break;
        }

        entries = storage.history_page_filtered_after(
            PAGE_SIZE - retained.len(),
            request.sort,
            &request.query,
            request.filter,
            &cursor,
        )?;
    }

    Ok(Some((retained, report)))
}
