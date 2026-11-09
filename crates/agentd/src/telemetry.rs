//! Logging: `tracing` output, human-readable on a terminal and JSON lines
//! otherwise, with a redaction backstop.
//!
//! Secrets are `secrecy` types and never reach a field. As a backstop, the
//! value of any field whose whole name is in [`REDACTED_FIELDS`] is written
//! as [`REDACTED`], in events and in spans, in both formats. Only whole names
//! match, so fields such as `scope_key` or `token_count` are still logged.

use std::fmt::{self, Write as _};
use std::io::IsTerminal;

use anyhow::Context as _;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Field names whose values are never written.
pub const REDACTED_FIELDS: [&str; 11] = [
    "token",
    "access_token",
    "refresh_token",
    "bot_token",
    "secret",
    "client_secret",
    "signing_secret",
    "api_key",
    "code",
    "verifier",
    "password",
];

/// What a redacted field's value is replaced with.
pub const REDACTED: &str = "[redacted]";

/// Whether a field named `name` is redacted.
pub fn is_redacted(name: &str) -> bool {
    REDACTED_FIELDS.contains(&name)
}

/// How log lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One human-readable line per event, colored if `ansi`.
    Human {
        /// Whether to use ANSI colors.
        ansi: bool,
    },
    /// One JSON object per line: `timestamp`, `level`, `target`, `fields`,
    /// and `spans` (outermost first, each with `name` and `fields`).
    Json,
}

impl LogFormat {
    /// [`Human`](Self::Human) with colors when standard error is a terminal,
    /// [`Json`](Self::Json) otherwise.
    pub fn detect() -> Self {
        Self::for_stderr(std::io::stderr().is_terminal())
    }

    /// [`Human`](Self::Human) with colors when standard error `is_terminal`,
    /// [`Json`](Self::Json) otherwise.
    pub fn for_stderr(is_terminal: bool) -> Self {
        if is_terminal {
            Self::Human { ansi: true }
        } else {
            Self::Json
        }
    }
}

/// Installs the global subscriber, writing to standard error in the
/// [detected](LogFormat::detect) format, filtered by `filter`
/// (`server.log_filter`). Records from the `log` crate are forwarded to it.
///
/// # Errors
///
/// If `filter` doesn't parse or a global subscriber is already installed.
pub fn init(filter: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(filter).context("server.log_filter")?;
    subscriber(LogFormat::detect(), filter, std::io::stderr)
        .try_init()
        .context("installing the log subscriber")
}

/// A subscriber writing `format` lines to `writer`, with redaction.
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Human { ansi } => Box::new(
            registry.with(
                tracing_subscriber::fmt::layer()
                    .fmt_fields(HumanFields)
                    .with_ansi(ansi)
                    .with_writer(writer),
            ),
        ),
        LogFormat::Json => Box::new(
            registry.with(
                tracing_subscriber::fmt::layer()
                    .fmt_fields(JsonFields)
                    .event_format(JsonEvents)
                    .with_writer(writer),
            ),
        ),
    }
}

/// Formats fields as `message key=value …`, redacting and escaping control
/// characters. The `log.*` fields that records from the `log` crate carry
/// are left out, as the stock formatter does.
struct HumanFields;

impl<'w> FormatFields<'w> for HumanFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut visitor = HumanVisitor {
            out: &mut writer,
            first: true,
            result: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

struct HumanVisitor<'a, 'w> {
    out: &'a mut Writer<'w>,
    first: bool,
    result: fmt::Result,
}

impl Visit for HumanVisitor<'_, '_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();
        if self.result.is_err() || name.starts_with("log.") {
            return;
        }
        let separator = if self.first { "" } else { " " };
        self.first = false;
        let mut out = Escaped(&mut *self.out);
        self.result = if name == "message" {
            write!(out, "{separator}{value:?}")
        } else if is_redacted(name) {
            write!(out, "{separator}{name}={REDACTED}")
        } else {
            write!(out, "{separator}{name}={value:?}")
        };
    }
}

/// Writes control characters, such as a newline in a logged value, as
/// escapes, so a value can't forge a log line or send terminal sequences.
struct Escaped<'a, W>(&'a mut W);

impl<W: fmt::Write> fmt::Write for Escaped<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            if c.is_control() {
                write!(self.0, "{}", c.escape_default())?;
            } else {
                self.0.write_char(c)?;
            }
        }
        Ok(())
    }
}

/// Formats span fields as a JSON object, redacting.
struct JsonFields;

impl<'w> FormatFields<'w> for JsonFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut map = Map::new();
        fields.record(&mut JsonVisitor(&mut map));
        write!(writer, "{}", Value::Object(map))
    }

    fn add_fields(
        &self,
        current: &'w mut FormattedFields<Self>,
        fields: &tracing::span::Record<'_>,
    ) -> fmt::Result {
        let mut map = match serde_json::from_str(&current.fields) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        fields.record(&mut JsonVisitor(&mut map));
        current.fields = Value::Object(map).to_string();
        Ok(())
    }
}

struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl JsonVisitor<'_> {
    fn put(&mut self, field: &Field, value: impl FnOnce() -> Value) {
        let name = field.name();
        let value = if is_redacted(name) {
            Value::from(REDACTED)
        } else {
            value()
        };
        self.0.insert(name.to_owned(), value);
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, || Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, || Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, || Value::from(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.put(field, || {
            i64::try_from(value).map_or_else(|_| Value::from(value.to_string()), Value::from)
        });
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.put(field, || {
            u64::try_from(value).map_or_else(|_| Value::from(value.to_string()), Value::from)
        });
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, || Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, || Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, || Value::from(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, || Value::from(format!("{value:?}")));
    }
}

/// Formats events as JSON lines.
struct JsonEvents;

impl<S> FormatEvent<S, JsonFields> for JsonEvents
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let mut fields = Map::new();
        event.record(&mut JsonVisitor(&mut fields));
        let mut line = Map::new();
        let timestamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        line.insert("timestamp".to_owned(), Value::from(timestamp));
        line.insert("level".to_owned(), Value::from(metadata.level().as_str()));
        line.insert("target".to_owned(), Value::from(metadata.target()));
        line.insert("fields".to_owned(), Value::Object(fields));
        if let Some(scope) = ctx.event_scope() {
            let spans: Vec<Value> = scope
                .from_root()
                .map(|span| {
                    let fields = span
                        .extensions()
                        .get::<FormattedFields<JsonFields>>()
                        .and_then(|stored| serde_json::from_str(&stored.fields).ok())
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    let mut entry = Map::new();
                    entry.insert("name".to_owned(), Value::from(span.name()));
                    entry.insert("fields".to_owned(), fields);
                    Value::Object(entry)
                })
                .collect();
            line.insert("spans".to_owned(), Value::Array(spans));
        }
        writeln!(writer, "{}", Value::Object(line))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A writer that collects everything written to it.
    #[derive(Clone, Default)]
    pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'w> MakeWriter<'w> for Captured {
        type Writer = Self;

        fn make_writer(&'w self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture(format: LogFormat, f: impl FnOnce()) -> String {
        let captured = Captured::default();
        let subscriber = subscriber(format, EnvFilter::new("trace"), captured.clone());
        tracing::subscriber::with_default(subscriber, f);
        captured.text()
    }

    fn emit_everything() {
        let span = tracing::info_span!(
            "turn",
            scope_key = "dm:slack:T1:D1",
            verifier = "span-verifier",
            later = tracing::field::Empty
        );
        let _entered = span.enter();
        span.record("later", "recorded-later");
        tracing::info!(
            token = "tok-1",
            access_token = "tok-2",
            refresh_token = "tok-3",
            bot_token = "tok-4",
            secret = "tok-5",
            client_secret = "tok-6",
            signing_secret = "tok-7",
            api_key = "tok-8",
            code = 424_242_i64,
            password = ?"tok-10",
            scope_key = "dm:slack:T1:D1",
            token_count = 7_u64,
            "handled a request"
        );
    }

    const SECRETS: [&str; 10] = [
        "tok-1", "tok-2", "tok-3", "tok-4", "tok-5", "tok-6", "tok-7", "tok-8", "424242", "tok-10",
    ];

    #[test]
    fn every_listed_name_is_redacted_in_human_output() {
        let out = capture(LogFormat::Human { ansi: false }, emit_everything);
        for secret in SECRETS {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(!out.contains("span-verifier"), "{out}");
        for name in REDACTED_FIELDS.iter().filter(|n| **n != "verifier") {
            assert!(out.contains(&format!("{name}={REDACTED}")), "{name}: {out}");
        }
        assert!(out.contains(&format!("verifier={REDACTED}")), "{out}");
        assert!(out.contains("scope_key=\"dm:slack:T1:D1\""), "{out}");
        assert!(out.contains("token_count=7"), "{out}");
        assert!(out.contains("later=\"recorded-later\""), "{out}");
        assert!(out.contains("handled a request"), "{out}");
    }

    #[test]
    fn every_listed_name_is_redacted_in_json_output() {
        let out = capture(LogFormat::Json, emit_everything);
        for secret in SECRETS {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(!out.contains("span-verifier"), "{out}");
        let line: Value = serde_json::from_str(out.trim()).unwrap();
        let fields = &line["fields"];
        for name in REDACTED_FIELDS.iter().filter(|n| **n != "verifier") {
            assert_eq!(fields[*name], REDACTED, "{name}: {out}");
        }
        assert_eq!(fields["scope_key"], "dm:slack:T1:D1");
        assert_eq!(fields["token_count"], 7);
        assert_eq!(fields["message"], "handled a request");
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["target"], module_path!());
        assert!(line["timestamp"].as_str().unwrap().ends_with('Z'), "{out}");
        let span = &line["spans"][0];
        assert_eq!(span["name"], "turn");
        assert_eq!(span["fields"]["verifier"], REDACTED);
        assert_eq!(span["fields"]["scope_key"], "dm:slack:T1:D1");
        assert_eq!(span["fields"]["later"], "recorded-later");
    }

    #[test]
    fn json_keeps_value_types() {
        let out = capture(LogFormat::Json, || {
            tracing::warn!(
                ratio = 0.5_f64,
                ok = true,
                small = -3_i128,
                big = u128::MAX,
                negative = i128::MIN,
                huge = 1_u128,
                error = &io::Error::other("boom") as &(dyn std::error::Error + 'static),
                "typed"
            );
        });
        let line: Value = serde_json::from_str(out.trim()).unwrap();
        let fields = &line["fields"];
        assert_eq!(fields["ratio"], 0.5);
        assert_eq!(fields["ok"], true);
        assert_eq!(fields["small"], -3);
        assert_eq!(fields["big"], u128::MAX.to_string());
        assert_eq!(fields["negative"], i128::MIN.to_string());
        assert_eq!(fields["huge"], 1);
        assert_eq!(fields["error"], "boom");
        assert_eq!(line["level"], "WARN");
        assert!(line.get("spans").is_none(), "{out}");
    }

    #[test]
    fn human_output_leaves_out_log_crate_fields() {
        let out = capture(LogFormat::Human { ansi: false }, || {
            tracing::info!(log.target = "sqlx::query", kept = 1, "from log");
        });
        assert!(out.contains("from log kept=1"), "{out}");
        assert!(!out.contains("sqlx::query"), "{out}");
    }

    #[test]
    fn human_output_escapes_control_characters() {
        let out = capture(LogFormat::Human { ansi: false }, || {
            tracing::info!(name = ?"a\nb", "line one\nforged \u{1b}[31m");
        });
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("line one\\nforged \\u{1b}[31m"), "{out}");
        assert!(!out.contains('\u{1b}'), "{out}");
    }

    #[test]
    fn json_output_is_one_line_per_event() {
        let out = capture(LogFormat::Json, || {
            tracing::info!(value = "a\nb", "one\ntwo");
            tracing::debug!("second");
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["fields"]["value"], "a\nb");
    }

    #[test]
    fn the_filter_applies() {
        let captured = Captured::default();
        let subscriber = subscriber(LogFormat::Json, EnvFilter::new("warn"), captured.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hidden");
            tracing::warn!("shown");
        });
        let out = captured.text();
        assert!(!out.contains("hidden") && out.contains("shown"), "{out}");
    }

    #[test]
    fn a_terminal_gets_colored_human_lines() {
        assert_eq!(LogFormat::for_stderr(true), LogFormat::Human { ansi: true });
    }

    #[test]
    fn anything_else_gets_json() {
        assert_eq!(LogFormat::for_stderr(false), LogFormat::Json);
    }

    #[test]
    fn init_rejects_a_bad_filter() {
        let err = init("agentd=loud").unwrap_err();
        assert!(format!("{err:#}").contains("server.log_filter"), "{err:#}");
    }
}
