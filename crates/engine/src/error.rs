use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cancelled")]
    Cancelled,
    #[error("not enough space on {}: {needed} bytes needed, {available} available", dest.display())]
    NotEnoughSpace {
        dest: PathBuf,
        needed: u64,
        available: u64,
    },
    #[error("{0}")]
    InvalidJob(String),
    #[error("MHL history: {0}")]
    Mhl(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Attaches the path an I/O error happened on; a bare "access denied" is
/// useless when 131 files and three drives are involved.
pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })
    }
}
