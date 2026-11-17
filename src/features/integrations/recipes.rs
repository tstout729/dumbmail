//! Recipes: ready-made integrations kept as files in `recipes/<key>/` and built into the binary,
//! so anyone can add one by pull request. Each has `recipe.json` (name, signature, pull, settings,
//! the credentials it needs, setup steps and suggested flows), `script.js` and `fixtures/`.

use rust_embed::Embed;
use serde_json::{json, Value};
use uuid::Uuid;

use super::model::{self, Changes, Integration};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Embed)]
#[folder = "recipes/"]
struct Files;

#[derive(Debug, Clone)]
pub struct Recipe {
    pub key: String,
    pub manifest: Value,
    pub script: String,
}

fn text(path: &str) -> Option<String> {
    Files::get(path).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

pub fn all() -> Vec<Recipe> {
    let mut keys: Vec<String> = Files::iter().filter_map(|p| p.strip_suffix("/recipe.json").map(String::from)).filter(|k| !k.contains('/')).collect();
    keys.sort();
    keys.iter().filter_map(|k| get(k)).collect()
}

pub fn get(key: &str) -> Option<Recipe> {
    if key.is_empty() || key.contains('/') || key.contains("..") {
        return None;
    }
    let manifest: Value = serde_json::from_str(&text(&format!("{key}/recipe.json"))?).ok()?;
    let script = text(&format!("{key}/script.js"))?;
    Some(Recipe { key: key.to_string(), manifest, script })
}

/// Sample payloads kept with a recipe: `(file name, fixture)`.
pub fn fixtures(key: &str) -> Vec<(String, Value)> {
    let prefix = format!("{key}/fixtures/");
    let mut out: Vec<(String, Value)> = Files::iter()
        .filter(|p| p.starts_with(&prefix) && p.ends_with(".json"))
        .filter_map(|p| Some((p.trim_start_matches(&prefix).to_string(), serde_json::from_str(&text(&p)?).ok()?)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

impl Recipe {
    /// What the gallery and `list_integration_recipes` show.
    pub fn view(&self, full: bool) -> Value {
        let m = &self.manifest;
        let mut v = json!({
            "key": self.key, "name": m["name"], "description": m["description"], "category": m["category"],
            "credentials": m["credentials"], "signature_secret": m["signature_secret"], "setup": m["setup"],
            "has_pull": !m["pull"].is_null(),
            "flows": m["flows"].as_array().map(|f| f.iter().map(|x| json!({"name": x["name"], "description": x["description"]})).collect::<Vec<_>>()),
            "docs": m["docs"],
        });
        if full {
            v["script"] = json!(self.script);
            v["signature"] = m["signature"].clone();
            v["pull"] = m["pull"].clone();
            v["settings"] = m["settings"].clone();
            v["flows"] = m["flows"].clone();
        }
        v
    }
}

/// Create an integration from a recipe, plus its suggested flows as drafts (nothing sends until
/// someone turns a flow on).
pub async fn create(state: &AppState, account_id: Uuid, key: &str, name: Option<String>, create_flows: bool) -> AppResult<(Integration, Vec<Value>)> {
    let r = get(key).ok_or_else(|| AppError::bad(format!("unknown recipe {key:?}; list_integration_recipes shows them")))?;
    let m = &r.manifest;
    let changes = Changes {
        name: Some(name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| m["name"].as_str().unwrap_or(key).to_string())),
        script: Some(r.script.clone()),
        signature: Some(m["signature"].clone()).filter(|v| !v.is_null()),
        settings: Some(m["settings"].clone()).filter(|v| v.is_object()),
        pull: Some(m["pull"].clone()).filter(|v| !v.is_null()),
        ..Default::default()
    };
    let integ = model::create(state, account_id, Some(key), changes).await?;
    let mut flows = Vec::new();
    if create_flows {
        for f in m["flows"].as_array().into_iter().flatten() {
            let body = crate::api::messaging::FlowBody { name: f["name"].as_str().map(String::from), trigger: Some(f["trigger"].clone()), definition: Some(f["definition"].clone()) };
            let row = crate::api::messaging::create_flow(state, account_id, body).await?;
            flows.push(json!({"id": row.id, "name": row.name, "status": row.status}));
        }
    }
    Ok((integ, flows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_are_complete_and_valid() {
        let all = all();
        assert!(all.len() >= 3, "{}", all.len());
        for r in &all {
            let m = &r.manifest;
            assert_eq!(m["key"], r.key.as_str());
            for k in ["name", "description"] {
                assert!(m[k].as_str().is_some_and(|s| !s.is_empty()), "{}: {k}", r.key);
            }
            assert!(m["setup"].as_array().is_some_and(|s| !s.is_empty()), "{}: setup", r.key);
            if !m["signature"].is_null() {
                super::super::signature::Signature::parse(&m["signature"]).unwrap_or_else(|e| panic!("{}: {}", r.key, e.public_message()));
            }
            if !m["pull"].is_null() {
                super::super::pull::PullConfig::parse(&m["pull"]).unwrap_or_else(|e| panic!("{}: {}", r.key, e.public_message()));
            }
            for f in m["flows"].as_array().into_iter().flatten() {
                crate::flows::parse_trigger(&f["trigger"]).unwrap_or_else(|e| panic!("{} flow {}: {}", r.key, f["name"], e.public_message()));
                crate::flows::parse_definition(&f["definition"]).unwrap_or_else(|e| panic!("{} flow {}: {}", r.key, f["name"], e.public_message()));
            }
            assert!(!fixtures(&r.key).is_empty(), "{} has no fixtures", r.key);
        }
    }
}
