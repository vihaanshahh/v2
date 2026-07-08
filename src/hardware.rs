use std::process::Command;

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: Vendor,
    pub vram_bytes: u64,
    pub shared_memory: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Apple,
    Intel,
    Unknown,
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Vendor::Nvidia => write!(f, "NVIDIA"),
            Vendor::Amd => write!(f, "AMD"),
            Vendor::Apple => write!(f, "Apple"),
            Vendor::Intel => write!(f, "Intel"),
            Vendor::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug)]
pub struct HardwareInfo {
    pub gpus: Vec<GpuInfo>,
    pub cpu_name: String,
    pub ram_bytes: u64,
    pub os: Os,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl std::fmt::Display for Os {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Os::Linux => write!(f, "Linux"),
            Os::MacOs => write!(f, "macOS"),
            Os::Windows => write!(f, "Windows"),
        }
    }
}

pub fn detect() -> HardwareInfo {
    let os = detect_os();
    let mut gpus = vec![];

    gpus.extend(detect_nvidia());
    match os {
        Os::Linux => {
            gpus.extend(detect_amd_linux());
            if gpus.is_empty() {
                gpus.extend(detect_apple_linux());
            }
        }
        Os::MacOs => {
            if gpus.is_empty() {
                gpus.extend(detect_macos());
            }
        }
        Os::Windows => {
            if gpus.is_empty() {
                gpus.extend(detect_windows_gpus());
            }
        }
    }

    // Drop any adapter that reported a blank name (seen from flaky wmic/driver
    // output) so it can't show up as a nameless zero-VRAM GPU.
    gpus.retain(|g| !g.name.trim().is_empty());

    HardwareInfo {
        cpu_name: detect_cpu_name(&os),
        ram_bytes: detect_ram(&os),
        gpus,
        os,
    }
}

fn detect_os() -> Os {
    match std::env::consts::OS {
        "macos" => Os::MacOs,
        "windows" => Os::Windows,
        _ => Os::Linux,
    }
}

// ── NVIDIA ──────────────────────────────────────────────────────────────────

fn detect_nvidia() -> Vec<GpuInfo> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
        .output();

    let Ok(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ',');
            let name = parts.next()?.trim().to_string();
            let mib: u64 = parts.next()?.trim().parse().ok()?;
            Some(GpuInfo {
                name,
                vendor: Vendor::Nvidia,
                vram_bytes: mib * 1024 * 1024,
                shared_memory: false,
            })
        })
        .collect()
}

// ── AMD (Linux) ──────────────────────────────────────────────────────────────

fn detect_amd_linux() -> Vec<GpuInfo> {
    let mut gpus = vec![];
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return gpus;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name_str = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if !name_str.starts_with("card") || name_str.contains('-') {
            continue;
        }

        let vendor_path = path.join("device/vendor");
        let vendor_str = std::fs::read_to_string(&vendor_path).unwrap_or_default();
        if !vendor_str.trim().eq_ignore_ascii_case("0x1002") {
            continue;
        }

        let vram_path = path.join("device/mem_info_vram_total");
        let Ok(vram_str) = std::fs::read_to_string(&vram_path) else {
            continue;
        };
        let Ok(vram_bytes) = vram_str.trim().parse::<u64>() else {
            continue;
        };

        let model_path = path.join("device/product_name");
        let gpu_name = std::fs::read_to_string(model_path)
            .unwrap_or_else(|_| "AMD GPU".to_string())
            .trim()
            .to_string();

        gpus.push(GpuInfo {
            name: gpu_name,
            vendor: Vendor::Amd,
            vram_bytes,
            shared_memory: false,
        });
    }
    gpus
}

// ── Apple (macOS) ────────────────────────────────────────────────────────────

/// True on Apple Silicon (M-series), false on Intel Macs. Uses the runtime
/// sysctl rather than the compiled ARCH, so an x86_64 binary under Rosetta on an
/// M-series machine is still correctly identified as Apple Silicon.
fn is_apple_silicon() -> bool {
    if sysctl_string("hw.optional.arm64").as_deref() == Some("1") {
        return true;
    }
    sysctl_string("machdep.cpu.brand_string")
        .map(|s| s.starts_with("Apple "))
        .unwrap_or(false)
}

fn sysctl_string(key: &str) -> Option<String> {
    Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn detect_macos() -> Vec<GpuInfo> {
    if is_apple_silicon() {
        let ram_bytes: u64 = sysctl_string("hw.memsize").and_then(|s| s.parse().ok()).unwrap_or(0);
        if ram_bytes == 0 {
            return vec![];
        }
        let chip = sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "Apple Silicon".into());
        // Unified memory: all of RAM is usable for the GPU; the engine caps the
        // GPU share at ~75% for layer offload.
        return vec![GpuInfo {
            name: chip,
            vendor: Vendor::Apple,
            vram_bytes: ram_bytes,
            shared_memory: true,
        }];
    }

    // Intel Mac: no unified memory. Ask system_profiler for the real GPU(s).
    // A machine with only an Intel iGPU returns nothing usable here, so the
    // engine correctly treats it as CPU-only.
    let out = Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-detailLevel", "mini"])
        .output();
    let text = out
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let mut gpus = parse_macos_gpus(&text);
    // If a discrete GPU exists, that's what Ollama/Metal uses — drop integrated
    // ones so the display and fit math don't over-count an unusable iGPU.
    if gpus.iter().any(|g| !g.shared_memory) {
        gpus.retain(|g| !g.shared_memory);
    }
    gpus
}

/// Parse `system_profiler SPDisplaysDataType` text into GPUs. Discrete cards
/// (`VRAM (Total)`) are dedicated; integrated GPUs (`VRAM (Dynamic, Max)`) are
/// shared. GPUs with no usable VRAM are dropped so the machine reads as CPU-only.
fn parse_macos_gpus(text: &str) -> Vec<GpuInfo> {
    let mut gpus = vec![];
    let mut name: Option<String> = None;
    let mut vram: u64 = 0;
    let mut shared = false;
    let mut vendor_hint = String::new();

    let flush = |gpus: &mut Vec<GpuInfo>, name: &Option<String>, vram: u64, shared: bool, vendor_hint: &str| {
        if let Some(n) = name {
            if vram > 0 {
                let hay = format!("{n} {vendor_hint}").to_lowercase();
                let vendor = if hay.contains("nvidia") || hay.contains("geforce") {
                    Vendor::Nvidia
                } else if hay.contains("amd") || hay.contains("radeon") {
                    Vendor::Amd
                } else if hay.contains("intel") {
                    Vendor::Intel
                } else if hay.contains("apple") {
                    Vendor::Apple
                } else {
                    Vendor::Unknown
                };
                gpus.push(GpuInfo { name: n.clone(), vendor, vram_bytes: vram, shared_memory: shared });
            }
        }
    };

    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("Chipset Model:") {
            // New GPU block — flush the previous one.
            flush(&mut gpus, &name, vram, shared, &vendor_hint);
            name = Some(v.trim().to_string());
            vram = 0;
            shared = false;
            vendor_hint.clear();
        } else if let Some(v) = t.strip_prefix("VRAM (Total):") {
            vram = parse_vram(v);
            shared = false;
        } else if let Some(v) = t.strip_prefix("VRAM (Dynamic, Max):") {
            vram = parse_vram(v);
            shared = true;
        } else if let Some(v) = t.strip_prefix("Vendor:") {
            vendor_hint = v.trim().to_string();
        }
    }
    flush(&mut gpus, &name, vram, shared, &vendor_hint);
    // Dedicated GPUs first, then largest VRAM — so display + bandwidth pick the
    // real discrete card over an integrated one.
    gpus.sort_by(|a, b| {
        a.shared_memory
            .cmp(&b.shared_memory)
            .then(b.vram_bytes.cmp(&a.vram_bytes))
    });
    gpus
}

/// Parse a system_profiler VRAM value like "4 GB", "1536 MB", "1.5 GB".
fn parse_vram(raw: &str) -> u64 {
    let s = raw.trim();
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if !num.is_empty() {
            break;
        }
    }
    let Ok(value) = num.parse::<f64>() else { return 0 };
    let upper = s.to_uppercase();
    let mult = if upper.contains("GB") {
        1024.0 * 1024.0 * 1024.0
    } else if upper.contains("MB") {
        1024.0 * 1024.0
    } else {
        1.0
    };
    (value * mult) as u64
}

fn detect_apple_linux() -> Vec<GpuInfo> {
    // Asahi Linux exposes GPU memory via DRM
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return vec![];
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let vendor_path = path.join("device/vendor");
        let vendor = std::fs::read_to_string(vendor_path).unwrap_or_default();
        if vendor.trim().eq_ignore_ascii_case("0x106b") {
            let vram_path = path.join("device/mem_info_vram_total");
            if let Ok(v) = std::fs::read_to_string(vram_path) {
                if let Ok(bytes) = v.trim().parse::<u64>() {
                    return vec![GpuInfo {
                        name: "Apple GPU (Asahi)".to_string(),
                        vendor: Vendor::Apple,
                        vram_bytes: bytes,
                        shared_memory: true,
                    }];
                }
            }
        }
    }
    vec![]
}

// ── Windows ──────────────────────────────────────────────────────────────────

fn detect_windows_gpus() -> Vec<GpuInfo> {
    let out = Command::new("wmic")
        .args(["path", "Win32_VideoController", "get", "Name,AdapterRAM", "/format:csv"])
        .output();

    let Ok(out) = out else { return vec![] };
    let text = String::from_utf8_lossy(&out.stdout);

    text.lines()
        .skip(1)
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 3 {
                return None;
            }
            let name = parts[2].trim().to_string();
            let vram: u64 = parts[1].trim().parse().ok()?;
            if name.is_empty() || vram == 0 {
                return None;
            }
            let vendor = if name.to_uppercase().contains("NVIDIA") {
                Vendor::Nvidia
            } else if name.to_uppercase().contains("AMD") || name.to_uppercase().contains("RADEON") {
                Vendor::Amd
            } else if name.to_uppercase().contains("INTEL") {
                Vendor::Intel
            } else {
                Vendor::Unknown
            };
            Some(GpuInfo { name, vendor, vram_bytes: vram, shared_memory: false })
        })
        .collect()
}

// ── RAM ──────────────────────────────────────────────────────────────────────

fn detect_ram(os: &Os) -> u64 {
    match os {
        Os::MacOs => {
            Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        }
        Os::Linux => {
            std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("MemTotal:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|kb| kb * 1024)
                })
                .unwrap_or(0)
        }
        Os::Windows => {
            Command::new("wmic")
                .args(["OS", "get", "TotalVisibleMemorySize", "/value"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.contains("TotalVisibleMemorySize"))
                        .and_then(|l| l.split('=').nth(1))
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(|kb| kb * 1024)
                })
                .unwrap_or(0)
        }
    }
}

// ── CPU ──────────────────────────────────────────────────────────────────────

fn detect_cpu_name(os: &Os) -> String {
    match os {
        Os::MacOs => Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "Unknown".to_string()),
        Os::Linux => std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.splitn(2, ':').nth(1))
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "Unknown".to_string()),
        Os::Windows => Command::new("wmic")
            .args(["cpu", "get", "Name", "/value"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                s.lines()
                    .find(|l| l.contains("Name="))
                    .and_then(|l| l.split('=').nth(1))
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "Unknown".to_string()),
    }
}

// ── Disk ──────────────────────────────────────────────────────────────────────

/// Best-effort free bytes on the filesystem that holds `path` (where Ollama
/// stores model weights). Shells out like the rest of detection; returns `None`
/// if the platform tool is missing or its output can't be parsed, so callers
/// degrade gracefully to "disk space unknown".
pub fn disk_free_bytes(path: &std::path::Path) -> Option<u64> {
    let p = path.to_string_lossy();
    match detect_os() {
        Os::Linux | Os::MacOs => {
            // POSIX `df -Pk` -> "Filesystem 1024-blocks Used Available Capacity Mounted".
            let out = Command::new("df").args(["-Pk", &p]).output().ok()?;
            let text = String::from_utf8_lossy(&out.stdout);
            let data = text.lines().nth(1)?; // skip the header row
            let avail_kb: u64 = data.split_whitespace().nth(3)?.parse().ok()?;
            Some(avail_kb * 1024)
        }
        Os::Windows => {
            // Drive letter of the path (e.g. "C:"), default to C: if absent.
            let drive = p.split(':').next().map(|d| format!("{d}:")).unwrap_or_else(|| "C:".into());
            let out = Command::new("wmic")
                .args(["logicaldisk", "where", &format!("DeviceID='{drive}'"), "get", "FreeSpace", "/value"])
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .find(|l| l.contains("FreeSpace="))
                .and_then(|l| l.split('=').nth(1))
                .and_then(|v| v.trim().parse::<u64>().ok())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output shape from a 2019 Intel MacBook Pro (i7-9750H).
    const INTEL_MBP: &str = "Graphics/Displays:

    Intel UHD Graphics 630:

      Chipset Model: Intel UHD Graphics 630
      Type: GPU
      Bus: Built-In
      VRAM (Dynamic, Max): 1536 MB
      Vendor: Intel

    AMD Radeon Pro 5300M:

      Chipset Model: AMD Radeon Pro 5300M
      Type: GPU
      Bus: PCIe
      VRAM (Total): 4 GB
      Vendor: AMD (0x1002)
";

    #[test]
    fn parses_intel_mac_discrete_and_igpu() {
        let gpus = parse_macos_gpus(INTEL_MBP);
        assert_eq!(gpus.len(), 2);
        // Dedicated AMD card must sort first (drives display + bandwidth).
        assert_eq!(gpus[0].vendor, Vendor::Amd);
        assert!(!gpus[0].shared_memory);
        assert_eq!(gpus[0].vram_bytes, 4 * 1024 * 1024 * 1024);
        // Intel iGPU is shared and second.
        assert_eq!(gpus[1].vendor, Vendor::Intel);
        assert!(gpus[1].shared_memory);
        assert_eq!(gpus[1].vram_bytes, 1536 * 1024 * 1024);
    }

    #[test]
    fn igpu_only_mac_reports_shared_not_dedicated() {
        let text = "    Intel Iris Plus Graphics:
      Chipset Model: Intel Iris Plus Graphics
      VRAM (Dynamic, Max): 1536 MB
      Vendor: Intel
";
        let gpus = parse_macos_gpus(text);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].shared_memory, "iGPU must be shared, never treated as VRAM");
    }

    #[test]
    fn gpu_with_no_vram_is_dropped() {
        let text = "      Chipset Model: Some Display Adapter
      Vendor: Unknown
";
        assert!(parse_macos_gpus(text).is_empty());
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(parse_macos_gpus("").is_empty());
    }

    #[test]
    fn parse_vram_units() {
        assert_eq!(parse_vram("4 GB"), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_vram("1536 MB"), 1536 * 1024 * 1024);
        assert_eq!(parse_vram("1.5 GB"), (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_vram("garbage"), 0);
    }
}
