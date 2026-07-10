use v2::sources::{LoadOptions, ModelSource};

/// Mirrors the CLI's `--ctx`/`--source`/`--family` scan flags exactly, calling
/// straight into the `v2` lib crate — no subprocess, no HTTP.
#[tauri::command]
pub fn scan(ctx: u32, source: String, family: Option<String>) -> Result<serde_json::Value, String> {
    let hw = v2::hardware::detect();
    let host = v2::ollama::default_host();
    let src = match source.as_str() {
        "catalog" => ModelSource::Catalog,
        "ollama" => ModelSource::Ollama,
        "all" => ModelSource::All,
        _ => ModelSource::Auto,
    };
    let load_opts = LoadOptions {
        source: src,
        ollama_host: &host,
        accepted: None,
        enterprise: false,
    };

    let models: Vec<_> = v2::sources::load(&load_opts)?
        .into_iter()
        .filter(|m| {
            family
                .as_deref()
                .map(|f| m.family.to_lowercase().contains(&f.to_lowercase()))
                .unwrap_or(true)
        })
        .collect();

    let results: Vec<_> = models
        .iter()
        .map(|m| v2::engine::evaluate(m, &hw, ctx, None))
        .collect();

    Ok(v2::display::scan_json(&hw, &results, ctx))
}
