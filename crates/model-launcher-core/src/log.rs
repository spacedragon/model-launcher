use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{AppError, ModelId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    Application,
    EngineStdout,
    EngineStderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp_ms: u64,
    pub source: LogSource,
    pub level: LogLevel,
    pub generation: Option<u64>,
    pub model_id: Option<ModelId>,
    pub message: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    pub source: Option<LogSource>,
    pub minimum_level: Option<LogLevel>,
}

impl LogFilter {
    fn matches(self, record: &LogRecord) -> bool {
        self.source.is_none_or(|source| source == record.source)
            && self
                .minimum_level
                .is_none_or(|minimum| record.level >= minimum)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogStoreLimits {
    max_records: usize,
    max_message_bytes: usize,
    broadcast_capacity: usize,
}

impl LogStoreLimits {
    #[must_use]
    pub const fn new(
        max_records: usize,
        max_message_bytes: usize,
        broadcast_capacity: usize,
    ) -> Self {
        Self {
            max_records,
            max_message_bytes,
            broadcast_capacity,
        }
    }
}

#[derive(Default)]
struct StoredRecords {
    records: VecDeque<LogRecord>,
    message_bytes: usize,
}

#[derive(Clone)]
pub struct LogStore {
    limits: LogStoreLimits,
    records: Arc<Mutex<StoredRecords>>,
    sender: broadcast::Sender<LogRecord>,
    bearer: Arc<Regex>,
    total_delivery_drops: Arc<AtomicU64>,
}

impl LogStore {
    pub fn new(limits: LogStoreLimits) -> Result<Self, AppError> {
        if limits.broadcast_capacity == 0 {
            return Err(AppError::InvalidLogLimit("broadcast_capacity"));
        }
        let (sender, _) = broadcast::channel(limits.broadcast_capacity);
        Ok(Self {
            limits,
            records: Arc::new(Mutex::new(StoredRecords::default())),
            sender,
            bearer: Arc::new(
                Regex::new(r"(?i)\bbearer\s+([^\s,;]+)").expect("bearer redaction regex is valid"),
            ),
            total_delivery_drops: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn append(&self, mut record: LogRecord) {
        record.message = self.redact(&record.message);
        let record_bytes = record.message.len();
        {
            let mut stored = self.records.lock();
            if self.limits.max_records != 0 && record_bytes <= self.limits.max_message_bytes {
                stored.message_bytes += record_bytes;
                stored.records.push_back(record.clone());
                while stored.records.len() > self.limits.max_records
                    || stored.message_bytes > self.limits.max_message_bytes
                {
                    if let Some(removed) = stored.records.pop_front() {
                        stored.message_bytes -= removed.message.len();
                    }
                }
            }
        }
        if self.sender.send(record).is_err() {
            saturating_add(&self.total_delivery_drops, 1);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<LogRecord> {
        self.records.lock().records.iter().cloned().collect()
    }

    #[must_use]
    pub fn filtered_snapshot(&self, filter: LogFilter) -> Vec<LogRecord> {
        self.records
            .lock()
            .records
            .iter()
            .filter(|record| filter.matches(record))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn subscribe(&self) -> LogSubscriber {
        LogSubscriber {
            receiver: self.sender.subscribe(),
            total_delivery_drops: Arc::clone(&self.total_delivery_drops),
            local_delivery_drops: AtomicU64::new(0),
        }
    }

    /// Returns observed delivery losses. A lagged record is counted once per subscriber that
    /// observes it, while a send with no subscribers is counted once.
    #[must_use]
    pub fn total_delivery_drops(&self) -> u64 {
        self.total_delivery_drops.load(Ordering::Relaxed)
    }

    pub fn export_json_lines(&self, mut writer: impl io::Write) -> io::Result<()> {
        let snapshot = self.snapshot();
        for record in snapshot {
            serde_json::to_writer(&mut writer, &record).map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }

    fn redact(&self, message: &str) -> String {
        let message = redact_authorization_headers(message);
        self.bearer
            .replace_all(&message, "Bearer [REDACTED]")
            .into_owned()
    }
}

pub struct LogSubscriber {
    receiver: broadcast::Receiver<LogRecord>,
    total_delivery_drops: Arc<AtomicU64>,
    local_delivery_drops: AtomicU64,
}

impl LogSubscriber {
    pub fn try_recv(&mut self) -> Result<LogRecord, LogRecvError> {
        self.receiver.try_recv().map_err(|error| {
            if let broadcast::error::TryRecvError::Lagged(count) = error {
                saturating_add(&self.local_delivery_drops, count);
                saturating_add(&self.total_delivery_drops, count);
            }
            LogRecvError(error)
        })
    }

    #[must_use]
    pub fn local_delivery_drops(&self) -> u64 {
        self.local_delivery_drops.load(Ordering::Relaxed)
    }
}

fn saturating_add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

fn redact_authorization_headers(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut authorization_continuation = false;
    for (content, ending) in logical_lines(message) {
        if authorization_continuation && content.chars().next().is_some_and(char::is_whitespace) {
            let whitespace_len = leading_whitespace_bytes(content);
            output.push_str(&content[..whitespace_len]);
            output.push_str("[REDACTED]");
            output.push_str(ending);
            continue;
        }
        authorization_continuation = false;
        if let Some(colon) = authorization_colon(content) {
            output.push_str(&content[..=colon]);
            output.push_str(" [REDACTED]");
            output.push_str(ending);
            authorization_continuation = true;
        } else {
            output.push_str(content);
            output.push_str(ending);
        }
    }
    output
}

fn leading_whitespace_bytes(value: &str) -> usize {
    let mut length = 0;
    for (offset, character) in value.char_indices() {
        if !character.is_whitespace() {
            break;
        }
        length = offset + character.len_utf8();
    }
    length
}

fn logical_lines(message: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= message.len() {
            return None;
        }
        let bytes = message.as_bytes();
        let mut end = start;
        while end < bytes.len() && !matches!(bytes[end], b'\r' | b'\n') {
            end += 1;
        }
        let content = &message[start..end];
        let ending = if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
            start = end + 2;
            "\r\n"
        } else if bytes.get(end) == Some(&b'\r') {
            start = end + 1;
            "\r"
        } else if bytes.get(end) == Some(&b'\n') {
            start = end + 1;
            "\n"
        } else {
            start = end;
            ""
        };
        Some((content, ending))
    })
}

fn authorization_colon(line: &str) -> Option<usize> {
    const NAME: &[u8] = b"authorization";
    let bytes = line.as_bytes();
    if bytes.len() < NAME.len() {
        return None;
    }
    for start in 0..=bytes.len() - NAME.len() {
        if start != 0 && is_http_token_byte(bytes[start - 1]) {
            continue;
        }
        if !bytes[start..start + NAME.len()].eq_ignore_ascii_case(NAME) {
            continue;
        }
        let mut cursor = start + NAME.len();
        for (offset, character) in line[cursor..].char_indices() {
            if !character.is_whitespace() {
                break;
            }
            cursor = start + NAME.len() + offset + character.len_utf8();
        }
        if bytes.get(cursor) == Some(&b':') {
            return Some(cursor);
        }
    }
    None
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[derive(Debug)]
pub struct LogRecvError(broadcast::error::TryRecvError);

impl LogRecvError {
    #[must_use]
    pub const fn lagged(&self) -> Option<u64> {
        match self.0 {
            broadcast::error::TryRecvError::Lagged(count) => Some(count),
            _ => None,
        }
    }
}

pub struct EngineLogFramer {
    store: LogStore,
    source: LogSource,
    level: LogLevel,
    generation: Option<u64>,
    model_id: Option<ModelId>,
    max_line_bytes: usize,
    pending: Vec<u8>,
    pending_cr: bool,
    discarding_oversized: bool,
}

pub const MAX_ENGINE_LOG_LINE_BYTES: usize = 1024 * 1024;

impl EngineLogFramer {
    pub fn new(
        store: LogStore,
        source: LogSource,
        level: LogLevel,
        generation: Option<u64>,
        model_id: Option<ModelId>,
        max_line_bytes: usize,
    ) -> Result<Self, AppError> {
        if max_line_bytes == 0 || max_line_bytes > MAX_ENGINE_LOG_LINE_BYTES {
            return Err(AppError::InvalidLogLimit("max_line_bytes"));
        }
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(max_line_bytes)
            .map_err(|error| AppError::LogBufferAllocation(Box::new(error)))?;
        Ok(Self {
            store,
            source,
            level,
            generation,
            model_id,
            max_line_bytes,
            pending,
            pending_cr: false,
            discarding_oversized: false,
        })
    }

    pub fn push(&mut self, timestamp_ms: u64, bytes: &[u8]) {
        for &byte in bytes {
            if self.discarding_oversized {
                if byte == b'\n' {
                    self.discarding_oversized = false;
                }
                continue;
            }
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    self.emit(timestamp_ms, false);
                    continue;
                }
                if !self.push_content(b'\r', timestamp_ms) {
                    if byte != b'\n' {
                        self.discarding_oversized = true;
                    }
                    continue;
                }
            }
            if byte == b'\n' {
                self.emit(timestamp_ms, false);
            } else if byte == b'\r' {
                self.pending_cr = true;
            } else {
                self.push_content(byte, timestamp_ms);
            }
        }
    }

    pub fn finish(&mut self, timestamp_ms: u64) {
        if self.pending_cr {
            self.pending_cr = false;
            self.push_content(b'\r', timestamp_ms);
        }
        if !self.pending.is_empty() {
            self.emit(timestamp_ms, false);
        }
        self.discarding_oversized = false;
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn emit(&mut self, timestamp_ms: u64, truncated: bool) {
        let message = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        self.store.append(LogRecord {
            timestamp_ms,
            source: self.source,
            level: self.level,
            generation: self.generation,
            model_id: self.model_id,
            message,
            truncated,
        });
    }

    fn push_content(&mut self, byte: u8, timestamp_ms: u64) -> bool {
        if self.pending.len() < self.max_line_bytes {
            self.pending.push(byte);
            true
        } else {
            self.emit(timestamp_ms, true);
            self.discarding_oversized = true;
            false
        }
    }
}
