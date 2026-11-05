//! The connections, as clients that stop see them: one that sends nothing,
//! or half its headers, is closed, and so is one left idle after its
//! answer; an upload that stops is answered 408 and its slot given back;
//! and a busy server's 503 comes before the upload is sent, and reaches a
//! client that sends the whole of it before it reads.

mod common;

use pktito_server::api::{self, Server};
use pktito_server::engine::{Config, Engine};
use pktito_server::serve;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A server of its own on a port of its own, with the timeouts given
async fn start(max_requests: usize, headers: Duration, stall: Duration) -> Option<SocketAddr> {
    let m = common::model("v2")?;
    let config = Config {
        threads: 2,
        workers: 1,
        fast: false,
        max_requests,
        max_seconds: 3600.0,
        ffmpeg: None,
    };
    let app = api::router(
        Server {
            engine: Engine::new(vec![m], config),
            api_key: None,
        },
        64 << 20,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve::serve(listener, app, headers, stall));
    Some(addr)
}

/// What the server sends until it closes, and when it closed; None if it
/// is still open at the limit
async fn until_closed(c: &mut TcpStream, limit: Duration) -> Option<(Vec<u8>, Instant)> {
    let end = tokio::time::Instant::now() + limit;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout_at(end, c.read(&mut buf)).await {
            Err(_) => return None,
            Ok(Ok(0)) | Ok(Err(_)) => return Some((got, Instant::now())),
            Ok(Ok(n)) => got.extend_from_slice(&buf[..n]),
        }
    }
}

/// An answer's status code
fn status(answer: &[u8]) -> String {
    String::from_utf8_lossy(answer)
        .get(9..12)
        .unwrap_or_default()
        .to_string()
}

async fn requests(addr: SocketAddr) -> u64 {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /health HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (got, _) = until_closed(&mut c, Duration::from_secs(5))
        .await
        .expect("an answer");
    let body = &got[got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
    serde_json::from_slice::<serde_json::Value>(body).unwrap()["requests"]
        .as_u64()
        .unwrap()
}

const BOUNDARY: &str = "pktito-test-boundary";

/// A transcription's request line and headers, for a body of len bytes,
/// and the body's first bytes, its file part's head
fn upload(len: usize) -> (Vec<u8>, Vec<u8>) {
    let head = format!(
        "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: test\r\nContent-Length: {len}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\n\r\n"
    );
    let part = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n"
    );
    (head.into_bytes(), part.into_bytes())
}

/// An upload of len bytes begun: its head, and 64 KB of its file
async fn begun(addr: SocketAddr, len: usize) -> TcpStream {
    let (head, part) = upload(len);
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&head).await.unwrap();
    c.write_all(&part).await.unwrap();
    c.write_all(&[0u8; 65536]).await.unwrap();
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_says_nothing_is_closed() {
    let Some(addr) = start(4, Duration::from_secs(1), Duration::from_secs(1)).await else {
        return;
    };
    let t0 = Instant::now();
    let mut silent = TcpStream::connect(addr).await.unwrap();
    let mut halfway = TcpStream::connect(addr).await.unwrap();
    halfway
        .write_all(b"POST /v1/audio/transcriptions HTTP/1.1\r\nHost: test\r\n")
        .await
        .unwrap();
    for c in [&mut silent, &mut halfway] {
        let (_, at) = until_closed(c, Duration::from_secs(5))
            .await
            .expect("closed");
        assert!(at - t0 >= Duration::from_millis(900), "{:?}", at - t0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_idle_after_its_answer_is_closed() {
    let Some(addr) = start(4, Duration::from_secs(1), Duration::from_secs(1)).await else {
        return;
    };
    let mut c = TcpStream::connect(addr).await.unwrap();
    let t0 = Instant::now();
    c.write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let (got, at) = until_closed(&mut c, Duration::from_secs(5))
        .await
        .expect("closed");
    assert_eq!(status(&got), "200");
    assert!(at - t0 >= Duration::from_millis(900), "{:?}", at - t0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_that_stops_is_answered_and_its_slot_given_back() {
    let Some(addr) = start(4, Duration::from_secs(30), Duration::from_secs(1)).await else {
        return;
    };
    let t0 = Instant::now();
    let mut c = begun(addr, 1 << 20).await;
    let (got, at) = until_closed(&mut c, Duration::from_secs(10))
        .await
        .expect("answered and closed");
    assert_eq!(status(&got), "408", "{}", String::from_utf8_lossy(&got));
    assert!(at - t0 >= Duration::from_millis(900), "{:?}", at - t0);
    assert_eq!(requests(addr).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_server_answers_before_the_upload() {
    let Some(addr) = start(2, Duration::from_secs(30), Duration::from_secs(30)).await else {
        return;
    };
    // two uploads begun and not finished: both slots
    let held = [begun(addr, 50 << 20).await, begun(addr, 50 << 20).await];
    let t0 = Instant::now();
    while requests(addr).await < 2 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the uploads begun hold no slots"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // fifty more, each 64 KB into its 50 MB: a 503 for each, at once, the
    // rest of the 2.5 GB never sent
    let t0 = Instant::now();
    let answers = futures::future::join_all((0..50).map(|_| async move {
        let mut c = begun(addr, 50 << 20).await;
        until_closed(&mut c, Duration::from_secs(10))
            .await
            .expect("answered and closed")
            .0
    }))
    .await;
    for got in &answers {
        assert_eq!(status(got), "503", "{}", String::from_utf8_lossy(got));
    }
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    // a client that sends all of its upload before it reads, as Python's
    // http.client does: its 503 is not lost to a reset
    let (head, part) = upload(16 << 20);
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&head).await.unwrap();
    c.write_all(&part).await.unwrap();
    c.write_all(&vec![0u8; (16 << 20) - part.len()])
        .await
        .expect("the whole upload sent");
    let (got, _) = until_closed(&mut c, Duration::from_secs(10))
        .await
        .expect("answered and closed");
    assert_eq!(status(&got), "503", "{}", String::from_utf8_lossy(&got));
    drop(held);
}
