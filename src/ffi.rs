//! libpktito through its own `extern "C"` (core/pktito.h, core/wav.h,
//! core/resample.h), and the few safe types the server holds it by.
//!
//! The library's own rules carry over: a model is read-only once loaded and
//! may be shared by any number of threads (Model is Sync); a transcriber
//! belongs to one thread at a time (Asr is Send, not Sync), as does a
//! step-wise transcription (Steps), which the engine moves between threads
//! only under its lock.

use std::ffi::{c_char, c_int, c_long, c_void, CStr, CString};
use std::path::Path;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PkTok {
    pub id: i32,
    pub frame: i32,
    pub dur: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PkSeg {
    pub start: i64,
    pub end: i64,
}

/// A recording as libpktito reads it, a stretch at a time through `read`
/// (pk_audio); pk_audio_floats makes one over samples in memory, which must
/// outlive what reads them. Its fields are C's to fill and read.
#[repr(C)]
#[allow(dead_code)]
struct PkAudio {
    n: i64,
    read: Option<unsafe extern "C" fn(*mut c_void, i64, *mut f32, usize) -> c_int>,
    arg: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PkJob {
    pub chunk: usize,
    pub retry: c_int,
    pub at: PkSeg,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PkTranscribeOpts {
    pub max_chunk_s: f64,
    pub retry: c_int,
    pub context: c_int,
}

#[repr(C)]
struct PkChunkOut {
    i: usize,
    n: usize,
    at: PkSeg,
    toks: *const PkTok,
    count: c_long,
    retried: c_int,
    done: i64,
    total: i64,
    at_frame: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PkWord {
    pub tok: i32,
    pub ntok: i32,
    pub start: i32,
    pub end: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PkSegParams {
    pub threshold: f64,
    pub neg_threshold: f64,
    pub min_speech_ms: f64,
    pub max_speech_s: f64,
    pub min_silence_ms: f64,
    pub speech_pad_ms: f64,
    pub min_silence_at_max_speech_ms: f64,
    pub use_max_poss_sil_at_max_speech: c_int,
}

#[repr(C)]
pub struct PkCapword {
    pub text: *const c_char,
    pub start: i64,
    pub end: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PkCue {
    pub first: i32,
    pub n: i32,
    pub line2: i32,
    pub chars: i32,
    pub flags: i32,
    pub start: i64,
    pub end: i64,
}

pub const PK_CUE_PAUSE: i32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PkSentence {
    pub first: i32,
    pub n: i32,
    pub flags: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PkWavInfo {
    pub rate: u32,
    pub channels: u32,
    pub format: u32,
    pub bytes: u32,
    pub frames: u64,
}

#[repr(C)]
pub struct PkModel {
    _private: [u8; 0],
}
#[repr(C)]
pub struct PkAsr {
    _private: [u8; 0],
}
#[repr(C)]
pub struct PkSteps {
    _private: [u8; 0],
}
#[repr(C)]
pub struct PkWav {
    _private: [u8; 0],
}
#[repr(C)]
pub struct PkResampler {
    _private: [u8; 0],
}

extern "C" {
    fn pk_model_load(path: *const c_char, err: *mut c_char, errlen: usize) -> *mut PkModel;
    fn pk_model_free(m: *mut PkModel);
    fn pk_model_name(m: *const PkModel) -> *const c_char;
    fn pk_model_meta(m: *const PkModel, key: *const c_char) -> *const c_char;
    fn pk_model_rate(m: *const PkModel) -> c_int;
    fn pk_model_frame_seconds(m: *const PkModel) -> f64;
    fn pk_asr_new(m: *const PkModel, threads: c_int) -> *mut PkAsr;
    fn pk_asr_free(a: *mut PkAsr);
    fn pk_asr_threads(a: *const PkAsr) -> c_int;
    fn pk_asr_set_fast_mode(a: *mut PkAsr, on: c_int);
    fn pk_asr_run(a: *mut PkAsr, x: *const f32, n: usize, toks: *mut *const PkTok) -> c_long;
    fn pk_transcribe_defaults(o: *mut PkTranscribeOpts);
    fn pk_audio_floats(x: *const f32, n: usize) -> PkAudio;
    fn pk_steps_new(a: *mut PkAsr, x: *const PkAudio, o: *const PkTranscribeOpts) -> *mut PkSteps;
    fn pk_steps_chunks(s: *const PkSteps) -> usize;
    fn pk_steps_job(s: *mut PkSteps, j: *mut PkJob) -> c_int;
    fn pk_steps_put(s: *mut PkSteps, j: *const PkJob, toks: *const PkTok, count: c_long) -> c_int;
    fn pk_steps_take(s: *mut PkSteps, out: *mut PkChunkOut) -> c_int;
    fn pk_steps_free(s: *mut PkSteps);
    fn pk_text(m: *const PkModel, t: *const PkTok, n: usize, out: *mut c_char, cap: usize)
        -> usize;
    fn pk_piece(m: *const PkModel, id: i32, len: *mut usize) -> *const c_char;
    fn pk_words(m: *const PkModel, t: *const PkTok, n: usize, out: *mut PkWord) -> usize;
    fn pk_backend() -> *const c_char;
    fn pk_compiler() -> *const c_char;
    fn pk_version() -> *const c_char;
    fn pk_seg_defaults(p: *mut PkSegParams);
    fn pk_asr_speech(
        a: *mut PkAsr,
        x: *const PkAudio,
        p: *const PkSegParams,
        out: *mut PkSeg,
        cap: usize,
    ) -> c_long;
    fn pk_captions(
        w: *const PkCapword,
        n: usize,
        lang: *const c_char,
        media_end: i64,
        out: *mut PkCue,
    ) -> c_long;
    fn pk_language(w: *const PkCapword, n: usize) -> *const c_char;
    fn pk_sentences(
        m: *const PkModel,
        t: *const PkTok,
        n: usize,
        lang: *const c_char,
        done: c_int,
        out: *mut PkSentence,
    ) -> c_long;
    pub fn pk_wav_open_file(
        f: *mut libc::FILE,
        name: *const c_char,
        info: *mut PkWavInfo,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut PkWav;
    pub fn pk_wav_read(w: *mut PkWav, out: *mut f32, n: usize) -> usize;
    pub fn pk_wav_status(w: *const PkWav, err: *mut c_char, errlen: usize) -> c_int;
    pub fn pk_wav_close(w: *mut PkWav);
    fn pk_resampler_new(from: u32, to: u32) -> *mut PkResampler;
    fn pk_resampler_free(r: *mut PkResampler);
    fn pk_resample_room(r: *const PkResampler, n: usize) -> usize;
    fn pk_resample(r: *mut PkResampler, input: *const f32, n: usize, out: *mut f32) -> usize;
    fn pk_resample_flush(r: *mut PkResampler, out: *mut f32) -> usize;
}

fn owned(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

/// An error buffer's message, up to its NUL
pub fn message(err: &[c_char]) -> String {
    let bytes: Vec<u8> = err
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The backend the library was built with, and the compiler that built it
pub fn backend() -> (String, String) {
    unsafe { (owned(pk_backend()), owned(pk_compiler())) }
}

/// The library's version (PKTITO_VERSION)
pub fn version() -> String {
    unsafe { owned(pk_version()) }
}

/// A loaded .pkf: the recognizer, its vocabulary and Silero's VAD
pub struct Model {
    ptr: *mut PkModel,
    pub name: String,
    /// The languages it writes, two-letter codes (its "languages" metadata)
    pub languages: Vec<String>,
    /// It writes English alone, and English's rules apply to its captions
    pub english: bool,
    /// Whose model it is (its "credit" metadata: NVIDIA's, for Parakeet)
    pub credit: String,
    pub rate: u32,
    pub frame_seconds: f64,
}

// core/pktito.h: "A model is read-only once loaded and may be shared by any
// number of transcribers on any number of threads".
unsafe impl Send for Model {}
unsafe impl Sync for Model {}

impl Model {
    pub fn load(path: &Path) -> Result<Model, String> {
        let p = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        let mut err = [0 as c_char; 512];
        let ptr = unsafe { pk_model_load(p.as_ptr(), err.as_mut_ptr(), err.len()) };
        if ptr.is_null() {
            return Err(message(&err));
        }
        let meta = |key: &CStr| owned(unsafe { pk_model_meta(ptr, key.as_ptr()) });
        let languages = meta(c"languages");
        unsafe {
            Ok(Model {
                ptr,
                name: owned(pk_model_name(ptr)),
                english: languages == "en",
                languages: languages
                    .split(',')
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect(),
                credit: meta(c"credit"),
                rate: pk_model_rate(ptr) as u32,
                frame_seconds: pk_model_frame_seconds(ptr),
            })
        }
    }

    /// It writes the language (a two-letter code)
    pub fn writes(&self, language: &str) -> bool {
        self.languages.iter().any(|l| l == language)
    }

    /// The text of the tokens, as pktito-c writes it (pk_text)
    pub fn text(&self, toks: &[PkTok]) -> String {
        let mut cap = 256.max(toks.len() * 16);
        loop {
            let mut buf = vec![0u8; cap];
            let len = unsafe {
                pk_text(
                    self.ptr,
                    toks.as_ptr(),
                    toks.len(),
                    buf.as_mut_ptr() as *mut c_char,
                    cap,
                )
            };
            if len < cap {
                buf.truncate(len);
                return String::from_utf8(buf)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            }
            cap = len + 1;
        }
    }

    /// A word's text (pk_text), cut at 1023 bytes as pktito-c cuts one
    pub fn word(&self, toks: &[PkTok]) -> String {
        let mut buf = [0u8; 1024];
        let len = unsafe {
            pk_text(
                self.ptr,
                toks.as_ptr(),
                toks.len(),
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        let len = if len < buf.len() {
            len
        } else {
            buf.iter().position(|&b| b == 0).unwrap_or(0)
        };
        String::from_utf8_lossy(&buf[..len]).into_owned()
    }

    /// A token's piece of the vocabulary, U+2581 standing for its space, cut
    /// at 255 bytes as pktito-c's --json cuts it
    pub fn piece(&self, id: i32) -> String {
        let mut len = 0usize;
        let p = unsafe { pk_piece(self.ptr, id, &mut len) };
        if p.is_null() {
            return String::new();
        }
        let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, len.min(255)) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// The words of the tokens, as NeMo groups them (pk_words)
    pub fn words(&self, toks: &[PkTok]) -> Vec<PkWord> {
        let mut out = vec![PkWord::default(); toks.len()];
        let n = unsafe { pk_words(self.ptr, toks.as_ptr(), toks.len(), out.as_mut_ptr()) };
        out.truncate(n);
        out
    }

    /// The sentences of a recording's tokens, their frames counted from its
    /// start (pk_sentences); lang a two-letter code, or None. done when the
    /// tokens run to the recording's end; before it, the words after the
    /// last sentence are none yet. None if memory runs out.
    pub fn sentences(
        &self,
        toks: &[PkTok],
        lang: Option<&str>,
        done: bool,
    ) -> Option<Vec<PkSentence>> {
        let lang = lang.map(|l| CString::new(l).unwrap_or_default());
        let lp = lang.as_ref().map_or(std::ptr::null(), |l| l.as_ptr());
        let mut out = vec![PkSentence::default(); toks.len()];
        let n = unsafe {
            pk_sentences(
                self.ptr,
                toks.as_ptr(),
                toks.len(),
                lp,
                done as c_int,
                out.as_mut_ptr(),
            )
        };
        if n < 0 {
            return None;
        }
        out.truncate(n as usize);
        Some(out)
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        unsafe { pk_model_free(self.ptr) }
    }
}

/// A transcriber on its own pool of threads, the calling thread one of them
pub struct Asr {
    ptr: *mut PkAsr,
}

// A transcriber belongs to one thread at a time.
unsafe impl Send for Asr {}

impl Asr {
    pub fn new(m: &Model, threads: usize, fast: bool) -> Option<Asr> {
        let ptr = unsafe { pk_asr_new(m.ptr, threads as c_int) };
        if ptr.is_null() {
            return None;
        }
        unsafe { pk_asr_set_fast_mode(ptr, fast as c_int) };
        Some(Asr { ptr })
    }

    pub fn threads(&self) -> usize {
        unsafe { pk_asr_threads(self.ptr) as usize }
    }

    /// pk_asr_run: NeMo's transcript of exactly these samples, its tokens
    /// copied out of the transcriber. None if memory runs out.
    pub fn run(&mut self, x: &[f32]) -> Option<Vec<PkTok>> {
        let mut t: *const PkTok = std::ptr::null();
        let n = unsafe { pk_asr_run(self.ptr, x.as_ptr(), x.len(), &mut t) };
        if n < 0 {
            return None;
        }
        if n == 0 {
            return Some(Vec::new());
        }
        Some(unsafe { std::slice::from_raw_parts(t, n as usize) }.to_vec())
    }

    /// The speech in x (pk_asr_speech), Silero's defaults. None if memory runs out.
    pub fn speech(&mut self, x: &[f32]) -> Option<Vec<PkSeg>> {
        let frames = x.len().div_ceil(512);
        let mut out = vec![PkSeg::default(); 2 * frames + 1];
        let n = unsafe {
            let rec = pk_audio_floats(x.as_ptr(), x.len());
            pk_asr_speech(
                self.ptr,
                &rec,
                std::ptr::null(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        if n < 0 {
            return None;
        }
        out.truncate(n as usize);
        Some(out)
    }
}

impl Drop for Asr {
    fn drop(&mut self) {
        unsafe { pk_asr_free(self.ptr) }
    }
}

/// pk_transcribe_defaults, then the caller's changes
pub fn transcribe_defaults() -> PkTranscribeOpts {
    let mut o = PkTranscribeOpts {
        max_chunk_s: 0.0,
        retry: 0,
        context: 0,
    };
    unsafe { pk_transcribe_defaults(&mut o) };
    o
}

/// One of a recording's chunks, in order, as pk_steps_take has it
#[derive(Clone, Debug)]
pub struct Chunk {
    pub i: usize,
    pub n: usize,
    pub at: PkSeg,
    pub toks: Vec<PkTok>,
    pub retried: i32,
    /// at.start in frames: a token's frame plus this counts from the
    /// recording's start
    pub at_frame: i32,
}

/// pk_steps said -1: memory ran out, or a job came back that it did not
/// hand out. The run is over: every later call says -1 too.
#[derive(Debug)]
pub struct StepsFailed;

/// A recording's transcription in steps (pk_steps)
pub struct Steps {
    ptr: *mut PkSteps,
}

// It belongs to one thread at a time; the engine holds it under its lock.
unsafe impl Send for Steps {}

impl Steps {
    /// The VAD and the chunks of x, on a's threads. None if memory runs out
    /// or max_chunk_s is under a second.
    ///
    /// # Safety
    /// x must outlive the Steps, which reads it until it is dropped (its
    /// pk_audio, which pk_steps_new copies, points at x).
    pub unsafe fn new(a: &mut Asr, x: &[f32], o: &PkTranscribeOpts) -> Option<Steps> {
        let rec = pk_audio_floats(x.as_ptr(), x.len());
        let ptr = pk_steps_new(a.ptr, &rec, o);
        if ptr.is_null() {
            None
        } else {
            Some(Steps { ptr })
        }
    }

    pub fn chunks(&self) -> usize {
        unsafe { pk_steps_chunks(self.ptr) }
    }

    /// The next job to hear, if there is one now; Err if memory runs out
    pub fn job(&mut self) -> Result<Option<PkJob>, StepsFailed> {
        let mut j = PkJob {
            chunk: 0,
            retry: 0,
            at: PkSeg::default(),
        };
        match unsafe { pk_steps_job(self.ptr, &mut j) } {
            1 => Ok(Some(j)),
            0 => Ok(None),
            _ => Err(StepsFailed),
        }
    }

    /// A job's tokens, as pk_asr_run gave them
    pub fn put(&mut self, j: &PkJob, toks: &[PkTok]) -> Result<(), StepsFailed> {
        match unsafe { pk_steps_put(self.ptr, j, toks.as_ptr(), toks.len() as c_long) } {
            0 => Ok(()),
            _ => Err(StepsFailed),
        }
    }

    /// The next chunk in order, if it is done
    pub fn take(&mut self) -> Result<Option<Chunk>, StepsFailed> {
        let mut c = PkChunkOut {
            i: 0,
            n: 0,
            at: PkSeg::default(),
            toks: std::ptr::null(),
            count: 0,
            retried: 0,
            done: 0,
            total: 0,
            at_frame: 0,
        };
        match unsafe { pk_steps_take(self.ptr, &mut c) } {
            1 => {
                let toks = if c.count > 0 {
                    unsafe { std::slice::from_raw_parts(c.toks, c.count as usize) }.to_vec()
                } else {
                    Vec::new()
                };
                Ok(Some(Chunk {
                    i: c.i,
                    n: c.n,
                    at: c.at,
                    toks,
                    retried: c.retried,
                    at_frame: c.at_frame,
                }))
            }
            0 => Ok(None),
            _ => Err(StepsFailed),
        }
    }
}

impl Drop for Steps {
    fn drop(&mut self) {
        unsafe { pk_steps_free(self.ptr) }
    }
}

/// Subtitle cues of words in order (pk_captions), in lang, a two-letter
/// code: "en" adds the English rules. None if memory runs out.
pub fn captions(
    texts: &[CString],
    times: &[(i64, i64)],
    lang: Option<&str>,
    media_end: i64,
) -> Option<Vec<PkCue>> {
    let w: Vec<PkCapword> = texts
        .iter()
        .zip(times)
        .map(|(t, &(start, end))| PkCapword {
            text: t.as_ptr(),
            start,
            end,
        })
        .collect();
    let mut out = vec![PkCue::default(); w.len().max(1)];
    let lang = lang.map(|l| CString::new(l).unwrap_or_default());
    let lang = lang.as_ref().map_or(std::ptr::null(), |l| l.as_ptr());
    let n = unsafe { pk_captions(w.as_ptr(), w.len(), lang, media_end, out.as_mut_ptr()) };
    if n < 0 {
        return None;
    }
    out.truncate(n as usize);
    Some(out)
}

/// The language of the words (pk_language), its two-letter code
pub fn language(texts: &[CString]) -> Option<String> {
    let w: Vec<PkCapword> = texts
        .iter()
        .map(|t| PkCapword {
            text: t.as_ptr(),
            start: 0,
            end: 0,
        })
        .collect();
    let l = unsafe { pk_language(w.as_ptr(), w.len()) };
    if l.is_null() {
        None
    } else {
        Some(owned(l))
    }
}

/// A resampler to the model's rate (core/resample.h: torchaudio's Resample,
/// its output the same however the input is cut into calls)
pub struct Resampler {
    ptr: *mut PkResampler,
}

unsafe impl Send for Resampler {}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Option<Resampler> {
        let ptr = unsafe { pk_resampler_new(from, to) };
        if ptr.is_null() {
            None
        } else {
            Some(Resampler { ptr })
        }
    }

    /// Push the inputs (none: the end), the outputs they complete onto out
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let room = unsafe { pk_resample_room(self.ptr, input.len()) };
        out.reserve(room);
        let at = out.len();
        let got = unsafe {
            let dst = out.as_mut_ptr().add(at);
            if input.is_empty() {
                pk_resample_flush(self.ptr, dst)
            } else {
                pk_resample(self.ptr, input.as_ptr(), input.len(), dst)
            }
        };
        unsafe { out.set_len(at + got) };
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { pk_resampler_free(self.ptr) }
    }
}

/// Silero's segment parameters at their defaults
pub fn seg_defaults() -> PkSegParams {
    let mut p = PkSegParams {
        threshold: 0.0,
        neg_threshold: 0.0,
        min_speech_ms: 0.0,
        max_speech_s: 0.0,
        min_silence_ms: 0.0,
        speech_pad_ms: 0.0,
        min_silence_at_max_speech_ms: 0.0,
        use_max_poss_sil_at_max_speech: 0,
    };
    unsafe { pk_seg_defaults(&mut p) };
    p
}
