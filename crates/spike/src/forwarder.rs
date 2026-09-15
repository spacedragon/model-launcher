//! Spike A: transparent SSE forwarding.
//!
//! `POST /chat` forwards the request to the (fake) upstream's `POST /sse` and
//! pipes the upstream response body — including `text/event-stream` — back to
//! the client byte-for-byte.
//!
//! HTTP client: **reqwest (stream feature)** — see `docs/adr/ADR-0004-http-client.md`.
//!
//! Cancellation contract (the point of this spike):
//! client disconnect ⇒ axum drops the response body ⇒ the reqwest stream is
//! dropped ⇒ the upstream TCP connection is closed ⇒ the upstream's sender
//! task observes the dead peer and finishes ⇒ upstream `active` returns to 0
//! (no leaked connections, no half-open streams).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Forwarder {
    client: Client,
    upstream: String,
}

impl Forwarder {
    pub fn new(upstream: &str) -> Self {
        let client = Client::builder()
            // No global request timeout: streaming SSE responses are unbounded.
            // Cancellation comes from the stream being dropped, not a timer.
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(30))
            // The upstream is ALWAYS a local child process (loopback). Never
            // let inherited HTTP(S)_PROXY settings route inference traffic
            // through a proxy (breaks tests; would expose payloads).
            .no_proxy()
            .build()
            .expect("build reqwest client");
        Self {
            client,
            upstream: upstream.trim_end_matches('/').to_string(),
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/chat", post(chat))
            .with_state(Arc::new(self))
    }
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct ChatRequest {
    scenario: String,
}

async fn chat(
    State(fwd): State<Arc<Forwarder>>,
    axum::Json(req): axum::Json<ChatRequest>,
) -> Response {
    let upstream_url = format!("{}/sse", fwd.upstream);
    let upstream_resp = match fwd.client.post(&upstream_url).json(&req).send().await {
        Ok(r) => r,
        // Upstream unreachable: explicit error, do not hang.
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(
                    serde_json::json!({ "error": "upstream_unreachable", "detail": e.to_string() }),
                ),
            )
                .into_response();
        }
    };

    // Pass the upstream status through (404 for unknown scenario, etc.).
    let status = upstream_resp.status();
    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let upstream_stream = upstream_resp.bytes_stream();
    let body = axum::body::Body::from_stream(upstream_stream.map(|c| c.map_err(axum::Error::new)));
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    if let Ok(ct) = content_type.parse() {
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, ct);
    }
    resp
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_upstream::{Stats, count_data_lines, poll_until};
    use std::sync::atomic::Ordering;
    use tokio::time::timeout;

    async fn harness() -> (String, Stats) {
        let (upstream, stats, _up) = crate::fake_upstream::start().await;
        let fwd = Forwarder::new(&upstream).router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind forwarder");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, fwd).await.expect("serve forwarder");
        });
        tokio::time::sleep(Duration::from_millis(50)).await; // let servers settle
        (format!("http://{addr}"), stats)
    }

    /// Read the whole stream; panic if it does not END within 10s (hang).
    async fn drain(resp: reqwest::Response) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut stream = resp.bytes_stream();
        loop {
            match timeout(Duration::from_secs(10), stream.next()).await {
                Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
                Ok(Some(Err(e))) => panic!("upstream stream error: {e}"),
                Ok(None) => break,
                Err(_) => panic!(
                    "HANG: stream never ended within 10s ({} bytes so far)",
                    buf.len()
                ),
            }
        }
        buf
    }

    /// Test-side client: also `no_proxy()`, because it talks to loopback
    /// servers and must not be affected by inherited proxy settings.
    fn test_client() -> Client {
        Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("build test client")
    }

    async fn post_chat(base: &str, client: &Client, scenario: &str) -> reqwest::Response {
        client
            .post(format!("{base}/chat"))
            .json(&serde_json::json!({ "scenario": scenario }))
            .send()
            .await
            .expect("POST /chat")
    }

    /// ① Healthy stream is forwarded completely, in order, and byte-for-byte
    /// identically to what the upstream actually sent (no re-encoding).
    #[tokio::test]
    async fn healthy_stream_fully_forwarded() {
        let (base, stats) = harness().await;
        let client = test_client();
        let resp = post_chat(&base, &client, "healthy").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .is_some_and(|v| v.to_str().is_ok_and(|s| s.starts_with("text/event-stream"))),
            "content-type must stay text/event-stream"
        );
        let buf = drain(resp).await;
        let expected = stats.sent.lock().unwrap().clone();
        assert_eq!(
            buf, expected,
            "forwarded body must be byte-for-byte identical to the upstream bytes"
        );
        let s = String::from_utf8_lossy(&buf);
        for n in 1..=5 {
            assert!(
                s.contains(&format!("data: evt-{n}")),
                "missing evt-{n} in:\n{s}"
            );
        }
        // ordering
        let pos = |n: usize| s.find(&format!("data: evt-{n}")).unwrap_or(usize::MAX);
        assert!(pos(1) < pos(2) && pos(2) < pos(3) && pos(3) < pos(4) && pos(4) < pos(5));
    }

    /// ② Client disconnect mid-stream ⇒ upstream sender task cancelled and
    /// upstream connection released: `active` returns to 0 and event sending
    /// actually stops (not merely paused).
    ///
    /// The "long" scenario is 300 events @ 50ms = 15s — far longer than the
    /// 3s assertion window — so a forwarder that fails to propagate the
    /// cancellation could not finish naturally and pass spuriously. We
    /// therefore also assert that at cancellation time fewer than 20% of the
    /// events were actually sent.
    #[tokio::test]
    async fn disconnect_releases_upstream_no_leak() {
        let (base, stats) = harness().await;
        let client = test_client();
        let resp = post_chat(&base, &client, "long").await; // 300 events @ 50ms = 15s
        assert_eq!(resp.status(), StatusCode::OK);

        // Consume until the first event arrives, then abandon the stream.
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        loop {
            match timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(c))) => {
                    buf.extend_from_slice(&c);
                    if !buf.is_empty() && count_data_lines(&buf) >= 1 {
                        break;
                    }
                }
                Ok(Some(Err(e))) => panic!("stream error: {e}"),
                Ok(None) => panic!("upstream closed before first event"),
                Err(_) => panic!("HANG: no first event within 5s"),
            }
        }
        assert_eq!(
            stats.active.load(Ordering::SeqCst),
            1,
            "one upstream stream in flight"
        );
        // Drop the stream ⇒ client "disconnects" (resp was consumed by bytes_stream).
        drop(stream);
        drop(client);

        // The upstream must observe the closed connection and free the slot.
        let active = stats.active.clone();
        let ok = poll_until(Duration::from_secs(3), move || {
            active.load(Ordering::SeqCst) == 0
        })
        .await;
        assert!(
            ok,
            "upstream `active` did not return to 0 within 3s (leak!)"
        );

        // Prove real cancellation, not "stream happened to finish": the
        // scenario has 300 events but must have sent far fewer than 20% of
        // them by the time the upstream slot is released.
        let total_events = 300usize;
        let sent = stats.events_sent.load(Ordering::Relaxed);
        assert!(
            sent < total_events * 2 / 10,
            "upstream sent {sent}/{total_events} events before releasing the slot — \
             cancellation not proven (a non-cancelling forwarder could pass)"
        );

        // And event count stays frozen afterwards.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let a = stats.events_sent.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let b = stats.events_sent.load(Ordering::Relaxed);
        assert_eq!(a, b, "upstream kept sending after client disconnect");
        assert!(
            stats.peak.load(Ordering::SeqCst) >= 1,
            "upstream never observed the connection (test not exercising the path)"
        );
    }

    /// ③a Upstream malformed bytes (non-SSE text, NUL/0x01 control bytes,
    /// truncated UTF-8): forwarded byte-for-byte, stream still ends normally
    /// — no hang. A parse-then-rebuild implementation would mangle the raw
    /// bytes and fail the equality assertion.
    #[tokio::test]
    async fn malformed_upstream_data_no_hang() {
        let (base, stats) = harness().await;
        let client = test_client();
        let resp = post_chat(&base, &client, "malformed").await;
        let buf = drain(resp).await; // panics on hang
        let expected = stats.sent.lock().unwrap().clone();
        assert_eq!(
            buf, expected,
            "malformed bytes must be forwarded byte-for-byte (no re-encoding)"
        );
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("data: evt-1"));
        assert!(s.contains("data: evt-2"));
        assert!(
            s.contains("this line is not valid SSE"),
            "malformed text must pass through verbatim:\n{s}"
        );
        assert!(
            buf.windows(2).any(|w| w == [0x00, 0x01]),
            "NUL+0x01 bytes must be forwarded verbatim"
        );
        assert!(
            buf.windows(2).any(|w| w == [0xE2, 0x80]),
            "truncated UTF-8 must be forwarded verbatim"
        );
    }

    /// ③b Upstream 断流 (body stream ends early): forwarder surfaces the
    /// truncation and ends promptly — no hang.
    #[tokio::test]
    async fn upstream_cut_stream_truncates_no_hang() {
        let (base, stats) = harness().await;
        let client = test_client();
        let before = stats.events_sent.load(Ordering::Relaxed);
        let resp = post_chat(&base, &client, "cut").await; // 20 lines but stops after 3
        let buf = drain(resp).await; // panics on hang
        assert_eq!(
            count_data_lines(&buf),
            3,
            "expected exactly the 3 events before the cut:\n{}",
            String::from_utf8_lossy(&buf)
        );
        let after = stats.events_sent.load(Ordering::Relaxed);
        assert_eq!(after - before, 3, "upstream sent beyond stop_after");
    }

    /// ④ Upstream force-drops the TCP connection mid-body (not a clean EOF):
    /// the forwarder surfaces it as a stream error to the client, does not
    /// hang, and the upstream slot is released.
    #[tokio::test]
    async fn upstream_abort_mid_body_errors_not_hang() {
        let (base, stats) = harness().await;
        let client = test_client();
        let resp = post_chat(&base, &client, "abort").await; // aborts TCP after 10 events
        assert_eq!(resp.status(), StatusCode::OK);

        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        loop {
            match timeout(Duration::from_secs(10), stream.next()).await {
                Ok(Some(Ok(c))) => buf.extend_from_slice(&c),
                // Mid-body connection drop surfaces as a stream error. A clean
                // None would mean the implementation buffered/re-encoded the
                // stream into a well-formed end.
                Ok(Some(Err(_))) => break,
                Ok(None) => panic!(
                    "stream ended cleanly — expected a mid-body disconnect error ({} bytes)",
                    buf.len()
                ),
                Err(_) => panic!("HANG: stream never ended within 10s"),
            }
        }
        assert!(
            count_data_lines(&buf) >= 1,
            "client must have received events before the abort"
        );

        // Upstream slot released after the forced drop.
        let active = stats.active.clone();
        let ok = poll_until(Duration::from_secs(3), move || {
            active.load(Ordering::SeqCst) == 0
        })
        .await;
        assert!(
            ok,
            "upstream `active` did not return to 0 after forced abort"
        );
    }
}
