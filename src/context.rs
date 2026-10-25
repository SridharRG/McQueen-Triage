//! Assembles everything the model sees for one incident, within a character
//! budget, and keeps a snapshot of it for the UI ("what did the AI look at?").

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::{
    code::Snippet,
    model::{FrontendError, Incident, Level, LogEvent},
    redact::{scrub, truncate},
    trace::{self, TraceView},
    AppState,
};

#[derive(Debug, Clone, Serialize)]
pub struct TriageContext {
    pub incident_id: String,
    pub trace: TraceView,
    pub snippets: Vec<Snippet>,
    pub tables: Vec<String>,
    pub schema_ddl: String,
    pub frontend: Option<FrontendError>,
    /// The exact user prompt sent to the model (already redacted).
    pub prompt: String,
    pub prompt_chars: usize,
    pub sections_truncated: Vec<String>,
}

impl TriageContext {
    /// Snapshot stored with the incident; trims the event lists so a chatty trace
    /// doesn't bloat the row (the full trace stays queryable until log retention).
    pub fn snapshot(&self) -> Value {
        let mut trace = self.trace.clone();
        trace.events = keep_important(&trace.events, 400);
        trace.correlated = keep_important(&trace.correlated, 200);
        serde_json::json!({
            "trace": trace,
            "snippets": self.snippets,
            "tables": self.tables,
            "schema_ddl": self.schema_ddl,
            "frontend": self.frontend,
            "prompt": self.prompt,
            "prompt_chars": self.prompt_chars,
            "sections_truncated": self.sections_truncated,
        })
    }
}

fn keep_important(events: &[LogEvent], max: usize) -> Vec<LogEvent> {
    if events.len() <= max {
        return events.to_vec();
    }
    let mut keep: Vec<&LogEvent> = events.iter().filter(|e| e.level >= Level::Warn).take(max / 2).collect();
    let rest = max - keep.len();
    keep.extend(events.iter().filter(|e| e.level < Level::Warn).take(rest));
    keep.sort_by_key(|e| (e.ts, e.id));
    keep.into_iter().cloned().collect()
}

pub async fn assemble(app: &Arc<AppState>, incident: &Incident) -> Result<TriageContext> {
    let fe: Option<FrontendError> = incident.fe_payload.clone().and_then(|v| serde_json::from_value(v).ok());
    let trace = match (&incident.trace_id, incident.anchor_log_id) {
        (Some(tid), _) => trace::build(app, tid, fe.as_ref()).await?,
        (None, Some(anchor)) => match app.store.log_by_id(anchor).await? {
            Some(ev) => trace::around_event(app, &ev).await?,
            None => anyhow::bail!("the log line behind this incident has been pruned"),
        },
        (None, None) => anyhow::bail!("incident has neither a trace id nor an anchor log line"),
    };

    let budget = app.cfg.llm.context_budget_chars.max(4000);
    let mut truncated = vec![];

    // ── code: where the problems were logged ────────────────────────────────
    let code = app.code();
    let mut snippets: Vec<Snippet> = vec![];
    let mut seen_loc = HashSet::new();
    let mut problem_events: Vec<&LogEvent> =
        trace.events.iter().chain(&trace.correlated).filter(|e| e.level >= Level::Warn).collect();
    problem_events.sort_by_key(|e| (std::cmp::Reverse(e.level), e.ts));
    if let Some(anchor) = trace.first_error_id.and_then(|id| trace.events.iter().find(|e| e.id == id)) {
        problem_events.insert(0, anchor);
    }
    for ev in problem_events {
        let repo = app.cfg.catalog.service(&ev.service).and_then(|s| s.repo.as_deref());
        if let Some(loc) = code.locate(ev, repo) {
            if seen_loc.insert((loc.repo.clone(), loc.file.clone(), loc.line / 8)) {
                if let Some(s) = code.snippet(&loc, 12, ev.id) {
                    snippets.push(s);
                }
            }
        }
        if snippets.len() >= 3 {
            break;
        }
    }

    // ── schema: tables the incident text points at ──────────────────────────
    let schema = app.schema();
    let mut schema_text = incident.title.clone();
    for ev in trace.events.iter().chain(&trace.correlated).filter(|e| e.level >= Level::Warn || e.fields.get("sql").is_some()) {
        schema_text.push('\n');
        schema_text.push_str(&ev.message);
        schema_text.push(' ');
        schema_text.push_str(&ev.fields.to_string());
    }
    if let Some(fe) = &fe {
        schema_text.push_str(&format!(" {} {}", fe.url.as_deref().unwrap_or(""), fe.response_body.as_deref().unwrap_or("")));
    }
    let tables = schema.relevant(&schema_text, 7);
    let table_names: Vec<String> = tables.iter().map(|t| t.qualified()).collect();
    let mut schema_ddl = schema.render(&tables);

    // ── prompt ──────────────────────────────────────────────────────────────
    let mut p = String::new();
    p.push_str(&format!(
        "## Incident {}\nsource: {} · first seen {} · occurrences {}\ntitle: {}\n",
        incident.id,
        incident.source,
        incident.first_seen.format("%Y-%m-%d %H:%M:%S UTC"),
        incident.occurrences,
        scrub(&incident.title)
    ));

    if let Some(fe) = &fe {
        p.push_str("\n## What the frontend saw\n");
        p.push_str(&format!(
            "{} {} → HTTP {}{}\n",
            fe.method.as_deref().unwrap_or("GET"),
            fe.url.as_deref().map(trace::path_only).unwrap_or_default(),
            fe.status.map(|s| s.to_string()).unwrap_or_else(|| "network error".into()),
            fe.duration_ms.map(|d| format!(" after {d:.0} ms")).unwrap_or_default()
        ));
        if let Some(page) = &fe.page {
            p.push_str(&format!("page: {}\n", trace::path_only(page)));
        }
        if let Some(m) = &fe.ui_message {
            p.push_str(&format!("UI error: {}\n", truncate(&scrub(m), 300)));
        }
        if let Some(b) = &fe.response_body {
            p.push_str(&format!("response body: {}\n", truncate(&scrub(b), 700)));
        }
    }

    p.push_str(&format!(
        "\n## Request journey{}\n",
        trace.trace_id.as_ref().map(|t| format!(" (trace {t}, {} ms)", trace.duration_ms)).unwrap_or_else(|| " (untraced: lines around the error)".into())
    ));
    for e in &trace.edges {
        p.push_str(&format!(
            "{} → {} [{}{}]{}\n",
            e.from,
            e.to,
            e.kind,
            if e.error { ", failed" } else { "" },
            e.label.as_ref().map(|l| format!(" {l}")).unwrap_or_default()
        ));
    }
    for s in &trace.services {
        p.push_str(&format!(
            "- {} ({}, via {}): {} lines, {} errors, {} warns{}\n",
            s.name,
            s.layer,
            s.via,
            s.events,
            s.errors,
            s.warns,
            if s.operations.is_empty() { String::new() } else { format!("; ops: {}", s.operations.join(", ")) }
        ));
    }
    for g in &trace.gaps {
        p.push_str(&format!("! {g}\n"));
    }

    let log_budget = budget * 45 / 100;
    let (log_lines, cut) = select_lines(&trace, log_budget);
    p.push_str("\n## Log lines (chronological; ~ = linked by shared key, not trace id)\n");
    p.push_str(&log_lines);
    if cut {
        truncated.push("logs".into());
    }

    if !trace.nearby_problems.is_empty() {
        p.push_str("\n## Other warnings/errors within ±30s and container starts/exits within ±2min (NOT linked to this request)\n");
        let (lifecycle, problems): (Vec<&LogEvent>, Vec<&LogEvent>) =
            trace.nearby_problems.iter().partition(|e| e.target.as_deref() == Some("docker"));
        for ev in lifecycle.iter().take(12).chain(problems.iter().take(8)) {
            p.push_str(&truncate(&scrub(&ev.prompt_line(4)), 300));
            p.push('\n');
        }
    }

    let code_budget = budget * 30 / 100;
    if !snippets.is_empty() {
        p.push_str("\n## Source at the logging sites (→ marks the line)\n");
        let mut used = 0;
        for s in &snippets {
            let block = format!(
                "### {}/{}:{} (found via {}, repo commit {})\n```\n{}\n```\n",
                s.location.repo,
                s.location.file,
                s.location.line,
                s.location.via,
                s.commit.as_deref().unwrap_or("?"),
                scrub(&s.code)
            );
            if used + block.len() > code_budget && used > 0 {
                truncated.push("code".into());
                break;
            }
            used += block.len();
            p.push_str(&block);
        }
    }

    let schema_budget = budget * 20 / 100;
    if !schema_ddl.is_empty() {
        if schema_ddl.len() > schema_budget {
            schema_ddl = truncate(&schema_ddl, schema_budget);
            truncated.push("schema".into());
        }
        p.push_str("\n## Database schema of the tables involved (from architect-sdk packages)\n```\n");
        p.push_str(&schema_ddl);
        p.push_str("\n```\n");
    }

    p.push_str(&format!(
        "\n## Repos you can read with tools\n{}\n",
        code.repos.iter().map(|r| format!("{}@{}", r.name, r.commit.as_deref().unwrap_or("?"))).collect::<Vec<_>>().join(", ")
    ));

    let prompt = scrub(&p);
    let prompt_chars = prompt.len();
    Ok(TriageContext {
        incident_id: incident.id.clone(),
        trace,
        snippets,
        tables: table_names,
        schema_ddl,
        frontend: fe,
        prompt,
        prompt_chars,
        sections_truncated: truncated,
    })
}

/// Pick log lines for the prompt: every WARN/ERROR, the first and last few
/// lines, and a little context around each problem; trimmed to `budget` chars.
fn select_lines(trace: &TraceView, budget: usize) -> (String, bool) {
    let mut all: Vec<&LogEvent> = trace.events.iter().chain(&trace.correlated).collect();
    all.sort_by_key(|e| (e.ts, e.id));
    let n = all.len();
    let mut keep = vec![false; n];
    for (i, e) in all.iter().enumerate() {
        if e.level >= Level::Warn {
            for k in &mut keep[i.saturating_sub(2)..(i + 2).min(n)] {
                *k = true;
            }
        }
    }
    for k in keep.iter_mut().take(4) {
        *k = true;
    }
    for k in keep.iter_mut().skip(n.saturating_sub(3)) {
        *k = true;
    }

    let mut out = String::new();
    let mut cut = false;
    let mut skipped = 0;
    let mut lines: Vec<String> = vec![];
    for (i, e) in all.iter().enumerate() {
        if !keep[i] {
            skipped += 1;
            continue;
        }
        if skipped > 0 {
            lines.push(format!("… {skipped} routine lines omitted"));
            skipped = 0;
        }
        let marker = if e.correlation.is_some() { "~" } else { " " };
        lines.push(format!("{marker}{}", truncate(&scrub(&e.prompt_line(6)), 420)));
    }
    if skipped > 0 {
        lines.push(format!("… {skipped} routine lines omitted"));
    }
    // Over budget: drop non-problem lines first, from the middle out.
    let total = |ls: &[String]| ls.iter().map(|l| l.len() + 1).sum::<usize>();
    while total(&lines) > budget && lines.len() > 1 {
        cut = true;
        let pos = lines
            .iter()
            .position(|l| !(l.contains(" ERROR ") || l.contains(" WARN ")) && !l.starts_with('…'))
            .unwrap_or(lines.len() / 2);
        lines.remove(pos);
    }
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }
    if all.is_empty() {
        out.push_str("(no log lines found for this trace id — the request may not have reached the backend, or logs were pruned)\n");
    }
    (out, cut)
}
