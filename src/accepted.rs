use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct AcceptedModels {
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AcceptedFile {
    accepted: Vec<String>,
}

impl AcceptedModels {
    pub fn load(path: Option<&Path>) -> Result<Option<Self>, String> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => match std::env::var("V2_ACCEPTED") {
                Ok(p) if !p.trim().is_empty() => Path::new(&p).to_path_buf(),
                _ => return Ok(None),
            },
        };

        if !path.exists() {
            return Err(format!("accepted models file not found: {}", path.display()));
        }

        let raw = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

        let patterns = if raw.trim_start().starts_with('{') {
            serde_json::from_str::<AcceptedFile>(&raw)
                .map_err(|e| format!("invalid accepted models JSON: {e}"))?
                .accepted
        } else {
            raw.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        };

        if patterns.is_empty() {
            return Err("accepted models file is empty".into());
        }

        Ok(Some(Self { patterns }))
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn matches(&self, text: &str) -> bool {
        let hay = text.to_lowercase();
        self.patterns.iter().any(|p| glob_match(&p.to_lowercase(), &hay))
    }

    pub fn matches_model(&self, model: &crate::models::Model) -> bool {
        model.match_keys().iter().any(|k| self.matches(k))
    }

    pub fn pattern_covers_model(&self, pattern: &str, model: &crate::models::Model) -> bool {
        let p = pattern.to_lowercase();
        model.match_keys().iter().any(|k| glob_match(&p, k))
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return text.contains(pattern) || pattern.contains(text);
    }

    if let Some(rest) = pattern.strip_prefix('*') {
        if rest.is_empty() {
            return true;
        }
        return text.ends_with(rest) || text.contains(rest);
    }

    if let Some(rest) = pattern.strip_suffix('*') {
        return text.starts_with(rest);
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.is_empty() {
        return true;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
            continue;
        }
        if i == parts.len() - 1 {
            return text[pos..].contains(part);
        }
        let Some(found) = text[pos..].find(part) else {
            return false;
        };
        pos += found + part.len();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_prefix_suffix() {
        assert!(glob_match("qwen3*", "qwen3:8b"));
        assert!(glob_match("*:8b", "qwen3:8b"));
        assert!(glob_match("llama3.2", "llama3.2:latest"));
    }
}
