use crate::foundation::SyncError;
use std::fmt;
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogField {
    pub key: String,
    pub value: String,
}

impl LogField {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    pub level: LogLevel,
    pub message: String,
    pub machine_id: String,
    pub correlation_id: Option<String>,
    pub fields: Vec<LogField>,
}

impl LogEvent {
    pub fn new(
        level: LogLevel,
        message: impl Into<String>,
        machine_id: impl Into<String>,
    ) -> Self {
        Self {
            level,
            message: message.into(),
            machine_id: machine_id.into(),
            correlation_id: None,
            fields: Vec::new(),
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }

    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push(LogField::new(key, value));
        self
    }
}

pub trait Logger {
    fn emit(&self, event: &LogEvent) -> Result<(), SyncError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StderrLogger;

impl Logger for StderrLogger {
    fn emit(&self, event: &LogEvent) -> Result<(), SyncError> {
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        write!(
            stderr,
            "level={} machine_id=\"{}\" message=\"{}\"",
            event.level,
            escape(&event.machine_id),
            escape(&event.message)
        )
        .map_err(|error| SyncError::logging(error.to_string()))?;

        if let Some(correlation_id) = &event.correlation_id {
            write!(stderr, " correlation_id=\"{}\"", escape(correlation_id))
                .map_err(|error| SyncError::logging(error.to_string()))?;
        }

        for field in &event.fields {
            write!(stderr, " {}=\"{}\"", escape(&field.key), escape(&field.value))
                .map_err(|error| SyncError::logging(error.to_string()))?;
        }

        writeln!(stderr).map_err(|error| SyncError::logging(error.to_string()))
    }
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_ascii_control() => {
                escaped.push_str("\\x");
                push_hex_byte(&mut escaped, character as u8);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0F) as usize] as char);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_encodes_line_breaks_tabs_and_ascii_controls() {
        let escaped = escape("path\nnext\rline\tindent\u{001B}\u{007F}\\\"");

        assert_eq!(
            escaped,
            concat!("path\\nnext\\rline\\tindent\\x1B\\x7F", "\\\\", "\\\"")
        );
        assert!(escaped.bytes().all(|byte| !byte.is_ascii_control()));
    }
}
