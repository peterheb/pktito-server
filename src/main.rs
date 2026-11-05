use clap::Parser;
use pktito_server::api::{self, Server};
use pktito_server::engine::{Config, Engine};
use pktito_server::ffi::{self, Asr, Model};
use pktito_server::ffmpeg::Ffmpeg;
use pktito_server::serve;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const HELP: &str = "\
usage: pktito-server -m FILE.pkf [options]   serve OpenAI's speech-to-text API

  -m, --model FILE.pkf    model to serve (repeatable); the first is the default
  -t, --threads N         threads (default: one per core)
  -w, --workers N         workers, the threads dealt among them (default: one
                          per 4 threads); a worker hears one chunk at a time
  -l, --listen ADDR       address to listen on (127.0.0.1:8000)
      --api-key KEY       require \"Authorization: Bearer KEY\" ($PKTITO_API_KEY)
      --max-upload-mb MB  largest upload (256)
      --max-hours H       longest recording (4)
      --max-requests N    requests at once (16); more get 503
      --fast              up to 7% faster; transcripts may differ slightly
      --ffmpeg PATH       ffmpeg 6.0 or later, for formats besides WAV and AIFF
  -V, --version           the version
";

#[derive(Parser)]
#[command(version, override_help = HELP)]
struct Args {
    #[arg(short, long = "model", value_name = "FILE.pkf", required = true)]
    model: Vec<PathBuf>,
    #[arg(short, long, value_name = "N")]
    threads: Option<usize>,
    #[arg(short, long, value_name = "N")]
    workers: Option<usize>,
    #[arg(short, long, value_name = "ADDR", default_value = "127.0.0.1:8000")]
    listen: SocketAddr,
    #[arg(
        long,
        value_name = "KEY",
        env = "PKTITO_API_KEY",
        hide_env_values = true
    )]
    api_key: Option<String>,
    #[arg(long, value_name = "MB", default_value_t = 256)]
    max_upload_mb: usize,
    #[arg(long, value_name = "H", default_value_t = 4.0)]
    max_hours: f64,
    #[arg(long, value_name = "N", default_value_t = 16)]
    max_requests: usize,
    // pk_asr_set_fast_mode: WER level, a few more transcripts not float32's
    #[arg(long)]
    fast: bool,
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let args = Args::parse();
    let mut models = Vec::new();
    for path in &args.model {
        match Model::load(path) {
            Ok(m) => models.push(Arc::new(m)),
            Err(e) => {
                eprintln!("pktito-server: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }
    // The ffmpeg's check: it runs under a decode's limits, and reads fd:
    let ffmpeg = match &args.ffmpeg {
        Some(p) => {
            // ffmpeg runs as the server's user, who can read a dumpable
            // process's environment, and the key in it, through /proc: in
            // Docker as DOCKER.md runs it, a stand-in for an ffmpeg that a
            // crafted upload took over read it (2026-11-04). The /proc
            // entries of a process that is not dumpable are root's.
            #[cfg(target_os = "linux")]
            if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
                eprintln!(
                    "pktito-server: --ffmpeg: cannot hide the server's environment: {}",
                    std::io::Error::last_os_error()
                );
                return ExitCode::FAILURE;
            }
            match Ffmpeg::new(p, args.max_hours * 3600.0) {
                Ok(f) => Some(Arc::new(f)),
                Err(e) => {
                    eprintln!("pktito-server: --ffmpeg {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };
    // libpktito's count of the cores, from a transcriber made to ask it
    let threads = args
        .threads
        .unwrap_or_else(|| Asr::new(&models[0], 0, false).map_or(1, |a| a.threads()));
    if threads == 0 {
        eprintln!("pktito-server: -t: 1 or more");
        return ExitCode::FAILURE;
    }
    // A worker for every 4 threads: libpktito's own split of a transcriber's
    // threads into lanes (core/asr.h, PK_LANE_THREADS), the threads 4 doesn't
    // divide dealt among them. -w 1, one worker with every thread, hears a
    // one-chunk request as fast as pktito-c does.
    let workers = args.workers.unwrap_or((threads / 4).max(1));
    if workers == 0 || workers > threads {
        eprintln!("pktito-server: -w: from 1 to the threads ({threads})");
        return ExitCode::FAILURE;
    }
    if args.workers.is_some() && threads % workers != 0 {
        tracing::warn!("{threads} threads don't divide evenly into {workers} workers");
    }
    let engine = Engine::new(
        models.clone(),
        Config {
            threads,
            workers,
            fast: args.fast,
            max_requests: args.max_requests.max(1),
            max_seconds: args.max_hours * 3600.0,
            ffmpeg: ffmpeg.clone(),
        },
    );
    let (backend, compiler) = ffi::backend();
    tracing::info!(
        "{}, {backend}, {compiler}{}",
        workers_of(&engine.worker_threads),
        if args.fast { ", fast mode" } else { "" }
    );
    for m in &models {
        tracing::info!("model {}, {}", m.name, m.languages.join(" "));
    }
    match &ffmpeg {
        Some(f) => tracing::info!(
            "formats besides WAV and AIFF: {}, {}",
            f.path.display(),
            f.version
        ),
        None => tracing::info!("WAV and AIFF only (--ffmpeg PATH for other formats)"),
    }
    let app = api::router(
        Server {
            engine,
            api_key: args.api_key,
        },
        args.max_upload_mb << 20,
    );
    // The HTTP side's threads read uploads and write answers; the workers
    // have the transcription's
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("pktito-http")
        .enable_all()
        .build()
        .expect("a runtime");
    let served = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(args.listen).await?;
        tracing::info!("listening on http://{}", listener.local_addr()?);
        serve::serve(listener, app, serve::HEADERS, serve::STALL).await;
        Ok::<_, std::io::Error>(())
    });
    if let Err(e) = served {
        eprintln!("pktito-server: {}: {e}", args.listen);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// "2 workers of 4 threads", or where they differ "3 workers: 2 of 5
/// threads, 1 of 4"
fn workers_of(each: &[usize]) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let (most, least) = (each[0], each[each.len() - 1]);
    if most == least {
        return format!(
            "{} of {}",
            plural(each.len(), "worker", "workers"),
            plural(most, "thread", "threads")
        );
    }
    let wide = each.iter().filter(|&&n| n == most).count();
    format!(
        "{} workers: {wide} of {most} threads, {} of {least}",
        each.len(),
        each.len() - wide
    )
}
