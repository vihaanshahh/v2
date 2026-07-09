//! Memory-bandwidth model and decode-throughput (tok/s) estimation.
//!
//! LLM token generation is memory-bandwidth bound: to emit one token the engine
//! streams every active weight (and the KV cache) through the memory bus once.
//! So:  tok/s  ≈  effective_bandwidth  /  bytes_moved_per_token.
//!
//! This module is pure (no I/O, no network) and is always compiled — it makes the
//! plain `v2` scan more useful and feeds the mesh scheduler's node cards.

use crate::engine::FitType;
use crate::hardware::{GpuInfo, HardwareInfo, Vendor};
use crate::models::{Model, Quant};

const GB: f64 = 1_000_000_000.0;
const MIB: f64 = 1024.0 * 1024.0;

/// Fraction of theoretical bandwidth a real decode loop achieves.
const GPU_EFFICIENCY: f64 = 0.80;
const CPU_EFFICIENCY: f64 = 0.55;

/// Same empirical KV constant as engine.rs (FP16 KV, MB per Bparam per Kctx).
const KV_MB_PER_BPARAM_PER_KCTX: f64 = 3.5;

/// Peak memory bandwidth in GB/s for a GPU / SoC, plus whether it was an exact
/// table hit (`false` = vendor-class fallback, shown with a `~`).
pub fn gpu_bandwidth_gbps(gpu: &GpuInfo) -> (f64, bool) {
    let n = gpu.name.to_lowercase();

    // Order matters: match the more specific variant (max/ultra/pro/ti) first.
    let table: &[(&str, f64)] = &[
        // Apple Silicon (unified memory bandwidth)
        ("m1 ultra", 800.0), ("m1 max", 400.0), ("m1 pro", 200.0), ("m1", 68.0),
        ("m2 ultra", 800.0), ("m2 max", 400.0), ("m2 pro", 200.0), ("m2", 100.0),
        ("m3 ultra", 800.0), ("m3 max", 400.0), ("m3 pro", 150.0), ("m3", 100.0),
        ("m4 max", 546.0), ("m4 pro", 273.0), ("m4", 120.0),
        // NVIDIA data-center
        ("h100", 3350.0), ("h200", 4800.0), ("a100", 1935.0), ("a6000", 768.0),
        ("a5000", 768.0), ("a4000", 448.0), ("l40", 864.0), ("l4", 300.0), ("t4", 320.0),
        // NVIDIA GeForce
        ("5090", 1792.0), ("5080", 960.0), ("4090", 1008.0), ("4080", 717.0),
        ("4070 ti", 504.0), ("4070", 504.0), ("4060 ti", 288.0), ("4060", 272.0),
        ("3090 ti", 1008.0), ("3090", 936.0), ("3080 ti", 912.0), ("3080", 760.0),
        ("3070", 448.0), ("3060 ti", 448.0), ("3060", 360.0),
        ("2080 ti", 616.0), ("2080", 448.0), ("2070", 448.0),
        // AMD
        ("mi300x", 5300.0), ("mi250", 3277.0), ("mi210", 1638.0),
        ("7900 xtx", 960.0), ("7900 xt", 800.0), ("7800 xt", 624.0),
        ("6950 xt", 576.0), ("6900 xt", 512.0), ("6800 xt", 512.0),
        // AMD discrete GPUs found in Intel Macs
        ("radeon pro 5600m", 394.0), ("radeon pro 5500m", 192.0), ("radeon pro 5300m", 192.0),
        ("radeon pro vega 20", 164.0), ("radeon pro vega 16", 164.0),
        ("radeon pro 560x", 96.0), ("radeon pro 555x", 96.0), ("radeon pro 580", 256.0),
        // Intel Arc
        ("a770", 560.0), ("a750", 512.0), ("a580", 512.0), ("a380", 186.0),
    ];

    for (needle, gbps) in table {
        if n.contains(needle) {
            return (*gbps, true);
        }
    }

    // Vendor-class fallback — deliberately conservative.
    let fallback = match gpu.vendor {
        Vendor::Apple => 100.0,
        Vendor::Nvidia => 400.0,
        Vendor::Amd => 400.0,
        Vendor::Intel => 250.0,
        Vendor::Unknown => 200.0,
    };
    (fallback, false)
}

/// System RAM bandwidth in GB/s. Used for CPU-only / offloaded layers.
/// Apple's unified memory means CPU sees the same high bandwidth as the GPU.
pub fn system_ram_bandwidth_gbps(hw: &HardwareInfo) -> f64 {
    if let Some(g) = hw.gpus.iter().find(|g| g.vendor == Vendor::Apple && g.shared_memory) {
        return gpu_bandwidth_gbps(g).0;
    }
    // Typical dual-channel desktop: DDR4 ~48, DDR5 ~64. Conservative middle.
    50.0
}

/// Bytes streamed through memory to generate one token: active weights at this
/// quant, plus a full read of the KV cache at this context length.
fn bytes_per_token(model: &Model, quant: Quant, ctx: u32) -> f64 {
    let active_params = if model.is_moe {
        model.params_active.unwrap_or(model.params)
    } else {
        model.params
    };
    let weight = active_params as f64 * quant.bytes_per_weight();

    let active_b = active_params as f64 / GB;
    let ctx_k = ctx as f64 / 1024.0;
    let kv = active_b * ctx_k * KV_MB_PER_BPARAM_PER_KCTX * MIB;

    weight + kv
}

/// Estimated decode throughput (tokens/sec) for a model at a quant on this
/// hardware, given how it fits. `None` when it cannot run. The bool is
/// `is_rough` — true when the GPU bandwidth was a vendor-class guess.
pub fn estimate_tps(
    model: &Model,
    quant: Quant,
    ctx: u32,
    hw: &HardwareInfo,
    fit: &FitType,
) -> Option<(f64, bool)> {
    let bpt = bytes_per_token(model, quant, ctx);
    if bpt <= 0.0 {
        return None;
    }

    let ram_bw = system_ram_bandwidth_gbps(hw) * GB;
    let (gpu_gbps, exact) = hw
        .gpus
        .first()
        .map(gpu_bandwidth_gbps)
        .unwrap_or((0.0, false));
    let gpu_bw = gpu_gbps * GB;

    let eff_bw = match fit {
        FitType::FullGpu => gpu_bw * GPU_EFFICIENCY,
        FitType::PartialOffload { offload_pct } => {
            // Throughput is gated by the slowest tier; model as time-weighted.
            let off = (*offload_pct as f64 / 100.0).clamp(0.0, 1.0);
            let on = 1.0 - off;
            let t_gpu = if gpu_bw > 0.0 { on / (gpu_bw * GPU_EFFICIENCY) } else { f64::INFINITY };
            let t_cpu = off / (ram_bw * CPU_EFFICIENCY);
            if (t_gpu + t_cpu) <= 0.0 { return None; }
            1.0 / (t_gpu + t_cpu)
        }
        FitType::CpuOnly => ram_bw * CPU_EFFICIENCY,
        FitType::TooBig => return None,
    };

    if eff_bw <= 0.0 {
        return None;
    }

    let rough = match fit {
        FitType::FullGpu => !exact,
        FitType::PartialOffload { .. } | FitType::CpuOnly => true,
        FitType::TooBig => return None,
    };
    Some((eff_bw / bpt, rough))
}

/// Compact human label, e.g. "48 tok/s" or "~6 tok/s" for rough estimates.
pub fn tps_label(tps: f64, is_rough: bool) -> String {
    let prefix = if is_rough { "~" } else { "" };
    if tps >= 100.0 {
        format!("{prefix}{:.0} tok/s", tps)
    } else if tps >= 10.0 {
        format!("{prefix}{:.0} tok/s", tps)
    } else {
        format!("{prefix}{:.1} tok/s", tps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelOrigin;

    fn dense(params: u64) -> Model {
        Model {
            name: "t".into(), family: "t".into(), params, params_active: None,
            is_moe: false, context_length: 8192, id: "t".into(), ollama_name: None,
            weight_bytes: None, fixed_quant: None, origin: ModelOrigin::Catalog,
        }
    }

    #[test]
    fn exact_table_hits() {
        let g = GpuInfo { name: "NVIDIA GeForce RTX 4090".into(), vendor: Vendor::Nvidia, vram_bytes: 0, shared_memory: false };
        assert_eq!(gpu_bandwidth_gbps(&g), (1008.0, true));
        let m = GpuInfo { name: "Apple M2 Max".into(), vendor: Vendor::Apple, vram_bytes: 0, shared_memory: true };
        assert_eq!(gpu_bandwidth_gbps(&m).0, 400.0);
        // Mac discrete AMD GPU resolves exactly, not the generic AMD fallback.
        let amd = GpuInfo { name: "AMD Radeon Pro 5300M".into(), vendor: Vendor::Amd, vram_bytes: 4 << 30, shared_memory: false };
        assert_eq!(gpu_bandwidth_gbps(&amd), (192.0, true));
    }

    #[test]
    fn specific_variant_wins_over_bare() {
        let g = GpuInfo { name: "Apple M3 Ultra".into(), vendor: Vendor::Apple, vram_bytes: 0, shared_memory: true };
        assert_eq!(gpu_bandwidth_gbps(&g).0, 800.0);
    }

    #[test]
    fn bigger_model_is_slower() {
        let hw = HardwareInfo {
            gpus: vec![GpuInfo { name: "RTX 4090".into(), vendor: Vendor::Nvidia, vram_bytes: 24 << 30, shared_memory: false }],
            cpu_name: "x".into(), ram_bytes: 64 << 30, os: crate::hardware::Os::Linux,
        };
        let small = estimate_tps(&dense(8_000_000_000), Quant::Q4KM, 4096, &hw, &FitType::FullGpu).unwrap().0;
        let big = estimate_tps(&dense(70_000_000_000), Quant::Q4KM, 4096, &hw, &FitType::FullGpu).unwrap().0;
        assert!(small > big, "8B ({small}) should decode faster than 70B ({big})");
        assert!(small > 20.0 && small < 400.0, "8B on 4090 sanity: {small}");
    }

    #[test]
    fn cpu_only_is_much_slower_than_gpu() {
        let hw = HardwareInfo {
            gpus: vec![GpuInfo { name: "RTX 4090".into(), vendor: Vendor::Nvidia, vram_bytes: 24 << 30, shared_memory: false }],
            cpu_name: "x".into(), ram_bytes: 64 << 30, os: crate::hardware::Os::Linux,
        };
        let m = dense(8_000_000_000);
        let gpu = estimate_tps(&m, Quant::Q4KM, 4096, &hw, &FitType::FullGpu).unwrap().0;
        let cpu = estimate_tps(&m, Quant::Q4KM, 4096, &hw, &FitType::CpuOnly).unwrap().0;
        assert!(gpu > cpu * 3.0, "gpu {gpu} should dwarf cpu {cpu}");
    }
}
