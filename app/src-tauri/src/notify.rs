//! System notifications for the moments nobody is watching: a copy that
//! finishes while the app sits in the tray.

use std::path::Path;

use backup_engine::report::the_files;
use backup_engine::{JobReport, JobSpec};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::shell::volumes;

fn leaf(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// "T7", "T7 e Backup", "T7, Backup e Shuttle".
fn list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} e {last}", rest.join(", ")),
    }
}

/// Title and text for the end of a job. Nothing when the user stopped it:
/// they already know.
fn summary(report: &JobReport, card: &str, dests: &[String]) -> Option<(String, String)> {
    if report.cancelled {
        return None;
    }
    Some(if report.safe_to_format {
        (
            "Puoi formattare".into(),
            format!(
                "Tutti {} di {card} sono verificati su {}.",
                the_files(report.total_files),
                list(dests)
            ),
        )
    } else if report.failed_files > 0 {
        (
            "Non formattare".into(),
            format!(
                "{} file su {} di {card} non sono verificati. Apri BackupTool per i dettagli.",
                report.failed_files, report.total_files
            ),
        )
    } else {
        (
            "Copia finita, non verificata".into(),
            format!("I file di {card} sono stati copiati senza verifica. Non formattare."),
        )
    })
}

/// The card's volume name when the source is a volume, its folder name otherwise.
fn card_name(source: &Path) -> String {
    let path = source.to_string_lossy();
    volumes()
        .into_iter()
        .find(|volume| volume.path == path && !volume.name.is_empty())
        .map_or_else(|| leaf(source), |volume| volume.name)
}

pub fn send(app: &AppHandle, title: &str, body: &str) {
    // Someone looking at the app sees the result there; a toast on top of
    // it would only be noise.
    let watching = app.webview_windows().values().any(|window| {
        window.is_visible().unwrap_or(false) && window.is_focused().unwrap_or(false)
    });
    if watching {
        return;
    }
    if let Err(err) = app.notification().builder().title(title).body(body).show() {
        eprintln!("notification not shown: {err}");
    }
}

/// Tells the user how a job ended, unless they are looking at the app.
pub fn job_ended(app: &AppHandle, spec: &JobSpec, outcome: Result<&JobReport, &str>) {
    let card = card_name(&spec.source);
    let dests: Vec<String> = spec.destinations.iter().map(|dest| leaf(dest)).collect();
    match outcome {
        Ok(report) => {
            if let Some((title, body)) = summary(report, &card, &dests) {
                send(app, &title, &body);
            }
        }
        Err(message) => send(app, "La copia non è partita", &format!("{card}: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(total: usize, failed: usize, safe: bool, cancelled: bool) -> JobReport {
        JobReport {
            files: Vec::new(),
            total_files: total,
            total_bytes: 0,
            failed_files: failed,
            cancelled,
            safe_to_format: safe,
            duration_ms: 0,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_verified_copy_says_the_card_can_be_formatted_and_where_it_is() {
        let (title, body) =
            summary(&report(131, 0, true, false), "NIKON Z6_3", &names(&["T7 Shield", "Backup"])).unwrap();
        assert_eq!(title, "Puoi formattare");
        assert_eq!(body, "Tutti i 131 file di NIKON Z6_3 sono verificati su T7 Shield e Backup.");
    }

    #[test]
    fn failures_and_unverified_copies_say_not_to_format() {
        let (title, body) = summary(&report(131, 3, false, false), "Card A", &names(&["T7"])).unwrap();
        assert_eq!(title, "Non formattare");
        assert!(body.starts_with("3 file su 131 di Card A non sono verificati."));

        let (title, body) = summary(&report(131, 0, false, false), "Card A", &names(&["T7"])).unwrap();
        assert_eq!(title, "Copia finita, non verificata");
        assert!(body.ends_with("Non formattare."));
    }

    #[test]
    fn a_copy_the_user_stopped_is_not_announced() {
        assert_eq!(summary(&report(131, 0, false, true), "Card A", &names(&["T7"])), None);
    }

    #[test]
    fn destinations_read_as_a_sentence() {
        assert_eq!(list(&names(&["T7"])), "T7");
        assert_eq!(list(&names(&["T7", "Backup", "Shuttle"])), "T7, Backup e Shuttle");
    }
}
