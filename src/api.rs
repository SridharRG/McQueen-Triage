//! HTTP API for the UI, the browser SDK and the CLI.

use std::{convert::Infallible, sync::atomic::Ordering, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::{ServeDir, ServeFile},
};

use crate::{
    model::{FrontendError, Level, STATUS_ACK, STATUS_OPEN, STATUS_RESOLVED},
    parse,
    store::{IncidentQuery, LogQuery},
    trace, triage, AppState,
};

type AppResult<T> = Result<T, ApiError>;

pub struct ApiError(StatusCode, String);

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(status: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into())
}

pub fn router(app: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/overview", get(overview))
        .route("/services", get(services))
        .route("/incidents", get(list_incidents))
        .route("/incidents/{id}", get(get_incident).patch(patch_incident))
        .route("/incidents/{id}/triage", post(retriage))
        .route("/traces/{trace_id}", get(get_trace))
        .route("/traces/{trace_id}/triage", post(triage_trace))
        .route("/logs", get(search_logs))
        .route("/logs/stream", get(stream_logs))
        .route("/events", get(stream_events))
        .route("/schema", get(schema_overview))
        .route("/schema/tables/{name}", get(schema_table))
        .route("/code/repos", get(code_repos))
        .route("/reload", post(reload_sources));

    let origins = app.cfg.ingest_allowed_origins.clone();
    let cors = CorsLayer::new()
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::HeaderName::from_static("x-mcqueen-key")])
        .allow_origin(if origins.is_empty() {
            AllowOrigin::mirror_request()
        } else {
            AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()))
        })
        .max_age(Duration::from_secs(3600));
    let ingest = Router::new().route("/ingest/frontend", post(ingest_frontend)).layer(cors);

    let index = app.cfg.static_dir.join("index.html");
    let static_files = ServeDir::new(&app.cfg.static_dir).fallback(ServeFile::new(index));

    Router::new()
        .nest("/api", api.merge(ingest))
        .fallback_service(static_files)
        .layer(middleware::from_fn_with_state(app.clone(), ui_auth))
        .with_state(app)
}

// ── auth ─────────────────────────────────────────────────────────────────────

fn credentials_ok(app: &AppState, headers: &HeaderMap) -> bool {
    let (Some(user), Some(pass)) = (&app.cfg.auth_user, &app.cfg.auth_password) else {
        return true;
    };
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(b64) = value.strip_prefix("Basic ") else { return false };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let expected = format!("{user}:{pass}");
    constant_time_eq(&decoded, expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"McQueen\"")],
        "authentication required",
    )
        .into_response()
}

/// Basic auth on everything except health and the browser-SDK ingest endpoint.
async fn ui_auth(State(app): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/api/health" || path.starts_with("/api/ingest/") || credentials_ok(&app, req.headers()) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn health(State(app): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "docker": app.registry.docker_ok.load(Ordering::Relaxed),
        "llm_configured": app.cfg.llm.api_key.is_some(),
    }))
}

async fn overview(State(app): State<Arc<AppState>>) -> AppResult<Json<Value>> {
    let since = Utc::now() - chrono::Duration::hours(6);
    let hist = app.store.histogram(since, Duration::from_secs(900)).await?;
    let counts = app.store.incident_counts().await?;
    let stats: Vec<Value> = app
        .store
        .service_stats(Utc::now() - chrono::Duration::hours(24))
        .await?
        .into_iter()
        .map(|s| {
            let mut v = serde_json::to_value(&s).unwrap_or_default();
            v["layer"] = json!(app.cfg.catalog.layer_of(&s.service));
            v
        })
        .collect();
    let m = &app.metrics;
    let containers = app.registry.containers.read().await;
    let code = app.code();
    let schema = app.schema();
    Ok(Json(json!({
        "incidents": counts,
        "histogram": hist,
        "services": stats,
        "pipeline": {
            "docker": {
                "ok": app.registry.docker_ok.load(Ordering::Relaxed),
                "error": app.registry.docker_error.read().await.clone(),
                "containers": containers.len(),
                "tailing": containers.values().filter(|c| c.watched).count(),
            },
            "ingested": m.ingested.load(Ordering::Relaxed),
            "problems": m.problems.load(Ordering::Relaxed),
            "incidents": m.incidents.load(Ordering::Relaxed),
            "contexts": m.contexts.load(Ordering::Relaxed),
            "triaged": m.triaged.load(Ordering::Relaxed),
            "triage_failed": m.triage_failed.load(Ordering::Relaxed),
            "notified": m.notified.load(Ordering::Relaxed),
            "queued": app.triage.queued(),
            "budget_left": app.triage.budget_left(app.cfg.triage.max_per_hour),
            "llm_tokens": m.llm_tokens.load(Ordering::Relaxed),
            "llm_tokens_last_minute": app.llm_budget.used_last_minute(),
            "llm_cost_usd": m.llm_cost_micro_usd.load(Ordering::Relaxed) as f64 / 1e6,
            "repos": code.repos.len(),
            "tables": schema.tables.len(),
            "started_at": m.started_at,
        },
        "config": {
            "model": app.cfg.llm.model,
            "llm_configured": app.cfg.llm.api_key.is_some(),
            "tools": app.cfg.llm.tools,
            "max_turns": app.cfg.llm.max_turns,
            "context_budget_chars": app.cfg.llm.context_budget_chars,
            "tpm_limit": app.cfg.llm.tpm_limit,
            "auto_triage": app.cfg.triage.auto,
            "max_per_hour": app.cfg.triage.max_per_hour,
            "settle_secs": app.cfg.triage.settle.as_secs(),
            "jaeger": app.cfg.jaeger_url.is_some(),
            "notifications": {
                "google_chat": app.cfg.google_chat_webhook.is_some(),
                "slack": app.cfg.slack_webhook.is_some(),
                "min_severity": app.cfg.notify_min_severity,
            },
            "ingest_key_required": app.cfg.ingest_key.is_some(),
            "retention_hours": app.cfg.retention.as_secs() / 3600,
        }
    })))
}

async fn services(State(app): State<Arc<AppState>>) -> AppResult<Json<Value>> {
    let stats = app.store.service_stats(Utc::now() - chrono::Duration::hours(24)).await?;
    let containers = app.registry.containers.read().await.clone();
    let code = app.code();
    let mut names: Vec<String> = app.cfg.catalog.services.iter().map(|s| s.name.clone()).collect();
    for c in containers.values() {
        if !names.contains(&c.service) {
            names.push(c.service.clone());
        }
    }
    for s in &stats {
        if !names.contains(&s.service) {
            names.push(s.service.clone());
        }
    }
    let rows: Vec<Value> = names
        .iter()
        .map(|name| {
            let def = app.cfg.catalog.service(name);
            let st = stats.iter().find(|s| &s.service == name);
            let conts: Vec<_> = containers.values().filter(|c| &c.service == name).collect();
            let repo = def.and_then(|d| d.repo.as_deref()).and_then(|r| code.repo(r));
            json!({
                "name": name,
                "layer": app.cfg.catalog.layer_of(name),
                "description": def.map(|d| d.description.clone()).unwrap_or_default(),
                "owner": def.and_then(|d| d.owner.clone()),
                "containers": conts,
                "repo": repo.map(|r| json!({"name": r.name, "branch": r.branch, "commit": r.commit, "files": r.files})),
                "stats": st,
            })
        })
        .collect();
    Ok(Json(json!({ "services": rows, "crates": code.crate_names() })))
}

async fn list_incidents(State(app): State<Arc<AppState>>, Query(q): Query<IncidentQuery>) -> AppResult<Json<Value>> {
    let items = app.store.list_incidents(&q).await?;
    Ok(Json(json!({ "incidents": items })))
}

async fn get_incident(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<Json<Value>> {
    let inc = app.store.incident(&id).await?.ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such incident"))?;
    Ok(Json(serde_json::to_value(inc)?))
}

#[derive(Deserialize)]
struct PatchIncident {
    status: String,
}

async fn patch_incident(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PatchIncident>,
) -> AppResult<Json<Value>> {
    if ![STATUS_OPEN, STATUS_ACK, STATUS_RESOLVED].contains(&body.status.as_str()) {
        return Err(bad(StatusCode::BAD_REQUEST, "status must be open, acknowledged or resolved"));
    }
    if !app.store.set_status(&id, &body.status).await? {
        return Err(bad(StatusCode::NOT_FOUND, "no such incident"));
    }
    let inc = app.store.incident(&id).await?.expect("exists");
    let _ = app.ui.send(crate::model::UiEvent::Incident { id: inc.id.clone(), triage_state: inc.triage_state.clone(), status: inc.status.clone() });
    Ok(Json(serde_json::to_value(inc)?))
}

async fn retriage(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<Json<Value>> {
    let inc = app.store.incident(&id).await?.ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such incident"))?;
    app.store.set_triage_state(&inc.id, crate::model::TRIAGE_PENDING, None).await?;
    app.triage.enqueue_now(&inc.id);
    Ok(Json(json!({ "id": inc.id, "triage_state": "pending" })))
}

async fn get_trace(State(app): State<Arc<AppState>>, Path(raw): Path<String>) -> AppResult<Json<Value>> {
    let tid = parse::trace_id_from_input(&raw).ok_or_else(|| bad(StatusCode::BAD_REQUEST, "not a trace id or traceparent"))?;
    let incident = app.store.incident_by_fingerprint(&format!("trace:{tid}")).await?;
    let fe: Option<FrontendError> = incident.as_ref().and_then(|i| i.fe_payload.clone()).and_then(|v| serde_json::from_value(v).ok());
    let view = trace::build(&app, &tid, fe.as_ref()).await?;
    Ok(Json(json!({ "trace": view, "incident_id": incident.map(|i| i.id) })))
}

async fn triage_trace(State(app): State<Arc<AppState>>, Path(raw): Path<String>) -> AppResult<Json<Value>> {
    let tid = parse::trace_id_from_input(&raw).ok_or_else(|| bad(StatusCode::BAD_REQUEST, "not a trace id or traceparent"))?;
    let inc = triage::manual_trace(&app, &tid).await?;
    Ok(Json(json!({ "id": inc.id, "trace_id": tid })))
}

async fn search_logs(State(app): State<Arc<AppState>>, Query(q): Query<LogQuery>) -> AppResult<Json<Value>> {
    let logs = app.store.search_logs(&q).await?;
    Ok(Json(json!({ "logs": logs })))
}

#[derive(Deserialize, Default)]
struct StreamFilter {
    service: Option<String>,
    level: Option<String>,
    q: Option<String>,
}

async fn stream_logs(
    State(app): State<Arc<AppState>>,
    Query(f): Query<StreamFilter>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let min = f.level.as_deref().and_then(Level::parse).unwrap_or(Level::Trace);
    let service = f.service.filter(|s| !s.is_empty());
    let needle = f.q.filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase());
    let stream = BroadcastStream::new(app.live.subscribe()).filter_map(move |ev| {
        let keep = match &ev {
            Ok(ev) => {
                ev.level >= min
                    && service.as_ref().is_none_or(|s| &ev.service == s)
                    && needle.as_ref().is_none_or(|n| ev.message.to_ascii_lowercase().contains(n))
            }
            Err(_) => false,
        };
        let out = if keep { ev.ok().and_then(|e| Event::default().json_data(&*e).ok()).map(Ok) } else { None };
        async move { out }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn stream_events(State(app): State<Arc<AppState>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.ui.subscribe())
        .filter_map(|ev| async move { ev.ok().and_then(|e| Event::default().json_data(e).ok()).map(Ok) });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct SchemaQuery {
    q: Option<String>,
}

async fn schema_overview(State(app): State<Arc<AppState>>, Query(q): Query<SchemaQuery>) -> Json<Value> {
    let schema = app.schema();
    let tables: Vec<Value> = schema
        .search(q.q.as_deref().unwrap_or(""))
        .iter()
        .map(|t| json!({"name": t.qualified(), "package": t.package, "columns": t.columns.len(), "api_path": t.api_path, "comment": t.comment}))
        .collect();
    Json(json!({ "packages": schema.packages, "tables": tables }))
}

async fn schema_table(State(app): State<Arc<AppState>>, Path(name): Path<String>) -> AppResult<Json<Value>> {
    let schema = app.schema();
    let t = schema.table(&name).ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such table"))?;
    Ok(Json(json!({ "table": t, "ddl": schema.render(&[t]) })))
}

async fn code_repos(State(app): State<Arc<AppState>>) -> Json<Value> {
    let code = app.code();
    Json(json!({ "repos": code.repos, "crates": code.crate_names() }))
}

async fn reload_sources(State(app): State<Arc<AppState>>) -> Json<Value> {
    crate::reload_sources(&app).await;
    let (code, schema) = (app.code(), app.schema());
    Json(json!({ "repos": code.repos.len(), "tables": schema.tables.len() }))
}

async fn ingest_frontend(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut fe): Json<FrontendError>,
) -> AppResult<Json<Value>> {
    if let Some(key) = &app.cfg.ingest_key {
        let given = headers.get("x-mcqueen-key").and_then(|v| v.to_str().ok()).unwrap_or_default();
        if !constant_time_eq(given.as_bytes(), key.as_bytes()) {
            return Err(bad(StatusCode::UNAUTHORIZED, "bad ingest key"));
        }
    }
    // Keep payloads small and free of query strings (tokens sometimes ride there).
    fe.url = fe.url.map(|u| trace::path_only(&u));
    fe.page = fe.page.map(|u| trace::path_only(&u));
    fe.response_body = fe.response_body.map(|b| crate::redact::truncate(&b, 4000));
    fe.ui_message = fe.ui_message.map(|b| crate::redact::truncate(&b, 1000));
    if fe.user_agent.is_none() {
        fe.user_agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_string);
    }
    let inc = triage::frontend_error(&app, fe).await.map_err(|e| bad(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({
        "incident_id": inc.id,
        "trace_id": inc.trace_id,
        "url": format!("{}/incidents/{}", app.cfg.public_url, inc.id),
    })))
}
