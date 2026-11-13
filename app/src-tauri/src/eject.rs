//! Ejecting a card once it is safe to pull it out.
//!
//! Only removable volumes are ever touched, and never while a copy runs.
//! The system does the ejecting: on Windows the same lock, dismount and eject
//! requests Explorer sends; on macOS `diskutil`.

use std::sync::atomic::Ordering;

use tauri::State;

use crate::shell::{volumes, Volume};
use crate::JobState;

/// Mount points compare without case and without a trailing separator, so
/// `E:\` and `e:` are the same volume.
fn same_mount(a: &str, b: &str) -> bool {
    let trim = |path: &str| path.trim_end_matches(['\\', '/']).to_lowercase();
    !trim(a).is_empty() && trim(a) == trim(b)
}

/// Why `path` must not be ejected, if there is a reason.
fn refusal(volumes: &[Volume], path: &str, job_running: bool) -> Option<&'static str> {
    if job_running {
        return Some("C'è una copia in corso. Aspetta che finisca, oppure fermala.");
    }
    match volumes.iter().find(|volume| same_mount(&volume.path, path)) {
        None => Some("Non è una scheda: si possono espellere solo i volumi rimovibili."),
        Some(volume) if !volume.removable => Some("Questo disco non è rimovibile."),
        Some(_) => None,
    }
}

#[cfg(windows)]
fn eject_volume(mount: &str) -> Result<(), String> {
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{null, null_mut};
    use std::time::Duration;

    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, FILE_SHARE_READ, FILE_SHARE_WRITE};
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA,
        IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
    };
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let mut chars = mount.chars();
    let letter = match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => letter,
        _ => return Err("Non è la lettera di un'unità.".into()),
    };
    // Ask Windows itself, not only the list we showed: a fixed disk must
    // never be dismounted from under the system.
    let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
    if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_REMOVABLE {
        return Err("Windows non considera rimovibile questa unità.".into());
    }

    let device = format!(r"\\.\{letter}:");
    let volume = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&device)
        .map_err(|err| format!("Non riesco ad aprire l'unità {letter}: ({err})."))?;
    let control = |code: u32, input: *const c_void, len: u32| -> io::Result<()> {
        let mut returned = 0u32;
        // SAFETY: the handle is open for the life of `volume`; `input` is
        // either null with length zero or points to `len` readable bytes;
        // no output buffer is passed.
        let ok = unsafe {
            DeviceIoControl(volume.as_raw_handle(), code, input, len, null_mut(), 0, &mut returned, null_mut())
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };

    // The lock fails while any program has a file open on the volume.
    // Something may still be letting go, so try for a few seconds.
    let mut locked = control(FSCTL_LOCK_VOLUME, null(), 0);
    for _ in 0..20 {
        if locked.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        locked = control(FSCTL_LOCK_VOLUME, null(), 0);
    }
    if locked.is_err() {
        return Err("La scheda è ancora in uso da un altro programma. Chiudilo e riprova.".into());
    }
    control(FSCTL_DISMOUNT_VOLUME, null(), 0)
        .map_err(|err| format!("Windows non ha smontato la scheda ({err})."))?;
    // Some readers hold the media until told they may release it.
    // SAFETY: an all-zero PREVENT_MEDIA_REMOVAL is the valid value "do not prevent".
    let release: PREVENT_MEDIA_REMOVAL = unsafe { std::mem::zeroed() };
    let _ = control(
        IOCTL_STORAGE_MEDIA_REMOVAL,
        (&release as *const PREVENT_MEDIA_REMOVAL).cast(),
        std::mem::size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
    );
    control(IOCTL_STORAGE_EJECT_MEDIA, null(), 0)
        .map_err(|err| format!("Windows non ha espulso la scheda ({err})."))
}

#[cfg(target_os = "macos")]
fn eject_volume(mount: &str) -> Result<(), String> {
    let output = std::process::Command::new("diskutil")
        .args(["eject", mount])
        .output()
        .map_err(|err| format!("Non riesco ad avviare diskutil ({err})."))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = if said.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { said };
    Err(format!("macOS non ha espulso la scheda: {}", said.trim()))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn eject_volume(_mount: &str) -> Result<(), String> {
    Err("L'espulsione non è disponibile su questo sistema.".into())
}

/// Ejects the removable volume mounted at `path`.
#[tauri::command(async)]
pub fn eject(job: State<JobState>, path: String) -> Result<(), String> {
    let running = job.running.load(Ordering::SeqCst);
    if let Some(reason) = refusal(&volumes(), &path, running) {
        return Err(reason.into());
    }
    eject_volume(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(path: &str, removable: bool) -> Volume {
        Volume {
            path: path.into(),
            name: String::new(),
            file_system: "exFAT".into(),
            removable,
            card: removable,
            network: false,
            total: 0,
            available: 0,
        }
    }

    #[test]
    fn only_a_removable_volume_may_be_ejected() {
        let drives = [volume("C:\\", false), volume("E:\\", true)];

        assert_eq!(refusal(&drives, "E:\\", false), None);
        assert_eq!(refusal(&drives, "e:", false), None);
        assert!(refusal(&drives, "C:\\", false).unwrap().contains("non è rimovibile"));
        // A folder, even one on the card, is not a volume.
        assert!(refusal(&drives, "E:\\DCIM", false).unwrap().contains("Non è una scheda"));
        assert!(refusal(&drives, "", false).is_some());
    }

    #[test]
    fn nothing_is_ejected_while_a_copy_runs() {
        let drives = [volume("E:\\", true)];
        assert!(refusal(&drives, "E:\\", true).unwrap().contains("copia in corso"));
    }

    /// The real system call path, on the one volume that must never go: it
    /// has to be refused before anything is asked of the drive.
    #[test]
    #[cfg(windows)]
    fn the_system_drive_is_refused_by_windows_drive_type() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let err = eject_volume(&format!("{system}\\")).unwrap_err();
        assert!(err.contains("non considera rimovibile"), "{err}");
        assert!(std::path::Path::new(&format!("{system}\\")).is_dir());
        assert!(eject_volume("DCIM").is_err());
    }
}
