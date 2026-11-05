//! What the tests share: a libpktito tree (PKTITO_DIR, or ../libpktito),
//! its models and test recording, and its pktito-c, whose answers the
//! server's are held to.

#![allow(dead_code)]

use pktito_server::engine::{Config, Engine};
use pktito_server::ffi::Model;
use pktito_server::ffmpeg::Ffmpeg;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

pub fn libpktito() -> PathBuf {
    std::env::var("PKTITO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../libpktito")))
}

pub fn model_path(name: &str) -> PathBuf {
    libpktito().join(format!("models/parakeet-tdt-0.6b-{name}.pkf"))
}

/// The model, or None and a note when the tree has not got it
pub fn model(name: &str) -> Option<Arc<Model>> {
    let p = model_path(name);
    if !p.exists() {
        eprintln!("{}: not there, its checks skipped", p.display());
        return None;
    }
    Some(Arc::new(Model::load(&p).expect("the model loads")))
}

pub fn data(name: &str) -> Vec<u8> {
    std::fs::read(libpktito().join("tests/data").join(name)).expect("libpktito's tests/data")
}

/// A file of the server's own tests/data
pub fn own(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .expect("the server's tests/data")
}

/// test.wav (16 kHz mono 16-bit PCM, 60 s, a 44-byte header) n times over
pub fn test_wav_times(n: usize) -> Vec<u8> {
    let wav = data("test.wav");
    assert_eq!(&wav[36..40], b"data");
    let pcm = &wav[44..];
    let len = (pcm.len() * n) as u32;
    let mut out = wav[..44].to_vec();
    out[4..8].copy_from_slice(&(36 + len).to_le_bytes());
    out[40..44].copy_from_slice(&len.to_le_bytes());
    for _ in 0..n {
        out.extend_from_slice(pcm);
    }
    out
}

/// pktito-c's standard output for the arguments, run in the tree
pub fn cli(args: &[&str]) -> Vec<u8> {
    let tree = libpktito();
    let out = Command::new(tree.join("pktito-c"))
        .args(args)
        .current_dir(&tree)
        .output()
        .expect("pktito-c runs");
    assert!(
        out.status.success(),
        "pktito-c {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn engine(models: Vec<Arc<Model>>, threads: usize, workers: usize, fast: bool) -> Engine {
    Engine::new(
        models,
        Config {
            threads,
            workers,
            fast,
            max_requests: 16,
            max_seconds: 4.0 * 3600.0,
            ffmpeg: None,
        },
    )
}

/// The ffmpeg the tests decode with: PKTITO_FFMPEG, which must pass the
/// server's check, or ffmpeg on PATH; None and a note when there is none,
/// or the one on PATH does not pass (older than 6.0, say)
pub fn ffmpeg() -> Option<Arc<Ffmpeg>> {
    let given = std::env::var_os("PKTITO_FFMPEG").map(PathBuf::from);
    let path = given.clone().or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("ffmpeg"))
            .find(|p| p.is_file())
    });
    let Some(path) = path else {
        eprintln!("no ffmpeg (PKTITO_FFMPEG, or on PATH): its checks skipped");
        return None;
    };
    match Ffmpeg::new(&path, 4.0 * 3600.0) {
        Ok(f) => Some(Arc::new(f)),
        Err(e) if given.is_some() => panic!("PKTITO_FFMPEG: {e}"),
        Err(e) => {
            eprintln!("ffmpeg {e}: its checks skipped");
            None
        }
    }
}

/// libpktito's test.wav as ffmpeg makes it with args, into a file named
/// like name (an MP4 is indexed at its end, as ffmpeg writes one); None and
/// a note when that ffmpeg cannot
pub fn transcode(ff: &Ffmpeg, args: &[&str], name: &str) -> Option<Vec<u8>> {
    let out =
        std::env::temp_dir().join(format!("pktito-server-test-{}-{name}", std::process::id()));
    let made = Command::new(&ff.path)
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(libpktito().join("tests/data/test.wav"))
        .args(args)
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let bytes = if made { std::fs::read(&out).ok() } else { None };
    let _ = std::fs::remove_file(&out);
    if bytes.is_none() {
        eprintln!(
            "{} cannot make {name} with {args:?}: its check skipped",
            ff.version
        );
    }
    bytes
}

pub fn engine_ffmpeg(models: Vec<Arc<Model>>, ff: Arc<Ffmpeg>, max_seconds: f64) -> Engine {
    Engine::new(
        models,
        Config {
            threads: 2,
            workers: 1,
            fast: false,
            max_requests: 16,
            max_seconds,
            ffmpeg: Some(ff),
        },
    )
}
