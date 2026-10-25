//! Rebuilds one request's journey across services.
//!
//! The services emit `trace_id` on their log lines but no spans, so the call
//! graph is reconstructed from logs:
//! - **traced** events: every line carrying the trace id;
//! - **correlated** events: untraced lines in the same time window that share a
//!   specific key with the traced ones (`user_id`, `execution_id`, `resource`…).
//!   This covers services that drop the context today (core logs almost no
//!   trace ids; authrs and decision-hub log `trace_id: ""`);
//! - **edges**: *observed* from URLs/slugs in log lines (`http://pf-message-flow:8080/…`,
//!   `slug=message-flow`), otherwise *inferred* from timing (the service whose
//!   active window encloses the callee's first line is taken as the caller).
//!
//! When `JAEGER_URL` is set and Jaeger knows the trace, its spans are merged in.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::LazyLock,
};

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::{
    model::{FrontendError, Level, LogEvent},
    AppState,
};

#[derive(Debug, Clone, Serialize)]
pub struct TraceView {
    pub trace_id: Option<String>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub duration_ms: i64,
    pub services: Vec<ServiceNode>,
    pub edges: Vec<Edge>,
    pub events: Vec<LogEvent>,
    pub correlated: Vec<LogEvent>,
    pub nearby_problems: Vec<LogEvent>,
    pub first_error_id: Option<i64>,
    pub gaps: Vec<String>,
    pub jaeger_spans: Vec<JaegerSpan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceNode {
    pub name: String,
    pub layer: String,
    pub first_ts: DateTime<Utc>,
    pub last_ts: DateTime<Utc>,
    pub events: usize,
    pub errors: usize,
    pub warns: usize,
    pub status: &'static str,
    pub operations: Vec<String>,
    /// `trace` | `correlated` | `jaeger` | `frontend` | `referenced` (called, logged nothing)
    pub via: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// `observed` (URL / slug / span reference) or `inferred` (timing).
    pub kind: &'static str,
    pub label: Option<String>,
    pub error: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct JaegerSpan {
    pub span_id: String,
    pub parent_id: Option<String>,
    pub service: String,
    pub operation: String,
    pub start: DateTime<Utc>,
    pub duration_ms: f64,
    pub error: bool,
}

static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://([A-Za-z0-9_.-]+)(?::\d+)?(/[^\s)\x22',]*)?").unwrap());

/// Keys specific enough to link an untraced line to a request in a ±few-second window.
const CORRELATION_KEYS: &[&str] = &[
    "user_id", "execution_id", "task", "task_id", "job_id", "workflow_id", "rule_id", "ruleset_id",
    "resource", "request_id", "journey_id", "notification_id",
];

pub async fn build(app: &AppState, trace_id: &str, fe: Option<&FrontendError>) -> Result<TraceView> {
    let events = app.store.logs_by_trace(trace_id, 3000).await?;
    let jaeger_spans = match &app.cfg.jaeger_url {
        Some(url) => fetch_jaeger(url, trace_id).await.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "jaeger lookup failed");
            vec![]
        }),
        None => vec![],
    };

    let (mut start, mut end) = bounds(&events);
    for s in &jaeger_spans {
        start = Some(start.map_or(s.start, |t| t.min(s.start)));
        let s_end = s.start + Duration::microseconds((s.duration_ms * 1000.0) as i64);
        end = Some(end.map_or(s_end, |t| t.max(s_end)));
    }

    let mut correlated = vec![];
    let mut nearby_problems = vec![];
    if let (Some(s), Some(e)) = (start, end) {
        let keys = correlation_keys(&events);
        let window = app
            .store
            .untraced_window(s - Duration::seconds(3), e + Duration::seconds(3), 4000)
            .await?;
        for mut ev in window {
            if let Some(hit) = matches_keys(&ev, &keys) {
                ev.correlation = Some(hit);
                correlated.push(ev);
                if correlated.len() >= 400 {
                    break;
                }
            }
        }
        let linked: HashSet<i64> = events.iter().chain(&correlated).map(|e| e.id).collect();
        nearby_problems = app
            .store
            .problems_window(s - Duration::seconds(30), e + Duration::seconds(30), 300)
            .await?
            .into_iter()
            .filter(|p| !linked.contains(&p.id) && p.trace_id.as_deref() != Some(trace_id))
            .take(60)
            .collect();
        with_lifecycle(app, &mut nearby_problems, s, e).await?;
    }

    Ok(assemble(app, Some(trace_id.to_string()), events, correlated, nearby_problems, jaeger_spans, fe))
}

/// For an untraced error: the service's own lines around it plus problems elsewhere.
pub async fn around_event(app: &AppState, anchor: &LogEvent) -> Result<TraceView> {
    let events = app
        .store
        .service_window(&anchor.service, anchor.ts - Duration::seconds(20), anchor.ts + Duration::seconds(5), 150)
        .await?;
    let nearby_problems = app
        .store
        .problems_window(anchor.ts - Duration::seconds(30), anchor.ts + Duration::seconds(30), 300)
        .await?
        .into_iter()
        .filter(|p| p.service != anchor.service)
        .take(60)
        .collect();
    let mut nearby_problems = nearby_problems;
    with_lifecycle(app, &mut nearby_problems, anchor.ts, anchor.ts).await?;
    let mut view = assemble(app, None, events, vec![], nearby_problems, vec![], None);
    view.first_error_id = Some(anchor.id);
    Ok(view)
}

/// Container starts/exits within ±2 min: deploys and crashes explain many cascades.
async fn with_lifecycle(app: &AppState, list: &mut Vec<LogEvent>, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<()> {
    let seen: HashSet<i64> = list.iter().map(|e| e.id).collect();
    let extra = app.store.lifecycle_window(from - Duration::seconds(120), to + Duration::seconds(120), 40).await?;
    list.extend(extra.into_iter().filter(|e| !seen.contains(&e.id)));
    list.sort_by_key(|e| (e.ts, e.id));
    Ok(())
}

fn bounds(events: &[LogEvent]) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    (events.iter().map(|e| e.ts).min(), events.iter().map(|e| e.ts).max())
}

fn correlation_keys(events: &[LogEvent]) -> HashMap<&'static str, HashSet<String>> {
    let mut keys: HashMap<&'static str, HashSet<String>> = HashMap::new();
    for ev in events {
        for key in CORRELATION_KEYS {
            if let Some(v) = ev.field_str(key).filter(|v| v.len() >= 6) {
                keys.entry(key).or_default().insert(v.to_string());
            }
        }
    }
    keys
}

fn matches_keys(ev: &LogEvent, keys: &HashMap<&'static str, HashSet<String>>) -> Option<String> {
    for (key, values) in keys {
        if let Some(v) = ev.field_str(key) {
            if values.contains(v) {
                return Some(format!("{key}={}", crate::redact::truncate(v, 48)));
            }
        }
    }
    // Ids embedded in free text, e.g. "task=6a5424e1-… tenant=… workflow=…".
    for (key, values) in keys {
        if matches!(*key, "resource" | "user_id") {
            continue;
        }
        for v in values {
            if v.len() >= 16 && ev.message.contains(v.as_str()) {
                return Some(format!("{key}={}", crate::redact::truncate(v, 48)));
            }
        }
    }
    None
}

fn assemble(
    app: &AppState,
    trace_id: Option<String>,
    events: Vec<LogEvent>,
    correlated: Vec<LogEvent>,
    nearby_problems: Vec<LogEvent>,
    jaeger_spans: Vec<JaegerSpan>,
    fe: Option<&FrontendError>,
) -> TraceView {
    let catalog = &app.cfg.catalog;
    let mut nodes: BTreeMap<String, ServiceNode> = BTreeMap::new();

    let add = |ev: &LogEvent, via: &'static str, nodes: &mut BTreeMap<String, ServiceNode>| {
        let node = nodes.entry(ev.service.clone()).or_insert_with(|| ServiceNode {
            name: ev.service.clone(),
            layer: catalog.layer_of(&ev.service),
            first_ts: ev.ts,
            last_ts: ev.ts,
            events: 0,
            errors: 0,
            warns: 0,
            status: "ok",
            operations: vec![],
            via,
        });
        node.first_ts = node.first_ts.min(ev.ts);
        node.last_ts = node.last_ts.max(ev.ts);
        node.events += 1;
        match ev.level {
            Level::Error => node.errors += 1,
            Level::Warn => node.warns += 1,
            _ => {}
        }
        if let Some(op) = operation_of(ev) {
            if !node.operations.contains(&op) && node.operations.len() < 6 {
                node.operations.push(op);
            }
        }
    };
    for ev in &events {
        add(ev, "trace", &mut nodes);
    }
    for ev in &correlated {
        add(ev, "correlated", &mut nodes);
    }
    for s in &jaeger_spans {
        let node = nodes.entry(s.service.clone()).or_insert_with(|| ServiceNode {
            name: s.service.clone(),
            layer: catalog.layer_of(&s.service),
            first_ts: s.start,
            last_ts: s.start,
            events: 0,
            errors: 0,
            warns: 0,
            status: "ok",
            operations: vec![],
            via: "jaeger",
        });
        node.first_ts = node.first_ts.min(s.start);
        if s.error {
            node.errors += 1;
        }
        if !node.operations.contains(&s.operation) && node.operations.len() < 6 {
            node.operations.push(s.operation.clone());
        }
    }
    for node in nodes.values_mut() {
        node.status = if node.errors > 0 {
            "error"
        } else if node.warns > 0 {
            "warn"
        } else {
            "ok"
        };
    }

    // Observed edges: URLs and proxy slugs mentioned by a service.
    let mut edges: Vec<Edge> = vec![];
    // Merge repeated sightings of a hop: any failure marks it failed, and a URL
    // path beats the generic `proxy` label.
    let push_edge = |edges: &mut Vec<Edge>, e: Edge| {
        if e.from == e.to {
            return;
        }
        match edges.iter_mut().find(|x| x.from == e.from && x.to == e.to) {
            Some(x) => {
                x.error |= e.error;
                if x.label.as_deref() == Some("proxy") && e.label.as_deref().is_some_and(|l| l.starts_with('/')) {
                    x.label = e.label;
                }
            }
            None => edges.push(e),
        }
    };
    for ev in events.iter().chain(&correlated) {
        let mut text = ev.message.clone();
        if let Value::Object(map) = &ev.fields {
            for v in map.values() {
                if let Value::String(s) = v {
                    text.push(' ');
                    text.push_str(s);
                }
            }
        }
        for cap in URL.captures_iter(&text) {
            if let Some(target) = catalog.service_for_host(&cap[1]) {
                push_edge(
                    &mut edges,
                    Edge {
                        from: ev.service.clone(),
                        to: target,
                        kind: "observed",
                        label: cap.get(2).map(|m| crate::redact::truncate(m.as_str(), 60)),
                        error: ev.level >= Level::Warn,
                    },
                );
            }
        }
        if let Some(target) = ev.field_str("slug").and_then(|s| catalog.service_for_host(s)) {
            push_edge(
                &mut edges,
                Edge { from: ev.service.clone(), to: target, kind: "observed", label: Some("proxy".into()), error: ev.level >= Level::Warn },
            );
        }
    }
    // Jaeger parent → child references.
    let span_service: HashMap<&str, &str> =
        jaeger_spans.iter().map(|s| (s.span_id.as_str(), s.service.as_str())).collect();
    for s in &jaeger_spans {
        if let Some(parent) = s.parent_id.as_deref().and_then(|p| span_service.get(p)) {
            push_edge(
                &mut edges,
                Edge { from: parent.to_string(), to: s.service.clone(), kind: "observed", label: Some(s.operation.clone()), error: s.error },
            );
        }
    }

    // Services that were called (URL/slug/span) but logged nothing for this request.
    for e in edges.iter() {
        if let Some(n) = nodes.get_mut(&e.to).filter(|n| n.via == "referenced") {
            if e.error {
                n.status = "error";
            }
            continue;
        }
        if !nodes.contains_key(&e.to) {
            let ts = nodes.get(&e.from).map(|n| n.first_ts).unwrap_or_else(Utc::now);
            nodes.insert(
                e.to.clone(),
                ServiceNode {
                    name: e.to.clone(),
                    layer: catalog.layer_of(&e.to),
                    first_ts: ts,
                    last_ts: ts,
                    events: 0,
                    errors: 0,
                    warns: 0,
                    // Called, failed, and logged nothing: most likely down or unreachable.
                    status: if e.error { "error" } else { "ok" },
                    operations: e.label.clone().into_iter().collect(),
                    via: "referenced",
                },
            );
        }
    }

    // Inferred edges for services nobody was seen calling.
    let mut ordered: Vec<&ServiceNode> = nodes.values().collect();
    ordered.sort_by_key(|n| n.first_ts);
    for (i, callee) in ordered.iter().enumerate().skip(1) {
        if edges.iter().any(|e| e.to == callee.name) {
            continue;
        }
        let caller = ordered[..i]
            .iter()
            .rev()
            .find(|c| c.last_ts >= callee.first_ts)
            .or_else(|| ordered[..i].last())
            .map(|c| c.name.clone());
        if let Some(caller) = caller {
            push_edge(
                &mut edges,
                Edge { from: caller, to: callee.name.clone(), kind: "inferred", label: None, error: callee.errors > 0 },
            );
        }
    }

    // The browser, when the frontend reported this request.
    if let Some(fe) = fe {
        let entry = ordered.first().map(|n| n.name.clone());
        let ts = ordered.first().map(|n| n.first_ts).unwrap_or_else(Utc::now);
        nodes.insert(
            "browser".into(),
            ServiceNode {
                name: "browser".into(),
                layer: "frontend".into(),
                first_ts: ts,
                last_ts: ts,
                events: 1,
                errors: fe.status.is_some_and(|s| s >= 500) as usize,
                warns: fe.status.is_some_and(|s| (400..500).contains(&s)) as usize,
                status: match fe.status {
                    Some(s) if s >= 500 => "error",
                    Some(s) if s >= 400 => "warn",
                    _ => "ok",
                },
                operations: vec![format!(
                    "{} {}",
                    fe.method.as_deref().unwrap_or("GET"),
                    fe.url.as_deref().map(path_only).unwrap_or_default()
                )],
                via: "frontend",
            },
        );
        if let Some(entry) = entry {
            edges.insert(
                0,
                Edge {
                    from: "browser".into(),
                    to: entry,
                    kind: "observed",
                    label: fe.status.map(|s| format!("HTTP {s}")),
                    error: fe.status.is_some_and(|s| s >= 500),
                },
            );
        }
    }

    let traced_services: HashSet<&str> = events.iter().map(|e| e.service.as_str()).collect();
    let mut gaps: Vec<String> = correlated
        .iter()
        .map(|e| e.service.as_str())
        .filter(|s| !traced_services.contains(s))
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|s| format!("{s} handled part of this request but logged no trace_id — traceparent is not propagated into or inside {s}."))
        .collect();
    gaps.sort();

    let first_error_id = events
        .iter()
        .chain(&correlated)
        .filter(|e| e.level == Level::Error)
        .min_by_key(|e| e.ts)
        .or_else(|| events.iter().chain(&correlated).filter(|e| e.level == Level::Warn).min_by_key(|e| e.ts))
        .map(|e| e.id);

    let (start, end) = {
        let all: Vec<DateTime<Utc>> = events
            .iter()
            .chain(&correlated)
            .map(|e| e.ts)
            .chain(jaeger_spans.iter().map(|s| s.start))
            .collect();
        (all.iter().min().copied(), all.iter().max().copied())
    };
    let mut correlated = correlated;
    correlated.sort_by_key(|e| (e.ts, e.id));

    let mut services: Vec<ServiceNode> = nodes.into_values().collect();
    services.sort_by_key(|n| (n.name != "browser", n.first_ts));

    TraceView {
        trace_id,
        start,
        end,
        duration_ms: match (start, end) {
            (Some(s), Some(e)) => (e - s).num_milliseconds(),
            _ => 0,
        },
        services,
        edges,
        events,
        correlated,
        nearby_problems,
        first_error_id,
        gaps,
        jaeger_spans,
    }
}

fn operation_of(ev: &LogEvent) -> Option<String> {
    if let (Some(m), Some(p)) = (ev.field_str("method"), ev.field_str("path")) {
        return Some(format!("{m} {p}"));
    }
    if let Some(wf) = ev.field_str("workflow") {
        return Some(format!("workflow {wf}"));
    }
    if let Some(action) = ev.field_str("action") {
        return Some(action.to_string());
    }
    ev.spans.iter().rev().find(|s| s.as_str() != "request").cloned()
}

pub fn path_only(url: &str) -> String {
    let no_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = match no_scheme.find('/') {
        Some(i) if url.contains("://") => &no_scheme[i..],
        _ => no_scheme,
    };
    path.split('?').next().unwrap_or(path).to_string()
}

async fn fetch_jaeger(base: &str, trace_id: &str) -> Result<Vec<JaegerSpan>> {
    let body: Value = reqwest::Client::new()
        .get(format!("{base}/api/traces/{trace_id}"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(trace) = body["data"].as_array().and_then(|d| d.first()) else {
        return Ok(vec![]);
    };
    let processes = &trace["processes"];
    let mut out = vec![];
    for span in trace["spans"].as_array().into_iter().flatten() {
        let service = processes[span["processID"].as_str().unwrap_or_default()]["serviceName"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        let start_us = span["startTime"].as_i64().unwrap_or_default();
        let error = span["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["key"] == "error" && t["value"] == true || t["key"] == "otel.status_code" && t["value"] == "ERROR");
        out.push(JaegerSpan {
            span_id: span["spanID"].as_str().unwrap_or_default().to_string(),
            parent_id: span["references"]
                .as_array()
                .and_then(|r| r.first())
                .and_then(|r| r["spanID"].as_str())
                .map(str::to_string),
            service,
            operation: span["operationName"].as_str().unwrap_or_default().to_string(),
            start: DateTime::from_timestamp_micros(start_us).unwrap_or_else(Utc::now),
            duration_ms: span["duration"].as_f64().unwrap_or_default() / 1000.0,
            error,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_only_strips_host_and_query() {
        assert_eq!(path_only("https://x.regere.ai/api/v1/package/siemens/projects?limit=1"), "/api/v1/package/siemens/projects");
        assert_eq!(path_only("/proxy/core/api?x=1"), "/proxy/core/api");
    }

    #[test]
    fn url_regex_finds_upstream_host() {
        let caps = URL
            .captures("service proxy upstream error (message-flow): error sending request for url (http://pf-message-flow:8080/client/v1/inbox/unread-count)")
            .unwrap();
        assert_eq!(&caps[1], "pf-message-flow");
        assert_eq!(&caps[2], "/client/v1/inbox/unread-count");
    }
}
