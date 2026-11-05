//! Formats other than WAV and AIFF, through the ffmpeg the server is given
//! (--ffmpeg PATH): a process of its own for each upload, with only what
//! decoding needs, so that what a damaged file does to it stays there. The
//! decoder this replaces, symphonia 0.5.5, panicked on 17,713 of 74,301
//! damaged files and took 4 to 16 GB on others of 53 KB (fuzzed 2026-11-04).
//!
//! What it has:
//! - the upload as its standard input: a file in memory, which it can seek
//!   in, as an MP4 indexed at its end needs (through a pipe ffmpeg gives
//!   "partial file"), read through its fd: protocol alone, so that it opens
//!   no path and no address (-protocol_whitelist fd; ffmpeg 6.0 on);
//! - no environment, so not the API key, nor the server's through /proc,
//!   since main.rs makes the server not dumpable; no descriptors but its
//!   three, whose files are mode 0 (no_access);
//! - limits: DATA of memory (not on macOS, which refuses any), CPU seconds
//!   by --max-hours, no file written, no core, one decoder thread.
//!
//! An ffmpeg that a crafted upload took over would still run as the
//! server's user, on its network: a stand-in for one, in Docker, reached
//! the server's port (2026-11-04). Where Yama's ptrace_scope is 0, it could
//! also read another ffmpeg's memory, unless ffmpeg's file is one the
//! server's user can run but not read (DOCKER.md's chmod 711), which Linux
//! runs not dumpable.
//!
//! It writes the first audio stream as float WAV at its own rate, which
//! libpktito's reader takes as it takes an uploaded WAV: the channels'
//! mean, resampled to 16 kHz. It is stopped when the audio passes
//! --max-hours, and by a watchdog when it writes nothing for STALL. Its
//! arguments are options ffmpeg has kept for years, so that the ffmpeg
//! can be updated apart from the server.

use crate::audio::{self, Audio, AudioError};
use std::fs::File;
use std::io::{self, PipeReader, PipeWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The memory ffmpeg may write to (RLIMIT_DATA). ffmpeg 8.0 decoded an hour
/// of 48 kHz stereo AAC, and of Opus, in 128 MiB, and could not start in 64
/// (9950X, 2026-11-04).
#[cfg(not(target_os = "macos"))]
const DATA: u64 = 512 << 20;
/// ffmpeg is stopped when it writes nothing for this long
const STALL: Duration = Duration::from_secs(30);
/// What is kept of ffmpeg's messages: their end, whose last line says why
/// it stopped. Their start would cut a line in two where a damaged AAC drew
/// more than this (3 of 16,433 damaged files, 2026-11-04).
const ERR_KEPT: usize = 4096;
/// Its arguments for a decode, after the program's name
const ARGS: [&str; 16] = [
    "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1", "-protocol_whitelist", "fd", "-i", "fd:",
    "-map", "0:a:0", "-c:a", "pcm_f32le", "-f", "wav",
];

pub struct Ffmpeg {
    pub path: PathBuf,
    /// The first line of its -version
    pub version: String,
    /// The CPU seconds a decode may take; twice this is its time in all
    cpu: u64,
}

impl Ffmpeg {
    /// The ffmpeg at path, if it runs under a decode's limits and reads fd:.
    /// max_seconds is the longest recording taken: a decode gets 60 s of CPU
    /// and 120 s more an hour of that (ffmpeg 8.0 took 4.3 s for an hour of
    /// AAC and 7.9 s of Opus on a 9950X).
    pub fn new(path: &Path, max_seconds: f64) -> Result<Ffmpeg, String> {
        // the file given, not a name looked for on PATH
        let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut f = Ffmpeg { path, version: String::new(), cpu: 60 + (max_seconds / 30.0).ceil() as u64 };
        let version = f.run(&["-hide_banner", "-version"])?;
        f.version = version.lines().next().unwrap_or("").to_string();
        let protocols = f.run(&["-hide_banner", "-protocols"])?;
        let input = protocols.split("Output:").next().unwrap_or("");
        if !input.lines().any(|l| l.trim() == "fd") {
            return Err(format!("{}: {} reads no fd:, which ffmpeg 6.0 and later do", f.path.display(), f.version));
        }
        Ok(f)
    }

    /// Its standard output for args, run as a decode is
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        if !out.status.success() {
            return Err(format!("{}: {}: {}", self.path.display(), status(out.status), last_line(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The program with its limits, and nothing of the server's
    fn command(&self) -> Command {
        let mut c = Command::new(&self.path);
        c.env_clear().current_dir("/");
        let cpu = self.cpu;
        // Safety: setrlimit alone between fork and exec, which is async-signal-safe
        unsafe { c.pre_exec(move || limits(cpu)) };
        c
    }

    /// An upload to `rate` Hz mono, of max_samples at most
    pub fn decode(&self, upload: Vec<u8>, rate: u32, max_samples: usize) -> Result<Audio, AudioError> {
        let fail = |why: String| AudioError::Unreadable(why);
        let input = memfile(&upload).map_err(|e| fail(format!("cannot hand the upload to ffmpeg: {e}")))?;
        drop(upload);
        let ((out, out_w), (mut stderr, err_w)) = match (pipe(), pipe()) {
            (Ok(o), Ok(e)) => (o, e),
            (Err(e), _) | (_, Err(e)) => return Err(fail(format!("cannot make ffmpeg's pipes: {e}"))),
        };
        let mut child = self
            .command()
            .args(ARGS)
            .arg("pipe:1")
            .stdin(input)
            .stdout(out_w)
            .stderr(err_w)
            .spawn()
            .map_err(|e| fail(format!("cannot run ffmpeg: {e}")))?;
        let pid = child.id();
        let errs = std::thread::Builder::new().name("pktito-ffmpeg".into()).spawn(move || {
            let (mut kept, mut buf) = (Vec::new(), [0u8; 4096]);
            while let Ok(n @ 1..) = stderr.read(&mut buf) {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 2 * ERR_KEPT {
                    kept.drain(..kept.len() - ERR_KEPT);
                }
            }
            kept
        });
        let w = Arc::new(Watch { state: Mutex::new((false, Instant::now(), None)), wake: Condvar::new() });
        let all = Duration::from_secs(2 * self.cpu);
        let watcher = {
            let w = w.clone();
            std::thread::Builder::new().name("pktito-ffmpeg".into()).spawn(move || watch(&w, pid, all))
        };
        let (Ok(errs), Ok(watcher)) = (errs, watcher) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(fail("no thread to watch ffmpeg with".into()));
        };

        // libpktito's reader on its output, as on an uploaded WAV
        let out = out.into_raw_fd();
        let f = unsafe { libc::fdopen(out, c"r".as_ptr()) };
        let read = if f.is_null() {
            unsafe { libc::close(out) };
            Err(fail("cannot read ffmpeg's output".into()))
        } else {
            let r = audio::read(f, c"ffmpeg's output", rate, max_samples, &mut || {
                if let Ok(mut s) = w.state.lock() {
                    s.1 = Instant::now();
                }
            });
            unsafe { libc::fclose(f) };
            r
        };
        if read.is_err() {
            let _ = child.kill();
        }
        // Its end, waited for without reaping it, so that the pid the
        // watchdog may stop is still its own; then the watchdog's end, and
        // the reaping, which tells the CPU time it used
        exited(pid);
        w.state.lock().map(|mut s| s.0 = true).ok();
        w.wake.notify_all();
        let _ = watcher.join();
        let (ended, cpu) = reap(pid).map_err(|e| fail(format!("ffmpeg: {e}")))?;
        let err = errs.join().unwrap_or_default();
        let stopped = w.state.lock().ok().and_then(|mut s| s.2.take());

        if let Some(why) = stopped {
            return Err(fail(why));
        }
        // the limit ends it with SIGKILL, or with SIGXCPU, on which ffmpeg
        // quits as asked and exits 255: its time says which it was
        if cpu + 1.0 >= self.cpu as f64 {
            return Err(fail(format!("ffmpeg used its {} s of CPU on it", self.cpu)));
        }
        // a decode ffmpeg did not finish is no transcript, whatever it wrote
        match read {
            Err(AudioError::TooLong(limit)) => Err(AudioError::TooLong(limit)),
            Ok(a) if ended.success() => {
                if !err.is_empty() {
                    tracing::warn!("ffmpeg: {}", last_line(&err));
                }
                Ok(a)
            }
            Err(e) if ended.success() => Err(e),
            _ => Err(fail(format!("ffmpeg could not read it ({}): {}", status(ended), last_line(&err)))),
        }
    }
}

/// What the watchdog and the reader share: whether the decode is done, when
/// ffmpeg last wrote, and why the watchdog stopped it
struct Watch {
    state: Mutex<(bool, Instant, Option<String>)>,
    wake: Condvar,
}

/// Stops ffmpeg when it writes nothing for STALL, or runs longer than all.
/// It is not reaped before `done` is set, under the lock this kills under,
/// so the pid is ffmpeg's.
fn watch(w: &Watch, pid: u32, all: Duration) {
    let end = Instant::now() + all;
    let Ok(mut s) = w.state.lock() else { return };
    while !s.0 {
        let now = Instant::now();
        let why = if now >= end {
            Some(format!("ffmpeg took longer than {} s", all.as_secs()))
        } else if now >= s.1 + STALL {
            Some(format!("ffmpeg wrote nothing for {} s", STALL.as_secs()))
        } else {
            None
        };
        if let Some(why) = why {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            s.2 = Some(why);
            return;
        }
        let wait = (s.1 + STALL).min(end).saturating_duration_since(now);
        let Ok((next, _)) = w.wake.wait_timeout(s, wait) else { return };
        s = next;
    }
}

/// Waits for pid to end, leaving it to be reaped
fn exited(pid: u32) {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    while unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) } != 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
}

/// Reaps pid: how it ended, and the CPU seconds it used
fn reap(pid: u32) -> io::Result<(ExitStatus, f64)> {
    let mut status = 0;
    let mut used: libc::rusage = unsafe { std::mem::zeroed() };
    while unsafe { libc::wait4(pid as libc::pid_t, &mut status, 0, &mut used) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    Ok((ExitStatus::from_raw(status), seconds(used.ru_utime) + seconds(used.ru_stime)))
}

/// The limits, set in the child between fork and exec
fn limits(cpu: u64) -> io::Result<()> {
    let set = |what, n: u64| {
        let l = libc::rlimit { rlim_cur: n as libc::rlim_t, rlim_max: n as libc::rlim_t };
        if unsafe { libc::setrlimit(what, &l) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    // macOS refuses any data limit (EINVAL, even 64 GiB; macOS 27 on an M4
    // Pro, 2026-11-04), so there ffmpeg's memory has no bound
    #[cfg(not(target_os = "macos"))]
    set(libc::RLIMIT_DATA, DATA)?;
    set(libc::RLIMIT_CPU, cpu)?;
    // it writes to pipes, which this does not limit; a file would end it
    set(libc::RLIMIT_FSIZE, 0)?;
    set(libc::RLIMIT_CORE, 0)?;
    set(libc::RLIMIT_NOFILE, 16)
}

/// The upload as a file in memory, which ffmpeg can seek in
#[cfg(target_os = "linux")]
fn memfile(bytes: &[u8]) -> io::Result<File> {
    let fd = unsafe { libc::memfd_create(c"pktito-upload".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: a descriptor just made, which the File alone owns
    let mut f = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
    no_access(&f)?;
    write_whole(&mut f, bytes)?;
    Ok(f)
}

/// The upload as a file ffmpeg can seek in: without memfd, one in the
/// temporary directory, unlinked once open
#[cfg(not(target_os = "linux"))]
fn memfile(bytes: &[u8]) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!("pktito-upload-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&p)?;
    std::fs::remove_file(&p)?;
    write_whole(&mut f, bytes)?;
    Ok(f)
}

fn write_whole(f: &mut File, bytes: &[u8]) -> io::Result<()> {
    f.write_all(bytes)?;
    f.seek(SeekFrom::Start(0))?;
    Ok(())
}

/// A pipe for ffmpeg to write to, mode 0 before it runs (no_access)
fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    let (r, w) = io::pipe()?;
    no_access(&r)?;
    Ok((r, w))
}

/// Mode 0 on fd's file, its pipe's both ends. Every ffmpeg runs as the
/// server's user, who could otherwise open another decode's upload or pipes
/// through /proc/<pid>/fd: a stand-in for an ffmpeg that a crafted upload
/// took over read the upload another was decoding so (Docker, 2026-11-04).
/// What is open stays open: ffmpeg reads and writes through the descriptors
/// it is given.
#[cfg(target_os = "linux")]
fn no_access(fd: &impl AsRawFd) -> io::Result<()> {
    if unsafe { libc::fchmod(fd.as_raw_fd(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// macOS has no /proc to open them through
#[cfg(not(target_os = "linux"))]
fn no_access(_fd: &impl AsRawFd) -> io::Result<()> {
    Ok(())
}

fn status(s: ExitStatus) -> String {
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (_, Some(libc::SIGKILL)) => "killed".into(),
        (_, Some(n)) => format!("signal {n}"),
        _ => "ended".into(),
    }
}

/// The last line ffmpeg wrote, less its "[component @ 0x...] "s
fn last_line(err: &[u8]) -> String {
    let s = String::from_utf8_lossy(err);
    let mut line = s.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("no message");
    while let (true, Some(i)) = (line.starts_with('['), line.find("] ")) {
        line = &line[i + 2..];
    }
    line.chars().take(200).collect()
}
