//! Database context from architect-sdk packages — never from the database itself.
//!
//! Sources (merged; later wins per package id):
//! - `SCHEMA_DIR`: package directories (`manifest.json`, `tables.json`, `columns.json`,
//!   `relationships.json`, `enums.json`, `indexes.json`, `api_entities.json`), `.zip`s
//!   of such a directory, or `.json` files in the `GET /config/packages/:id` shape;
//! - `CORE_PACKAGES_URL`: core's read-only `GET /api/v1/config/packages` (+ `/:id`).
//!
//! For a prompt only the tables an incident touches are rendered, as compact DDL.

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::Path,
    sync::LazyLock,
};

use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForeignKey {
    pub name: String,
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
    pub on_delete: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Table {
    pub package: String,
    pub schema: String,
    pub name: String,
    pub comment: Option<String>,
    pub primary_key: Option<String>,
    pub columns: Vec<Column>,
    pub unique: Vec<Vec<String>>,
    pub checks: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    pub indexes: Vec<String>,
    pub api_path: Option<String>,
    pub operations: Vec<String>,
}

impl Table {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EnumDef {
    pub schema: String,
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageInfo {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub schema: String,
    pub tables: usize,
    pub source: String,
}

#[derive(Default, Clone)]
pub struct SchemaCatalog {
    pub packages: Vec<PackageInfo>,
    pub tables: Vec<Table>,
    pub enums: Vec<EnumDef>,
}

impl SchemaCatalog {
    pub async fn load(dir: Option<&Path>, url: Option<&str>) -> Self {
        let mut raw: Vec<(Value, String)> = vec![];
        if let Some(dir) = dir {
            match load_dir(dir) {
                Ok(mut pkgs) => raw.append(&mut pkgs),
                Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "schema dir not loaded"),
            }
        }
        if let Some(url) = url {
            match load_url(url).await {
                Ok(mut pkgs) => raw.append(&mut pkgs),
                Err(e) => tracing::warn!(url, error = %e, "core packages not loaded"),
            }
        }
        // Later sources override earlier ones for the same package id.
        let mut by_id: HashMap<String, (Value, String)> = HashMap::new();
        for (pkg, source) in raw {
            let id = pkg["id"].as_str().unwrap_or("unknown").to_string();
            by_id.insert(id, (pkg, source));
        }
        let mut cat = SchemaCatalog::default();
        let mut ids: Vec<_> = by_id.keys().cloned().collect();
        ids.sort();
        // Table ids are referenced across packages (FKs into manufacturing_essential…).
        let mut table_names: HashMap<String, String> = HashMap::new();
        let mut column_names: HashMap<String, String> = HashMap::new();
        for id in &ids {
            let (pkg, _) = &by_id[id];
            let schema = package_schema(pkg);
            for t in arr(pkg, &["tables"]) {
                if let (Some(tid), Some(name)) = (t["id"].as_str(), t["name"].as_str()) {
                    table_names.insert(tid.into(), format!("{schema}.{name}"));
                }
            }
            for c in arr(pkg, &["columns"]) {
                if let (Some(cid), Some(name)) = (c["id"].as_str(), c["name"].as_str()) {
                    column_names.insert(cid.into(), name.into());
                }
            }
        }
        for id in ids {
            let (pkg, source) = &by_id[&id];
            cat.add_package(pkg, source, &table_names, &column_names);
        }
        tracing::info!(packages = cat.packages.len(), tables = cat.tables.len(), "schema catalog loaded");
        cat
    }

    fn add_package(
        &mut self,
        pkg: &Value,
        source: &str,
        table_names: &HashMap<String, String>,
        column_names: &HashMap<String, String>,
    ) {
        let id = pkg["id"].as_str().unwrap_or("unknown").to_string();
        let schema = package_schema(pkg);
        let columns = arr(pkg, &["columns"]);
        let rels = arr(pkg, &["relationships"]);
        let indexes = arr(pkg, &["indexes"]);
        let apis = arr(pkg, &["apiEntities", "api_entities"]);
        let mut count = 0;

        for t in arr(pkg, &["tables"]) {
            let (Some(tid), Some(name)) = (t["id"].as_str(), t["name"].as_str()) else { continue };
            let cols = columns
                .iter()
                .filter(|c| c["table_id"] == tid)
                .map(|c| Column {
                    name: c["name"].as_str().unwrap_or_default().into(),
                    ty: c["type"].as_str().unwrap_or("?").into(),
                    nullable: c["nullable"].as_bool().unwrap_or(true),
                    default: match &c["default"] {
                        Value::Null => None,
                        Value::String(s) => Some(s.clone()),
                        Value::Object(o) => o.get("expression").and_then(Value::as_str).map(str::to_string),
                        other => Some(other.to_string()),
                    },
                    comment: c["comment"].as_str().map(str::to_string),
                })
                .collect();
            let fks = rels
                .iter()
                .filter(|r| r["from_table_id"] == tid)
                .map(|r| ForeignKey {
                    name: r["name"].as_str().unwrap_or_default().into(),
                    column: lookup(column_names, &r["from_column_id"]),
                    ref_table: lookup(table_names, &r["to_table_id"]),
                    ref_column: lookup(column_names, &r["to_column_id"]),
                    on_delete: r["on_delete"].as_str().map(str::to_string),
                })
                .collect();
            let idx = indexes
                .iter()
                .filter(|i| i["table_id"] == tid)
                .map(|i| {
                    let cols: Vec<String> = i["columns"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| c["name"].as_str().unwrap_or("?").into()))
                        .collect();
                    format!(
                        "{}{} ({})",
                        if i["unique"].as_bool().unwrap_or(false) { "UNIQUE " } else { "" },
                        i["name"].as_str().unwrap_or("idx"),
                        cols.join(", ")
                    )
                })
                .collect();
            let api = apis.iter().find(|a| a["entity_id"] == tid);
            self.tables.push(Table {
                package: id.clone(),
                schema: schema.clone(),
                name: name.into(),
                comment: t["comment"].as_str().map(str::to_string),
                primary_key: t["primary_key"].as_str().map(str::to_string),
                columns: cols,
                unique: t["unique"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|u| u.as_array().into_iter().flatten().filter_map(|c| c.as_str().map(str::to_string)).collect())
                    .collect(),
                checks: t["check"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| format!("{}: {}", c["name"].as_str().unwrap_or("check"), c["expression"].as_str().unwrap_or("?")))
                    .collect(),
                foreign_keys: fks,
                indexes: idx,
                api_path: api.and_then(|a| a["path_segment"].as_str()).map(|p| format!("/package/{id}/{p}")),
                operations: api
                    .map(|a| a["operations"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            });
            count += 1;
        }
        for e in arr(pkg, &["enums"]) {
            self.enums.push(EnumDef {
                schema: schema.clone(),
                name: e["name"].as_str().unwrap_or_default().into(),
                values: e["values"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect(),
            });
        }
        self.packages.push(PackageInfo {
            id: id.clone(),
            name: pkg["name"].as_str().unwrap_or(&id).into(),
            version: pkg["version"].as_str().map(str::to_string),
            schema,
            tables: count,
            source: source.into(),
        });
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        let name = name.trim().trim_matches('"').to_ascii_lowercase();
        self.tables
            .iter()
            .find(|t| t.qualified().eq_ignore_ascii_case(&name))
            .or_else(|| self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&name)))
    }

    pub fn search(&self, q: &str) -> Vec<&Table> {
        let q = q.to_ascii_lowercase();
        self.tables
            .iter()
            .filter(|t| q.is_empty() || t.qualified().contains(&q) || t.columns.iter().any(|c| c.name.contains(&q)))
            .collect()
    }

    /// Tables an incident's text points at, best first, plus 1-hop FK neighbours.
    pub fn relevant(&self, text: &str, max: usize) -> Vec<&Table> {
        static QUALIFIED: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""?([a-z_][a-z0-9_]*)"?\."?([a-z_][a-z0-9_]*)"?"#).unwrap());
        static RESOURCE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?:package:([a-z0-9_]+)/)?table:([a-z0-9_]+)").unwrap());
        static API_PATH: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"/package/([a-z0-9_]+)/([a-z0-9_]+)").unwrap());
        static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)(relation|table|constraint|column|index|type)\s+\\?"([a-z0-9_.]+)\\?""#).unwrap()
        });

        let lower = text.to_ascii_lowercase();
        let mut score: HashMap<usize, i32> = HashMap::new();
        let mut bump = |i: usize, s: i32| *score.entry(i).or_default() += s;

        for c in QUALIFIED.captures_iter(&lower) {
            for (i, t) in self.tables.iter().enumerate() {
                if t.schema == c[1] && t.name == c[2] {
                    bump(i, 10);
                }
            }
        }
        for c in RESOURCE.captures_iter(&lower) {
            for (i, t) in self.tables.iter().enumerate() {
                if t.name == c[2] && c.get(1).is_none_or(|p| p.as_str() == t.package) {
                    bump(i, 10);
                }
            }
        }
        for c in API_PATH.captures_iter(&lower) {
            let path = format!("/package/{}/{}", &c[1], &c[2]);
            for (i, t) in self.tables.iter().enumerate() {
                if t.api_path.as_deref() == Some(path.as_str()) {
                    bump(i, 8);
                }
            }
        }
        for c in QUOTED.captures_iter(&lower) {
            let (kind, name) = (c[1].to_string(), c[2].to_string());
            let bare = name.rsplit('.').next().unwrap_or(&name).to_string();
            for (i, t) in self.tables.iter().enumerate() {
                match kind.as_str() {
                    "relation" | "table" if t.name == bare => bump(i, 9),
                    "constraint" | "index"
                        if t.foreign_keys.iter().any(|f| f.name == bare)
                            || t.indexes.iter().any(|x| x.contains(&bare)) =>
                    {
                        bump(i, 9)
                    }
                    "column" if t.columns.iter().any(|col| col.name == bare) => bump(i, 2),
                    _ => {}
                }
            }
        }
        for (i, t) in self.tables.iter().enumerate() {
            if (t.name.len() >= 5 || t.name.contains('_')) && contains_word(&lower, &t.name) {
                bump(i, 3);
            }
        }

        let mut ranked: Vec<(usize, i32)> = score.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut picked: Vec<usize> = ranked.iter().take(max).map(|(i, _)| *i).collect();
        // FK neighbours of the top hits give the model the join context.
        let top: Vec<usize> = picked.iter().take(3).copied().collect();
        for i in top {
            for fk in &self.tables[i].foreign_keys {
                if picked.len() >= max {
                    break;
                }
                if let Some(j) = self.tables.iter().position(|t| t.qualified() == fk.ref_table) {
                    if !picked.contains(&j) {
                        picked.push(j);
                    }
                }
            }
        }
        picked.into_iter().map(|i| &self.tables[i]).collect()
    }

    /// Compact DDL-ish rendering of tables plus the enums their columns use.
    pub fn render(&self, tables: &[&Table]) -> String {
        let mut out = String::new();
        let mut enums_used: HashSet<String> = HashSet::new();
        for t in tables {
            out.push_str(&format!("TABLE {}", t.qualified()));
            if let Some(api) = &t.api_path {
                out.push_str(&format!("  [API {api}]"));
            }
            if let Some(c) = &t.comment {
                out.push_str(&format!("  -- {}", crate::redact::truncate(c, 120)));
            }
            out.push('\n');
            for c in &t.columns {
                let pk = if t.primary_key.as_deref() == Some(&c.name) { " PK" } else { "" };
                let null = if c.nullable { "" } else { " NOT NULL" };
                let def = c.default.as_ref().map(|d| format!(" DEFAULT {d}")).unwrap_or_default();
                out.push_str(&format!("  {} {}{null}{def}{pk}\n", c.name, c.ty));
                let bare = c.ty.rsplit('.').next().unwrap_or(&c.ty);
                if self.enums.iter().any(|e| e.name == bare) {
                    enums_used.insert(bare.to_string());
                }
            }
            for fk in &t.foreign_keys {
                out.push_str(&format!(
                    "  FK {} → {}({}){}\n",
                    fk.column,
                    fk.ref_table,
                    fk.ref_column,
                    fk.on_delete.as_ref().map(|d| format!(" ON DELETE {d}")).unwrap_or_default()
                ));
            }
            for u in t.unique.iter().filter(|u| !u.is_empty()) {
                out.push_str(&format!("  UNIQUE ({})\n", u.join(", ")));
            }
            for c in &t.checks {
                out.push_str(&format!("  CHECK {c}\n"));
            }
        }
        let mut enums: Vec<&EnumDef> = self.enums.iter().filter(|e| enums_used.contains(&e.name)).collect();
        enums.sort_by(|a, b| a.name.cmp(&b.name));
        enums.dedup_by(|a, b| a.name == b.name);
        for e in enums {
            out.push_str(&format!("ENUM {}.{}: {}\n", e.schema, e.name, e.values.join(" | ")));
        }
        out
    }
}

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.match_indices(word).any(|(i, _)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

fn lookup(map: &HashMap<String, String>, id: &Value) -> String {
    id.as_str().and_then(|i| map.get(i)).cloned().unwrap_or_else(|| id.as_str().unwrap_or("?").to_string())
}

fn package_schema(pkg: &Value) -> String {
    pkg["schema"]
        .as_str()
        .or_else(|| pkg["manifest"]["schema"].as_str())
        .or_else(|| pkg["schemas"].as_array().and_then(|s| s.first()).and_then(|s| s["name"].as_str()))
        .or_else(|| pkg["id"].as_str())
        .unwrap_or("public")
        .to_string()
}

fn arr(v: &Value, keys: &[&str]) -> Vec<Value> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

/// `{"data": {...}}` → `{...}`.
fn unwrap_data(v: Value) -> Value {
    match v {
        Value::Object(mut o) if o.contains_key("data") && !o.contains_key("tables") => o.remove("data").unwrap_or_default(),
        other => other,
    }
}

fn load_dir(dir: &Path) -> Result<Vec<(Value, String)>> {
    let mut out = vec![];
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    entries.sort();
    // The directory itself may be one package.
    if dir.join("manifest.json").is_file() {
        out.push((package_from_files(|f| std::fs::read_to_string(dir.join(f)).ok())?, dir.display().to_string()));
        return Ok(out);
    }
    for path in entries {
        let source = path.display().to_string();
        let result = if path.is_dir() && path.join("manifest.json").is_file() {
            package_from_files(|f| std::fs::read_to_string(path.join(f)).ok())
        } else if path.extension().is_some_and(|e| e == "zip") {
            package_from_zip(&path)
        } else if path.extension().is_some_and(|e| e == "json") {
            std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|t| Ok(unwrap_data(serde_json::from_str(&t)?)))
        } else {
            continue;
        };
        match result {
            Ok(pkg) if pkg.get("tables").is_some() => out.push((pkg, source)),
            Ok(_) => tracing::debug!(%source, "not a package, skipped"),
            Err(e) => tracing::warn!(%source, error = %e, "package not loaded"),
        }
    }
    Ok(out)
}

/// Assemble a package from its split files (`manifest.json`, `tables.json`, …).
fn package_from_files(read: impl Fn(&str) -> Option<String>) -> Result<Value> {
    let manifest: Value = serde_json::from_str(&read("manifest.json").context("manifest.json missing")?)?;
    let mut pkg = manifest.clone();
    let obj = pkg.as_object_mut().context("manifest is not an object")?;
    obj.insert("manifest".into(), manifest);
    for (file, key) in [
        ("tables.json", "tables"),
        ("columns.json", "columns"),
        ("relationships.json", "relationships"),
        ("enums.json", "enums"),
        ("indexes.json", "indexes"),
        ("api_entities.json", "apiEntities"),
        ("schemas.json", "schemas"),
    ] {
        if let Some(text) = read(file) {
            obj.insert(key.into(), serde_json::from_str(&text).with_context(|| file.to_string())?);
        }
    }
    Ok(pkg)
}

fn package_from_zip(path: &Path) -> Result<Value> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut files: HashMap<String, String> = HashMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        // Files may sit in a top-level folder inside the zip.
        let name = f.name().rsplit('/').next().unwrap_or_default().to_string();
        let mut text = String::new();
        if f.read_to_string(&mut text).is_ok() {
            files.insert(name, text);
        }
    }
    package_from_files(|f| files.get(f).cloned())
}

async fn load_url(url: &str) -> Result<Vec<(Value, String)>> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
    let list: Value = client.get(url).send().await?.error_for_status()?.json().await?;
    let ids: Vec<String> = list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["id"].as_str().map(str::to_string))
        .collect();
    let mut out = vec![];
    for id in ids {
        let detail_url = format!("{}/{id}", url.trim_end_matches('/'));
        let detail: Value = client.get(&detail_url).send().await?.error_for_status()?.json().await?;
        out.push((unwrap_data(detail), detail_url));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mq-schema-{}", uuid::Uuid::new_v4()));
        let pkg = dir.join("sample");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("manifest.json"), r#"{"id":"sample","name":"Sample","version":"1.0.0","schema":"sample"}"#).unwrap();
        std::fs::write(pkg.join("tables.json"), r#"[{"id":"tbl_users","name":"users","primary_key":"id","unique":[["email"]],"check":[]},{"id":"tbl_orders","name":"orders","primary_key":"id","unique":[],"check":[]}]"#).unwrap();
        std::fs::write(pkg.join("columns.json"), r#"[{"id":"c1","table_id":"tbl_users","name":"id","type":"uuid","nullable":false,"default":{"expression":"gen_random_uuid()"}},{"id":"c2","table_id":"tbl_orders","name":"user_id","type":"uuid","nullable":false,"default":null},{"id":"c3","table_id":"tbl_orders","name":"status","type":"sample.order_status","nullable":false,"default":"'pending'"}]"#).unwrap();
        std::fs::write(pkg.join("relationships.json"), r#"[{"id":"r1","from_table_id":"tbl_orders","from_column_id":"c2","to_table_id":"tbl_users","to_column_id":"c1","on_delete":"CASCADE","name":"fk_orders_user_id"}]"#).unwrap();
        std::fs::write(pkg.join("enums.json"), r#"[{"id":"e1","name":"order_status","values":["pending","shipped"]}]"#).unwrap();
        std::fs::write(pkg.join("api_entities.json"), r#"[{"entity_id":"tbl_orders","path_segment":"orders","operations":["read"]}]"#).unwrap();
        dir
    }

    #[tokio::test]
    async fn loads_split_package_and_ranks_tables() {
        let dir = sample_dir();
        let cat = SchemaCatalog::load(Some(&dir), None).await;
        assert_eq!(cat.tables.len(), 2);
        let orders = cat.table("sample.orders").unwrap();
        assert_eq!(orders.foreign_keys[0].ref_table, "sample.users");
        assert_eq!(orders.api_path.as_deref(), Some("/package/sample/orders"));

        let hits = cat.relevant(r#"insert or update on table "orders" violates foreign key constraint "fk_orders_user_id""#, 4);
        assert_eq!(hits[0].name, "orders");
        assert!(hits.iter().any(|t| t.name == "users"), "FK neighbour included");
        let ddl = cat.render(&hits);
        assert!(ddl.contains("FK user_id → sample.users(id) ON DELETE CASCADE"));
        assert!(ddl.contains("ENUM sample.order_status: pending | shipped"));

        let hits = cat.relevant("GET /api/v1/package/sample/orders 500", 3);
        assert_eq!(hits[0].name, "orders");
        let hits = cat.relevant("resource=service:core/package:sample/table:users", 3);
        assert_eq!(hits[0].name, "users");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn word_boundaries() {
        assert!(contains_word("select from projects where", "projects"));
        assert!(!contains_word("project_journeys", "project"));
    }
}
