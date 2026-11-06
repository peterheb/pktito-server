//! The answers. pktito_json is pktito-c --json's, byte for byte, so that an
//! answer here compares with the CLI's (tests/api.rs holds it to libpktito's
//! tests/data/test.v2.json), and srt and vtt are pktito-c --srt's and
//! --vtt's: the same words, times, cues and characters. json, text and
//! verbose_json are OpenAI's. Their text is the transcript's sentences
//! (pk_sentences), the lines pktito-c prints, with a space between them;
//! verbose_json's segments are those sentences.

use crate::ffi::{self, Chunk, Model, PkTok, PK_CUE_PAUSE};
use serde_json::{json, Value};
use std::ffi::CString;
use std::fmt::Write;
use std::ops::Range;

pub struct Word {
    pub text: String,
    /// Seconds into the recording, as pktito-c reckons them
    pub start: f64,
    pub end: f64,
}

pub struct Token {
    pub id: i32,
    pub piece: String,
    pub start: f64,
    pub end: f64,
}

/// A chunk's tokens as text, words and times
pub struct Heard {
    pub start: f64,
    pub end: f64,
    pub retried: i32,
    pub text: String,
    pub words: Vec<Word>,
    pub tokens: Vec<Token>,
    /// Its tokens, frames counted from the recording's start
    pub toks: Vec<PkTok>,
}

/// A chunk as pktito-c prints it (core/cli.c, print_chunk), its times in
/// the same arithmetic
pub fn heard(m: &Model, c: &Chunk) -> Heard {
    let rate = m.rate as f64;
    let fs = m.frame_seconds;
    let start = c.at.start as f64 / rate;
    let words = m
        .words(&c.toks)
        .iter()
        .map(|w| Word {
            text: m.word(&c.toks[w.tok as usize..(w.tok + w.ntok) as usize]),
            start: start + w.start as f64 * fs,
            end: start + w.end as f64 * fs,
        })
        .collect();
    let tokens = c
        .toks
        .iter()
        .map(|t| Token {
            id: t.id,
            piece: m.piece(t.id),
            start: start + t.frame as f64 * fs,
            end: start + (t.frame + t.dur) as f64 * fs,
        })
        .collect();
    let toks = c
        .toks
        .iter()
        .map(|t| PkTok {
            frame: t.frame + c.at_frame,
            ..*t
        })
        .collect();
    Heard {
        start,
        end: c.at.end as f64 / rate,
        retried: c.retried,
        text: m.text(&c.toks),
        words,
        tokens,
        toks,
    }
}

/// A recording's transcript, its chunks in order
pub struct Transcript<'a> {
    pub model: &'a Model,
    pub fast: bool,
    /// Its length in the model's samples
    pub samples: usize,
    pub chunks: Vec<Heard>,
}

impl Transcript<'_> {
    pub fn seconds(&self) -> f64 {
        self.samples as f64 / self.model.rate as f64
    }

    /// Its sentences, its chunks given in order. None if memory runs out
    pub fn sentences(&self) -> Option<Vec<Sentence>> {
        let mut s = Sentences::new(self.model);
        let mut out = Vec::new();
        for c in &self.chunks {
            out.extend(s.add(self.model, c)?);
        }
        out.extend(s.finish(self.model)?);
        Some(out)
    }

    /// Its sentences' texts, a space between them. None if memory runs out
    pub fn text(&self) -> Option<String> {
        Some(text_of(&self.sentences()?))
    }

    fn words(&self) -> impl Iterator<Item = &Word> {
        self.chunks.iter().flat_map(|c| c.words.iter())
    }

    /// Its language's two-letter code: the English model's, or what its
    /// words say (pk_language), None when they say too little
    pub fn language(&self) -> Option<String> {
        if self.model.english {
            return Some("en".into());
        }
        let texts: Vec<CString> = self.words().map(|w| cstring(&w.text)).collect();
        ffi::language(&texts)
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap_or_default() // a word's text holds no NUL
}

/// A sentence: its tokens' place among the recording's, and its text
pub struct Sentence {
    pub toks: Range<usize>,
    pub text: String,
}

/// Sentences' texts, a space between them
pub fn text_of(sentences: &[Sentence]) -> String {
    let texts: Vec<&str> = sentences.iter().map(|s| s.text.as_str()).collect();
    texts.join(" ")
}

/// A recording's sentences as its chunks come, as pktito-c prints them
/// (core/cli.c, print_sentences). A sentence that runs on into the next
/// chunk waits for it. The language is the English model's, or what the
/// multilingual model's words say once they say it.
pub struct Sentences {
    lang: Option<String>,
    /// The words so far, while the language is not known
    words: Vec<CString>,
    /// The tokens in no sentence yet
    tail: Vec<PkTok>,
    /// The first's place among the recording's tokens
    at: usize,
}

impl Sentences {
    pub fn new(m: &Model) -> Sentences {
        Sentences {
            lang: m.english.then(|| "en".to_string()),
            words: Vec::new(),
            tail: Vec::new(),
            at: 0,
        }
    }

    /// The sentences that end in a chunk. None if memory runs out
    pub fn add(&mut self, m: &Model, c: &Heard) -> Option<Vec<Sentence>> {
        self.tail.extend_from_slice(&c.toks);
        if self.lang.is_none() && !c.toks.is_empty() {
            self.words.extend(c.words.iter().map(|w| cstring(&w.text)));
            self.lang = ffi::language(&self.words);
        }
        self.take(m, false)
    }

    /// The sentences the recording's end ends. None if memory runs out
    pub fn finish(&mut self, m: &Model) -> Option<Vec<Sentence>> {
        self.take(m, true)
    }

    fn take(&mut self, m: &Model, done: bool) -> Option<Vec<Sentence>> {
        let found = m.sentences(&self.tail, self.lang.as_deref(), done)?;
        let mut out = Vec::new();
        for s in &found {
            let r = s.first as usize..(s.first + s.n) as usize;
            let text = m.text(&self.tail[r.clone()]);
            if !text.is_empty() {
                out.push(Sentence {
                    toks: self.at + r.start..self.at + r.end,
                    text,
                });
            }
        }
        if let Some(s) = found.last() {
            let used = (s.first + s.n) as usize;
            self.tail.drain(..used);
            self.at += used;
        }
        Some(out)
    }
}

/// printf's %.*f: Rust's {:.*} rounds a value's exact binary expansion to
/// nearest, ties to even, as printf does in the default rounding mode; the
/// test below holds it to the C library's snprintf.
fn fixed(x: f64, places: usize) -> String {
    format!("{x:.places$}")
}

/// pktito-c's json_str: '"' and '\' escaped, control characters as \u00XX,
/// the rest as it is
fn json_str(s: &mut String, t: &str) {
    s.push('"');
    for ch in t.chars() {
        match ch {
            '"' | '\\' => {
                s.push('\\');
                s.push(ch);
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

/// pktito-c --json of one file: its chunks, then its meta
pub fn pktito_json(t: &Transcript) -> String {
    let mut s = String::from("{\"chunks\": [");
    for (i, c) in t.chunks.iter().enumerate() {
        let _ = write!(
            s,
            "{}\n {{\"start\": {}, \"end\": {}, ",
            if i > 0 { "," } else { "" },
            fixed(c.start, 3),
            fixed(c.end, 3)
        );
        if c.retried != 0 {
            let _ = write!(s, "\"retried\": {}, ", c.retried);
        }
        s.push_str("\"text\": ");
        json_str(&mut s, &c.text);
        s.push_str(", \"words\": [");
        for (k, w) in c.words.iter().enumerate() {
            s.push_str(if k > 0 {
                ", {\"word\": "
            } else {
                "{\"word\": "
            });
            json_str(&mut s, &w.text);
            let _ = write!(
                s,
                ", \"start\": {}, \"end\": {}}}",
                fixed(w.start, 2),
                fixed(w.end, 2)
            );
        }
        s.push_str("], \"tokens\": [");
        for (k, tk) in c.tokens.iter().enumerate() {
            let _ = write!(
                s,
                "{}{{\"id\": {}, \"piece\": ",
                if k > 0 { ", " } else { "" },
                tk.id
            );
            json_str(&mut s, &tk.piece);
            let _ = write!(
                s,
                ", \"start\": {}, \"end\": {}}}",
                fixed(tk.start, 2),
                fixed(tk.end, 2)
            );
        }
        s.push_str("]}");
    }
    s.push_str("\n],\n \"meta\": {\"model\": ");
    json_str(&mut s, &t.model.name);
    s.push_str(if t.fast { ", \"fast\": true, " } else { ", " });
    let _ = write!(s, "\"audio\": {}, \"language\": ", fixed(t.seconds(), 3));
    match t.language() {
        Some(l) => json_str(&mut s, &l),
        None => s.push_str("null"),
    }
    s.push_str("}}\n");
    s
}

fn sub_time(s: &mut String, ms: i64, sep: char) {
    let _ = write!(
        s,
        "{:02}:{:02}:{:02}{sep}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    );
}

/// pktito-c --srt or --vtt: pk_captions' cues of every word. None if
/// memory runs out.
pub fn subtitles(t: &Transcript, vtt: bool) -> Option<String> {
    let texts: Vec<CString> = t.words().map(|w| cstring(&w.text)).collect();
    let times: Vec<(i64, i64)> = t
        .words()
        .map(|w| {
            (
                (w.start * 1000.0).round() as i64,
                (w.end * 1000.0).round() as i64,
            )
        })
        .collect();
    let cues = ffi::captions(
        &texts,
        &times,
        t.language().as_deref(),
        t.samples as i64 * 1000 / t.model.rate as i64,
    )?;
    let words: Vec<&Word> = t.words().collect();
    let sep = if vtt { '.' } else { ',' };
    let mut s = String::from(if vtt { "WEBVTT\n\n" } else { "" });
    for (k, cue) in cues.iter().enumerate() {
        let _ = writeln!(s, "{}", k + 1);
        sub_time(&mut s, cue.start, sep);
        s.push_str(" --> ");
        sub_time(&mut s, cue.end, sep);
        s.push('\n');
        for i in cue.first..cue.first + cue.n {
            if i > cue.first {
                s.push(if i == cue.line2 { '\n' } else { ' ' });
            }
            let w = &words[i as usize].text;
            if vtt {
                // WebVTT reads & < > as markup
                s.push_str(
                    &w.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;"),
                );
            } else {
                s.push_str(w);
            }
        }
        s.push_str(if cue.flags & PK_CUE_PAUSE != 0 {
            "\u{2026}\n\n"
        } else {
            "\n\n"
        });
    }
    Some(s)
}

/// OpenAI's json. None if memory runs out
pub fn json(t: &Transcript) -> Option<Value> {
    Some(json!({ "text": t.text()? }))
}

/// The English names OpenAI's verbose_json gives the languages
fn language_name(code: &str) -> &str {
    match code {
        "bg" => "bulgarian",
        "cs" => "czech",
        "da" => "danish",
        "de" => "german",
        "el" => "greek",
        "en" => "english",
        "es" => "spanish",
        "et" => "estonian",
        "fi" => "finnish",
        "fr" => "french",
        "hr" => "croatian",
        "hu" => "hungarian",
        "it" => "italian",
        "lt" => "lithuanian",
        "lv" => "latvian",
        "mt" => "maltese",
        "nl" => "dutch",
        "pl" => "polish",
        "pt" => "portuguese",
        "ro" => "romanian",
        "ru" => "russian",
        "sk" => "slovak",
        "sl" => "slovenian",
        "sv" => "swedish",
        "uk" => "ukrainian",
        other => other,
    }
}

/// x to so many decimal places, as a JSON number prints it
fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// OpenAI's verbose_json: segments (the sentences), words, or both, as
/// timestamp_granularities asks. The fields a transducer has no counterpart
/// for (Whisper's avg_logprob, compression_ratio, no_speech_prob, seek,
/// temperature) are left out, not made up. None if memory runs out.
pub fn verbose_json(t: &Transcript, segments: bool, words: bool) -> Option<Value> {
    let language = t.language();
    let sentences = t.sentences()?;
    let mut v = json!({
        "task": "transcribe",
        "language": language.as_ref().map(|l| language_name(l).to_string()),
        "duration": round(t.seconds(), 3),
        "text": text_of(&sentences),
    });
    if segments {
        let toks: Vec<&Token> = t.chunks.iter().flat_map(|c| c.tokens.iter()).collect();
        let segs: Vec<Value> = sentences
            .iter()
            .enumerate()
            .map(|(id, s)| {
                let toks = &toks[s.toks.clone()];
                json!({
                    "id": id,
                    "start": round(toks[0].start, 3),
                    "end": round(toks[toks.len() - 1].end, 3),
                    "text": s.text,
                    "tokens": toks.iter().map(|tk| tk.id).collect::<Vec<_>>(),
                })
            })
            .collect();
        v["segments"] = Value::Array(segs);
    }
    if words {
        let ws: Vec<Value> = t
            .words()
            .map(|w| json!({ "word": w.text, "start": round(w.start, 2), "end": round(w.end, 2) }))
            .collect();
        v["words"] = Value::Array(ws);
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snprintf(x: f64, places: usize) -> String {
        let mut buf = [0u8; 64];
        let fmt = c"%.*f";
        let n = unsafe {
            libc::snprintf(
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                fmt.as_ptr(),
                places as libc::c_int,
                x,
            )
        };
        String::from_utf8(buf[..n as usize].to_vec()).unwrap()
    }

    /// fixed is the C library's %.*f: on the times a transcript has (a
    /// sample count over the rate, plus frames of 0.08 s), on the ties a
    /// binary fraction can hold, and on a million values of a generator
    #[test]
    fn fixed_is_printf() {
        let check = |x: f64| {
            for places in [2, 3] {
                assert_eq!(
                    fixed(x, places),
                    snprintf(x, places),
                    "{x:e} to {places} places"
                );
            }
        };
        for s in (0..16000 * 600).step_by(31) {
            let start = s as f64 / 16000.0;
            check(start);
            check(start + (s % 997) as f64 * 0.08);
        }
        for k in 0..100000 {
            check(k as f64 / 16.0);
            check(k as f64 / 1024.0 + 0.0005);
        }
        let mut z = 0x9e3779b97f4a7c15u64;
        for _ in 0..1_000_000 {
            z ^= z << 13;
            z ^= z >> 7;
            z ^= z << 17;
            check((z >> 11) as f64 / (1u64 << 53) as f64 * 1e4);
        }
    }

    #[test]
    fn json_str_is_pktito_cs() {
        let mut s = String::new();
        json_str(&mut s, "a\"b\\c\n\u{1}\u{7f}é…");
        assert_eq!(s, "\"a\\\"b\\\\c\\u000a\\u0001\u{7f}é…\"");
    }
}
