//! Runtime configuration.
//!
//! Two sources:
//! - **environment** for anything deployment-specific or secret (keys, URLs, budgets);
//! - **`mcqueen.toml`** (path in `MCQUEEN_CONFIG`) for the service catalog: which
//!   containers to watch, what each service is, which repo/crates hold its code and
//!   which hostnames other services use to reach it.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub public_url: String,
    pub database_path: PathBuf,
    pub static_dir: PathBuf,
    pub catalog: Catalog,

    pub auth_user: Option<String>,
    pub auth_password: Option<String>,
    pub ingest_key: Option<String>,
    pub ingest_allowed_origins: Vec<String>,

    pub llm: LlmConfig,
    pub triage: TriageConfig,

    pub repos_dir: PathBuf,
    pub schema_dir: Option<PathBuf>,
    pub core_packages_url: Option<String>,
    pub jaeger_url: Option<String>,

    pub google_chat_webhook: Option<String>,
    pub slack_webhook: Option<String>,
    pub notify_min_severity: String,

    pub retention: Duration,
    pub max_log_rows: i64,
    pub backfill: Duration,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_key: Option<String>,
    pub model: String,
    /// Rough prompt budget for the pre-assembled context, in characters (~4 chars/token).
    pub context_budget_chars: usize,
    /// Model calls per triage run, including the final `submit_report` call.
    pub max_turns: usize,
    /// Let the model call `read_source` / `search_code` / ... tools.
    pub tools: bool,
    pub temperature: f64,
    /// Provider tokens-per-minute cap to pace calls under (0 = no pacing). Groq's
    /// free tier is 8000 for gpt-oss-120b.
    pub tpm_limit: u64,
}

#[derive(Debug, Clone)]
pub struct TriageConfig {
    pub auto: bool,
    pub max_per_hour: u32,
    pub concurrency: usize,
    /// Quiet period after the last error of a trace before triaging it, so the
    /// whole cascade across services has landed.
    pub settle: Duration,
}

/// The service catalog loaded from `mcqueen.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Catalog {
    #[serde(default)]
    pub docker: DockerSelect,
    #[serde(default)]
    pub services: Vec<ServiceDef>,
    #[serde(default)]
    pub detection: DetectionRules,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DockerSelect {
    /// Watch every container whose `com.docker.compose.project` label is listed.
    #[serde(default)]
    pub compose_projects: Vec<String>,
    /// Also watch these container names regardless of project.
    #[serde(default)]
    pub include_names: Vec<String>,
    /// Never watch these container names.
    #[serde(default)]
    pub exclude_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceDef {
    pub name: String,
    /// Container names (exact) or prefixes ending in `*`.
    #[serde(default)]
    pub containers: Vec<String>,
    /// `app` | `platform` | `frontend` | `infra`. Infra errors (postgres, rabbitmq…)
    /// are shown as nearby context but never open incidents on their own.
    #[serde(default = "default_layer")]
    pub layer: String,
    #[serde(default)]
    pub description: String,
    /// Hostnames other services use for it (`pf-message-flow`, `authrs`, …).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Repo directory under `REPOS_DIR` holding its source.
    #[serde(default)]
    pub repo: Option<String>,
    /// Who to point at in the report.
    #[serde(default)]
    pub owner: Option<String>,
}

fn default_layer() -> String {
    "app".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DetectionRules {
    /// WARN lines matching any of these (case-insensitive substrings) are treated as errors.
    #[serde(default = "default_warn_patterns")]
    pub warn_as_error: Vec<String>,
    /// Lines matching any of these (case-insensitive substrings) never open incidents.
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl Default for DetectionRules {
    fn default() -> Self {
        Self {
            warn_as_error: default_warn_patterns(),
            ignore: vec![],
        }
    }
}

fn default_warn_patterns() -> Vec<String> {
    [
        "upstream error",
        "connection refused",
        "timed out",
        "timeout",
        "panicked",
        "internal server error",
        "permission denied",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Catalog {
    pub fn service(&self, name: &str) -> Option<&ServiceDef> {
        self.services.iter().find(|s| s.name == name)
    }

    /// Resolve a container to a service name: catalog patterns first, then the
    /// compose service label with the stack prefixes (`pf-`, `dev_`) stripped.
    pub fn service_for_container(&self, container: &str, compose_service: Option<&str>) -> String {
        for s in &self.services {
            if s.containers.iter().any(|p| matches_pattern(p, container)) {
                return s.name.clone();
            }
        }
        let base = compose_service.unwrap_or(container);
        base.trim_start_matches("pf-")
            .trim_start_matches("dev_")
            .replace('_', "-")
    }

    /// Map a hostname seen in a URL (`pf-message-flow`, `authrs`) to a service.
    pub fn service_for_host(&self, host: &str) -> Option<String> {
        let host = host.to_ascii_lowercase();
        self.services
            .iter()
            .find(|s| {
                s.hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
                    || s.containers.iter().any(|p| matches_pattern(p, &host))
                    || s.name.eq_ignore_ascii_case(&host)
            })
            .map(|s| s.name.clone())
    }

    pub fn layer_of(&self, service: &str) -> String {
        self.service(service)
            .map(|s| s.layer.clone())
            .unwrap_or_else(|| "app".into())
    }

    pub fn wants_container(&self, name: &str, project: Option<&str>) -> bool {
        if self.docker.exclude_names.iter().any(|p| matches_pattern(p, name)) {
            return false;
        }
        if self.docker.include_names.iter().any(|p| matches_pattern(p, name)) {
            return true;
        }
        if self.services.iter().any(|s| s.containers.iter().any(|p| matches_pattern(p, name))) {
            return true;
        }
        project.is_some_and(|p| self.docker.compose_projects.iter().any(|w| w == p))
    }
}

/// Exact match, or prefix match when the pattern ends with `*`.
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    env(key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let catalog_path = env_or("MCQUEEN_CONFIG", "mcqueen.toml");
        let catalog: Catalog = match std::fs::read_to_string(&catalog_path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {catalog_path}"))?,
            Err(_) => {
                tracing::warn!(path = %catalog_path, "service catalog not found, using defaults");
                Catalog::default()
            }
        };

        Ok(Self {
            bind: env_or("BIND", "0.0.0.0:3028"),
            public_url: env_or("PUBLIC_URL", "http://localhost:3028").trim_end_matches('/').into(),
            database_path: env_or("DATABASE_PATH", "data/mcqueen.db").into(),
            static_dir: env_or("STATIC_DIR", "web/dist").into(),
            catalog,
            auth_user: env("AUTH_USER"),
            auth_password: env("AUTH_PASSWORD"),
            ingest_key: env("INGEST_KEY"),
            ingest_allowed_origins: env("INGEST_ALLOWED_ORIGINS")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default(),
            llm: LlmConfig {
                api_key: env("GROQ_API_KEY"),
                model: env_or("LLM_MODEL", "openai/gpt-oss-120b"),
                context_budget_chars: env_num("LLM_CONTEXT_BUDGET_CHARS", 16_000),
                max_turns: env_num("LLM_MAX_TURNS", 4),
                tools: env_bool("LLM_TOOLS", true),
                temperature: env_num("LLM_TEMPERATURE", 0.1),
                tpm_limit: env_num("LLM_TPM_LIMIT", 8000),
            },
            triage: TriageConfig {
                auto: env_bool("AUTO_TRIAGE", true),
                max_per_hour: env_num("TRIAGE_MAX_PER_HOUR", 20),
                concurrency: env_num("TRIAGE_CONCURRENCY", 1).max(1),
                settle: Duration::from_secs(env_num("TRIAGE_SETTLE_SECS", 8)),
            },
            repos_dir: env_or("REPOS_DIR", "repos").into(),
            schema_dir: env("SCHEMA_DIR").map(PathBuf::from),
            core_packages_url: env("CORE_PACKAGES_URL"),
            jaeger_url: env("JAEGER_URL").map(|u| u.trim_end_matches('/').to_string()),
            google_chat_webhook: env("GOOGLE_CHAT_WEBHOOK_URL"),
            slack_webhook: env("SLACK_WEBHOOK_URL"),
            notify_min_severity: env_or("NOTIFY_MIN_SEVERITY", "high"),
            retention: Duration::from_secs(env_num("RETENTION_HOURS", 72u64) * 3600),
            max_log_rows: env_num("MAX_LOG_ROWS", 3_000_000),
            backfill: Duration::from_secs(env_num("BACKFILL_MINUTES", 30u64) * 60),
        })
    }

    pub fn service_owners(&self) -> HashMap<String, String> {
        self.catalog
            .services
            .iter()
            .filter_map(|s| s.owner.clone().map(|o| (s.name.clone(), o)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_and_host_resolution() {
        let cat: Catalog = toml::from_str(
            r#"
            [docker]
            compose_projects = ["new-app-dev"]
            exclude_names = ["new-app-dev-redis*"]
            [[services]]
            name = "message-flow"
            containers = ["pf-message-flow"]
            hosts = ["pf-message-flow"]
            layer = "platform"
            "#,
        )
        .unwrap();
        assert_eq!(cat.service_for_container("pf-message-flow", None), "message-flow");
        assert_eq!(
            cat.service_for_container("new-app-dev-workflow_engine-1", Some("workflow_engine")),
            "workflow-engine"
        );
        assert_eq!(cat.service_for_host("PF-MESSAGE-FLOW").as_deref(), Some("message-flow"));
        assert!(cat.wants_container("x", Some("new-app-dev")));
        assert!(!cat.wants_container("new-app-dev-redis-new-1", Some("new-app-dev")));
        assert!(cat.wants_container("pf-message-flow", None));
    }
}
