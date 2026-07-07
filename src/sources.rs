use std::collections::HashMap;

use crate::accepted::AcceptedModels;
use crate::models::{Model, ModelOrigin, catalog, catalog_by_ollama_tag};
use crate::ollama;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Auto,
    Catalog,
    Ollama,
    All,
}

pub struct LoadOptions<'a> {
    pub source: ModelSource,
    pub ollama_host: &'a str,
    pub accepted: Option<&'a AcceptedModels>,
    pub enterprise: bool,
}

pub fn load(options: &LoadOptions<'_>) -> Result<Vec<Model>, String> {
    if options.enterprise && options.accepted.is_none() {
        return Err(
            "enterprise mode requires --accepted or V2_ACCEPTED".into(),
        );
    }

    let mut models = match options.source {
        ModelSource::Catalog => catalog(),
        ModelSource::Ollama => ollama::fetch_local(options.ollama_host)?,
        ModelSource::All => merge_catalog_and_ollama(options.ollama_host)?,
        ModelSource::Auto => {
            match ollama::fetch_local(options.ollama_host) {
                Ok(local) if !local.is_empty() => merge_with_catalog(local),
                Ok(_) => catalog(),
                Err(_) => catalog(),
            }
        }
    };

    if let Some(accepted) = options.accepted {
        models = apply_accepted(models, accepted);
    }

    if models.is_empty() {
        return Err("no models matched the current filters".into());
    }

    Ok(models)
}

fn merge_catalog_and_ollama(host: &str) -> Result<Vec<Model>, String> {
    let local = ollama::fetch_local(host).unwrap_or_default();
    Ok(merge_with_catalog(local))
}

fn merge_with_catalog(local: Vec<Model>) -> Vec<Model> {
    let mut by_key: HashMap<String, Model> = HashMap::new();

    for m in catalog() {
        let key = m
            .ollama_name
            .as_deref()
            .unwrap_or(&m.name)
            .to_lowercase();
        by_key.insert(key, m);
    }

    for m in local {
        let key = m
            .ollama_name
            .as_deref()
            .unwrap_or(&m.name)
            .to_lowercase();
        by_key.insert(key, m);
    }

    let mut out: Vec<_> = by_key.into_values().collect();
    out.sort_by(|a, b| a.display_name().cmp(b.display_name()));
    out
}

fn apply_accepted(mut models: Vec<Model>, accepted: &AcceptedModels) -> Vec<Model> {
    models.retain(|m| accepted.matches_model(m));

    let index = catalog_by_ollama_tag();
    for pattern in accepted.patterns() {
        if models
            .iter()
            .any(|m| accepted.pattern_covers_model(pattern, m))
        {
            continue;
        }

        let p = pattern.to_lowercase();
        if let Some(m) = index.get(&p) {
            if !models.iter().any(|x| x.ollama_name == m.ollama_name) {
                models.push(m.clone());
            }
            continue;
        }

        if let Some(m) = infer_from_pattern(pattern) {
            models.push(m);
        }
    }

    models.sort_by(|a, b| a.display_name().cmp(b.display_name()));
    models
}

fn infer_from_pattern(pattern: &str) -> Option<Model> {
    let tag = pattern.trim();
    let family = tag.split(':').next()?.to_string();
    let params = tag
        .split(':')
        .nth(1)
        .and_then(parse_size_token)
        .or_else(|| guess_params_from_tag(tag))?;

    Some(Model {
        name: tag.to_string(),
        family: family.clone(),
        params,
        params_active: None,
        is_moe: tag.contains("mixtral") || tag.contains("a3b"),
        context_length: 8192,
        id: tag.to_string(),
        ollama_name: Some(tag.to_string()),
        weight_bytes: None,
        fixed_quant: None,
        origin: ModelOrigin::Catalog,
    })
}

fn parse_size_token(token: &str) -> Option<u64> {
    crate::models::parse_param_size(token).or_else(|| {
        let t = token.to_lowercase();
        t.strip_suffix('b')
            .and_then(|n| n.parse::<f64>().ok())
            .map(|n| (n * 1e9) as u64)
    })
}

fn guess_params_from_tag(tag: &str) -> Option<u64> {
    for part in tag.split([':', '-', '_']) {
        if let Some(p) = parse_size_token(part) {
            return Some(p);
        }
    }
    None
}
