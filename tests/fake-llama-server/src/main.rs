use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderValue, StatusCode},
    response::Response,
    routing::post,
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:0".into());
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("{}", listener.local_addr()?);
    axum::serve(listener, Router::new().route("/{*path}", post(echo))).await
}

async fn echo(request: Request) -> Response<Body> {
    let status = request
        .headers()
        .get("x-fake-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(StatusCode::OK);
    let content_type = request
        .headers()
        .get("x-fake-content-type")
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    let sse = request.headers().contains_key("x-fake-sse");
    let body = if sse {
        Body::from_stream(as_one_chunk(request.into_body()))
    } else {
        request.into_body()
    };
    let mut response = Response::builder()
        .status(status)
        .body(body)
        .expect("valid fake response");
    response.headers_mut().insert("content-type", content_type);
    response
}

fn as_one_chunk(body: Body) -> impl futures_core::Stream<Item = Result<Bytes, std::io::Error>> {
    use http_body_util::BodyExt as _;
    futures_util::stream::once(async move {
        body.collect()
            .await
            .map(|body| body.to_bytes())
            .map_err(std::io::Error::other)
    })
}
