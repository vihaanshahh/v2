//! Phase 2: fit-aware model management — thin, opinionated wrappers over Ollama.
//!
//! The value v2 adds over `ollama pull` / `ollama run` is that it checks fit and
//! estimated speed *before* you download 40GB that will crawl, and picks a quant
//! matched to your hardware.

use std::io::{self, Write};

use colored::Colorize;
use serde::Serialize;

use crate::bandwidth;
use crate::engine::{self, fit_type_str, FitType};
use crate::hardware::HardwareInfo;
use crate::models::{catalog, Model, ModelOrigin, Quant};
use crate::ollama;
use crate::ollama_api;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Pure data version of `fit_preview` — no I/O, reusable by the desktop app's
/// pull-confirmation dialog (the CLI keeps its own inline-printed preview).
#[derive(Debug, Clone, Serialize)]
pub struct FitCheck {
    pub display_name: String,
    pub in_catalog: bool,
    pub fits: bool,
    pub fit: String,
    pub quant: Option<String>,
    pub vram_gb: Option<f64>,
    pub est_tps: Option<f64>,
    pub notes: Vec<String>,
}

pub fn fit_check(query: &str, hw: &HardwareInfo, ctx: u32) -> FitCheck {
    let Some(model) = resolve(query) else {
        return FitCheck {
            display_name: query.to_string(),
            in_catalog: false,
            fits: false,
            fit: "unknown".into(),
            quant: None,
            vram_gb: None,
            est_tps: None,
            notes: vec!["not in catalog — no fit estimate".into()],
        };
    };

    let Some((quant, result)) = engine::best_quant(&model, hw, ctx) else {
        return FitCheck {
            display_name: model.display_name().to_string(),
            in_catalog: true,
            fits: false,
            fit: fit_type_str(&FitType::TooBig).to_string(),
            quant: None,
            vram_gb: None,
            est_tps: None,
            notes: vec!["will not fit on this machine".into()],
        };
    };

    let est_tps = bandwidth::estimate_tps(&model, quant, ctx, hw, &result.fit)
        .map(|(t, _)| (t * 10.0).round() / 10.0);

    FitCheck {
        display_name: model.display_name().to_string(),
        in_catalog: true,
        fits: true,
        fit: fit_type_str(&result.fit).to_string(),
        quant: Some(quant.label().to_string()),
        vram_gb: Some(result.vram_required as f64 / GIB),
        est_tps,
        notes: result.notes,
    }
}

/// Resolve a user query ("qwen3:8b", "llama3.1") to a catalog model, or infer a
/// minimal one from the tag so we can still estimate fit.
pub fn resolve(query: &str) -> Option<Model> {
    let q = query.to_lowercase();
    let cat = catalog();
    // Exact ollama tag, then prefix, then name/family substring.
    if let Some(m) = cat.iter().find(|m| m.ollama_name.as_deref() == Some(q.as_str())) {
        return Some(m.clone());
    }
    if let Some(m) = cat.iter().find(|m| {
        m.ollama_name.as_deref().map(|t| t.starts_with(&q) || q.starts_with(t)).unwrap_or(false)
    }) {
        return Some(m.clone());
    }
    if let Some(m) = cat.iter().find(|m| {
        m.name.to_lowercase().contains(&q) || m.family.to_lowercase().contains(&q)
    }) {
        return Some(m.clone());
    }
    infer_from_tag(query)
}

fn infer_from_tag(tag: &str) -> Option<Model> {
    let family = tag.split(':').next()?.to_string();
    let params = tag.split([':', '-', '_', '.']).find_map(param_token)?;
    Some(Model {
        name: tag.to_string(),
        family,
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

/// Parse a size token like "13b", "70B", "1.5b", "3.8b" to param count.
/// Handles the plain `Nb` case that `parse_param_size` (K/M-only) misses.
fn param_token(token: &str) -> Option<u64> {
    crate::models::parse_param_size(token).or_else(|| {
        let t = token.to_lowercase();
        t.strip_suffix('b')
            .and_then(|n| n.parse::<f64>().ok())
            .map(|n| (n * 1e9) as u64)
    })
}

/// Print a one-line fit + speed preview for a model. Returns the recommended
/// quant and whether it can run at all.
fn fit_preview(model: &Model, hw: &HardwareInfo, ctx: u32) -> Option<(Quant, FitType, u64)> {
    let (quant, result) = engine::best_quant(model, hw, ctx)?;
    let fit_word = match &result.fit {
        FitType::FullGpu => "fits fully on GPU".green(),
        FitType::PartialOffload { offload_pct } => {
            format!("{}% offloaded to CPU", offload_pct).yellow()
        }
        FitType::CpuOnly => "CPU only (slow)".yellow(),
        FitType::TooBig => "will not fit".red(),
    };
    let speed = bandwidth::estimate_tps(model, quant, ctx, hw, &result.fit)
        .map(|(t, rough)| bandwidth::tps_label(t, rough))
        .unwrap_or_else(|| "?".into());
    println!(
        "  {} · {} · {} · est {}",
        model.display_name().bold(),
        quant.label().green(),
        fit_word,
        speed.cyan(),
    );
    Some((quant, result.fit, result.vram_required))
}

/// `v2 pull <model>` — fit-check, confirm, then stream the download.
pub fn pull(host: &str, hw: &HardwareInfo, ctx: u32, query: &str, yes: bool) -> Result<(), String> {
    match resolve(query) {
        Some(model) => {
            println!("v2 pull");
            let preview = fit_preview(&model, hw, ctx);
            if matches!(preview.map(|(_, f, _)| f), Some(FitType::TooBig)) && !yes {
                if !confirm(&format!("{} may not run here. Pull anyway?", query)) {
                    println!("aborted.");
                    return Ok(());
                }
            } else if !yes && !confirm(&format!("Pull {}?", query)) {
                println!("aborted.");
                return Ok(());
            }
        }
        None => {
            println!("v2 pull  {} (not in catalog — no fit estimate)", query);
            if !yes && !confirm(&format!("Pull {}?", query)) {
                println!("aborted.");
                return Ok(());
            }
        }
    }

    let mut last = String::new();
    ollama_api::pull(host, query, |status, completed, total| {
        if total > 0 {
            let pct = completed as f64 / total as f64 * 100.0;
            print!(
                "\r  {:<20} {:>5.1}%  {:.1}/{:.1}G   ",
                status,
                pct,
                completed as f64 / GIB,
                total as f64 / GIB
            );
        } else if status != last {
            print!("\r  {:<40}", status);
            last = status.to_string();
        }
        let _ = io::stdout().flush();
    })?;
    println!("\r  {}                                        ", "done".green());
    Ok(())
}

/// `v2 rm <model>` — delete a local model.
pub fn rm(host: &str, model: &str) -> Result<(), String> {
    ollama_api::delete(host, model)?;
    println!("removed {model}");
    Ok(())
}

/// Installed models with fit info for this machine — pure data, shared by `v2
/// ps` (prints it) and the desktop app's Models tab (renders it).
#[derive(Debug, Clone, Serialize)]
pub struct InstalledModel {
    pub name: String,
    pub display_name: String,
    pub size_gb: Option<f64>,
    pub fit: String,
    pub offload_pct: Option<u8>,
    pub quant: Option<String>,
    pub est_tps: Option<f64>,
    pub tps_label: Option<String>,
}

pub fn installed_with_fit(host: &str, hw: &HardwareInfo, ctx: u32) -> Result<Vec<InstalledModel>, String> {
    let mut rows = ollama::fetch_local(host)?;
    rows.sort_by(|a, b| a.display_name().cmp(b.display_name()));
    Ok(rows
        .iter()
        .map(|m| {
            let size_gb = m.weight_bytes.map(|b| b as f64 / GIB);
            match engine::best_quant(m, hw, ctx) {
                Some((q, r)) => {
                    let (est_tps, tps_label) = match bandwidth::estimate_tps(m, q, ctx, hw, &r.fit) {
                        Some((t, rough)) => (Some((t * 10.0).round() / 10.0), Some(bandwidth::tps_label(t, rough))),
                        None => (None, None),
                    };
                    let offload_pct = match r.fit {
                        FitType::PartialOffload { offload_pct } => Some(offload_pct),
                        _ => None,
                    };
                    InstalledModel {
                        name: m.name.clone(),
                        display_name: m.display_name().to_string(),
                        size_gb,
                        fit: fit_type_str(&r.fit).to_string(),
                        offload_pct,
                        quant: Some(q.label().to_string()),
                        est_tps,
                        tps_label,
                    }
                }
                None => InstalledModel {
                    name: m.name.clone(),
                    display_name: m.display_name().to_string(),
                    size_gb,
                    fit: fit_type_str(&FitType::TooBig).to_string(),
                    offload_pct: None,
                    quant: None,
                    est_tps: None,
                    tps_label: None,
                },
            }
        })
        .collect())
}

/// `v2 ps` — installed models with fit info for this machine.
pub fn ps_installed(host: &str, hw: &HardwareInfo, ctx: u32) -> Result<(), String> {
    let rows = installed_with_fit(host, hw, ctx)?;
    if rows.is_empty() {
        println!("v2 ps  no models installed  (try `v2 pull qwen3:8b`)");
        return Ok(());
    }
    crate::ui::section(&format!("installed  ({})", rows.len()));
    for m in &rows {
        let size = m.size_gb.map(|g| format!("{g:.1}G")).unwrap_or_default();
        let fit = fit_word_str(&m.fit, m.offload_pct);
        let speed = m.tps_label.clone().unwrap_or_default();
        println!("  {:<28} {:>7}  {:<18} {}", m.display_name, size, fit, speed.dimmed());
    }
    Ok(())
}

fn fit_word_str(fit: &str, offload_pct: Option<u8>) -> String {
    match fit {
        "full_gpu" => "gpu".green().to_string(),
        "partial_offload" => format!("~{}% offload", offload_pct.unwrap_or(0)).yellow().to_string(),
        "cpu_only" => "cpu".dimmed().to_string(),
        _ => "n/a".red().to_string(),
    }
}

/// `v2 run <model>` — ensure it's installed (pull if missing), then chat.
pub fn run(host: &str, hw: &HardwareInfo, ctx: u32, query: &str, yes: bool) -> Result<(), String> {
    let installed = ollama::fetch_local(host).unwrap_or_default();
    let have = installed.iter().any(|m| {
        m.ollama_name.as_deref() == Some(query)
            || m.display_name().eq_ignore_ascii_case(query)
    });
    if !have {
        println!("{query} not installed.");
        pull(host, hw, ctx, query, yes)?;
    }
    chat_repl(host, query)
}

fn chat_repl(host: &str, model: &str) -> Result<(), String> {
    println!("v2 run {}  (empty line or Ctrl-D to exit)", model.bold());
    let mut messages: Vec<serde_json::Value> = Vec::new();
    let stdin = io::stdin();
    loop {
        print!("{} ", ">".cyan());
        io::stdout().flush().ok();
        let mut input = String::new();
        let n = stdin.read_line(&mut input).map_err(|e| e.to_string())?;
        if n == 0 {
            break; // EOF
        }
        let input = input.trim();
        if input.is_empty() {
            break;
        }
        messages.push(serde_json::json!({ "role": "user", "content": input }));

        let (reply, stats) = ollama_api::chat_stream(
            host,
            model,
            &serde_json::Value::Array(messages.clone()),
            |tok| {
                print!("{tok}");
                io::stdout().flush().ok();
                true
            },
        )?;
        println!();
        if stats.eval_count > 0 && stats.total_duration > 0 {
            let tps = stats.eval_count as f64 / (stats.total_duration as f64 / 1e9);
            println!("{}", format!("  {} tok · {:.0} tok/s", stats.eval_count, tps).dimmed());
        }
        messages.push(serde_json::json!({ "role": "assistant", "content": reply }));
    }
    println!("bye.");
    Ok(())
}


fn confirm(prompt: &str) -> bool {
    print!("{prompt} [Y/n] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let a = line.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_catalog_tag() {
        let m = resolve("qwen3:8b").expect("known tag");
        assert_eq!(m.params, 8_000_000_000);
    }

    #[test]
    fn infers_unknown_tag() {
        let m = resolve("someorg-13b:latest").expect("inferred");
        assert_eq!(m.params, 13_000_000_000);
    }

    fn beefy_hw() -> HardwareInfo {
        HardwareInfo {
            gpus: vec![crate::hardware::GpuInfo {
                name: "RTX 4090".into(),
                vendor: crate::hardware::Vendor::Nvidia,
                vram_bytes: 24 << 30,
                shared_memory: false,
            }],
            cpu_name: "test cpu".into(),
            ram_bytes: 64 << 30,
            os: crate::hardware::Os::Linux,
        }
    }

    // `fit_check` is the data function behind the desktop app's pull-preview
    // dialog (and shares its wording vocabulary with `--json`'s `fit_type_str`)
    // — this is the "API test" for that command's underlying logic.
    #[test]
    fn fit_check_known_model_reports_fit_and_speed() {
        let check = fit_check("qwen3:8b", &beefy_hw(), 4096);
        assert!(check.in_catalog);
        assert!(check.fits);
        assert_eq!(check.fit, "full_gpu");
        assert!(check.quant.is_some());
        assert!(check.vram_gb.unwrap() > 0.0);
    }

    #[test]
    fn fit_check_unknown_query_has_no_estimate() {
        let check = fit_check("not-a-real-model-xyz", &beefy_hw(), 4096);
        assert!(!check.in_catalog);
        assert!(!check.fits);
        assert_eq!(check.fit, "unknown");
        assert!(check.quant.is_none());
        assert!(!check.notes.is_empty());
    }

    #[test]
    fn fit_check_oversized_model_reports_too_big() {
        // Inferred (not catalog) tag with an absurd param count: guarantees
        // nothing fits, even on the beefy test rig — proves fit_check's
        // "in catalog but will not fit" branch, not just the happy path.
        let check = fit_check("hugeorg-900b:latest", &beefy_hw(), 4096);
        assert!(check.in_catalog);
        assert!(!check.fits);
        assert_eq!(check.fit, "too_big");
        assert!(check.quant.is_none());
    }
}
