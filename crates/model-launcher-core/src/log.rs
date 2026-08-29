use std::{collections::VecDeque, io, sync::Arc};

use parking_lot::Mutex;
use regex::{Captures, Regex};
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
    authorization: Arc<Regex>,
    bearer: Arc<Regex>,
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
            authorization: Arc::new(
                Regex::new(r"(?im)(authorization\s*:\s*)([^\r\n]*)")
                    .expect("authorization redaction regex is valid"),
            ),
            bearer: Arc::new(
                Regex::new(r"(?i)\bbearer\s+([^\s,;]+)").expect("bearer redaction regex is valid"),
            ),
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
        let _ = self.sender.send(record);
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
        LogSubscriber(self.sender.subscribe())
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
        let message = self
            .authorization
            .replace_all(message, |captures: &Captures<'_>| {
                let prefix = captures
                    .get(1)
                    .map_or("Authorization: ", |value| value.as_str());
                let credential = captures.get(2).map_or("", |value| value.as_str());
                if credential
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("bearer")
                {
                    format!("{prefix}Bearer [REDACTED]")
                } else {
                    format!("{prefix}[REDACTED]")
                }
            });
        self.bearer
            .replace_all(&message, "Bearer [REDACTED]")
            .into_owned()
    }
}

pub struct LogSubscriber(broadcast::Receiver<LogRecord>);

impl LogSubscriber {
    pub fn try_recv(&mut self) -> Result<LogRecord, LogRecvError> {
        self.0.try_recv().map_err(LogRecvError)
    }
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

impl EngineLogFramer {
    #[must_use]
    pub fn new(
        store: LogStore,
        source: LogSource,
        level: LogLevel,
        generation: Option<u64>,
        model_id: Option<ModelId>,
        max_line_bytes: usize,
    ) -> Self {
        Self {
            store,
            source,
            level,
            generation,
            model_id,
            max_line_bytes,
            pending: Vec::with_capacity(max_line_bytes),
            pending_cr: false,
            discarding_oversized: false,
        }
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
