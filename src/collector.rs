//! Docker log collection over the Engine API (bollard).
//!
//! One task per watched container follows its log stream with `timestamps=true`.
//! A per-container cursor (last Docker timestamp seen) is persisted so a restart
//! of McQueen resumes where it left off instead of re-ingesting or dropping
//! lines. Containers are (re)discovered from Docker events plus a periodic
//! reconcile, so restarts and redeploys of the watched stacks are picked up.
//!
//! Only read endpoints are used (`/containers/json`, `/containers/{id}/logs`,
//! `/events`); the compose file puts a socket proxy in front that refuses
//! everything else.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use anyhow::Result;
use bollard::{
    container::LogOutput,
    query_parameters::{EventsOptions, ListContainersOptions, LogsOptions},
    Docker,
};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::{
    sync::{mpsc, Notify, RwLock},
    task::JoinHandle,
};

use crate::{
    model::LogEvent,
    parse::{self, Aggregator},
    AppState,
};

#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub service: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub project: Option<String>,
    pub watched: bool,
}

#[derive(Default)]
pub struct Registry {
    pub containers: RwLock<HashMap<String, ContainerInfo>>,
    pub docker_ok: std::sync::atomic::AtomicBool,
    pub docker_error: RwLock<Option<String>>,
}

pub async fn run(app: Arc<AppState>, tx: mpsc::Sender<LogEvent>) {
    loop {
        match Docker::connect_with_defaults() {
            Ok(docker) => {
                if let Err(e) = watch(app.clone(), docker, tx.clone()).await {
                    tracing::warn!(error = %e, "docker watcher stopped; retrying");
                    *app.registry.docker_error.write().await = Some(e.to_string());
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot connect to docker; retrying");
                *app.registry.docker_error.write().await = Some(e.to_string());
            }
        }
        app.registry.docker_ok.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn watch(app: Arc<AppState>, docker: Docker, tx: mpsc::Sender<LogEvent>) -> Result<()> {
    docker.ping().await?;
    app.registry.docker_ok.store(true, Ordering::Relaxed);
    *app.registry.docker_error.write().await = None;
    tracing::info!("connected to docker");

    let wake = Arc::new(Notify::new());
    // Container lifecycle events trigger an immediate reconcile, and are stored as
    // log events so triage can see that a dependency was restarting, died or OOMed.
    {
        let docker = docker.clone();
        let wake = wake.clone();
        let app = app.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let filters = HashMap::from([
                ("type".to_string(), vec!["container".to_string()]),
                (
                    "event".to_string(),
                    ["start", "die", "destroy", "oom", "kill", "health_status"].map(String::from).to_vec(),
                ),
            ]);
            let mut events = docker.events(Some(EventsOptions { filters: Some(filters), ..Default::default() }));
            while let Some(ev) = events.next().await {
                let Ok(ev) = ev else { break };
                if let Some(lifecycle) = lifecycle_event(&app, &ev) {
                    let _ = tx.send(lifecycle).await;
                }
                if matches!(ev.action.as_deref(), Some("start" | "die" | "destroy")) {
                    // Give compose a moment to finish wiring the container.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    wake.notify_one();
                }
            }
        });
    }

    let mut tails: HashMap<String, JoinHandle<()>> = HashMap::new();
    loop {
        reconcile(&app, &docker, &tx, &mut tails).await?;
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
    }
}

/// A Docker container event as a log line of the service (`target: docker`).
fn lifecycle_event(app: &AppState, ev: &bollard::models::EventMessage) -> Option<LogEvent> {
    let action = ev.action.as_deref()?;
    let attrs = ev.actor.as_ref()?.attributes.clone().unwrap_or_default();
    let name = attrs.get("name")?.clone();
    let project = attrs.get("com.docker.compose.project").map(String::as_str);
    if !app.cfg.catalog.wants_container(&name, project) {
        return None;
    }
    let service = app.cfg.catalog.service_for_container(&name, attrs.get("com.docker.compose.service").map(String::as_str));
    let (level, message) = match action {
        "start" => (crate::model::Level::Info, "container started".to_string()),
        "die" => {
            let code = attrs.get("exitCode").cloned().unwrap_or_else(|| "?".into());
            let level = if code == "0" { crate::model::Level::Info } else { crate::model::Level::Warn };
            (level, format!("container exited with code {code}"))
        }
        "oom" => (crate::model::Level::Error, "container ran out of memory (OOM killed)".to_string()),
        "kill" => (crate::model::Level::Info, format!("container sent signal {}", attrs.get("signal").map(String::as_str).unwrap_or("?"))),
        "destroy" => (crate::model::Level::Info, "container removed".to_string()),
        a if a.starts_with("health_status") => {
            let healthy = a.ends_with("healthy") && !a.ends_with("unhealthy");
            if healthy {
                return None;
            }
            (crate::model::Level::Warn, "container became unhealthy".to_string())
        }
        _ => return None,
    };
    let ts = ev
        .time_nano
        .and_then(|n| DateTime::from_timestamp(n / 1_000_000_000, (n % 1_000_000_000) as u32))
        .unwrap_or_else(Utc::now);
    Some(LogEvent {
        id: 0,
        ts,
        service,
        container: name,
        level,
        target: Some("docker".into()),
        message,
        trace_id: None,
        fields: serde_json::json!({ "image": attrs.get("image"), "action": action }),
        spans: vec![],
        correlation: None,
    })
}

async fn reconcile(
    app: &Arc<AppState>,
    docker: &Docker,
    tx: &mpsc::Sender<LogEvent>,
    tails: &mut HashMap<String, JoinHandle<()>>,
) -> Result<()> {
    let list = docker
        .list_containers(Some(ListContainersOptions { all: true, ..Default::default() }))
        .await?;
    let catalog = &app.cfg.catalog;
    let mut seen = HashMap::new();

    for c in list {
        let Some(id) = c.id.clone() else { continue };
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id[..12].to_string());
        let labels = c.labels.clone().unwrap_or_default();
        let project = labels.get("com.docker.compose.project").cloned();
        let compose_service = labels.get("com.docker.compose.service").cloned();
        let wanted = catalog.wants_container(&name, project.as_deref());
        let state = c.state.map(|s| s.to_string()).unwrap_or_default();
        let running = state == "running";
        let service = catalog.service_for_container(&name, compose_service.as_deref());

        if wanted {
            seen.insert(
                name.clone(),
                ContainerInfo {
                    id: id.clone(),
                    name: name.clone(),
                    service: service.clone(),
                    image: c.image.clone().unwrap_or_default(),
                    state: state.clone(),
                    status: c.status.clone().unwrap_or_default(),
                    project: project.clone(),
                    watched: running,
                },
            );
        }

        let alive = tails.get(&id).is_some_and(|h| !h.is_finished());
        if wanted && running && !alive {
            tracing::info!(container = %name, service = %service, "tailing");
            let handle = tokio::spawn(tail(app.clone(), docker.clone(), id.clone(), name, service, tx.clone()));
            tails.insert(id, handle);
        }
    }
    tails.retain(|_, h| !h.is_finished());
    *app.registry.containers.write().await = seen;
    Ok(())
}

async fn tail(
    app: Arc<AppState>,
    docker: Docker,
    id: String,
    name: String,
    service: String,
    tx: mpsc::Sender<LogEvent>,
) {
    let cursor = app.store.cursor(&id).await.ok().flatten();
    let backfill_from = Utc::now() - chrono::Duration::from_std(app.cfg.backfill).unwrap_or_default();
    let since = cursor.map(|c| c.max(backfill_from)).unwrap_or(backfill_from);

    let opts = LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        timestamps: true,
        since: since.timestamp() as i32,
        ..Default::default()
    };
    let mut stream = docker.logs(&id, Some(opts));
    let mut agg = Aggregator::default();
    let mut last_ts: Option<DateTime<Utc>> = cursor;
    let mut partial = String::new();
    let mut last_saved = std::time::Instant::now();

    let emit = |ts: DateTime<Utc>, text: String| {
        let p = parse::parse(&text, ts);
        LogEvent {
            id: 0,
            ts: p.ts,
            service: service.clone(),
            container: name.clone(),
            level: p.level,
            target: p.target,
            message: p.message,
            trace_id: p.trace_id,
            fields: p.fields,
            spans: p.spans,
            correlation: None,
        }
    };

    loop {
        let next = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
        let item = match next {
            Err(_) => {
                // Idle: release any folded multi-line block.
                if let Some((ts, text)) = agg.flush() {
                    if tx.send(emit(ts, text)).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                tracing::debug!(container = %name, error = %e, "log stream error");
                break;
            }
            Ok(Some(Ok(item))) => item,
        };
        let bytes = match item {
            LogOutput::StdOut { message } | LogOutput::StdErr { message } | LogOutput::Console { message } => message,
            LogOutput::StdIn { .. } => continue,
        };
        partial.push_str(&String::from_utf8_lossy(&bytes));
        // Docker splits lines over 16KB into several frames; only complete lines are parsed.
        while let Some(pos) = partial.find('\n') {
            let line: String = partial.drain(..=pos).collect();
            let line = parse::strip_ansi(line.trim_end_matches(['\n', '\r']));
            let (dts, body) = parse::split_docker_ts(&line);
            let dts = dts.unwrap_or_else(Utc::now);
            if cursor.is_some_and(|c| dts <= c) || body.trim().is_empty() {
                continue;
            }
            last_ts = Some(dts);
            for (ts, text) in agg.push(dts, body) {
                if tx.send(emit(ts, text)).await.is_err() {
                    return;
                }
            }
        }
        if last_saved.elapsed() > Duration::from_secs(5) {
            if let Some(ts) = last_ts {
                let _ = app.store.set_cursor(&id, &name, ts).await;
            }
            last_saved = std::time::Instant::now();
        }
    }
    if let Some((ts, text)) = agg.flush() {
        let _ = tx.send(emit(ts, text)).await;
    }
    if let Some(ts) = last_ts {
        let _ = app.store.set_cursor(&id, &name, ts).await;
    }
    tracing::info!(container = %name, "log stream ended");
}

/// Batches incoming events into SQLite, then fans them out to the detector and live tail.
pub async fn writer(app: Arc<AppState>, mut rx: mpsc::Receiver<LogEvent>) {
    let mut batch: Vec<LogEvent> = Vec::with_capacity(512);
    loop {
        let first = match rx.recv().await {
            Some(ev) => ev,
            None => return,
        };
        batch.push(first);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while batch.len() < 500 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => batch.push(ev),
                _ => break,
            }
        }
        if let Err(e) = app.store.insert_events(&mut batch).await {
            tracing::error!(error = %e, n = batch.len(), "failed to store log batch");
            batch.clear();
            continue;
        }
        app.metrics.ingested.fetch_add(batch.len() as u64, Ordering::Relaxed);
        for ev in batch.drain(..) {
            let _ = app.live.send(Arc::new(ev));
        }
    }
}
