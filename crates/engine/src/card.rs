//! What a card holds: how many clips, photos and sound files, when they
//! were shot, and which camera wrote them.
//!
//! Camera detection is a suggestion, never a requirement: every caller must
//! work when it returns nothing, because plenty of cameras leave no metadata
//! this module knows how to read.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::scan::FileEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Photo,
    Audio,
    /// Sidecars, indexes, thumbnails: whatever the camera keeps next to the media.
    Other,
}

pub fn kind_of(rel_path: &str) -> MediaKind {
    const VIDEO: &[&str] = &[
        "mov", "mp4", "m4v", "mxf", "mts", "m2ts", "avi", "braw", "r3d", "ari", "arx", "crm",
        "insv", "360", "mkv", "mpg", "webm",
    ];
    const PHOTO: &[&str] = &[
        "jpg", "jpeg", "heic", "heif", "png", "tif", "tiff", "arw", "nef", "nrw", "cr2", "cr3",
        "raf", "orf", "rw2", "dng", "srw", "pef", "3fr", "iiq", "gpr", "x3f",
    ];
    const AUDIO: &[&str] = &["wav", "bwf", "mp3", "aif", "aiff", "flac", "m4a"];

    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => return MediaKind::Other,
    };
    let ext = ext.as_str();
    if VIDEO.contains(&ext) {
        MediaKind::Video
    } else if PHOTO.contains(&ext) {
        MediaKind::Photo
    } else if AUDIO.contains(&ext) {
        MediaKind::Audio
    } else {
        MediaKind::Other
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Camera {
    pub make: Option<String>,
    pub model: String,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CardInfo {
    pub video: usize,
    pub photo: usize,
    pub audio: usize,
    pub other: usize,
    /// Median modification time of the media files, seconds since the Unix
    /// epoch. The median ignores a stray file with a wrong clock.
    pub shot_at: Option<i64>,
    pub camera: Option<Camera>,
    /// The card uses a folder structure that software expects to find intact
    /// (Sony XAVC/XDCAM, Canon XF, RED, AVCHD), unlike a plain `DCIM` card.
    pub structured: bool,
}

pub fn analyze(root: &Path, files: &[FileEntry]) -> CardInfo {
    let mut info = CardInfo {
        video: 0,
        photo: 0,
        audio: 0,
        other: 0,
        shot_at: None,
        camera: None,
        structured: files.iter().any(|f| is_structured(&f.rel_path)),
    };
    let mut times = Vec::new();
    for file in files {
        let kind = kind_of(&file.rel_path);
        match kind {
            MediaKind::Video => info.video += 1,
            MediaKind::Photo => info.photo += 1,
            MediaKind::Audio => info.audio += 1,
            MediaKind::Other => info.other += 1,
        }
        if kind != MediaKind::Other {
            times.extend(file.modified);
        }
    }
    times.sort_unstable();
    info.shot_at = times.get(times.len() / 2).copied();
    info.camera = detect_camera(root, files);
    info
}

fn is_structured(rel_path: &str) -> bool {
    const MARKERS: &[&str] = &["M4ROOT/", "XDROOT/", "CONTENTS/", "CLIPS001/", "AVCHD/", "BDMV/", ".RDC/", ".RDM/"];
    let path = rel_path.to_ascii_uppercase();
    MARKERS.iter().any(|marker| path.contains(marker))
}

/// Whether the top level of a volume holds the folders a camera or recorder
/// writes. Tells a card apart from an ordinary external drive.
pub fn looks_like_card(root: &Path) -> bool {
    const FOLDERS: &[&str] = &[
        "DCIM", "PRIVATE", "AVCHD", "XDROOT", "CONTENTS", "CLIP", "BPAV", "MP_ROOT", "M4ROOT",
    ];
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_ascii_uppercase();
        // RED magazines carry a `.RDM` folder at the top.
        FOLDERS.contains(&name.as_str()) || name.ends_with(".RDM")
    })
}

/// Files of each sort to try before giving up; a card is written by one
/// camera, so a few are enough.
const TRIES: usize = 3;

fn has_ext(file: &FileEntry, exts: &[&str]) -> bool {
    file.rel_path
        .rsplit_once('.')
        .is_some_and(|(_, ext)| exts.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn detect_camera(root: &Path, files: &[FileEntry]) -> Option<Camera> {
    // Cheapest first: a Sony sidecar is a few kB of text.
    let sidecars = files.iter().filter(|f| f.rel_path.to_ascii_uppercase().ends_with("M01.XML"));
    let clips = files.iter().filter(|f| has_ext(f, &["mp4", "mov", "m4v"]));
    // JPEGs before raw files: raw files are read whole.
    let jpegs = files.iter().filter(|f| has_ext(f, &["jpg", "jpeg"]));
    let raws = files
        .iter()
        .filter(|f| kind_of(&f.rel_path) == MediaKind::Photo && !has_ext(f, &["jpg", "jpeg"]));

    sidecars
        .take(TRIES)
        .find_map(|f| sony_sidecar(&f.path_in(root)))
        .or_else(|| clips.take(TRIES).find_map(|f| movie_camera(&f.path_in(root))))
        .or_else(|| jpegs.take(TRIES).find_map(|f| exif_camera(&f.path_in(root))))
        .or_else(|| raws.take(TRIES).find_map(|f| exif_camera(&f.path_in(root))))
}

fn text(bytes: &[u8]) -> Option<String> {
    let value = String::from_utf8_lossy(bytes);
    let value = value.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!value.is_empty()).then(|| value.to_string())
}

/// Cameras that don't know their serial write a placeholder.
fn real_serial(serial: Option<String>) -> Option<String> {
    serial.filter(|s| s != "4294967295" && s.chars().any(|c| c != '0'))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn be32(data: &[u8], at: usize) -> Option<usize> {
    let bytes = data.get(at..at + 4)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?) as usize)
}

// ---- Sony --------------------------------------------------------------

/// `<Device manufacturer="Sony" modelName="ILCE-7SM3" serialNo="..."/>`, as
/// found in the `M01.XML` sidecars and inside the clips themselves.
fn sony_device(data: &[u8]) -> Option<Camera> {
    let start = find(data, b"<Device ")?;
    let end = start + data[start..].iter().position(|&b| b == b'>')?;
    let tag = String::from_utf8_lossy(&data[start..end]);
    let attr = |name: &str| {
        let from = tag.find(&format!("{name}=\""))? + name.len() + 2;
        let len = tag[from..].find('"')?;
        text(tag[from..from + len].as_bytes())
    };
    Some(Camera {
        make: attr("manufacturer"),
        model: attr("modelName")?,
        serial: real_serial(attr("serialNo")),
    })
}

fn sony_sidecar(path: &Path) -> Option<Camera> {
    let mut data = Vec::new();
    File::open(path).ok()?.take(256 * 1024).read_to_end(&mut data).ok()?;
    sony_device(&data)
}

// ---- QuickTime / MP4 ---------------------------------------------------

/// Metadata boxes are small; anything bigger is not one we want in memory.
const MAX_BOX: u64 = 32 * 1024 * 1024;

/// Walks the top-level boxes of a QuickTime or MP4 file and looks for a
/// camera in the ones that carry metadata. Never reads the media data.
fn movie_camera(path: &Path) -> Option<Camera> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut offset = 0u64;
    while offset + 8 <= len {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let mut size = u64::from(u32::from_be_bytes(header[..4].try_into().unwrap()));
        let mut header_len = 8u64;
        if size == 1 {
            file.read_exact(&mut header[8..]).ok()?;
            size = u64::from_be_bytes(header[8..].try_into().unwrap());
            header_len = 16;
        } else if size == 0 {
            size = len - offset;
        }
        if size < header_len {
            return None;
        }
        if matches!(&header[4..8], b"moov" | b"meta" | b"uuid" | b"udta") && size <= MAX_BOX {
            let mut body = vec![0u8; (size - header_len) as usize];
            file.read_exact(&mut body).ok()?;
            let found = sony_device(&body)
                .or_else(|| quicktime_keys(&body))
                .or_else(|| quicktime_user_data(&body));
            if found.is_some() {
                return found;
            }
        }
        offset += size;
    }
    None
}

/// Apple-style metadata: a `keys` box naming each entry and an `ilst` box
/// holding the values, linked by 1-based index.
fn quicktime_keys(data: &[u8]) -> Option<Camera> {
    let keys = find(data, b"keys")?.checked_sub(4)?;
    let keys_end = (keys + be32(data, keys)?).min(data.len());
    let count = be32(data, keys + 12)?;
    let mut names = Vec::new();
    let mut pos = keys + 16;
    for _ in 0..count.min(256) {
        let size = be32(data, pos)?;
        if size < 8 || pos + size > keys_end {
            break;
        }
        names.push(String::from_utf8_lossy(&data[pos + 8..pos + size]).into_owned());
        pos += size;
    }

    let list = find(data, b"ilst")?.checked_sub(4)?;
    let list_end = (list + be32(data, list)?).min(data.len());
    let (mut make, mut model) = (None, None);
    let mut pos = list + 8;
    while pos + 8 <= list_end {
        let size = be32(data, pos)?;
        if size < 8 || pos + size > list_end {
            break;
        }
        let index = be32(data, pos + 4)?;
        let body = &data[pos + 8..pos + size];
        // Value box: size, "data", type, locale, then the value itself.
        if body.len() > 16 && &body[4..8] == b"data" {
            match index.checked_sub(1).and_then(|i| names.get(i)).map(String::as_str) {
                Some("com.apple.quicktime.model") => model = text(&body[16..]),
                Some("com.apple.quicktime.make") => make = text(&body[16..]),
                _ => {}
            }
        }
        pos += size;
    }
    Some(Camera { make, model: model?, serial: None })
}

/// Classic QuickTime user data: `©mod` and `©mak` text entries.
fn quicktime_user_data(data: &[u8]) -> Option<Camera> {
    let entry = |fourcc: &[u8; 4]| {
        let start = find(data, fourcc)?.checked_sub(4)?;
        let end = (start + be32(data, start)?).min(data.len());
        let body = data.get(start + 8..end)?;
        if body.len() > 16 && &body[4..8] == b"data" {
            // iTunes flavour: the text sits in a nested value box.
            text(&body[16..])
        } else {
            // QuickTime flavour: 16-bit length, 16-bit language, text.
            let len = usize::from(u16::from_be_bytes(body.get(..2)?.try_into().ok()?));
            text(body.get(4..4 + len)?)
        }
    };
    Some(Camera {
        make: entry(b"\xA9mak"),
        model: entry(b"\xA9mod")?,
        serial: None,
    })
}

// ---- Photos ------------------------------------------------------------

/// EXIF from a JPEG or a TIFF-based raw file (NEF, ARW, CR2, DNG...).
fn exif_camera(path: &Path) -> Option<Camera> {
    let file = File::open(path).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut BufReader::new(file)).ok()?;
    let field = |tag| match &exif.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(parts) => text(parts.first()?),
        _ => None,
    };
    Some(Camera {
        make: field(exif::Tag::Make),
        model: field(exif::Tag::Model)?,
        serial: real_serial(field(exif::Tag::BodySerialNumber)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn movie(metadata_box: Vec<u8>) -> Vec<u8> {
        let mut file = boxed(b"ftyp", b"qt  \0\0\0\0qt  ");
        // Media data comes first, as cameras write it, and must be skipped unread.
        file.extend(boxed(b"mdat", &[0x55; 5000]));
        file.extend(metadata_box);
        file
    }

    fn camera_in(bytes: &[u8]) -> Option<Camera> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("clip.mov");
        std::fs::write(&path, bytes).unwrap();
        movie_camera(&path)
    }

    #[test]
    fn tells_a_card_from_an_ordinary_drive() {
        let card = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(card.path().join("dcim/100MSDCF")).unwrap();
        let red = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(red.path().join("A001_0101XY.RDM")).unwrap();
        let drive = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(drive.path().join("2026/Client work")).unwrap();

        assert!(looks_like_card(card.path()));
        assert!(looks_like_card(red.path()));
        assert!(!looks_like_card(drive.path()));
        assert!(!looks_like_card(&drive.path().join("not there")));
    }

    #[test]
    fn sorts_files_into_kinds_whatever_the_case() {
        assert_eq!(kind_of("DCIM/100NCZ_6/DSC_0001.MOV"), MediaKind::Video);
        assert_eq!(kind_of("PRIVATE/M4ROOT/CLIP/C0001.mp4"), MediaKind::Video);
        assert_eq!(kind_of("DCIM/100MSDCF/DSC00001.ARW"), MediaKind::Photo);
        assert_eq!(kind_of("ZOOM0001/Tr1.WAV"), MediaKind::Audio);
        assert_eq!(kind_of("PRIVATE/M4ROOT/CLIP/C0001M01.XML"), MediaKind::Other);
        assert_eq!(kind_of("PRIVATE/SONY/SONYCARD"), MediaKind::Other);
    }

    #[test]
    fn reads_the_camera_from_a_sony_sidecar() {
        // The Device line as written by an ILCE-7SM3.
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.20">
	<Duration value="768"/>
	<Device manufacturer="Sony" modelName="ILCE-7SM3" serialNo="4294967295"/>
</NonRealTimeMeta>"#;
        assert_eq!(
            sony_device(xml),
            Some(Camera {
                make: Some("Sony".into()),
                model: "ILCE-7SM3".into(),
                // 0xFFFFFFFF is the camera saying "unknown".
                serial: None,
            })
        );
        let with_serial = br#"<Device manufacturer="Sony" modelName="ILME-FX6V" serialNo="0012345"/>"#;
        assert_eq!(sony_device(with_serial).unwrap().serial.as_deref(), Some("0012345"));
    }

    #[test]
    fn reads_the_camera_from_sony_xml_inside_the_clip() {
        let xml = br#"<NonRealTimeMeta><Device manufacturer="Sony" modelName="ILCE-7M4" serialNo="7654321"/></NonRealTimeMeta>"#;
        let mut body = vec![0, 0, 0, 0];
        body.extend(boxed(b"xml ", xml));
        let camera = camera_in(&movie(boxed(b"meta", &body))).unwrap();
        assert_eq!(camera.model, "ILCE-7M4");
        assert_eq!(camera.serial.as_deref(), Some("7654321"));
    }

    #[test]
    fn reads_the_camera_from_apple_style_metadata_keys() {
        let key = |name: &str| {
            let mut entry = ((name.len() + 8) as u32).to_be_bytes().to_vec();
            entry.extend_from_slice(b"mdta");
            entry.extend_from_slice(name.as_bytes());
            entry
        };
        let value = |index: u32, text: &str| {
            let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0];
            data.extend_from_slice(text.as_bytes());
            let mut item = ((8 + 8 + data.len()) as u32).to_be_bytes().to_vec();
            item.extend_from_slice(&index.to_be_bytes());
            item.extend(boxed(b"data", &data));
            item
        };
        let mut keys = vec![0, 0, 0, 0, 0, 0, 0, 2];
        keys.extend(key("com.apple.quicktime.make"));
        keys.extend(key("com.apple.quicktime.model"));
        let mut list = value(1, "Apple");
        list.extend(value(2, "iPhone 17 Pro"));
        let mut meta = boxed(b"keys", &keys);
        meta.extend(boxed(b"ilst", &list));

        let camera = camera_in(&movie(boxed(b"moov", &boxed(b"meta", &meta)))).unwrap();

        assert_eq!(camera.make.as_deref(), Some("Apple"));
        assert_eq!(camera.model, "iPhone 17 Pro");
    }

    #[test]
    fn reads_the_camera_from_classic_quicktime_user_data() {
        let entry = |kind: &[u8; 4], text: &str| {
            let mut body = (text.len() as u16).to_be_bytes().to_vec();
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(text.as_bytes());
            boxed(kind, &body)
        };
        let mut udta = entry(b"\xA9mak", "FUJIFILM");
        udta.extend(entry(b"\xA9mod", "X-H2S"));

        let camera = camera_in(&movie(boxed(b"moov", &boxed(b"udta", &udta)))).unwrap();

        assert_eq!(camera.make.as_deref(), Some("FUJIFILM"));
        assert_eq!(camera.model, "X-H2S");
    }

    #[test]
    fn a_clip_without_camera_metadata_gives_nothing() {
        assert_eq!(camera_in(&movie(boxed(b"moov", &boxed(b"mvhd", &[0; 100])))), None);
        assert_eq!(camera_in(b"not a movie at all"), None);
        assert_eq!(camera_in(&[]), None);
    }

    /// Minimal little-endian TIFF, the container of NEF, ARW, CR2 and DNG.
    fn tiff_with_exif(make: &str, model: &str, serial: &str) -> Vec<u8> {
        let ascii = |s: &str| [s.as_bytes(), &[0]].concat();
        let (make, model, serial) = (ascii(make), ascii(model), ascii(serial));
        let ifd0_at = 8u32;
        let exif_at = ifd0_at + 2 + 3 * 12 + 4;
        let make_at = exif_at + 2 + 12 + 4;
        let model_at = make_at + make.len() as u32;
        let serial_at = model_at + model.len() as u32;
        let entry = |tag: u16, kind: u16, count: u32, value: u32| {
            [&tag.to_le_bytes()[..], &kind.to_le_bytes(), &count.to_le_bytes(), &value.to_le_bytes()]
                .concat()
        };
        let mut out = b"II\x2A\x00".to_vec();
        out.extend(ifd0_at.to_le_bytes());
        out.extend(3u16.to_le_bytes());
        out.extend(entry(0x010F, 2, make.len() as u32, make_at));
        out.extend(entry(0x0110, 2, model.len() as u32, model_at));
        out.extend(entry(0x8769, 4, 1, exif_at));
        out.extend(0u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(entry(0xA431, 2, serial.len() as u32, serial_at));
        out.extend(0u32.to_le_bytes());
        out.extend(make);
        out.extend(model);
        out.extend(serial);
        out
    }

    #[test]
    fn reads_camera_and_serial_from_a_raw_photo() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("DSC_0001.NEF");
        std::fs::write(&path, tiff_with_exif("NIKON CORPORATION", "NIKON Z6_3", "6001234")).unwrap();

        assert_eq!(
            exif_camera(&path),
            Some(Camera {
                make: Some("NIKON CORPORATION".into()),
                model: "NIKON Z6_3".into(),
                serial: Some("6001234".into()),
            })
        );
    }

    #[test]
    fn analysis_counts_kinds_and_dates_the_card_by_its_media() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let file = |rel: &str, modified: i64| FileEntry {
            rel_path: rel.into(),
            size: 1,
            modified: Some(modified),
            dest: None,
        };
        std::fs::create_dir_all(root.join("CLIP")).unwrap();
        std::fs::write(
            root.join("CLIP/C0001M01.XML"),
            br#"<Device manufacturer="Sony" modelName="ILCE-7SM3" serialNo="42"/>"#,
        )
        .unwrap();
        let files = [
            file("CLIP/C0001.MP4", 1_000),
            file("CLIP/C0001M01.XML", 9_999_999),
            file("CLIP/C0002.MP4", 2_000),
            file("CLIP/C0003.MP4", 3_000),
            file("DCIM/DSC0001.JPG", 2_500),
        ];

        let info = analyze(root, &files);

        assert_eq!((info.video, info.photo, info.audio, info.other), (3, 1, 0, 1));
        // The sidecar's odd timestamp does not count: it is not media.
        assert_eq!(info.shot_at, Some(2_500));
        assert_eq!(info.camera.unwrap().serial.as_deref(), Some("42"));
        assert!(!info.structured);
        let sony = [file("PRIVATE/M4ROOT/CLIP/C0001.MP4", 1)];
        assert!(analyze(root, &sony).structured);
    }
}
