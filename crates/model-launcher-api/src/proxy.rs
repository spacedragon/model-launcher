use axum::{
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response},
};
use futures_util::{StreamExt, stream};
use serde_json::Value;
use std::collections::HashSet;

use crate::{ApiError, AppState};

pub(crate) async fn proxy(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let (parts, body) = request.into_parts();
    if parts.headers.len() > state.limits.max_headers {
        return Err(ApiError::payload(
            "too_many_headers",
            "too many request headers",
        ));
    }
    let bytes = to_bytes(body, state.limits.max_body_bytes)
        .await
        .map_err(|_| ApiError::payload("request_too_large", "request body is too large"))?;
    let parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::bad_request("invalid_request", "request body is not valid JSON"))?;
    let model_key = parsed
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model_required", "model is required"))?;
    let model = state.find_model(model_key)?.record.clone();
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
    let permit =
        state.connections.clone().try_acquire_owned().map_err(|_| {
            ApiError::unavailable("connection_limit", "too many active connections")
        })?;
    let url = format!(
        "{upstream}{}",
        parts.uri.path_and_query().map_or("/", |v| v.as_str())
    );
    let mut outgoing = state.client.request(parts.method, url).body(bytes);
    let nominated = nominated_headers(&parts.headers);
    for (name, value) in &parts.headers {
        if safe_request_header(name) && !nominated.contains(name.as_str()) {
            outgoing = outgoing.header(name, value);
        }
    }
    let response = outgoing
        .send()
        .await
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
