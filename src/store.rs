//! Local SQLite store: the log index (keyed by trace id) and incidents.
//!
//! This is McQueen's own Loki/Tempo stand-in. Volume on the dev box is ~100k
//! lines/day across both stacks, which SQLite in WAL mode handles easily; old
//! rows are pruned by age and by a hard row cap.

use std::{str::FromStr, time::Duration};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous},
    Row, SqlitePool,
};

use crate::model::{Incident, Level, LogEvent};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS logs (
    id        INTEGER PRIMARY KEY,
    ts        INTEGER NOT NULL,
    service   TEXT NOT NULL,
    container TEXT NOT NULL,
    level     TEXT NOT NULL,
    target    TEXT,
    message   TEXT NOT NULL,
    trace_id  TEXT,
    fields    TEXT NOT NULL DEFAULT '{}',
    spans     TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS logs_trace ON logs(trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS logs_ts ON logs(ts);
CREATE INDEX IF NOT EXISTS logs_service_ts ON logs(service, ts);
CREATE INDEX IF NOT EXISTS logs_problems ON logs(ts) WHERE level IN ('ERROR', 'WARN');

CREATE TABLE IF NOT EXISTS incidents (
    id           TEXT PRIMARY KEY,
    fingerprint  TEXT NOT NULL UNIQUE,
    trace_id     TEXT,
    source       TEXT NOT NULL,
    service      TEXT,
    anchor_log_id INTEGER,
    title        TEXT NOT NULL,
    level        TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'open',
    triage_state TEXT NOT NULL DEFAULT 'pending',
    severity     TEXT,
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    occurrences  INTEGER NOT NULL DEFAULT 1,
    fe_payload   TEXT,
    report       TEXT,
    context      TEXT,
    error        TEXT,
    llm_usage    TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS incidents_last_seen ON incidents(last_seen);
CREATE INDEX IF NOT EXISTS incidents_trace ON incidents(trace_id);

CREATE TABLE IF NOT EXISTS cursors (
    container_id TEXT PRIMARY KEY,
    container    TEXT NOT NULL,
    last_ts      INTEGER NOT NULL
);
"#;

fn micros(ts: DateTime<Utc>) -> i64 {
    ts.timestamp_micros()
}

fn from_micros(v: i64) -> DateTime<Utc> {
    Utc.timestamp_micros(v).single().unwrap_or_else(Utc::now)
}

fn json_col(row: &SqliteRow, col: &str) -> Option<Value> {
    row.try_get::<Option<String>, _>(col)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn row_to_event(row: &SqliteRow) -> LogEvent {
    LogEvent {
        id: row.get("id"),
        ts: from_micros(row.get("ts")),
        service: row.get("service"),
        container: row.get("container"),
        level: Level::parse(row.get::<&str, _>("level")).unwrap_or(Level::Info),
        target: row.get("target"),
        message: row.get("message"),
        trace_id: row.get("trace_id"),
        fields: json_col(row, "fields").unwrap_or(Value::Null),
        spans: json_col(row, "spans")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default(),
        correlation: None,
    }
}

fn row_to_incident(row: &SqliteRow) -> Incident {
    Incident {
        id: row.get("id"),
        fingerprint: row.get("fingerprint"),
        trace_id: row.get("trace_id"),
        source: row.get("source"),
        service: row.get("service"),
        anchor_log_id: row.get("anchor_log_id"),
        title: row.get("title"),
        level: row.get("level"),
        status: row.get("status"),
        triage_state: row.get("triage_state"),
        severity: row.get("severity"),
        first_seen: from_micros(row.get("first_seen")),
        last_seen: from_micros(row.get("last_seen")),
        occurrences: row.get("occurrences"),
        fe_payload: json_col(row, "fe_payload"),
        report: json_col(row, "report"),
        context: json_col(row, "context"),
        error: row.get("error"),
        llm_usage: json_col(row, "llm_usage"),
        created_at: from_micros(row.get("created_at")),
        updated_at: from_micros(row.get("updated_at")),
    }
}

/// Filters for the log explorer.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct LogQuery {
    pub service: Option<String>,
    pub level: Option<String>,
    pub q: Option<String>,
    pub trace_id: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub before_id: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct IncidentQuery {
    pub status: Option<String>,
    pub service: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ServiceStats {
    pub service: String,
    pub container: String,
    pub total: i64,
    pub with_trace: i64,
    /// Lines carrying an empty `trace_id` field: the service has the field but got no traceparent.
    pub empty_trace_field: i64,
    pub errors: i64,
    pub warns: i64,
    pub last_ts: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct Bucket {
    pub service: String,
    pub bucket: DateTime<Utc>,
    pub total: i64,
    pub errors: i64,
    pub warns: i64,
}

/// Fields for opening (or bumping) an incident.
pub struct NewIncident<'a> {
    pub fingerprint: &'a str,
    pub trace_id: Option<&'a str>,
    pub source: &'a str,
    pub service: Option<&'a str>,
    pub anchor_log_id: Option<i64>,
    pub title: &'a str,
    pub level: &'a str,
    pub seen: DateTime<Utc>,
}

impl Store {
    pub async fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new().max_connections(6).connect_with(opts).await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Insert a batch in one transaction; ids are written back into `events`.
    pub async fn insert_events(&self, events: &mut [LogEvent]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for ev in events.iter_mut() {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO logs (ts, service, container, level, target, message, trace_id, fields, spans)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
            )
            .bind(micros(ev.ts))
            .bind(&ev.service)
            .bind(&ev.container)
            .bind(ev.level.as_str())
            .bind(&ev.target)
            .bind(&ev.message)
            .bind(&ev.trace_id)
            .bind(ev.fields.to_string())
            .bind(serde_json::to_string(&ev.spans)?)
            .fetch_one(&mut *tx)
            .await?;
            ev.id = id;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn cursor(&self, container_id: &str) -> Result<Option<DateTime<Utc>>> {
        let v: Option<i64> = sqlx::query_scalar("SELECT last_ts FROM cursors WHERE container_id = ?")
            .bind(container_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(v.map(from_micros))
    }

    pub async fn set_cursor(&self, container_id: &str, container: &str, ts: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "INSERT INTO cursors (container_id, container, last_ts) VALUES (?, ?, ?)
             ON CONFLICT(container_id) DO UPDATE SET last_ts = MAX(last_ts, excluded.last_ts)",
        )
        .bind(container_id)
        .bind(container)
        .bind(micros(ts))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn logs_by_trace(&self, trace_id: &str, limit: i64) -> Result<Vec<LogEvent>> {
        let rows = sqlx::query("SELECT * FROM logs WHERE trace_id = ? ORDER BY ts, id LIMIT ?")
            .bind(trace_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_event).collect())
    }

    /// Untraced events in a time window, optionally restricted to some services.
    pub async fn untraced_window(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<LogEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM logs WHERE ts BETWEEN ? AND ? AND trace_id IS NULL ORDER BY ts, id LIMIT ?",
        )
        .bind(micros(from))
        .bind(micros(to))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_event).collect())
    }

    /// Container start/exit/OOM events (stored with `target = 'docker'`) in a window.
    pub async fn lifecycle_window(&self, from: DateTime<Utc>, to: DateTime<Utc>, limit: i64) -> Result<Vec<LogEvent>> {
        let rows = sqlx::query("SELECT * FROM logs WHERE ts BETWEEN ? AND ? AND target = 'docker' ORDER BY ts, id LIMIT ?")
            .bind(micros(from))
            .bind(micros(to))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_event).collect())
    }

    /// WARN/ERROR events in a window, any service.
    pub async fn problems_window(&self, from: DateTime<Utc>, to: DateTime<Utc>, limit: i64) -> Result<Vec<LogEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM logs WHERE ts BETWEEN ? AND ? AND level IN ('ERROR', 'WARN') ORDER BY ts, id LIMIT ?",
        )
        .bind(micros(from))
        .bind(micros(to))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_event).collect())
    }

    /// Events of one service around a moment (context for untraced errors).
    pub async fn service_window(
        &self,
        service: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<LogEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM logs WHERE service = ? AND ts BETWEEN ? AND ? ORDER BY ts, id LIMIT ?",
        )
        .bind(service)
        .bind(micros(from))
        .bind(micros(to))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_event).collect())
    }

    pub async fn log_by_id(&self, id: i64) -> Result<Option<LogEvent>> {
        let row = sqlx::query("SELECT * FROM logs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_event))
    }

    pub async fn search_logs(&self, q: &LogQuery) -> Result<Vec<LogEvent>> {
        let mut sql = String::from("SELECT * FROM logs WHERE 1=1");
        let mut binds: Vec<Bind> = vec![];
        if let Some(s) = q.service.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND service = ?");
            binds.push(Bind::Text(s.into()));
        }
        if let Some(level) = q.level.as_deref().and_then(Level::parse) {
            let allowed: Vec<&str> = [Level::Trace, Level::Debug, Level::Info, Level::Warn, Level::Error]
                .into_iter()
                .filter(|l| *l >= level)
                .map(Level::as_str)
                .collect();
            sql.push_str(&format!(
                " AND level IN ({})",
                allowed.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(",")
            ));
        }
        if let Some(t) = q.trace_id.as_deref().and_then(crate::parse::trace_id_from_input) {
            sql.push_str(" AND trace_id = ?");
            binds.push(Bind::Text(t));
        }
        if let Some(text) = q.q.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND (message LIKE ? OR fields LIKE ? OR target LIKE ?)");
            let like = format!("%{}%", text.replace('%', "\\%"));
            binds.push(Bind::Text(like.clone()));
            binds.push(Bind::Text(like.clone()));
            binds.push(Bind::Text(like));
        }
        if let Some(since) = q.since {
            sql.push_str(" AND ts >= ?");
            binds.push(Bind::Int(micros(since)));
        }
        if let Some(until) = q.until {
            sql.push_str(" AND ts <= ?");
            binds.push(Bind::Int(micros(until)));
        }
        if let Some(before) = q.before_id {
            sql.push_str(" AND id < ?");
            binds.push(Bind::Int(before));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        binds.push(Bind::Int(q.limit.unwrap_or(200).clamp(1, 2000)));

        let mut query = sqlx::query(&sql);
        for b in binds {
            query = match b {
                Bind::Text(s) => query.bind(s),
                Bind::Int(i) => query.bind(i),
            };
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_event).collect())
    }

    pub async fn service_stats(&self, since: DateTime<Utc>) -> Result<Vec<ServiceStats>> {
        let rows = sqlx::query(
            "SELECT service, MAX(container) AS container, COUNT(*) AS total,
                    SUM(trace_id IS NOT NULL) AS with_trace,
                    SUM(trace_id IS NULL AND json_extract(fields, '$.trace_id') = '') AS empty_trace_field,
                    SUM(level = 'ERROR') AS errors, SUM(level = 'WARN') AS warns, MAX(ts) AS last_ts
             FROM logs WHERE ts >= ? GROUP BY service ORDER BY service",
        )
        .bind(micros(since))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ServiceStats {
                service: r.get("service"),
                container: r.get("container"),
                total: r.get("total"),
                with_trace: r.get::<Option<i64>, _>("with_trace").unwrap_or(0),
                empty_trace_field: r.get::<Option<i64>, _>("empty_trace_field").unwrap_or(0),
                errors: r.get::<Option<i64>, _>("errors").unwrap_or(0),
                warns: r.get::<Option<i64>, _>("warns").unwrap_or(0),
                last_ts: r.get::<Option<i64>, _>("last_ts").map(from_micros),
            })
            .collect())
    }

    pub async fn histogram(&self, since: DateTime<Utc>, bucket: Duration) -> Result<Vec<Bucket>> {
        let width = bucket.as_micros() as i64;
        let rows = sqlx::query(
            "SELECT service, (ts / ?) * ? AS b, COUNT(*) AS total,
                    SUM(level = 'ERROR') AS errors, SUM(level = 'WARN') AS warns
             FROM logs WHERE ts >= ? GROUP BY service, b ORDER BY b",
        )
        .bind(width)
        .bind(width)
        .bind(micros(since))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Bucket {
                service: r.get("service"),
                bucket: from_micros(r.get("b")),
                total: r.get("total"),
                errors: r.get::<Option<i64>, _>("errors").unwrap_or(0),
                warns: r.get::<Option<i64>, _>("warns").unwrap_or(0),
            })
            .collect())
    }

    // ── incidents ───────────────────────────────────────────────────────────

    /// Open an incident for `fingerprint`, or bump the existing one. Returns
    /// `(incident, created)`. A resolved incident that recurs is reopened.
    pub async fn upsert_incident(&self, n: NewIncident<'_>) -> Result<(Incident, bool)> {
        let now = micros(Utc::now());
        if let Some(existing) = self.incident_by_fingerprint(n.fingerprint).await? {
            let level = if level_rank(n.level) > level_rank(&existing.level) { n.level } else { existing.level.as_str() };
            sqlx::query(
                "UPDATE incidents SET last_seen = MAX(last_seen, ?), occurrences = occurrences + 1,
                        level = ?, status = CASE WHEN status = 'resolved' THEN 'open' ELSE status END,
                        triage_state = CASE WHEN status = 'resolved' THEN 'pending' ELSE triage_state END,
                        service = COALESCE(service, ?), anchor_log_id = COALESCE(anchor_log_id, ?),
                        updated_at = ? WHERE id = ?",
            )
            .bind(micros(n.seen))
            .bind(level)
            .bind(n.service)
            .bind(n.anchor_log_id)
            .bind(now)
            .bind(&existing.id)
            .execute(&self.pool)
            .await?;
            let inc = self.incident(&existing.id).await?.expect("incident exists");
            return Ok((inc, false));
        }
        let id = format!("MQ-{}", uuid::Uuid::new_v4().simple().to_string()[..8].to_uppercase());
        sqlx::query(
            "INSERT INTO incidents (id, fingerprint, trace_id, source, service, anchor_log_id, title, level, first_seen, last_seen, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(fingerprint) DO NOTHING",
        )
        .bind(&id)
        .bind(n.fingerprint)
        .bind(n.trace_id)
        .bind(n.source)
        .bind(n.service)
        .bind(n.anchor_log_id)
        .bind(crate::redact::truncate(n.title, 240))
        .bind(n.level)
        .bind(micros(n.seen))
        .bind(micros(n.seen))
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        let inc = self
            .incident_by_fingerprint(n.fingerprint)
            .await?
            .expect("incident just inserted");
        let created = inc.id == id;
        Ok((inc, created))
    }

    pub async fn incident(&self, id: &str) -> Result<Option<Incident>> {
        let row = sqlx::query("SELECT * FROM incidents WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_incident))
    }

    pub async fn incident_by_fingerprint(&self, fp: &str) -> Result<Option<Incident>> {
        let row = sqlx::query("SELECT * FROM incidents WHERE fingerprint = ?")
            .bind(fp)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_incident))
    }

    pub async fn list_incidents(&self, q: &IncidentQuery) -> Result<Vec<Incident>> {
        let mut sql = String::from(
            "SELECT id, fingerprint, trace_id, source, service, anchor_log_id, title, level, status, triage_state, severity,
                    first_seen, last_seen, occurrences, NULL AS fe_payload,
                    json_object('title', json_extract(report, '$.title'), 'frontend_summary', json_extract(report, '$.frontend_summary'), 'origin_service', json_extract(report, '$.origin_service'), 'category', json_extract(report, '$.category')) AS report,
                    NULL AS context, error, NULL AS llm_usage, created_at, updated_at
             FROM incidents WHERE 1=1",
        );
        let mut binds: Vec<String> = vec![];
        if let Some(s) = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all") {
            sql.push_str(" AND status = ?");
            binds.push(s.into());
        }
        if let Some(s) = q.service.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND service = ?");
            binds.push(s.into());
        }
        if let Some(text) = q.q.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND (title LIKE ? OR trace_id LIKE ? OR id LIKE ?)");
            let like = format!("%{text}%");
            binds.extend([like.clone(), like.clone(), like]);
        }
        sql.push_str(" ORDER BY last_seen DESC LIMIT ?");
        let mut query = sqlx::query(&sql);
        for b in binds {
            query = query.bind(b);
        }
        let rows = query
            .bind(q.limit.unwrap_or(200).clamp(1, 1000))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_incident).collect())
    }

    pub async fn set_triage_state(&self, id: &str, state: &str, error: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE incidents SET triage_state = ?, error = ?, updated_at = ? WHERE id = ?")
            .bind(state)
            .bind(error)
            .bind(micros(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_triage(
        &self,
        id: &str,
        report: &Value,
        context: &Value,
        usage: &Value,
        severity: Option<&str>,
        service: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE incidents SET triage_state = 'done', report = ?, context = ?, llm_usage = ?, severity = ?,
                    service = COALESCE(?, service), error = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(report.to_string())
        .bind(context.to_string())
        .bind(usage.to_string())
        .bind(severity)
        .bind(service)
        .bind(micros(Utc::now()))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn save_context(&self, id: &str, context: &Value) -> Result<()> {
        sqlx::query("UPDATE incidents SET context = ?, updated_at = ? WHERE id = ?")
            .bind(context.to_string())
            .bind(micros(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_fe_payload(&self, id: &str, payload: &Value) -> Result<()> {
        sqlx::query("UPDATE incidents SET fe_payload = ?, source = 'frontend', updated_at = ? WHERE id = ?")
            .bind(payload.to_string())
            .bind(micros(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_status(&self, id: &str, status: &str) -> Result<bool> {
        let res = sqlx::query("UPDATE incidents SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(micros(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn incident_counts(&self) -> Result<Value> {
        let row = sqlx::query(
            "SELECT SUM(status = 'open') AS open, SUM(status = 'acknowledged') AS acknowledged,
                    SUM(status = 'resolved') AS resolved, SUM(triage_state = 'done') AS triaged,
                    SUM(triage_state = 'pending') AS queued, SUM(triage_state = 'running') AS running
             FROM incidents",
        )
        .fetch_one(&self.pool)
        .await?;
        let n = |c: &str| row.get::<Option<i64>, _>(c).unwrap_or(0);
        Ok(serde_json::json!({
            "open": n("open"), "acknowledged": n("acknowledged"), "resolved": n("resolved"),
            "triaged": n("triaged"), "queued": n("queued"), "running": n("running"),
        }))
    }

    /// Incidents left `running` by a crash go back to `pending` on boot.
    pub async fn reset_stuck_triage(&self) -> Result<Vec<String>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM incidents WHERE triage_state IN ('running', 'pending')")
            .fetch_all(&self.pool)
            .await?;
        sqlx::query("UPDATE incidents SET triage_state = 'pending' WHERE triage_state = 'running'")
            .execute(&self.pool)
            .await?;
        Ok(ids)
    }

    pub async fn prune(&self, retention: Duration, max_rows: i64) -> Result<u64> {
        let cutoff = micros(Utc::now() - chrono::Duration::from_std(retention)?);
        let mut removed = sqlx::query("DELETE FROM logs WHERE ts < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs").fetch_one(&self.pool).await?;
        if count > max_rows {
            removed += sqlx::query("DELETE FROM logs WHERE id IN (SELECT id FROM logs ORDER BY id LIMIT ?)")
                .bind(count - max_rows)
                .execute(&self.pool)
                .await?
                .rows_affected();
        }
        // Incidents are small; keep a month.
        let inc_cutoff = micros(Utc::now() - chrono::Duration::days(30));
        sqlx::query("DELETE FROM incidents WHERE last_seen < ? AND status = 'resolved'")
            .bind(inc_cutoff)
            .execute(&self.pool)
            .await?;
        Ok(removed)
    }
}

enum Bind {
    Text(String),
    Int(i64),
}

fn level_rank(level: &str) -> u8 {
    Level::parse(level).map(|l| l as u8).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(service: &str, level: Level, trace: Option<&str>, msg: &str) -> LogEvent {
        LogEvent {
            id: 0,
            ts: Utc::now(),
            service: service.into(),
            container: format!("c-{service}"),
            level,
            target: Some(format!("{service}::x")),
            message: msg.into(),
            trace_id: trace.map(str::to_string),
            fields: serde_json::json!({"trace_id": trace.unwrap_or("")}),
            spans: vec!["request".into()],
            correlation: None,
        }
    }

    #[tokio::test]
    async fn round_trip_logs_and_incidents() {
        let dir = std::env::temp_dir().join(format!("mq-test-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir.join("t.db")).await.unwrap();
        let tid = "4bf92f3577b34da6a3ce929d0e0e4736";
        let mut batch = vec![
            ev("launchpad", Level::Info, Some(tid), "proxy"),
            ev("authrs", Level::Error, None, "boom"),
        ];
        store.insert_events(&mut batch).await.unwrap();
        assert!(batch[0].id > 0 && batch[1].id > batch[0].id);
        assert_eq!(store.logs_by_trace(tid, 10).await.unwrap().len(), 1);

        let stats = store.service_stats(Utc::now() - chrono::Duration::hours(1)).await.unwrap();
        let authrs = stats.iter().find(|s| s.service == "authrs").unwrap();
        assert_eq!((authrs.errors, authrs.empty_trace_field), (1, 1));

        let found = store
            .search_logs(&LogQuery { level: Some("warn".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(found.len(), 1);

        let n = NewIncident {
            fingerprint: "trace:x",
            trace_id: Some(tid),
            source: "backend",
            service: Some("authrs"),
            anchor_log_id: Some(batch[1].id),
            title: "boom",
            level: "WARN",
            seen: Utc::now(),
        };
        let (a, created) = store.upsert_incident(n).await.unwrap();
        assert!(created);
        let (b, created) = store
            .upsert_incident(NewIncident { fingerprint: "trace:x", trace_id: Some(tid), source: "backend", service: None, anchor_log_id: None, title: "boom", level: "ERROR", seen: Utc::now() })
            .await
            .unwrap();
        assert!(!created);
        assert_eq!((a.id, b.occurrences, b.level.as_str()), (b.id.clone(), 2, "ERROR"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
