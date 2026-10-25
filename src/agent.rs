//! The rig agent: one investigation run per incident.
//!
//! The pre-assembled context (trace, logs, code, schema) goes in the prompt.
//! Read-only tools let the model pull more source or schema when the snippets
//! aren't enough. The report comes back through rig's output-tool mode (a
//! synthetic final tool call), because Groq refuses JSON mode combined with tools.
//! Groq's free tier allows 8k tokens/minute, so turns are capped and 429s back off.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use rig_agent::{
    agent::{CompletionCallAction, CompletionCallEvent, ModelTurnAction, ModelTurnFinished},
    prelude::*,
    run::{output::OutputMode, RequestPatch},
};
use rig_core::{
    completion::message::{ToolChoice, ToolName},
    providers::groq,
    tool::context::ToolContext,
};

/// Name rig gives the synthetic structured-output tool.
const REPORT_TOOL: &str = "final_result";
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{context::TriageContext, model::Level, redact::scrub, trace::TraceView, AppState};

/// Severity helpers; the report carries severity as a normalised string.
pub struct Severity;

impl Severity {
    pub fn rank(s: &str) -> u8 {
        match s {
            "critical" => 4,
            "high" => 3,
            "medium" => 2,
            "low" => 1,
            _ => 0,
        }
    }

    fn normalise(s: &str) -> String {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "critical" | "blocker" | "sev1" | "p0" => "critical",
            "high" | "major" | "sev2" | "p1" | "error" => "high",
            "low" | "minor" | "info" | "trivial" | "p3" => "low",
            _ => "medium",
        }
        .into()
    }
}

const CATEGORIES: [&str; 8] =
    ["code_bug", "data_issue", "config", "dependency_down", "auth_permission", "bad_request", "infra", "unknown"];

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChainStep {
    /// Service name exactly as listed in the request journey.
    pub service: String,
    /// What happened in that service, one sentence.
    #[serde(default, alias = "description", alias = "what", alias = "detail")]
    pub observation: String,
}

/// The triage report. Models drift from schemas, so only the core fields are
/// required and the rest default; `normalise` fixes enum-like strings afterwards.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TriageReport {
    /// One-line headline, at most 12 words, naming the failing service.
    pub title: String,
    /// One of: critical, high, medium, low.
    pub severity: String,
    /// One of: code_bug, data_issue, config, dependency_down, auth_permission, bad_request, infra, unknown.
    #[serde(default)]
    pub category: String,
    /// The service where the failure originated (not where it surfaced).
    pub origin_service: String,
    /// Hop-by-hop path of the failure, using only services from the request journey, entry service first.
    #[serde(default)]
    pub failure_chain: Vec<ChainStep>,
    /// Technical root cause, 2-4 sentences, citing log lines / code.
    pub root_cause: String,
    /// Plain-English explanation for a frontend developer with no backend knowledge (2-3 sentences).
    pub frontend_summary: String,
    /// What the frontend developer should do right now.
    #[serde(default)]
    pub frontend_action: String,
    /// True only if the frontend sent a wrong request (bad payload/params/auth) that it must fix.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_frontend_issue: bool,
    /// Which service/team should fix it.
    #[serde(default)]
    pub owner: String,
    /// The fix, concretely (markdown allowed).
    #[serde(default)]
    pub suggested_fix: String,
    /// `repo/path/file.rs:line` of the fix, or empty if unknown.
    #[serde(default)]
    pub fix_location: String,
    /// Code patch or SQL for the fix, or empty.
    #[serde(default)]
    pub patch: String,
    /// Relevant database observations (schema/constraints/data), or empty.
    #[serde(default)]
    pub db_notes: String,
    /// 0.0-1.0 confidence in the root cause.
    #[serde(default = "half", deserialize_with = "lenient_f64")]
    pub confidence: f64,
    /// The specific log lines / code lines the conclusion rests on.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Information that was missing and would raise confidence (e.g. trace propagation gaps).
    #[serde(default)]
    pub missing_context: Vec<String>,
}

impl TriageReport {
    fn normalise(mut self) -> Self {
        self.severity = Severity::normalise(&self.severity);
        let cat = self.category.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        self.category = if CATEGORIES.contains(&cat.as_str()) { cat } else { "unknown".into() };
        self.confidence = if self.confidence > 1.0 { self.confidence / 100.0 } else { self.confidence }.clamp(0.0, 1.0);
        self
    }
}

fn half() -> f64 {
    0.5
}

fn lenient_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64().unwrap_or(0.5),
        Value::String(s) => s.trim().trim_end_matches('%').parse().unwrap_or(0.5),
        _ => 0.5,
    })
}

fn lenient_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => b,
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes"),
        _ => false,
    })
}

const PREAMBLE: &str = r#"You are McQueen, a senior SRE triaging failures in Regere's microservice platform for frontend developers.

Platform: the browser app (client-ui-next, Next.js) calls `launchpad` (edge gateway: resolves the session via `authrs`, sets X-User-ID, proxies /proxy/<slug>/… to services from its registry). `core` (service-kit on architect-sdk: config-driven REST over Postgres packages, checks permissions with authrs, calls workflow-engine and decision-hub). `workflow-engine` runs workflows; `decision-hub` evaluates rules and fires actions (RabbitMQ); `message-flow` sends notifications; plus load-optimizer, yard-recom, simulation, common-services. Services propagate W3C `traceparent`; some don't yet, so some lines are linked by shared keys (marked ~).

How to work:
1. Find where the failure ORIGINATED (often several hops behind where it surfaced) and the chain to it. The chain uses ONLY services listed in the request journey; if the request came from a webhook, schedule or background job (e.g. trigger=webhook), start the chain at the first service that logged it, not at the browser.
2. Ground every claim in the given log lines, code or schema. Never invent files, functions, tables or columns. If the evidence is thin, say so and lower confidence.
3. Use tools only when the provided snippets are not enough (e.g. read a caller, search for an error string, fetch a table). Keep tool use to a minimum.
4. Separate "the frontend sent a bad request" from "the backend failed"; most 5xx are backend issues.
5. Warnings/errors listed as NOT linked are background noise unless clearly related.
6. Write `frontend_summary` for a frontend developer: no jargon, say whether it is their bug and what the user experienced.
Return the final answer by calling the `final_result` tool."#;

/// Result of one triage run.
pub struct AgentOutcome {
    pub report: TriageReport,
    pub usage: Value,
}

#[derive(Clone)]
struct ToolEnv {
    app: Arc<AppState>,
    trace: Arc<TraceView>,
    /// Every tool call and what it returned, so a fallback pass can reuse the findings.
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl ToolEnv {
    fn record(&self, call: String, output: &Result<String, ToolErr>) {
        let out = match output {
            Ok(s) => s.clone(),
            Err(e) => format!("error: {e}"),
        };
        if let Ok(mut c) = self.calls.lock() {
            c.push((call, out));
        }
    }

    fn call_names(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.iter().map(|(n, _)| n.clone()).collect()).unwrap_or_default()
    }

    /// Tool results so far, for the no-tools fallback prompt.
    fn findings(&self, budget: usize) -> String {
        let calls = self.calls.lock().map(|c| c.clone()).unwrap_or_default();
        let mut out = String::new();
        for (call, result) in calls {
            let block = format!("### {call}\n```\n{}\n```\n", crate::redact::truncate(&result, 1800));
            if out.len() + block.len() > budget {
                break;
            }
            out.push_str(&block);
        }
        out
    }
}

#[derive(Debug)]
pub struct ToolErr(String);
impl std::fmt::Display for ToolErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ToolErr {}

const TOOL_OUTPUT_MAX: usize = 3500;

fn bounded(s: String) -> String {
    crate::redact::truncate(&scrub(&s), TOOL_OUTPUT_MAX)
}

// ── rate limiting ────────────────────────────────────────────────────────────

/// Tokens spent on the LLM in the last 60 s, shared by every triage run, so each
/// model call (including every turn of a tool loop) waits for room under the
/// provider's tokens-per-minute cap instead of hitting a 429.
pub struct TokenBudget {
    limit: u64,
    spent: Mutex<VecDeque<(Instant, u64)>>,
}

impl TokenBudget {
    pub fn new(limit_per_minute: u64) -> Self {
        Self { limit: limit_per_minute, spent: Mutex::new(VecDeque::new()) }
    }

    fn window(&self) -> (u64, Option<Instant>) {
        let mut spent = self.spent.lock().unwrap();
        while spent.front().is_some_and(|(t, _)| t.elapsed() >= Duration::from_secs(60)) {
            spent.pop_front();
        }
        (spent.iter().map(|(_, n)| n).sum(), spent.front().map(|(t, _)| *t))
    }

    /// Wait until `need` more tokens fit in the window. A no-op without a limit.
    pub async fn wait_for(&self, need: u64) {
        if self.limit == 0 {
            return;
        }
        let need = need.min(self.limit);
        loop {
            let (used, oldest) = self.window();
            if used + need <= self.limit {
                return;
            }
            let wait = oldest
                .map(|t| Duration::from_secs(60).saturating_sub(t.elapsed()))
                .unwrap_or(Duration::from_secs(1))
                .max(Duration::from_millis(250));
            tracing::debug!(used, need, wait_ms = wait.as_millis() as u64, "waiting for LLM token budget");
            tokio::time::sleep(wait).await;
        }
    }

    /// Align with the provider's view of the window (it also counts other clients of the key).
    pub fn sync_used(&self, provider_used: u64) {
        let ours = self.window().0;
        self.record(provider_used.saturating_sub(ours));
    }

    pub fn record(&self, tokens: u64) {
        if tokens > 0 {
            self.spent.lock().unwrap().push_back((Instant::now(), tokens));
        }
    }

    pub fn used_last_minute(&self) -> u64 {
        self.window().0
    }
}

/// Paces each model call through the shared [`TokenBudget`], and on the last
/// allowed turn forces the report tool so a run can't end without a report.
#[derive(Clone)]
struct Pacer {
    app: Arc<AppState>,
    first_call_estimate: u64,
    last_input: Arc<AtomicU64>,
    max_turns: usize,
}

impl AgentHook for Pacer {
    fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> impl Future<Output = CompletionCallAction> + Send {
        // Later turns resend everything so far, plus the tool results.
        let need = if event.turn <= 1 {
            self.first_call_estimate
        } else {
            self.last_input.load(Ordering::Relaxed) + 1500
        };
        let app = self.app.clone();
        let last_turn = event.turn >= self.max_turns;
        async move {
            app.llm_budget.wait_for(need).await;
            if last_turn {
                // Keep the tools listed (the model has seen them in history and would
                // try them anyway) but name the report tool as the only allowed call.
                CompletionCallAction::Patch(RequestPatch {
                    tool_choice: ToolName::new(REPORT_TOOL)
                        .ok()
                        .map(|name| ToolChoice::Specific { function_names: vec![name] }),
                    ..Default::default()
                })
            } else {
                CompletionCallAction::Continue
            }
        }
    }

    fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> impl Future<Output = ModelTurnAction> + Send {
        let u = &event.usage;
        let total = u.total_tokens.unwrap_or(u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0));
        self.app.llm_budget.record(total);
        self.last_input.store(u.input_tokens.unwrap_or(total), Ordering::Relaxed);
        async { ModelTurnAction::Continue }
    }
}

// ── tools ────────────────────────────────────────────────────────────────────

struct ReadSource(ToolEnv);
#[derive(Deserialize)]
struct ReadSourceArgs {
    repo: String,
    path: String,
    start_line: usize,
    end_line: usize,
}

impl rig_core::tool::Tool for ReadSource {
    const NAME: &'static str = "read_source";
    type Args = ReadSourceArgs;
    type Output = String;
    type Error = ToolErr;
    fn description(&self) -> String {
        "Read lines of a source file from one of the service repos (read-only, max 160 lines).".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "repo":{"type":"string","description":"Repo name, e.g. launchpad, architect-sdk"},
            "path":{"type":"string","description":"Path relative to the repo root, e.g. src/service_registry.rs"},
            "start_line":{"type":"integer"},"end_line":{"type":"integer"}},
            "required":["repo","path","start_line","end_line"]})
    }
    async fn call(&self, _ctx: &mut ToolContext, a: ReadSourceArgs) -> Result<String, ToolErr> {
        let out = self.0.app.code().read(&a.repo, &a.path, a.start_line, a.end_line).map(bounded).map_err(ToolErr);
        self.0.record(format!("read_source {}/{}:{}-{}", a.repo, a.path, a.start_line, a.end_line), &out);
        out
    }
}

struct SearchCode(ToolEnv);
#[derive(Deserialize)]
struct SearchCodeArgs {
    query: String,
    #[serde(default)]
    repo: Option<String>,
}

impl rig_core::tool::Tool for SearchCode {
    const NAME: &'static str = "search_code";
    type Args = SearchCodeArgs;
    type Output = String;
    type Error = ToolErr;
    fn description(&self) -> String {
        "Literal (case-sensitive) text search across the service repos. Returns repo/file:line: text.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"Exact text, e.g. an error message fragment or function name"},
            "repo":{"type":"string","description":"Optional repo to restrict to"}},
            "required":["query"]})
    }
    async fn call(&self, _ctx: &mut ToolContext, a: SearchCodeArgs) -> Result<String, ToolErr> {
        let hits = self.0.app.code().search(&a.query, a.repo.as_deref(), 25);
        let out = Ok(if hits.is_empty() { "no matches".into() } else { bounded(hits.join("\n")) });
        self.0.record(format!("search_code {:?}{}", a.query, a.repo.as_ref().map(|r| format!(" in {r}")).unwrap_or_default()), &out);
        out
    }
}

struct TableSchema(ToolEnv);
#[derive(Deserialize)]
struct TableSchemaArgs {
    table: String,
}

impl rig_core::tool::Tool for TableSchema {
    const NAME: &'static str = "table_schema";
    type Args = TableSchemaArgs;
    type Output = String;
    type Error = ToolErr;
    fn description(&self) -> String {
        "Schema (columns, FKs, unique, checks, enums, API path) of a table from the architect-sdk packages. Accepts `schema.table` or `table`.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"table":{"type":"string"}},"required":["table"]})
    }
    async fn call(&self, _ctx: &mut ToolContext, a: TableSchemaArgs) -> Result<String, ToolErr> {
        let schema = self.0.app.schema();
        let out = match schema.table(&a.table) {
            Some(t) => Ok(bounded(schema.render(&[t]))),
            None => {
                let close: Vec<String> = schema.search(&a.table).iter().take(10).map(|t| t.qualified()).collect();
                Err(ToolErr(format!("unknown table '{}'. Similar: {}", a.table, close.join(", "))))
            }
        };
        self.0.record(format!("table_schema {}", a.table), &out);
        out
    }
}

struct TraceLogs(ToolEnv);
#[derive(Deserialize)]
struct TraceLogsArgs {
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    min_level: Option<String>,
}

impl rig_core::tool::Tool for TraceLogs {
    const NAME: &'static str = "trace_logs";
    type Args = TraceLogsArgs;
    type Output = String;
    type Error = ToolErr;
    fn description(&self) -> String {
        "More log lines of this request (including routine ones omitted from the prompt), filtered by service / text / minimum level.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "service":{"type":"string"},"contains":{"type":"string"},
            "min_level":{"type":"string","enum":["TRACE","DEBUG","INFO","WARN","ERROR"]}}})
    }
    async fn call(&self, _ctx: &mut ToolContext, a: TraceLogsArgs) -> Result<String, ToolErr> {
        let call = format!("trace_logs service={:?} contains={:?} level={:?}", a.service, a.contains, a.min_level);
        let min = a.min_level.as_deref().and_then(Level::parse).unwrap_or(Level::Trace);
        let lines: Vec<String> = self
            .0
            .trace
            .events
            .iter()
            .chain(&self.0.trace.correlated)
            .filter(|e| a.service.as_ref().is_none_or(|s| &e.service == s))
            .filter(|e| e.level >= min)
            .filter(|e| a.contains.as_ref().is_none_or(|c| e.message.contains(c.as_str()) || e.fields.to_string().contains(c.as_str())))
            .take(40)
            .map(|e| e.prompt_line(10))
            .collect();
        let out = Ok(if lines.is_empty() { "no matching lines".into() } else { bounded(lines.join("\n")) });
        self.0.record(call, &out);
        out
    }
}

// ── run ──────────────────────────────────────────────────────────────────────

pub async fn run(app: &Arc<AppState>, ctx: &TriageContext) -> Result<AgentOutcome> {
    let key = app
        .cfg
        .llm
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("GROQ_API_KEY is not configured — context was assembled but no model was called"))?;
    let env = ToolEnv { app: app.clone(), trace: Arc::new(ctx.trace.clone()), calls: Arc::new(Mutex::new(vec![])) };

    let mut use_tools = app.cfg.llm.tools;
    let (mut failures, mut rate_limited) = (0u32, 0u32);
    let started = std::time::Instant::now();
    loop {
        let attempt = failures + rate_limited + 1;
        match run_once(app, &key, ctx, &env, use_tools).await {
            Ok((report, mut usage)) => {
                usage["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
                usage["attempts"] = json!(attempt);
                usage["tool_calls"] = json!(env.call_names());
                return Ok(AgentOutcome { report, usage });
            }
            Err(e) => {
                let msg = e.to_string();
                // Groq rejects a final tool call that drifts from the schema but returns
                // what the model wrote; a lenient parse usually recovers the report.
                if let Some(report) = salvage_failed_generation(&msg) {
                    tracing::info!("recovered report from a schema-rejected tool call");
                    let usage = json!({
                        "model": app.cfg.llm.model,
                        "salvaged": true,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "attempts": attempt,
                        "tools_enabled": use_tools,
                        "tool_calls": env.call_names(),
                    });
                    return Ok(AgentOutcome { report, usage });
                }
                if let Some(wait) = rate_limit_wait(&msg) {
                    // Waiting out a rate limit is not a failed attempt, but don't wait forever.
                    rate_limited += 1;
                    if rate_limited > 6 {
                        return Err(e);
                    }
                    if let Some(used) = provider_used_tokens(&msg) {
                        app.llm_budget.sync_used(used);
                    }
                    tracing::warn!(wait_s = wait.as_secs_f32(), "LLM rate limited; backing off");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                failures += 1;
                if failures >= 3 {
                    return Err(e);
                }
                // Invented tool names, ignored tool_choice, turn limits: retry as a single
                // forced-report turn that carries the tool findings gathered so far.
                tracing::warn!(error = %crate::redact::truncate(&msg, 300), tools = use_tools, "triage attempt failed; retrying without tools");
                use_tools = false;
            }
        }
    }
}

/// Groq reports the org's spend in the current window: "Used 4419".
fn provider_used_tokens(msg: &str) -> Option<u64> {
    let re = regex::Regex::new(r"Used (\d+)").ok()?;
    re.captures(msg).and_then(|c| c[1].parse().ok())
}

async fn run_once(
    app: &Arc<AppState>,
    key: &str,
    ctx: &TriageContext,
    env: &ToolEnv,
    use_tools: bool,
) -> Result<(TriageReport, Value)> {
    let cfg = &app.cfg.llm;
    let model = groq::new(key.to_string()).chat(cfg.model.clone());
    // ~3.2 chars/token for this mix of prose, JSON and code; tool and report
    // schemas add ~1.5k tokens, the answer up to ~1.2k.
    let extra = if use_tools { 0 } else { env.findings(app.cfg.llm.context_budget_chars / 2).len() };
    let estimate = ((PREAMBLE.len() + ctx.prompt.len() + extra) as u64 * 10 / 32) + if use_tools { 2700 } else { 2000 };
    let turns = if use_tools { cfg.max_turns.max(2) } else { 1 };
    let pacer = Pacer { app: app.clone(), first_call_estimate: estimate, last_input: Arc::new(AtomicU64::new(0)), max_turns: turns };
    let mut builder = AgentBuilder::new(model)
        .preamble(PREAMBLE)
        .temperature(cfg.temperature)
        .max_tokens(3000)
        .output_schema::<TriageReport>()
        .output_mode(OutputMode::Tool)
        .add_hook(pacer);
    if !use_tools {
        // Only the report tool exists, and calling it is mandatory.
        builder = builder.tool_choice(ToolChoice::Required);
    }
    if use_tools {
        builder = builder
            .tool(ReadSource(env.clone()))
            .tool(SearchCode(env.clone()))
            .tool(TableSchema(env.clone()))
            .tool(TraceLogs(env.clone()));
    }
    let agent = builder.build();
    let findings = if use_tools { String::new() } else { env.findings(app.cfg.llm.context_budget_chars / 2) };
    let findings = if findings.is_empty() {
        findings
    } else {
        format!("\n## Results of the tools you already called\n{findings}")
    };
    let resp = agent
        .prompt(format!(
            "{}{findings}\n\nTriage this incident. {}",
            ctx.prompt,
            if use_tools {
                format!(
                    "Tools available: read_source, search_code, table_schema, trace_logs; no others exist. \
                     You have at most {} tool-call rounds, then submit the report with the `final_result` tool. \
                     Submit right away if the context above is enough.",
                    turns - 1
                )
            } else {
                "No investigation tools are available. Submit the report now with the `final_result` tool.".to_string()
            }
        ))
        .max_turns(turns)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let out = resp.output();
    let report: TriageReport = parse_report(&out)?;
    let u = &resp.usage;
    let usage = json!({
        "model": cfg.model,
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "total_tokens": u.total_tokens,
        "cost_usd": u.cost.as_ref().map(|c| c.total),
        "model_calls": resp.completion_calls.len(),
        "tools_enabled": use_tools,
    });
    Ok((report, usage))
}

/// The output tool returns the JSON arguments; tolerate a fenced block or prose around it.
fn parse_report(out: &str) -> Result<TriageReport> {
    if let Ok(r) = serde_json::from_str::<TriageReport>(out) {
        return Ok(r.normalise());
    }
    let start = out.find('{').ok_or_else(|| anyhow!("model returned no JSON report: {}", crate::redact::truncate(out, 300)))?;
    let end = out.rfind('}').ok_or_else(|| anyhow!("unterminated JSON report"))?;
    let v: Value = serde_json::from_str(&out[start..=end]).map_err(|e| anyhow!("report is not JSON: {e}"))?;
    // `{"name": "...", "arguments": {...}}` wrapper from a raw tool call.
    let v = if v.get("arguments").is_some() && v.get("title").is_none() { v["arguments"].clone() } else { v };
    serde_json::from_value::<TriageReport>(v)
        .map(TriageReport::normalise)
        .map_err(|e| anyhow!("report did not match schema: {e}"))
}

/// Pull `failed_generation` out of a Groq `tool_use_failed` error and parse it.
fn salvage_failed_generation(msg: &str) -> Option<TriageReport> {
    if !msg.contains("tool_use_failed") {
        return None;
    }
    let start = msg.find('{')?;
    let end = msg.rfind('}')?;
    let body: Value = serde_json::from_str(&msg[start..=end]).ok()?;
    let generation = body["error"]["failed_generation"].as_str()?;
    parse_report(generation).ok()
}

/// Groq says "Please try again in 7.66s" (or ms) on 429s.
fn rate_limit_wait(msg: &str) -> Option<Duration> {
    if !(msg.contains("429") || msg.contains("rate_limit") || msg.contains("Rate limit")) {
        return None;
    }
    let re = regex::Regex::new(r"try again in (?:(\d+)m)?([\d.]+)(ms|s)").ok()?;
    let wait = re.captures(msg).map(|c| {
        let mins: f64 = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0.0);
        let v: f64 = c[2].parse().unwrap_or(10.0);
        let secs = if &c[3] == "ms" { v / 1000.0 } else { v };
        mins * 60.0 + secs
    });
    Some(Duration::from_secs_f64(wait.unwrap_or(20.0).clamp(1.0, 90.0) + 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_parsing() {
        let w = rate_limit_wait("status 429: Rate limit reached ... Please try again in 7.66s").unwrap();
        assert!(w.as_secs_f64() > 8.0 && w.as_secs_f64() < 9.0);
        assert!(rate_limit_wait("status 400 bad request").is_none());
        let w = rate_limit_wait("429 try again in 1m2.5s").unwrap();
        assert!(w.as_secs_f64() > 63.0);
    }

    #[test]
    fn report_parses_with_noise() {
        let json = r#"{"title":"t","severity":"high","category":"code_bug","origin_service":"authrs","failure_chain":[],"root_cause":"r","frontend_summary":"f","frontend_action":"a","is_frontend_issue":false,"owner":"o","suggested_fix":"s","fix_location":"","patch":"","db_notes":"","confidence":0.8,"evidence":[],"missing_context":[]}"#;
        assert!(parse_report(json).is_ok());
        assert!(parse_report(&format!("here:\n```json\n{json}\n```")).is_ok());
    }

    #[test]
    fn salvages_schema_drift_from_groq_error() {
        let msg = r#"ProviderResponseError: status 400 Bad Request: {"error":{"message":"Tool call validation failed","type":"invalid_request_error","code":"tool_use_failed","failed_generation":"{\"name\": \"final_result\", \"arguments\": {\"title\": \"x\", \"severity\": \"Major\", \"category\": \"backend\", \"origin_service\": \"workflow-engine\", \"failure_chain\": [{\"service\": \"workflow-engine\", \"description\": \"loop failed\"}], \"root_cause\": \"r\", \"frontend_summary\": \"f\", \"confidence\": \"92\", \"is_frontend_issue\": \"false\"}}"}}"#;
        let r = salvage_failed_generation(msg).expect("salvaged");
        assert_eq!(r.severity, "high");
        assert_eq!(r.category, "unknown");
        assert_eq!(r.failure_chain[0].observation, "loop failed");
        assert!((r.confidence - 0.92).abs() < 1e-9);
        assert!(!r.is_frontend_issue);
    }
}
