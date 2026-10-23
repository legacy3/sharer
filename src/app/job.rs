//! Background upload worker and capture preparation.

use anyhow::Context as _;
use sharer::upload::UploadReceipt;

use super::{
    Arc, FailureStage, HistoryEntry, Instant, Job, JobCallbacks, JobFailure, JobResult, JobSource,
    PathBuf, PlatformStorage, ProcessedUpload, Receiver, Result, Sender, UploadCancellation,
    UploadClient, UploadOptions, UploadPayload, UploadProgress, capture, clipboard,
    is_upload_cancelled, is_upload_timeout, mpsc, preferred_upload_route,
};

pub(super) fn spawn_worker(
    job_rx: Receiver<Job>,
    result_tx: Sender<JobResult>,
    upload_progress: Arc<UploadProgress>,
    require_tor: bool,
) -> Result<()> {
    std::thread::Builder::new()
        .name("sharer-upload".to_owned())
        .spawn(move || {
            while let Ok(job) = receive_job(&job_rx, require_tor) {
                upload_progress.reset();
                let restore_window_after_capture = job.restore_window_after_capture;
                let result = process_job(job, &upload_progress, || {
                    let _ = result_tx.send(JobResult::CaptureComplete {
                        restore_window: restore_window_after_capture,
                    });
                });

                upload_progress.finish();
                let event = match result {
                    Ok(receipt) => JobResult::Success(Box::new(receipt)),

                    Err(error) => JobResult::Error {
                        message: format!("{:#}", error.error),
                        stage: error.stage,
                    },
                };
                let _ = result_tx.send(event);
            }
        })
        .context("failed to start upload worker")?;

    Ok(())
}

pub(super) fn receive_job(
    job_rx: &Receiver<Job>,
    require_tor: bool,
) -> Result<Job, mpsc::RecvError> {
    let mut job = job_rx.recv()?;

    // The command-line requirement remains in force after Settings change or the UI is recreated.
    if let Some(upload) = job.upload.as_mut() {
        upload.require_tor |= require_tor;
    }

    Ok(job)
}

fn process_job(
    job: Job,
    upload_progress: &Arc<UploadProgress>,
    capture_complete: impl FnOnce(),
) -> std::result::Result<ProcessedUpload, JobFailure> {
    process_job_with_history(
        job,
        upload_progress,
        JobCallbacks {
            persist_history: persist_history_entry,
            capture_complete,
        },
    )
}

pub(super) fn process_job_with_history<P, C>(
    job: Job,
    upload_progress: &Arc<UploadProgress>,
    callbacks: JobCallbacks<P, C>,
) -> std::result::Result<ProcessedUpload, JobFailure>
where
    P: FnOnce(&mut HistoryEntry, Option<&HistoryEntry>) -> Option<String>,
    C: FnOnce(),
{
    let JobCallbacks {
        persist_history,
        capture_complete,
    } = callbacks;
    let Job {
        source,
        upload,
        remove_exif,
        cancellation,
        restore_window_after_capture: _,
    } = job;
    let retained = match &source {
        JobSource::History(entry) => Some((entry.clone(), true)),
        JobSource::ExistingLocal(entry) => Some((entry.clone(), false)),
        _ => None,
    };
    let prepared = match prepare_upload_source(source, remove_exif, capture_complete) {
        Ok(prepared) => prepared,

        Err(failure) => {
            let Some((mut entry, persisted)) = retained.filter(|(entry, _)| {
                entry.link.is_empty() && std::path::Path::new(&entry.local_path).is_file()
            }) else {
                return Err(failure);
            };
            let history_warning = if persisted {
                None
            } else {
                persist_history(&mut entry, None)
            };

            return Ok(retained_artifact_failure(entry, failure, history_warning));
        }
    };

    process_prepared_upload(
        prepared,
        upload,
        cancellation.as_ref(),
        upload_progress,
        persist_history,
    )
}

fn process_prepared_upload<P>(
    prepared: PreparedSource,
    upload: Option<UploadOptions>,
    cancellation: &UploadCancellation,
    upload_progress: &Arc<UploadProgress>,
    persist_history: P,
) -> std::result::Result<ProcessedUpload, JobFailure>
where
    P: FnOnce(&mut HistoryEntry, Option<&HistoryEntry>) -> Option<String>,
{
    let PreparedSource {
        mut payload,
        save_directory,
        warning: recording_warning,
        history_entry,
        retained_local_path,
    } = prepared;

    let (local_copy, save_warning) = if let Some(original) = history_entry.as_ref() {
        (Some(PathBuf::from(&original.local_path)), None)
    } else if retained_local_path.is_some() {
        (retained_local_path, None)
    } else {
        save_capture_copy(&mut payload, save_directory.as_deref())
    };
    let capture_warning = match (recording_warning, save_warning) {
        (Some(recording), Some(save)) => Some(format!("{recording}; {save}")),
        (recording, save) => recording.or(save),
    };
    let captured_filename = payload.filename.clone();
    let captured_size = payload.len();
    let mut upload_failure = None;
    let mut clipboard_copied = false;
    let (mut entry, clipboard_warning) = if let Some(upload) = upload {
        let upload_result = upload_payload(payload, &upload, cancellation, upload_progress);

        match upload_result {
            Ok(receipt) => {
                let entry = HistoryEntry::from_receipt(receipt, local_copy.as_deref());
                let (copied, clipboard_warning) = copy_uploaded_link(&entry.link, upload.copy_link);

                clipboard_copied = copied;

                (entry, clipboard_warning)
            }

            Err(error) => {
                if let Some(entry) = history_entry {
                    return Ok(ProcessedUpload {
                        entry,
                        history_warning: None,
                        clipboard_warning: None,
                        clipboard_copied: false,
                        local_copy,
                        capture_warning,
                        upload_failure: Some(upload_job_failure(error)),
                    });
                }

                let Some(local_copy) = local_copy.as_deref() else {
                    return Err(upload_job_failure(error));
                };

                upload_failure = Some(upload_job_failure(error));

                (
                    HistoryEntry::local(captured_filename, captured_size, local_copy),
                    None,
                )
            }
        }
    } else {
        let Some(local_copy) = local_copy.as_deref() else {
            let error = anyhow::anyhow!(
                "{}",
                capture_warning
                    .as_deref()
                    .unwrap_or("capture was not saved because local copies are disabled")
            );

            return Err(JobFailure::new(error, FailureStage::Save));
        };

        (
            HistoryEntry::local(captured_filename, captured_size, local_copy),
            None,
        )
    };
    let history_warning = persist_history(&mut entry, history_entry.as_ref());

    Ok(ProcessedUpload {
        entry,
        history_warning,
        clipboard_warning,
        clipboard_copied,
        local_copy,
        capture_warning,
        upload_failure,
    })
}

fn retained_artifact_failure(
    entry: HistoryEntry,
    failure: JobFailure,
    history_warning: Option<String>,
) -> ProcessedUpload {
    ProcessedUpload {
        local_copy: Some(PathBuf::from(&entry.local_path)),
        entry,
        history_warning,
        clipboard_warning: None,
        clipboard_copied: false,
        capture_warning: None,
        upload_failure: Some(failure),
    }
}

fn upload_payload(
    payload: UploadPayload,
    upload: &UploadOptions,
    cancellation: &UploadCancellation,
    progress: &Arc<UploadProgress>,
) -> Result<UploadReceipt> {
    progress.start();
    let route = preferred_upload_route(upload.require_tor)?;
    let client = UploadClient::for_target(&upload.uploader, &route)?;
    let progress = Arc::clone(progress);

    client.upload_with_progress_and_cancellation(
        payload,
        upload.lifetime_seconds,
        cancellation,
        move |transferred, total| {
            progress.update(transferred, total);
        },
    )
}

fn copy_uploaded_link(link: &str, requested: bool) -> (bool, Option<String>) {
    if !requested {
        return (false, None);
    }

    match clipboard::copy_link(link) {
        Ok(()) => (true, None),
        Err(error) => (false, Some(format!("link was not copied: {error:#}"))),
    }
}

const fn job_source_stage(source: &JobSource) -> FailureStage {
    match source {
        JobSource::File(_)
        | JobSource::ExistingLocal(_)
        | JobSource::History(_)
        | JobSource::Clipboard => FailureStage::Upload,
        JobSource::Prepared { .. }
        | JobSource::RegionCapture { .. }
        | JobSource::Recording { .. } => FailureStage::Capture,
    }
}

fn save_capture_copy(
    payload: &mut UploadPayload,
    save_directory: Option<&std::path::Path>,
) -> (Option<PathBuf>, Option<String>) {
    let Some(directory) = save_directory else {
        return (None, None);
    };
    let directory = sharer::storage::monthly_capture_directory(directory);

    match payload.save_generated_copy(&directory) {
        Ok(path) => (Some(path), None),

        Err(error) => (
            None,
            Some(format!("local copy could not be saved: {error:#}")),
        ),
    }
}

struct PreparedSource {
    payload: UploadPayload,
    save_directory: Option<PathBuf>,
    warning: Option<String>,
    history_entry: Option<HistoryEntry>,
    retained_local_path: Option<PathBuf>,
}

impl PreparedSource {
    fn new(payload: UploadPayload, save_directory: Option<PathBuf>) -> Self {
        Self {
            payload,
            save_directory,
            warning: None,
            history_entry: None,
            retained_local_path: None,
        }
    }
}

fn prepare_upload_source(
    source: JobSource,
    remove_exif: bool,
    capture_complete: impl FnOnce(),
) -> std::result::Result<PreparedSource, JobFailure> {
    let stage = job_source_stage(&source);
    let should_remove_exif = remove_exif
        && matches!(
            &source,
            JobSource::File(_)
                | JobSource::ExistingLocal(_)
                | JobSource::History(_)
                | JobSource::Clipboard
        );
    let prepared = prepare_job_source(source);

    capture_complete();
    let mut prepared = prepared.map_err(|error| JobFailure::new(error, stage))?;

    if should_remove_exif {
        prepared
            .payload
            .remove_exif()
            .map_err(|error| JobFailure::new(error, stage))?;
    }

    Ok(prepared)
}

fn prepare_job_source(source: JobSource) -> Result<PreparedSource> {
    match source {
        JobSource::File(path) => Ok(PreparedSource::new(UploadPayload::from_path(&path)?, None)),

        JobSource::ExistingLocal(entry) => {
            anyhow::ensure!(
                entry.link.is_empty() && !entry.local_path.is_empty(),
                "only local-only artifacts can be uploaded"
            );
            let path = PathBuf::from(entry.local_path);
            let mut prepared = PreparedSource::new(UploadPayload::from_path(&path)?, None);

            prepared.retained_local_path = Some(path);

            Ok(prepared)
        }

        JobSource::History(entry) => {
            anyhow::ensure!(
                entry.link.is_empty() && !entry.local_path.is_empty(),
                "only local-only history entries can be uploaded"
            );
            let payload = UploadPayload::from_path(std::path::Path::new(&entry.local_path))?;

            Ok(PreparedSource {
                payload,
                save_directory: None,
                warning: None,
                history_entry: Some(entry),
                retained_local_path: None,
            })
        }

        JobSource::Clipboard => Ok(PreparedSource::new(clipboard::read_payload()?, None)),

        JobSource::Prepared {
            payload,
            save_directory,
        } => Ok(PreparedSource::new(payload, save_directory)),

        JobSource::RegionCapture {
            selection,
            filename_stem,
            capture_resolution,
            resize_quality,
            save_directory,
        } => {
            let mut payload =
                capture::capture_selected_region(&selection, capture_resolution, resize_quality)?;

            payload.filename = capture::named_capture_filename(&filename_stem, &payload.filename);
            Ok(PreparedSource::new(payload, save_directory))
        }

        JobSource::Recording {
            region,
            stop,
            filename_stem,
            frames_per_second,
            max_seconds,
            max_gib,
            format,
            capture_resolution,
            save_directory,
        } => {
            sharer::validate_recording_gib(max_gib)?;

            let recorded = crate::recording::capture_region(
                region,
                &stop,
                &filename_stem,
                crate::recording::RecordingSettings {
                    frames_per_second,
                    max_seconds,
                    format,
                    max_bytes: u64::from(max_gib) * 1024 * 1024 * 1024,
                    capture_resolution,
                },
                Instant::now,
            )?;

            Ok(PreparedSource {
                payload: recorded.payload,
                save_directory,
                warning: recorded.warning,
                history_entry: None,
                retained_local_path: None,
            })
        }
    }
}

fn persist_history_entry(
    entry: &mut HistoryEntry,
    original: Option<&HistoryEntry>,
) -> Option<String> {
    let result = PlatformStorage::open().and_then(|storage| {
        if let Some(original) = original {
            storage.complete_history_upload(original, entry)
        } else {
            storage.insert_history(entry).map(|id| entry.id = id)
        }
    });

    result
        .err()
        .map(|error| format!("history could not be saved: {error:#}"))
}

fn upload_failure_stage(error: &anyhow::Error) -> FailureStage {
    if is_upload_cancelled(error.as_ref()) {
        FailureStage::UploadCancelled
    } else if is_upload_timeout(error.as_ref()) {
        FailureStage::UploadTimedOut
    } else {
        FailureStage::Upload
    }
}

fn upload_job_failure(error: anyhow::Error) -> JobFailure {
    let stage = upload_failure_stage(&error);

    JobFailure::new(error, stage)
}
