# McQueen: AI triage for the Regere microservice stack

When a frontend developer sees a `500` or an `undefined` field, the real cause is
usually a few hops back: launchpad → core → authrs / workflow-engine / decision-hub.
McQueen closes that gap. It ties the browser request to every service it touched,
gathers the code and database schema involved, and asks an LLM for a root cause
written for the frontend developer. It runs as a single Rust binary next to the
stacks on regere-yotta, with an Atlassian Design System UI.

```
 Browser (client-ui-next)                       regere-yotta
 ┌────────────────────────┐   traceparent    ┌──────────────────────────────────────────────┐
 │ McQueen SDK            │ ───────────────▶ │ launchpad → core → authrs / workflow-engine  │
 │  • stamps traceparent  │                  │   → decision-hub → message-flow → …          │
 │  • reports 5xx + body  │                  │        (JSON logs carry trace_id)            │
 └──────────┬─────────────┘                  └──────────────────┬───────────────────────────┘
            │ POST /api/ingest/frontend                         │ docker logs (read-only proxy)
            ▼                                                   ▼
 ┌────────────────────────────────────────────────────────────────────────────────────────────┐
 │ McQueen                                                                                    │
 │  collector (bollard) → parser → SQLite log index (by trace_id) → detector → incident       │
 │                                                                      │ settle 8s           │
 │  context: request journey (traced + key-linked lines, call graph, deploy/OOM events)       │
 │           + source at each logging site (repos, read-only) + tables involved (packages)    │
 │                                                                      ▼                     │
 │  rig agent (Groq gpt-oss-120b) ── read_source / search_code / table_schema / trace_logs    │
 │                                                                      ▼                     │
 │  report: origin service, failure chain, root cause, plain-English summary, fix + location  │
 └───────────────────────────────────────┬────────────────────────────────────────────────────┘
                                         ▼
                      UI (Atlaskit) · Google Chat / Slack · DevTools console link
```

## The three layers

### 1. W3C `traceparent` context bridge

One click in the browser should give one `trace_id` across every service. Status
measured from the live logs on regere-yotta (24h):

| Service | Trace coverage | Why |
|---|---|---|
| launchpad | ~100% | Reads/creates `traceparent`, puts `trace_id` on every log line, forwards it to upstreams (`service_registry.rs:568`), echoes it on responses. |
| workflow-engine | ~100% | Execution span carries the trace id. |
| authrs | partial | Receives it from launchpad's session calls; calls from core arrive without it. |
| core (service-kit) | ≈0% (6 of 24k lines) | **The gap.** service-kit never adds architect-sdk's `trace_id_layer()`, so the header launchpad forwards is dropped. Core's own calls to authrs and decision-hub then have nothing to forward. |
| decision-hub | 0% (`trace_id: ""`) | Has the ingress middleware; callers (core) don't send the header. |

**The one-line fix with the biggest payoff** is in service-kit's router (outermost layer):

```rust
let app = axum::Router::new()
    .merge(common_routes_with_ready(state))
    .nest("/api/v1", api_v1)
    .layer(architect_sdk::trace_id_layer()); // ← read traceparent, put trace_id on every line, forward downstream
```

architect-sdk's authrs client already forwards `outbound_traceparent()`, and
decision-hub already reads the header. So this one change carries traces through
core → authrs and core → decision-hub. Background work (`tokio::spawn`, jobq
workers) needs the trace id captured before spawning, as service-kit's workflow path already does.

The **Services** page tracks this per service and flags each one as
*propagating*, *partial*, *not receiving* or *no trace field*. Until the gaps are
closed, McQueen links untraced lines to a request by shared keys (`user_id`,
`execution_id`, `task`, `resource`…) inside the request's time window. These lines
are marked `~` in the UI and in the prompt.

**Frontend.** `sdk/browser/mcqueen.ts` (no dependencies) stamps a `traceparent`
on every `fetch`/XHR to the API. When a call fails it posts `{trace_id, method,
url, status, response body, page}` to McQueen and prints
`[McQueen] GET /proxy/… → 502 · trace 4bf9… https://triage.regere.ai/traces/4bf9…`
in DevTools. Without the SDK, a developer can still copy the `traceparent`
response header (launchpad echoes it) from the Network tab and paste it into
*Triage trace*.

### 2. Collection and context

- **Logs.** The collector follows every container in `new-app-dev` and
  `new-platform-dev` through a socket proxy that only allows `GET` on
  containers, events and ping. It parses both tracing-JSON shapes and plain
  text, folding Rust panics and Python tracebacks into one event, and stores
  everything in SQLite indexed by `trace_id`. It resumes from a per-container
  cursor after restarts. Container start, exit, OOM and unhealthy events are
  stored too, so "message-flow was restarting at that moment" is visible.
- **Code.** Every `Cargo.toml` under `REPOS_DIR` maps crate → repo. A log line
  with target `architect_sdk::authrs` resolves to `architect-sdk/src/authrs.rs`,
  and the line comes from matching the log message's literal text. Panics,
  Python frames and `file:line` mentions resolve by path suffix. Repos are
  mounted read-only.
- **Database.** Comes from architect-sdk packages: JSON or zip files in
  `SCHEMA_DIR`, and/or core's read-only `GET /api/v1/config/packages`. McQueen
  never connects to Postgres. Only the tables an incident points at are sent
  (from SQL, `table:` resources, `/package/<pkg>/<entity>` paths, constraint
  names), as compact DDL with their FK neighbours.
- **Jaeger** (optional). If `JAEGER_URL` is set and Jaeger knows the trace, its
  spans are merged into the call graph.

### 3. AI triage

The detector opens one incident per failing trace, or per message
*fingerprint* for untraced errors. It waits for the cascade to settle, then
triages: context (about 4k tokens) goes to a rig agent on Groq
`openai/gpt-oss-120b`. The agent has four read-only tools and returns a typed
report through rig's output-tool mode. Groq rejects JSON mode combined with
tools, so the report comes back as a tool call instead.

Hardening, all learned against the real Groq endpoint:

- **Rate limits.** Every model call is paced under `LLM_TPM_LIMIT` (8k/min on
  the current key). On a 429 the budget syncs to Groq's own count and backs off;
  waiting doesn't count as a failed attempt.
- **Schema drift.** If gpt-oss drifts from the schema, Groq's `failed_generation`
  is parsed leniently instead of paying for another run.
- **Bad tool calls.** If the model invents a tool or ignores the forced final
  turn, one forced-report pass runs and carries the tool findings gathered so far.
- **Secrets.** Redacted before anything leaves the box: connection-string
  passwords, bearer tokens, JWTs, API keys, `password`/`token` fields.

A triage costs about $0.001 in tokens. A single-pass triage takes a few
seconds. Runs that use tools can take up to a minute or two on the 8k/min
tier, because each call waits for budget.

## Deploy on regere-yotta

```bash
# clone or copy this repo to ~/McQueen-Triage on the server, then:
cd ~/McQueen-Triage/deploy
cp mcqueen.env.example mcqueen.env      # GROQ_API_KEY, INGEST_KEY, PUBLIC_URL, webhooks
./sync-repos.sh                         # read-only clones → deploy/repos (cron it)
docker compose up -d --build
```

- UI/API on `127.0.0.1:3028` (free in `docker-ports.csv`). Add
  `deploy/Caddyfile.snippet` to `yotta-infra/Caddyfile` for `triage.regere.ai`.
  It applies basic auth to everything except `/api/ingest/*` and `/api/health`.
- The container joins `new_app_network` only to reach `dev_core:5000` for schema
  packages. It never touches the databases.
- Data is a SQLite file in the `mcqueen_data` volume: 72h of logs (about 100k
  lines/day today) plus 30 days of resolved incidents.
- Add `mcqueen-triage,3028,3028,McQueen-Triage/deploy/docker-compose.yml` to
  `docker-ports.csv`.

### Wire the frontend (client-ui-next)

1. Copy `sdk/browser/mcqueen.ts` and `sdk/browser/McQueenInit.tsx` into the app,
   and render `<McQueenInit />` once in the root layout.
2. Set `NEXT_PUBLIC_MCQUEEN_URL=https://triage.regere.ai` and
   `NEXT_PUBLIC_MCQUEEN_KEY=<INGEST_KEY>` (build-time variables).
3. Optional: when a screen renders bad data, call
   `installMcQueen(...).reportUiError('project name is undefined')`. That files a
   UI incident against the last request's trace.

## Configuration

Secrets and deployment settings come from the environment (see
`deploy/mcqueen.env.example`). The service catalog lives in `mcqueen.toml`: which
containers to watch, each service's layer, repo, hosts and owner, and the
detection rules (`warn_as_error`, `ignore`).

| Variable | Default | |
|---|---|---|
| `GROQ_API_KEY` | — | Without it, context is still assembled and shown; no model call. |
| `LLM_MODEL` | `openai/gpt-oss-120b` | Any Groq chat model with tool calling. |
| `LLM_TPM_LIMIT` | `8000` | Provider tokens/min to pace under; `0` disables pacing. |
| `LLM_MAX_TURNS` / `LLM_TOOLS` | `4` / `true` | Model calls per triage, including the report call. |
| `LLM_CONTEXT_BUDGET_CHARS` | `16000` | Pre-assembled prompt size. |
| `AUTO_TRIAGE`, `TRIAGE_MAX_PER_HOUR`, `TRIAGE_SETTLE_SECS` | `true`, `20`, `8` | Manual triage from the UI ignores the hourly budget. |
| `REPOS_DIR`, `SCHEMA_DIR`, `CORE_PACKAGES_URL` | `repos`, —, — | Code and schema sources. |
| `INGEST_KEY`, `INGEST_ALLOWED_ORIGINS` | —, any | Browser SDK auth and CORS. |
| `AUTH_USER`, `AUTH_PASSWORD` | — | In-app basic auth (or let Caddy do it). |
| `GOOGLE_CHAT_WEBHOOK_URL`, `SLACK_WEBHOOK_URL`, `NOTIFY_MIN_SEVERITY` | —, —, `high` | Alerts link back to `PUBLIC_URL`. |
| `JAEGER_URL` | — | Optional span source. |
| `RETENTION_HOURS`, `MAX_LOG_ROWS`, `BACKFILL_MINUTES` | `72`, `3000000`, `30` | Storage. |

## Local development

```bash
cargo test                                   # parser, store, code/schema resolution, salvage…
cd web && npm ci && npm run dev              # UI on :5173, proxies /api → :3028
GROQ_API_KEY=… REPOS_DIR=~/src SCHEMA_DIR=./schema cargo run -- serve

# Replay real logs without Docker access (read-only pull):
ssh regere-yotta docker logs -t --since 2h dev_core > dev_core.log
cargo run -- import dev_core.log             # service inferred from mcqueen.toml
cargo run -- triage <trace-id|traceparent|incident-id>
cargo run -- check                           # what the code index and schema catalog found
```

## API

| | |
|---|---|
| `GET /api/overview` | Pipeline counters, incident counts, per-service stats. |
| `GET /api/incidents?status&service&q` · `GET/PATCH /api/incidents/:id` · `POST /api/incidents/:id/triage` | |
| `GET /api/traces/:trace_id` · `POST /api/traces/:trace_id/triage` | Accepts a trace id or a whole `traceparent`. |
| `GET /api/logs?service&level&q&trace_id` · `GET /api/logs/stream` (SSE) | |
| `GET /api/events` (SSE) | Incident updates for the UI. |
| `GET /api/services` · `GET /api/schema` · `GET /api/schema/tables/:name` · `POST /api/reload` | |
| `POST /api/ingest/frontend` | Browser SDK (`x-mcqueen-key`, CORS). |
| `GET /api/health` | Unauthenticated. |
