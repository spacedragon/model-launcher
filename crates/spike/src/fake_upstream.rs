//! Fake SSE upstream built directly on hyper.
//!
//! Why raw hyper (not axum) here: the spike's core assertion is that a client
//! disconnect cancels the upstream streaming task and releases the upstream
//! connection. We track that with per-sender-task counters, and hyper's
//! `serve_connection` gives us per-connection ownership of the stream
//! lifecycle (client TCP close ⇒ hyper drops the in-flight response body ⇒
//! our mpsc receiver drops ⇒ the sender task's next `tx.send` fails ⇒ task
//! ends ⇒ `active` decrements).
//!
//! Scenario line syntax (one SSE event per line, sent every 50ms by default):
//! - `evt-1`    → sent as `data: evt-1\r\n\r\n`
//! - `RAW:<text>` → sent verbatim + CRLF framing (malformed, non-SSE text)
//! - `RAWHEX:<hex>` → bytes decoded from hex, sent verbatim (arbitrary bytes:
//!   NUL/control chars, truncated UTF-8 — proves byte-transparent forwarding,
//!   not parse-then-rebuild)
//! - `Script::stop_after` → upstream closes the body stream early (断流, clean EOF)
//! - `Script::abort_after` → upstream force-drops the TCP connection mid-body
//!   (simulated client-side abrupt disconnect)

use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::Stream;
use futures::stream::StreamExt;

use http_body::Frame;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::tokio::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant as TokioInstant, sleep, sleep_until};
use tokio_stream::wrappers::ReceiverStream;

/// Counters the fake upstream exposes to tests.
///
/// `active` counts live sender (streaming) tasks — i.e. the upstream-side view
/// of concurrent in-flight SSE connections. A client disconnect must drive
/// this back to 0 (no leaked connections).
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub active: Arc<AtomicUsize>,
    pub peak: Arc<AtomicUsize>,
    pub finished: Arc<AtomicUsize>,
    pub events_sent: Arc<AtomicUsize>,
    /// Exact bytes every sender task handed to the wire (including RAW/RAWHEX
    /// payloads), in send order. Tests assert byte-for-byte equality between
    /// this and what the forwarder's client received.
    pub sent: Arc<Mutex<Vec<u8>>>,
}

/// A scripted scenario: one SSE event per line.
#[derive(Debug, Clone)]
pub struct Script {
    pub name: &'static str,
    pub lines: Vec<String>,
    pub interval: Duration,
    /// If set, the body stream is closed after this many events (upstream 断流).
    pub stop_after: Option<usize>,
    /// If set, the upstream force-drops the TCP connection after this many
    /// events have been sent (mid-body disconnect, not a clean EOF).
    pub abort_after: Option<usize>,
}

fn ev(n: usize) -> String {
    format!("evt-{n}")
}

fn hex_to_bytes(h: &str) -> Option<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(h.len() / 2);
    for i in (0..h.len()).step_by(2) {
        out.push(u8::from_str_radix(&h[i..i + 2], 16).ok()?);
    }
    Some(out)
}

/// Default scenario registry used by tests.
pub fn default_scripts() -> Vec<Arc<Script>> {
    vec![
        Arc::new(Script {
            name: "healthy",
            lines: (1..=5).map(ev).collect(),
            interval: Duration::from_millis(50),
            stop_after: None,
            abort_after: None,
        }),
        Arc::new(Script {
            name: "malformed",
            lines: vec![
                ev(1),
                // Non-SSE text injected verbatim.
                "RAW:: this line is not valid SSE ###\r\n### trailing garbage".to_string(),
                // NUL + 0x01 control bytes — no text-level codec survives these.
                "RAWHEX:0001".to_string(),
                // Truncated UTF-8: 0xE2 0x80 expects a third byte.
                "RAWHEX:e280".to_string(),
                ev(2),
            ],
            interval: Duration::from_millis(50),
            stop_after: None,
            abort_after: None,
        }),
        Arc::new(Script {
            // 300 events @ 50ms = 15s — far longer than any test assertion
            // window, so a non-cancelling forwarder cannot finish naturally
            // within the window.
            name: "long",
            lines: (1..=300).map(ev).collect(),
            interval: Duration::from_millis(50),
            stop_after: None,
            abort_after: None,
        }),
        Arc::new(Script {
            name: "cut",
            lines: (1..=20).map(ev).collect(),
            interval: Duration::from_millis(50),
            stop_after: Some(3),
            abort_after: None,
        }),
        Arc::new(Script {
            name: "abort",
            lines: (1..=100).map(ev).collect(),
            interval: Duration::from_millis(50),
            stop_after: None,
            abort_after: Some(10),
        }),
    ]
}

#[derive(Clone)]
struct State {
    scripts: Vec<Arc<Script>>,
    stats: Stats,
    /// One-shot force-close handle for the connection currently being served
    /// (last-writer-wins; tests run one request at a time per harness).
    conn_abort: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

/// Start the fake upstream on `127.0.0.1:0`. Returns (base URL, stats, server task).
pub async fn start() -> (String, Stats, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let state = State {
        scripts: default_scripts(),
        stats: Stats::default(),
        conn_abort: Arc::new(Mutex::new(None)),
    };
    let stats = state.stats.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (io, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let state = state.clone();
            let (abort_tx, abort_rx) = oneshot::channel::<()>();
            *state.conn_abort.lock().unwrap() = Some(abort_tx);
            let conn = http1::Builder::new().keep_alive(true).serve_connection(
                TokioIo::new(io),
                service_fn(move |req| handle_req(req, state.clone())),
            );
            // `tokio::pin!` on a moved-in future needs a nested async block
            // (rustc E0716 workaround).
            tokio::spawn(async move {
                let _ = async move {
                    tokio::pin!(conn);
                    tokio::select! {
                        r = &mut conn => {
                            if let Err(e) = r {
                                tracing::trace!(%e, "fake upstream connection ended");
                            }
                        }
                        _ = abort_rx => {
                            // Force-drop `conn` on exit: closing the socket
                            // mid response makes the client see a mid-body
                            // disconnect (not a clean EOF).
                            tracing::debug!("fake upstream: abort requested, force-closing connection");
                        }
                    }
                }
                .await;
            });
        }
    });
    (format!("http://{addr}"), stats, handle)
}

async fn handle_req(
    req: Request<Incoming>,
    state: State,
) -> Result<Response<BoxBody<Bytes, std::convert::Infallible>>, std::convert::Infallible> {
    if req.uri().path() != "/sse" || req.method() != hyper::Method::POST {
        let mut resp = Response::new(BoxBody::new(Full::default()));
        *resp.status_mut() = StatusCode::NOT_FOUND;
        return Ok(resp);
    }

    let raw = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        // Client went away while sending the body — nothing to stream.
        Err(_) => return Ok(Response::new(BoxBody::new(Full::default()))),
    };
    let name: String = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("scenario").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_default();
    let script = match state.scripts.iter().find(|s| s.name == name) {
        Some(s) => s.clone(),
        None => {
            let body = BoxBody::new(Full::new(Bytes::from(format!("unknown scenario {name:?}"))));
            let mut resp = Response::new(body);
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return Ok(resp);
        }
    };

    let (tx, rx) = mpsc::channel::<Bytes>(16);
    // In `abort_after` scenarios the body must never complete on its own (no
    // terminating chunk): the only way the response can end is the TCP
    // connection being force-closed mid-body. Without this, a race exists
    // where the sender task drops the channel first, hyper writes the final
    // chunk, and the client sees a clean EOF instead of a disconnect error.
    let hanging = script.abort_after.is_some();
    let sender_state = state.clone();
    tokio::spawn(run_sender(script, tx, sender_state));

    let stream = ReceiverStream::new(rx);
    let stream: Pin<
        Box<dyn Stream<Item = Result<Frame<Bytes>, std::convert::Infallible>> + Send + Sync>,
    > = if hanging {
        Box::pin(HangingStream(stream).map(|b| Ok::<_, std::convert::Infallible>(Frame::data(b))))
    } else {
        Box::pin(stream.map(|b| Ok::<_, std::convert::Infallible>(Frame::data(b))))
    };
    let body = BoxBody::new(StreamBody::new(stream));
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    Ok(resp)
}

/// Sends the scripted events. The task itself is the "upstream connection slot":
/// it holds `active` while streaming and releases it on drop (completion,
/// upstream cut, or client disconnect causing the next `send` to fail).
async fn run_sender(script: Arc<Script>, tx: mpsc::Sender<Bytes>, state: State) {
    let stats = state.stats.clone();
    let now_active = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
    let mut peak = 0usize;
    loop {
        match stats.peak.compare_exchange_weak(
            peak,
            now_active.max(peak),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(p) => peak = p,
        }
    }
    let _release = ReleaseOnDrop(&stats);
    for (i, line) in script.lines.iter().enumerate() {
        if let Some(stop) = script.stop_after
            && i >= stop
        {
            break;
        }
        let payload: Bytes = if let Some(rest) = line.strip_prefix("RAWHEX:") {
            let mut raw = hex_to_bytes(rest).unwrap_or_default();
            raw.extend_from_slice(b"\r\n\r\n");
            Bytes::from(raw)
        } else if let Some(rest) = line.strip_prefix("RAW:") {
            Bytes::from(format!("{rest}\r\n\r\n"))
        } else {
            Bytes::from(format!("data: {line}\r\n\r\n"))
        };
        {
            let mut sent = stats.sent.lock().unwrap();
            sent.extend_from_slice(&payload);
        }
        match tx.send(payload).await {
            Ok(()) => {
                stats.events_sent.fetch_add(1, Ordering::Relaxed);
                // Simulated abrupt upstream disconnect: force-drop the TCP
                // connection mid-body, then stop sending.
                if let Some(a) = script.abort_after
                    && i + 1 == a
                {
                    if let Some(abort_tx) = state.conn_abort.lock().unwrap().take() {
                        let _ = abort_tx.send(());
                    }
                    return;
                }
            }
            // Receiver dropped: downstream went away (client disconnect) or the
            // body stream ended. Stop sending.
            Err(_) => return,
        }
        let deadline = TokioInstant::now() + script.interval;
        loop {
            sleep(Duration::from_millis(25)).await;
            if TokioInstant::now() >= deadline {
                break;
            }
        }
    }
}

struct ReleaseOnDrop<'a>(&'a Stats);

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        let stats = self.0;
        stats.active.fetch_sub(1, Ordering::SeqCst);
        stats.finished.fetch_add(1, Ordering::SeqCst);
    }
}

/// Receiver stream that hangs forever once the sender side is gone, instead
/// of terminating. Used for `abort_after` scenarios so the response body can
/// never end cleanly — the connection abort is the only way it can stop.
struct HangingStream(ReceiverStream<Bytes>);

impl Stream for HangingStream {
    type Item = Bytes;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.0.poll_next_unpin(cx) {
            // Sender dropped: hang (Pending forever) rather than signaling
            // end-of-body.
            Poll::Ready(None) => Poll::Pending,
            other => other,
        }
    }
}

/// Poll `f` every 20ms until it returns true or `timeout` elapses.
pub async fn poll_until<FnT: Fn() -> bool + Send + Sync + 'static>(
    timeout: Duration,
    f: FnT,
) -> bool {
    let deadline = sleep_until(TokioInstant::now() + timeout);
    tokio::pin!(deadline);
    loop {
        if f() {
            return true;
        }
        tokio::select! {
            _ = &mut deadline => return false,
            _ = sleep(Duration::from_millis(20)) => {}
        }
    }
}

/// Small helper: count `data:` lines in a raw SSE byte buffer.
pub fn count_data_lines(buf: &[u8]) -> usize {
    let s = String::from_utf8_lossy(buf);
    s.lines().filter(|l| l.starts_with("data:")).count()
}
