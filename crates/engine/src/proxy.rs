//! Proxies: light, edit-friendly versions of clips, made with FFmpeg.
//!
//! A proxy goes in a `Proxy` folder beside its clip, under the same name.
//! That is the convention DaVinci Resolve and Blackmagic Proxy Generator
//! use, so the editor finds the proxies on its own. Picture and sound are
//! re-encoded; timecode, clip name and audio layout are kept, and no look is
//! applied: log footage stays log.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

/// Folder, beside the clips, that proxies are written to.
pub const PROXY_DIR: &str = "Proxy";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyCodec {
    /// Apple ProRes 422 Proxy, 10-bit, in a `.mov`. The usual editing proxy.
    ProresProxy,
    /// Apple ProRes 422 LT: heavier, closer to the original.
    ProresLt,
    /// H.264 8-bit in an `.mp4`: the smallest files, plays anywhere.
    H264,
    /// H.265 10-bit in an `.mp4`: small and keeps 10-bit colour.
    H265,
    /// Avid DNxHR LB in a `.mov`.
    DnxhrLb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxySize {
    /// At most 1080 lines high; smaller clips keep their size.
    Hd,
    Half,
    Quarter,
    Original,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyPreset {
    pub codec: ProxyCodec,
    pub size: ProxySize,
}

impl ProxyPreset {
    /// Container, which is also the file extension.
    pub fn extension(self) -> &'static str {
        match self.codec {
            ProxyCodec::H264 | ProxyCodec::H265 => "mp4",
            ProxyCodec::ProresProxy | ProxyCodec::ProresLt | ProxyCodec::DnxhrLb => "mov",
        }
    }

    /// FFmpeg arguments for picture and sound.
    fn encode_args(self) -> Vec<&'static str> {
        let mut args = match self.size {
            // Even dimensions throughout: several encoders refuse odd ones.
            ProxySize::Hd => vec!["-vf", "scale=-2:'min(1080,ih)'"],
            ProxySize::Half => vec!["-vf", "scale=trunc(iw/4)*2:trunc(ih/4)*2"],
            ProxySize::Quarter => vec!["-vf", "scale=trunc(iw/8)*2:trunc(ih/8)*2"],
            ProxySize::Original => vec![],
        };
        args.extend(match self.codec {
            ProxyCodec::ProresProxy => {
                vec!["-c:v", "prores_ks", "-profile:v", "0", "-vendor", "apl0", "-pix_fmt", "yuv422p10le"]
            }
            ProxyCodec::ProresLt => {
                vec!["-c:v", "prores_ks", "-profile:v", "1", "-vendor", "apl0", "-pix_fmt", "yuv422p10le"]
            }
            ProxyCodec::H264 => vec!["-c:v", "libx264", "-preset", "veryfast", "-crf", "21", "-pix_fmt", "yuv420p"],
            ProxyCodec::H265 => vec![
                "-c:v", "libx265", "-preset", "fast", "-crf", "24", "-pix_fmt", "yuv420p10le", "-tag:v", "hvc1",
            ],
            ProxyCodec::DnxhrLb => vec!["-c:v", "dnxhd", "-profile:v", "dnxhr_lb", "-pix_fmt", "yuv422p"],
        });
        args.extend(match self.extension() {
            // `write_tmcd` puts the timecode track in an .mp4 as well.
            "mp4" => vec!["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "-write_tmcd", "1"],
            _ => vec!["-c:a", "pcm_s16le"],
        });
        args
    }
}

/// Camera raw formats FFmpeg cannot read; their proxies have to come from
/// the maker's own software.
pub fn can_convert(clip: &Path) -> bool {
    const RAW: &[&str] = &["braw", "r3d", "ari", "arx", "crm", "nev"];
    clip.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| !RAW.iter().any(|raw| ext.eq_ignore_ascii_case(raw)))
}

/// Where the proxy of `clip` goes: `Proxy/<same name>.<mov or mp4>` beside it.
pub fn proxy_path(clip: &Path, preset: ProxyPreset) -> PathBuf {
    let folder = clip.parent().unwrap_or(Path::new("")).join(PROXY_DIR);
    let stem = clip.file_stem().unwrap_or_default().to_string_lossy();
    folder.join(format!("{stem}.{}", preset.extension()))
}

/// What FFmpeg says about a clip before touching it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipInfo {
    pub duration_secs: Option<f64>,
    /// Start timecode, wherever in the file the camera put it.
    pub timecode: Option<String>,
}

/// Reads duration and timecode out of FFmpeg's description of its input.
fn parse_info(text: &str) -> ClipInfo {
    let duration_secs = text.lines().find_map(|line| {
        let stamp = line.trim().strip_prefix("Duration:")?.split(',').next()?.trim();
        let mut parts = stamp.split(':').map(|part| part.parse::<f64>().ok());
        let (hours, minutes, seconds) = (parts.next()??, parts.next()??, parts.next()??);
        Some(hours * 3600.0 + minutes * 60.0 + seconds)
    });
    let timecode = text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        // 01:23:45:12, or 01:23:45;12 for drop-frame.
        let shaped = value.len() == 11 && value.chars().filter(char::is_ascii_digit).count() == 8;
        (key.trim() == "timecode" && shaped).then(|| value.to_string())
    });
    ClipInfo { duration_secs, timecode }
}

/// Seconds encoded so far, from a line of FFmpeg's `-progress` output.
fn parse_progress(line: &str) -> Option<f64> {
    // Despite the name, `out_time_ms` is in microseconds too.
    let value = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms="))?;
    Some(value.trim().parse::<f64>().ok()? / 1_000_000.0)
}

fn ffmpeg(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.args(["-hide_banner", "-nostdin"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up behind the app, and below normal
        // priority so encoding does not starve a copy that is running.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    command
}

/// The FFmpeg to use: the one shipped beside the app if there is one, else
/// one installed on the machine.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let beside_app = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join(name)));
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).map(|dir| dir.join(name)).collect::<Vec<_>>())
        .unwrap_or_default();
    // Where Homebrew puts it; an app started from the Finder has a bare PATH.
    let usual = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"].map(PathBuf::from);
    beside_app.into_iter().chain(on_path).chain(usual).find(|candidate| candidate.is_file())
}

/// Asks FFmpeg what a clip is. Fails if FFmpeg cannot read it at all.
pub fn inspect(program: &Path, clip: &Path) -> Result<ClipInfo> {
    let output = ffmpeg(program).arg("-i").arg(clip).output().at(program)?;
    let text = String::from_utf8_lossy(&output.stderr);
    if !text.contains("Stream #0") {
        let reason = text.lines().last().unwrap_or("FFmpeg could not read it").trim().to_string();
        return Err(Error::Proxy(reason));
    }
    Ok(parse_info(&text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyOutcome {
    Made,
    /// A proxy by that name was already there and was left alone.
    AlreadyThere,
}

/// Makes the proxy of `clip`. `on_progress` receives the fraction done.
///
/// The proxy is written under a temporary name and renamed when FFmpeg
/// finishes, so a file with the final name is always a whole one. Setting
/// `cancel` stops FFmpeg and removes the partial file.
pub fn make_proxy(
    program: &Path,
    clip: &Path,
    preset: ProxyPreset,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(f64),
) -> Result<ProxyOutcome> {
    let target = proxy_path(clip, preset);
    if std::fs::metadata(&target).is_ok_and(|meta| meta.len() > 0) {
        return Ok(ProxyOutcome::AlreadyThere);
    }
    let info = inspect(program, clip)?;
    let folder = target.parent().expect("proxy path has a folder");
    std::fs::create_dir_all(folder).at(folder)?;
    let mut part = target.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);

    let mut command = ffmpeg(program);
    command.arg("-y").arg("-i").arg(clip);
    // First picture stream and every sound stream; camera data streams
    // (Sony's `rtmd`, GoPro telemetry) do not fit these containers.
    command.args(["-map", "0:v:0", "-map", "0:a?", "-map_metadata", "0"]);
    if let Some(timecode) = &info.timecode {
        command.args(["-timecode", timecode]);
    }
    command.args(preset.encode_args());
    // The temporary name hides the extension, so name the container.
    command.args(["-f", preset.extension(), "-progress", "pipe:1", "-nostats"]).arg(&part);
    let mut child =
        command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().at(program)?;

    // Drain the log as it comes (a full pipe would stall FFmpeg) and keep it
    // for the error message.
    let mut log = child.stderr.take().expect("stderr is piped");
    let log_reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = log.read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    let progress = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut cancelled = false;
    for line in progress.lines().map_while(|line| line.ok()) {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            let _ = child.kill();
            break;
        }
        if let (Some(done), Some(total)) = (parse_progress(&line), info.duration_secs) {
            if total > 0.0 {
                on_progress((done / total).clamp(0.0, 1.0));
            }
        }
    }
    let status = child.wait().at(program)?;
    let log = log_reader.join().unwrap_or_default();

    // Nothing half made stays behind, nor a folder made for it alone
    // (removing a folder that still holds proxies simply fails).
    let discard = || {
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_dir(folder);
    };
    if cancelled || cancel.load(Ordering::Relaxed) {
        discard();
        return Err(Error::Cancelled);
    }
    if !status.success() {
        discard();
        let reason = log.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("FFmpeg failed");
        return Err(Error::Proxy(reason.trim().to_string()));
    }
    std::fs::rename(&part, &target).at(&target)?;
    on_progress(1.0);
    Ok(ProxyOutcome::Made)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: ProxyPreset = ProxyPreset { codec: ProxyCodec::ProresProxy, size: ProxySize::Hd };

    #[test]
    fn a_proxy_sits_in_a_proxy_folder_beside_its_clip_under_the_same_name() {
        let clip = Path::new("F:/Kestrel/08.11.26 Trail/Footage/Z6 III/DSC_0001.MOV");
        assert_eq!(proxy_path(clip, HD), Path::new("F:/Kestrel/08.11.26 Trail/Footage/Z6 III/Proxy/DSC_0001.mov"));
        let mp4 = ProxyPreset { codec: ProxyCodec::H264, size: ProxySize::Half };
        assert_eq!(
            proxy_path(Path::new("CLIP/C0001.MP4"), mp4),
            Path::new("CLIP/Proxy/C0001.mp4")
        );
    }

    #[test]
    fn reads_duration_and_timecode_as_a_sony_clip_reports_them() {
        // The timecode of an XAVC clip sits on its data stream, not on the picture.
        let text = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'C1310.MP4':
  Metadata:
    creation_time   : 2026-09-24T21:00:16.000000Z
  Duration: 00:00:15.36, start: 0.000000, bitrate: 524432 kb/s
  Stream #0:0[0x1](und): Video: h264 (High 4:2:2 Intra), yuv422p10le, 3840x2160, 50 fps
  Stream #0:2[0x3](und): Data: none (rtmd / 0x646D7472), 11878 kb/s (default)
      creation_time   : 2026-09-24T21:00:16.000000Z
      timecode        : 05:31:14:44";
        assert_eq!(
            parse_info(text),
            ClipInfo { duration_secs: Some(15.36), timecode: Some("05:31:14:44".into()) }
        );
        let drop_frame = "  Duration: 01:02:03.50, start: 0\n      timecode        : 00:59:58;02";
        assert_eq!(
            parse_info(drop_frame),
            ClipInfo { duration_secs: Some(3723.5), timecode: Some("00:59:58;02".into()) }
        );
        assert_eq!(parse_info("no such file"), ClipInfo { duration_secs: None, timecode: None });
    }

    #[test]
    fn reads_progress_in_seconds() {
        assert_eq!(parse_progress("out_time_us=7680000"), Some(7.68));
        assert_eq!(parse_progress("out_time_ms=1500000"), Some(1.5));
        assert_eq!(parse_progress("out_time_us=N/A"), None);
        assert_eq!(parse_progress("frame=120"), None);
    }

    #[test]
    fn camera_raw_formats_are_left_to_their_makers() {
        assert!(can_convert(Path::new("C0001.MP4")));
        assert!(can_convert(Path::new("A001C003.mxf")));
        assert!(!can_convert(Path::new("A001_0101.BRAW")));
        assert!(!can_convert(Path::new("A001_C001.R3D")));
        assert!(!can_convert(Path::new("NO_EXTENSION")));
    }

    fn preset(codec: ProxyCodec) -> ProxyPreset {
        ProxyPreset { codec, size: ProxySize::Half }
    }

    /// A two-second 4:2:2 10-bit clip with sound and a timecode, as a stand-in
    /// for camera footage. `None` when this machine has no FFmpeg.
    fn test_clip(dir: &Path) -> Option<(PathBuf, PathBuf)> {
        let program = find_ffmpeg()?;
        let clip = dir.join("A001C001.MOV");
        let made = ffmpeg(&program)
            .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le", "-c:a", "pcm_s16le"])
            .args(["-timecode", "10:20:30:05", "-loglevel", "error"])
            .arg(&clip)
            .status()
            .ok()?;
        made.success().then_some((program, clip))
    }

    #[test]
    fn every_codec_makes_a_playable_proxy_with_timecode_and_sound() {
        let dir = tempfile::TempDir::new().unwrap();
        let Some((program, clip)) = test_clip(dir.path()) else {
            eprintln!("no FFmpeg on this machine: proxy encoding not exercised");
            return;
        };
        for codec in [
            ProxyCodec::ProresProxy,
            ProxyCodec::ProresLt,
            ProxyCodec::H264,
            ProxyCodec::H265,
            ProxyCodec::DnxhrLb,
        ] {
            let seen = std::cell::Cell::new(0.0f64);
            let outcome =
                make_proxy(&program, &clip, preset(codec), &AtomicBool::new(false), &|f| seen.set(f)).unwrap();
            assert_eq!(outcome, ProxyOutcome::Made, "{codec:?}");
            assert_eq!(seen.get(), 1.0, "{codec:?}");

            let proxy = proxy_path(&clip, preset(codec));
            let about = ffmpeg(&program).arg("-i").arg(&proxy).output().unwrap();
            let about = String::from_utf8_lossy(&about.stderr);
            let info = parse_info(&about);
            assert!((info.duration_secs.unwrap() - 2.0).abs() < 0.2, "{codec:?}: {about}");
            assert_eq!(info.timecode.as_deref(), Some("10:20:30:05"), "{codec:?}: {about}");
            // Half of 1280x720, and the sound came along.
            assert!(about.contains("640x360"), "{codec:?}: {about}");
            assert!(about.contains("Audio:"), "{codec:?}: {about}");
            assert!(std::fs::metadata(&proxy).unwrap().len() < std::fs::metadata(&clip).unwrap().len());
            std::fs::remove_file(proxy).unwrap();
        }
    }

    #[test]
    fn an_existing_proxy_is_left_alone_and_a_stopped_one_leaves_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let Some((program, clip)) = test_clip(dir.path()) else {
            return;
        };
        let never = AtomicBool::new(false);
        assert_eq!(make_proxy(&program, &clip, HD, &never, &|_| {}).unwrap(), ProxyOutcome::Made);
        let first = std::fs::metadata(proxy_path(&clip, HD)).unwrap().modified().unwrap();
        assert_eq!(make_proxy(&program, &clip, HD, &never, &|_| {}).unwrap(), ProxyOutcome::AlreadyThere);
        assert_eq!(std::fs::metadata(proxy_path(&clip, HD)).unwrap().modified().unwrap(), first);

        // Asked to stop before it starts: no proxy, no partial file.
        let other = preset(ProxyCodec::H264);
        let stop = AtomicBool::new(true);
        assert!(matches!(make_proxy(&program, &clip, other, &stop, &|_| {}), Err(Error::Cancelled)));
        let left: Vec<_> = std::fs::read_dir(dir.path().join(PROXY_DIR)).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "{left:?}");

        // And where no proxy was made yet, no empty folder either.
        let elsewhere = dir.path().join("B");
        std::fs::create_dir(&elsewhere).unwrap();
        let alone = elsewhere.join("clip.mp4");
        std::fs::copy(&clip, &alone).unwrap();
        assert!(matches!(make_proxy(&program, &alone, other, &stop, &|_| {}), Err(Error::Cancelled)));
        assert!(!elsewhere.join(PROXY_DIR).exists());
    }

    #[test]
    fn a_file_ffmpeg_cannot_read_is_an_error_not_a_proxy() {
        let dir = tempfile::TempDir::new().unwrap();
        let Some(program) = find_ffmpeg() else {
            return;
        };
        let fake = dir.path().join("broken.mp4");
        std::fs::write(&fake, b"this is not a movie").unwrap();

        let err = make_proxy(&program, &fake, HD, &AtomicBool::new(false), &|_| {}).unwrap_err();

        assert!(matches!(err, Error::Proxy(_)), "{err}");
        assert!(!dir.path().join(PROXY_DIR).exists());
    }
}
