/// VRAM estimation and compatibility checking.
///
/// Core math adapted from whichllm (MIT):
///   weights + KV cache + activations + framework overhead

use crate::hardware::{GpuInfo, HardwareInfo, Vendor};
use crate::models::{Model, Quant};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// 500 MB: framework + graph buffers (llama.cpp / ollama overhead)
const FRAMEWORK_OVERHEAD: u64 = 500 * MIB;

/// 3.5 MB per billion active-params per K context tokens (empirical, FP16 KV)
const KV_MB_PER_BPARAM_PER_KCTX: f64 = 3.5;

/// MoE: attention scales from active params × this multiplier
const MOE_ATTENTION_MULTIPLIER: f64 = 4.0;

// ─────────────────────────────────────────────────────────────────────────────

pub fn weight_bytes(model: &Model, quant: Quant) -> u64 {
    let bpw = quant.bytes_per_weight();
    (model.params as f64 * bpw) as u64
}

fn kv_cache_bytes(model: &Model, ctx: u32) -> u64 {
    let params_b = if model.is_moe {
        model.params_active.unwrap_or(model.params) as f64 / 1e9 * MOE_ATTENTION_MULTIPLIER
    } else {
        model.params as f64 / 1e9
    };
    let ctx_k = ctx as f64 / 1024.0;
    (params_b * ctx_k * KV_MB_PER_BPARAM_PER_KCTX * MIB as f64) as u64
}

fn activation_bytes(model: &Model, ctx: u32) -> u64 {
    let effective_p = if model.is_moe {
        model.params_active.unwrap_or(model.params)
    } else {
        model.params
    };
    let base = 400 * MIB;
    let param_term = (effective_p as f64 * 0.08) as u64;
    let ctx_term = (ctx as f64 / 4096.0 * 150.0 * MIB as f64) as u64;
    base + param_term + ctx_term
}

pub fn estimate_vram(model: &Model, quant: Quant, ctx: u32) -> u64 {
    weight_bytes(model, quant)
        + kv_cache_bytes(model, ctx)
        + activation_bytes(model, ctx)
        + FRAMEWORK_OVERHEAD
}

pub fn runtime_vram(model: &Model, quant: Quant, ctx: u32) -> u64 {
    if model.fixed_quant == Some(quant) {
        if let Some(weights) = model.weight_bytes {
            return weights + kv_cache_bytes(model, ctx) + activation_bytes(model, ctx) + FRAMEWORK_OVERHEAD;
        }
    }
    estimate_vram(model, quant, ctx)
}

// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum FitType {
    /// Fully fits in GPU VRAM
    FullGpu,
    /// Partially offloaded to CPU RAM (slower)
    PartialOffload { offload_pct: u8 },
    /// CPU only (no usable GPU, or VRAM insufficient)
    CpuOnly,
    /// Not enough combined memory
    TooBig,
}

#[derive(Debug, Clone)]
pub struct CompatResult {
    pub fit: FitType,
    pub vram_required: u64,
    pub notes: Vec<String>,
}

pub fn check(
    model: &Model,
    quant: Quant,
    hw: &HardwareInfo,
    ctx: u32,
) -> CompatResult {
    let vram_required = runtime_vram(model, quant, ctx);
    let mut notes = vec![];

    // Apple Silicon: unified memory — treat RAM as VRAM, cap at 75%
    let (vram_avail, ram_avail) = compute_memory_pools(hw, &mut notes);

    let fit = if vram_avail >= vram_required {
        FitType::FullGpu
    } else if vram_avail > 0 && vram_avail + ram_avail >= vram_required {
        let offload = ((vram_required - vram_avail) as f64 / vram_required as f64 * 100.0) as u8;
        if !hw.gpus.is_empty() && hw.gpus.iter().any(|g| g.shared_memory) {
            notes.push("Uses shared system memory".into());
        } else {
            notes.push(format!("~{}% of layers offloaded to CPU RAM", offload));
        }
        FitType::PartialOffload { offload_pct: offload }
    } else if ram_avail >= vram_required {
        notes.push("CPU only — expect slow inference".into());
        FitType::CpuOnly
    } else {
        notes.push("Not enough memory (VRAM + RAM)".into());
        FitType::TooBig
    };

    // Warn if context exceeds model's native max
    if ctx > model.context_length {
        notes.push(format!(
            "Requested ctx {} > model max {}; runtime may truncate",
            ctx, model.context_length
        ));
    }

    CompatResult {
        fit,
        vram_required,
        notes,
    }
}

fn compute_memory_pools(hw: &HardwareInfo, notes: &mut Vec<String>) -> (u64, u64) {
    // Usable RAM = 80% of total (leave headroom for OS)
    let usable_ram = (hw.ram_bytes as f64 * 0.80) as u64;

    if hw.gpus.is_empty() {
        return (0, usable_ram);
    }

    // Check for Apple Silicon (unified memory)
    if hw.gpus.iter().any(|g| g.vendor == Vendor::Apple && g.shared_memory) {
        // Apple: GPU can use ~75% of unified memory
        let apple_gpu_bytes = (hw.ram_bytes as f64 * 0.75) as u64;
        return (apple_gpu_bytes, usable_ram);
    }

    // Dedicated GPU(s): sum VRAM, exclude shared-memory GPUs when dedicated exist
    let dedicated: Vec<&GpuInfo> = hw
        .gpus
        .iter()
        .filter(|g| !g.shared_memory)
        .collect();

    if dedicated.is_empty() {
        // Only shared-memory GPUs (e.g. Intel iGPU)
        let best_vram = hw.gpus.iter().map(|g| g.vram_bytes).max().unwrap_or(0);
        return (best_vram, usable_ram);
    }

    let total_vram: u64 = dedicated.iter().map(|g| g.vram_bytes).sum();

    if dedicated.len() > 1 {
        // Multi-GPU: apply 5% overhead + 90% utilization (conservative)
        let overhead = dedicated.len() as u64 * 300 * MIB;
        let effective = ((total_vram.saturating_sub(overhead)) as f64 * 0.90) as u64;
        notes.push(format!(
            "Multi-GPU: {}×GPU, {:.1} GB effective",
            dedicated.len(),
            effective as f64 / GIB as f64
        ));
        return (effective, usable_ram);
    }

    // Single dedicated GPU — usable = 95% of VRAM (leave room for desktop/OS)
    let usable_vram = (total_vram as f64 * 0.95) as u64;
    (usable_vram, usable_ram)
}

// ─────────────────────────────────────────────────────────────────────────────

/// For a given model, find the best (highest quality) quant that fits fully in GPU.
/// Falls back to partial offload, then CPU, then None.
pub fn best_quant(model: &Model, hw: &HardwareInfo, ctx: u32) -> Option<(Quant, CompatResult)> {
    if let Some(q) = model.fixed_quant {
        let result = check(model, q, hw, ctx);
        return if matches!(result.fit, FitType::TooBig) {
            None
        } else {
            Some((q, result))
        };
    }

    // Try from highest quality down to lowest
    let quants = [
        Quant::F16,
        Quant::Q8_0,
        Quant::Q6K,
        Quant::Q5KM,
        Quant::Q4KM,
        Quant::Q3KM,
        Quant::Q2K,
    ];

    let mut best_partial: Option<(Quant, CompatResult)> = None;
    let mut best_cpu: Option<(Quant, CompatResult)> = None;

    for &q in &quants {
        let result = check(model, q, hw, ctx);
        match &result.fit {
            FitType::FullGpu => return Some((q, result)),
            FitType::PartialOffload { .. } => {
                if best_partial.is_none() {
                    best_partial = Some((q, result));
                }
            }
            FitType::CpuOnly => {
                if best_cpu.is_none() {
                    best_cpu = Some((q, result));
                }
            }
            FitType::TooBig => {}
        }
    }

    best_partial.or(best_cpu)
}

pub fn evaluate<'a>(
    model: &'a Model,
    hw: &HardwareInfo,
    ctx: u32,
    quant_filter: Option<Quant>,
) -> (
    &'a Model,
    Vec<(Quant, CompatResult)>,
    Option<(Quant, CompatResult)>,
) {
    let all_quants = if let Some(q) = model.fixed_quant {
        vec![(q, check(model, q, hw, ctx))]
    } else {
        Quant::all()
            .iter()
            .map(|&q| (q, check(model, q, hw, ctx)))
            .collect()
    };

    let best = if let Some(q) = quant_filter {
        Some((q, check(model, q, hw, ctx)))
    } else {
        best_quant(model, hw, ctx)
    };

    (model, all_quants, best)
}
