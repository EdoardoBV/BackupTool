//! Checking a backup that already exists against its ASC MHL history: every
//! file the history knows is read again and compared with the checksum first
//! recorded for it. The check can itself be recorded as a new generation, so
//! the history says when the files were last seen intact.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use walkdir::WalkDir;

use crate::copy::hash_file;
use crate::error::{Error, Result};
use crate::hash::HashAlgo;
use crate::mhl::{
    append_generation, load_history, path_under, Creator, Entry, Generation, Process, DEFAULT_IGNORE,
    HISTORY_DIR,
};
use crate::scan::scan;

const BUFFER: usize = 8 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// How far below the chosen folder histories are looked for: a drive, a
/// year, a client, a shoot.
const SEARCH_DEPTH: usize = 5;

/// A history found on disk, before anything is read.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryInfo {
    pub root: PathBuf,
    pub generations: usize,
    /// Files the history knows.
    pub files: usize,
    /// Size of those that are still there.
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mismatch {
    pub path: String,
    pub format: String,
    pub expected: String,
    pub actual: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub root: PathBuf,
    pub files: usize,
    /// Files read again and found identical to what was recorded.
    pub verified: usize,
    /// Files whose content is no longer what the history recorded.
    pub mismatched: Vec<Mismatch>,
    /// Files in the history that are no longer on disk.
    pub missing: Vec<String>,
    /// Files on disk the history has never heard of.
    pub untracked: Vec<String>,
    /// Files recorded with a kind of checksum this tool cannot compute.
    pub unsupported: Vec<String>,
    pub cancelled: bool,
    /// The generation that records this check, if one was written.
    pub recorded: Option<Generation>,
    pub duration_ms: u64,
}

impl VerifyReport {
    /// Everything the history knows is there and unchanged. Untracked files
    /// do not count against it: they are extra, not damaged.
    pub fn intact(&self) -> bool {
        !self.cancelled
            && self.files > 0
            && self.verified == self.files
            && self.mismatched.is_empty()
            && self.missing.is_empty()
            && self.unsupported.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerifyEvent {
    File { index: usize, total: usize, path: String },
    Progress { bytes_done: u64, bytes_total: u64 },
}

/// The folders under `folder` (itself included) that carry an ASC MHL history.
pub fn find_histories(folder: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = WalkDir::new(folder)
        .max_depth(SEARCH_DEPTH + 1)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir() && entry.file_name() == HISTORY_DIR)
        .filter_map(|entry| entry.path().parent().map(Path::to_owned))
        .collect();
    roots.sort();
    roots
}

/// What a history covers. Fails if its seal is broken.
pub fn summarize(root: &Path) -> Result<HistoryInfo> {
    let history = load_history(&root.join(HISTORY_DIR))?;
    let bytes = history
        .known
        .keys()
        .filter_map(|path| fs::metadata(path_under(root, path)).ok())
        .map(|meta| meta.len())
        .sum();
    Ok(HistoryInfo {
        root: root.to_owned(),
        generations: history.generations(),
        files: history.known.len(),
        bytes,
    })
}

/// `*` and `?` within one path component, as in an ignore pattern.
fn glob(pattern: &str, name: &str) -> bool {
    fn matches(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|skip| matches(rest, &name[skip..])),
            Some(('?', rest)) => !name.is_empty() && matches(rest, &name[1..]),
            Some((c, rest)) => name.first() == Some(c) && matches(rest, &name[1..]),
        }
    }
    let (pattern, name): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    matches(&pattern, &name)
}

/// Whether a file is left out by the history's ignore patterns. Covers the
/// forms histories use in practice: a name anywhere (`.DS_Store`), a folder
/// anywhere (`ascmhl/`), and a path from the root (`/Reports`).
fn ignored(patterns: &[String], rel_path: &str) -> bool {
    let parts: Vec<&str> = rel_path.split('/').collect();
    patterns.iter().any(|pattern| {
        let folder_only = pattern.ends_with('/');
        let body = pattern.trim_end_matches('/');
        let from_root = body.trim_start_matches('/');
        if body.starts_with('/') || from_root.contains('/') {
            let wanted: Vec<&str> = from_root.split('/').collect();
            let long_enough =
                if folder_only { parts.len() > wanted.len() } else { parts.len() >= wanted.len() };
            long_enough && wanted.iter().zip(&parts).all(|(want, part)| glob(want, part))
        } else {
            let last = parts.len() - 1;
            parts.iter().enumerate().any(|(i, part)| glob(body, part) && !(folder_only && i == last))
        }
    })
}

/// Reads every file the history at `root` knows and compares it with the
/// checksum first recorded for it. With `record`, the result is appended to
/// the history as a new generation. Nothing else on disk is changed.
pub fn verify_history(
    root: &Path,
    record: bool,
    creator: &Creator,
    cancel: &AtomicBool,
    on_event: &(dyn Fn(VerifyEvent) + Sync),
) -> Result<VerifyReport> {
    let started = Instant::now();
    let history = load_history(&root.join(HISTORY_DIR))?;
    if history.known.is_empty() {
        return Err(Error::Mhl(format!("{} has no ASC MHL history", root.display())));
    }
    let known: Vec<(&String, &Vec<(String, String)>)> = {
        let mut sorted: Vec<_> = history.known.iter().collect();
        sorted.sort();
        sorted
    };
    let sizes: Vec<Option<u64>> = known
        .iter()
        .map(|(path, _)| fs::metadata(path_under(root, path)).ok().map(|meta| meta.len()))
        .collect();
    let bytes_total: u64 = sizes.iter().flatten().sum();

    let mut report = VerifyReport {
        root: root.to_owned(),
        files: known.len(),
        verified: 0,
        mismatched: Vec::new(),
        missing: Vec::new(),
        untracked: Vec::new(),
        unsupported: Vec::new(),
        cancelled: false,
        recorded: None,
        duration_ms: 0,
    };
    // (path, algorithm, checksum found now) for the generation to record.
    let mut checked: Vec<(&str, HashAlgo, String)> = Vec::new();
    let mut bytes_before = 0u64;
    let last_progress: Mutex<Option<Instant>> = Mutex::new(None);

    for (index, ((path, formats), size)) in known.iter().zip(&sizes).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        on_event(VerifyEvent::File {
            index,
            total: known.len(),
            path: path.to_string(),
        });
        let Some(size) = size else {
            report.missing.push(path.to_string());
            continue;
        };
        // The first checksum recorded is the one the file is held to.
        let (format, expected) = &formats[0];
        let Ok(algo) = format.parse::<HashAlgo>() else {
            report.unsupported.push(path.to_string());
            bytes_before += size;
            continue;
        };
        let read = std::sync::atomic::AtomicU64::new(0);
        let progress = |chunk: u64| {
            let done = read.fetch_add(chunk, Ordering::Relaxed) + chunk;
            let mut last = last_progress.lock().unwrap();
            if last.is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
            drop(last);
            on_event(VerifyEvent::Progress {
                bytes_done: bytes_before + done,
                bytes_total,
            });
        };
        match hash_file(&path_under(root, path), algo, BUFFER, cancel, &progress) {
            Ok(actual) => {
                if actual == *expected {
                    report.verified += 1;
                } else {
                    report.mismatched.push(Mismatch {
                        path: path.to_string(),
                        format: format.clone(),
                        expected: expected.clone(),
                        actual: actual.clone(),
                    });
                }
                checked.push((path, algo, actual));
            }
            Err(Error::Cancelled) => {
                report.cancelled = true;
                break;
            }
            // A file that cannot be read is as good as gone.
            Err(_) => report.missing.push(path.to_string()),
        }
        bytes_before += size;
    }

    if !report.cancelled {
        on_event(VerifyEvent::Progress {
            bytes_done: bytes_total,
            bytes_total,
        });
        // Anything on disk the history does not cover, other histories
        // further down and their files excepted.
        let mut patterns: Vec<String> = DEFAULT_IGNORE.iter().map(|p| p.to_string()).collect();
        patterns.extend(history.ignore.iter().cloned());
        let on_disk = scan(root, |_| {})?;
        let nested: BTreeSet<String> = on_disk
            .iter()
            .filter_map(|file| {
                let parts: Vec<&str> = file.rel_path.split('/').collect();
                let at = parts.iter().position(|part| *part == HISTORY_DIR)?;
                (at > 0).then(|| format!("{}/", parts[..at].join("/")))
            })
            .collect();
        report.untracked = on_disk
            .iter()
            .map(|file| file.rel_path.as_str())
            .filter(|path| !history.known.contains_key(*path))
            .filter(|path| !ignored(&patterns, path))
            .filter(|path| !nested.iter().any(|prefix| path.starts_with(prefix.as_str())))
            .map(str::to_string)
            .collect();

        if record && !checked.is_empty() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64);
            let entries: Vec<Entry> = checked
                .iter()
                .map(|(path, algo, hash)| Entry {
                    path,
                    algo: *algo,
                    hash,
                    hashed_at: now,
                })
                .collect();
            report.recorded = append_generation(root, &entries, creator, &[], Process::InPlace)?;
        }
    }
    report.duration_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hasher;
    use tempfile::TempDir;

    fn creator() -> Creator {
        Creator {
            tool: "BackupTool".into(),
            version: "test".into(),
        }
    }

    fn digest(algo: HashAlgo, data: &[u8]) -> String {
        let mut hasher = Hasher::new(algo);
        hasher.update(data);
        hasher.finalize()
    }

    fn put(root: &Path, rel: &str, data: &[u8]) {
        let path = path_under(root, rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    /// A backup of three clips with a history, as a copy job leaves it.
    fn backup(algo: HashAlgo) -> TempDir {
        let dir = TempDir::new().unwrap();
        let files: [(&str, &[u8]); 3] =
            [("Footage/A001.MOV", b"first clip"), ("Footage/A002.MOV", b"second"), ("Photo/P1.NEF", b"still")];
        let hashes: Vec<String> = files.iter().map(|(_, data)| digest(algo, data)).collect();
        let entries: Vec<Entry> = files
            .iter()
            .zip(&hashes)
            .map(|((path, data), hash)| {
                put(dir.path(), path, data);
                Entry { path, algo, hash, hashed_at: None }
            })
            .collect();
        append_generation(dir.path(), &entries, &creator(), &["/Reports".to_string()], Process::Transfer)
            .unwrap();
        dir
    }

    fn verify(root: &Path, record: bool) -> VerifyReport {
        verify_history(root, record, &creator(), &AtomicBool::new(false), &|_| {}).unwrap()
    }

    #[test]
    fn an_untouched_backup_is_intact() {
        for algo in [HashAlgo::Xxh64, HashAlgo::Md5, HashAlgo::C4] {
            let dir = backup(algo);
            let report = verify(dir.path(), false);
            assert!(report.intact(), "{algo}");
            assert_eq!((report.files, report.verified), (3, 3));
            assert!(report.untracked.is_empty() && report.recorded.is_none());
            // Checking without recording leaves the history as it was.
            assert_eq!(fs::read_dir(dir.path().join("ascmhl")).unwrap().count(), 2);
        }
    }

    #[test]
    fn a_changed_file_and_a_missing_file_are_named() {
        let dir = backup(HashAlgo::Xxh64);
        // Same length, one different byte: silent corruption.
        put(dir.path(), "Footage/A002.MOV", b"sec0nd");
        fs::remove_file(dir.path().join("Photo/P1.NEF")).unwrap();

        let report = verify(dir.path(), false);

        assert!(!report.intact());
        assert_eq!(report.verified, 1);
        assert_eq!(report.missing, ["Photo/P1.NEF"]);
        assert_eq!(report.mismatched.len(), 1);
        let changed = &report.mismatched[0];
        assert_eq!(changed.path, "Footage/A002.MOV");
        assert_eq!(changed.expected, digest(HashAlgo::Xxh64, b"second"));
        assert_eq!(changed.actual, digest(HashAlgo::Xxh64, b"sec0nd"));
    }

    #[test]
    fn files_the_history_never_recorded_are_listed_but_ignored_ones_are_not() {
        let dir = backup(HashAlgo::Xxh64);
        put(dir.path(), "Footage/B001.MOV", b"added later");
        put(dir.path(), "Reports/2026 report.html", b"ours");
        put(dir.path(), ".DS_Store", b"junk");

        let report = verify(dir.path(), false);

        assert_eq!(report.untracked, ["Footage/B001.MOV"]);
        // Extra files do not make the recorded ones any less intact.
        assert!(report.intact());
    }

    #[test]
    fn recording_adds_a_generation_that_says_what_was_found() {
        let dir = backup(HashAlgo::Xxh64);
        put(dir.path(), "Footage/A002.MOV", b"sec0nd");

        let report = verify(dir.path(), true);

        let generation = report.recorded.unwrap();
        assert_eq!((generation.number, generation.verified), (2, 2));
        assert_eq!(generation.failed, ["Footage/A002.MOV"]);
        let xml = fs::read_to_string(generation.manifest).unwrap();
        assert!(xml.contains("<process>in-place</process>"));
        assert_eq!(xml.matches("action=\"failed\"").count(), 1);
        // The damaged file stays damaged on the next check: the history still
        // holds it to the checksum first recorded.
        assert_eq!(verify(dir.path(), false).mismatched.len(), 1);
    }

    #[test]
    fn a_tampered_history_is_refused() {
        let dir = backup(HashAlgo::Xxh64);
        let manifest = fs::read_dir(dir.path().join("ascmhl"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "mhl"))
            .unwrap();
        fs::write(&manifest, fs::read_to_string(&manifest).unwrap().replace("A001", "A009")).unwrap();

        let err = verify_history(dir.path(), false, &creator(), &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert!(matches!(err, Error::Mhl(_)), "{err}");
        assert!(summarize(dir.path()).is_err());
    }

    #[test]
    fn stopping_midway_reports_it_and_records_nothing() {
        let dir = backup(HashAlgo::Xxh64);
        let cancel = AtomicBool::new(false);
        let report = verify_history(dir.path(), true, &creator(), &cancel, &|event| {
            if matches!(event, VerifyEvent::File { index: 1, .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();

        assert!(report.cancelled && !report.intact());
        assert!(report.recorded.is_none());
        assert_eq!(fs::read_dir(dir.path().join("ascmhl")).unwrap().count(), 2);
    }

    #[test]
    fn histories_are_found_below_a_folder_and_summarised() {
        let drive = TempDir::new().unwrap();
        let shoot = backup(HashAlgo::Xxh64);
        let target = drive.path().join("2026/Kestrel/08.11.26 Trail");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::rename(shoot.path(), &target).unwrap();
        fs::create_dir_all(drive.path().join("2026/Other client")).unwrap();

        let found = find_histories(drive.path());

        assert_eq!(found, std::slice::from_ref(&target));
        let info = summarize(&target).unwrap();
        assert_eq!((info.generations, info.files), (1, 3));
        assert_eq!(info.bytes, (b"first clip".len() + b"second".len() + b"still".len()) as u64);
        assert!(find_histories(&drive.path().join("2026/Other client")).is_empty());
    }

    #[test]
    fn ignore_patterns_cover_names_folders_and_paths_from_the_root() {
        let patterns: Vec<String> =
            [".DS_Store", "ascmhl/", "/Reports", "*.tmp", "/Project/"].iter().map(|p| p.to_string()).collect();
        assert!(ignored(&patterns, ".DS_Store"));
        assert!(ignored(&patterns, "Footage/.DS_Store"));
        assert!(ignored(&patterns, "Footage/card/ascmhl/0001.mhl"));
        assert!(ignored(&patterns, "Reports/a.html"));
        assert!(ignored(&patterns, "Footage/render.tmp"));
        assert!(ignored(&patterns, "Project/edit.drp"));
        // Anchored to the root: a folder of the same name deeper down counts.
        assert!(!ignored(&patterns, "Footage/Reports/a.mov"));
        assert!(!ignored(&patterns, "Footage/Project/a.mov"));
        // A file that merely shares a folder pattern's name is not a folder.
        assert!(!ignored(&patterns, "Footage/ascmhl"));
        assert!(!ignored(&patterns, "Footage/A001.MOV"));
    }
}
