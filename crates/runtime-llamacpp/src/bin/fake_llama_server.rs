//! Test-support fake `llama-server` for `tests/lifecycle.rs`.
//!
//! This binary is **not** part of the product. The integration tests build it
//! as `CARGO_BIN_EXE_fake_llama_server` and use it as a controllable child so
//! the [`LlamaCppLifecycle`] orchestrator is exercised against a real process
//! on every platform, without a real GPU or GGUF model.
//!
//! Behaviour is controlled by the `FAKE_LLAMA_MODE` environment variable:
//!
//! - `healthy`: Immediately ready. `/health` → 200 `{"status":"ok"}`,
//!   `/v1/models` → model list, `/v1/chat/completions` → minimal response.
//! - `crash_early`: Prints an error to stderr and exits with code 1.
//! - `invalid_model`: Prints an invalid model error to stderr and exits with code 1.
//! - `wrong_model`: Healthy, but `/v1/models` reports a different model id.
//! - `never_ready`: Binds the port but `/health` always returns 503.
//! - `slow_start`: Waits `FAKE_LLAMA_DELAY_MS` milliseconds before becoming
//!   healthy.
//! - `inference_malformed_json`: Healthy and identity verified, but
//!   `/v1/chat/completions` returns malformed JSON.
//! - `inference_empty_choices`: Healthy and identity verified, but
//!   `/v1/chat/completions` returns empty choices array (`[]`).
//! - `inference_empty_content`: Healthy and identity verified, but
//!   `/v1/chat/completions` returns empty completion content (`content: ""`).
//!
//! The fake parses `--host`, `--port`, `--alias`, and `--model` from argv
//! (matching the real `llama-server` interface). It uses only `std` for the
//! HTTP server (no external dependencies).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() {
    let mode = std::env::var("FAKE_LLAMA_MODE").unwrap_or_else(|_| "healthy".to_owned());
    let args: Vec<String> = std::env::args().collect();
    let host = find_arg(&args, "--host").unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = find_arg(&args, "--port").unwrap_or_else(|| "8080".to_owned());
    let alias = find_arg(&args, "--alias").unwrap_or_else(|| "test-model".to_owned());

    match mode.as_str() {
        "crash_early" => {
            eprintln!("fake_llama_server: simulated startup crash");
            std::process::exit(1);
        }
        "invalid_model" => {
            eprintln!(
                "fake_llama_server: failed to load model: invalid GGUF format or missing file"
            );
            std::process::exit(1);
        }
        "healthy"
        | "wrong_model"
        | "never_ready"
        | "slow_start"
        | "inference_malformed_json"
        | "inference_empty_choices"
        | "inference_empty_content" => {
            let ready = Arc::new(AtomicBool::new(
                mode == "healthy" || mode == "wrong_model" || mode.starts_with("inference_"),
            ));
            let never_ready = mode == "never_ready";
            let model_key = if mode == "wrong_model" {
                "wrong-model-id".to_owned()
            } else {
                alias.clone()
            };

            // Handle slow_start: become ready after a delay
            if mode == "slow_start" {
                let delay_ms: u64 = std::env::var("FAKE_LLAMA_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(500);
                let ready_clone = Arc::clone(&ready);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    ready_clone.store(true, Ordering::Release);
                });
            }

            let addr = format!("{host}:{port}");
            let listener = match TcpListener::bind(&addr) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("fake_llama_server: cannot bind {addr}: {error}");
                    std::process::exit(1);
                }
            };

            // Signal readiness on stdout (tests may watch for this)
            let _ = writeln!(std::io::stdout(), "fake_llama_server: listening on {addr}");
            let _ = std::io::stdout().flush();

            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                handle_request(stream, mode.as_str(), &model_key, never_ready, &ready);
            }
        }
        other => {
            eprintln!("fake_llama_server: unknown mode: {other}");
            std::process::exit(5);
        }
    }
}

fn handle_request(
    mut stream: std::net::TcpStream,
    mode: &str,
    model_key: &str,
    never_ready: bool,
    ready: &AtomicBool,
) {
    let cloned = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("fake_llama_server: failed to clone stream: {err}");
            return;
        }
    };
    let mut reader = BufReader::new(cloned);

    // Read the request line
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    // Read headers (consume until empty line)
    let mut content_length: usize = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some(len) = parse_content_length(&header) {
            content_length = len;
        }
    }

    // Read body if present
    let mut body_buf = vec![0u8; content_length];
    if content_length > 0 {
        let _ = std::io::Read::read_exact(&mut reader, &mut body_buf);
    }

    let is_ready = !never_ready && ready.load(Ordering::Acquire);

    if request_line.starts_with("GET /health") {
        if is_ready {
            respond(&mut stream, 200, r#"{"status":"ok"}"#);
        } else {
            respond(&mut stream, 503, r#"{"status":"loading"}"#);
        }
    } else if request_line.starts_with("GET /v1/models") {
        let body = format!(
            r#"{{"data":[{{"id":"{model_key}","object":"model","owned_by":"llama.cpp"}}],"object":"list"}}"#
        );
        respond(&mut stream, 200, &body);
    } else if request_line.starts_with("POST /v1/chat/completions") {
        match mode {
            "inference_malformed_json" => {
                respond(&mut stream, 200, "not valid json {");
            }
            "inference_empty_choices" => {
                let body = r#"{"id":"chatcmpl-fake","object":"chat.completion","choices":[],"model":"test"}"#;
                respond(&mut stream, 200, body);
            }
            "inference_empty_content" => {
                let body = r#"{"id":"chatcmpl-fake","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"model":"test"}"#;
                respond(&mut stream, 200, body);
            }
            _ => {
                let body = r#"{"id":"chatcmpl-fake","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"model":"test"}"#;
                respond(&mut stream, 200, body);
            }
        }
    } else {
        respond(&mut stream, 404, r#"{"error":"not found"}"#);
    }
}

fn find_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn respond(stream: &mut std::net::TcpStream, status: u16, body: &str) {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn parse_content_length(header_line: &str) -> Option<usize> {
    let (name, value) = header_line.split_once(':')?;
    if name.trim().eq_ignore_ascii_case("content-length") {
        value.trim().parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_length_standard() {
        assert_eq!(parse_content_length("Content-Length: 42"), Some(42));
    }

    #[test]
    fn parse_content_length_case_insensitive() {
        assert_eq!(parse_content_length("content-length: 42"), Some(42));
        assert_eq!(parse_content_length("CONTENT-LENGTH: 42"), Some(42));
        assert_eq!(parse_content_length("CoNtEnT-lEnGtH: 42"), Some(42));
    }

    #[test]
    fn parse_content_length_whitespace_around_colon_and_value() {
        assert_eq!(parse_content_length("Content-Length : 42"), Some(42));
        assert_eq!(
            parse_content_length("Content-Length:    42   \r\n"),
            Some(42)
        );
        assert_eq!(parse_content_length("Content-Length   :   42\n"), Some(42));
        assert_eq!(parse_content_length("Content-Length:0"), Some(0));
    }

    #[test]
    fn parse_content_length_other_headers_or_invalid() {
        assert_eq!(parse_content_length("Content-Type: application/json"), None);
        assert_eq!(parse_content_length("Host: 127.0.0.1"), None);
        assert_eq!(parse_content_length("invalid header without colon"), None);
        assert_eq!(parse_content_length("Content-Length: not_a_number"), None);
        assert_eq!(parse_content_length("Content-Length: -5"), None);
    }
}
