use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response},
};
use futures_util::{Stream, StreamExt, stream};
use serde_json::Value;
use std::collections::HashSet;

use crate::{ApiError, AppState};

pub(crate) async fn proxy(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let (parts, body) = request.into_parts();
    if parts.headers.len() > state.limits.max_headers {
        return Err(ApiError::headers(
            "header_limits_exceeded",
            "too many request headers",
        ));
    }
    let permit =
        state.connections.clone().try_acquire_owned().map_err(|_| {
            ApiError::unavailable("connection_limit", "too many active connections")
        })?;
    let chunks = bounded_spool(body.into_data_stream(), state.limits.max_body_bytes).await?;
    let parsed: Value = serde_json::from_reader(ChunkReader::new(&chunks))
        .map_err(|_| ApiError::bad_request("invalid_request", "request body is not valid JSON"))?;
    let model_key = parsed
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model_required", "model is required"))?;
    let model = state.find_model(model_key)?.record;
    let lease = tokio::time::timeout(
        state.limits.startup_timeout,
        state.lifecycle.acquire(model.clone()),
    )
    .await
    .map_err(|_| ApiError::unavailable("model_starting", "model is starting"))?
    .map_err(ApiError::core)?;
    let upstream = (state.upstream)(&model).ok_or_else(|| {
        ApiError::unavailable("engine_unavailable", "engine endpoint is unavailable")
    })?;
    validate_upstream(&upstream)?;
    let url = format!(
        "{upstream}{}",
        parts.uri.path_and_query().map_or("/", |v| v.as_str())
    );
    let request_stream = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    let mut outgoing = state
        .client
        .request(parts.method, url)
        .body(reqwest::Body::wrap_stream(request_stream));
    let nominated = nominated_headers(&parts.headers);
    for (name, value) in &parts.headers {
        if safe_request_header(name) && !nominated.contains(name.as_str()) {
            outgoing = outgoing.header(name, value);
        }
    }
    let response = tokio::time::timeout(state.limits.upstream_header_timeout, outgoing.send())
        .await
        .map_err(|_| {
            ApiError::unavailable("upstream_timeout", "upstream response headers timed out")
        })?
        .map_err(|_| ApiError::unavailable("upstream_unavailable", "upstream request failed"))?;
    let status = response.status();
    let headers = response.headers().clone();
    let upstream_stream = response.bytes_stream();
    let output = stream::unfold(
        (upstream_stream, lease, permit, false),
        |(mut source, mut lease, permit, done)| async move {
            if done {
                return None;
            }
            tokio::select! {
                item = source.next() => item.map(|item| (item.map_err(std::io::Error::other), (source, lease, permit, false))),
                () = lease.cancelled() => Some((Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "model ejected")), (source, lease, permit, true))),
            }
        },
    );
    let mut result = Response::builder()
        .status(status)
        .body(Body::from_stream(output))
        .map_err(|_| ApiError::unavailable("proxy_error", "failed to construct response"))?;
    copy_response_headers(&headers, result.headers_mut());
    Ok(result)
}

fn validate_upstream(value: &str) -> Result<(), ApiError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| ApiError::unavailable("invalid_upstream", "engine endpoint is invalid"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "http"
        || !loopback
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::unavailable(
            "invalid_upstream",
            "engine endpoint is invalid",
        ));
    }
    Ok(())
}

pub(crate) async fn bounded_spool<S, E>(
    mut incoming: S,
    limit: usize,
) -> Result<Vec<bytes::Bytes>, ApiError>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
{
    let mut chunks = Vec::new();
    let mut total = 0_usize;
    while let Some(chunk) = incoming.next().await {
        let chunk = chunk
            .map_err(|_| ApiError::bad_request("invalid_request", "request body stream failed"))?;
        total = total
            .checked_add(chunk.len())
            .ok_or_else(|| ApiError::payload("request_too_large", "request body is too large"))?;
        if total > limit {
            return Err(ApiError::payload(
                "request_too_large",
                "request body is too large",
            ));
        }
        chunks.push(chunk);
    }
    Ok(chunks)
}

pub(crate) struct ChunkReader<'a> {
    chunks: &'a [bytes::Bytes],
    index: usize,
    offset: usize,
}
impl<'a> ChunkReader<'a> {
    pub(crate) fn new(chunks: &'a [bytes::Bytes]) -> Self {
        Self {
            chunks,
            index: 0,
            offset: 0,
        }
    }
}
impl std::io::Read for ChunkReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        while self.index < self.chunks.len() {
            let remaining = &self.chunks[self.index][self.offset..];
            if remaining.is_empty() {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let count = remaining.len().min(output.len());
            output[..count].copy_from_slice(&remaining[..count]);
            self.offset += count;
            return Ok(count);
        }
        Ok(0)
    }
}

fn safe_request_header(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "upgrade"
            | "te"
            | "trailer"
    )
}
fn copy_response_headers(source: &HeaderMap, target: &mut HeaderMap<HeaderValue>) {
    let nominated = nominated_headers(source);
    for (name, value) in source {
        if !matches!(
            name.as_str(),
            "connection"
                | "keep-alive"
                | "transfer-encoding"
                | "upgrade"
                | "te"
                | "trailer"
                | "content-length"
        ) && !nominated.contains(name.as_str())
        {
            target.append(name, value.clone());
        }
    }
}

fn nominated_headers(headers: &HeaderMap) -> HashSet<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ChunkReader, bounded_spool};
    use bytes::Bytes;
    use futures_util::stream;
    use serde_json::Value;

    #[tokio::test]
    async fn bounded_spool_preserves_frames_and_chunk_reader_joins_json() {
        let frames = vec![
            Bytes::from_static(b"{\"model\":\""),
            Bytes::from_static(b"test\",\"prompt\":\""),
            Bytes::from_static(b"hello\"}"),
        ];
        let expected_frames = frames.clone();
        let pointers: Vec<_> = frames.iter().map(|frame| frame.as_ptr()).collect();

        let spool = bounded_spool(
            stream::iter(frames.into_iter().map(Ok::<_, std::io::Error>)),
            1024,
        )
        .await
        .unwrap();

        assert_eq!(spool, expected_frames);
        assert_eq!(
            spool.iter().map(|frame| frame.as_ptr()).collect::<Vec<_>>(),
            pointers
        );
        let parsed: Value = serde_json::from_reader(ChunkReader::new(&spool)).unwrap();
        assert_eq!(parsed["model"], "test");
        assert_eq!(parsed["prompt"], "hello");
    }
}
