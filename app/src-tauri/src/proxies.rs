//! Making proxies in the background, one clip at a time, after a copy.
//!
//! The queue is separate from the copy itself: the card is already safe and
//! can be ejected, and the next one can be copied while these still render.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use backup_engine::proxy::{
    can_convert, find_ffmpeg, make_proxy, ProxyCodec, ProxyPreset, ProxySize,
};
use backup_engine::{kind_of, scan, DestStatus, FileEntry, JobReport, JobSpec, MediaKind};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::notify;

/// How often the windows hear about the clip being encoded.
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

/// What the user asked for when starting a copy.
#[derive(Deserialize, Clone)]
pub struct ProxyRequest {
    pub codec: ProxyCodec,
    pub size: ProxySize,
    /// Clips to leave out, by their path on the source.
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Serialize, Clone)]
struct Failure {
    clip: String,
    error: String,
}

/// Where the queue has got to. `total` and `done` count since it was last idle.
#[derive(Serialize, Clone, Default)]
pub struct ProxyStatus {
    active: bool,
    total: usize,
    done: usize,
    /// File name of the clip being encoded, and how far along it is.
    current: Option<String>,
    fraction: f64,
    failed: Vec<Failure>,
    /// Camera raw clips that were not queued because FFmpeg cannot read them.
    skipped: usize,
    cancelled: bool,
}

#[derive(Default)]
struct Inner {
    tasks: VecDeque<(PathBuf, ProxyPreset)>,
    status: ProxyStatus,
    notify: bool,
}

#[derive(Default)]
pub struct ProxyQueue {
    inner: Mutex<Inner>,
    cancel: Arc<AtomicBool>,
}

fn leaf(path: &Path) -> String {
    path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// The clips of a finished job that should get a proxy, as they are on the
/// first destination, and how many camera raw clips had to be left out.
pub fn clips_for(
    spec: &JobSpec,
    files: &[FileEntry],
    report: &JobReport,
    request: &ProxyRequest,
) -> (Vec<PathBuf>, usize) {
    let Some(working_drive) = spec.destinations.first() else {
        return (Vec::new(), 0);
    };
    let mut skipped = 0;
    let clips = files
        .iter()
        .zip(&report.files)
        .filter(|(file, _)| kind_of(&file.rel_path) == MediaKind::Video)
        .filter(|(file, _)| !request.exclude.contains(&file.rel_path))
        // Only clips that actually made it to the working drive.
        .filter(|(_, result)| result.dests.first().is_some_and(|dest| dest.status != DestStatus::Failed))
        .filter_map(|(file, _)| {
            let clip = file.dest_in(working_drive);
            if can_convert(&clip) {
                Some(clip)
            } else {
                skipped += 1;
                None
            }
        })
        .collect();
    (clips, skipped)
}

/// Adds clips to the queue and starts the worker if it is idle.
pub fn enqueue(app: &AppHandle, clips: Vec<PathBuf>, skipped: usize, preset: ProxyPreset, notify: bool) {
    let queue = app.state::<ProxyQueue>();
    let start = {
        let mut inner = queue.inner.lock().unwrap();
        let idle = !inner.status.active;
        if idle {
            // A new batch: forget how the last one went.
            inner.status = ProxyStatus::default();
            queue.cancel.store(false, Ordering::SeqCst);
        }
        inner.status.total += clips.len();
        inner.status.skipped += skipped;
        inner.notify = notify;
        inner.tasks.extend(clips.into_iter().map(|clip| (clip, preset)));
        let start = idle && !inner.tasks.is_empty();
        inner.status.active = inner.status.active || start;
        let _ = app.emit("proxy-status", &inner.status);
        start
    };
    if start {
        let app = app.clone();
        std::thread::spawn(move || work(app));
    }
}

fn work(app: AppHandle) {
    let queue = app.state::<ProxyQueue>();
    let program = find_ffmpeg();
    let last_status = Mutex::new(Instant::now());
    loop {
        let next = {
            let mut inner = queue.inner.lock().unwrap();
            let next = inner.tasks.pop_front();
            match &next {
                Some((clip, _)) => {
                    inner.status.current = Some(leaf(clip));
                    inner.status.fraction = 0.0;
                }
                None => {
                    inner.status.active = false;
                    inner.status.current = None;
                }
            }
            let _ = app.emit("proxy-status", &inner.status);
            next
        };
        let Some((clip, preset)) = next else { break };

        let outcome = match &program {
            Some(program) => make_proxy(program, &clip, preset, &queue.cancel, &|fraction| {
                let mut last = last_status.lock().unwrap();
                if last.elapsed() < STATUS_INTERVAL {
                    return;
                }
                *last = Instant::now();
                let mut inner = queue.inner.lock().unwrap();
                inner.status.fraction = fraction;
                let _ = app.emit("proxy-status", &inner.status);
            }),
            None => Err(backup_engine::Error::Proxy("FFmpeg non trovato su questo computer".into())),
        };

        let mut inner = queue.inner.lock().unwrap();
        match outcome {
            Ok(_) => inner.status.done += 1,
            Err(backup_engine::Error::Cancelled) => {
                // Stopped by the user: drop what was still waiting.
                inner.tasks.clear();
                inner.status.cancelled = true;
                queue.cancel.store(false, Ordering::SeqCst);
            }
            Err(err) => inner.status.failed.push(Failure {
                clip: leaf(&clip),
                error: err.to_string(),
            }),
        }
    }

    let (status, wanted) = {
        let inner = queue.inner.lock().unwrap();
        (inner.status.clone(), inner.notify)
    };
    if wanted && !status.cancelled {
        if status.failed.is_empty() {
            notify::send(&app, "Proxy pronti", &format!("{} clip convertiti.", status.done));
        } else {
            notify::send(
                &app,
                "Proxy non tutti riusciti",
                &format!("{} convertiti, {} no. Apri BackupTool per i dettagli.", status.done, status.failed.len()),
            );
        }
    }
}

/// Stops the encoder before the app goes away: FFmpeg is a separate program
/// and would otherwise carry on by itself.
pub fn stop_for_exit(app: &AppHandle) {
    let queue = app.state::<ProxyQueue>();
    queue.inner.lock().unwrap().tasks.clear();
    queue.cancel.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(3);
    while queue.inner.lock().unwrap().status.active && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether there is an FFmpeg to make proxies with.
#[tauri::command(async)]
pub fn proxy_available() -> bool {
    find_ffmpeg().is_some()
}

#[tauri::command]
pub fn proxy_status(queue: State<ProxyQueue>) -> ProxyStatus {
    queue.inner.lock().unwrap().status.clone()
}

/// Stops the clip being encoded and drops the ones waiting.
#[tauri::command]
pub fn cancel_proxies(queue: State<ProxyQueue>) {
    if queue.inner.lock().unwrap().status.active {
        queue.cancel.store(true, Ordering::SeqCst);
    }
}

/// A clip on the source, for choosing which ones get a proxy.
#[derive(Serialize)]
pub struct VideoFile {
    rel_path: String,
    size: u64,
    /// False for camera raw formats FFmpeg cannot read.
    convertible: bool,
}

#[tauri::command(async)]
pub fn list_videos(source: PathBuf) -> Result<Vec<VideoFile>, String> {
    let files = scan(&source, |_| {}).map_err(|err| err.to_string())?;
    Ok(files
        .into_iter()
        .filter(|file| kind_of(&file.rel_path) == MediaKind::Video)
        .map(|file| VideoFile {
            convertible: can_convert(Path::new(&file.rel_path)),
            rel_path: file.rel_path,
            size: file.size,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use backup_engine::{CopyOptions, DestOutcome, FileResult};

    fn file(rel: &str, dest: &str) -> FileEntry {
        FileEntry {
            rel_path: rel.into(),
            size: 10,
            modified: None,
            dest: Some(dest.into()),
        }
    }

    fn result(rel: &str, first_dest: DestStatus) -> FileResult {
        let outcome = |status| DestOutcome {
            status,
            verified: status != DestStatus::Failed,
            error: None,
        };
        FileResult {
            rel_path: rel.into(),
            size: 10,
            hash: Some("0".into()),
            verified_at: None,
            dests: vec![outcome(first_dest), outcome(DestStatus::Copied)],
            error: None,
        }
    }

    #[test]
    fn proxies_are_for_the_clips_on_the_working_drive_minus_what_was_left_out() {
        let spec = JobSpec {
            source: PathBuf::from("E:/"),
            destinations: vec![PathBuf::from("F:/Lavoro"), PathBuf::from("G:/Backup")],
            options: CopyOptions::default(),
        };
        let files = [
            file("DCIM/A001.MOV", "Shoot/Footage/Z6/A001.MOV"),
            file("DCIM/A002.MOV", "Shoot/Footage/Z6/A002.MOV"),
            file("DCIM/A003.MOV", "Shoot/Footage/Z6/A003.MOV"),
            file("DCIM/A004.BRAW", "Shoot/Footage/Z6/A004.BRAW"),
            file("DCIM/P001.NEF", "Shoot/Photo/Z6/P001.NEF"),
            file("DCIM/A001.XML", "Shoot/Footage/Z6/A001.XML"),
        ];
        let report = JobReport {
            files: vec![
                result("DCIM/A001.MOV", DestStatus::Copied),
                result("DCIM/A002.MOV", DestStatus::Copied),
                // Did not reach the working drive: nothing to make a proxy from.
                result("DCIM/A003.MOV", DestStatus::Failed),
                result("DCIM/A004.BRAW", DestStatus::Copied),
                result("DCIM/P001.NEF", DestStatus::Copied),
                result("DCIM/A001.XML", DestStatus::AlreadyPresent),
            ],
            total_files: 6,
            total_bytes: 60,
            failed_files: 1,
            cancelled: false,
            safe_to_format: false,
            duration_ms: 0,
        };
        let request = ProxyRequest {
            codec: ProxyCodec::ProresProxy,
            size: ProxySize::Hd,
            exclude: vec!["DCIM/A002.MOV".into()],
        };

        let (clips, skipped) = clips_for(&spec, &files, &report, &request);

        assert_eq!(clips, [PathBuf::from("F:/Lavoro/Shoot/Footage/Z6/A001.MOV")]);
        // The Blackmagic raw clip is counted, not queued.
        assert_eq!(skipped, 1);
    }
}
