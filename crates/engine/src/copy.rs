//! Copies one file to any number of destinations and proves each copy.
//!
//! The source is read once and fanned out to one writer per destination.
//! Each destination is written to a `.part` file, read back from the device,
//! compared against the source checksum, and only then renamed into place:
//! a file with its final name is always a verified file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use filetime::FileTime;
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::hash::{HashAlgo, Hasher};
use crate::io::{create_for_write, AlignedBuf, SeqReader};
use crate::scan::FileEntry;

/// Chunks queued per consumer. Bounds memory to roughly
/// `buffer_size * QUEUE_DEPTH * (destinations + 1)` and lets a slow drive
/// throttle the reader instead of filling RAM.
const QUEUE_DEPTH: usize = 4;

type Chunk = Arc<Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// Copy only. Nothing is proven, so nothing is ever "safe to format".
    None,
    /// Read every copy back from its drive and compare with the source checksum.
    Destination,
    /// Also read the source a second time, to catch a card or reader that
    /// returns different data on each read.
    SourceAndDestination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExistingPolicy {
    /// Keep what is there and check it against the source. A match counts as
    /// backed up, which is also how an interrupted job resumes; a difference
    /// is an error and the existing file is left untouched.
    Verify,
    /// Replace whatever is at the destination path.
    Overwrite,
}

/// Fields left out when deserialising take their default value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyOptions {
    pub algo: HashAlgo,
    pub verify: VerifyMode,
    pub on_existing: ExistingPolicy,
    pub buffer_size: usize,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            algo: HashAlgo::Xxh64,
            verify: VerifyMode::Destination,
            on_existing: ExistingPolicy::Verify,
            buffer_size: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Copying,
    Verifying,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestStatus {
    Copied,
    AlreadyPresent,
    Failed,
}

/// What happened on one destination, in the order the destinations were given.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestOutcome {
    pub status: DestStatus,
    /// The file on this destination was read back and matched the source checksum.
    pub verified: bool,
    pub error: Option<String>,
}

impl DestOutcome {
    fn ok(status: DestStatus, verified: bool) -> Self {
        DestOutcome { status, verified, error: None }
    }

    fn failed(message: impl Into<String>) -> Self {
        DestOutcome {
            status: DestStatus::Failed,
            verified: false,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResult {
    pub rel_path: String,
    pub size: u64,
    /// Checksum of the source, when it was read to the end.
    pub hash: Option<String>,
    /// When the copies were confirmed, seconds since the Unix epoch.
    #[serde(default)]
    pub verified_at: Option<i64>,
    pub dests: Vec<DestOutcome>,
    /// A problem with the source itself; every destination failed with it.
    pub error: Option<String>,
}

impl FileResult {
    /// True when every destination holds a verified copy.
    pub fn is_safe(&self) -> bool {
        self.error.is_none() && !self.dests.is_empty() && self.dests.iter().all(|d| d.verified)
    }

    fn source_failure(entry: &FileEntry, dest_count: usize, message: String) -> Self {
        FileResult {
            rel_path: entry.rel_path.clone(),
            size: entry.size,
            hash: None,
            verified_at: None,
            dests: vec![DestOutcome::failed(message.clone()); dest_count],
            error: Some(message),
        }
    }
}

/// Checksums a file, reading from the device rather than the OS cache where
/// the platform allows. `on_bytes` receives the size of each chunk read.
pub fn hash_file(
    path: &Path,
    algo: HashAlgo,
    buffer_size: usize,
    cancel: &AtomicBool,
    on_bytes: &dyn Fn(u64),
) -> Result<String> {
    let mut reader = SeqReader::uncached(path).at(path)?;
    let mut buf = AlignedBuf::new(buffer_size);
    let mut hasher = Hasher::new(algo);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = reader.read_chunk(buf.as_mut_slice()).at(path)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf.as_mut_slice()[..n]);
        on_bytes(n as u64);
    }
    Ok(hasher.finalize())
}

enum Plan {
    Write { tmp: PathBuf, dest: PathBuf },
    Existing { dest: PathBuf },
    Failed(String),
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_owned();
    name.push(".part");
    dest.with_file_name(name)
}

fn plan_destination(dest: PathBuf, size: u64, policy: ExistingPolicy) -> Plan {
    match fs::metadata(&dest) {
        Ok(meta) if policy == ExistingPolicy::Verify => {
            if meta.is_file() && meta.len() == size {
                Plan::Existing { dest }
            } else {
                Plan::Failed(format!(
                    "a different file is already there ({} bytes, the source has {size})",
                    meta.len()
                ))
            }
        }
        Ok(_) => write_plan(dest),
        Err(err) if err.kind() == io::ErrorKind::NotFound => write_plan(dest),
        Err(err) => Plan::Failed(err.to_string()),
    }
}

fn write_plan(dest: PathBuf) -> Plan {
    if let Some(parent) = dest.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            return Plan::Failed(format!("cannot create {}: {err}", parent.display()));
        }
    }
    Plan::Write { tmp: part_path(&dest), dest }
}

struct ReadPass {
    hash: String,
    bytes: u64,
    /// One entry per `.part` path, in order.
    writes: Vec<io::Result<()>>,
}

/// Reads the source once, hashing it and feeding every writer in parallel.
/// With `stop_without_writers`, gives up early once every destination has
/// failed instead of reading the rest of the file for nothing.
fn read_and_write(
    src: &Path,
    part_paths: &[&Path],
    stop_without_writers: bool,
    opts: &CopyOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Phase, u64) + Sync),
) -> Result<ReadPass> {
    thread::scope(|scope| {
        let algo = opts.algo;
        let (hash_tx, hash_rx) = sync_channel::<Chunk>(QUEUE_DEPTH);
        let hashing = scope.spawn(move || {
            let mut hasher = Hasher::new(algo);
            for chunk in hash_rx {
                hasher.update(&chunk);
            }
            hasher.finalize()
        });

        let mut senders: Vec<Option<SyncSender<Chunk>>> = Vec::new();
        let mut writing = Vec::new();
        for &part in part_paths {
            let (tx, rx) = sync_channel::<Chunk>(QUEUE_DEPTH);
            senders.push(Some(tx));
            writing.push(scope.spawn(move || -> io::Result<()> {
                let mut file = create_for_write(part)?;
                for chunk in rx {
                    file.write_all(&chunk)?;
                }
                // Push the data to the device before it is read back.
                file.sync_all()
            }));
        }

        let read = (|| -> Result<u64> {
            let mut reader = SeqReader::buffered(src).at(src)?;
            let mut total = 0u64;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let mut buf = vec![0u8; opts.buffer_size];
                let n = reader.read_chunk(&mut buf).at(src)?;
                if n == 0 {
                    break;
                }
                buf.truncate(n);
                let chunk: Chunk = Arc::new(buf);
                // The hasher only stops when its channel closes.
                let _ = hash_tx.send(Arc::clone(&chunk));
                for sender in senders.iter_mut() {
                    // A failed writer drops its receiver; stop feeding it.
                    if sender.as_ref().is_some_and(|tx| tx.send(Arc::clone(&chunk)).is_err()) {
                        *sender = None;
                    }
                }
                total += n as u64;
                progress(Phase::Copying, total);
                if stop_without_writers && !senders.is_empty() && senders.iter().all(Option::is_none) {
                    break;
                }
            }
            Ok(total)
        })();

        // Closing the channels lets the worker threads finish.
        drop(hash_tx);
        drop(senders);
        let hash = hashing.join().expect("hasher thread panicked");
        let writes = writing
            .into_iter()
            .map(|handle| handle.join().expect("writer thread panicked"))
            .collect();
        read.map(|bytes| ReadPass { hash, bytes, writes })
    })
}

/// Moves a verified `.part` file into place, carrying over the source's
/// modification time (editors and cameras sort by it).
fn publish(src: &Path, tmp: &Path, dest: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::metadata(src) {
        // Best effort: a copy with a wrong timestamp is still a good copy.
        let _ = filetime::set_file_mtime(tmp, FileTime::from_last_modification_time(&meta));
    }
    fs::rename(tmp, dest)
}

/// Copies `entry` from `source_root` to its destination path under each of
/// `dest_roots`.
///
/// Problems with individual files or drives are reported inside the returned
/// [`FileResult`] so a job can carry on with the next file; the only error
/// returned is [`Error::Cancelled`]. `progress` receives the bytes processed
/// so far in the current phase and may be called from several threads.
pub fn copy_file(
    source_root: &Path,
    entry: &FileEntry,
    dest_roots: &[PathBuf],
    opts: &CopyOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Phase, u64) + Sync),
) -> Result<FileResult> {
    let src = entry.path_in(source_root);
    let plans: Vec<Plan> = dest_roots
        .iter()
        .map(|root| plan_destination(entry.dest_in(root), entry.size, opts.on_existing))
        .collect();
    let part_paths: Vec<&Path> = plans
        .iter()
        .filter_map(|plan| match plan {
            Plan::Write { tmp, .. } => Some(tmp.as_path()),
            _ => None,
        })
        .collect();
    let has_existing = plans.iter().any(|plan| matches!(plan, Plan::Existing { .. }));
    let remove_parts = || {
        for part in &part_paths {
            let _ = fs::remove_file(part);
        }
    };

    let mut outcomes: Vec<Option<DestOutcome>> = plans
        .iter()
        .map(|plan| match plan {
            Plan::Failed(message) => Some(DestOutcome::failed(message.clone())),
            _ => None,
        })
        .collect();
    let finish = |hash: Option<String>, outcomes: Vec<Option<DestOutcome>>| FileResult {
        rel_path: entry.rel_path.clone(),
        size: entry.size,
        hash,
        verified_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs() as i64),
        dests: outcomes.into_iter().map(|o| o.expect("every destination resolved")).collect(),
        error: None,
    };
    if outcomes.iter().all(Option::is_some) {
        // Nowhere to write and nothing to check: don't read the source at all.
        return Ok(finish(None, outcomes));
    }

    let pass = match read_and_write(&src, &part_paths, !has_existing, opts, cancel, progress) {
        Ok(pass) => pass,
        Err(Error::Cancelled) => {
            remove_parts();
            return Err(Error::Cancelled);
        }
        Err(err) => {
            remove_parts();
            return Ok(FileResult::source_failure(entry, plans.len(), err.to_string()));
        }
    };

    let mut writes = pass.writes.into_iter();
    for (plan, outcome) in plans.iter().zip(outcomes.iter_mut()) {
        if let Plan::Write { tmp, .. } = plan {
            if let Err(err) = writes.next().expect("one write result per .part file") {
                let _ = fs::remove_file(tmp);
                *outcome = Some(DestOutcome::failed(format!("write failed: {err}")));
            }
        }
    }
    if outcomes.iter().all(Option::is_some) {
        return Ok(finish(None, outcomes));
    }
    if pass.bytes != entry.size {
        remove_parts();
        return Ok(FileResult::source_failure(
            entry,
            plans.len(),
            format!(
                "the source changed while it was being copied ({} bytes expected, {} read)",
                entry.size, pass.bytes
            ),
        ));
    }
    let source_hash = pass.hash;

    // Read back whatever is still pending: fresh .part files and files that
    // were already at the destination.
    let mut to_check: Vec<(usize, &Path)> = Vec::new();
    for (index, plan) in plans.iter().enumerate() {
        if outcomes[index].is_some() {
            continue;
        }
        match plan {
            Plan::Write { tmp, dest } => {
                if opts.verify == VerifyMode::None {
                    outcomes[index] = Some(match publish(&src, tmp, dest) {
                        Ok(()) => DestOutcome::ok(DestStatus::Copied, false),
                        Err(err) => DestOutcome::failed(format!("cannot finish the copy: {err}")),
                    });
                } else {
                    to_check.push((index, tmp));
                }
            }
            Plan::Existing { dest } => {
                if opts.verify == VerifyMode::None {
                    outcomes[index] = Some(DestOutcome::ok(DestStatus::AlreadyPresent, false));
                } else {
                    to_check.push((index, dest));
                }
            }
            Plan::Failed(_) => unreachable!("failed plans are resolved up front"),
        }
    }

    let recheck_source = opts.verify == VerifyMode::SourceAndDestination && !to_check.is_empty();
    let readers = (to_check.len() + usize::from(recheck_source)).max(1) as u64;
    let verified_bytes = AtomicU64::new(0);
    let on_bytes = |n: u64| {
        let sum = verified_bytes.fetch_add(n, Ordering::Relaxed) + n;
        // All readers run in parallel; report their average as "the file".
        progress(Phase::Verifying, sum / readers);
    };
    let (dest_hashes, source_recheck) = thread::scope(|scope| {
        let handles: Vec<_> = to_check
            .iter()
            .map(|&(_, path)| {
                let on_bytes = &on_bytes;
                scope.spawn(move || hash_file(path, opts.algo, opts.buffer_size, cancel, on_bytes))
            })
            .collect();
        let source = recheck_source
            .then(|| hash_file(&src, opts.algo, opts.buffer_size, cancel, &on_bytes));
        let dests: Vec<Result<String>> = handles
            .into_iter()
            .map(|handle| handle.join().expect("verify thread panicked"))
            .collect();
        (dests, source)
    });

    let cancelled = cancel.load(Ordering::Relaxed)
        || dest_hashes.iter().any(|r| matches!(r, Err(Error::Cancelled)))
        || matches!(source_recheck, Some(Err(Error::Cancelled)));
    if cancelled {
        remove_parts();
        return Err(Error::Cancelled);
    }
    match source_recheck {
        Some(Err(err)) => {
            remove_parts();
            return Ok(FileResult::source_failure(entry, plans.len(), err.to_string()));
        }
        Some(Ok(second)) if second != source_hash => {
            remove_parts();
            return Ok(FileResult::source_failure(
                entry,
                plans.len(),
                format!(
                    "the source returned different data on a second read ({source_hash} then {second}); \
                     the card or reader is unreliable"
                ),
            ));
        }
        _ => {}
    }

    for ((index, _), dest_hash) in to_check.iter().zip(dest_hashes) {
        outcomes[*index] = Some(match (&plans[*index], dest_hash) {
            (Plan::Write { tmp, dest }, Ok(hash)) if hash == source_hash => {
                match publish(&src, tmp, dest) {
                    Ok(()) => DestOutcome::ok(DestStatus::Copied, true),
                    Err(err) => {
                        let _ = fs::remove_file(tmp);
                        DestOutcome::failed(format!("cannot finish the copy: {err}"))
                    }
                }
            }
            (Plan::Existing { .. }, Ok(hash)) if hash == source_hash => {
                DestOutcome::ok(DestStatus::AlreadyPresent, true)
            }
            (Plan::Write { tmp, .. }, Ok(hash)) => {
                let _ = fs::remove_file(tmp);
                DestOutcome::failed(format!(
                    "checksum mismatch after copying (source {source_hash}, copy {hash})"
                ))
            }
            (Plan::Existing { .. }, Ok(hash)) => DestOutcome::failed(format!(
                "the file already there differs from the source (source {source_hash}, existing {hash})"
            )),
            (plan, Err(err)) => {
                if let Plan::Write { tmp, .. } = plan {
                    let _ = fs::remove_file(tmp);
                }
                DestOutcome::failed(format!("cannot read the copy back: {err}"))
            }
            (Plan::Failed(_), Ok(_)) => unreachable!("failed plans are never checked"),
        });
    }

    Ok(finish(Some(source_hash), outcomes))
}
