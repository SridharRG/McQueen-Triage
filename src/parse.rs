//! Log line normalisation.
//!
//! The Rust services log with `tracing_subscriber::fmt().json()`, in two shapes:
//! - flattened (`flatten_event(true)`): `{"timestamp","level","message","target",…fields,"span":{…},"spans":[…]}`
//! - nested (platform-gw): `{"timestamp","level","fields":{"message",…},"target"}`
//!
//! `trace_id` can sit at the top level, in the current `span`, or in any entry of
//! `spans`; an empty string means the service has the field but did not receive a
//! `traceparent`. Everything else (Next.js, Python, panics) is plain text, where
//! multi-line blocks (Rust panics, Python tracebacks, JS stacks) are folded into
//! one event by [`Aggregator`].

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Map, Value};

use crate::model::Level;

/// The parts of a [`crate::model::LogEvent`] that come from the line itself.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub ts: DateTime<Utc>,
    pub level: Level,
    pub target: Option<String>,
    pub message: String,
    pub trace_id: Option<String>,
    pub fields: Value,
    pub spans: Vec<String>,
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap());
static TRACEPARENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b00-([0-9a-f]{32})-[0-9a-f]{16}-[0-9a-f]{2}\b").unwrap());
static TEXT_TRACE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\btrace[_-]?id["']?\s*[=:]\s*["']?([0-9a-f]{32})\b"#).unwrap()
});
static TEXT_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s\[|(])(TRACE|DEBUG|INFO|WARN|WARNING|ERROR|FATAL|CRITICAL)(?:[\s\]|):]|$)").unwrap()
});

/// Valid W3C trace id: 32 hex, not all zeros. Returned lowercase.
pub fn valid_trace_id(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0'))
        .then(|| s.to_ascii_lowercase())
}

/// Accept either a bare trace id or a full `traceparent` header value.
pub fn trace_id_from_input(s: &str) -> Option<String> {
    let s = s.trim();
    valid_trace_id(s).or_else(|| {
        TRACEPARENT
            .captures(&s.to_ascii_lowercase())
            .and_then(|c| valid_trace_id(&c[1]))
    })
}

/// Split Docker's `timestamps=true` prefix off a line.
pub fn split_docker_ts(line: &str) -> (Option<DateTime<Utc>>, &str) {
    if let Some((head, rest)) = line.split_once(' ') {
        if let Ok(ts) = DateTime::parse_from_rfc3339(head) {
            return (Some(ts.with_timezone(&Utc)), rest);
        }
    }
    (None, line)
}

pub fn strip_ansi(s: &str) -> String {
    if s.contains('\x1b') {
        ANSI.replace_all(s, "").into_owned()
    } else {
        s.to_string()
    }
}

/// Parse one (possibly folded multi-line) log record.
pub fn parse(text: &str, docker_ts: DateTime<Utc>) -> Parsed {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) {
            return parse_json(obj, docker_ts);
        }
    }
    parse_text(trimmed, docker_ts)
}

const TOP_LEVEL_KEYS: &[&str] = &[
    "timestamp", "time", "ts", "@timestamp", "level", "severity", "levelname", "lvl", "message",
    "msg", "target", "logger", "span", "spans", "fields", "threadName", "threadId",
];

fn parse_json(mut obj: Map<String, Value>, docker_ts: DateTime<Utc>) -> Parsed {
    let str_of = |obj: &Map<String, Value>, keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(Value::as_str).map(str::to_string))
    };

    let ts = str_of(&obj, &["timestamp", "time", "ts", "@timestamp"])
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or(docker_ts);
    let level = str_of(&obj, &["level", "severity", "levelname", "lvl"])
        .and_then(|s| Level::parse(&s))
        .unwrap_or(Level::Info);
    let target = str_of(&obj, &["target", "logger"]);

    let mut fields = Map::new();
    let nested = obj.remove("fields");
    let mut message = str_of(&obj, &["message", "msg"]);
    if let Some(Value::Object(nested)) = nested {
        for (k, v) in nested {
            if k == "message" && message.is_none() {
                message = v.as_str().map(str::to_string);
            } else {
                fields.insert(k, v);
            }
        }
    }

    // Spans: outermost first. Span fields never overwrite event fields.
    let mut spans = vec![];
    let mut span_fields: Vec<(String, Value)> = vec![];
    if let Some(Value::Array(list)) = obj.get("spans") {
        for span in list {
            if let Value::Object(span) = span {
                if let Some(name) = span.get("name").and_then(Value::as_str) {
                    spans.push(name.to_string());
                }
                for (k, v) in span {
                    if k != "name" {
                        span_fields.push((k.clone(), v.clone()));
                    }
                }
            }
        }
    }
    if spans.is_empty() {
        if let Some(Value::Object(span)) = obj.get("span") {
            if let Some(name) = span.get("name").and_then(Value::as_str) {
                spans.push(name.to_string());
            }
            for (k, v) in span {
                if k != "name" {
                    span_fields.push((k.clone(), v.clone()));
                }
            }
        }
    }

    for (k, v) in obj.iter() {
        if !TOP_LEVEL_KEYS.contains(&k.as_str()) {
            fields.insert(k.clone(), v.clone());
        }
    }

    // trace id: event field, then innermost span outward.
    let mut trace_id = fields
        .get("trace_id")
        .or_else(|| fields.get("traceId"))
        .and_then(Value::as_str)
        .and_then(valid_trace_id);
    if trace_id.is_none() {
        trace_id = span_fields
            .iter()
            .rev()
            .filter(|(k, _)| k == "trace_id" || k == "traceId")
            .find_map(|(_, v)| v.as_str().and_then(valid_trace_id));
    }
    if trace_id.is_none() {
        if let Some(tp) = fields.get("traceparent").and_then(Value::as_str) {
            trace_id = trace_id_from_input(tp);
        }
    }

    for (k, v) in span_fields {
        fields.entry(k).or_insert(v);
    }
    // Keep the empty-string marker visible: it tells the coverage view the
    // service has a trace field but received no traceparent.
    if let Some(tid) = &trace_id {
        fields.insert("trace_id".into(), Value::String(tid.clone()));
    }

    let message = message.unwrap_or_else(|| {
        // No message key: show the fields compactly rather than nothing.
        Value::Object(fields.clone()).to_string()
    });

    Parsed {
        ts,
        level,
        target,
        message,
        trace_id,
        fields: Value::Object(fields),
        spans,
    }
}

fn parse_text(text: &str, docker_ts: DateTime<Utc>) -> Parsed {
    let first_line = text.lines().next().unwrap_or("");
    let level = if text.contains("panicked at")
        || text.starts_with("Traceback (most recent call last)")
        || first_line.starts_with('⨯')
        || first_line.contains(" ⨯ ")
    {
        Level::Error
    } else if let Some(c) = TEXT_LEVEL.captures(first_line) {
        Level::parse(&c[1]).unwrap_or(Level::Info)
    } else if first_line.contains("Error:") || first_line.contains("Exception") {
        Level::Error
    } else {
        Level::Info
    };
    let lower = text.to_ascii_lowercase();
    let trace_id = TRACEPARENT
        .captures(&lower)
        .and_then(|c| valid_trace_id(&c[1]))
        .or_else(|| TEXT_TRACE_ID.captures(&lower).and_then(|c| valid_trace_id(&c[1])));

    Parsed {
        ts: docker_ts,
        level,
        target: None,
        message: text.to_string(),
        trace_id,
        fields: Value::Object(Map::new()),
        spans: vec![],
    }
}

/// Folds multi-line text records (panics, tracebacks, JS stacks) into one.
#[derive(Default)]
pub struct Aggregator {
    pending: Option<(DateTime<Utc>, String, BlockKind)>,
}

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Plain,
    Panic,
    Traceback,
}

static EXCEPTION_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][\w.]*(Error|Exception|Exit|Interrupt)\b").unwrap());

impl Aggregator {
    /// Feed one line; returns the records that are now complete.
    pub fn push(&mut self, ts: DateTime<Utc>, line: &str) -> Vec<(DateTime<Utc>, String)> {
        let mut done = vec![];
        let is_json = line.trim_start().starts_with('{');

        if let Some((_, buf, kind)) = &mut self.pending {
            if !is_json && is_continuation(line, *kind) {
                buf.push('\n');
                buf.push_str(line);
                // A traceback ends at its exception line.
                if *kind == BlockKind::Traceback && EXCEPTION_LINE.is_match(line) {
                    done.extend(self.flush());
                }
                return done;
            }
            done.extend(self.flush());
        }

        if is_json {
            done.push((ts, line.to_string()));
            return done;
        }
        let kind = if line.contains("panicked at") {
            BlockKind::Panic
        } else if line.starts_with("Traceback (most recent call last)") {
            BlockKind::Traceback
        } else {
            BlockKind::Plain
        };
        self.pending = Some((ts, line.to_string(), kind));
        done
    }

    pub fn flush(&mut self) -> Option<(DateTime<Utc>, String)> {
        self.pending.take().map(|(ts, buf, _)| (ts, buf))
    }
}

fn is_continuation(line: &str, kind: BlockKind) -> bool {
    if line.is_empty() {
        return false;
    }
    let indented = line.starts_with(' ') || line.starts_with('\t');
    match kind {
        BlockKind::Panic => {
            // `thread 'x' panicked at src/a.rs:1:2:` is followed by the message and a note.
            !line.starts_with('{') && !TEXT_LEVEL.is_match(line) || indented
        }
        BlockKind::Traceback => indented || EXCEPTION_LINE.is_match(line),
        BlockKind::Plain => {
            indented && (line.trim_start().starts_with("at ") || line.trim_start().starts_with("..."))
                || line.starts_with("Caused by:")
                || line.starts_with("stack backtrace:")
                || line.starts_with("note: run with")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn flattened_tracing_json_with_span_trace_id() {
        let line = r#"{"timestamp":"2026-10-20T04:35:28.995165Z","level":"DEBUG","message":"X-User-ID resolved from session token","user_id":"22556570","slug":"message-flow","target":"launchpad::service_registry","span":{"trace_id":"206a533f376743d3a79403650e2a2958","name":"request"},"spans":[{"trace_id":"206a533f376743d3a79403650e2a2958","name":"request"}]}"#;
        let p = parse(line, now());
        assert_eq!(p.level, Level::Debug);
        assert_eq!(p.target.as_deref(), Some("launchpad::service_registry"));
        assert_eq!(p.trace_id.as_deref(), Some("206a533f376743d3a79403650e2a2958"));
        assert_eq!(p.fields["slug"], "message-flow");
        assert_eq!(p.spans, vec!["request"]);
    }

    #[test]
    fn empty_trace_id_is_none_but_span_fields_kept() {
        let line = r#"{"timestamp":"2026-10-20T04:35:27.677649Z","level":"INFO","message":"permission check request","tenant_id":"siemens_energy","target":"authrs::routes::admin","span":{"method":"POST","path":"/admin/permissions/check","trace_id":"","name":"request"},"spans":[{"method":"POST","path":"/admin/permissions/check","trace_id":"","name":"request"}]}"#;
        let p = parse(line, now());
        assert_eq!(p.trace_id, None);
        assert_eq!(p.fields["path"], "/admin/permissions/check");
        assert_eq!(p.fields["trace_id"], "");
    }

    #[test]
    fn nested_fields_shape() {
        let line = r#"{"timestamp":"2026-10-19T10:32:32.809551Z","level":"WARN","fields":{"message":"GW_ADMIN_PASSWORD is still set","mode":"all"},"target":"gw_admin::bootstrap"}"#;
        let p = parse(line, now());
        assert_eq!(p.message, "GW_ADMIN_PASSWORD is still set");
        assert_eq!(p.level, Level::Warn);
        assert_eq!(p.fields["mode"], "all");
    }

    #[test]
    fn innermost_span_trace_id_wins_over_empty_outer() {
        let line = r#"{"timestamp":"2026-10-19T14:56:57.433355Z","level":"ERROR","message":"x","target":"t","spans":[{"trace_id":"","name":"request"},{"trace_id":"aa24a61056204f52938a80af2d74de6b","name":"execution"}]}"#;
        let p = parse(line, now());
        assert_eq!(p.trace_id.as_deref(), Some("aa24a61056204f52938a80af2d74de6b"));
        assert_eq!(p.spans, vec!["request", "execution"]);
    }

    #[test]
    fn text_levels_and_traceparent() {
        let p = parse("2026 ERROR something broke traceparent=00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", now());
        assert_eq!(p.level, Level::Error);
        assert_eq!(p.trace_id.as_deref(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(parse("✓ Ready in 258ms", now()).level, Level::Info);
        assert_eq!(parse(" ⨯ TypeError: x is undefined", now()).level, Level::Error);
    }

    #[test]
    fn trace_input_forms() {
        assert_eq!(
            trace_id_from_input("00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01").as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert_eq!(trace_id_from_input("00000000000000000000000000000000"), None);
    }

    #[test]
    fn folds_rust_panic_and_python_traceback() {
        let t = now();
        let mut agg = Aggregator::default();
        assert!(agg.push(t, "thread 'tokio-runtime-worker' panicked at src/policy/engine.rs:42:17:").is_empty());
        assert!(agg.push(t, "index out of bounds: the len is 0 but the index is 0").is_empty());
        assert!(agg.push(t, "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace").is_empty());
        let out = agg.push(t, r#"{"level":"INFO","message":"next"}"#);
        assert_eq!(out.len(), 2);
        assert!(out[0].1.contains("index out of bounds") && out[0].1.contains("RUST_BACKTRACE"));

        let mut agg = Aggregator::default();
        agg.push(t, "Traceback (most recent call last):");
        agg.push(t, "  File \"/app/worker.py\", line 12, in <module>");
        agg.push(t, "    run()");
        let out = agg.push(t, "KeyError: 'job_id'");
        assert_eq!(out.len(), 1);
        assert!(out[0].1.ends_with("KeyError: 'job_id'"));
        assert_eq!(parse(&out[0].1, t).level, Level::Error);
    }
}
