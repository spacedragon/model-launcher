use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    response::Response,
    routing::{get, post},
};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Notify, oneshot};

#[derive(Default)]
pub struct Control {
    requests: AtomicUsize,
    stream_drops: AtomicUsize,
    gate_entered: Notify,
    gate_release: Notify,
    request_bytes: std::sync::Mutex<Vec<u8>>,
}
impl Control {
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
    pub fn stream_drops(&self) -> usize {
        self.stream_drops.load(Ordering::SeqCst)
    }
    pub async fn gate_entered(&self) {
        self.gate_entered.notified().await;
    }
    pub fn release_gate(&self) {
        self.gate_release.notify_waiters();
    }
    pub fn request_bytes(&self) -> Vec<u8> {
        self.request_bytes.lock().unwrap().clone()
    }
}

pub struct FakeServer {
    address: SocketAddr,
    pub control: Arc<Control>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl FakeServer {
    pub async fn spawn() -> std::io::Result<Self> {
        let control = Arc::new(Control::default());
        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/{*path}", post(handle))
            .with_state(control.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Ok(Self {
            address,
            control,
            stop: Some(stop),
            task,
        })
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
    pub async fn stop(mut self) -> std::io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.await.map_err(std::io::Error::other)?
    }
}

struct DropCount(Arc<Control>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.stream_drops.fetch_add(1, Ordering::SeqCst);
    }
}

async fn handle(State(control): State<Arc<Control>>, request: Request) -> Response<Body> {
    control.requests.fetch_add(1, Ordering::SeqCst);
    let mode = request
        .headers()
        .get("x-fake-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("echo")
        .to_owned();
    if mode == "gate" {
        control.gate_entered.notify_one();
        control.gate_release.notified().await;
    }
    if mode == "error" {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("upstream error"))
            .unwrap();
    }
    if mode == "sse" || mode == "sse-multi" {
        let chunks = if mode == "sse-multi" {
            vec![
                Bytes::from_static(b"data: a\n\n"),
                Bytes::from_static(b"data: \xff\x00\n"),
                Bytes::from_static(b"\n"),
            ]
        } else {
            vec![
                Bytes::from_static(b"data: a"),
                Bytes::from_static(b"\xff\x00\n"),
                Bytes::from_static(b"\n"),
            ]
        };
        let guard = DropCount(control);
        let stream = futures_util::stream::unfold(
            (chunks.into_iter(), Some(guard)),
            |(mut chunks, guard)| async move {
                chunks
                    .next()
                    .map(|chunk| (Ok::<_, std::io::Error>(chunk), (chunks, guard)))
            },
        );
        let mut response = Response::new(Body::from_stream(stream));
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        return response;
    }
    let (parts, body) = request.into_parts();
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        if let Ok(chunk) = chunk {
            bytes.extend_from_slice(&chunk);
        }
    }
    *control.request_bytes.lock().unwrap() = bytes.clone();
    let bytes = Bytes::from(bytes);
    let mut response = Response::new(Body::from(bytes));
    if let Some(value) = parts.headers.get("x-safe") {
        response.headers_mut().insert("x-seen-safe", value.clone());
    }
    response
}
