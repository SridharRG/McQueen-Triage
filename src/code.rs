//! Source lookup: from a log event to the code that emitted it.
//!
//! Resolution order for one event:
//! 1. explicit locations in the text — Rust panics (`panicked at src/a.rs:12:5`),
//!    Python tracebacks (`File "/app/x.py", line 12`), `path.ext:line`, or
//!    tracing's `filename`/`line_number` fields;
//! 2. the tracing `target` (`architect_sdk::authrs`): crate → crate dir (from every
//!    `Cargo.toml` under `REPOS_DIR`), module path → file, then the line where the
//!    message's literal prefix appears. Because the crate is resolved globally, a
//!    `core` container logging `architect_sdk::…` lands in the architect-sdk repo.
//!
//! Repos are mounted read-only; nothing here writes to them. All tool access is
//! confined to the indexed roots.

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::model::LogEvent;

const SOURCE_EXTS: &[&str] = &["rs", "ts", "tsx", "js", "jsx", "py", "go", "toml", "sql", "json"];
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", ".next", "dist", "build", "__pycache__", ".venv"];
const MAX_FILES_PER_REPO: usize = 40_000;
const MAX_FILE_BYTES: u64 = 1_500_000;

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub name: String,
    #[serde(skip)]
    pub root: PathBuf,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub files: usize,
}

#[derive(Debug, Clone)]
struct CrateInfo {
    repo: String,
    /// Crate dir relative to the repo root.
    dir: PathBuf,
}

#[derive(Default)]
pub struct CodeIndex {
    pub repos: Vec<Repo>,
    /// crate ident (underscored) → location.
    crates: HashMap<String, CrateInfo>,
    /// repo → relative file paths.
    files: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CodeLocation {
    pub repo: String,
    pub file: String,
    pub line: usize,
    /// How it was found: `panic`, `stack`, `tracing-target`, `tracing-file`.
    pub via: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub location: CodeLocation,
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
    pub commit: Option<String>,
    /// Which log event led here.
    pub log_id: i64,
}

static PANIC_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"panicked at ([^\s:]+\.rs):(\d+)(?::\d+)?").unwrap());
static PY_FRAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"File "([^"]+\.py)", line (\d+)"#).unwrap());
static PATH_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z0-9_\-./]+\.(?:rs|ts|tsx|js|jsx|py|go)):(\d+)").unwrap()
});

impl CodeIndex {
    /// Index every repo directory directly under `repos_dir` (or `repos_dir` itself
    /// if it is a repo). Cheap: only paths and Cargo manifests are read.
    pub fn build(repos_dir: &Path) -> Self {
        let mut index = CodeIndex::default();
        let Ok(entries) = std::fs::read_dir(repos_dir) else {
            tracing::warn!(dir = %repos_dir.display(), "REPOS_DIR not readable; code context disabled");
            return index;
        };
        let mut roots: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        roots.sort();
        for root in roots {
            let name = root.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            index.add_repo(name, root);
        }
        tracing::info!(repos = index.repos.len(), crates = index.crates.len(), "code index built");
        index
    }

    fn add_repo(&mut self, name: String, root: PathBuf) {
        let mut files = vec![];
        let walker = WalkDir::new(&root).follow_links(false).into_iter().filter_entry(|e| {
            !(e.file_type().is_dir() && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        });
        for entry in walker.flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
            if !SOURCE_EXTS.contains(&ext) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&root) else { continue };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if path.file_name().is_some_and(|f| f == "Cargo.toml") {
                for ident in crate_idents(path) {
                    self.crates.entry(ident).or_insert_with(|| CrateInfo {
                        repo: name.clone(),
                        dir: rel.parent().map(Path::to_path_buf).unwrap_or_default(),
                    });
                }
            }
            files.push(rel_str);
            if files.len() >= MAX_FILES_PER_REPO {
                break;
            }
        }
        let (branch, commit) = git_head(&root);
        self.repos.push(Repo { name: name.clone(), root, branch, commit, files: files.len() });
        self.files.insert(name, files);
    }

    pub fn repo(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.name == name)
    }

    pub fn crate_names(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self.crates.iter().map(|(k, c)| (k.clone(), c.repo.clone())).collect();
        v.sort();
        v
    }

    /// Best-effort location for one event. `service_repo` narrows path-suffix matches.
    pub fn locate(&self, ev: &LogEvent, service_repo: Option<&str>) -> Option<CodeLocation> {
        let mut text = ev.message.clone();
        for key in ["error", "panic", "backtrace", "exception"] {
            if let Some(v) = ev.field_str(key) {
                text.push('\n');
                text.push_str(v);
            }
        }

        if let Some(c) = PANIC_AT.captures(&text) {
            if let Some(loc) = self.resolve_path(&c[1], c[2].parse().unwrap_or(1), service_repo, "panic") {
                return Some(loc);
            }
        }
        // Innermost Python frame is the last one.
        if let Some(c) = PY_FRAME.captures_iter(&text).last() {
            if let Some(loc) = self.resolve_path(&c[1], c[2].parse().unwrap_or(1), service_repo, "stack") {
                return Some(loc);
            }
        }
        if let (Some(file), Some(line)) = (ev.field_str("filename").or(ev.field_str("log.file")), line_field(&ev.fields)) {
            if let Some(loc) = self.resolve_path(file, line, service_repo, "tracing-file") {
                return Some(loc);
            }
        }
        if let Some(target) = &ev.target {
            if let Some(loc) = self.locate_target(target, &ev.message) {
                return Some(loc);
            }
        }
        for c in PATH_LINE.captures_iter(&text) {
            if c[1].contains("/cargo/registry/") || c[1].contains("/rustc/") {
                continue;
            }
            if let Some(loc) = self.resolve_path(&c[1], c[2].parse().unwrap_or(1), service_repo, "stack") {
                return Some(loc);
            }
        }
        None
    }

    /// `crate::mod::sub` → file in that crate, line of the message literal.
    fn locate_target(&self, target: &str, message: &str) -> Option<CodeLocation> {
        let mut parts = target.split("::");
        let krate = parts.next()?;
        let mods: Vec<&str> = parts.collect();
        let info = self.crates.get(krate)?;
        let repo = self.repo(&info.repo)?;
        let src = repo.root.join(&info.dir).join("src");

        let mut candidates: Vec<PathBuf> = vec![];
        for depth in (0..=mods.len()).rev() {
            let m = &mods[..depth];
            if m.is_empty() {
                candidates.push(src.join("main.rs"));
                candidates.push(src.join("lib.rs"));
            } else {
                let joined = m.join("/");
                candidates.push(src.join(format!("{joined}.rs")));
                candidates.push(src.join(&joined).join("mod.rs"));
            }
        }
        let needles = needles(message);
        for path in candidates.iter().filter(|p| p.is_file()) {
            if let Some(line) = find_line(path, &needles) {
                return Some(self.location(repo, path, line, "tracing-target"));
            }
        }
        // Message not in the module file (helper macro, re-export…): search the crate.
        let crate_dir = repo.root.join(&info.dir);
        if let Some(files) = self.files.get(&repo.name) {
            let prefix = info.dir.to_string_lossy().replace('\\', "/");
            for rel in files.iter().filter(|f| f.ends_with(".rs") && (prefix.is_empty() || f.starts_with(&prefix))) {
                let path = repo.root.join(rel);
                if !path.starts_with(&crate_dir) {
                    continue;
                }
                if let Some(line) = find_line(&path, &needles) {
                    return Some(self.location(repo, &path, line, "tracing-target"));
                }
            }
        }
        // Fall back to the module file itself, top of file.
        candidates
            .iter()
            .find(|p| p.is_file())
            .map(|p| self.location(repo, p, 1, "tracing-target"))
    }

    fn location(&self, repo: &Repo, path: &Path, line: usize, via: &str) -> CodeLocation {
        CodeLocation {
            repo: repo.name.clone(),
            file: path.strip_prefix(&repo.root).unwrap_or(path).to_string_lossy().replace('\\', "/"),
            line,
            via: via.into(),
        }
    }

    /// Match a path from a stack/panic (`/app/src/x.py`, `src/a.rs`, `crates/dh-core/src/x.rs`)
    /// against indexed files by the longest path suffix.
    fn resolve_path(&self, raw: &str, line: usize, prefer: Option<&str>, via: &str) -> Option<CodeLocation> {
        let raw = raw.trim_start_matches("./").replace('\\', "/");
        let segs: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).collect();
        let mut best: Option<(usize, bool, &str, &str)> = None; // (matched segs, preferred, repo, file)
        for (repo, files) in &self.files {
            for f in files {
                let fsegs: Vec<&str> = f.split('/').collect();
                let n = segs.iter().rev().zip(fsegs.iter().rev()).take_while(|(a, b)| a == b).count();
                if n == 0 {
                    continue;
                }
                let preferred = prefer == Some(repo.as_str());
                let better = match best {
                    None => true,
                    Some((bn, bp, _, _)) => n > bn || (n == bn && preferred && !bp),
                };
                if better {
                    best = Some((n, preferred, repo.as_str(), f.as_str()));
                }
            }
        }
        let (n, _, repo, file) = best?;
        // A lone filename (`main.rs`) only counts inside the service's own repo.
        if n < 2 && prefer != Some(repo) {
            return None;
        }
        Some(CodeLocation { repo: repo.into(), file: file.into(), line, via: via.into() })
    }

    pub fn snippet(&self, loc: &CodeLocation, radius: usize, log_id: i64) -> Option<Snippet> {
        let repo = self.repo(&loc.repo)?;
        let (start, end, code) = read_range(&repo.root.join(&loc.file), loc.line.saturating_sub(radius).max(1), loc.line + radius, Some(loc.line))?;
        Some(Snippet { location: loc.clone(), start_line: start, end_line: end, code, commit: repo.commit.clone(), log_id })
    }

    /// Bounded read for the agent's `read_source` tool. `path` is repo-relative.
    pub fn read(&self, repo: &str, path: &str, start: usize, end: usize) -> Result<String, String> {
        let repo = self.repo(repo).ok_or_else(|| format!("unknown repo '{repo}'. Known: {}", self.repo_names()))?;
        // Models often repeat the repo name in the path (`decision-hub/crates/…`).
        let path = path.trim_start_matches('/');
        let path = path.strip_prefix(&format!("{}/", repo.name)).unwrap_or(path);
        let full = match safe_join(&repo.root, path) {
            Some(p) if p.is_file() => p,
            _ => {
                // Fall back to a unique suffix match, e.g. `dh-queue/src/lib.rs`.
                let hits: Vec<&String> = self.files.get(&repo.name).map(|f| f.iter().filter(|f| f.ends_with(path)).collect()).unwrap_or_default();
                match hits.as_slice() {
                    [one] => repo.root.join(one),
                    [] => return Err(format!("no such file: {path}")),
                    many => return Err(format!("ambiguous path {path}: {}", many.iter().take(5).map(|s| s.as_str()).collect::<Vec<_>>().join(", "))),
                }
            }
        };
        let path = full.strip_prefix(&repo.root).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| path.to_string());
        let end = end.max(start).min(start + 160);
        read_range(&full, start.max(1), end, None)
            .map(|(s, e, code)| format!("{}/{path} lines {s}-{e}:\n{code}", repo.name))
            .ok_or_else(|| "could not read file".into())
    }

    /// Literal search across indexed source for the agent's `search_code` tool.
    pub fn search(&self, needle: &str, repo: Option<&str>, max: usize) -> Vec<String> {
        let mut out = vec![];
        if needle.trim().len() < 3 {
            return out;
        }
        for r in &self.repos {
            if repo.is_some_and(|want| want != r.name) {
                continue;
            }
            let Some(files) = self.files.get(&r.name) else { continue };
            for rel in files {
                if rel.ends_with(".json") || rel.ends_with(".toml") {
                    continue;
                }
                let path = r.root.join(rel);
                if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for (i, line) in text.lines().enumerate() {
                    if line.contains(needle) {
                        out.push(format!("{}/{}:{}: {}", r.name, rel, i + 1, crate::redact::truncate(line.trim(), 200)));
                        if out.len() >= max {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    pub fn repo_names(&self) -> String {
        self.repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

fn line_field(fields: &Value) -> Option<usize> {
    ["line_number", "line", "log.line"]
        .iter()
        .find_map(|k| fields.get(*k).and_then(Value::as_u64))
        .map(|v| v as usize)
}

/// Literal fragments of a formatted log message, longest first. A message like
/// `service proxy upstream error (message-flow): error sending…` was produced by
/// `"service proxy upstream error ({slug}): {e}"`, so the text before the first
/// interpolation-looking character is what appears in source.
fn needles(message: &str) -> Vec<String> {
    let first_line = message.lines().next().unwrap_or_default().trim();
    let mut out = vec![];
    if first_line.len() <= 100 {
        out.push(first_line.to_string());
    }
    if let Some(i) = first_line.find(|c: char| "({[:=\"'`,".contains(c)) {
        let head = first_line[..i].trim();
        if head.len() >= 8 {
            out.push(head.to_string());
        }
    }
    let words: Vec<&str> = first_line.split_whitespace().take(4).collect();
    if words.len() >= 3 {
        out.push(words.join(" "));
    }
    out.dedup();
    out
}

fn find_line(path: &Path, needles: &[String]) -> Option<usize> {
    if std::fs::metadata(path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    for needle in needles {
        if let Some(i) = text.lines().position(|l| l.contains(needle.as_str())) {
            return Some(i + 1);
        }
    }
    None
}

fn read_range(path: &Path, start: usize, end: usize, mark: Option<usize>) -> Option<(usize, usize, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Some((1, 1, String::new()));
    }
    let start = start.clamp(1, lines.len());
    let end = end.clamp(start, lines.len());
    let width = end.to_string().len();
    let code = (start..=end)
        .map(|n| {
            let marker = if Some(n) == mark { "→" } else { " " };
            format!("{marker}{n:>width$} │ {}", lines[n - 1])
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((start, end, code))
}

/// Join without escaping `root` (no `..`, no absolute paths).
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return None;
    }
    let full = root.join(rel);
    let canon = full.canonicalize().ok()?;
    canon.starts_with(root.canonicalize().ok()?).then_some(canon)
}

/// Crate identifiers declared by a manifest: `[package] name` and `[lib] name`, underscored.
fn crate_idents(manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else { return vec![] };
    let Ok(doc) = text.parse::<toml::Table>() else { return vec![] };
    let mut out = vec![];
    for section in ["package", "lib"] {
        if let Some(name) = doc.get(section).and_then(|s| s.get("name")).and_then(|n| n.as_str()) {
            let ident = name.replace('-', "_");
            if !out.contains(&ident) {
                out.push(ident);
            }
        }
    }
    out
}

/// Current branch and commit from `.git` without shelling out.
fn git_head(root: &Path) -> (Option<String>, Option<String>) {
    let git = root.join(".git");
    let Ok(head) = std::fs::read_to_string(git.join("HEAD")) else { return (None, None) };
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref: ") {
        let branch = r.trim_start_matches("refs/heads/").to_string();
        let commit = std::fs::read_to_string(git.join(r)).ok().map(|s| s.trim().to_string()).or_else(|| {
            std::fs::read_to_string(git.join("packed-refs")).ok().and_then(|p| {
                p.lines().find(|l| l.ends_with(r)).and_then(|l| l.split_whitespace().next().map(str::to_string))
            })
        });
        (Some(branch), commit.map(|c| c[..c.len().min(12)].to_string()))
    } else {
        (None, Some(head[..head.len().min(12)].to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fixture() -> (PathBuf, CodeIndex) {
        let root = std::env::temp_dir().join(format!("mq-code-{}", uuid::Uuid::new_v4()));
        let repo = root.join("launchpad");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"launchpad\"\n").unwrap();
        std::fs::write(
            repo.join("src/service_registry.rs"),
            "fn a() {}\nfn proxy() {\n    tracing::warn!(\"service proxy upstream error ({slug}): {e}\");\n}\n",
        )
        .unwrap();
        let sdk = root.join("architect-sdk");
        std::fs::create_dir_all(sdk.join("src")).unwrap();
        std::fs::write(sdk.join("Cargo.toml"), "[package]\nname = \"foundry-rs\"\n[lib]\nname = \"architect_sdk\"\n").unwrap();
        std::fs::write(sdk.join("src/authrs.rs"), "\n\nwarn!(\"permission denied\");\n").unwrap();
        let idx = CodeIndex::build(&root);
        (root, idx)
    }

    fn ev(target: &str, msg: &str) -> LogEvent {
        LogEvent {
            id: 1,
            ts: Utc::now(),
            service: "x".into(),
            container: "x".into(),
            level: crate::model::Level::Warn,
            target: Some(target.into()),
            message: msg.into(),
            trace_id: None,
            fields: serde_json::json!({}),
            spans: vec![],
            correlation: None,
        }
    }

    #[test]
    fn resolves_target_and_message_line() {
        let (root, idx) = fixture();
        let loc = idx
            .locate(&ev("launchpad::service_registry", "service proxy upstream error (message-flow): error sending request"), None)
            .unwrap();
        assert_eq!((loc.repo.as_str(), loc.file.as_str(), loc.line), ("launchpad", "src/service_registry.rs", 3));
        // lib name differs from package name
        let loc = idx.locate(&ev("architect_sdk::authrs", "permission denied"), None).unwrap();
        assert_eq!((loc.repo.as_str(), loc.line), ("architect-sdk", 3));
        let snip = idx.snippet(&loc, 2, 1).unwrap();
        assert!(snip.code.contains("→3"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_panic_location_and_blocks_traversal() {
        let (root, idx) = fixture();
        let loc = idx
            .locate(&ev("", "thread 'main' panicked at src/service_registry.rs:2:5:\nboom"), Some("launchpad"))
            .unwrap();
        assert_eq!((loc.file.as_str(), loc.line, loc.via.as_str()), ("src/service_registry.rs", 2, "panic"));
        assert!(idx.read("launchpad", "../architect-sdk/src/authrs.rs", 1, 3).is_err());
        assert!(idx.read("launchpad", "src/service_registry.rs", 1, 3).is_ok());
        assert!(idx.read("launchpad", "launchpad/src/service_registry.rs", 1, 3).is_ok(), "repo prefix tolerated");
        assert!(idx.read("launchpad", "service_registry.rs", 1, 3).is_ok(), "unique suffix tolerated");
        assert_eq!(idx.search("upstream error", None, 5).len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
