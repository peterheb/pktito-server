//! GET /metrics: Prometheus's text format (0.0.4), written here, since a
//! few counters, gauges and one histogram need no client library. The names
//! are the server's and the measure's, as vLLM's and SGLang's are
//! (vllm:num_requests_running), and vLLM's own where the measure is the
//! same. Every label's value is the server's - a route, a status, a model's
//! name, a worker's state - never one a request sent, so the series stay
//! few.

use crate::engine::Engine;
use crate::ffi;
use std::collections::BTreeMap;
use std::fmt::{Display, Write};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A transcription's time's bounds, in seconds: a short recording takes a
/// tenth of one, four hours of audio minutes, more on a small board
pub const BUCKETS: [f64; 13] = [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

/// Times, counted as a Prometheus histogram counts them
#[derive(Clone, Debug, Default)]
pub struct Histogram {
    /// Each bucket's own, not those below it; the last past every bound
    counts: [u64; BUCKETS.len() + 1],
    sum: f64,
}

impl Histogram {
    pub fn observe(&mut self, seconds: f64) {
        let k = BUCKETS.iter().position(|&le| seconds <= le).unwrap_or(BUCKETS.len());
        self.counts[k] += 1;
        self.sum += seconds;
    }

    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }
}

/// The HTTP side's: when it started, and the API's answers by route and
/// status
pub struct Http {
    started: SystemTime,
    answers: Mutex<BTreeMap<(String, u16), u64>>,
}

impl Http {
    pub fn new() -> Http {
        Http { started: SystemTime::now(), answers: Mutex::default() }
    }

    pub fn answered(&self, route: &str, status: u16) {
        let mut a = self.answers.lock().unwrap_or_else(|e| e.into_inner());
        *a.entry((route.to_string(), status)).or_default() += 1;
    }

    fn answers(&self) -> Vec<((String, u16), u64)> {
        let a = self.answers.lock().unwrap_or_else(|e| e.into_inner());
        a.iter().map(|(k, &n)| (k.clone(), n)).collect()
    }
}

impl Default for Http {
    fn default() -> Http {
        Http::new()
    }
}

/// A label's value as the format quotes it
fn quoted(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[derive(Default)]
struct Page(String);

impl Page {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl Display) {
        self.0.push_str(name);
        if !labels.is_empty() {
            let ls: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", quoted(v))).collect();
            let _ = write!(self.0, "{{{}}}", ls.join(","));
        }
        let _ = writeln!(self.0, " {value}");
    }

    fn histogram(&mut self, name: &str, label: (&str, &str), h: &Histogram) {
        let mut below = 0;
        for (k, n) in h.counts.iter().enumerate() {
            below += n;
            let le = BUCKETS.get(k).map_or("+Inf".to_string(), |b| b.to_string());
            self.sample(&format!("{name}_bucket"), &[label, ("le", &le)], below);
        }
        self.sample(&format!("{name}_sum"), &[label], h.sum);
        self.sample(&format!("{name}_count"), &[label], below);
    }
}

/// What /metrics answers
pub fn page(e: &Engine, http: &Http) -> String {
    let s = e.stats();
    let w = e.workers_now();
    let (backend, compiler) = ffi::backend();
    let backend = backend.split(' ').next().unwrap_or_default(); // its name, "avx2 (x86-64 ...)"'s avx2
    let mut p = Page::default();
    p.family("pktito:server_info", "gauge", "The server's version, libpktito's, and the backend and compiler of its kernels");
    let libpktito = ffi::version();
    let info = [("version", env!("CARGO_PKG_VERSION")), ("libpktito", &libpktito), ("backend", backend), ("compiler", &compiler)];
    p.sample("pktito:server_info", &info, 1);
    p.family("process_start_time_seconds", "gauge", "Unix time the server started");
    p.sample("process_start_time_seconds", &[], http.started.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    p.family("pktito:threads", "gauge", "Threads for transcription, the workers' together (-t)");
    p.sample("pktito:threads", &[], e.worker_threads.iter().sum::<usize>());
    p.family("pktito:workers", "gauge", "Workers: idle, busy with a job, or failed (no transcriber to be had, or the thread gone)");
    for (state, n) in [("idle", w.idle), ("busy", w.busy), ("failed", w.failed)] {
        p.sample("pktito:workers", &[("state", state)], n);
    }
    p.family("pktito:max_requests", "gauge", "Requests taken at once (--max-requests); one more is a 503");
    p.sample("pktito:max_requests", &[], e.max_requests());
    p.family("pktito:num_requests_running", "gauge", "Requests being heard: decoded, cut into chunks, or their chunks heard");
    p.sample("pktito:num_requests_running", &[], s.running);
    p.family("pktito:num_requests_waiting", "gauge", "Requests holding a slot and not yet begun: their uploads coming in, or in hand");
    p.sample("pktito:num_requests_waiting", &[], s.waiting);
    p.family("pktito:http_requests_total", "counter", "Requests to the API (/v1) answered, by route and status");
    for ((route, status), n) in http.answers() {
        p.sample("pktito:http_requests_total", &[("route", &route), ("code", &status.to_string())], n);
    }
    p.family("pktito:audio_seconds_total", "counter", "Audio transcribed, in seconds, by model");
    for (m, seconds) in e.models().iter().zip(&s.heard) {
        p.sample("pktito:audio_seconds_total", &[("model_name", &m.name)], seconds);
    }
    p.family("pktito:audio_pending_seconds", "gauge", "Audio decoded and not yet transcribed, in seconds: the work in hand");
    p.sample("pktito:audio_pending_seconds", &[], s.pending);
    p.family(
        "pktito:e2e_request_latency_seconds",
        "histogram",
        "A transcription's time, from its upload in hand to its last chunk heard, by model",
    );
    for (m, h) in e.models().iter().zip(&s.took) {
        p.histogram("pktito:e2e_request_latency_seconds", ("model_name", &m.name), h);
    }
    p.0
}
