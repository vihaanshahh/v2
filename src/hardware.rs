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
                gpus.extend(detect_apple_macos());
            }
        }
        Os::Windows => {
            if gpus.is_empty() {
                gpus.extend(detect_windows_gpus());
            }
        }
    }

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

fn detect_apple_macos() -> Vec<GpuInfo> {
    // Get unified memory size from sysctl
    let mem_out = Command::new("sysctl")
        .arg("-n")
        .arg("hw.memsize")
        .output();

    let ram_bytes: u64 = mem_out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if ram_bytes == 0 {
        return vec![];
    }

    // Get chip name from system_profiler
    let sp_out = Command::new("sysctl")
        .arg("-n")
        .arg("machdep.cpu.brand_string")
        .output();

    let chip = sp_out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Apple Silicon".to_string());

    // Apple Silicon uses unified memory — all of RAM is usable for GPU
    // but llama.cpp typically caps at ~75% for GPU layers
    vec![GpuInfo {
        name: chip,
        vendor: Vendor::Apple,
        vram_bytes: ram_bytes,
        shared_memory: true,
    }]
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
