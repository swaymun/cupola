//! Owner-visible local JSONL logging with conservative secret redaction.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use serde_json::{Map, Value};

const MAX_LOG_BYTES: u64 = 100 * 1024 * 1024;

pub struct JsonlLogger {
    path: PathBuf,
    max_bytes: u64,
    /// Shared by every logger in this process that writes `path`, so one
    /// record and its rotation finish before the next record starts.
    writer: Arc<Mutex<()>>,
}

/// One lock per log file, so independently created loggers sharing a
/// destination never interleave their writes or rotation.
fn writer_for(path: &Path) -> Arc<Mutex<()>> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut writers = WRITERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writers.entry(path.to_path_buf()).or_default().clone()
}

impl JsonlLogger {
    pub fn new(directory: impl AsRef<Path>, filename: &str) -> std::io::Result<Self> {
        Self::with_limit(directory, filename, MAX_LOG_BYTES)
    }

    fn with_limit(
        directory: impl AsRef<Path>,
        filename: &str,
        max_bytes: u64,
    ) -> std::io::Result<Self> {
        fs::create_dir_all(directory.as_ref())?;
        let path = directory.as_ref().join(filename);
        Ok(Self {
            writer: writer_for(&path),
            path,
            max_bytes,
        })
    }

    pub fn record(&self, severity: &str, event: &str, fields: Value) -> std::io::Result<()> {
        let mut record = Map::new();
        record.insert(
            "timestamp".into(),
            Value::String(
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            ),
        );
        record.insert("severity".into(), Value::String(severity.into()));
        record.insert("subsystem".into(), Value::String("wonderd".into()));
        record.insert("event".into(), Value::String(event.into()));
        if let Value::Object(fields) = redact(fields) {
            record.extend(fields);
        }
        let mut record = Value::Object(record);
        crate::secrets::redact_json(&mut record);
        let mut line = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        // The line break travels with its record: written apart, two records
        // written at once could share a line.
        line.push(b'\n');
        let _writing = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.rotate_if_needed()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&line)?;
        file.flush()
    }

    fn rotate_if_needed(&self) -> std::io::Result<()> {
        if self
            .path
            .metadata()
            .map(|metadata| metadata.len() >= self.max_bytes)
            .unwrap_or(false)
        {
            let rotated = self.path.with_extension("jsonl.1");
            let _ = fs::remove_file(&rotated);
            fs::rename(&self.path, rotated)?;
        }
        Ok(())
    }
}

/// The key fragments whose values are never logged or served.
const SENSITIVE_KEYS: [&str; 12] = [
    "token",
    "secret",
    "cookie",
    "privatekey",
    "authorization",
    "credential",
    "password",
    "prompt",
    "audio",
    "verification",
    "challenge",
    "nonce",
];

/// The longest line [`redact_log_line`] returns.
const MAX_LOG_LINE_CHARS: usize = 2000;

/// One log line made safe to hand to a phone: a JSON record goes through the
/// same redaction the logger applies when it writes, and any other line (the
/// supervisor's own, which is plain text) has the value of each `key=value`
/// whose key is sensitive replaced, and the whole line when it holds a
/// bearer credential or pairing secret.
pub fn redact_log_line(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    let redacted = match serde_json::from_str::<Value>(line) {
        Ok(value @ Value::Object(_)) => redact(value).to_string(),
        _ => redact_text(line),
    };
    match redacted.char_indices().nth(MAX_LOG_LINE_CHARS) {
        Some((end, _)) => format!("{}…", &redacted[..end]),
        None => redacted,
    }
}

fn redact_text(line: &str) -> String {
    if line.contains("#secret=") || line.contains("Bearer ") || line.contains("authorization=") {
        return "[REDACTED]".into();
    }
    // Replace the value after each `=` whose key (the run of name characters
    // before it) is sensitive, up to the next space or `&`.
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(equals) = rest.find('=') {
        let (before, after) = rest.split_at(equals + 1);
        out.push_str(before);
        let key_start = before[..equals]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .map_or(0, |index| index + 1);
        let key = before[key_start..equals].to_ascii_lowercase();
        if SENSITIVE_KEYS.iter().any(|needle| key.contains(needle)) {
            let end = after.find([' ', '&']).unwrap_or(after.len());
            if end > 0 {
                out.push_str("[REDACTED]");
            }
            rest = &after[end..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn redact(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let lowered = key.to_ascii_lowercase();
                    let redacted = SENSITIVE_KEYS.iter().any(|needle| lowered.contains(needle))
                        || matches!(lowered.as_str(), "code" | "phonecode" | "phone_code");
                    (
                        key,
                        if redacted {
                            Value::String("[REDACTED]".into())
                        } else {
                            redact(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
        Value::String(value)
            if value.contains("#secret=")
                || value.contains("Bearer ")
                || value.contains("authorization=") =>
        {
            Value::String("[REDACTED]".into())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_nested_fields() {
        let value = redact(serde_json::json!({
            "deviceId": "device-1",
            "sessionToken": "secret",
            "nested": [{"privateKeyJwk": "secret"}],
        }));
        assert_eq!(value["deviceId"], "device-1");
        assert_eq!(value["sessionToken"], "[REDACTED]");
        assert_eq!(value["nested"][0]["privateKeyJwk"], "[REDACTED]");
    }

    #[test]
    fn redacts_log_lines_for_a_phone() {
        let json =
            redact_log_line(r#"{"event":"x","sessionToken":"hunter2","nested":{"password":"p"}}"#);
        assert!(
            !json.contains("hunter2") && !json.contains("\"p\""),
            "{json}"
        );
        assert!(json.contains("\"event\":\"x\""));
        assert_eq!(
            redact_log_line("retry token=abc123 id=7 password=x&ok=1"),
            "retry token=[REDACTED] id=7 password=[REDACTED]&ok=1"
        );
        assert_eq!(redact_log_line("open https://h/#secret=zz"), "[REDACTED]");
        assert_eq!(redact_log_line("Authorization: Bearer abc"), "[REDACTED]");
        assert_eq!(redact_log_line("plain line\r\n"), "plain line");
        let long = redact_log_line(&"x".repeat(5000));
        assert_eq!(long.chars().count(), MAX_LOG_LINE_CHARS + 1);
    }

    #[test]
    fn records_written_at_once_stay_one_per_line_through_rotation() {
        let directory = tempfile::tempdir().unwrap();
        // Independently created loggers must still share the file's writer.
        let loggers: Vec<_> = (0..2)
            .map(|_| {
                Arc::new(
                    JsonlLogger::with_limit(directory.path(), "host.jsonl", 64 * 1024).unwrap(),
                )
            })
            .collect();
        let padding = "x".repeat(512);
        let threads: Vec<_> = (0..8)
            .map(|writer| {
                let logger = loggers[writer % 2].clone();
                let padding = padding.clone();
                std::thread::spawn(move || {
                    for index in 0..200 {
                        logger
                            .record(
                                "info",
                                "burst",
                                serde_json::json!({"writer": writer, "index": index, "padding": padding}),
                            )
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let mut lines = 0;
        for name in ["host.jsonl", "host.jsonl.1"] {
            let saved = std::fs::read_to_string(directory.path().join(name)).unwrap();
            for line in saved.lines() {
                let record: Value = serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("{name}: {error}: {line:.200}"));
                assert_eq!(record["event"], "burst");
                lines += 1;
            }
        }
        // Rotation happened, and the kept files hold only whole records.
        assert!(directory.path().join("host.jsonl.1").exists());
        assert!(lines > 0);
    }

    #[test]
    fn pairing_verification_cannot_be_recovered_from_logs() {
        let directory = tempfile::tempdir().unwrap();
        let logger = JsonlLogger::new(directory.path(), "pairing.jsonl").unwrap();
        logger
            .record(
                "info",
                "pairing",
                serde_json::json!({
                    "deviceId": "phone",
                    "exitCode": 1,
                    "nested": [{"verificationText": "B36D1B62BB5F", "code": "B36D1B62BB5F",
                        "challenge": {"nonce": "phone-only-nonce"}}],
                }),
            )
            .unwrap();
        let saved = std::fs::read_to_string(directory.path().join("pairing.jsonl")).unwrap();
        assert!(!saved.contains("B36D1B62BB5F"), "{saved}");
        assert!(!saved.contains("phone-only-nonce"), "{saved}");
        assert!(saved.contains("phone"));
        assert_eq!(
            serde_json::from_str::<Value>(&saved).unwrap()["exitCode"],
            1
        );
    }
}
