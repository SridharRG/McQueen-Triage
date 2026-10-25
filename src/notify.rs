//! Google Chat / Slack incoming-webhook alerts for triaged incidents at or above
//! `NOTIFY_MIN_SEVERITY`. Text is redacted and trimmed; the link goes to the UI.

use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use crate::{
    agent::{Severity, TriageReport},
    model::Incident,
    redact::{scrub, truncate},
    AppState,
};

/// Returns whether anything was sent.
pub async fn send(app: &Arc<AppState>, inc: &Incident, report: &TriageReport) -> Result<bool> {
    let cfg = &app.cfg;
    if cfg.google_chat_webhook.is_none() && cfg.slack_webhook.is_none() {
        return Ok(false);
    }
    if Severity::rank(&report.severity) < Severity::rank(&cfg.notify_min_severity) {
        return Ok(false);
    }
    let url = format!("{}/incidents/{}", cfg.public_url, inc.id);
    let icon = match report.severity.as_str() {
        "critical" => "🔴",
        "high" => "🟠",
        "medium" => "🟡",
        _ => "🔵",
    };
    let chain = report
        .failure_chain
        .iter()
        .map(|s| s.service.as_str())
        .collect::<Vec<_>>()
        .join(" → ");
    let body = |bold: &str, link: String| {
        scrub(&format!(
            "{icon} {b}[{sev}] {id} · {title}{b}\n\
             Origin: {origin}{chain}{trace}\n\
             For frontend: {fe}\n\
             Root cause: {rc}\n\
             Fix: {fix}\n\
             {link}",
            b = bold,
            sev = report.severity.to_uppercase(),
            id = inc.id,
            title = truncate(&report.title, 120),
            origin = report.origin_service,
            chain = if chain.is_empty() { String::new() } else { format!(" · Path: {chain}") },
            trace = inc.trace_id.as_ref().map(|t| format!(" · Trace: {t}")).unwrap_or_default(),
            fe = truncate(&report.frontend_summary, 300),
            rc = truncate(&report.root_cause, 400),
            fix = truncate(&report.suggested_fix, 300),
        ))
    };

    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
    let mut sent = false;
    if let Some(hook) = &cfg.google_chat_webhook {
        let text = body("*", format!("<{url}|Open in McQueen>"));
        match client.post(hook).json(&json!({ "text": text })).send().await {
            Ok(r) if r.status().is_success() => sent = true,
            Ok(r) => tracing::warn!(status = %r.status(), "google chat webhook rejected the alert"),
            Err(e) => tracing::warn!(error = %e, "google chat webhook failed"),
        }
    }
    if let Some(hook) = &cfg.slack_webhook {
        let text = body("*", format!("<{url}|Open in McQueen>"));
        match client.post(hook).json(&json!({ "text": text })).send().await {
            Ok(r) if r.status().is_success() => sent = true,
            Ok(r) => tracing::warn!(status = %r.status(), "slack webhook rejected the alert"),
            Err(e) => tracing::warn!(error = %e, "slack webhook failed"),
        }
    }
    Ok(sent)
}
