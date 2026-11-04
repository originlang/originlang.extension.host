//! Bounded in-memory log plus best-effort file mirror.
//!
//! Extension output is untrusted: every line is truncated and the buffer is a
//! ring, so a chatty or malicious extension cannot exhaust host memory. File
//! writes are best-effort by design — a full disk must not fail a call.

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum characters kept from a single message.
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// One log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    /// Unix milliseconds.
    pub at_ms: u64,
    /// `trace` | `debug` | `info` | `warn` | `error`.
    pub level: String,
    /// Attributed extension, absent for host-level lines.
    pub extension: Option<String>,
    /// Message text.
    pub message: String,
}

/// Ring buffer with an optional file mirror.
pub struct LogBuffer {
    capacity: usize,
    entries: VecDeque<LogRecord>,
    file: Option<Mutex<File>>,
}

impl LogBuffer {
    /// Creates a buffer retaining at most `capacity` records.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
            file: None,
        }
    }

    /// Creates a buffer that also appends to `path` (created on demand).
    pub fn with_file(capacity: usize, path: &Path) -> Self {
        let file = path.parent().and_then(|parent| {
            fs::create_dir_all(parent).ok()?;
            OpenOptions::new().create(true).append(true).open(path).ok()
        });
        Self {
            file: file.map(Mutex::new),
            ..Self::new(capacity)
        }
    }

    /// Normalizes a level string to the supported set.
    pub fn normalize_level(level: &str) -> &'static str {
        match level.to_ascii_lowercase().as_str() {
            "trace" => "trace",
            "debug" => "debug",
            "warn" | "warning" => "warn",
            "error" => "error",
            _ => "info",
        }
    }

    /// Appends one record, truncating the message and evicting the oldest entry.
    pub fn record(&mut self, level: &str, extension: Option<&str>, message: &str) {
        let record = LogRecord {
            at_ms: now_ms(),
            level: Self::normalize_level(level).to_owned(),
            extension: extension.map(str::to_owned),
            message: truncate(message),
        };
        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(
                file,
                "{} {:<5} {} {}",
                record.at_ms,
                record.level,
                record.extension.as_deref().unwrap_or("-"),
                record.message
            );
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(record);
    }

    /// Most recent records, optionally filtered to one extension.
    pub fn snapshot(&self, extension: Option<&str>, limit: usize) -> Vec<LogRecord> {
        let limit = limit.clamp(1, self.capacity);
        let mut records: Vec<LogRecord> = self
            .entries
            .iter()
            .filter(|record| match extension {
                Some(id) => record.extension.as_deref() == Some(id),
                None => true,
            })
            .cloned()
            .collect();
        if records.len() > limit {
            records.drain(..records.len() - limit);
        }
        records
    }
}

fn truncate(message: &str) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        return message.to_owned();
    }
    let mut truncated: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
    truncated.push('…');
    truncated
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_is_a_ring_and_truncates_messages() {
        let mut buffer = LogBuffer::new(3);
        for index in 0..5 {
            buffer.record("info", Some("ext"), &format!("line {index}"));
        }
        let records = buffer.snapshot(None, 10);
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].message, "line 4");

        let long = "x".repeat(MAX_MESSAGE_CHARS + 100);
        buffer.record("error", Some("ext"), &long);
        let last = buffer.snapshot(None, 1).pop().unwrap();
        assert_eq!(last.message.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert_eq!(last.level, "error");
    }

    #[test]
    fn unknown_levels_fall_back_to_info_and_filters_apply() {
        assert_eq!(LogBuffer::normalize_level("LOUD"), "info");
        assert_eq!(LogBuffer::normalize_level("warning"), "warn");

        let mut buffer = LogBuffer::new(10);
        buffer.record("info", Some("a"), "one");
        buffer.record("info", Some("b"), "two");
        buffer.record("info", None, "host");
        assert_eq!(buffer.snapshot(Some("a"), 10).len(), 1);
        assert_eq!(buffer.snapshot(None, 10).len(), 3);
        assert_eq!(buffer.snapshot(None, 1).len(), 1);
    }
}
