//! McQueen — self-hosted AI triage for the Regere microservice stack.
//!
//! ```text
//! docker socket ─▶ collector ─▶ parse ─▶ SQLite log index ─▶ detector ─▶ incident
//!                                                                         │ (settle)
//!   trace graph + logs ◀─┐                                                ▼
//!   code at log sites  ◀─┼──────── context ◀────────────────────────── triage worker
//!   schema (packages)  ◀─┘            │                                   │
//!                                     └──▶ rig agent (Groq) ──▶ report ──▶ UI / Chat
//! ```
//!
//! Commands:
//! - `mcqueen` / `mcqueen serve` — run everything (default).
//! - `mcqueen import --service S --container C FILE` — load `docker logs -t` output into the store.
//! - `mcqueen triage <trace-id|traceparent|incident-id>` — triage once and print the report.
//! - `mcqueen check` — print what the code index and schema catalog found.

mod agent;
mod api;
mod code;
mod collector;
mod config;
mod context;
mod model;
mod notify;
mod parse;
mod redact;
mod schema;
mod store;
mod trace;
mod triage;

use std::{
    io::BufRead,
    sync::{atomic::AtomicU64, Arc, RwLock},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc};

use crate::{
    code::CodeIndex,
    config::Config,
    model::{LogEvent, UiEvent},
    schema::SchemaCatalog,
    store::Store,
};

#[derive(Default)]
pub struct Metrics {
    pub ingested: AtomicU64,
    pub problems: AtomicU64,
    pub incidents: AtomicU64,
    pub contexts: AtomicU64,
    pub triaged: AtomicU64,
    pub triage_failed: AtomicU64,
    pub notified: AtomicU64,
    pub llm_tokens: AtomicU64,
    pub llm_cost_micro_usd: AtomicU64,
    pub started_at: DateTime<Utc>,
}

pub struct AppState {
    pub cfg: Config,
    pub store: Store,
    pub registry: collector::Registry,
    pub metrics: Metrics,
    /// Every stored event, for the detector and the live tail.
    pub live: broadcast::Sender<Arc<LogEvent>>,
    pub ui: broadcast::Sender<UiEvent>,
    pub triage: triage::Triage,
    pub llm_budget: agent::TokenBudget,
    code: RwLock<Arc<CodeIndex>>,
    schema: RwLock<Arc<SchemaCatalog>>,
}

impl AppState {
    pub fn code(&self) -> Arc<CodeIndex> {
        self.code.read().unwrap().clone()
    }
    pub fn schema(&self) -> Arc<SchemaCatalog> {
        self.schema.read().unwrap().clone()
    }
}

/// Re-index repos and reload schema packages (also on `POST /api/reload`).
pub async fn reload_sources(app: &AppState) {
    let dir = app.cfg.repos_dir.clone();
    let index = tokio::task::spawn_blocking(move || CodeIndex::build(&dir)).await.unwrap_or_default();
    *app.code.write().unwrap() = Arc::new(index);
    let catalog = SchemaCatalog::load(app.cfg.schema_dir.as_deref(), app.cfg.core_packages_url.as_deref()).await;
    *app.schema.write().unwrap() = Arc::new(catalog);
}

async fn build_state(cfg: Config) -> Result<(Arc<AppState>, mpsc::UnboundedReceiver<String>)> {
    let store = Store::open(&cfg.database_path).await.context("opening the SQLite store")?;
    let (live, _) = broadcast::channel(8192);
    let (ui, _) = broadcast::channel(512);
    let (triage, rx) = triage::Triage::new();
    let llm_budget = agent::TokenBudget::new(cfg.llm.tpm_limit);
    let app = Arc::new(AppState {
        llm_budget,
        cfg,
        store,
        registry: Default::default(),
        metrics: Metrics { started_at: Utc::now(), ..Default::default() },
        live,
        ui,
        triage,
        code: RwLock::new(Arc::new(CodeIndex::default())),
        schema: RwLock::new(Arc::new(SchemaCatalog::default())),
    });
    reload_sources(&app).await;
    Ok((app, rx))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mcqueen=info,mcqueen_triage=info,tower_http=warn")),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Config::from_env()?;
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(cfg).await,
        Some("import") => import(cfg, &args[1..]).await,
        Some("triage") => triage_once(cfg, args.get(1).context("usage: mcqueen triage <trace-id|incident-id>")?).await,
        Some("check") => check(cfg).await,
        Some(other) => bail!("unknown command '{other}' (serve | import | triage | check)"),
    }
}

async fn serve(cfg: Config) -> Result<()> {
    let bind = cfg.bind.clone();
    let (app, triage_rx) = build_state(cfg).await?;

    // Incidents interrupted by a restart are picked up again.
    for id in app.store.reset_stuck_triage().await? {
        app.triage.schedule(&id, Duration::from_secs(5), true);
    }

    let (tx, rx) = mpsc::channel::<LogEvent>(20_000);
    tokio::spawn(collector::writer(app.clone(), rx));
    tokio::spawn(collector::run(app.clone(), tx));
    tokio::spawn(triage::detector(app.clone()));
    tokio::spawn(triage::scheduler(app.clone()));
    tokio::spawn(triage::worker(app.clone(), triage_rx));
    tokio::spawn(housekeeping(app.clone()));

    let listener = tokio::net::TcpListener::bind(&bind).await.with_context(|| format!("binding {bind}"))?;
    tracing::info!(%bind, model = %app.cfg.llm.model, llm = app.cfg.llm.api_key.is_some(), "McQueen listening");
    axum::serve(listener, api::router(app)).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

/// Hourly pruning, and a source refresh every 10 minutes (repos get pulled, packages change).
async fn housekeeping(app: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        if n > 0 {
            reload_sources(&app).await;
        }
        if n.is_multiple_of(6) {
            match app.store.prune(app.cfg.retention, app.cfg.max_log_rows).await {
                Ok(removed) if removed > 0 => tracing::info!(removed, "pruned old log lines"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "prune failed"),
            }
        }
        n += 1;
    }
}

/// Load `docker logs -t <container>` output, e.g. pulled read-only from the server:
/// `ssh host docker logs -t --since 2h dev_core > core.log`.
async fn import(cfg: Config, args: &[String]) -> Result<()> {
    let mut service = None;
    let mut container = None;
    let mut file = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--service" => service = it.next().cloned(),
            "--container" => container = it.next().cloned(),
            other => file = Some(other.to_string()),
        }
    }
    let file = file.context("usage: mcqueen import [--service S] [--container C] FILE")?;
    let container = container.unwrap_or_else(|| {
        std::path::Path::new(&file).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    });
    let service = service.unwrap_or_else(|| cfg.catalog.service_for_container(&container, None));
    let (app, _rx) = build_state(cfg).await?;

    let reader = std::io::BufReader::new(std::fs::File::open(&file)?);
    let mut agg = parse::Aggregator::default();
    let mut batch: Vec<LogEvent> = vec![];
    let mut total = 0;
    let mk = |ts: DateTime<Utc>, text: String| {
        let p = parse::parse(&text, ts);
        LogEvent {
            id: 0,
            ts: p.ts,
            service: service.clone(),
            container: container.clone(),
            level: p.level,
            target: p.target,
            message: p.message,
            trace_id: p.trace_id,
            fields: p.fields,
            spans: p.spans,
            correlation: None,
        }
    };
    for line in reader.lines() {
        let line = parse::strip_ansi(&line?);
        let (ts, body) = parse::split_docker_ts(&line);
        if body.trim().is_empty() {
            continue;
        }
        for (ts, text) in agg.push(ts.unwrap_or_else(Utc::now), body) {
            batch.push(mk(ts, text));
        }
        if batch.len() >= 1000 {
            total += flush_import(&app, &mut batch).await?;
        }
    }
    if let Some((ts, text)) = agg.flush() {
        batch.push(mk(ts, text));
    }
    total += flush_import(&app, &mut batch).await?;
    println!("imported {total} events from {file} as service={service} container={container}");
    Ok(())
}

/// Store a batch and open incidents for its problems (without auto-triaging them).
async fn flush_import(app: &Arc<AppState>, batch: &mut Vec<LogEvent>) -> Result<usize> {
    let n = batch.len();
    app.store.insert_events(batch).await?;
    for ev in batch.drain(..) {
        if triage::is_problem(app, &ev) {
            let fp = match &ev.trace_id {
                Some(t) => format!("trace:{t}"),
                None => triage::fingerprint(&ev),
            };
            let title = format!("{}: {}", ev.service, redact::truncate(ev.message.lines().next().unwrap_or_default(), 180));
            app.store
                .upsert_incident(store::NewIncident {
                    fingerprint: &fp,
                    trace_id: ev.trace_id.as_deref(),
                    source: "backend",
                    service: Some(&ev.service),
                    anchor_log_id: Some(ev.id),
                    title: &title,
                    level: ev.level.as_str(),
                    seen: ev.ts,
                })
                .await?;
        }
    }
    Ok(n)
}

async fn triage_once(cfg: Config, target: &str) -> Result<()> {
    let (app, _rx) = build_state(cfg).await?;
    let id = if let Some(tid) = parse::trace_id_from_input(target) {
        let inc = triage::manual_trace(&app, &tid).await?;
        inc.id
    } else {
        target.to_string()
    };
    triage::run(&app, &id).await?;
    let inc = app.store.incident(&id).await?.context("incident vanished")?;
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "incident": inc.id,
        "report": inc.report,
        "usage": inc.llm_usage,
        "prompt_chars": inc.context.as_ref().and_then(|c| c.get("prompt_chars")),
    }))?);
    Ok(())
}

async fn check(cfg: Config) -> Result<()> {
    let (app, _rx) = build_state(cfg).await?;
    let code = app.code();
    println!("repos ({}):", code.repos.len());
    for r in &code.repos {
        println!("  {:<22} {:>6} files  {}@{}", r.name, r.files, r.branch.as_deref().unwrap_or("-"), r.commit.as_deref().unwrap_or("-"));
    }
    println!("crates: {}", code.crate_names().iter().map(|(c, r)| format!("{c}({r})")).collect::<Vec<_>>().join(", "));
    let schema = app.schema();
    println!("schema packages ({}):", schema.packages.len());
    for p in &schema.packages {
        println!("  {:<26} {:>3} tables  schema={}  v{}", p.id, p.tables, p.schema, p.version.as_deref().unwrap_or("?"));
    }
    println!("services in catalog: {}", app.cfg.catalog.services.len());
    println!("llm: {} (key {})", app.cfg.llm.model, if app.cfg.llm.api_key.is_some() { "set" } else { "missing" });
    Ok(())
}
