//! User-registered remote model endpoints — e.g. a model you host on Modal.
//!
//! An endpoint is just a base URL + a model id (+ an optional API key). Most
//! hosted inference (Modal/vLLM/TGI/OpenAI/Together/…) speaks the OpenAI
//! `/v1/chat/completions` shape, which is the default; point-and-add an Ollama
//! server by choosing the `ollama` kind. Stored as JSON in `~/.v2/endpoints.json`.
//!
//! HTTPS works because the daemon build enables the `remote` feature (ureq TLS).

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::paths;

/// Reachability check + model discovery shouldn't hang on a dead host.
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const CHAT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ENDPOINT_ERROR_BODY: u64 = 64 * 1024;

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

/// Stable labels this endpoint can be addressed by from v2 surfaces.
pub fn aliases(ep: &Endpoint) -> Vec<String> {
    let mut out = vec![ep.name.clone()];
    if ep.model != ep.name {
        out.push(ep.model.clone());
    }
    out
}

/// Whether a caller's requested model should route to this endpoint. Friendly
/// endpoint names are case-insensitive; provider model ids are exact.
pub fn matches_model(ep: &Endpoint, model: &str) -> bool {
    let model = model.trim();
    ep.model == model || ep.name == model || ep.name.eq_ignore_ascii_case(model)
}

/// Registered endpoint matching a requested model id or friendly endpoint name.
pub fn find_model(model: &str) -> Option<Endpoint> {
    load().into_iter().find(|ep| matches_model(ep, model))
}

pub fn kind_label(kind: ApiKind) -> &'static str {
    match kind {
        ApiKind::Openai => "openai",
        ApiKind::Ollama => "ollama",
    }
}

/// Canonical upstream root for an endpoint. We accept the common forms users
/// paste into the panel:
///   - host:port                 -> http://host:port for local/private IPs
///   - api.example.com           -> https://api.example.com
///   - https://host/v1           -> https://host     (OpenAI-compatible)
///   - http://host:11434/api     -> http://host:11434 (Ollama)
pub fn normalize_base_url(input: &str, kind: ApiKind) -> Result<String, String> {
    let mut u = input.trim().trim_end_matches('/').to_string();
    if u.is_empty() {
        return Err("endpoint url is required".into());
    }
    if u.contains('?') || u.contains('#') {
        return Err("endpoint url must not include a query string or fragment".into());
    }
    if u.contains("://") && !u.starts_with("http://") && !u.starts_with("https://") {
        return Err("endpoint url must use http:// or https://".into());
    }
    if !u.starts_with("http://") && !u.starts_with("https://") {
        u = format!("{}://{}", default_scheme(&u), u);
    }
    let authority = u
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .unwrap_or("");
    if authority.contains('@') {
        return Err("endpoint url must not include username or password".into());
    }
    match kind {
        ApiKind::Openai => {
            u = strip_suffix_ci(u, "/v1/chat/completions");
            u = strip_suffix_ci(u, "/v1/models");
            u = strip_suffix_ci(u, "/v1");
        }
        ApiKind::Ollama => {
            u = strip_suffix_ci(u, "/api/chat");
            u = strip_suffix_ci(u, "/api/tags");
            u = strip_suffix_ci(u, "/api");
        }
    }
    Ok(u.trim_end_matches('/').to_string())
}

fn strip_suffix_ci(mut s: String, suffix: &str) -> String {
    if s.to_lowercase().ends_with(&suffix.to_lowercase()) {
        let keep = s.len().saturating_sub(suffix.len());
        s.truncate(keep);
        s.truncate(s.trim_end_matches('/').len());
    }
    s
}

fn default_scheme(raw: &str) -> &'static str {
    let host = raw.split('/').next().unwrap_or(raw);
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    let host = host.trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("0.0.0.0")
        || host == "::1"
        || host == "::"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || host.ends_with(".local")
        || !host.contains('.')
        || is_private_172(host)
    {
        "http"
    } else {
        "https"
    }
}

fn is_private_172(host: &str) -> bool {
    let mut parts = host.split('.');
    match (parts.next(), parts.next()) {
        (Some("172"), Some(second)) => second.parse::<u8>().map(|n| (16..=31).contains(&n)).unwrap_or(false),
        _ => false,
    }
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
    write_private(&path, &raw).map_err(|e| e.to_string())
}

fn write_private(path: &std::path::Path, raw: &str) -> std::io::Result<()> {
    paths::write_private(path, raw.as_bytes())
}

/// Add (or replace by name) an endpoint.
pub fn add(ep: Endpoint) -> Result<(), String> {
    if ep.name.trim().is_empty() || ep.url.trim().is_empty() {
        return Err("name and url are required".into());
    }
    let ep = Endpoint {
        name: ep.name.trim().to_string(),
        url: normalize_base_url(&ep.url, ep.kind)?,
        model: ep.model.trim().to_string(),
        kind: ep.kind,
        api_key: ep.api_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()),
    };
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
    let base = normalize_base_url(url, kind)?;
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
    on_token: F,
) -> Result<(String, (u64, u64)), String> {
    chat_openai_with_timeout(ep, messages, CHAT_TIMEOUT, on_token)
}

/// Like `chat_openai`, but lets mesh serving bind the upstream network request
/// to the policy timeout so a silent endpoint cannot occupy a serving slot.
pub fn chat_openai_with_timeout<F: FnMut(&str) -> bool>(
    ep: &Endpoint,
    messages: &serde_json::Value,
    timeout: Duration,
    mut on_token: F,
) -> Result<(String, (u64, u64)), String> {
    let url = format!("{}/v1/chat/completions", normalize_base_url(&ep.url, ep.kind)?);
    let mut req = ureq::post(&url).timeout(timeout).set("Content-Type", "application/json");
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
            let mut msg = String::new();
            let _ = r.into_reader().take(MAX_ENDPOINT_ERROR_BODY).read_to_string(&mut msg);
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

    #[test]
    fn normalize_base_url_accepts_common_pasted_forms() {
        assert_eq!(
            normalize_base_url("https://api.example.com/v1", ApiKind::Openai).unwrap(),
            "https://api.example.com"
        );
        assert_eq!(
            normalize_base_url("https://api.example.com/v1/chat/completions", ApiKind::Openai).unwrap(),
            "https://api.example.com"
        );
        assert_eq!(
            normalize_base_url("http://192.168.1.7:11434/api", ApiKind::Ollama).unwrap(),
            "http://192.168.1.7:11434"
        );
        assert_eq!(
            normalize_base_url("192.168.1.7:8000", ApiKind::Openai).unwrap(),
            "http://192.168.1.7:8000"
        );
        assert_eq!(
            normalize_base_url("[::1]:8000", ApiKind::Openai).unwrap(),
            "http://[::1]:8000"
        );
        assert_eq!(
            normalize_base_url("api.example.com", ApiKind::Openai).unwrap(),
            "https://api.example.com"
        );
        assert_eq!(
            normalize_base_url("lanbox:8000", ApiKind::Openai).unwrap(),
            "http://lanbox:8000"
        );
        assert!(normalize_base_url("ftp://api.example.com", ApiKind::Openai).is_err());
        assert!(normalize_base_url("https://user:pass@api.example.com/v1", ApiKind::Openai).is_err());
        assert!(normalize_base_url("https://api.example.com/v1?key=secret", ApiKind::Openai).is_err());
        assert!(normalize_base_url("https://api.example.com/v1#frag", ApiKind::Openai).is_err());
    }

    #[test]
    fn aliases_include_name_and_model_once() {
        let ep = Endpoint {
            name: "zo".into(),
            url: "https://zo.example".into(),
            model: "meta-llama/Llama-3.1-8B-Instruct".into(),
            kind: ApiKind::Openai,
            api_key: None,
        };
        assert_eq!(aliases(&ep), vec!["zo", "meta-llama/Llama-3.1-8B-Instruct"]);

        let ep = Endpoint { model: "zo".into(), ..ep };
        assert_eq!(aliases(&ep), vec!["zo"]);
    }

    #[test]
    fn matches_endpoint_by_friendly_name_or_exact_model_id() {
        let ep = Endpoint {
            name: "zo".into(),
            url: "https://zo.example".into(),
            model: "Meta/CaseSensitive".into(),
            kind: ApiKind::Openai,
            api_key: None,
        };
        assert!(matches_model(&ep, "zo"));
        assert!(matches_model(&ep, "ZO"));
        assert!(matches_model(&ep, "Meta/CaseSensitive"));
        assert!(!matches_model(&ep, "meta/casesensitive"));
    }
}
