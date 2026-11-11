//! File access tuned for big sequential transfers, including reads that skip
//! the OS page cache. Verification is only meaningful if the bytes come back
//! from the device rather than from RAM that still holds what was just written.

use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

/// Covers both 512-byte and 4K-sector devices.
const ALIGN: usize = 4096;

/// Heap buffer aligned for unbuffered device I/O.
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

// Plain owned bytes; nothing ties the allocation to a thread.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    /// `size` is rounded up to a whole number of sectors.
    pub(crate) fn new(size: usize) -> Self {
        let size = size.max(ALIGN).div_ceil(ALIGN) * ALIGN;
        let layout = Layout::from_size_align(size, ALIGN).expect("valid buffer layout");
        // SAFETY: layout has non-zero size.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        AlignedBuf { ptr, layout }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for layout.size() initialised (zeroed) bytes
        // and uniquely borrowed through &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this exact layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    const ERROR_INVALID_PARAMETER: i32 = 87;

    pub fn open_sequential(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path)
    }

    /// Returns the file and whether it really bypasses the cache.
    pub fn open_uncached(path: &Path) -> io::Result<(File, bool)> {
        let direct = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path);
        match direct {
            Ok(file) => Ok((file, true)),
            // Some network and virtual file systems refuse unbuffered handles.
            Err(_) => open_sequential(path).map(|file| (file, false)),
        }
    }

    pub fn create(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path)
    }

    pub fn is_alignment_error(err: &io::Error) -> bool {
        err.raw_os_error() == Some(ERROR_INVALID_PARAMETER)
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    fn no_cache(file: &File) -> bool {
        // SAFETY: fcntl on a descriptor we own; F_NOCACHE takes an int.
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) != -1 }
    }

    pub fn open_sequential(path: &Path) -> io::Result<File> {
        File::open(path)
    }

    pub fn open_uncached(path: &Path) -> io::Result<(File, bool)> {
        let file = File::open(path)?;
        let direct = no_cache(&file);
        Ok((file, direct))
    }

    /// F_NOCACHE does not evict pages that are already cached, so the write
    /// side must skip the cache too or the verify pass would read RAM.
    pub fn create(path: &Path) -> io::Result<File> {
        let file = File::create(path)?;
        no_cache(&file);
        Ok(file)
    }

    pub fn is_alignment_error(_: &io::Error) -> bool {
        false
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub fn open_sequential(path: &Path) -> io::Result<File> {
        File::open(path)
    }

    pub fn open_uncached(path: &Path) -> io::Result<(File, bool)> {
        File::open(path).map(|file| (file, false))
    }

    pub fn create(path: &Path) -> io::Result<File> {
        File::create(path)
    }

    pub fn is_alignment_error(_: &io::Error) -> bool {
        false
    }
}

pub(crate) use sys::create as create_for_write;

/// Sequential reader that hands out whole chunks.
pub(crate) struct SeqReader {
    path: PathBuf,
    file: File,
    /// Unbuffered handles need sector-aligned buffers and offsets.
    direct: bool,
    eof: bool,
    started: bool,
}

impl SeqReader {
    /// Normal cached read with read-ahead; right for the first pass over a card.
    pub(crate) fn buffered(path: &Path) -> io::Result<Self> {
        Ok(SeqReader {
            path: path.to_owned(),
            file: sys::open_sequential(path)?,
            direct: false,
            eof: false,
            started: false,
        })
    }

    /// Reads straight from the device where the platform allows it. Callers
    /// must pass an [`AlignedBuf`] slice of the same length on every call.
    pub(crate) fn uncached(path: &Path) -> io::Result<Self> {
        let (file, direct) = sys::open_uncached(path)?;
        Ok(SeqReader {
            path: path.to_owned(),
            file,
            direct,
            eof: false,
            started: false,
        })
    }

    /// Fills `buf` as far as the file allows. Returns 0 at end of file; a
    /// short count only ever happens on the last chunk.
    pub(crate) fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        if !self.direct {
            let n = read_full(&mut self.file, buf)?;
            self.eof = n < buf.len();
            return Ok(n);
        }
        // One read per chunk: after a short read the file position is no
        // longer sector-aligned, so a follow-up read would be rejected.
        match self.file.read(buf) {
            Ok(n) => {
                self.started = true;
                self.eof = n < buf.len();
                Ok(n)
            }
            // The device wants a stricter alignment than ours. Nothing has
            // been consumed yet, so fall back to a cached handle.
            Err(err) if !self.started && sys::is_alignment_error(&err) => {
                self.file = sys::open_sequential(&self.path)?;
                self.direct = false;
                self.read_chunk(buf)
            }
            Err(err) => Err(err),
        }
    }
}

fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against the uncached path silently degrading to cached reads,
    /// which would make verification read back RAM instead of the drive.
    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn uncached_reader_bypasses_the_cache_and_reads_a_ragged_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("clip.mov");
        // Not a multiple of the sector size or of the buffer.
        let data: Vec<u8> = (0..150_001u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let mut reader = SeqReader::uncached(&path).unwrap();
        let mut buf = AlignedBuf::new(64 * 1024);
        let mut read_back = Vec::new();
        loop {
            let n = reader.read_chunk(buf.as_mut_slice()).unwrap();
            if n == 0 {
                break;
            }
            read_back.extend_from_slice(&buf.as_mut_slice()[..n]);
        }

        assert!(reader.direct, "fell back to cached reads");
        assert_eq!(read_back, data);
    }
}
