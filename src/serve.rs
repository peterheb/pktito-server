//! The connections: hyper's HTTP/1, a task each, as axum::serve makes them,
//! with what a server that must not fill up with clients that stop needs
//! and axum::serve leaves out - a clock, without which hyper keeps no
//! timeout on a request's headers; a timeout on its body; and a close in
//! stages, so that an answer given before an upload was read reaches the
//! client.

use axum::body::Body;
use axum::Router;
use hyper::body::{Bytes, Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::future::Future;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Sleep;
use tower::ServiceExt;

/// How long a client may take over a request's line and headers, from when
/// it connects or from its last answer: a connection idle this long closes
pub const HEADERS: Duration = Duration::from_secs(30);
/// How long a request's body may go without a byte
pub const STALL: Duration = Duration::from_secs(30);
/// The longest a closing connection reads what its client still sends
const LINGER: Duration = Duration::from_secs(30);

/// app on every connection the listener takes, until the process ends;
/// headers and stall are HEADERS and STALL but in the tests
pub async fn serve(listener: TcpListener, app: Router, headers: Duration, stall: Duration) {
    loop {
        let tcp = match listener.accept().await {
            Ok((tcp, _)) => tcp,
            // a connection gone before it was taken: the next
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionReset
                ) =>
            {
                continue
            }
            // out of descriptors, most likely: as axum::serve does, a
            // second for some to close rather than a loop of failures
            Err(e) => {
                tracing::error!("accept: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                app.clone().oneshot(req.map(|body| {
                    Body::new(Idle {
                        body,
                        after: stall,
                        sleep: None,
                    })
                }))
            });
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(headers)
                .serve_connection(TokioIo::new(tcp), service)
                .without_shutdown();
            // an error - headers too slow, a reset - ends the connection there
            if let Ok(parts) = conn.await {
                linger(parts.io.into_inner()).await;
            }
        });
    }
}

/// A request's body, which errs with Stalled when no byte of it comes for
/// a while: an upload that stops - its client gone without closing, or a
/// Content-Length longer than what it sends - gives its slot back
struct Idle {
    body: Incoming,
    after: Duration,
    /// Running while the body waits for its next bytes
    sleep: Option<Pin<Box<Sleep>>>,
}

/// No byte of a request's body for the time it holds
#[derive(Debug)]
pub struct Stalled(pub Duration);

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the upload stopped: no bytes of it for {} s",
            self.0.as_secs()
        )
    }
}

impl std::error::Error for Stalled {}

impl hyper::body::Body for Idle {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let me = self.get_mut();
        if let Poll::Ready(frame) = Pin::new(&mut me.body).poll_frame(cx) {
            me.sleep = None; // bytes, or the end: the wait starts again
            return Poll::Ready(frame.map(|f| f.map_err(Into::into)));
        }
        let after = me.after;
        let sleep = me
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(after)));
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Some(Err(Box::new(Stalled(after))))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// A connection's close in stages (RFC 9112, 9.6): its sending side shut,
/// then what its client still sends read and dropped, for LINGER at most.
/// A close with bytes unread sends a reset, which can lose the answer
/// before the client reads it - a 503 or a 413, given before the upload it
/// answers was read.
async fn linger(mut tcp: TcpStream) {
    if tcp.shutdown().await.is_err() {
        return;
    }
    let mut sink = [0u8; 16384];
    let _ = tokio::time::timeout(LINGER, async {
        while let Ok(n) = tcp.read(&mut sink).await {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}
