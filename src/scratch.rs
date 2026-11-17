//! Scratch files: an upload, and the samples ffmpeg decodes from it, held
//! on disk rather than in the server's memory, where on a small board they
//! could only be swapped out. A file's pages are the page cache's, which
//! the kernel writes back and lets go of as memory runs short; an hour's
//! samples are 230 MB, and an upload up to --max-upload-mb.
//!
//! The folder is pktito-c's (libpktito's pk_os_temp): TMPDIR, else
//! /var/tmp, else /tmp, the first a file can be made in. A tmpfs there is
//! memory again, so it should be a disk (DOCKER.md). A file has no name
//! once open: Linux makes it with none (O_TMPFILE), elsewhere it is
//! unlinked at once, so that it goes when the server lets it go or stops,
//! however it stops.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// The folders tried, in order
fn folders() -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = std::env::var_os("TMPDIR")
        .filter(|t| !t.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    d.push("/var/tmp".into());
    d.push("/tmp".into());
    d
}

/// An empty scratch file, open to read and write, with no name, mode 0
/// (no_access)
pub fn file() -> io::Result<File> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no folder for a scratch file");
    for dir in folders() {
        match open_in(&dir) {
            Ok(f) => {
                no_access(&f)?;
                return Ok(f);
            }
            Err(e) => last = io::Error::new(e.kind(), format!("{}: {e}", dir.display())),
        }
    }
    Err(last)
}

/// The folder scratch files go to, checked at the server's start by making
/// one there; and whether it is a tmpfs, whose files are memory
pub fn folder() -> Result<(PathBuf, bool), String> {
    let mut why = Vec::new();
    for dir in folders() {
        match open_in(&dir) {
            Ok(_) => {
                let tmpfs = is_tmpfs(&dir);
                return Ok((dir, tmpfs));
            }
            Err(e) => why.push(format!("{}: {e}", dir.display())),
        }
    }
    Err(why.join("; "))
}

#[cfg(target_os = "linux")]
fn is_tmpfs(dir: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(p) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(p.as_ptr(), &mut st) } != 0 {
        return false;
    }
    // f_type and the magic are i64 on x86-64, other types elsewhere
    #[allow(clippy::unnecessary_cast)]
    let tmpfs = st.f_type as i64 == libc::TMPFS_MAGIC as i64;
    tmpfs
}

#[cfg(not(target_os = "linux"))]
fn is_tmpfs(_dir: &std::path::Path) -> bool {
    false
}

/// A scratch file holding bytes
pub fn of(bytes: &[u8]) -> io::Result<File> {
    let mut f = file()?;
    f.write_all(bytes)?;
    Ok(f)
}

#[cfg(target_os = "linux")]
fn open_in(dir: &std::path::Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let made = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_TMPFILE | libc::O_CLOEXEC)
        .open(dir);
    match made {
        // a file system without O_TMPFILE says so as EOPNOTSUPP (or as
        // EISDIR, on a kernel before it)
        Err(e) if matches!(e.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EISDIR)) => named(dir),
        r => r,
    }
}

#[cfg(not(target_os = "linux"))]
fn open_in(dir: &std::path::Path) -> io::Result<File> {
    named(dir)
}

/// A file made by a name of its own, and unlinked once open
fn named(dir: &std::path::Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let p = dir.join(format!(
        "pktito-server-{}-{}-{nanos}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&p)?;
    std::fs::remove_file(&p)?;
    Ok(f)
}

/// Mode 0 on fd's file, a pipe's both ends for a pipe. Every ffmpeg runs as
/// the server's user, who could otherwise open another decode's upload or
/// pipes through /proc/<pid>/fd: a stand-in for an ffmpeg that a crafted
/// upload took over read the upload another was decoding so (Docker,
/// 2026-11-04). What is open stays open: the server and ffmpeg read and
/// write through the descriptors they hold.
#[cfg(target_os = "linux")]
pub(crate) fn no_access(fd: &impl AsRawFd) -> io::Result<()> {
    if unsafe { libc::fchmod(fd.as_raw_fd(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// macOS has no /proc to open them through
#[cfg(not(target_os = "linux"))]
pub(crate) fn no_access(_fd: &impl AsRawFd) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileExt, MetadataExt};

    /// A scratch file has no name, holds what was written, and on Linux
    /// cannot be opened again through /proc
    #[test]
    fn a_scratch_file_is_nameless_and_holds_its_bytes() {
        let f = of(b"pktito").unwrap();
        let m = f.metadata().unwrap();
        assert_eq!(m.nlink(), 0);
        let mut back = [0u8; 6];
        f.read_exact_at(&mut back, 0).unwrap();
        assert_eq!(&back, b"pktito");
        #[cfg(target_os = "linux")]
        assert_eq!(m.mode() & 0o777, 0);
    }
}
