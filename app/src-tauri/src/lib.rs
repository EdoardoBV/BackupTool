mod checkup;
mod eject;
mod notify;
mod shell;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use backup_engine::mhl::{record_destination, Creator, Generation};
use backup_engine::report::{self, ReportInput, REPORTS_DIR};
use backup_engine::shoot::EMPTY_FOLDERS;
use backup_engine::{
    analyze, clients, compare, create_folders, folders, route, run_job, scan, CardInfo, Client,
    FileEntry, FolderLine, JobReport, JobSpec, Presence, ShootLayout, VerifyMode,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
pub(crate) struct JobState {
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) running: Arc<AtomicBool>,
}

/// The source's files, each with its place on the destinations: the same
/// relative path, or wherever the shoot layout sends it.
fn planned(source: &Path, layout: Option<&ShootLayout>) -> Result<Vec<FileEntry>, String> {
    let mut files = scan(source, |_| {}).map_err(|err| err.to_string())?;
    if let Some(layout) = layout {
        route(&mut files, layout).map_err(|err| err.to_string())?;
    }
    Ok(files)
}

/// What the card holds and which camera wrote it. Reads a few small
/// metadata blocks, never the footage.
#[tauri::command(async)]
fn analyze_card(source: PathBuf) -> Result<CardInfo, String> {
    let files = scan(&source, |_| {}).map_err(|err| err.to_string())?;
    Ok(analyze(&source, &files))
}

#[derive(Serialize)]
struct DestState {
    path: String,
    /// The folder can be reached: a remembered drive may not be plugged in.
    online: bool,
    missing: usize,
    different: usize,
    present: usize,
    missing_bytes: u64,
}

#[derive(Serialize)]
struct Inspection {
    files: usize,
    bytes: u64,
    dests: Vec<DestState>,
}

fn dest_state(files: &[FileEntry], dest: &Path) -> DestState {
    let mut state = DestState {
        path: dest.to_string_lossy().into_owned(),
        online: dest.is_dir(),
        missing: 0,
        different: 0,
        present: 0,
        missing_bytes: 0,
    };
    for (file, presence) in files.iter().zip(compare(files, dest)) {
        match presence {
            Presence::Present => state.present += 1,
            Presence::SizeMismatch => state.different += 1,
            Presence::Missing => {
                state.missing += 1;
                state.missing_bytes += file.size;
            }
        }
    }
    state
}

/// Reads the source and reports, per destination, how much of it is not
/// there yet. Looks at names and sizes only; no file contents are read.
#[tauri::command(async)]
fn inspect(
    source: PathBuf,
    destinations: Vec<PathBuf>,
    layout: Option<ShootLayout>,
) -> Result<Inspection, String> {
    let files = planned(&source, layout.as_ref())?;
    Ok(Inspection {
        files: files.len(),
        bytes: files.iter().map(|file| file.size).sum(),
        dests: destinations.iter().map(|dest| dest_state(&files, dest)).collect(),
    })
}

#[tauri::command(async)]
fn list_clients(destination: PathBuf) -> Vec<Client> {
    clients(&destination)
}

#[derive(Serialize)]
struct ShootPreview {
    folders: Vec<FolderLine>,
    /// Files whose place on the destination is taken by something of a
    /// different size: the sign of another card with the same file names.
    conflicts: usize,
}

/// The folders a shoot would create on `destination`. Changes nothing.
#[tauri::command(async)]
fn preview_shoot(
    source: PathBuf,
    destination: PathBuf,
    layout: ShootLayout,
) -> Result<ShootPreview, String> {
    let files = planned(&source, Some(&layout))?;
    Ok(ShootPreview {
        folders: folders(&files, &layout, &destination).map_err(|err| err.to_string())?,
        conflicts: dest_state(&files, &destination).different,
    })
}

/// Creates the shoot's folders on every destination and returns the shoot
/// folder's path relative to them.
#[tauri::command(async)]
fn create_shoot(
    source: PathBuf,
    destinations: Vec<PathBuf>,
    layout: ShootLayout,
) -> Result<String, String> {
    let files = planned(&source, Some(&layout))?;
    create_folders(&files, &layout, &destinations).map_err(|err| err.to_string())?;
    layout.shoot_dir().map_err(|err| err.to_string())
}

/// Names the user gave their cameras, keyed by serial number (or by model
/// when the camera does not record one).
fn camera_file(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|err| err.to_string())?;
    Ok(dir.join("cameras.json"))
}

#[tauri::command]
fn camera_names(app: AppHandle) -> BTreeMap<String, String> {
    camera_file(&app)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn remember_camera(app: AppHandle, key: String, name: String) -> Result<(), String> {
    let path = camera_file(&app)?;
    let mut names = camera_names(app);
    names.insert(key, name);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&names).map_err(|err| err.to_string())?;
    fs::write(path, json).map_err(|err| err.to_string())
}

/// What to leave next to the footage once a job is done.
#[derive(Deserialize, Clone, Copy)]
struct Extras {
    /// Record the verified files in an ASC MHL history.
    mhl: bool,
    /// Write an HTML and CSV report.
    report: bool,
    /// Show a system notification when the job ends.
    notify: bool,
}

/// What was written on one destination after the copy.
#[derive(Serialize, Clone)]
struct DestExtras {
    destination: String,
    mhl: Option<Generation>,
    mhl_error: Option<String>,
    /// Path of the HTML report.
    report: Option<String>,
    report_error: Option<String>,
}

#[derive(Serialize, Clone)]
struct Finished {
    report: JobReport,
    extras: Vec<DestExtras>,
}

fn under(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_owned();
    path.extend(rel.split('/').filter(|part| !part.is_empty()));
    path
}

/// Writes the MHL generation and the report on every destination. A problem
/// here never changes the outcome of the copy; it is reported next to it.
fn write_extras(
    spec: &JobSpec,
    layout: Option<&ShootLayout>,
    extras: Extras,
    files: &[FileEntry],
    job: &JobReport,
) -> Vec<DestExtras> {
    if job.files.is_empty() {
        return Vec::new();
    }
    // With a shoot, history and reports belong to the shoot folder.
    let home = layout.and_then(|l| l.shoot_dir().ok()).unwrap_or_default();
    let creator = Creator {
        tool: "BackupTool".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    // Things under the history root that are deliberately not tracked.
    let mut ignore = vec![format!("/{REPORTS_DIR}")];
    if layout.is_some() {
        ignore.extend(EMPTY_FOLDERS.iter().map(|name| format!("/{name}")));
    }
    let algo = spec.options.algo;
    let mhl = extras.mhl && spec.options.verify != VerifyMode::None && algo.in_mhl();
    let card = extras.report.then(|| analyze(&spec.source, files));
    let finished_at = chrono::Local::now();
    let title = if home.is_empty() {
        spec.source.file_name().map_or_else(
            || spec.source.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    } else {
        home.replace('/', " / ")
    };

    spec.destinations
        .iter()
        .enumerate()
        .map(|(index, dest)| {
            let mut written = DestExtras {
                destination: dest.to_string_lossy().into_owned(),
                mhl: None,
                mhl_error: None,
                report: None,
                report_error: None,
            };
            let mut notes = Vec::new();
            if mhl {
                match record_destination(dest, &home, files, &job.files, index, algo, &creator, &ignore) {
                    Ok(Some(generation)) => {
                        notes.push(format!(
                            "MHL: generazione {}, {} file nuovi e {} riverificati",
                            generation.number, generation.original, generation.verified
                        ));
                        if !generation.failed.is_empty() {
                            notes.push(format!(
                                "MHL: {} file non corrispondono più a quanto registrato in passato",
                                generation.failed.len()
                            ));
                        }
                        written.mhl = Some(generation);
                    }
                    Ok(None) => {}
                    Err(err) => written.mhl_error = Some(err.to_string()),
                }
            }
            if extras.report {
                let input = ReportInput {
                    title: &title,
                    source: &spec.source,
                    destinations: &spec.destinations,
                    card: card.as_ref(),
                    options: &spec.options,
                    files,
                    report: job,
                    finished_at,
                    notes: &notes,
                    tool: concat!("BackupTool ", env!("CARGO_PKG_VERSION")),
                };
                match report::write(&input, &under(dest, &home)) {
                    Ok(path) => written.report = Some(path.to_string_lossy().into_owned()),
                    Err(err) => written.report_error = Some(err.to_string()),
                }
            }
            written
        })
        .collect()
}

/// Opens a file with the system's default application.
#[tauri::command]
fn open_path(path: PathBuf) -> Result<(), String> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Starts the copy on a background thread. Progress arrives as `job-event`,
/// and the run ends with either `job-finished` (the report, plus what was
/// written next to the footage) or `job-failed`.
#[tauri::command]
fn start_job(
    app: AppHandle,
    state: State<JobState>,
    spec: JobSpec,
    layout: Option<ShootLayout>,
    extras: Extras,
) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("C'è già un'operazione in corso.".into());
    }
    state.cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.cancel);
    let running = Arc::clone(&state.running);
    std::thread::spawn(move || {
        // Scan again here so the job reflects the card as it is right now.
        let outcome = planned(&spec.source, layout.as_ref()).and_then(|files| {
            let report = run_job(&spec, &files, &cancel, &|event| {
                let _ = app.emit("job-event", &event);
            })
            .map_err(|err| err.to_string())?;
            let written = write_extras(&spec, layout.as_ref(), extras, &files, &report);
            Ok(Finished { report, extras: written })
        });
        running.store(false, Ordering::SeqCst);
        if extras.notify {
            let ended = outcome.as_ref().map(|finished| &finished.report).map_err(String::as_str);
            notify::job_ended(&app, &spec, ended);
        }
        let _ = match outcome {
            Ok(finished) => app.emit("job-finished", &finished),
            Err(message) => app.emit("job-failed", message),
        };
    });
    Ok(())
}

#[tauri::command]
fn cancel_job(state: State<JobState>) {
    state.cancel.store(true, Ordering::SeqCst);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(JobState::default())
        .manage(shell::PopoverState::default())
        .setup(|app| Ok(shell::setup(app.handle())?))
        .invoke_handler(tauri::generate_handler![
            shell::list_volumes,
            shell::set_tray_status,
            shell::open_main,
            shell::open_popover,
            shell::close_popover,
            shell::pin_popover,
            shell::fit_popover,
            shell::popover_material,
            shell::quit,
            eject::eject,
            checkup::find_backups,
            checkup::start_verify,
            analyze_card,
            inspect,
            list_clients,
            preview_shoot,
            create_shoot,
            camera_names,
            remember_camera,
            start_job,
            cancel_job,
            open_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running the app");
}
