//! The engine with requests at once: each gets pktito-c's answer, its
//! chunks heard on any worker in any order; a short one is not held up
//! behind long ones; a request whose caller goes is dropped; what it
//! cannot hear is said so; a slot is held from before the upload; and a
//! job that panics fails its request alone.

mod common;

use pktito_server::engine::{deal, Busy, Config, Engine, Event, Failure, Health, Task, Workers};
use pktito_server::ffi::{self, Model, PkTranscribeOpts};
use pktito_server::format::{self, Transcript};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// A transcription's events to their end, as pktito-c --json would print
/// them, and when the last came
fn answer(m: &Model, fast: bool, mut rx: UnboundedReceiver<Event>) -> (String, Instant) {
    let mut t = Transcript { model: m, fast, samples: 0, chunks: Vec::new() };
    loop {
        match rx.blocking_recv() {
            Some(Event::Planned { samples, .. }) => t.samples = samples,
            Some(Event::Chunk(c)) => t.chunks.push(format::heard(m, &c)),
            Some(Event::Done) => return (format::pktito_json(&t), Instant::now()),
            other => panic!("{other:?}"),
        }
    }
}

fn opts(max_chunk_s: f64) -> PkTranscribeOpts {
    PkTranscribeOpts { max_chunk_s, ..ffi::transcribe_defaults() }
}

#[test]
fn requests_at_once_each_get_pktito_cs_answer() {
    let Some(m) = common::model("v2") else { return };
    // two minutes in chunks of 10 s at most: a dozen chunks or more a request
    let long = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("test-twice.wav");
    std::fs::write(&long, common::test_wav_times(2)).unwrap();
    let path = common::model_path("v2");
    let want_long = String::from_utf8(common::cli(&[
        "-t", "4", "--json", "--max-chunk", "10", "-m", path.to_str().unwrap(), long.to_str().unwrap(),
    ]))
    .unwrap();
    let want_short = String::from_utf8(common::data("test.v2.json")).unwrap();
    assert!(want_long.matches("\"start\": ").count() > 12 * 2);

    let engine = common::engine(vec![m.clone()], 6, 3, false);
    assert_eq!((engine.workers, engine.worker_threads.clone()), (3, vec![2, 2, 2]));
    let mut rxs: Vec<_> = (0..4)
        .map(|_| engine.submit(0, common::test_wav_times(2), Task::Transcribe(opts(10.0))).unwrap())
        .collect();
    rxs.push(engine.submit(0, common::data("test.wav"), Task::Transcribe(ffi::transcribe_defaults())).unwrap());
    let answers: Vec<(String, Instant)> = std::thread::scope(|s| {
        let waits: Vec<_> = rxs.into_iter().map(|rx| s.spawn(|| answer(&m, false, rx))).collect();
        waits.into_iter().map(|w| w.join().unwrap()).collect()
    });
    for (k, (json, _)) in answers[..4].iter().enumerate() {
        assert!(*json == want_long, "request {k}");
    }
    assert!(answers[4].0 == want_short);
    // the short one, sent last, is done before the long ones are
    let last_long = answers[..4].iter().map(|a| a.1).max().unwrap();
    assert!(answers[4].1 < last_long);
    assert_eq!(engine.requests(), 0);
    // every second of the five heard, chunk by chunk, and each one's time
    let s = engine.stats();
    assert_eq!((s.heard[0], s.pending, s.took[0].count()), (4.0 * 120.0 + 60.0, 0.0, 5));
}

#[test]
fn a_request_whose_caller_goes_is_dropped() {
    let Some(m) = common::model("v2") else { return };
    let engine = common::engine(vec![m.clone()], 4, 2, false);
    drop(engine.submit(0, common::test_wav_times(4), Task::Transcribe(opts(10.0))).unwrap());
    let rx = engine.submit(0, common::data("test.wav"), Task::Transcribe(ffi::transcribe_defaults())).unwrap();
    let (json, _) = answer(&m, false, rx);
    assert!(json.as_bytes() == common::data("test.v2.json"));
    let t0 = Instant::now();
    while engine.requests() > 0 {
        assert!(t0.elapsed() < Duration::from_secs(30), "the dropped request stays");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn what_cannot_be_heard_is_said() {
    let Some(m) = common::model("v2") else { return };
    let config = |max_requests, max_seconds| Config { threads: 2, workers: 1, fast: false, max_requests, max_seconds, ffmpeg: None };
    let task = || Task::Transcribe(ffi::transcribe_defaults());
    // two minutes' work on two threads: the third request comes while both are heard
    let engine = Engine::new(vec![m.clone()], config(2, 90.0));
    let _a = engine.submit(0, common::data("test.wav"), task()).unwrap();
    let _b = engine.submit(0, common::data("test.wav"), task()).unwrap();
    assert!(matches!(engine.submit(0, common::data("test.wav"), task()), Err(Busy)));
    drop(engine);

    let engine = Engine::new(vec![m.clone()], config(2, 5.0));
    let mut long = engine.submit(0, common::data("test.wav"), task()).unwrap();
    let mut junk = engine.submit(0, b"OggS, and then nothing an Ogg file has".to_vec(), task()).unwrap();
    assert!(matches!(long.blocking_recv(), Some(Event::Failed(Failure::TooLong(s))) if s == 5.0));
    assert!(matches!(junk.blocking_recv(), Some(Event::Failed(Failure::Unreadable(_)))));
    assert!(long.blocking_recv().is_none() && junk.blocking_recv().is_none());

    // the VAD's segments, each within the recording
    let mut rx = engine.submit(0, common::data("test.wav")[..44 + 2 * 16000 * 4].to_vec(), Task::Speech).unwrap();
    match rx.blocking_recv() {
        Some(Event::Speech { samples, segments }) => {
            assert_eq!(samples, 16000 * 4);
            assert!(!segments.is_empty());
            assert!(segments.iter().all(|s| 0 <= s.start && s.start < s.end && s.end <= samples as i64));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_slot_is_held_from_before_the_upload() {
    let Some(m) = common::model("v2") else { return };
    let engine = Engine::new(vec![m], Config { threads: 2, workers: 1, fast: false, max_requests: 2, max_seconds: 90.0, ffmpeg: None });
    let a = engine.slot().unwrap();
    let b = engine.slot().unwrap();
    assert_eq!(engine.requests(), 2);           // two uploads coming, nothing heard yet
    assert!(matches!(engine.slot(), Err(Busy)));
    assert!(matches!(engine.submit(0, common::data("test.wav"), Task::Speech), Err(Busy)));
    drop(a);                                    // an upload refused, or stopped: its slot back
    let mut rx = engine.slot().unwrap().submit(0, common::data("test.wav"), Task::Speech);
    assert!(matches!(rx.blocking_recv(), Some(Event::Speech { .. })));
    assert_eq!(engine.requests(), 1);
    drop(b);
    assert_eq!(engine.requests(), 0);
}

#[test]
fn a_job_that_panics_fails_its_request_alone() {
    let Some(m) = common::model("v2") else { return };
    // one worker: the request after the panic is heard by the one that caught it
    let engine = common::engine(vec![m.clone()], 2, 1, false);
    assert_eq!(engine.workers, 1);
    let task = || Task::Transcribe(ffi::transcribe_defaults());
    // a model the engine has not got: its plan panics
    let mut wrong = engine.submit(1, common::data("test.wav"), task()).unwrap();
    assert!(matches!(wrong.blocking_recv(), Some(Event::Failed(Failure::Internal(_)))));
    let (json, _) = answer(&m, false, engine.submit(0, common::data("test.wav"), task()).unwrap());
    assert!(json.as_bytes() == common::data("test.v2.json"));
    assert_eq!(engine.requests(), 0);
}

/// A recording with no speech is heard to its end at once, all of it
#[test]
fn silence_is_heard_at_once() {
    let Some(m) = common::model("v2") else { return };
    let engine = common::engine(vec![m.clone()], 2, 1, false);
    let mut wav = common::test_wav_times(1);
    wav[44..].fill(0);
    let mut rx = engine.submit(0, wav, Task::Transcribe(ffi::transcribe_defaults())).unwrap();
    assert!(matches!(rx.blocking_recv(), Some(Event::Planned { samples: 960000, chunks: 0 })));
    assert!(matches!(rx.blocking_recv(), Some(Event::Done)));
    let s = engine.stats();
    assert_eq!((s.heard[0], s.took[0].count(), engine.workers_now()), (60.0, 1, Workers { idle: 1, busy: 0, failed: 0 }));
}

/// The server can hear a request while one worker can
#[test]
fn health_is_the_workers() {
    assert_eq!(Workers { idle: 2, busy: 1, failed: 0 }.health(), Health::Ok);
    assert_eq!(Workers { idle: 0, busy: 1, failed: 2 }.health(), Health::Degraded);
    assert_eq!(Workers { idle: 0, busy: 0, failed: 3 }.health(), Health::Unavailable);
}

/// The threads dealt among the workers as evenly as they go, none left idle
#[test]
fn threads_are_dealt_evenly() {
    assert_eq!(deal(8, 2), [4, 4]);
    assert_eq!(deal(14, 3), [5, 5, 4]);
    assert_eq!(deal(19, 4), [5, 5, 5, 4]);
    assert_eq!(deal(10, 3), [4, 3, 3]);
    assert_eq!(deal(5, 1), [5]);
    assert_eq!(deal(2, 9), [1, 1]);
}
