//! Types shared across the pipeline and the HTTP API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalised severity of one log event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "TRACE" => Some(Self::Trace),
            "DEBUG" => Some(Self::Debug),
            "INFO" | "NOTICE" | "LOG" => Some(Self::Info),
            "WARN" | "WARNING" => Some(Self::Warn),
            "ERROR" | "ERR" | "FATAL" | "CRITICAL" | "PANIC" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// One normalised log event, as stored and as served to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    #[serde(default)]
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub service: String,
    pub container: String,
    pub level: Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Event fields plus the fields of every enclosing span, flattened.
    #[serde(default)]
    pub fields: Value,
    /// Span names, outermost first (`request`, `evaluate`, `rule.evaluate`…).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<String>,
    /// Set by the trace builder when the event was linked by shared keys rather than trace id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
}

impl LogEvent {
    pub fn field_str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    /// HTTP status mentioned in fields (`status: 503` or `status: "503 Service Unavailable"`).
    pub fn http_status(&self) -> Option<u16> {
        for key in ["status", "http_status", "status_code"] {
            match self.fields.get(key) {
                Some(Value::Number(n)) => return n.as_u64().map(|v| v as u16),
                Some(Value::String(s)) => {
                    if let Some(code) = s.split_whitespace().next().and_then(|c| c.parse().ok()) {
                        return Some(code);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// One compact line for prompts: `12:01:02.345 ERROR authrs authrs::routes::admin — msg {k=v}`.
    pub fn prompt_line(&self, max_fields: usize) -> String {
        let mut out = format!(
            "{} {:5} {} {}— {}",
            self.ts.format("%H:%M:%S%.3f"),
            self.level.as_str(),
            self.service,
            self.target.as_deref().map(|t| format!("{t} ")).unwrap_or_default(),
            self.message
        );
        if let Value::Object(map) = &self.fields {
            let mut parts = vec![];
            for (k, v) in map.iter() {
                if matches!(k.as_str(), "trace_id" | "message") {
                    continue;
                }
                let v = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if v.is_empty() {
                    continue;
                }
                parts.push(format!("{k}={}", crate::redact::truncate(&v, 160)));
                if parts.len() >= max_fields {
                    break;
                }
            }
            if !parts.is_empty() {
                out.push_str(&format!(" {{{}}}", parts.join(", ")));
            }
        }
        out
    }
}

/// Incident lifecycle.
pub const STATUS_OPEN: &str = "open";
pub const STATUS_ACK: &str = "acknowledged";
pub const STATUS_RESOLVED: &str = "resolved";

pub const TRIAGE_PENDING: &str = "pending";
pub const TRIAGE_RUNNING: &str = "running";
pub const TRIAGE_DONE: &str = "done";
pub const TRIAGE_FAILED: &str = "failed";
pub const TRIAGE_SKIPPED: &str = "skipped";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub fingerprint: String,
    pub trace_id: Option<String>,
    /// `backend` (detected in logs) | `frontend` (reported by the browser SDK) | `manual`.
    pub source: String,
    pub service: Option<String>,
    /// The log line that opened the incident (first error).
    pub anchor_log_id: Option<i64>,
    pub title: String,
    pub level: String,
    pub status: String,
    pub triage_state: String,
    pub severity: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub occurrences: i64,
    pub fe_payload: Option<Value>,
    pub report: Option<Value>,
    pub context: Option<Value>,
    pub error: Option<String>,
    pub llm_usage: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What the browser SDK posts when a request fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendError {
    /// 32-hex trace id, or a whole `traceparent` header value.
    pub trace_id: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub response_body: Option<String>,
    #[serde(default)]
    pub ui_message: Option<String>,
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub app_version: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<f64>,
    #[serde(default)]
    pub extra: Option<Value>,
}

/// Events pushed to the UI over SSE.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEvent {
    Incident { id: String, triage_state: String, status: String },
    Stats { ingested: u64, errors: u64, triaged: u64 },
}
