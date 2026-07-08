//! User-registered remote model endpoints — e.g. a model you host on Modal.
//!
//! An endpoint is just a base URL + a model id (+ an optional API key). Most
//! hosted inference (Modal/vLLM/TGI/OpenAI/Together/…) speaks the OpenAI
//! `/v1/chat/completions` shape, which is the default; point-and-add an Ollama
//! server by choosing the `ollama` kind. Stored as JSON in `~/.v2/endpoints.json`.
//!
//! HTTPS works because the daemon build enables the `remote` feature (ureq TLS).

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::paths;

/// Reachability check + model discovery shouldn't hang on a dead host.
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    /// OpenAI-compatible `/v1/chat/completions` (Modal, vLLM, TGI, OpenAI, …).
    #[default]
    Openai,
    /// A remote Ollama server (`/api/chat`).
    Ollama,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Friendly name shown in the panel.
    pub name: String,
    /// Base URL, e.g. `https://youruser--app.modal.run`.
    pub url: String,
    /// Model id the endpoint expects, e.g. `meta-llama/Llama-3.1-8B-Instruct`.
    pub model: String,
    #[serde(default)]
    pub kind: ApiKind,
    /// Optional bearer token (sent as `Authorization: Bearer …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

fn store_path() -> Result<std::path::PathBuf, String> {
    paths::file("endpoints.json").map_err(|e| e.to_string())
}

/// All registered endpoints (empty if none / unreadable).
pub fn load() -> Vec<Endpoint> {
    let Ok(path) = store_path() else { return Vec::new() };
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(list: &[Endpoint]) -> Result<(), String> {
    let path = store_path()?;
    let raw = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| e.to_string())
}

/// Add (or replace by name) an endpoint.
pub fn add(ep: Endpoint) -> Result<(), String> {
    if ep.name.trim().is_empty() || ep.url.trim().is_empty() {
        return Err("name and url are required".into());
    }
    let mut list = load();
    list.retain(|e| e.name != ep.name);
    list.push(ep);
    save(&list)
}

/// Remove an endpoint by name. Returns whether one was removed.
pub fn remove(name: &str) -> Result<bool, String> {
    let mut list = load();
    let before = list.len();
    list.retain(|e| e.name != name);
    let removed = list.len() != before;
    save(&list)?;
    Ok(removed)
}

/// Guess the API kind from a URL so adding is one fewer question: an `/api`
/// path or the default Ollama port reads as Ollama, everything else OpenAI.
pub fn guess_kind(url: &str) -> ApiKind {
    let u = url.to_lowercase();
    if u.contains(":11434") || u.trim_end_matches('/').ends_with("/api") {
        ApiKind::Ollama
    } else {
        ApiKind::Openai
    }
}

/// List the model ids an endpoint reports — OpenAI `GET /v1/models` or Ollama
/// `GET /api/tags`. A cheap reachability + capability check that never runs
/// inference, so adding a model can offer a pick-list and health can be tested.
pub fn probe(url: &str, kind: ApiKind, api_key: Option<&str>) -> Result<Vec<String>, String> {
    let base = url.trim_end_matches('/');
    let (path, list_key, id_key) = match kind {
        ApiKind::Openai => ("/v1/models", "data", "id"),
        ApiKind::Ollama => ("/api/tags", "models", "name"),
    };
    let mut req = ureq::get(&format!("{base}{path}")).timeout(PROBE_TIMEOUT);
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err(format!("endpoint returned HTTP {code}")),
        Err(e) => return Err(format!("cannot reach {base}: {e}")),
    };
    let v: serde_json::Value = resp.into_json().map_err(|e| format!("bad response: {e}"))?;
    let ids = v[list_key]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m[id_key].as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok(ids)
}

/// The host portion of the endpoint URL, for compact display.
pub fn host_of(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Stream a chat turn against an OpenAI-compatible endpoint. `on_token` gets each
/// content delta and returns false to abort. Returns the full reply and
/// `(prompt_tokens, completion_tokens)` when the endpoint reports usage.
pub fn chat_openai<F: FnMut(&str) -> bool>(
    ep: &Endpoint,
    messages: &serde_json::Value,
    mut on_token: F,
) -> Result<(String, (u64, u64)), String> {
    let url = format!("{}/v1/chat/completions", ep.url.trim_end_matches('/'));
    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if let Some(key) = ep.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let body = ureq::json!({
        "model": ep.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let resp = match req.send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            return Err(format!("endpoint returned {code}: {}", msg.trim()));
        }
        Err(e) => return Err(format!("cannot reach {url}: {e}")),
    };

    let reader = BufReader::new(resp.into_reader());
    let mut reply = String::new();
    let mut usage = (0u64, 0u64);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Some(data) = line.trim().strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
        if let Some(tok) = v["choices"][0]["delta"]["content"].as_str() {
            if !tok.is_empty() {
                reply.push_str(tok);
                if !on_token(tok) {
                    break;
                }
            }
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            usage.0 = u["prompt_tokens"].as_u64().unwrap_or(usage.0);
            usage.1 = u["completion_tokens"].as_u64().unwrap_or(usage.1);
        }
    }
    Ok((reply, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_kind_defaults_openai_detects_ollama() {
        assert_eq!(guess_kind("https://x.modal.run"), ApiKind::Openai);
        assert_eq!(guess_kind("https://api.openai.com/v1"), ApiKind::Openai);
        assert_eq!(guess_kind("http://192.168.1.5:11434"), ApiKind::Ollama);
        assert_eq!(guess_kind("http://box:11434/api"), ApiKind::Ollama);
    }

    #[test]
    fn host_of_strips_scheme_and_path() {
        assert_eq!(host_of("https://user--app.modal.run/v1"), "user--app.modal.run");
        assert_eq!(host_of("http://127.0.0.1:8000"), "127.0.0.1:8000");
    }
}
