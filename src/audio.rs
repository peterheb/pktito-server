//! An upload to 16 kHz mono float, the samples the model hears, read a
//! stretch at a time from disk (src/scratch.rs) as the library asks for
//! them (pk_audio), not held in memory whole.
//!
//! WAV and AIFF go through libpktito's own reader and resampler, the ones
//! pktito-c reads a file with, and as it reads one: from the file, seeking
//! to each stretch (pk_wav_seekable, pk_wav_seek) and resampling the window
//! of input that stretch needs (pk_resampler_seek), so that a WAV's
//! transcript here is the CLI's byte for byte. Anything else goes to the
//! ffmpeg the server was given (--ffmpeg, src/ffmpeg.rs), whose WAV the
//! same reader takes front to back, resampled, into a scratch file of 16
//! kHz floats that a stretch is then read from; without one it is refused.

use crate::ffi::{self, PkWav, PkWavInfo, Resampler, Source};
use crate::ffmpeg::Ffmpeg;
use std::ffi::{c_char, CStr};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

/// Why an upload could not be heard
#[derive(Debug)]
pub enum AudioError {
    /// Not audio this server reads, or not sound audio
    Unreadable(String),
    /// Longer than the server's limit (seconds)
    TooLong(f64),
}

/// Frames pktito-c reads at a time (core/cli.c, READ_FRAMES); the
/// resampler's output is the same however the input is cut, and this keeps
/// to the CLI's calls anyway.
const READ_FRAMES: usize = 16384;

/// A recording at the model's rate, on disk
pub struct Recording {
    n: usize,
    from: From,
}

enum From {
    /// A WAV or AIFF upload, read where each stretch is
    Wav(Mutex<WavFile>),
    /// Samples as they are, native floats: ffmpeg's, resampled
    Floats(File),
}

impl Source for Recording {
    fn len(&self) -> usize {
        self.n
    }

    fn read(&self, at: usize, out: &mut [f32]) -> bool {
        if at.checked_add(out.len()).is_none_or(|end| end > self.n) {
            return false;
        }
        match &self.from {
            From::Wav(w) => w.lock().is_ok_and(|mut w| w.read(at, out)),
            From::Floats(f) => {
                // Safety: any bytes are some f32, and out's are its own
                let bytes = unsafe {
                    std::slice::from_raw_parts_mut(out.as_mut_ptr() as *mut u8, out.len() * 4)
                };
                f.read_exact_at(bytes, at as u64 * 4).is_ok()
            }
        }
    }
}

impl Recording {
    /// Samples [at, at + n), or None if they cannot be read
    pub fn stretch(&self, at: usize, n: usize) -> Option<Vec<f32>> {
        let mut out = vec![0f32; n];
        self.read(at, &mut out).then_some(out)
    }
}

/// The upload, a file read from its start, to `rate` Hz mono of max_samples
/// at most
pub fn decode(
    mut upload: File,
    rate: u32,
    max_samples: usize,
    ffmpeg: Option<&Ffmpeg>,
) -> Result<Recording, AudioError> {
    let mut magic = [0u8; 12];
    let wav = upload.read_exact_at(&mut magic, 0).is_ok()
        && matches!(&magic[0..4], b"RIFF" | b"RF64" | b"FORM");
    upload
        .seek(SeekFrom::Start(0))
        .map_err(|e| AudioError::Unreadable(format!("cannot read the upload: {e}")))?;
    if wav {
        WavFile::open(upload, rate, max_samples)
    } else if let Some(f) = ffmpeg {
        f.decode(upload, rate, max_samples)
    } else {
        Err(AudioError::Unreadable(
            "not WAV or AIFF: this server reads other formats only through ffmpeg (--ffmpeg)"
                .into(),
        ))
    }
}

fn too_long(n: u64, rate: u32, max_samples: usize) -> Result<(), AudioError> {
    if n > max_samples as u64 {
        return Err(AudioError::TooLong(max_samples as f64 / rate as f64));
    }
    Ok(())
}

/// libpktito's reader on a WAV or AIFF that can seek, as pktito-c's
/// file_read reads one (core/cli.c)
struct WavFile {
    w: *mut PkWav,
    f: *mut libc::FILE,
    /// At another rate
    rs: Option<Resampler>,
    /// A read's input frames, and their outputs
    input: Vec<f32>,
    made: Vec<f32>,
}

// It is read by one thread at a time, under its Recording's lock.
unsafe impl Send for WavFile {}

impl WavFile {
    fn open(upload: File, rate: u32, max_samples: usize) -> Result<Recording, AudioError> {
        let fd = upload.into_raw_fd();
        let f = unsafe { libc::fdopen(fd, c"r".as_ptr()) };
        if f.is_null() {
            unsafe { libc::close(fd) };
            return Err(AudioError::Unreadable("cannot open the upload".into()));
        }
        let name: &CStr = c"the upload";
        let mut info = PkWavInfo::default();
        let mut err = [0 as c_char; 256];
        let w = unsafe {
            ffi::pk_wav_open_file(f, name.as_ptr(), &mut info, err.as_mut_ptr(), err.len())
        };
        if w.is_null() {
            unsafe { libc::fclose(f) };
            return Err(AudioError::Unreadable(ffi::message(&err)));
        }
        // its own from here, w and f closed when it goes
        let mut me = WavFile {
            w,
            f,
            rs: None,
            input: Vec::new(),
            made: Vec::new(),
        };
        let mut frames = 0u64;
        if unsafe { ffi::pk_wav_seekable(w, &mut frames) } != 0 {
            return Err(AudioError::Unreadable("cannot read the upload".into()));
        }
        // cut short, as a read to its end would find it: pktito-c warns,
        // and keeps the samples there are
        if unsafe { ffi::pk_wav_status(w, err.as_mut_ptr(), err.len()) } != 0 {
            tracing::warn!("{}: {}", name.to_string_lossy(), ffi::message(&err));
        }
        let mut n = frames;
        if info.rate != rate {
            let r = Resampler::new(info.rate, rate).ok_or_else(|| {
                AudioError::Unreadable(format!("cannot resample from {} Hz", info.rate))
            })?;
            n = r.total(frames);
            me.rs = Some(r);
            me.input = vec![0f32; READ_FRAMES];
        }
        too_long(n, rate, max_samples)?;
        Ok(Recording {
            n: n as usize,
            from: From::Wav(Mutex::new(me)),
        })
    }

    /// Samples [at, at + out.len()), as the whole file resampled gives them
    fn read(&mut self, at: usize, out: &mut [f32]) -> bool {
        let (from, mut skip) = match self.rs.as_mut() {
            Some(r) => r.seek(at as u64),
            None => (at as u64, 0),
        };
        if unsafe { ffi::pk_wav_seek(self.w, from) } != 0 {
            return false;
        }
        let Some(rs) = self.rs.as_mut() else {
            return unsafe { ffi::pk_wav_read(self.w, out.as_mut_ptr(), out.len()) } == out.len();
        };
        let mut got = 0;
        while got < out.len() {
            let r = unsafe { ffi::pk_wav_read(self.w, self.input.as_mut_ptr(), READ_FRAMES) };
            self.made.clear();
            // none pushed is the flush
            rs.push(&self.input[..r], &mut self.made);
            let before = skip.min(self.made.len());
            let take = (self.made.len() - before).min(out.len() - got);
            out[got..got + take].copy_from_slice(&self.made[before..before + take]);
            skip -= before;
            got += take;
            if r == 0 {
                break;
            }
        }
        got == out.len()
    }
}

impl Drop for WavFile {
    fn drop(&mut self) {
        unsafe {
            ffi::pk_wav_close(self.w);
            libc::fclose(self.f);
        }
    }
}

/// libpktito's reader on f, a WAV or AIFF read front to back, to `rate` Hz
/// mono, into a scratch file; progress is told after each block
pub(crate) fn spool(
    f: *mut libc::FILE,
    name: &CStr,
    rate: u32,
    max_samples: usize,
    progress: &mut dyn FnMut(),
) -> Result<Recording, AudioError> {
    let mut info = PkWavInfo::default();
    let mut err = [0 as c_char; 256];
    let w =
        unsafe { ffi::pk_wav_open_file(f, name.as_ptr(), &mut info, err.as_mut_ptr(), err.len()) };
    if w.is_null() {
        return Err(AudioError::Unreadable(ffi::message(&err)));
    }
    let result = (|| {
        let scratch = |e: std::io::Error| {
            AudioError::Unreadable(format!("cannot write the samples to a scratch file: {e}"))
        };
        let mut out = crate::scratch::file().map_err(scratch)?;
        let mut rs = if info.rate != rate {
            Some(Resampler::new(info.rate, rate).ok_or_else(|| {
                AudioError::Unreadable(format!("cannot resample from {} Hz", info.rate))
            })?)
        } else {
            None
        };
        let mut block = vec![0f32; READ_FRAMES];
        let mut made = Vec::new();
        let mut n = 0u64;
        loop {
            let r = unsafe { ffi::pk_wav_read(w, block.as_mut_ptr(), READ_FRAMES) };
            progress();
            made.clear();
            match &mut rs {
                Some(rs) => rs.push(&block[..r], &mut made),
                None => made.extend_from_slice(&block[..r]),
            }
            n += made.len() as u64;
            too_long(n, rate, max_samples)?;
            // Safety: an f32's bytes, as Recording reads them back
            let bytes =
                unsafe { std::slice::from_raw_parts(made.as_ptr() as *const u8, made.len() * 4) };
            out.write_all(bytes).map_err(scratch)?;
            if r == 0 {
                break;
            }
        }
        // pktito-c warns of a reader's error after the samples and keeps them
        if unsafe { ffi::pk_wav_status(w, err.as_mut_ptr(), err.len()) } != 0 {
            tracing::warn!("{}: {}", name.to_string_lossy(), ffi::message(&err));
        }
        Ok(Recording {
            n: n as usize,
            from: From::Floats(out),
        })
    })();
    unsafe { ffi::pk_wav_close(w) };
    result
}
