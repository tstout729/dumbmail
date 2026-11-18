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

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
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
        message_id: Some(m["message_id"].clone()).filter(|v| !v.is_null()),
        // The recipe's events are trusted by flows: saving marks them server-only, so a browser
        // can't send them for this account, even before the first real one arrives.
        metrics: Some(strings(&m["metrics"])),
        redact: Some(strings(&m["redact"])),
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
        let metric_re = regex::Regex::new(r#"metric: "([^"]+)""#).unwrap();
        for r in &all {
            let m = &r.manifest;
            assert_eq!(m["key"], r.key.as_str());
            for k in ["name", "description"] {
                assert!(m[k].as_str().is_some_and(|s| !s.is_empty()), "{}: {k}", r.key);
            }
            assert!(m["setup"].as_array().is_some_and(|s| !s.is_empty()), "{}: setup", r.key);
            // Every metric the script records is listed, so it's server-only from the start.
            let listed: Vec<&str> = m["metrics"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            for cap in metric_re.captures_iter(&r.script) {
                assert!(listed.contains(&&cap[1]), "{}: metric {:?} isn't in recipe.json's metrics", r.key, &cap[1]);
            }
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
            assert!(Files::get(&format!("{}/README.md", r.key)).is_some(), "{} has no README.md saying which docs each assumption comes from", r.key);
        }
    }

    /// The script input a fixture describes, filled in the way the engine fills it.
    fn fixture_input(r: &Recipe, f: &Value) -> Value {
        let mut input = f["input"].clone();
        let body = input["payload"].to_string();
        let defaults = json!({
            "source": "webhook", "replay": false, "body": body, "headers": {}, "query": {},
            "settings": r.manifest["settings"].clone(), "state": {}, "now": "2026-11-17T18:00:00.000Z",
            "integration": {"id": "00000000-0000-0000-0000-000000000000", "name": r.manifest["name"].clone()},
        });
        for (k, v) in defaults.as_object().unwrap() {
            if input.get(k).is_none_or(Value::is_null) {
                input[k] = v.clone();
            }
        }
        if input["source"] == "pull" && input.get("backfill").is_none() {
            input["backfill"] = Value::Null;
        }
        input
    }

    /// Every fixture's script output matches what's recorded. To record new fixtures, run with
    /// RECIPE_FIXTURES_WRITE=1, then read the recorded `expect` and check it by hand.
    #[test]
    fn recipe_scripts_match_their_fixtures() {
        let write = std::env::var("RECIPE_FIXTURES_WRITE").is_ok();
        for r in all() {
            for (name, f) in fixtures(&r.key) {
                let input = fixture_input(&r, &f);
                let pull = input["source"] == "pull";
                let out: Value = super::super::sandbox::run_engine(&r.script, &input.to_string(), std::time::Duration::from_secs(5))
                    .map(|t| serde_json::from_str(&t).unwrap())
                    .unwrap_or_else(|e| panic!("{}/{name}: {e}", r.key));
                super::super::actions::parse(&out, pull || !r.manifest["pull"].is_null()).unwrap_or_else(|e| panic!("{}/{name}: invalid actions: {e}", r.key));
                // Long outputs are checked by their action counts and last two actions.
                let summary_only = f["summary_only"] == true;
                let got = if summary_only {
                    let arr = out.as_array().cloned().unwrap_or_default();
                    let mut types = serde_json::Map::new();
                    for a in &arr {
                        let t = a["type"].as_str().unwrap_or("").to_string();
                        let n = types.get(&t).and_then(Value::as_u64).unwrap_or(0);
                        types.insert(t, json!(n + 1));
                    }
                    json!({"types": types, "tail": arr[arr.len().saturating_sub(2)..]})
                } else {
                    out.clone()
                };
                if write {
                    let mut f = f.clone();
                    f["expect"] = got;
                    let path = format!("{}/recipes/{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"), r.key);
                    std::fs::write(path, serde_json::to_string_pretty(&f).unwrap() + "\n").unwrap();
                    continue;
                }
                assert_eq!(got, f["expect"], "{}/{name}: script output differs from the fixture's expect", r.key);
                // A webhook's (or fetch's) script runs on the stored payload, which has the redact
                // list applied: removing those fields must not change what the script does.
                if !pull {
                    let paths = super::super::redact::paths_with(&strings(&r.manifest["redact"]), input["settings"].as_object().unwrap_or(&serde_json::Map::new()));
                    let body = input["payload"].to_string();
                    if let Some(kept) = super::super::redact::body(Some("application/json"), &body, &paths) {
                        let mut redacted = input.clone();
                        redacted["payload"] = serde_json::from_str(&kept).unwrap();
                        redacted["body"] = json!(kept);
                        let again: Value = super::super::sandbox::run_engine(&r.script, &redacted.to_string(), std::time::Duration::from_secs(5))
                            .map(|t| serde_json::from_str(&t).unwrap())
                            .unwrap_or_else(|e| panic!("{}/{name} (redacted): {e}", r.key));
                        assert_eq!(again, out, "{}/{name}: the redact list removes something the script reads", r.key);
                    }
                }
            }
        }
    }
}
