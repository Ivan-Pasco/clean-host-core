//! `clean:host/log@1.0.0` (§7.2, CLNH-37/39) and the sink concrete hosts
//! override (CLNH-66).
//!
//! Bridges and guests use this rather than `wasi:logging` when they want
//! structured output — the difference that matters is the automatically
//! injected `component`, `bridge` and `namespace` fields, which is what makes
//! a record attributable after the fact.
//!
//! Schema source of truth:
//! `foundation/02 components/hosts/clean-host-core/schema/host-log.wit.md`.

use std::sync::Arc;

/// Severity, mirroring the WIT `level` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Parse a `[host] log-level` value. Unknown values fall back to `info`
    /// rather than failing startup: a typo in a log level should not stop a
    /// deployment from serving.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// One structured record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub level: Level,
    pub message: String,
    /// Caller-supplied fields.
    pub fields: Vec<(String, String)>,
    /// Which component emitted it. Injected by the library (CLNH-39), not
    /// supplied by the caller — a component cannot claim to be another.
    pub component: String,
}

impl Record {
    /// Render as one `key=value` line.
    ///
    /// Values containing whitespace or `=` are quoted so a parser can recover
    /// the original fields; without that, a message with a space silently
    /// becomes several fields.
    pub fn render(&self) -> String {
        let mut out = format!(
            "level={} component={} message={}",
            self.level.as_str(),
            quote(&self.component),
            quote(&self.message)
        );
        for (key, value) in &self.fields {
            out.push_str(&format!(" {}={}", sanitize_key(key), quote(value)));
        }
        out
    }
}

/// Quote a value when it would otherwise be ambiguous.
fn quote(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || c == '=' || c == '"');
    if needs_quoting {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

/// Keys must not carry separators, or a reader cannot split the line.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_whitespace() || c == '=' || c == '"' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Where log records go.
///
/// CLNH-66: the default writes to stderr; concrete hosts replace it — the
/// server routes records into `tracing` so guest output interleaves correctly
/// with its own request logs.
pub trait LogSink: Send + Sync {
    fn emit(&self, record: &Record);
}

/// The default sink: one line per record on stderr.
///
/// This is the only place the library writes to a stream, and it exists
/// precisely so a host that does not care about logging still gets output
/// rather than silence.
#[derive(Debug, Default)]
pub struct StderrSink;

impl LogSink for StderrSink {
    fn emit(&self, record: &Record) {
        eprintln!("{}", record.render());
    }
}

/// A sink that discards everything, for tests.
#[derive(Debug, Default)]
pub struct NullSink;

impl LogSink for NullSink {
    fn emit(&self, _record: &Record) {}
}

/// The log surface handed to composed components.
pub struct Logger {
    sink: Arc<dyn LogSink>,
    /// Records below this level are dropped before reaching the sink.
    min_level: Level,
}

impl Logger {
    pub fn new(sink: Arc<dyn LogSink>, min_level: Level) -> Self {
        Self { sink, min_level }
    }

    /// Emit a record from a named component.
    ///
    /// `component` is supplied by the library rather than the caller so a
    /// record cannot claim to have come from somewhere else.
    pub fn emit(
        &self,
        component: &str,
        level: Level,
        message: &str,
        fields: Vec<(String, String)>,
    ) {
        if level < self.min_level {
            return;
        }
        self.sink.emit(&Record {
            level,
            message: message.to_string(),
            fields,
            component: component.to_string(),
        });
    }

    pub fn min_level(&self) -> Level {
        self.min_level
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self::new(Arc::new(StderrSink), Level::Info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturingSink {
        records: Mutex<Vec<Record>>,
    }

    impl LogSink for CapturingSink {
        fn emit(&self, record: &Record) {
            self.records.lock().unwrap().push(record.clone());
        }
    }

    #[test]
    fn levels_round_trip_through_their_names() {
        for level in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ] {
            assert_eq!(Level::parse(level.as_str()), Some(level));
        }
    }

    #[test]
    fn an_unknown_level_is_none_rather_than_a_panic() {
        assert_eq!(Level::parse("shout"), None);
        // "warning" is common enough in config files to accept.
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
    }

    #[test]
    fn records_below_the_minimum_are_dropped() {
        let sink = Arc::new(CapturingSink::default());
        let logger = Logger::new(sink.clone(), Level::Warn);

        logger.emit("guest", Level::Debug, "noisy", vec![]);
        logger.emit("guest", Level::Error, "important", vec![]);

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, "important");
    }

    #[test]
    fn the_component_is_injected_not_taken_from_the_caller() {
        // CLNH-39: a record must be attributable, so the emitter cannot claim
        // to be someone else.
        let sink = Arc::new(CapturingSink::default());
        let logger = Logger::new(sink.clone(), Level::Trace);

        logger.emit(
            "session-bridge",
            Level::Info,
            "stored",
            vec![("component".into(), "not-me".into())],
        );

        let records = sink.records.lock().unwrap();
        assert_eq!(records[0].component, "session-bridge");
    }

    #[test]
    fn a_plain_record_renders_as_key_values() {
        let record = Record {
            level: Level::Info,
            message: "started".into(),
            fields: vec![("port".into(), "3000".into())],
            component: "guest".into(),
        };
        assert_eq!(
            record.render(),
            "level=info component=guest message=started port=3000"
        );
    }

    #[test]
    fn a_message_with_spaces_is_quoted() {
        // Otherwise one message silently becomes several fields.
        let record = Record {
            level: Level::Warn,
            message: "could not connect".into(),
            fields: vec![],
            component: "guest".into(),
        };
        assert!(record.render().contains(r#"message="could not connect""#));
    }

    #[test]
    fn a_field_key_cannot_inject_a_separator() {
        let record = Record {
            level: Level::Info,
            message: "x".into(),
            fields: vec![("evil key=injected".into(), "v".into())],
            component: "guest".into(),
        };
        let line = record.render();
        assert!(line.contains("evil_key_injected=v"), "{line}");
    }

    #[test]
    fn quotes_in_a_value_are_escaped() {
        let record = Record {
            level: Level::Info,
            message: r#"say "hi""#.into(),
            fields: vec![],
            component: "guest".into(),
        };
        let line = record.render();
        assert!(line.contains(r#"\"hi\""#), "{line}");
    }

    #[test]
    fn an_empty_value_is_still_representable() {
        let record = Record {
            level: Level::Info,
            message: String::new(),
            fields: vec![],
            component: "guest".into(),
        };
        assert!(record.render().contains(r#"message="""#));
    }

    #[test]
    fn the_null_sink_discards_everything() {
        let logger = Logger::new(Arc::new(NullSink), Level::Trace);
        logger.emit("guest", Level::Error, "ignored", vec![]);
    }
}
