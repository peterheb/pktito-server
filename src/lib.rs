//! pktito-server: OpenAI's speech-to-text API over libpktito, Parakeet TDT
//! on the CPU. The threads it may use are dealt into workers, and the
//! requests' chunks among them in turn (engine); what comes back is the
//! transcript pktito-c gives (format), over connections that close on a
//! client that stops (serve), and what it is doing is Prometheus's to read
//! (metrics).

pub mod api;
pub mod audio;
pub mod engine;
pub mod ffi;
pub mod ffmpeg;
pub mod format;
pub mod metrics;
pub mod serve;
