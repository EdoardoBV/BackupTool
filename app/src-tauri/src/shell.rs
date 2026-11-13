//! The app's presence on the desktop: the tray icon, the small window that
//! opens from it, and the watch for cards being plugged in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use backup_engine::card::looks_like_card;
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Rect, State, WebviewWindow,
    WindowEvent,
};

const TRAY: &str = "tray";
const MAIN: &str = "main";
const POPOVER: &str = "popover";
/// Gap between the tray icon and the popover, and between it and screen edges.
const MARGIN: f64 = 8.0;
/// Popover size in logical pixels; the height follows its content.
const POPOVER_WIDTH: f64 = 380.0;
const POPOVER_HEIGHTS: std::ops::RangeInclusive<f64> = 96.0..=640.0;
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct PopoverState {
    hidden_at: Mutex<Option<Instant>>,
    /// Height the content asked for, in logical pixels; zero until it has.
    height: Mutex<f64>,
    /// While set, losing focus does not close the popover (a system dialog
    /// opened from it takes the focus away).
    pinned: AtomicBool,
}

// ---- Volumes -------------------------------------------------------------

#[derive(Serialize, Clone, PartialEq)]
pub struct Volume {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) file_system: String,
    pub(crate) removable: bool,
    /// Holds the folders a camera writes.
    pub(crate) card: bool,
    /// A share on another machine, mapped as a drive. Its sizes are not known.
    pub(crate) network: bool,
    pub(crate) total: u64,
    pub(crate) available: u64,
}

pub(crate) fn volumes() -> Vec<Volume> {
    // The disk list leaves out everything that is not attached to this machine.
    let mut found: Vec<Volume> = sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .map(|disk| Volume {
            path: disk.mount_point().to_string_lossy().into_owned(),
            name: disk.name().to_string_lossy().into_owned(),
            file_system: disk.file_system().to_string_lossy().into_owned(),
            removable: disk.is_removable(),
            card: looks_like_card(disk.mount_point()),
            network: false,
            total: disk.total_space(),
            available: disk.available_space(),
        })
        .collect();
    found.extend(network_volumes());
    found
}

/// Shares mapped to a drive letter, named by where they point (`\\nas\video`).
///
/// Nothing here may wait on the network: this runs every couple of seconds,
/// and a NAS that is switched off would hold up noticing a card. So there are
/// no sizes, and no look inside for camera folders: a share is never a card.
#[cfg(windows)]
fn network_volumes() -> Vec<Volume> {
    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    let wide = |text: String| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
    // SAFETY: takes no arguments and returns a bit per drive letter.
    let letters = unsafe { GetLogicalDrives() };
    ('A'..='Z')
        .enumerate()
        .filter(|(bit, _)| letters & (1 << bit) != 0)
        .filter_map(|(_, letter)| {
            let root = wide(format!("{letter}:\\"));
            // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
            if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_REMOTE {
                return None;
            }
            let local = wide(format!("{letter}:"));
            let mut remote = [0u16; 1024];
            let mut len = remote.len() as u32;
            // SAFETY: `local` is NUL-terminated; `remote` has room for `len`
            // characters, and both outlive the call.
            let known = unsafe { WNetGetConnectionW(local.as_ptr(), remote.as_mut_ptr(), &mut len) } == 0;
            let end = remote.iter().position(|&unit| unit == 0).unwrap_or(0);
            Some(Volume {
                path: format!("{letter}:\\"),
                name: if known { String::from_utf16_lossy(&remote[..end]) } else { String::new() },
                file_system: String::new(),
                removable: false,
                card: false,
                network: true,
                total: 0,
                available: 0,
            })
        })
        .collect()
}

/// Only Windows shares are listed so far; elsewhere a share is picked as a folder.
#[cfg(not(windows))]
fn network_volumes() -> Vec<Volume> {
    Vec::new()
}

#[tauri::command(async)]
pub fn list_volumes() -> Vec<Volume> {
    volumes()
}

/// What changed between two looks at the drives: cards that appeared and
/// volumes that went away, by mount path.
fn changes(before: &[Volume], after: &[Volume]) -> (Vec<String>, Vec<String>) {
    let inserted = after
        .iter()
        .filter(|now| now.card && !before.iter().any(|was| was.path == now.path && was.card))
        .map(|volume| volume.path.clone())
        .collect();
    let removed = before
        .iter()
        .filter(|was| !after.iter().any(|now| now.path == was.path))
        .map(|volume| volume.path.clone())
        .collect();
    (inserted, removed)
}

/// Tells the windows when drives come and go, and brings up the popover when
/// a card is plugged in. Cards already there at launch do not pop anything up.
fn watch_volumes(app: AppHandle) {
    std::thread::spawn(move || {
        let mut before = volumes();
        loop {
            std::thread::sleep(WATCH_INTERVAL);
            let after = volumes();
            let (inserted, removed) = changes(&before, &after);
            let same_set = before.len() == after.len()
                && before.iter().zip(&after).all(|(a, b)| a.path == b.path && a.card == b.card);
            if !same_set {
                let _ = app.emit("volumes-changed", &after);
            }
            for path in removed {
                let _ = app.emit("volume-removed", path);
            }
            if let Some(card) = inserted.into_iter().next() {
                let _ = app.emit("card-inserted", card);
                show_popover(&app);
            }
            before = after;
        }
    });
}

// ---- Tray icon -----------------------------------------------------------

/// The app's lens, small: a metal barrel, a coating in the colour of the
/// state, a dark aperture. Drawn here so no image files and no decoder are
/// needed; the pale barrel keeps it visible on a dark or a light taskbar.
fn dot(tone: &str) -> Image<'static> {
    const SIZE: u32 = 32;
    let state: [f32; 3] = match tone {
        "red" => [255.0, 95.0, 82.0],
        "yellow" => [246.0, 185.0, 63.0],
        "green" => [50.0, 200.0, 115.0],
        _ => [138.0, 147.0, 163.0],
    };
    let centre = (SIZE as f32 - 1.0) / 2.0;
    let radius = 14.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 - centre, y as f32 - centre);
            let distance = (dx * dx + dy * dy).sqrt();
            let colour = if distance < 3.4 {
                [6.0, 8.0, 12.0]
            } else if distance < 9.0 {
                state
            } else if distance < 11.0 {
                [14.0, 17.0, 23.0]
            } else {
                // Lit from the top left, like the lens in the windows.
                let light = 150.0 - (dx + dy) * 3.0;
                [light, light + 6.0, light + 18.0]
            };
            // One pixel of falloff at the edge instead of a jagged outline.
            let alpha = (radius + 0.5 - distance).clamp(0.0, 1.0);
            rgba.extend(colour.map(|channel| channel.clamp(0.0, 255.0) as u8));
            rgba.push((alpha * 255.0) as u8);
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}

/// Shows the current state on the tray icon: grey, red, yellow or green.
#[tauri::command]
pub fn set_tray_status(app: AppHandle, tone: String, title: String) -> Result<(), String> {
    let tray = app.tray_by_id(TRAY).ok_or("no tray icon")?;
    tray.set_icon(Some(dot(&tone))).map_err(|err| err.to_string())?;
    tray.set_tooltip(Some(format!("BackupTool: {title}"))).map_err(|err| err.to_string())
}

// ---- Windows -------------------------------------------------------------

/// Puts the popover next to the tray icon: above it when the icon sits in
/// the lower half of the screen (Windows), below it otherwise (macOS).
fn place(window: &WebviewWindow, tray: &Rect, logical_height: f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let icon_at = tray.position.to_physical::<f64>(scale);
    let icon = tray.size.to_physical::<f64>(scale);
    // A size just asked for may not be reported back yet: when the content
    // has said how tall it is, trust that.
    let (width, height) = if logical_height > 0.0 {
        (POPOVER_WIDTH * scale, logical_height * scale)
    } else {
        let Ok(size) = window.outer_size() else { return };
        (f64::from(size.width), f64::from(size.height))
    };
    let centre = icon_at.x + icon.width / 2.0;
    let mut x = centre - width / 2.0;
    let mut y = icon_at.y - height - MARGIN;
    if let Ok(Some(monitor)) = window.monitor_from_point(centre, icon_at.y) {
        let area = monitor.work_area();
        let (left, top) = (f64::from(area.position.x), f64::from(area.position.y));
        let (right, bottom) = (left + f64::from(area.size.width), top + f64::from(area.size.height));
        if icon_at.y < (top + bottom) / 2.0 {
            y = icon_at.y + icon.height + MARGIN;
        }
        x = x.clamp(left + MARGIN, (right - width - MARGIN).max(left + MARGIN));
        y = y.clamp(top + MARGIN, (bottom - height - MARGIN).max(top + MARGIN));
    }
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn show_popover(app: &AppHandle) {
    let Some(window) = app.get_webview_window(POPOVER) else { return };
    let height = *app.state::<PopoverState>().height.lock().unwrap();
    if let Some(rect) = app.tray_by_id(TRAY).and_then(|tray| tray.rect().ok().flatten()) {
        place(&window, &rect, height);
    }
    let _ = window.show();
    let _ = window.set_focus();
}

fn hide_popover(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(POPOVER) {
        let _ = window.hide();
        *app.state::<PopoverState>().hidden_at.lock().unwrap() = Some(Instant::now());
    }
}

fn toggle_popover(app: &AppHandle) {
    let visible = app
        .get_webview_window(POPOVER)
        .is_some_and(|window| window.is_visible().unwrap_or(false));
    if visible {
        hide_popover(app);
        return;
    }
    // Clicking the tray icon first takes focus from an open popover, which
    // hides it; without this check the same click would open it again.
    let just_hidden = app
        .state::<PopoverState>()
        .hidden_at
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
    if !just_hidden {
        show_popover(app);
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[tauri::command]
pub fn open_main(app: AppHandle) {
    hide_popover(&app);
    show_main(&app);
}

#[tauri::command]
pub fn open_popover(app: AppHandle) {
    show_popover(&app);
}

#[tauri::command]
pub fn close_popover(app: AppHandle) {
    hide_popover(&app);
}

/// Makes the popover as tall as its content and keeps it against the tray.
#[tauri::command]
pub fn fit_popover(app: AppHandle, height: f64) {
    let Some(window) = app.get_webview_window(POPOVER) else { return };
    let height = height.clamp(*POPOVER_HEIGHTS.start(), *POPOVER_HEIGHTS.end());
    *app.state::<PopoverState>().height.lock().unwrap() = height;
    let _ = window.set_size(LogicalSize::new(POPOVER_WIDTH, height));
    if window.is_visible().unwrap_or(false) {
        if let Some(rect) = app.tray_by_id(TRAY).and_then(|tray| tray.rect().ok().flatten()) {
            place(&window, &rect, height);
        }
    }
}

/// Whether the popover window shows what is behind it, blurred. Only where
/// that has been seen to work; elsewhere the panel is drawn solid.
#[tauri::command]
pub fn popover_material() -> &'static str {
    if cfg!(windows) {
        "glass"
    } else {
        "solid"
    }
}

/// Keeps the popover open while a system dialog it opened has the focus.
#[tauri::command]
pub fn pin_popover(state: State<PopoverState>, pinned: bool) {
    state.pinned.store(pinned, Ordering::SeqCst);
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}

/// Sets up the tray icon, the windows' behaviour and the card watch.
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Apri BackupTool", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Esci", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    TrayIconBuilder::with_id(TRAY)
        .icon(dot("blank"))
        .tooltip("BackupTool")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_popover(tray.app_handle());
            }
        })
        .build(app)?;

    // Closing the main window leaves the app in the tray: a copy in progress
    // carries on, and cards are still noticed.
    if let Some(main) = app.get_webview_window(MAIN) {
        let window = main.clone();
        main.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        });
    }
    if let Some(popover) = app.get_webview_window(POPOVER) {
        let handle = app.clone();
        popover.on_window_event(move |event| {
            let pinned = handle.state::<PopoverState>().pinned.load(Ordering::SeqCst);
            if matches!(event, WindowEvent::Focused(false)) && !pinned {
                hide_popover(&handle);
            }
        });
    }
    watch_volumes(app.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(path: &str, card: bool) -> Volume {
        Volume {
            path: path.into(),
            name: String::new(),
            file_system: "exFAT".into(),
            removable: true,
            card,
            network: false,
            total: 0,
            available: 0,
        }
    }

    /// The real system call path: whatever shares this machine has mapped,
    /// none is a card or ejectable, and no local disk is taken for one.
    #[test]
    #[cfg(windows)]
    fn local_disks_are_not_listed_as_network_shares() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        for share in network_volumes() {
            assert!(share.network && !share.card && !share.removable, "{}", share.path);
            assert!(!share.path.eq_ignore_ascii_case(&format!("{system}\\")));
        }
        assert!(volumes().iter().any(|volume| !volume.network));
    }

    #[test]
    fn a_new_card_is_reported_once_and_other_drives_are_not() {
        let before = [volume("C:\\", false)];
        let after = [volume("C:\\", false), volume("E:\\", true), volume("F:\\", false)];

        assert_eq!(changes(&before, &after), (vec!["E:\\".to_string()], vec![]));
        // Still plugged in on the next look: nothing new.
        assert_eq!(changes(&after, &after), (vec![], vec![]));
    }

    #[test]
    fn a_drive_that_becomes_a_card_counts_as_inserted() {
        // A freshly formatted card gets its DCIM folder a moment after mounting.
        let before = [volume("E:\\", false)];
        let after = [volume("E:\\", true)];
        assert_eq!(changes(&before, &after).0, ["E:\\"]);
    }

    #[test]
    fn a_pulled_volume_is_reported_as_removed() {
        let before = [volume("C:\\", false), volume("E:\\", true)];
        let after = [volume("C:\\", false)];
        assert_eq!(changes(&before, &after), (vec![], vec!["E:\\".to_string()]));
    }

    #[test]
    fn every_status_has_an_icon_with_a_transparent_corner_and_solid_centre() {
        for tone in ["blank", "red", "yellow", "green"] {
            let icon = dot(tone);
            let rgba = icon.rgba();
            assert_eq!(rgba.len(), 32 * 32 * 4);
            assert_eq!(rgba[3], 0, "{tone}: corner");
            assert_eq!(rgba[(16 * 32 + 16) * 4 + 3], 255, "{tone}: centre");
        }
        // The coating, a few pixels out from the dark aperture, carries the state.
        let coating = (16 * 32 + 22) * 4;
        assert_ne!(dot("red").rgba()[coating], dot("green").rgba()[coating]);
    }
}
