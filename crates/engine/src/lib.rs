//! Card offload engine: scan a source, copy it to one or more destinations,
//! and prove every copy matches the source before calling the card safe.
//!
//! The engine never writes to the source and has no UI dependencies.

pub mod card;
pub mod compare;
pub mod copy;
pub mod error;
pub mod hash;
mod io;
pub mod job;
pub mod mhl;
pub mod report;
pub mod scan;
pub mod shoot;
pub mod verify;

pub use card::{analyze, kind_of, Camera, CardInfo, MediaKind};
pub use compare::{compare, Presence};
pub use copy::{
    copy_file, hash_file, CopyOptions, DestOutcome, DestStatus, ExistingPolicy, FileResult, Phase,
    VerifyMode,
};
pub use error::{Error, Result};
pub use hash::{HashAlgo, Hasher};
pub use job::{preflight, run_job, JobEvent, JobReport, JobSpec};
pub use scan::{scan, FileEntry};
pub use shoot::{clients, create_folders, folders, route, Client, DateFormat, FolderLine, ShootLayout};
