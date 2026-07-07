use colored::Colorize;

use crate::accepted::AcceptedModels;
use crate::bandwidth;
use crate::engine::{CompatResult, FitType};
use crate::hardware::HardwareInfo;
use crate::models::{Model, ModelOrigin, Quant};
use crate::sources::LoadOptions;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

pub fn print_hardware(
    hw: &HardwareInfo,
    ctx: u32,
    load_opts: &LoadOptions<'_>,
    accepted: Option<&AcceptedModels>,
) {
    let gpu = if hw.gpus.is_empty() {
        "no gpu".yellow().to_string()
    } else {
        let g = &hw.gpus[0];
        let mem = if g.vram_bytes == 0 {
            "?".to_string()
        } else if g.shared_memory {
            format!("{:.0}G unified", g.vram_bytes as f64 / GIB)
        } else {
            format!("{:.0}G vram", g.vram_bytes as f64 / GIB)
        };
        if hw.gpus.len() > 1 {
            format!("{}× GPU {}", hw.gpus.len(), mem)
        } else {
            mem
        }
    };

    let ram = format!("{:.0}G ram", hw.ram_bytes as f64 / GIB);
    let ctx_k = if ctx >= 1000 {
        format!("{}k ctx", ctx / 1000)
    } else {
        format!("{} ctx", ctx)
    };

    let source = match load_opts.source {
        crate::sources::ModelSource::Ollama => "ollama",
        crate::sources::ModelSource::Catalog => "catalog",
        crate::sources::ModelSource::All => "all",
        crate::sources::ModelSource::Auto => "auto",
    };

    let policy = if accepted.is_some() || load_opts.enterprise {
        " · enterprise".yellow().to_string()
    } else {
        String::new()
    };

    println!(
        "{}  {} · {} · {} · {} · {}{}",
        "v2".bold(),
        gpu.cyan(),
        ram,
        hw.os.to_string().dimmed(),
        ctx_k.dimmed(),
        source.dimmed(),
        policy,
    );
}

pub fn print_hw_json(hw: &HardwareInfo) {
    let gpus: Vec<_> = hw.gpus.iter().map(|g| {
        serde_json::json!({
            "name": g.name,
            "vendor": g.vendor.to_string(),
            "vram_gb": format!("{:.1}", g.vram_bytes as f64 / GIB),
            "shared": g.shared_memory,
        })
    }).collect();
    let out = serde_json::json!({
        "gpus": gpus,
        "ram_gb": format!("{:.1}", hw.ram_bytes as f64 / GIB),
        "cpu": hw.cpu_name,
        "os": hw.os.to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}

pub fn print_model_list(models: &[Model]) {
    println!("{}  {} models", "v2".bold(), models.len());
    println!(
        "  {}  {:<22}  {:<7}  {:<8}  {}",
        "src".dimmed(),
        "model".dimmed(),
        "size".dimmed(),
        "ctx".dimmed(),
        "quant".dimmed(),
    );
    for m in models {
        let size = if m.is_moe {
            format!(
                "{}*",
                format_params(m.params_active.unwrap_or(m.params))
            )
        } else {
            format_params(m.params)
        };
        let quant = m
            .fixed_quant
            .map(|q| q.label().to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<7}  {:<22}  {:<7}  {:<8}  {}",
            origin_tag(m.origin),
            m.display_name(),
            size,
            format!("{}k", m.context_length / 1000),
            quant.dimmed(),
        );
    }
}

fn origin_tag(origin: ModelOrigin) -> String {
    match origin {
        ModelOrigin::OllamaLocal => "ollama".cyan().to_string(),
        ModelOrigin::Catalog => "catalog".dimmed().to_string(),
    }
}

fn format_params(p: u64) -> String {
    if p >= 1_000_000_000 {
        format!("{:.0}B", p as f64 / 1e9)
    } else if p >= 1_000_000 {
        format!("{:.0}M", p as f64 / 1e6)
    } else {
        format!("{}", p)
    }
}

pub fn print_results(
    results: &[(
        &Model,
        Vec<(Quant, CompatResult)>,
        Option<(Quant, CompatResult)>,
    )],
    verbose: bool,
    hw: &HardwareInfo,
    ctx: u32,
) {
    let can_run: Vec<_> = results.iter().filter(|(_, _, best)| best.is_some()).collect();
    let cant_run: Vec<_> = results.iter().filter(|(_, _, best)| best.is_none()).collect();

    if can_run.is_empty() {
        println!("{}", "no models fit".red());
    } else {
        let mut rows: Vec<_> = can_run;
        rows.sort_by(|a, b| {
            let (_, _, best_a) = a;
            let (_, _, best_b) = b;
            fit_rank(best_a.as_ref().map(|(_, r)| &r.fit))
                .cmp(&fit_rank(best_b.as_ref().map(|(_, r)| &r.fit)))
                .then_with(|| {
                    best_a
                        .as_ref()
                        .map(|(_, r)| r.vram_required)
                        .unwrap_or(0)
                        .cmp(&best_b.as_ref().map(|(_, r)| r.vram_required).unwrap_or(0))
                })
        });

        println!(
            "  {}  {:<22}  {:<7}  {:<7}  {}",
            "fit".dimmed(),
            "model".dimmed(),
            "quant".dimmed(),
            "mem".dimmed(),
            "speed".dimmed(),
        );

        for (model, all_quants, best) in rows {
            let (best_q, best_r) = best.as_ref().unwrap();
            let name = model_label(model);
            println!(
                "  {:<4}  {:<22}  {:<7}  {:<7}  {}",
                fit_tag(&best_r.fit),
                name,
                best_q.label().green(),
                mem_gb(best_r.vram_required).dimmed(),
                tps_cell(model, *best_q, ctx, hw, &best_r.fit),
            );

            if verbose {
                for (q, r) in all_quants {
                    if Some(*q) == Some(*best_q) {
                        continue;
                    }
                    println!(
                        "  {:<4}  {:<22}  {:<7}  {:<7}  {}",
                        fit_tag(&r.fit).dimmed(),
                        "",
                        q.label().dimmed(),
                        mem_gb(r.vram_required).dimmed(),
                        tps_cell(model, *q, ctx, hw, &r.fit).dimmed(),
                    );
                }
            } else if best_r.notes.iter().any(|n| !n.contains("shared")) {
                for note in &best_r.notes {
                    if !note.contains("shared") {
                        println!("       {}", note.yellow());
                    }
                }
            }
        }
    }

    if !cant_run.is_empty() {
        let names: Vec<String> = cant_run
            .iter()
            .map(|(m, _, _)| model_label(m).to_string())
            .collect();
        println!();
        println!(
            "  {} {} too large: {}",
            "✗".red(),
            cant_run.len(),
            names.join(", ").dimmed()
        );
    }
}

fn model_label(model: &Model) -> String {
    let base = model.display_name().to_string();
    if model.is_moe {
        format!("{base}*")
    } else {
        base
    }
}

fn mem_gb(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / GIB)
}

fn tps_cell(model: &Model, quant: Quant, ctx: u32, hw: &HardwareInfo, fit: &FitType) -> String {
    match bandwidth::estimate_tps(model, quant, ctx, hw, fit) {
        Some((tps, rough)) => bandwidth::tps_label(tps, rough).dimmed().to_string(),
        None => "-".dimmed().to_string(),
    }
}

fn fit_tag(fit: &FitType) -> String {
    match fit {
        FitType::FullGpu => "gpu".green().to_string(),
        FitType::PartialOffload { offload_pct } => format!("~{}%", offload_pct).yellow().to_string(),
        FitType::CpuOnly => "cpu".dimmed().to_string(),
        FitType::TooBig => "n/a".red().to_string(),
    }
}

fn fit_rank(fit: Option<&FitType>) -> u8 {
    match fit {
        Some(FitType::FullGpu) => 0,
        Some(FitType::PartialOffload { .. }) => 1,
        Some(FitType::CpuOnly) => 2,
        Some(FitType::TooBig) => 3,
        None => 4,
    }
}

pub fn print_json(
    hw: &HardwareInfo,
    results: &[(
        &Model,
        Vec<(Quant, CompatResult)>,
        Option<(Quant, CompatResult)>,
    )],
    ctx: u32,
) {
    use std::collections::HashMap;

    let gpus: Vec<_> = hw
        .gpus
        .iter()
        .map(|g| {
            let mut m = HashMap::new();
            m.insert("name", serde_json::Value::String(g.name.clone()));
            m.insert("vendor", serde_json::Value::String(g.vendor.to_string()));
            m.insert(
                "vram_gb",
                serde_json::Value::Number(
                    serde_json::Number::from_f64(g.vram_bytes as f64 / GIB)
                        .unwrap_or(serde_json::Number::from(0)),
                ),
            );
            m.insert("shared", serde_json::Value::Bool(g.shared_memory));
            m
        })
        .collect();

    let models: Vec<_> = results
        .iter()
        .map(|(model, all_quants, best)| {
            let quants: Vec<_> = all_quants
                .iter()
                .map(|(q, r)| {
                    let tps = bandwidth::estimate_tps(model, *q, ctx, hw, &r.fit)
                        .map(|(t, _)| (t * 10.0).round() / 10.0);
                    serde_json::json!({
                        "quant": q.label(),
                        "vram_gb": format!("{:.2}", r.vram_required as f64 / GIB),
                        "fit": fit_type_str(&r.fit),
                        "est_tps": tps,
                        "notes": r.notes,
                    })
                })
                .collect();

            let recommended = best.as_ref().map(|(q, _)| q.label());

            serde_json::json!({
                "name": model.name,
                "display_name": model.display_name(),
                "family": model.family,
                "id": model.id,
                "ollama_name": model.ollama_name,
                "origin": match model.origin {
                    ModelOrigin::Catalog => "catalog",
                    ModelOrigin::OllamaLocal => "ollama",
                },
                "is_moe": model.is_moe,
                "recommended_quant": recommended,
                "can_run": best.is_some(),
                "quants": quants,
            })
        })
        .collect();

    let out = serde_json::json!({
        "hardware": {
            "gpus": gpus,
            "ram_gb": format!("{:.1}", hw.ram_bytes as f64 / GIB),
            "cpu": hw.cpu_name,
            "os": hw.os.to_string(),
        },
        "models": models,
    });

    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

fn fit_type_str(fit: &FitType) -> &'static str {
    match fit {
        FitType::FullGpu => "full_gpu",
        FitType::PartialOffload { .. } => "partial_offload",
        FitType::CpuOnly => "cpu_only",
        FitType::TooBig => "too_big",
    }
}
