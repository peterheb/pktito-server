//! The answers. pktito_json is pktito-c --json's document, pktito/1, byte
//! for byte, so that an answer here compares with the CLI's (tests/api.rs
//! holds it to libpktito's tests/data/test.v2.json), and srt and vtt are
//! pktito-c --srt's and
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
    /// Its own start in those frames (pk_chunk_out's at_frame)
    pub at_frame: i32,
    /// Each token's word's end, in seconds
    pub wend: Vec<f64>,
}

/// A chunk as pktito-c prints it (core/cli.c, print_chunk), its times in
/// the same arithmetic
pub fn heard(m: &Model, c: &Chunk) -> Heard {
    let rate = m.rate as f64;
    let fs = m.frame_seconds;
    let start = c.at.start as f64 / rate;
    let stop = c.at.end as f64 / rate;
    // A word ends at its last token's end, but no later than its chunk,
    // nor than the next word's start, and no earlier than its own
    // (core/cli.c, word_ends)
    let found = m.words(&c.toks);
    let mut wend = vec![0.0; c.toks.len()];
    let mut words = Vec::with_capacity(found.len());
    for (k, w) in found.iter().enumerate() {
        let s = start + w.start as f64 * fs;
        let mut e = start + w.end as f64 * fs;
        if e > stop {
            e = stop;
        }
        if let Some(next) = found.get(k + 1) {
            let n = start + next.start as f64 * fs;
            if e > n {
                e = n;
            }
        }
        let e = if e > s { e } else { s };
        let toks = w.tok as usize..(w.tok + w.ntok) as usize;
        wend[toks.clone()].fill(e);
        words.push(Word {
            text: m.word(&c.toks[toks]),
            start: s,
            end: e,
        });
    }
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
        end: stop,
        retried: c.retried,
        text: m.text(&c.toks),
        words,
        tokens,
        toks,
        at_frame: c.at_frame,
        wend,
    }
}

/// A recording's transcript, its chunks in order
pub struct Transcript<'a> {
    pub model: &'a Model,
    pub fast: bool,
    /// The upload's name, as the document names its file
    pub file: String,
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
        self.sentences_with(Sentences::new(self.model))
    }

    fn sentences_with(&self, mut s: Sentences) -> Option<Vec<Sentence>> {
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
    /// A sentence with no text is one too: the document's, where the
    /// lines pktito-c prints leave it out
    empty: bool,
}

impl Sentences {
    pub fn new(m: &Model) -> Sentences {
        Sentences {
            lang: m.english.then(|| "en".to_string()),
            words: Vec::new(),
            tail: Vec::new(),
            at: 0,
            empty: false,
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
            if self.empty || !text.is_empty() {
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

/// pktito-c's sb_str: '"', '\' and the control characters JSON names
/// escaped by name, the other control characters as \u00XX, the rest as it
/// is
fn json_str(s: &mut String, t: &str) {
    s.push('"');
    for ch in t.chars() {
        match ch {
            '"' | '\\' => {
                s.push('\\');
                s.push(ch);
            }
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            '\u{8}' => s.push_str("\\b"),
            '\u{c}' => s.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

/// pktito-c's sb_time: seconds to the millisecond, in the fewest digits
/// that read back the same ("60", "0.5", "21.793")
fn time(s: &mut String, x: f64) {
    let mut ms = (x * 1000.0).round() as i64;
    if ms < 0 {
        s.push('-');
        ms = -ms;
    }
    let _ = write!(s, "{}", ms / 1000);
    if ms % 1000 != 0 {
        let d = format!(".{:03}", ms % 1000);
        s.push_str(d.trim_end_matches('0'));
    }
}

/// A recording's end in milliseconds, which no cue may pass, as pktito-c
/// rounds it
fn media_end(t: &Transcript) -> i64 {
    (t.seconds() * 1000.0).round() as i64
}

/// pk_captions' cues of every word, with the words they count. None if
/// memory runs out
fn cues<'a>(t: &'a Transcript) -> Option<(Vec<ffi::PkCue>, Vec<&'a Word>)> {
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
    let cues = ffi::captions(&texts, &times, t.language().as_deref(), media_end(t))?;
    Some((cues, t.words().collect()))
}

/// pktito-c --json of one file (core/cli.c, json_doc), the document
/// pktito/1: the file, the model, the words' language; the sentences, each
/// with its words; the subtitles' cues, each with its lines; and the chunks
/// the recording was cut into, each with its tokens. None if memory runs
/// out.
pub fn pktito_json(t: &Transcript) -> Option<String> {
    let m = t.model;
    let fs = m.frame_seconds;
    let mut s = String::from("{\n  \"format\": \"pktito/1\",\n  \"file\": ");
    json_str(&mut s, &t.file);
    s.push_str(",\n  \"duration\": ");
    time(&mut s, t.seconds());
    s.push_str(",\n  \"model\": ");
    json_str(&mut s, &m.name);
    if t.fast {
        s.push_str(",\n  \"fast\": true");
    }
    s.push_str(",\n  \"language\": ");
    match t.language() {
        Some(l) => json_str(&mut s, &l),
        None => s.push_str("null"),
    }

    // every token of the recording, its chunk and its word's end
    let toks: Vec<PkTok> = t
        .chunks
        .iter()
        .flat_map(|c| c.toks.iter().copied())
        .collect();
    let of: Vec<(&Heard, f64)> = t
        .chunks
        .iter()
        .flat_map(|c| c.wend.iter().map(move |&e| (c, e)))
        .collect();
    let mut all = Sentences::new(m);
    all.empty = true;
    let sentences = t.sentences_with(all)?;
    s.push_str(",\n  \"segments\": [");
    for (k, sent) in sentences.iter().enumerate() {
        let tk = &toks[sent.toks.clone()];
        let at = &of[sent.toks.clone()];
        // a word's time is its chunk's start and its own frames past it
        let start_of = |w: &ffi::PkWord| {
            let c = at[w.tok as usize].0;
            c.start + (w.start - c.at_frame) as f64 * fs
        };
        let words = m.words(tk);
        let (start, end) = match (words.first(), words.last()) {
            (Some(a), Some(z)) => (start_of(a), at[z.tok as usize].1),
            _ => (0.0, 0.0),
        };
        s.push_str(if k > 0 { ",\n" } else { "\n" });
        s.push_str("    {\"start\": ");
        time(&mut s, start);
        s.push_str(", \"end\": ");
        time(&mut s, end);
        s.push_str(", \"text\": ");
        json_str(&mut s, &sent.text);
        s.push_str(", \"words\": [\n");
        for (i, w) in words.iter().enumerate() {
            s.push_str(if i > 0 { ",\n" } else { "" });
            s.push_str("      {\"word\": ");
            json_str(
                &mut s,
                &m.word(&tk[w.tok as usize..(w.tok + w.ntok) as usize]),
            );
            s.push_str(", \"start\": ");
            time(&mut s, start_of(w));
            s.push_str(", \"end\": ");
            time(&mut s, at[w.tok as usize].1);
            s.push('}');
        }
        s.push_str("\n    ]}");
    }
    s.push_str(if sentences.is_empty() { "]" } else { "\n  ]" });

    let (cues, words) = cues(t)?;
    s.push_str(",\n  \"captions\": [");
    for (k, cue) in cues.iter().enumerate() {
        s.push_str(if k > 0 { ",\n" } else { "\n" });
        s.push_str("    {\"start\": ");
        time(&mut s, cue.start as f64 / 1000.0);
        s.push_str(", \"end\": ");
        time(&mut s, cue.end as f64 / 1000.0);
        s.push_str(", \"lines\": [");
        let mut line = String::new();
        for i in cue.first..cue.first + cue.n {
            if i > cue.first && i == cue.line2 {
                json_str(&mut s, &line);
                s.push_str(", ");
                line.clear();
            } else if i > cue.first {
                line.push(' ');
            }
            line.push_str(&words[i as usize].text);
        }
        if cue.flags & PK_CUE_PAUSE != 0 {
            line.push('\u{2026}');
        }
        json_str(&mut s, &line);
        s.push_str("]}");
    }
    s.push_str(if cues.is_empty() { "]" } else { "\n  ]" });

    let mut first_word = 0;
    for (k, c) in t.chunks.iter().enumerate() {
        s.push_str(if k > 0 { ",\n" } else { ",\n  \"chunks\": [\n" });
        s.push_str("    {\"start\": ");
        time(&mut s, c.start);
        s.push_str(", \"end\": ");
        time(&mut s, c.end);
        s.push_str(", \"gap\": "); // the silence to the next
        match t.chunks.get(k + 1) {
            Some(next) => time(&mut s, next.start - c.end),
            None => s.push_str("null"),
        }
        let _ = write!(
            s,
            ", \"first_word\": {first_word}, \"words\": {}",
            c.words.len()
        );
        first_word += c.words.len();
        if c.retried != 0 {
            let _ = write!(s, ", \"retried\": {}", c.retried);
        }
        s.push_str(", \"tokens\": [");
        for (i, tk) in c.tokens.iter().enumerate() {
            s.push_str(if i > 0 { ", " } else { "" });
            let _ = write!(s, "{{\"id\": {}, \"piece\": ", tk.id);
            json_str(&mut s, &tk.piece);
            s.push_str(", \"start\": ");
            time(&mut s, tk.start);
            s.push_str(", \"end\": ");
            time(&mut s, tk.end);
            s.push('}');
        }
        s.push_str("]}");
    }
    if !t.chunks.is_empty() {
        s.push_str("\n  ]");
    }
    s.push_str("\n}\n");
    Some(s)
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
    let (cues, words) = cues(t)?;
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
            .map(|w| json!({ "word": w.text, "start": round(w.start, 3), "end": round(w.end, 3) }))
            .collect();
        v["words"] = Value::Array(ws);
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// time is pktito-c's sb_time: llround of the milliseconds, a half
    /// away from zero, and no zero left at the end
    #[test]
    fn time_is_pktito_cs() {
        let of = |x: f64| {
            let mut s = String::new();
            time(&mut s, x);
            s
        };
        assert_eq!(of(60.0), "60");
        assert_eq!(of(0.5), "0.5");
        assert_eq!(of(21.793), "21.793");
        assert_eq!(of(2.0004), "2");
        assert_eq!(of(0.0625), "0.063"); // a tie, exact in binary: up
        assert_eq!(of(1.9996), "2");
        assert_eq!(of(0.01), "0.01");
        assert_eq!(of(-0.25), "-0.25");
        assert_eq!(of(0.0), "0");
    }

    #[test]
    fn json_str_is_pktito_cs() {
        let mut s = String::new();
        json_str(&mut s, "a\"b\\c\n\t\u{1}\u{7f}é…");
        assert_eq!(s, "\"a\\\"b\\\\c\\n\\t\\u0001\u{7f}é…\"");
    }
}
