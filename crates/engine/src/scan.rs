use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

use crate::error::{Error, Result};

/// One file on the source, addressed relative to the source root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Always forward slashes, whatever the platform.
    pub rel_path: String,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// Where the file goes under each destination root, when that differs
    /// from `rel_path` (a shoot layout sorts files into its own folders).
    #[serde(default)]
    pub dest: Option<String>,
}

impl FileEntry {
    /// The file's location on the source rooted at `root`.
    pub fn path_in(&self, root: &Path) -> PathBuf {
        let mut path = root.to_owned();
        path.extend(self.rel_path.split('/'));
        path
    }

    /// Path relative to a destination root.
    pub fn dest_rel(&self) -> &str {
        self.dest.as_deref().unwrap_or(&self.rel_path)
    }

    /// Where the copy of this file lives under the destination `root`.
    pub fn dest_in(&self, root: &Path) -> PathBuf {
        let mut path = root.to_owned();
        path.extend(self.dest_rel().split('/'));
        path
    }
}

/// Files and folders the operating systems scatter on removable media.
/// They are not footage and differ between machines, so they are never
/// copied and never count as "missing".
fn is_system_junk(name: &str) -> bool {
    const JUNK: &[&str] = &[
        ".DS_Store",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        ".TemporaryItems",
        ".DocumentRevisions-V100",
        ".apdisk",
        "System Volume Information",
        "$RECYCLE.BIN",
        "Thumbs.db",
        "desktop.ini",
    ];
    // "._name" files are macOS resource forks on non-Apple file systems.
    name.starts_with("._") || JUNK.iter().any(|junk| junk.eq_ignore_ascii_case(name))
}

fn keep(entry: &DirEntry) -> bool {
    // The root itself is always kept, even if it is named like junk.
    entry.depth() == 0 || !entry.file_name().to_str().is_some_and(is_system_junk)
}

/// Lists every file under `root`, sorted by path. `on_found` receives the
/// running file count so a UI can show "Reading 32 files…".
///
/// Any unreadable folder is an error: silently skipping part of a card would
/// make a later "everything is backed up" claim false.
pub fn scan(root: &Path, mut on_found: impl FnMut(usize)) -> Result<Vec<FileEntry>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_entry(keep) {
        let entry = entry.map_err(|err| {
            let path = err.path().unwrap_or(root).to_owned();
            let source = err
                .into_io_error()
                .unwrap_or_else(|| io::Error::other("file system loop"));
            Error::Io { path, source }
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(|err| Error::Io {
            path: entry.path().to_owned(),
            source: err.into_io_error().unwrap_or_else(|| io::Error::other("metadata unavailable")),
        })?;
        let relative = entry.path().strip_prefix(root).expect("walk stays under root");
        let mut parts = Vec::new();
        for component in relative.components() {
            let part = component.as_os_str().to_str().ok_or_else(|| Error::Io {
                path: entry.path().to_owned(),
                source: io::Error::new(io::ErrorKind::InvalidData, "file name is not valid Unicode"),
            })?;
            parts.push(part);
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_secs() as i64);
        files.push(FileEntry {
            rel_path: parts.join("/"),
            size: metadata.len(),
            modified,
            dest: None,
        });
        on_found(files.len());
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}
