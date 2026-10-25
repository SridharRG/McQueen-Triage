//! Detection → incident → (debounced) triage.
//!
//! - The **detector** watches every stored event. Problems with a trace id open
//!   one incident per trace (`trace:<id>`); untraced problems are grouped by a
//!   normalised fingerprint (`fp:<service>:<target>:<message shape>`).
//! - The **scheduler** waits for a trace to go quiet (`TRIAGE_SETTLE_SECS`, capped
//!   at 30s) so the whole cascade is in the store before context is assembled.
//! - The **worker** assembles context, runs the agent and notifies. Auto triage
//!   is rate-limited (`TRIAGE_MAX_PER_HOUR`); manual triage from the UI is not.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{atomic::Ordering, Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use regex::Regex;
use tokio::sync::{broadcast::error::RecvError, mpsc, Semaphore};

use crate::{
    agent, context,
    model::{FrontendError, Incident, Level, LogEvent, UiEvent, TRIAGE_DONE, TRIAGE_FAILED, TRIAGE_PENDING, TRIAGE_RUNNING, TRIAGE_SKIPPED},
    notify, parse,
    store::NewIncident,
    AppState,
};

const MAX_SETTLE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct Pending {
    first: Instant,
    due: Instant,
    auto: bool,
}

pub struct Triage {
    queue: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<String, Pending>>,
    recent_auto: Mutex<VecDeque<Instant>>,
    in_flight: Mutex<HashSet<String>>,
}

impl Triage {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                queue: tx,
                pending: Mutex::new(HashMap::new()),
                recent_auto: Mutex::new(VecDeque::new()),
                in_flight: Mutex::new(HashSet::new()),
            },
            rx,
        )
    }

    /// Debounced: each new problem in the same incident pushes `due` out by `settle`.
    pub fn schedule(&self, id: &str, settle: Duration, auto: bool) {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        let entry = pending.entry(id.to_string()).or_insert(Pending { first: now, due: now + settle, auto });
        entry.due = (now + settle).min(entry.first + MAX_SETTLE);
        entry.auto &= auto;
    }

    /// Run now, bypassing debounce and budget.
    pub fn enqueue_now(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
        let _ = self.queue.send(id.to_string());
    }

    pub fn budget_left(&self, max_per_hour: u32) -> u32 {
        let mut recent = self.recent_auto.lock().unwrap();
        while recent.front().is_some_and(|t| t.elapsed() > Duration::from_secs(3600)) {
            recent.pop_front();
        }
        max_per_hour.saturating_sub(recent.len() as u32)
    }

    fn take_budget(&self, max_per_hour: u32) -> bool {
        if self.budget_left(max_per_hour) == 0 {
            return false;
        }
        self.recent_auto.lock().unwrap().push_back(Instant::now());
        true
    }

    fn due(&self) -> Vec<(String, bool)> {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        let ready: Vec<(String, bool)> =
            pending.iter().filter(|(_, p)| p.due <= now).map(|(id, p)| (id.clone(), p.auto)).collect();
        for (id, _) in &ready {
            pending.remove(id);
        }
        ready
    }

    pub fn queued(&self) -> usize {
        self.pending.lock().unwrap().len() + self.in_flight.lock().unwrap().len()
    }
}

// ── detector ─────────────────────────────────────────────────────────────────

pub async fn detector(app: Arc<AppState>) {
    let mut rx = app.live.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if let Err(e) = on_event(&app, &ev).await {
                    tracing::warn!(error = %e, "detector failed on event");
                }
            }
            Err(RecvError::Lagged(n)) => tracing::warn!(skipped = n, "detector lagging behind ingestion"),
            Err(RecvError::Closed) => return,
        }
    }
}

/// Is this line a problem worth an incident?
pub fn is_problem(app: &AppState, ev: &LogEvent) -> bool {
    let rules = &app.cfg.catalog.detection;
    if app.cfg.catalog.layer_of(&ev.service) == "infra" {
        return false;
    }
    let lower = ev.message.to_ascii_lowercase();
    if rules.ignore.iter().any(|p| lower.contains(&p.to_ascii_lowercase())) {
        return false;
    }
    match ev.level {
        Level::Error => true,
        Level::Warn => {
            rules.warn_as_error.iter().any(|p| lower.contains(&p.to_ascii_lowercase()))
                || ev.http_status().is_some_and(|s| s >= 500)
        }
        _ => ev.http_status().is_some_and(|s| s >= 500) && lower.contains("response"),
    }
}

async fn on_event(app: &Arc<AppState>, ev: &LogEvent) -> Result<()> {
    if !is_problem(app, ev) {
        return Ok(());
    }
    app.metrics.problems.fetch_add(1, Ordering::Relaxed);
    let title = format!("{}: {}", ev.service, first_line(&ev.message));
    let (fingerprint, trace_id) = match &ev.trace_id {
        Some(t) => (format!("trace:{t}"), Some(t.as_str())),
        None => (fingerprint(ev), None),
    };
    let (inc, created) = app
        .store
        .upsert_incident(NewIncident {
            fingerprint: &fingerprint,
            trace_id,
            source: "backend",
            service: Some(&ev.service),
            anchor_log_id: Some(ev.id),
            title: &title,
            level: ev.level.as_str(),
            seen: ev.ts,
        })
        .await?;
    if created {
        app.metrics.incidents.fetch_add(1, Ordering::Relaxed);
    }
    // New incidents and recurrences of resolved ones are pending; a trace that keeps
    // failing pushes its triage out until the cascade settles.
    if inc.triage_state == TRIAGE_PENDING {
        let settle = if trace_id.is_some() { app.cfg.triage.settle } else { Duration::from_secs(2) };
        app.triage.schedule(&inc.id, settle, true);
    }
    let _ = app.ui.send(UiEvent::Incident { id: inc.id, triage_state: inc.triage_state, status: inc.status });
    Ok(())
}

fn first_line(s: &str) -> String {
    crate::redact::truncate(s.lines().next().unwrap_or_default().trim(), 180)
}

static VOLATILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|\b[0-9a-f]{16,}\b|\d+(\.\d+)?|"[^"]{0,80}"|'[^']{0,80}'"#).unwrap()
});

/// Shape of an untraced message: ids, numbers and quoted values replaced.
pub fn fingerprint(ev: &LogEvent) -> String {
    let shape = VOLATILE.replace_all(&first_line(&ev.message), "_").to_string();
    format!("fp:{}:{}:{}", ev.service, ev.target.as_deref().unwrap_or(""), crate::redact::truncate(&shape, 140))
}

/// The browser SDK reported a failed request.
pub async fn frontend_error(app: &Arc<AppState>, fe: FrontendError) -> Result<Incident> {
    let trace_id = parse::trace_id_from_input(&fe.trace_id).ok_or_else(|| anyhow::anyhow!("invalid trace id"))?;
    let title = match (&fe.ui_message, fe.status) {
        (Some(msg), None) => format!("UI error on {}: {}", fe.page.as_deref().map(crate::trace::path_only).unwrap_or_default(), first_line(msg)),
        _ => format!(
            "{} {} → {}",
            fe.method.as_deref().unwrap_or("GET"),
            fe.url.as_deref().map(crate::trace::path_only).unwrap_or_else(|| "request".into()),
            fe.status.map(|s| format!("HTTP {s}")).unwrap_or_else(|| "network error".into())
        ),
    };
    let (inc, created) = app
        .store
        .upsert_incident(NewIncident {
            fingerprint: &format!("trace:{trace_id}"),
            trace_id: Some(&trace_id),
            source: "frontend",
            service: None,
            anchor_log_id: None,
            title: &title,
            level: if fe.status.is_some_and(|s| s < 500) { "WARN" } else { "ERROR" },
            seen: chrono::Utc::now(),
        })
        .await?;
    if created {
        app.metrics.incidents.fetch_add(1, Ordering::Relaxed);
    }
    let had_fe = inc.fe_payload.is_some();
    let mut fe = fe;
    fe.trace_id = trace_id;
    app.store.set_fe_payload(&inc.id, &serde_json::to_value(&fe)?).await?;
    // Re-triage a backend-only report now that we know what the user saw.
    if inc.triage_state == TRIAGE_PENDING || (!had_fe && inc.triage_state != TRIAGE_RUNNING) {
        app.store.set_triage_state(&inc.id, TRIAGE_PENDING, None).await?;
        app.triage.schedule(&inc.id, Duration::from_secs(5), true);
    }
    let _ = app.ui.send(UiEvent::Incident { id: inc.id.clone(), triage_state: TRIAGE_PENDING.into(), status: inc.status.clone() });
    Ok(app.store.incident(&inc.id).await?.unwrap_or(inc))
}

/// Manual "triage this trace" from the UI or CLI.
pub async fn manual_trace(app: &Arc<AppState>, trace_id: &str) -> Result<Incident> {
    let events = app.store.logs_by_trace(trace_id, 3000).await?;
    let worst = events.iter().max_by_key(|e| (e.level, std::cmp::Reverse(e.ts)));
    let title = match worst {
        Some(e) if e.level >= Level::Warn => format!("{}: {}", e.service, first_line(&e.message)),
        Some(e) => format!("trace through {} ({} lines, no errors logged)", e.service, events.len()),
        None => format!("trace {trace_id} (no logs found)"),
    };
    let (inc, created) = app
        .store
        .upsert_incident(NewIncident {
            fingerprint: &format!("trace:{trace_id}"),
            trace_id: Some(trace_id),
            source: "manual",
            service: worst.map(|e| e.service.as_str()),
            anchor_log_id: worst.map(|e| e.id),
            title: &title,
            level: worst.map(|e| e.level.as_str()).unwrap_or("INFO"),
            seen: worst.map(|e| e.ts).unwrap_or_else(chrono::Utc::now),
        })
        .await?;
    if created {
        app.metrics.incidents.fetch_add(1, Ordering::Relaxed);
    }
    app.store.set_triage_state(&inc.id, TRIAGE_PENDING, None).await?;
    app.triage.enqueue_now(&inc.id);
    Ok(inc)
}

// ── scheduler + worker ───────────────────────────────────────────────────────

pub async fn scheduler(app: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        for (id, auto) in app.triage.due() {
            if auto && !app.cfg.triage.auto {
                let _ = app.store.set_triage_state(&id, TRIAGE_SKIPPED, Some("auto-triage is disabled (AUTO_TRIAGE=false); run it from the UI")).await;
                continue;
            }
            if auto && !app.triage.take_budget(app.cfg.triage.max_per_hour) {
                let _ = app
                    .store
                    .set_triage_state(&id, TRIAGE_SKIPPED, Some("hourly auto-triage budget used up (TRIAGE_MAX_PER_HOUR); run it from the UI"))
                    .await;
                continue;
            }
            let _ = app.triage.queue.send(id);
        }
    }
}

pub async fn worker(app: Arc<AppState>, mut rx: mpsc::UnboundedReceiver<String>) {
    let sem = Arc::new(Semaphore::new(app.cfg.triage.concurrency));
    while let Some(id) = rx.recv().await {
        if !app.triage.in_flight.lock().unwrap().insert(id.clone()) {
            continue;
        }
        let permit = sem.clone().acquire_owned().await.expect("semaphore open");
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = run(&app, &id).await {
                tracing::warn!(incident = %id, error = %e, "triage failed");
                app.metrics.triage_failed.fetch_add(1, Ordering::Relaxed);
                let _ = app.store.set_triage_state(&id, TRIAGE_FAILED, Some(&e.to_string())).await;
            }
            app.triage.in_flight.lock().unwrap().remove(&id);
            if let Ok(Some(inc)) = app.store.incident(&id).await {
                let _ = app.ui.send(UiEvent::Incident { id: inc.id, triage_state: inc.triage_state, status: inc.status });
            }
            drop(permit);
        });
    }
}

/// One full triage: context → agent → store → notify.
pub async fn run(app: &Arc<AppState>, id: &str) -> Result<()> {
    let Some(incident) = app.store.incident(id).await? else { return Ok(()) };
    app.store.set_triage_state(id, TRIAGE_RUNNING, None).await?;
    let _ = app.ui.send(UiEvent::Incident { id: id.into(), triage_state: TRIAGE_RUNNING.into(), status: incident.status.clone() });

    let ctx = context::assemble(app, &incident).await?;
    let snapshot = ctx.snapshot();
    app.store.save_context(id, &snapshot).await?;
    app.metrics.contexts.fetch_add(1, Ordering::Relaxed);
    tracing::info!(incident = id, prompt_chars = ctx.prompt_chars, snippets = ctx.snippets.len(), tables = ctx.tables.len(), "context assembled");

    let outcome = agent::run(app, &ctx).await?;
    let report = serde_json::to_value(&outcome.report)?;
    let origin = Some(outcome.report.origin_service.as_str()).filter(|s| ctx.trace.services.iter().any(|n| n.name == *s));
    app.store
        .save_triage(id, &report, &snapshot, &outcome.usage, Some(&outcome.report.severity), origin)
        .await?;
    app.metrics.triaged.fetch_add(1, Ordering::Relaxed);
    if let Some(cost) = outcome.usage["cost_usd"].as_f64() {
        app.metrics.llm_cost_micro_usd.fetch_add((cost * 1e6) as u64, Ordering::Relaxed);
    }
    if let Some(t) = outcome.usage["total_tokens"].as_u64() {
        app.metrics.llm_tokens.fetch_add(t, Ordering::Relaxed);
    }
    tracing::info!(incident = id, severity = %outcome.report.severity, origin = %outcome.report.origin_service, "triage done");

    if let Some(inc) = app.store.incident(id).await? {
        debug_assert_eq!(inc.triage_state, TRIAGE_DONE);
        if notify::send(app, &inc, &outcome.report).await? {
            app.metrics.notified.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn fingerprint_ignores_ids_and_numbers() {
        let mk = |msg: &str| LogEvent {
            id: 1,
            ts: Utc::now(),
            service: "core".into(),
            container: "dev_core".into(),
            level: Level::Error,
            target: Some("architect_sdk::authrs".into()),
            message: msg.into(),
            trace_id: None,
            fields: serde_json::json!({}),
            spans: vec![],
            correlation: None,
        };
        let a = fingerprint(&mk("task 6a5424e1-25f8-42d8-bfeb-1f6f029ecd04 failed after 3 attempts"));
        let b = fingerprint(&mk("task 0f0f0f0f-25f8-42d8-bfeb-1f6f029ecd04 failed after 5 attempts"));
        assert_eq!(a, b);
        assert_ne!(a, fingerprint(&mk("something else entirely")));
    }

    #[test]
    fn debounce_caps_at_max_settle() {
        let (t, _rx) = Triage::new();
        t.schedule("a", Duration::from_millis(0), true);
        assert_eq!(t.due().len(), 1);
        t.schedule("b", Duration::from_secs(60), true);
        assert!(t.due().is_empty());
        assert!(t.pending.lock().unwrap()["b"].due <= Instant::now() + MAX_SETTLE);
    }

    #[test]
    fn budget_window() {
        let (t, _rx) = Triage::new();
        assert!(t.take_budget(2));
        assert!(t.take_budget(2));
        assert!(!t.take_budget(2));
        assert_eq!(t.budget_left(2), 0);
    }
}
