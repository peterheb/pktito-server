//! The HTTP side, a request at a time: each answer is pktito-c's for the
//! same recording - pktito_json its --json byte for byte (libpktito's
//! tests/data/test.v2.json and the rest), srt and vtt its --srt and --vtt,
//! the VAD its segments - and OpenAI's formats carry the same text.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use pktito_server::api::{self, Server};
use pktito_server::ffi::Model;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

fn app(models: Vec<Arc<Model>>, fast: bool, api_key: Option<&str>) -> Router {
    let engine = common::engine(models, 4, 2, fast);
    api::router(Server { engine, api_key: api_key.map(String::from) }, 64 << 20)
}

fn multipart(fields: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "pktito-test-boundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        if *name == "file" {
            body.extend_from_slice(
                b"Content-Disposition: form-data; name=\"file\"; filename=\"test.wav\"\r\nContent-Type: audio/wav\r\n\r\n",
            );
        } else {
            body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
        }
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ctype = resp.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    (status, ctype, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

async fn post(app: &Router, path: &str, fields: &[(&str, &[u8])], key: Option<&str>) -> (StatusCode, String, Vec<u8>) {
    let (ctype, body) = multipart(fields);
    let mut req = Request::builder().method("POST").uri(path).header("content-type", ctype);
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    send(app, req.body(Body::from(body)).unwrap()).await
}

async fn get(app: &Router, path: &str, key: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().uri(path);
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let (status, _, body) = send(app, req.body(Body::empty()).unwrap()).await;
    (status, body)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(body)))
}

/// Where two answers part, for a failure's message
fn parting(got: &[u8], want: &[u8]) -> String {
    let at = got.iter().zip(want).position(|(a, b)| a != b).unwrap_or(got.len().min(want.len()));
    let around = |b: &[u8]| String::from_utf8_lossy(&b[at.saturating_sub(60)..(at + 60).min(b.len())]).into_owned();
    format!("{} bytes against {}, parting at {at}:\n got  {}\n want {}", got.len(), want.len(), around(got), around(want))
}

#[tokio::test]
async fn pktito_json_is_pktito_cs_byte_for_byte() {
    let wav = common::data("test.wav");
    for v in ["v2", "v3"] {
        let Some(m) = common::model(v) else { continue };
        for fast in [false, true] {
            let app = app(vec![m.clone()], fast, None);
            let fields: [(&str, &[u8]); 3] =
                [("file", &wav), ("model", m.name.as_bytes()), ("response_format", b"pktito_json")];
            let (status, ctype, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
            assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
            assert_eq!(ctype, "application/json");
            let want = common::data(&format!("test.{v}{}.json", if fast { ".fast" } else { "" }));
            assert!(body == want, "{v}{}: {}", if fast { " fast" } else { "" }, parting(&body, &want));
        }
    }
}

/// test.wav as two channels at 22.05 kHz, the right at half the left: what
/// the server's reader mixes and resamples as pktito-c's does
fn stereo_22k(wav: &[u8]) -> Vec<u8> {
    let pcm: Vec<i16> = wav[44..].chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let len = (pcm.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());                  // PCM
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&22050u32.to_le_bytes());
    out.extend_from_slice(&(22050u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&(s / 2).to_le_bytes());
    }
    out
}

#[tokio::test]
async fn another_rate_and_two_channels_are_pktito_cs() {
    let Some(m) = common::model("v2") else { return };
    let wav = stereo_22k(&common::data("test.wav"));
    let file = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("test-stereo-22k.wav");
    std::fs::write(&file, &wav).unwrap();
    let path = common::model_path("v2");
    let want = common::cli(&["-t", "4", "--json", "-m", path.to_str().unwrap(), file.to_str().unwrap()]);
    let app = app(vec![m], false, None);
    let fields: [(&str, &[u8]); 2] = [("file", &wav), ("response_format", b"pktito_json")];
    let (status, _, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body == want, "{}", parting(&body, &want));
}

#[tokio::test]
async fn subtitles_are_pktito_cs() {
    let wav = common::data("test.wav");
    for v in ["v2", "v3"] {
        let Some(m) = common::model(v) else { continue };
        let path = common::model_path(v);
        let app = app(vec![m.clone()], false, None);
        for (format, flag) in [("srt", "--srt"), ("vtt", "--vtt")] {
            let fields: [(&str, &[u8]); 2] = [("file", &wav), ("response_format", format.as_bytes())];
            let (status, _, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
            assert_eq!(status, StatusCode::OK);
            let want = common::cli(&["-t", "4", flag, "-m", path.to_str().unwrap(), "tests/data/test.wav"]);
            assert!(body == want, "{v} {format}: {}", parting(&body, &want));
        }
    }
}

#[tokio::test]
async fn openais_formats_carry_pktito_cs_text() {
    let Some(m) = common::model("v2") else { return };
    let wav = common::data("test.wav");
    let path = common::model_path("v2");
    // pktito-c writes a line a chunk; OpenAI's text is the chunks' with a space between
    let lines = common::cli(&["-t", "4", "-m", path.to_str().unwrap(), "tests/data/test.wav"]);
    let text = String::from_utf8(lines).unwrap().lines().collect::<Vec<_>>().join(" ");
    let app = app(vec![m.clone()], false, None);

    let fields: [(&str, &[u8]); 2] = [("file", &wav), ("model", b"whisper-1")];
    let (status, _, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body), serde_json::json!({ "text": text }));

    let fields: [(&str, &[u8]); 2] = [("file", &wav), ("response_format", b"text")];
    let (status, ctype, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/plain"));
    assert_eq!(String::from_utf8(body).unwrap(), text.clone() + "\n");

    let fields: [(&str, &[u8]); 4] = [
        ("file", &wav),
        ("response_format", b"verbose_json"),
        ("timestamp_granularities[]", b"word"),
        ("timestamp_granularities[]", b"segment"),
    ];
    let (status, _, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    let cli = json(&common::data("test.v2.json"));
    assert_eq!(v["text"], text.as_str());
    assert_eq!(v["language"], "english");
    assert_eq!(v["duration"], 60.0);
    let segs = v["segments"].as_array().unwrap();
    let joined: Vec<&str> = segs.iter().map(|s| s["text"].as_str().unwrap()).collect();
    assert_eq!(joined.join(" "), text);
    let words: Vec<&Value> = cli["chunks"].as_array().unwrap().iter().flat_map(|c| c["words"].as_array().unwrap()).collect();
    // the segments are libpktito's sentences (its tests/sentcheck prints test.wav's eight)
    assert_eq!(segs.len(), 8);
    assert_eq!(segs[0]["text"], "And says, How do I get to Dublin?");
    assert_eq!(segs[7]["text"], "So when I think about normative questions,");
    let time = |x: &Value| x.as_f64().unwrap();
    assert!((time(&segs[0]["start"]) - time(&words[0]["start"])).abs() < 0.006);
    for (i, s) in segs.iter().enumerate() {
        assert_eq!(s["id"], i);
        assert!(time(&s["start"]) < time(&s["end"]) && (i == 0 || time(&segs[i - 1]["end"]) <= time(&s["start"])));
    }
    let got = v["words"].as_array().unwrap();
    assert_eq!(got.len(), words.len());
    for (g, w) in got.iter().zip(&words) {
        assert_eq!((&g["word"], &g["start"], &g["end"]), (&w["word"], &w["start"], &w["end"]));
    }

    // words alone, when only words are asked for
    let fields: [(&str, &[u8]); 3] =
        [("file", &wav), ("response_format", b"verbose_json"), ("timestamp_granularities[]", b"word")];
    let (_, _, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    let v = json(&body);
    assert!(v.get("segments").is_none() && v["words"].as_array().unwrap().len() == words.len());
}

#[tokio::test]
async fn a_stream_is_the_text_chunk_by_chunk() {
    let Some(m) = common::model("v2") else { return };
    let wav = common::data("test.wav");
    let app = app(vec![m.clone()], false, None);
    let (_, _, body) = post(&app, "/v1/audio/transcriptions", &[("file", &wav)], None).await;
    let text = json(&body)["text"].as_str().unwrap().to_string();

    let fields: [(&str, &[u8]); 2] = [("file", &wav), ("stream", b"true")];
    let (status, ctype, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/event-stream"));
    let events: Vec<Value> = String::from_utf8(body)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| json(d.as_bytes()))
        .collect();
    let (last, deltas) = events.split_last().unwrap();
    assert!(deltas.len() >= 2, "a delta a chunk: {deltas:?}");
    let streamed: String = deltas
        .iter()
        .map(|e| {
            assert_eq!(e["type"], "transcript.text.delta");
            e["delta"].as_str().unwrap()
        })
        .collect();
    assert_eq!(streamed, text);
    assert_eq!(last["type"], "transcript.text.done");
    assert_eq!(last["text"], text.as_str());

    // an upload that is not audio is an error before the stream
    let fields: [(&str, &[u8]); 2] = [("file", b"not audio at all"), ("stream", b"true")];
    let (status, _, body) = post(&app, "/v1/audio/transcriptions", &fields, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["error"]["param"], "file");
}

#[tokio::test]
async fn vad_is_pktito_c_segments() {
    let Some(m) = common::model("v2") else { return };
    let wav = common::data("test.wav");
    let path = common::model_path("v2");
    let app = app(vec![m], false, None);
    let (status, _, body) = post(&app, "/v1/audio/vad", &[("file", &wav)], None).await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["duration"], 60.0);
    let got: String = v["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| format!("{:.3} {:.3}\n", s["start"].as_f64().unwrap(), s["end"].as_f64().unwrap()))
        .collect();
    let want = common::cli(&["segments", "-t", "4", "-m", path.to_str().unwrap(), "tests/data/test.wav"]);
    assert_eq!(got, String::from_utf8(want).unwrap());
}

#[tokio::test]
async fn errors_are_openais() {
    let Some(m) = common::model("v2") else { return };
    let wav = common::data("test.wav");
    let app = app(vec![m.clone()], false, Some("sesame"));
    let key = Some("sesame");

    let (status, body) = get(&app, "/v1/models", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json(&body)["error"]["code"], "invalid_api_key");
    let (status, body) = get(&app, "/v1/models", Some("sesame!")).await;
    assert_eq!((status, json(&body)["error"]["code"].clone()), (StatusCode::UNAUTHORIZED, "invalid_api_key".into()));

    let (status, body) = get(&app, "/v1/models", key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["data"][0]["id"], m.name.as_str());
    assert_eq!(json(&body)["data"][0]["owned_by"], "NVIDIA");
    let (status, _) = get(&app, &format!("/v1/models/{}", m.name), key).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get(&app, "/v1/models/whisper-1", key).await;
    assert_eq!((status, json(&body)["error"]["code"].clone()), (StatusCode::NOT_FOUND, "model_not_found".into()));

    type Fields<'a> = &'a [(&'a str, &'a [u8])];
    let cases: [(Fields, &str); 5] = [
        (&[("model", b"whisper-1")], "file"),
        (&[("file", &wav), ("response_format", b"xml")], "response_format"),
        (&[("file", &wav), ("timestamp_granularities[]", b"char")], "timestamp_granularities"),
        (&[("file", b"RIFF and nothing of a WAV after it")], "file"),
        (&[("file", &wav), ("model", m.name.as_bytes()), ("language", b"fr")], "language"),
    ];
    for (fields, param) in cases {
        let (status, _, body) = post(&app, "/v1/audio/transcriptions", fields, key).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{param}");
        let e = json(&body);
        assert_eq!((e["error"]["type"].as_str(), e["error"]["param"].as_str()), (Some("invalid_request_error"), Some(param)));
    }
    let (status, _, _) = post(&app, "/v1/audio/transcriptions", &[("file", &wav), ("language", b"en")], key).await;
    assert_eq!(status, StatusCode::OK);
}

/// A metrics page's samples, each line not a comment checked against the
/// format: a name, its labels, a number
fn samples(page: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for line in page.lines() {
        if line.starts_with("# HELP ") || line.starts_with("# TYPE ") {
            continue;
        }
        let (series, value) = line.rsplit_once(' ').unwrap_or_else(|| panic!("{line}"));
        let name = series.split('{').next().unwrap();
        let named = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == ':';
        assert!(name.starts_with(|c: char| c.is_ascii_lowercase()) && name.chars().all(named), "{line}");
        assert!(series == name || (series.ends_with('}') && series.matches('{').count() == 1), "{line}");
        assert!(!value.starts_with('-'), "{line}"); // none of these goes below 0, nor shows -0
        out.push((series.to_string(), value.parse::<f64>().unwrap_or_else(|e| panic!("{e}: {line}"))));
    }
    out
}

fn sample(page: &[(String, f64)], series: &str) -> f64 {
    page.iter().find(|(s, _)| s == series).unwrap_or_else(|| panic!("no {series}")).1
}

/// /health, /ping, /version and /metrics, with the key set and without it;
/// a transcription's answer, audio and time in the metrics after it
#[tokio::test]
async fn the_checks_need_no_key_and_count_what_was_done() {
    let Some(m) = common::model("v2") else { return };
    let app = app(vec![m.clone()], false, Some("sesame"));
    let (status, body) = get(&app, "/health", None).await;
    let h = json(&body);
    assert_eq!(status, StatusCode::OK);
    assert_eq!((h["status"].as_str(), h["workers"].as_u64(), h["workers_failed"].as_u64()), (Some("ok"), Some(2), Some(0)));
    assert_eq!((h["requests"].as_u64(), h["max_requests"].as_u64()), (Some(0), Some(16)));
    assert_eq!(get(&app, "/ping", None).await, (StatusCode::OK, Vec::new()));
    let (status, body) = get(&app, "/version", None).await;
    assert_eq!((status, json(&body)["version"].as_str()), (StatusCode::OK, Some(env!("CARGO_PKG_VERSION"))));

    let model = format!("{{model_name=\"{}\"}}", m.name);
    let metrics = |app: Router| async move {
        let (status, ctype, body) = send(&app, Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await;
        assert_eq!((status, ctype.as_str()), (StatusCode::OK, "text/plain; version=0.0.4; charset=utf-8"));
        samples(&String::from_utf8(body).unwrap())
    };
    let before = metrics(app.clone()).await;
    assert_eq!(sample(&before, "pktito:threads"), 4.0);
    assert_eq!(sample(&before, "pktito:workers{state=\"idle\"}"), 2.0);
    assert_eq!(sample(&before, "pktito:max_requests"), 16.0);
    assert_eq!(sample(&before, &format!("pktito:audio_seconds_total{model}")), 0.0);
    assert!(sample(&before, "process_start_time_seconds") > 1.7e9);
    assert!(!before.iter().any(|(s, _)| s.starts_with("pktito:http_requests_total")));

    assert_eq!(get(&app, "/v1/models", None).await.0, StatusCode::UNAUTHORIZED);
    let (status, _, _) = post(&app, "/v1/audio/transcriptions", &[("file", &common::data("test.wav"))], Some("sesame")).await;
    assert_eq!(status, StatusCode::OK);
    let after = metrics(app.clone()).await;
    assert_eq!(sample(&after, "pktito:http_requests_total{route=\"/v1/models\",code=\"401\"}"), 1.0);
    assert_eq!(sample(&after, "pktito:http_requests_total{route=\"/v1/audio/transcriptions\",code=\"200\"}"), 1.0);
    assert_eq!(sample(&after, &format!("pktito:audio_seconds_total{model}")), 60.0);
    assert_eq!(sample(&after, "pktito:audio_pending_seconds"), 0.0);
    assert_eq!(sample(&after, "pktito:num_requests_running") + sample(&after, "pktito:num_requests_waiting"), 0.0);
    let model = format!("model_name=\"{}\"", m.name);
    assert_eq!(sample(&after, &format!("pktito:e2e_request_latency_seconds_count{{{model}}}")), 1.0);
    assert_eq!(sample(&after, &format!("pktito:e2e_request_latency_seconds_bucket{{{model},le=\"+Inf\"}}")), 1.0);
    assert!(sample(&after, &format!("pktito:e2e_request_latency_seconds_sum{{{model}}}")) > 0.0);
}

/// An upload neither WAV nor AIFF, to a server without ffmpeg: a 400 that
/// names --ffmpeg (tests/data/esds-no-descriptor.m4a)
#[tokio::test]
async fn other_formats_need_ffmpeg() {
    let Some(m) = common::model("v2") else { return };
    let app = app(vec![m], false, None);
    let m4a = common::own("esds-no-descriptor.m4a");
    let (status, _, body) = post(&app, "/v1/audio/transcriptions", &[("file", &m4a)], None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let e = json(&body);
    assert_eq!(e["error"]["param"], "file");
    assert!(e["error"]["message"].as_str().unwrap().contains("--ffmpeg"), "{e}");
}

/// Through ffmpeg (common::ffmpeg), on one worker: test.wav as FLAC is
/// test.wav's answer byte for byte, the same samples; as AAC in an MP4
/// indexed at its end, and as Opus in WebM, it is heard; a damaged M4A
/// (tests/data/README) is a 400, and the worker hears the next request as
/// pktito-c would; a recording longer than the server takes is a 400.
#[tokio::test]
async fn other_formats_through_ffmpeg() {
    let Some(m) = common::model("v2") else { return };
    let Some(ff) = common::ffmpeg() else { return };
    let engine = common::engine_ffmpeg(vec![m.clone()], ff.clone(), 4.0 * 3600.0);
    assert_eq!(engine.workers, 1);
    let app = api::router(Server { engine, api_key: None }, 64 << 20);
    let want = common::data("test.v2.json");
    let pktito_json = |file: &[u8]| -> Vec<(String, Vec<u8>)> {
        vec![("file".into(), file.to_vec()), ("response_format".into(), b"pktito_json".to_vec())]
    };
    let send = |fields: Vec<(String, Vec<u8>)>| {
        let app = app.clone();
        async move {
            let f: Vec<(&str, &[u8])> = fields.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
            post(&app, "/v1/audio/transcriptions", &f, None).await
        }
    };
    if let Some(flac) = common::transcode(&ff, &["-c:a", "flac"], "t.flac") {
        let (status, _, body) = send(pktito_json(&flac)).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(body == want, "{}", parting(&body, &want));
    }
    for (args, name) in [(&["-c:a", "aac", "-b:a", "64k"][..], "t.m4a"), (&["-c:a", "libopus", "-b:a", "32k"][..], "t.webm")] {
        let Some(bytes) = common::transcode(&ff, args, name) else { continue };
        let (status, _, body) = send(vec![("file".into(), bytes)]).await;
        assert_eq!(status, StatusCode::OK, "{name}: {}", String::from_utf8_lossy(&body));
        assert!(json(&body)["text"].as_str().unwrap().len() > 100, "{name}: {}", String::from_utf8_lossy(&body));
    }
    let m4a = common::own("esds-no-descriptor.m4a");
    let (status, _, body) = send(vec![("file".into(), m4a)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    assert!(json(&body)["error"]["message"].as_str().unwrap().contains("ffmpeg"));
    let (status, _, body) = send(pktito_json(&common::data("test.wav"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body == want, "{}", parting(&body, &want));

    let short = common::engine_ffmpeg(vec![m], ff.clone(), 30.0);
    let app = api::router(Server { engine: short, api_key: None }, 64 << 20);
    if let Some(flac) = common::transcode(&ff, &["-c:a", "flac"], "t.flac") {
        let (status, _, body) = post(&app, "/v1/audio/transcriptions", &[("file", &flac)], None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json(&body)["error"]["message"].as_str().unwrap().contains("longer"), "{}", String::from_utf8_lossy(&body));
    }
}
