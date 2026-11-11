//! A job: one source copied to one or more destinations, file by file.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::copy::{copy_file, CopyOptions, FileResult, Phase, VerifyMode};
use crate::error::{Error, IoContext, Result};
use crate::scan::FileEntry;

/// Progress events are throttled to this interval; a fast drive would
/// otherwise emit hundreds per second.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub source: PathBuf,
    /// Root folders; each file keeps its path relative to the source.
    pub destinations: Vec<PathBuf>,
    pub options: CopyOptions,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    FileStarted {
        index: usize,
        rel_path: String,
        size: u64,
    },
    Progress {
        index: usize,
        phase: Phase,
        /// Source bytes copied so far across the whole job.
        bytes_done: u64,
        bytes_total: u64,
        /// Copy and verification passes together, for a single progress bar.
        work_done: u64,
        work_total: u64,
    },
    FileFinished {
        index: usize,
        result: FileResult,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct JobReport {
    /// Results for the files that were processed, in scan order.
    pub files: Vec<FileResult>,
    pub total_files: usize,
    pub total_bytes: u64,
    pub failed_files: usize,
    pub cancelled: bool,
    /// Every file on the source has a verified copy on every destination.
    pub safe_to_format: bool,
    pub duration_ms: u64,
}

/// Checks a job before a single byte is written: the source is never a write
/// target, and every destination has room for what it is missing.
pub fn preflight(spec: &JobSpec, files: &[FileEntry]) -> Result<()> {
    if spec.destinations.is_empty() {
        return Err(Error::InvalidJob("no destination selected".into()));
    }
    // Two files landing on one path would silently overwrite each other.
    // Compared without case: Windows and macOS volumes usually ignore it.
    let mut targets = std::collections::HashSet::new();
    for file in files {
        if !targets.insert(file.dest_rel().to_lowercase()) {
            return Err(Error::InvalidJob(format!(
                "more than one file would be written to {}",
                file.dest_rel()
            )));
        }
    }
    let source = fs::canonicalize(&spec.source).at(&spec.source)?;
    if !source.is_dir() {
        return Err(Error::InvalidJob(format!("{} is not a folder", spec.source.display())));
    }
    let mut seen = Vec::new();
    for dest in &spec.destinations {
        fs::create_dir_all(dest).at(dest)?;
        let resolved = fs::canonicalize(dest).at(dest)?;
        if resolved.starts_with(&source) || source.starts_with(&resolved) {
            return Err(Error::InvalidJob(format!(
                "{} and the source contain each other; the source must never be written to",
                dest.display()
            )));
        }
        if seen.contains(&resolved) {
            return Err(Error::InvalidJob(format!("{} is listed twice", dest.display())));
        }

        let needed: u64 = files
            .iter()
            .filter(|file| !file.dest_in(dest).exists())
            .map(|file| file.size)
            .sum();
        let available = fs4::available_space(&resolved).at(dest)?;
        if needed > available {
            return Err(Error::NotEnoughSpace {
                dest: dest.clone(),
                needed,
                available,
            });
        }
        seen.push(resolved);
    }
    Ok(())
}

/// Runs the job over `files` (the result of scanning `spec.source`).
///
/// A file that fails does not stop the job; it is recorded in the report.
/// Setting `cancel` stops after the current chunk and removes partial files;
/// the report then covers the files completed so far.
pub fn run_job(
    spec: &JobSpec,
    files: &[FileEntry],
    cancel: &AtomicBool,
    on_event: &(dyn Fn(JobEvent) + Sync),
) -> Result<JobReport> {
    preflight(spec, files)?;
    let started = Instant::now();
    let verifying = spec.options.verify != VerifyMode::None;
    let passes: u64 = if verifying { 2 } else { 1 };
    let bytes_total: u64 = files.iter().map(|file| file.size).sum();
    let work_total = bytes_total * passes;

    let mut results = Vec::with_capacity(files.len());
    let mut finished_bytes = 0u64;
    let mut cancelled = false;
    let last_progress: Mutex<Option<Instant>> = Mutex::new(None);

    for (index, entry) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        on_event(JobEvent::FileStarted {
            index,
            rel_path: entry.rel_path.clone(),
            size: entry.size,
        });

        let base = finished_bytes;
        let progress = |phase: Phase, file_bytes: u64| {
            {
                let mut last = last_progress.lock().unwrap();
                if last.is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL) {
                    return;
                }
                *last = Some(Instant::now());
            }
            let (copied, work) = match phase {
                Phase::Copying => (file_bytes, file_bytes),
                Phase::Verifying => (entry.size, entry.size + file_bytes),
            };
            on_event(JobEvent::Progress {
                index,
                phase,
                bytes_done: base + copied,
                bytes_total,
                work_done: base * passes + work,
                work_total,
            });
        };

        match copy_file(&spec.source, entry, &spec.destinations, &spec.options, cancel, &progress) {
            Ok(result) => {
                finished_bytes += entry.size;
                on_event(JobEvent::FileFinished {
                    index,
                    result: result.clone(),
                });
                results.push(result);
            }
            Err(Error::Cancelled) => {
                cancelled = true;
                break;
            }
            Err(err) => return Err(err),
        }
    }

    let failed_files = results
        .iter()
        .filter(|r| r.error.is_some() || r.dests.iter().any(|d| d.error.is_some()))
        .count();
    let safe_to_format = !cancelled
        && verifying
        && !files.is_empty()
        && results.len() == files.len()
        && results.iter().all(FileResult::is_safe);

    Ok(JobReport {
        files: results,
        total_files: files.len(),
        total_bytes: bytes_total,
        failed_files,
        cancelled,
        safe_to_format,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}
