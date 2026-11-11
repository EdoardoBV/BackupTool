use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::scan::FileEntry;

/// Whether a source file appears to exist under a destination root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Missing,
    /// Something is at that path but its size differs from the source.
    SizeMismatch,
    /// Same relative path and same size. This is a quick look, not proof:
    /// only a checksum run can say the content matches.
    Present,
}

/// Fast pre-copy check: for each source file, is there a same-sized file at
/// its destination path under `dest_root`? Reads no file contents.
pub fn compare(files: &[FileEntry], dest_root: &Path) -> Vec<Presence> {
    files
        .iter()
        .map(|file| match fs::metadata(file.dest_in(dest_root)) {
            Ok(meta) if meta.is_file() && meta.len() == file.size => Presence::Present,
            Ok(_) => Presence::SizeMismatch,
            Err(_) => Presence::Missing,
        })
        .collect()
}
