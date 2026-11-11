//! Checking backups that already exist against their ASC MHL histories.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use backup_engine::mhl::Creator;
use backup_engine::report::the_files;
use backup_engine::verify::{
    find_histories, summarize, verify_history, VerifyEvent, VerifyReport,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::{notify, JobState};

/// A history found under the folder the user picked.
#[derive(Serialize)]
pub struct HistoryRow {
    root: String,
    generations: usize,
    files: usize,
    bytes: u64,
    /// Why this history cannot be checked (its seal is broken, for one).
    error: Option<String>,
}

/// Looks for backups with a history in `folder` and below. Reads no footage.
#[tauri::command(async)]
pub fn find_backups(folder: PathBuf) -> Vec<HistoryRow> {
    find_histories(&folder)
        .into_iter()
        .map(|root| {
            let path = root.to_string_lossy().into_owned();
            match summarize(&root) {
                Ok(info) => HistoryRow {
                    root: path,
                    generations: info.generations,
                    files: info.files,
                    bytes: info.bytes,
                    error: None,
                },
                Err(err) => HistoryRow {
                    root: path,
                    generations: 0,
                    files: 0,
                    bytes: 0,
                    error: Some(err.to_string()),
                },
            }
        })
        .collect()
}

/// Where a run over several histories has got to, as one stream.
#[derive(Serialize, Clone, Default)]
struct Progress {
    /// File being read, counted across all the histories.
    index: usize,
    total: usize,
    path: String,
    bytes_done: u64,
    bytes_total: u64,
}

/// How checking one history ended.
#[derive(Serialize, Clone)]
pub struct Checked {
    root: String,
    report: Option<VerifyReport>,
    error: Option<String>,
}

fn leaf(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Title and text of the notification for a finished check.
fn summary(results: &[Checked]) -> Option<(String, String)> {
    let reports: Vec<&VerifyReport> = results.iter().filter_map(|r| r.report.as_ref()).collect();
    if reports.iter().any(|report| report.cancelled) {
        return None;
    }
    let names = results
        .iter()
        .map(|r| leaf(Path::new(&r.root)))
        .collect::<Vec<_>>()
        .join(", ");
    let broken = results.iter().filter(|r| r.error.is_some()).count();
    let changed: usize = reports.iter().map(|r| r.mismatched.len()).sum();
    let missing: usize = reports.iter().map(|r| r.missing.len() + r.unsupported.len()).sum();
    let files: usize = reports.iter().map(|r| r.files).sum();
    Some(if broken == 0 && reports.iter().all(|r| r.intact()) {
        (
            "Backup integro".into(),
            format!("Tutti {} di {names} corrispondono alla storia MHL.", the_files(files)),
        )
    } else {
        (
            "Backup non integro".into(),
            format!(
                "{names}: {changed} file modificati, {missing} mancanti o non verificabili. \
                 Apri BackupTool per i dettagli."
            ),
        )
    })
}

/// Checks the given histories one after the other on a background thread.
/// Progress arrives as `verify-progress`; the run ends with `verify-finished`.
/// Stopped with the same command that stops a copy.
#[tauri::command]
pub fn start_verify(
    app: AppHandle,
    state: State<JobState>,
    roots: Vec<PathBuf>,
    record: bool,
    notify: bool,
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("C'è già un'operazione in corso.".into());
    }
    state.cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.cancel);
    let running = Arc::clone(&state.running);
    std::thread::spawn(move || {
        let creator = Creator {
            tool: "BackupTool".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        // What each history adds to the totals; one that cannot be read adds
        // nothing and fails when its turn comes.
        let sizes: Vec<(usize, u64)> = roots
            .iter()
            .map(|root| summarize(root).map_or((0, 0), |info| (info.files, info.bytes)))
            .collect();
        let progress = Mutex::new(Progress {
            total: sizes.iter().map(|(files, _)| files).sum(),
            bytes_total: sizes.iter().map(|(_, bytes)| bytes).sum(),
            ..Progress::default()
        });

        let mut results = Vec::new();
        let (mut files_before, mut bytes_before) = (0usize, 0u64);
        for (root, (files, bytes)) in roots.iter().zip(&sizes) {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let outcome = verify_history(root, record, &creator, &cancel, &|event| {
                let mut now = progress.lock().unwrap();
                match event {
                    VerifyEvent::File { index, path, .. } => {
                        now.index = files_before + index;
                        now.path = path;
                    }
                    VerifyEvent::Progress { bytes_done, .. } => now.bytes_done = bytes_before + bytes_done,
                }
                let _ = app.emit("verify-progress", now.clone());
            });
            files_before += files;
            bytes_before += bytes;
            results.push(match outcome {
                Ok(report) => Checked {
                    root: root.to_string_lossy().into_owned(),
                    report: Some(report),
                    error: None,
                },
                Err(err) => Checked {
                    root: root.to_string_lossy().into_owned(),
                    report: None,
                    error: Some(err.to_string()),
                },
            });
        }
        running.store(false, Ordering::SeqCst);
        if notify {
            if let Some((title, body)) = summary(&results) {
                notify::send(&app, &title, &body);
            }
        }
        let _ = app.emit("verify-finished", &results);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(files: usize, verified: usize, changed: usize, missing: usize, cancelled: bool) -> VerifyReport {
        VerifyReport {
            root: PathBuf::from("F:/2026/Trail series"),
            files,
            verified,
            mismatched: (0..changed)
                .map(|i| backup_engine::verify::Mismatch {
                    path: format!("A{i}.MOV"),
                    format: "xxh64".into(),
                    expected: "a".into(),
                    actual: "b".into(),
                })
                .collect(),
            missing: (0..missing).map(|i| format!("B{i}.MOV")).collect(),
            untracked: Vec::new(),
            unsupported: Vec::new(),
            cancelled,
            recorded: None,
            duration_ms: 0,
        }
    }

    fn checked(report: VerifyReport) -> Checked {
        Checked {
            root: "F:/2026/Trail series".into(),
            report: Some(report),
            error: None,
        }
    }

    #[test]
    fn an_intact_backup_is_announced_as_such() {
        let (title, body) = summary(&[checked(report(131, 131, 0, 0, false))]).unwrap();
        assert_eq!(title, "Backup integro");
        assert_eq!(body, "Tutti i 131 file di Trail series corrispondono alla storia MHL.");
    }

    #[test]
    fn damage_and_a_broken_history_are_announced_as_not_intact() {
        let (title, body) = summary(&[checked(report(131, 128, 2, 1, false))]).unwrap();
        assert_eq!(title, "Backup non integro");
        assert!(body.contains("2 file modificati, 1 mancanti"));

        let broken = Checked {
            root: "F:/2026/Trail series".into(),
            report: None,
            error: Some("history cannot be trusted".into()),
        };
        assert_eq!(summary(&[broken]).unwrap().0, "Backup non integro");
    }

    #[test]
    fn a_check_the_user_stopped_is_not_announced() {
        assert!(summary(&[checked(report(131, 40, 0, 0, true))]).is_none());
    }
}
