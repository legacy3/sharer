use super::{
    Arc, FailureStage, HistoryEntry, Instant, Job, JobCallbacks, JobFailure, JobSource, PathBuf,
    PlatformStorage, ProcessedUpload, Result, UploadClient, UploadFailure, UploadPayload,
    UploadProgress, capture, clipboard, is_upload_cancelled, is_upload_timeout,
    preferred_upload_route,
};

pub(super) fn process_job(
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
    P: FnOnce(&mut HistoryEntry) -> Option<String>,
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
    let source_stage = job_source_stage(&source);
    let should_remove_exif =
        remove_exif && matches!(&source, JobSource::File(_) | JobSource::Clipboard);
    let payload_result = prepare_job_source(source);

    capture_complete();
    let (mut payload, save_directory) =
        payload_result.map_err(|error| JobFailure::new(error, source_stage))?;

    if should_remove_exif {
        payload
            .remove_exif()
            .map_err(|error| JobFailure::new(error, source_stage))?;
    }

    let (local_copy, capture_warning) = save_capture_copy(&mut payload, save_directory.as_deref());
    let captured_filename = payload.filename.clone();
    let captured_size = payload.len();
    let mut upload_failure = None;
    let (mut entry, clipboard_warning) = if let Some(upload) = upload {
        let progress = Arc::clone(upload_progress);
        let upload_result = (|| -> Result<_> {
            upload_progress.start();
            let route = preferred_upload_route(upload.require_tor)?;
            let client = UploadClient::for_target(&upload.uploader, &route)?;

            client.upload_with_progress_and_cancellation(
                payload,
                upload.lifetime_seconds,
                cancellation.as_ref(),
                move |transferred, total| {
                    progress.update(transferred, total);
                },
            )
        })();

        match upload_result {
            Ok(receipt) => {
                let entry = HistoryEntry::from_receipt(receipt, local_copy.as_deref());
                let clipboard_warning = clipboard::copy_link(&entry.link)
                    .err()
                    .map(|error| format!("link was not copied: {error:#}"));

                (entry, clipboard_warning)
            }

            Err(error) => {
                let Some(local_copy) = local_copy.as_deref() else {
                    return Err(upload_job_failure(error));
                };

                upload_failure = Some(UploadFailure {
                    stage: upload_failure_stage(&error),
                    message: format!("{error:#}"),
                });

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
    let history_warning = persist_history(&mut entry);

    Ok(ProcessedUpload {
        entry,
        history_warning,
        clipboard_warning,
        local_copy,
        capture_warning,
        upload_failure,
    })
}

const fn job_source_stage(source: &JobSource) -> FailureStage {
    match source {
        JobSource::File(_) | JobSource::Clipboard => FailureStage::Upload,
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

fn prepare_job_source(source: JobSource) -> Result<(UploadPayload, Option<PathBuf>)> {
    match source {
        JobSource::File(path) => Ok((UploadPayload::from_path(&path)?, None)),

        JobSource::Clipboard => Ok((clipboard::read_payload()?, None)),

        JobSource::Prepared {
            payload,
            save_directory,
        } => Ok((payload, save_directory)),

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
            Ok((payload, save_directory))
        }

        JobSource::Recording {
            region,
            stop,
            filename_stem,
            frames_per_second,
            max_seconds,
            capture_resolution,
            save_directory,
        } => Ok((
            crate::recording::capture_region(
                region,
                &stop,
                &filename_stem,
                crate::recording::RecordingSettings {
                    frames_per_second,
                    max_seconds,
                    capture_resolution,
                },
                Instant::now,
            )?,
            save_directory,
        )),
    }
}

fn persist_history_entry(entry: &mut HistoryEntry) -> Option<String> {
    match PlatformStorage::open() {
        Ok(storage) => match storage.insert_history(entry) {
            Ok(id) => {
                entry.id = id;
                None
            }

            Err(error) => Some(format!("history could not be saved: {error:#}")),
        },

        Err(error) => Some(format!("history could not be saved: {error:#}")),
    }
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
