use crate::models::{Model, ModelOrigin, Quant, parse_param_size};
use serde::Deserialize;

const DEFAULT_HOST: &str = "http://127.0.0.1:11434";

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: String,
    size: u64,
    details: Option<OllamaDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaDetails {
    family: Option<String>,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
}

pub fn default_host() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HOST.to_string())
}

pub fn fetch_local(host: &str) -> Result<Vec<Model>, String> {
    let url = format!("{}/api/tags", host.trim_end_matches('/'));
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("ollama unreachable at {url}: {e}"))?;

    let payload: TagsResponse = resp
        .into_json()
        .map_err(|e| format!("invalid ollama response: {e}"))?;

    Ok(payload.models.into_iter().filter_map(parse_ollama_model).collect())
}

fn parse_ollama_model(raw: OllamaModel) -> Option<Model> {
    let details = raw.details.unwrap_or(OllamaDetails {
        family: None,
        parameter_size: None,
        quantization_level: None,
    });

    let params = details
        .parameter_size
        .as_deref()
        .and_then(parse_param_size)
        .or_else(|| guess_params_from_name(&raw.name))?;

    let family = details
        .family
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| raw.name.split(':').next().unwrap_or("ollama").to_string());

    let fixed_quant = details
        .quantization_level
        .as_deref()
        .and_then(Quant::from_label);

    Some(Model {
        name: raw.name.clone(),
        family,
        params,
        params_active: None,
        is_moe: raw.name.contains("mixtral") || raw.name.contains("a3b"),
        context_length: 8192,
        id: raw.name.clone(),
        ollama_name: Some(raw.name),
        weight_bytes: Some(raw.size),
        fixed_quant,
        origin: ModelOrigin::OllamaLocal,
    })
}

fn guess_params_from_name(name: &str) -> Option<u64> {
    let tag = name.split(':').nth(1).unwrap_or(name);
    let lower = tag.to_lowercase();

    for token in lower.split(['-', '_', '.']) {
        if let Some(p) = parse_param_size(token) {
            return Some(p);
        }
        if let Some(num) = token.strip_suffix('b') {
            if let Ok(n) = num.parse::<f64>() {
                return Some((n * 1e9) as u64);
            }
        }
    }
    None
}
