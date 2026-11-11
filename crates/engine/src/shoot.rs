//! The folder structure of a shoot on a destination drive:
//!
//! ```text
//! <destination>/<Client>/<date> <Shoot name>/
//!     Footage/<Camera>/<Card>/...   clips
//!     Photo/<Camera>/<Card>/...     stills, only if the card has any
//!     Audio/<Camera>/<Card>/...     sound, only if the card has any
//!     Project/               empty, for the edit
//! ```
//!
//! Clients are not stored anywhere: they are the folders already on the
//! drive, so the drive itself stays the single source of truth.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::card::{kind_of, MediaKind};
use crate::error::{Error, IoContext, Result};
use crate::scan::FileEntry;

/// Folders every shoot gets even though nothing is copied into them.
pub const EMPTY_FOLDERS: &[&str] = &["Project"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateFormat {
    /// `08.11.26`
    DayMonthYear,
    /// `2026-11-08`, sorts chronologically by name.
    YearMonthDay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShootLayout {
    pub client: String,
    /// May be empty: the shoot folder is then named by its date alone.
    pub shoot: String,
    /// Shooting day as `YYYY-MM-DD`.
    pub date: String,
    pub date_format: DateFormat,
    /// Folder name for this card's camera; empty for no camera level.
    pub camera: String,
    /// Optional name for this card ("A002", "Card 2"), a level under the
    /// camera. Needed when two cards of one camera repeat file names.
    #[serde(default)]
    pub reel: String,
    /// Keep the card's own folders (`DCIM/100...`) under the camera folder
    /// instead of putting the files directly in it.
    pub keep_card_folders: bool,
}

/// Makes a name safe as a folder on Windows and macOS.
fn clean(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '-' } else { c })
        .collect();
    // Windows silently drops trailing dots and spaces; do it knowingly.
    replaced.trim().trim_end_matches(['.', ' ']).to_string()
}

fn parse_iso(date: &str) -> Option<(u32, u32, u32)> {
    let mut parts = date.split('-').map(|p| p.parse::<u32>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    let valid = parts.next().is_none() && year >= 1970 && (1..=12).contains(&month) && (1..=31).contains(&day);
    valid.then_some((year, month, day))
}

fn bucket(kind: MediaKind) -> Option<&'static str> {
    match kind {
        MediaKind::Video => Some("Footage"),
        MediaKind::Photo => Some("Photo"),
        MediaKind::Audio => Some("Audio"),
        MediaKind::Other => None,
    }
}

impl ShootLayout {
    /// `Client/08.11.26 Trail series`, relative to a destination root.
    pub fn shoot_dir(&self) -> Result<String> {
        let client = clean(&self.client);
        if client.is_empty() {
            return Err(Error::InvalidJob("the shoot needs a client name".into()));
        }
        let (year, month, day) = parse_iso(&self.date)
            .ok_or_else(|| Error::InvalidJob(format!("{} is not a valid date", self.date)))?;
        let date = match self.date_format {
            DateFormat::DayMonthYear => format!("{day:02}.{month:02}.{:02}", year % 100),
            DateFormat::YearMonthDay => format!("{year:04}-{month:02}-{day:02}"),
        };
        let shoot = clean(&self.shoot);
        let folder = if shoot.is_empty() { date } else { format!("{date} {shoot}") };
        Ok(format!("{client}/{folder}"))
    }

    fn media_dir(&self, shoot_dir: &str, bucket: &str) -> String {
        let mut dir = format!("{shoot_dir}/{bucket}");
        for level in [clean(&self.camera), clean(&self.reel)] {
            if !level.is_empty() {
                dir.push('/');
                dir.push_str(&level);
            }
        }
        dir
    }
}

/// Shortest tail of each path that keeps every file distinct: the bare file
/// name if possible, then with its parent folder, then the whole path.
fn flatten(paths: &[&str]) -> Vec<String> {
    for depth in 1..=2 {
        let tails: Vec<String> = paths
            .iter()
            .map(|path| {
                let parts: Vec<&str> = path.split('/').collect();
                parts[parts.len().saturating_sub(depth)..].join("/")
            })
            .collect();
        let distinct: HashSet<String> = tails.iter().map(|t| t.to_lowercase()).collect();
        if distinct.len() == tails.len() {
            return tails;
        }
    }
    paths.iter().map(|p| p.to_string()).collect()
}

/// Gives every file its place in the shoot. Clips, stills and sound go to
/// their own folders; everything else (sidecars, indexes) stays with the
/// card's main kind of media so the camera's own structure is not split up.
pub fn route(files: &mut [FileEntry], layout: &ShootLayout) -> Result<()> {
    let shoot_dir = layout.shoot_dir()?;
    let kinds: Vec<MediaKind> = files.iter().map(|f| kind_of(&f.rel_path)).collect();
    let main = [MediaKind::Video, MediaKind::Photo, MediaKind::Audio]
        .into_iter()
        .find(|kind| kinds.contains(kind))
        .unwrap_or(MediaKind::Video);

    let mut by_bucket: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, kind) in kinds.iter().enumerate() {
        let name = bucket(*kind).or(bucket(main)).expect("main kind has a bucket");
        by_bucket.entry(name).or_default().push(index);
    }
    for (name, indexes) in by_bucket {
        let dir = layout.media_dir(&shoot_dir, name);
        let sources: Vec<&str> = indexes.iter().map(|&i| files[i].rel_path.as_str()).collect();
        let inner = if layout.keep_card_folders {
            sources.iter().map(|s| s.to_string()).collect()
        } else {
            flatten(&sources)
        };
        for (index, inner) in indexes.into_iter().zip(inner) {
            files[index].dest = Some(format!("{dir}/{inner}"));
        }
    }
    Ok(())
}

/// One folder of the shoot, for showing the structure before it is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderLine {
    /// Relative to the destination root, forward slashes.
    pub path: String,
    pub exists: bool,
    /// Files that will be copied directly into this folder's tree; zero for
    /// the levels above the media folders.
    pub files: usize,
}

/// The folders a routed set of files needs under `dest_root`, top down.
pub fn folders(files: &[FileEntry], layout: &ShootLayout, dest_root: &Path) -> Result<Vec<FolderLine>> {
    let shoot_dir = layout.shoot_dir()?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for name in ["Footage", "Photo", "Audio"] {
        let dir = layout.media_dir(&shoot_dir, name);
        let prefix = format!("{dir}/");
        let count = files.iter().filter(|f| f.dest_rel().starts_with(&prefix)).count();
        if count > 0 {
            counts.insert(dir, count);
        }
    }
    let extra = EMPTY_FOLDERS.iter().map(|name| format!("{shoot_dir}/{name}"));
    for leaf in counts.keys().cloned().chain(extra) {
        // Every ancestor is a line of its own.
        let mut path = String::new();
        for part in leaf.split('/') {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            paths.insert(path.clone());
        }
    }
    // Media folders first, the empty ones last, each under its parents.
    let rank = |path: &str| {
        let under = path.strip_prefix(&shoot_dir).unwrap_or("").trim_start_matches('/');
        let top = under.split('/').next().unwrap_or("");
        ["", "Footage", "Photo", "Audio"].iter().position(|n| *n == top).unwrap_or(usize::MAX)
    };
    let mut ordered: Vec<String> = paths.into_iter().collect();
    ordered.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    Ok(ordered
        .into_iter()
        .map(|path| FolderLine {
            exists: path_under(dest_root, &path).is_dir(),
            files: counts.get(&path).copied().unwrap_or(0),
            path,
        })
        .collect())
}

fn path_under(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_owned();
    path.extend(rel.split('/'));
    path
}

/// Creates the shoot's folders on every destination. Existing folders are
/// left alone, so adding a second card to a shoot is the same operation.
pub fn create_folders(files: &[FileEntry], layout: &ShootLayout, dest_roots: &[PathBuf]) -> Result<()> {
    for root in dest_roots {
        for line in folders(files, layout, root)? {
            let path = path_under(root, &line.path);
            fs::create_dir_all(&path).at(&path)?;
        }
    }
    Ok(())
}

/// A client folder found on a drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Client {
    pub name: String,
    /// Years of the dated shoot folders inside, newest first.
    pub years: Vec<u32>,
}

/// Year of a shoot folder named `DD.MM.YY ...` or `YYYY-MM-DD ...`.
fn shoot_year(folder: &str) -> Option<u32> {
    let date = folder.split(' ').next()?;
    if let Some((year, _, _)) = parse_iso(date) {
        return Some(year);
    }
    let mut parts = date.split('.').map(|p| p.parse::<u32>().ok());
    let (day, month, year) = (parts.next()??, parts.next()??, parts.next()??);
    let valid = parts.next().is_none() && (1..=31).contains(&day) && (1..=12).contains(&month) && year < 100;
    valid.then_some(2000 + year)
}

fn subfolders(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with(['.', '$']) && name != "System Volume Information")
        .collect()
}

/// The clients already on a destination: its folders, most recently shot
/// first. An unreadable or missing destination simply has none.
pub fn clients(dest_root: &Path) -> Vec<Client> {
    let mut found: Vec<Client> = subfolders(dest_root)
        .into_iter()
        .map(|name| {
            let years: BTreeSet<u32> = subfolders(&dest_root.join(&name))
                .iter()
                .filter_map(|shoot| shoot_year(shoot))
                .collect();
            Client {
                name,
                years: years.into_iter().rev().collect(),
            }
        })
        .collect();
    found.sort_by(|a, b| {
        b.years.first().cmp(&a.years.first()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str) -> FileEntry {
        FileEntry {
            rel_path: rel.into(),
            size: 10,
            modified: None,
            dest: None,
        }
    }

    fn layout() -> ShootLayout {
        ShootLayout {
            client: "Kestrel Running".into(),
            shoot: "Trail series".into(),
            date: "2026-11-08".into(),
            date_format: DateFormat::DayMonthYear,
            camera: "Z6 III".into(),
            reel: String::new(),
            keep_card_folders: false,
        }
    }

    fn dests(files: &[FileEntry]) -> Vec<&str> {
        files.iter().map(FileEntry::dest_rel).collect()
    }

    #[test]
    fn names_the_shoot_folder_by_date_and_name() {
        assert_eq!(layout().shoot_dir().unwrap(), "Kestrel Running/08.11.26 Trail series");
        let sortable = ShootLayout { date_format: DateFormat::YearMonthDay, ..layout() };
        assert_eq!(sortable.shoot_dir().unwrap(), "Kestrel Running/2026-11-08 Trail series");
        let unnamed = ShootLayout { shoot: "  ".into(), ..layout() };
        assert_eq!(unnamed.shoot_dir().unwrap(), "Kestrel Running/08.11.26");
    }

    #[test]
    fn rejects_a_shoot_without_client_or_with_a_bad_date() {
        assert!(ShootLayout { client: " ".into(), ..layout() }.shoot_dir().is_err());
        assert!(ShootLayout { date: "08/11/2026".into(), ..layout() }.shoot_dir().is_err());
        assert!(ShootLayout { date: "2026-13-01".into(), ..layout() }.shoot_dir().is_err());
    }

    #[test]
    fn makes_names_safe_for_windows_and_mac() {
        let odd = ShootLayout {
            client: "Rossi / Bianchi: \"Sposi\"".into(),
            shoot: "Giorno 1?.".into(),
            ..layout()
        };
        assert_eq!(odd.shoot_dir().unwrap(), "Rossi - Bianchi- -Sposi-/08.11.26 Giorno 1-");
    }

    #[test]
    fn video_only_card_goes_flat_into_the_camera_folder() {
        let mut files = [file("DCIM/100NCZ_6/DSC_0001.MOV"), file("DCIM/100NCZ_6/DSC_0002.MOV")];
        route(&mut files, &layout()).unwrap();
        assert_eq!(
            dests(&files),
            [
                "Kestrel Running/08.11.26 Trail series/Footage/Z6 III/DSC_0001.MOV",
                "Kestrel Running/08.11.26 Trail series/Footage/Z6 III/DSC_0002.MOV",
            ]
        );
    }

    #[test]
    fn mixed_card_splits_clips_and_stills_and_sidecars_follow_the_clips() {
        let mut files = [
            file("DCIM/100MSDCF/DSC00001.ARW"),
            file("PRIVATE/M4ROOT/CLIP/C0001.MP4"),
            file("PRIVATE/M4ROOT/CLIP/C0001M01.XML"),
        ];
        route(&mut files, &layout()).unwrap();
        let base = "Kestrel Running/08.11.26 Trail series";
        assert_eq!(
            dests(&files),
            [
                format!("{base}/Photo/Z6 III/DSC00001.ARW"),
                format!("{base}/Footage/Z6 III/C0001.MP4"),
                format!("{base}/Footage/Z6 III/C0001M01.XML"),
            ]
        );
    }

    #[test]
    fn keeping_card_folders_preserves_the_camera_structure() {
        let mut files = [file("PRIVATE/M4ROOT/CLIP/C0001.MP4"), file("PRIVATE/M4ROOT/MEDIAPRO.XML")];
        let keep = ShootLayout { keep_card_folders: true, camera: "".into(), ..layout() };
        route(&mut files, &keep).unwrap();
        assert_eq!(
            dests(&files),
            [
                "Kestrel Running/08.11.26 Trail series/Footage/PRIVATE/M4ROOT/CLIP/C0001.MP4",
                "Kestrel Running/08.11.26 Trail series/Footage/PRIVATE/M4ROOT/MEDIAPRO.XML",
            ]
        );
    }

    #[test]
    fn same_file_name_in_two_card_folders_never_collides_when_flattened() {
        // The counter wrapped: the camera started a second folder.
        let mut files = [
            file("DCIM/100NCZ_6/DSC_0001.MOV"),
            file("DCIM/101NCZ_6/DSC_0001.MOV"),
            file("DCIM/101NCZ_6/dsc_0002.MOV"),
        ];
        route(&mut files, &layout()).unwrap();
        let base = "Kestrel Running/08.11.26 Trail series/Footage/Z6 III";
        assert_eq!(
            dests(&files),
            [
                format!("{base}/100NCZ_6/DSC_0001.MOV"),
                format!("{base}/101NCZ_6/DSC_0001.MOV"),
                format!("{base}/101NCZ_6/dsc_0002.MOV"),
            ]
        );
    }

    #[test]
    fn naming_the_card_keeps_two_cards_of_one_camera_apart() {
        let mut files = [file("DCIM/100NCZ_6/DSC_0001.MOV")];
        let second = ShootLayout { reel: "Card 2".into(), ..layout() };
        route(&mut files, &second).unwrap();
        assert_eq!(
            dests(&files),
            ["Kestrel Running/08.11.26 Trail series/Footage/Z6 III/Card 2/DSC_0001.MOV"]
        );
        let lines = folders(&files, &second, Path::new("nowhere")).unwrap();
        assert_eq!(lines.iter().find(|l| l.files == 1).unwrap().path, "Kestrel Running/08.11.26 Trail series/Footage/Z6 III/Card 2");
    }

    #[test]
    fn folder_list_shows_what_exists_and_where_the_files_go() {
        let drive = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(drive.path().join("Kestrel Running/01.03.25 Road")).unwrap();
        let mut files = [file("DCIM/100NCZ_6/DSC_0001.MOV"), file("DCIM/100NCZ_6/DSC_0002.MOV")];
        route(&mut files, &layout()).unwrap();

        let lines = folders(&files, &layout(), drive.path()).unwrap();

        let shown: Vec<(&str, bool, usize)> =
            lines.iter().map(|l| (l.path.as_str(), l.exists, l.files)).collect();
        assert_eq!(
            shown,
            [
                ("Kestrel Running", true, 0),
                ("Kestrel Running/08.11.26 Trail series", false, 0),
                ("Kestrel Running/08.11.26 Trail series/Footage", false, 0),
                ("Kestrel Running/08.11.26 Trail series/Footage/Z6 III", false, 2),
                // Video only, so no Photo folder.
                ("Kestrel Running/08.11.26 Trail series/Project", false, 0),
            ]
        );
    }

    #[test]
    fn creating_folders_is_repeatable_and_covers_every_destination() {
        let (a, b) = (tempfile::TempDir::new().unwrap(), tempfile::TempDir::new().unwrap());
        let roots = [a.path().to_owned(), b.path().to_owned()];
        let mut files = [file("DCIM/DSC_0001.MOV"), file("DCIM/DSC_0001.NEF")];
        route(&mut files, &layout()).unwrap();

        create_folders(&files, &layout(), &roots).unwrap();
        create_folders(&files, &layout(), &roots).unwrap();

        for root in &roots {
            let shoot = root.join("Kestrel Running/08.11.26 Trail series");
            assert!(shoot.join("Footage/Z6 III").is_dir());
            assert!(shoot.join("Photo/Z6 III").is_dir());
            assert!(shoot.join("Project").is_dir());
            assert!(!shoot.join("Audio").exists());
            assert!(folders(&files, &layout(), root).unwrap().iter().all(|line| line.exists));
        }
    }

    #[test]
    fn clients_are_the_folders_on_the_drive_newest_first() {
        let drive = tempfile::TempDir::new().unwrap();
        for dir in [
            "Kesh Studio/14.02.24 Lookbook",
            "Kestrel Running/08.11.26 Trail series",
            "Kestrel Running/2025-03-01 Road",
            "Kestrel Running/Loghi",
            "Archivio",
            ".Trashes/501",
        ] {
            fs::create_dir_all(drive.path().join(dir)).unwrap();
        }
        fs::write(drive.path().join("note.txt"), b"").unwrap();

        let found = clients(drive.path());

        assert_eq!(
            found,
            [
                Client { name: "Kestrel Running".into(), years: vec![2026, 2025] },
                Client { name: "Kesh Studio".into(), years: vec![2024] },
                Client { name: "Archivio".into(), years: vec![] },
            ]
        );
        assert!(clients(&drive.path().join("missing")).is_empty());
    }
}
