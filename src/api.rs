//! The HTTP side: OpenAI's POST /v1/audio/transcriptions and GET /v1/models,
//! and pktito's own POST /v1/audio/vad (a recording's speech, as pktito-c
//! segments finds it). Each request's work is the engine's; here are its
//! form, its answer's format, and OpenAI's errors.
//!
//! The key, when the server has one, is the API's, /v1: GET /health, /ping,
//! /version and /metrics take none, as vLLM's take none, so that a probe or
//! a scraper needs no secret. They tell what the server runs and how busy
//! it is, never what a request sent.

use crate::engine::{Busy, Engine, Event, Failure, Health, Task};
use crate::ffi::{self, Model};
use crate::format::{self, Heard, Transcript};
use crate::metrics::{self, Http};
use crate::serve::Stalled;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, MatchedPath, Multipart, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::ffi::c_int;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;

pub struct Server {
    pub engine: Engine,
    /// The key every request bears, if the server has one
    pub api_key: Option<String>,
}

/// The server as its handlers have it, with what the HTTP side counts
struct Api {
    server: Server,
    http: Http,
}

impl std::ops::Deref for Api {
    type Target = Server;

    fn deref(&self) -> &Server {
        &self.server
    }
}

type App = Arc<Api>;

pub fn router(server: Server, max_upload: usize) -> Router {
    let app = Arc::new(Api { server, http: Http::new() });
    Router::new()
        .route("/v1/audio/transcriptions", post(transcriptions))
        .route("/v1/audio/vad", post(vad))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model))
        .route_layer(middleware::from_fn_with_state(app.clone(), count))
        .route("/health", get(health))
        .route("/ping", get(ping))
        .route("/version", get(version))
        .route("/metrics", get(prometheus))
        .layer(DefaultBodyLimit::max(max_upload))
        .with_state(app)
}

/// An API request's answer, counted by its route and status
async fn count(State(app): State<App>, req: Request, next: Next) -> Response {
    let route = req.extensions().get::<MatchedPath>().map_or(String::new(), |p| p.as_str().to_string());
    let res = next.run(req).await;
    app.http.answered(&route, res.status().as_u16());
    res
}

/// An error as OpenAI's API gives one
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    code: Option<&'static str>,
    param: Option<&'static str>,
    message: String,
}

impl ApiError {
    fn invalid(param: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::BAD_REQUEST, kind: "invalid_request_error", code: None, param: Some(param), message: message.into() }
    }

    fn internal(message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, kind: "server_error", code: None, param: None, message: message.into() }
    }

    fn body(&self) -> Value {
        json!({ "error": { "message": self.message, "type": self.kind, "param": self.param, "code": self.code } })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

impl From<Failure> for ApiError {
    fn from(f: Failure) -> ApiError {
        match f {
            Failure::Unreadable(why) => ApiError::invalid("file", why),
            Failure::TooLong(limit) => {
                ApiError::invalid("file", format!("the recording is longer than this server takes, {limit:.0} s (--max-hours)"))
            }
            Failure::Internal(why) => ApiError::internal(why),
        }
    }
}

impl From<Busy> for ApiError {
    fn from(_: Busy) -> ApiError {
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            kind: "server_error",
            code: Some("server_busy"),
            param: None,
            message: "every request this server takes at once is taken (--max-requests): try again".into(),
        }
    }
}

impl From<MultipartError> for ApiError {
    fn from(e: MultipartError) -> ApiError {
        // an upload that stopped coming (serve's Idle): Stalled, down the
        // errors multer and axum wrap it in
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&e);
        while let Some(c) = cause {
            if let Some(s) = c.downcast_ref::<Stalled>() {
                return ApiError {
                    status: StatusCode::REQUEST_TIMEOUT,
                    kind: "invalid_request_error",
                    code: None,
                    param: Some("file"),
                    message: s.to_string(),
                };
            }
            cause = c.source();
        }
        ApiError { status: e.status(), kind: "invalid_request_error", code: None, param: None, message: e.body_text() }
    }
}

/// The same bytes, in a time that does not say how many lead
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn authorize(app: &Server, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(key) = &app.api_key else {
        return Ok(());
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if same(given.as_bytes(), key.as_bytes()) {
        return Ok(());
    }
    Err(ApiError {
        status: StatusCode::UNAUTHORIZED,
        kind: "invalid_request_error",
        code: Some("invalid_api_key"),
        param: None,
        message: "this server takes requests with its key: Authorization: Bearer KEY".into(),
    })
}

/// A request's form. prompt and temperature, Whisper's, are read and left:
/// a transducer has nothing they set.
#[derive(Default)]
struct Form {
    file: Option<Vec<u8>>,
    model: String,
    language: Option<String>,
    response_format: Option<String>,
    granularities: Vec<String>,
    stream: bool,
    /// pktito's own: pktito-c's --no-retry and --no-context, as false
    retry: Option<bool>,
    context: Option<bool>,
}

fn boolean(param: &'static str, s: &str) -> Result<bool, ApiError> {
    match s.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(ApiError::invalid(param, format!("{param} {other}: true or false"))),
    }
}

async fn form(mut mp: Multipart) -> Result<Form, ApiError> {
    let mut f = Form::default();
    while let Some(field) = mp.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => f.file = Some(field.bytes().await?.into()),
            "model" => f.model = field.text().await?.trim().to_string(),
            "language" => {
                let l = field.text().await?.trim().to_lowercase();
                f.language = (!l.is_empty()).then_some(l);
            }
            "response_format" => f.response_format = Some(field.text().await?.trim().to_string()),
            "timestamp_granularities[]" | "timestamp_granularities" => {
                f.granularities.push(field.text().await?.trim().to_string())
            }
            "stream" => f.stream = boolean("stream", &field.text().await?)?,
            "retry" => f.retry = Some(boolean("retry", &field.text().await?)?),
            "context" => f.context = Some(boolean("context", &field.text().await?)?),
            _ => {}
        }
    }
    Ok(f)
}

/// The model a request names; for a name the server does not have (an
/// OpenAI client's whisper-1), the first that writes its language, or the
/// first of all
fn pick(app: &Server, name: &str, language: Option<&str>) -> Result<usize, ApiError> {
    let models = app.engine.models();
    if let Some(i) = models.iter().position(|m| m.name == name) {
        return match language {
            Some(l) if !models[i].writes(l) => {
                Err(ApiError::invalid("language", format!("{name} writes {}, not {l}", models[i].languages.join(" "))))
            }
            _ => Ok(i),
        };
    }
    match language {
        None => Ok(0),
        Some(l) => models
            .iter()
            .position(|m| m.writes(l))
            .ok_or_else(|| ApiError::invalid("language", format!("no model here writes {l}"))),
    }
}

/// A transcription's chunks as they come, to its end
async fn collect(m: &Model, mut rx: UnboundedReceiver<Event>) -> Result<(usize, Vec<Heard>), ApiError> {
    let mut samples = 0;
    let mut chunks = Vec::new();
    loop {
        match rx.recv().await {
            Some(Event::Planned { samples: n, .. }) => samples = n,
            Some(Event::Chunk(c)) => chunks.push(format::heard(m, &c)),
            Some(Event::Done) => return Ok((samples, chunks)),
            Some(Event::Failed(f)) => return Err(f.into()),
            Some(Event::Speech { .. }) | None => return Err(ApiError::internal("the request was dropped")),
        }
    }
}

fn sse(v: Value) -> SseEvent {
    SseEvent::default().data(v.to_string())
}

/// stream=true: each chunk's text as it is done, in order, as OpenAI's
/// transcript.text.delta events, then transcript.text.done with the whole
fn stream(m: Arc<Model>, rx: UnboundedReceiver<Event>) -> Response {
    let events = futures::stream::unfold(Some((rx, String::new())), move |st| {
        let m = m.clone();
        async move {
            let (mut rx, mut text) = st?;
            let last = loop {
                match rx.recv().await {
                    Some(Event::Chunk(c)) => {
                        let t = m.text(&c.toks);
                        if t.is_empty() {
                            continue;
                        }
                        let delta = if text.is_empty() { t } else { format!(" {t}") };
                        text.push_str(&delta);
                        let e = sse(json!({ "type": "transcript.text.delta", "delta": delta }));
                        return Some((Ok::<_, Infallible>(e), Some((rx, text))));
                    }
                    Some(Event::Planned { .. }) => continue,
                    Some(Event::Done) => break sse(json!({ "type": "transcript.text.done", "text": text })),
                    Some(Event::Failed(f)) => {
                        let mut e = ApiError::from(f).body();
                        e["type"] = "error".into();
                        break sse(e);
                    }
                    Some(Event::Speech { .. }) | None => {
                        let mut e = ApiError::internal("the request was dropped").body();
                        e["type"] = "error".into();
                        break sse(e);
                    }
                }
            };
            Some((Ok(last), None))
        }
    });
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
}

fn plain(content_type: &'static str, body: String) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

const FORMATS: &str = "json, text, srt, vtt, verbose_json or pktito_json";

async fn transcriptions(State(app): State<App>, headers: HeaderMap, mp: Multipart) -> Result<Response, ApiError> {
    authorize(&app, &headers)?;
    // the request's slot before its upload: uploads coming in count against
    // --max-requests, bytes and all, and a busy server says so before the
    // file is sent
    let slot = app.engine.slot()?;
    let f = form(mp).await?;
    let format = f.response_format.clone().unwrap_or_else(|| "json".into());
    if !matches!(format.as_str(), "json" | "text" | "srt" | "vtt" | "verbose_json" | "pktito_json") {
        return Err(ApiError::invalid("response_format", format!("response_format {format}: {FORMATS}")));
    }
    if let Some(g) = f.granularities.iter().find(|g| *g != "word" && *g != "segment") {
        return Err(ApiError::invalid("timestamp_granularities", format!("timestamp_granularities {g}: word or segment")));
    }
    let file = f.file.ok_or_else(|| ApiError::invalid("file", "no file"))?;
    let i = pick(&app, &f.model, f.language.as_deref())?;
    let m = app.engine.models()[i].clone();
    let mut o = ffi::transcribe_defaults();
    if let Some(r) = f.retry {
        o.retry = r as c_int;
    }
    if let Some(c) = f.context {
        o.context = c as c_int;
    }
    let t0 = Instant::now();
    let mut rx = slot.submit(i, file, Task::Transcribe(o));
    if f.stream {
        // An upload that is not audio is an error before the stream, not in it
        return match rx.recv().await {
            Some(Event::Planned { .. }) => Ok(stream(m, rx)),
            Some(Event::Failed(f)) => Err(f.into()),
            _ => Err(ApiError::internal("the request was dropped")),
        };
    }
    let (samples, chunks) = collect(&m, rx).await?;
    let t = Transcript { model: &m, fast: app.engine.fast(), samples, chunks };
    let took = t0.elapsed().as_secs_f64();
    tracing::info!(
        "{}: {:.1} s of audio in {:.2} s ({:.0}x), {} chunks, {}",
        m.name,
        t.seconds(),
        took,
        t.seconds() / took.max(1e-9),
        t.chunks.len(),
        format
    );
    let words = f.granularities.iter().any(|g| g == "word");
    let segments = f.granularities.is_empty() || f.granularities.iter().any(|g| g == "segment");
    let no_memory = || ApiError::internal("out of memory");
    Ok(match format.as_str() {
        "json" => Json(format::json(&t)).into_response(),
        "text" => plain("text/plain; charset=utf-8", t.text() + "\n"),
        "srt" => plain("text/plain; charset=utf-8", format::subtitles(&t, false).ok_or_else(no_memory)?),
        "vtt" => plain("text/vtt; charset=utf-8", format::subtitles(&t, true).ok_or_else(no_memory)?),
        "verbose_json" => Json(format::verbose_json(&t, segments, words).ok_or_else(no_memory)?).into_response(),
        _ => plain("application/json", format::pktito_json(&t)),
    })
}

/// A recording's speech: Silero's segments with its defaults, as pktito-c
/// segments prints them, in seconds
async fn vad(State(app): State<App>, headers: HeaderMap, mp: Multipart) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let slot = app.engine.slot()?;                       // as a transcription's
    let f = form(mp).await?;
    let file = f.file.ok_or_else(|| ApiError::invalid("file", "no file"))?;
    let i = pick(&app, &f.model, None)?;                 // each model file holds the VAD
    let rate = app.engine.models()[i].rate as f64;
    let mut rx = slot.submit(i, file, Task::Speech);
    match rx.recv().await {
        Some(Event::Speech { samples, segments }) => {
            let segs: Vec<Value> = segments
                .iter()
                .map(|s| json!({ "start": s.start as f64 / rate, "end": s.end as f64 / rate }))
                .collect();
            Ok(Json(json!({ "duration": samples as f64 / rate, "segments": segs })))
        }
        Some(Event::Failed(f)) => Err(f.into()),
        _ => Err(ApiError::internal("the request was dropped")),
    }
}

fn model_json(m: &Model) -> Value {
    let owner = if m.credit.is_empty() { "pktito" } else { m.credit.as_str() };
    json!({ "id": m.name, "object": "model", "created": 0, "owned_by": owner })
}

async fn models(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let data: Vec<Value> = app.engine.models().iter().map(|m| model_json(m)).collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}

async fn model(State(app): State<App>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    match app.engine.models().iter().find(|m| m.name == id) {
        Some(m) => Ok(Json(model_json(m))),
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            kind: "invalid_request_error",
            code: Some("model_not_found"),
            param: Some("model"),
            message: format!("no model {id} here"),
        }),
    }
}

fn status(h: Health) -> (&'static str, StatusCode) {
    match h {
        Health::Ok => ("ok", StatusCode::OK),
        Health::Degraded => ("degraded", StatusCode::OK),
        Health::Unavailable => ("unavailable", StatusCode::SERVICE_UNAVAILABLE),
    }
}

/// Whether the server can hear a request, and what it is doing: 200 while a
/// worker can ("ok" when every one can, "degraded" when some cannot), 503
/// when none can
async fn health(State(app): State<App>) -> (StatusCode, Json<Value>) {
    let w = app.engine.workers_now();
    let (said, code) = status(w.health());
    let (backend, compiler) = ffi::backend();
    let body = json!({
        "status": said,
        "workers": app.engine.workers,
        "worker_threads": app.engine.worker_threads,
        "workers_busy": w.busy,
        "workers_failed": w.failed,
        "requests": app.engine.requests(),
        "max_requests": app.engine.max_requests(),
        "fast": app.engine.fast(),
        "backend": backend,
        "compiler": compiler,
    });
    (code, Json(body))
}

/// /health's status alone, with no body, as vLLM's and SGLang's /ping
async fn ping(State(app): State<App>) -> StatusCode {
    status(app.engine.workers_now().health()).1
}

/// The server's version, and libpktito's
async fn version() -> Json<Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION"), "libpktito": ffi::version() }))
}

async fn prometheus(State(app): State<App>) -> Response {
    plain("text/plain; version=0.0.4; charset=utf-8", metrics::page(&app.engine, &app.http))
}
