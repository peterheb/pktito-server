//! The scheduler. The threads the server may use are dealt into workers of
//! a few threads each, every worker with a transcriber of its own for each
//! model, and every request's work into jobs: its plan (the upload decoded,
//! then the VAD and the chunks, pk_steps_new, on the worker's threads), then
//! a chunk or a retry's stretch at a time (pk_steps_job). A free worker takes
//! the next job of the request whose turn it is, and that request's turn
//! goes to the back, so the requests share the workers job by job: a short
//! request waits for a chunk, not for a long one, and a request alone has
//! every worker, its chunks heard several at once as libpktito's lanes hear
//! them (core/asr.c, run_lanes). The chunks come back in any order and go
//! out in order (pk_steps_take), the bits pk_asr_transcribe gives.
//!
//! A request holds one of max_requests slots from before its upload is
//! read until it is done (Slot), so that what the requests hold - their
//! uploads, then their samples - has a bound; and a job that panics fails
//! its own request, not its worker.
//!
//! /health and /metrics read it as it goes: what each worker is doing, and
//! the audio heard and still in hand.

use crate::audio::{self, AudioError};
use crate::ffi::{Asr, Chunk, Model, PkJob, PkSeg, PkTok, PkTranscribeOpts, Steps};
use crate::ffmpeg::Ffmpeg;
use crate::metrics::Histogram;
use std::collections::{HashMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;
use tokio::sync::mpsc;

pub struct Config {
    /// Threads for transcription in all, the workers' together
    pub threads: usize,
    /// Workers, the threads dealt among them (deal): a worker's
    /// transcriber's pool, the worker one of them
    pub workers: usize,
    /// libpktito's fast mode (pk_asr_set_fast_mode), in every transcriber
    pub fast: bool,
    /// Requests taken at once; more are turned away
    pub max_requests: usize,
    /// The longest recording taken, in seconds
    pub max_seconds: f64,
    /// What reads formats other than WAV and AIFF (--ffmpeg), or None
    pub ffmpeg: Option<Arc<Ffmpeg>>,
}

/// What a request wants of its recording
pub enum Task {
    Transcribe(PkTranscribeOpts),
    /// Its speech segments alone (pk_asr_speech)
    Speech,
}

#[derive(Debug, Clone)]
pub enum Failure {
    Unreadable(String),
    TooLong(f64),
    Internal(String),
}

/// What a request hears back, in order: Planned, its chunks, Done; or
/// Speech; or Failed at any point
#[derive(Debug)]
pub enum Event {
    /// Decoded and cut: its samples at the model's rate, and its chunks
    Planned { samples: usize, chunks: usize },
    Chunk(Chunk),
    Done,
    Speech { samples: usize, segments: Vec<PkSeg> },
    Failed(Failure),
}

/// Every request slot is taken
#[derive(Debug)]
pub struct Busy;

struct Request {
    model: usize,
    task: Option<Task>,
    upload: Option<Vec<u8>>,
    /// Before samples, so that it goes first: it points into them
    steps: Option<Steps>,
    /// A Vec's own buffer: Arc<[f32]> from a Vec copies it, and for that
    /// moment holds the samples twice
    samples: Option<Arc<Vec<f32>>>,
    taken: usize,
    /// Its audio counted as heard, to this sample: its last chunk's end
    heard_to: usize,
    /// When its upload was in hand
    since: Instant,
    tx: mpsc::UnboundedSender<Event>,
}

enum Work {
    Plan { id: u64, model: usize, upload: Vec<u8>, task: Task },
    Hear { id: u64, model: usize, samples: Arc<Vec<f32>>, job: PkJob },
}

impl Work {
    fn id(&self) -> u64 {
        match self {
            Work::Plan { id, .. } | Work::Hear { id, .. } => *id,
        }
    }
}

#[derive(Default)]
struct State {
    requests: HashMap<u64, Request>,
    /// The requests in the order their turns come
    turns: VecDeque<u64>,
    /// Slots taken by requests whose uploads are still coming
    coming: usize,
    next_id: u64,
    /// The engine is going: its workers end
    stop: bool,
    /// Each model's audio heard, in samples
    heard: Vec<u64>,
    /// Each model's transcriptions' times, upload in hand to done
    took: Vec<Histogram>,
}

impl State {
    fn remove(&mut self, id: u64) -> Option<Request> {
        self.turns.retain(|&k| k != id);
        self.requests.remove(&id)
    }

    fn fail(&mut self, id: u64, f: Failure) {
        if let Some(r) = self.remove(id) {
            let _ = r.tx.send(Event::Failed(f));
        }
    }

    /// The next job, of the first request in turn that has one, whose turn
    /// then goes to the back
    fn next_work(&mut self) -> Option<Work> {
        let mut k = 0;
        while k < self.turns.len() {
            let id = self.turns[k];
            let r = self.requests.get_mut(&id).expect("a request in turn");
            if r.tx.is_closed() {
                self.remove(id);            // its caller is gone: no more of its jobs
                continue;
            }
            let work = if let Some(upload) = r.upload.take() {
                Ok(Some(Work::Plan { id, model: r.model, upload, task: r.task.take().expect("a task") }))
            } else if let Some(s) = r.steps.as_mut() {
                s.job().map(|j| {
                    j.map(|job| Work::Hear {
                        id,
                        model: r.model,
                        samples: r.samples.clone().expect("samples with the steps"),
                        job,
                    })
                })
            } else {
                Ok(None)                    // being planned
            };
            match work {
                Ok(Some(w)) => {
                    self.turns.remove(k);
                    self.turns.push_back(id);
                    return Some(w);
                }
                Ok(None) => k += 1,
                Err(_) => self.fail(id, Failure::Internal("out of memory".into())),
            }
        }
        None
    }

    /// A job's tokens into its request, and every chunk that completes out
    /// to its caller, in order
    fn heard(&mut self, id: u64, job: &PkJob, toks: Option<Vec<PkTok>>) {
        let Some(r) = self.requests.get_mut(&id) else {
            return;                         // gone; its job's tokens go with it
        };
        let steps = r.steps.as_mut().expect("steps for a job");
        let mut failed = toks.is_none();
        let mut gone = false;
        if let Some(t) = toks {
            failed = steps.put(job, &t).is_err();
        }
        while !failed && !gone {
            match steps.take() {
                Ok(Some(c)) => {
                    r.taken += 1;
                    let end = c.at.end.max(0) as usize;
                    self.heard[r.model] += end.saturating_sub(r.heard_to) as u64;
                    r.heard_to = r.heard_to.max(end);
                    gone = r.tx.send(Event::Chunk(c)).is_err();
                }
                Ok(None) => break,
                Err(_) => failed = true,
            }
        }
        if failed {
            self.fail(id, Failure::Internal("out of memory".into()));
        } else if gone {
            self.remove(id);
        } else if r.taken == steps.chunks() {
            self.done(id);
        }
    }

    /// A transcription heard to its end: the rest of its audio counted, its
    /// time taken, and Done to its caller
    fn done(&mut self, id: u64) {
        if let Some(r) = self.remove(id) {
            let n = r.samples.as_ref().map_or(0, |x| x.len());
            self.heard[r.model] += n.saturating_sub(r.heard_to) as u64;
            self.took[r.model].observe(r.since.elapsed().as_secs_f64());
            let _ = r.tx.send(Event::Done);
        }
    }
}

struct Shared {
    state: Mutex<State>,
    /// A job may be there to take
    ready: Condvar,
    models: Vec<Arc<Model>>,
    config: Config,
    workers: Vec<Worker>,
}

/// A worker's threads, and what /health reads of it
struct Worker {
    threads: usize,
    /// On a job
    busy: AtomicBool,
    /// Its last try at a transcriber failed: memory or threads ran out
    broken: AtomicBool,
}

impl Shared {
    /// The state, though a job panicked holding it: the worker that caught
    /// the panic fails that job's request, and what it left half done
    /// goes with the request
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A request's place among max_requests, taken before its upload is read,
/// so that the uploads coming in count, and their bytes; given back if it
/// is dropped unfilled, as when the upload is refused or stops
pub struct Slot {
    shared: Arc<Shared>,
    filled: bool,
}

impl Slot {
    /// The recording to hear in this slot, with a model: its events come on
    /// the receiver
    pub fn submit(mut self, model: usize, upload: Vec<u8>, task: Task) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut st = self.shared.lock();
        st.coming -= 1;                     // the request's own now
        let id = st.next_id;
        st.next_id += 1;
        st.requests.insert(
            id,
            Request {
                model,
                task: Some(task),
                upload: Some(upload),
                steps: None,
                samples: None,
                taken: 0,
                heard_to: 0,
                since: Instant::now(),
                tx,
            },
        );
        st.turns.push_back(id);
        drop(st);
        self.filled = true;
        self.shared.ready.notify_one();
        rx
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.filled {
            self.shared.lock().coming -= 1;
        }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    pub workers: usize,
    /// Each worker's threads
    pub worker_threads: Vec<usize>,
}

/// The workers by what they are doing; failed is a worker that cannot hear
/// a job, its thread gone or no transcriber to be had
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Workers {
    pub idle: usize,
    pub busy: usize,
    pub failed: usize,
}

/// Whether the server can hear a request: every worker can, some can, or none
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Degraded,
    Unavailable,
}

impl Workers {
    pub fn health(&self) -> Health {
        if self.failed == 0 {
            Health::Ok
        } else if self.idle + self.busy > 0 {
            Health::Degraded
        } else {
            Health::Unavailable
        }
    }
}

/// What /metrics reads of the engine at a moment
pub struct Stats {
    /// Requests begun: decoded, cut, or their chunks heard
    pub running: usize,
    /// Requests holding a slot and not begun: their uploads coming, or in hand
    pub waiting: usize,
    /// Each model's audio heard, in seconds
    pub heard: Vec<f64>,
    /// Audio decoded and not yet heard, in seconds
    pub pending: f64,
    /// Each model's transcriptions' times
    pub took: Vec<Histogram>,
}

/// threads dealt among workers (one at least, threads at most) as evenly as
/// they go, the first threads % workers a thread more than the rest: 14
/// among 3 as 5, 5 and 4
pub fn deal(threads: usize, workers: usize) -> Vec<usize> {
    let threads = threads.max(1);
    let w = workers.clamp(1, threads);
    (0..w).map(|k| threads / w + usize::from(k < threads % w)).collect()
}

impl Engine {
    /// config.workers workers, each a thread of its own and its share of
    /// config.threads in all (deal)
    pub fn new(models: Vec<Arc<Model>>, config: Config) -> Engine {
        let each = deal(config.threads, config.workers);
        let state = State {
            heard: vec![0; models.len()],
            took: vec![Histogram::default(); models.len()],
            ..State::default()
        };
        let workers =
            each.iter().map(|&threads| Worker { threads, busy: AtomicBool::new(false), broken: AtomicBool::new(false) });
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            ready: Condvar::new(),
            models,
            config,
            workers: workers.collect(),
        });
        let threads = (0..each.len())
            .map(|k| {
                let s = shared.clone();
                std::thread::Builder::new()
                    .name(format!("pktito-worker-{k}"))
                    .spawn(move || work(s, k))
                    .expect("a worker thread")
            })
            .collect();
        Engine { shared, threads, workers: each.len(), worker_threads: each }
    }

    pub fn models(&self) -> &[Arc<Model>] {
        &self.shared.models
    }

    pub fn fast(&self) -> bool {
        self.shared.config.fast
    }

    /// Requests in progress: their uploads coming, or their work not done
    pub fn requests(&self) -> usize {
        let st = self.shared.lock();
        st.requests.len() + st.coming
    }

    pub fn max_requests(&self) -> usize {
        self.shared.config.max_requests
    }

    /// What the workers are doing now
    pub fn workers_now(&self) -> Workers {
        let mut w = Workers::default();
        for (t, me) in self.threads.iter().zip(&self.shared.workers) {
            if t.is_finished() || me.broken.load(Relaxed) {
                w.failed += 1;
            } else if me.busy.load(Relaxed) {
                w.busy += 1;
            } else {
                w.idle += 1;
            }
        }
        w
    }

    pub fn stats(&self) -> Stats {
        let st = self.shared.lock();
        let seconds = |samples: u64, model: usize| samples as f64 / self.shared.models[model].rate as f64;
        let mut pending = vec![0u64; self.shared.models.len()];
        for r in st.requests.values() {
            if let Some(x) = &r.samples {
                pending[r.model] += x.len().saturating_sub(r.heard_to) as u64;
            }
        }
        let waiting = st.coming + st.requests.values().filter(|r| r.upload.is_some()).count();
        Stats {
            running: st.requests.len() + st.coming - waiting,
            waiting,
            heard: st.heard.iter().enumerate().map(|(m, &n)| seconds(n, m)).collect(),
            // from 0, where sum() starts at -0
            pending: pending.iter().enumerate().fold(0.0, |a, (m, &n)| a + seconds(n, m)),
            took: st.took.clone(),
        }
    }

    /// A slot for a request, before its upload is read; Busy when every one
    /// of max_requests is taken
    pub fn slot(&self) -> Result<Slot, Busy> {
        let mut st = self.shared.lock();
        if st.requests.len() + st.coming >= self.shared.config.max_requests {
            return Err(Busy);
        }
        st.coming += 1;
        Ok(Slot { shared: self.shared.clone(), filled: false })
    }

    /// A slot, filled at once: for a caller with the upload in hand
    pub fn submit(&self, model: usize, upload: Vec<u8>, task: Task) -> Result<mpsc::UnboundedReceiver<Event>, Busy> {
        Ok(self.slot()?.submit(model, upload, task))
    }
}

/// The workers end once their jobs are done, and the requests not done
/// with them: their callers hear nothing more
impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.ready.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Worker k's transcriber for a model, on its threads, made the first time
/// it is needed; while none can be made, the worker counts as failed
fn transcriber<'a>(asr: &'a mut [Option<Asr>], s: &Shared, k: usize, model: usize) -> Option<&'a mut Asr> {
    if asr[model].is_none() {
        asr[model] = Asr::new(&s.models[model], s.workers[k].threads, s.config.fast);
        s.workers[k].broken.store(asr[model].is_none(), Relaxed);
    }
    asr[model].as_mut()
}

/// Worker k: the next job, done, until the engine goes
fn work(s: Arc<Shared>, k: usize) {
    let me = &s.workers[k];
    let mut asr: Vec<Option<Asr>> = s.models.iter().map(|_| None).collect();
    loop {
        let w = {
            let mut st = s.lock();
            loop {
                if st.stop {
                    return;
                }
                if let Some(w) = st.next_work() {
                    break w;
                }
                st = s.ready.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let id = w.id();
        me.busy.store(true, Relaxed);
        // A job that panics fails its request, and the worker goes on. Its
        // transcribers are whole: a panic is Rust's, between libpktito's
        // calls, never inside one.
        let job = AssertUnwindSafe(|| match w {
            Work::Plan { id, model, upload, task } => plan(&s, &mut asr, k, id, model, upload, task),
            Work::Hear { id, model, samples, job } => {
                let x = &samples[job.at.start as usize..job.at.end as usize];
                let toks = transcriber(&mut asr, &s, k, model).and_then(|a| a.run(x));
                s.lock().heard(id, &job, toks);
            }
        });
        if panic::catch_unwind(job).is_err() {
            s.lock().fail(id, Failure::Internal("the server failed on this request".into()));
            s.state.clear_poison();         // what the panic left is gone with the request
        }
        me.busy.store(false, Relaxed);
        s.ready.notify_all();              // its jobs may have made others: a retry, or the next plan's
    }
}

/// A request's plan, on worker k: its upload decoded, then its VAD and chunks
fn plan(s: &Shared, asr: &mut [Option<Asr>], k: usize, id: u64, model: usize, upload: Vec<u8>, task: Task) {
    let m = &s.models[model];
    let max = (s.config.max_seconds * m.rate as f64) as usize;
    let samples = match audio::decode(upload, m.rate, max, s.config.ffmpeg.as_deref()) {
        Ok(a) => Arc::new(a.samples),
        Err(e) => {
            let f = match e {
                AudioError::Unreadable(why) => Failure::Unreadable(why),
                AudioError::TooLong(limit) => Failure::TooLong(limit),
            };
            s.lock().fail(id, f);
            return;
        }
    };
    let n = samples.len();
    let Some(a) = transcriber(asr, s, k, model) else {
        s.lock().fail(id, Failure::Internal("no transcriber: out of memory or threads".into()));
        return;
    };
    match task {
        Task::Speech => {
            let segments = a.speech(&samples);
            let mut st = s.lock();
            if let Some(r) = st.remove(id) {
                let _ = r.tx.send(match segments {
                    Some(segments) => Event::Speech { samples: n, segments },
                    None => Event::Failed(Failure::Internal("out of memory".into())),
                });
            }
        }
        Task::Transcribe(o) => {
            // Safety: the steps go into the request beside the samples they
            // point into, and go before them (Request's field order); if the
            // request is gone, they go here, before the samples too.
            let steps = unsafe { Steps::new(a, &samples, &o) };
            let mut st = s.lock();
            let Some(r) = st.requests.get_mut(&id) else {
                drop(steps);
                return;
            };
            match steps {
                None => st.fail(id, Failure::Internal("out of memory, or max_chunk_s under a second".into())),
                Some(steps) => {
                    let chunks = steps.chunks();
                    let gone = r.tx.send(Event::Planned { samples: n, chunks }).is_err();
                    if gone {
                        drop(steps);
                        st.remove(id);
                    } else if chunks == 0 {
                        drop(steps);                // before the samples it points into
                        r.samples = Some(samples);
                        st.done(id);                // no speech: heard to its end
                    } else {
                        r.steps = Some(steps);
                        r.samples = Some(samples);
                    }
                }
            }
        }
    }
}
