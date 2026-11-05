//! An upload to 16 kHz mono float, the samples the model hears.
//!
//! WAV and AIFF go through libpktito's own reader and resampler, the ones
//! pktito-c reads a file with, so that a WAV's transcript here is the CLI's
//! byte for byte. Anything else goes to the ffmpeg the server was given
//! (--ffmpeg, src/ffmpeg.rs), whose WAV the same reader takes; without one
//! it is refused.

use crate::ffi::{self, PkWavInfo, Resampler};
use crate::ffmpeg::Ffmpeg;
use std::ffi::{c_char, c_void, CStr};

/// Why an upload could not be heard
#[derive(Debug)]
pub enum AudioError {
    /// Not audio this server reads, or not sound audio
    Unreadable(String),
    /// Longer than the server's limit (seconds)
    TooLong(f64),
}

/// The samples, and what they were before
pub struct Audio {
    pub samples: Vec<f32>,
    pub rate: u32,
    pub channels: u32,
}

/// Frames pktito-c reads at a time (core/cli.c, READ_FRAMES); the
/// resampler's output is the same however the input is cut, and this keeps
/// to the CLI's calls anyway.
const READ_FRAMES: usize = 16384;

pub fn decode(
    bytes: Vec<u8>,
    rate: u32,
    max_samples: usize,
    ffmpeg: Option<&Ffmpeg>,
) -> Result<Audio, AudioError> {
    if bytes.len() >= 12 && matches!(&bytes[0..4], b"RIFF" | b"RF64" | b"FORM") {
        wav(&bytes, rate, max_samples)
    } else if let Some(f) = ffmpeg {
        f.decode(bytes, rate, max_samples)
    } else {
        Err(AudioError::Unreadable(
            "not WAV or AIFF: this server reads other formats only through ffmpeg (--ffmpeg)"
                .into(),
        ))
    }
}

/// Inputs pushed into out, through the resampler where there is one
fn push(rs: &mut Option<Resampler>, input: &[f32], out: &mut Vec<f32>) {
    match rs {
        Some(r) => r.push(input, out),
        None => out.extend_from_slice(input),
    }
}

fn too_long(out: &[f32], rate: u32, max_samples: usize) -> Result<(), AudioError> {
    if out.len() > max_samples {
        return Err(AudioError::TooLong(max_samples as f64 / rate as f64));
    }
    Ok(())
}

fn wav(bytes: &[u8], rate: u32, max_samples: usize) -> Result<Audio, AudioError> {
    // The reader reads front to back from a FILE; fmemopen makes one of the
    // bytes, which outlive it.
    let f = unsafe { libc::fmemopen(bytes.as_ptr() as *mut c_void, bytes.len(), c"r".as_ptr()) };
    if f.is_null() {
        return Err(AudioError::Unreadable("cannot open the upload".into()));
    }
    let result = read(f, c"the upload", rate, max_samples, &mut || {});
    unsafe { libc::fclose(f) };
    result
}

/// libpktito's reader on f, a WAV or AIFF read front to back, to `rate` Hz
/// mono; progress is told after each block
pub(crate) fn read(
    f: *mut libc::FILE,
    name: &CStr,
    rate: u32,
    max_samples: usize,
    progress: &mut dyn FnMut(),
) -> Result<Audio, AudioError> {
    let mut info = PkWavInfo::default();
    let mut err = [0 as c_char; 256];
    let w =
        unsafe { ffi::pk_wav_open_file(f, name.as_ptr(), &mut info, err.as_mut_ptr(), err.len()) };
    if w.is_null() {
        return Err(AudioError::Unreadable(ffi::message(&err)));
    }
    let result = (|| {
        let mut rs = if info.rate != rate {
            Some(Resampler::new(info.rate, rate).ok_or_else(|| {
                AudioError::Unreadable(format!("cannot resample from {} Hz", info.rate))
            })?)
        } else {
            None
        };
        let mut block = vec![0f32; READ_FRAMES];
        let mut out = Vec::new();
        loop {
            let r = unsafe { ffi::pk_wav_read(w, block.as_mut_ptr(), READ_FRAMES) };
            progress();
            push(&mut rs, &block[..r], &mut out);
            too_long(&out, rate, max_samples)?;
            if r == 0 {
                break;
            }
        }
        // pktito-c warns of a reader's error after the samples and keeps them
        if unsafe { ffi::pk_wav_status(w, err.as_mut_ptr(), err.len()) } != 0 {
            tracing::warn!("{}: {}", name.to_string_lossy(), ffi::message(&err));
        }
        Ok(Audio {
            samples: out,
            rate: info.rate,
            channels: info.channels,
        })
    })();
    unsafe { ffi::pk_wav_close(w) };
    result
}
