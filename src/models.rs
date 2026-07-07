/// Curated catalog of popular open-weight models.
/// Each entry stores enough to estimate VRAM at any quantization level.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOrigin {
    Catalog,
    OllamaLocal,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub name: String,
    pub family: String,
    pub params: u64,
    pub params_active: Option<u64>,
    pub is_moe: bool,
    pub context_length: u32,
    pub id: String,
    /// Ollama pull name, e.g. qwen3:8b
    pub ollama_name: Option<String>,
    /// Known on-disk / loaded weight bytes (from Ollama)
    pub weight_bytes: Option<u64>,
    /// Quantization baked into the Ollama model
    pub fixed_quant: Option<Quant>,
    pub origin: ModelOrigin,
}

impl Model {
    pub fn display_name(&self) -> &str {
        match self.origin {
            ModelOrigin::OllamaLocal => self.ollama_name.as_deref().unwrap_or(&self.name),
            ModelOrigin::Catalog => &self.name,
        }
    }

    pub fn match_keys(&self) -> Vec<String> {
        let mut keys = vec![self.name.to_lowercase(), self.id.to_lowercase()];
        if let Some(o) = &self.ollama_name {
            keys.push(o.to_lowercase());
            keys.push(o.split(':').next().unwrap_or(o).to_lowercase());
        }
        keys
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quant {
    Q2K,
    Q3KM,
    Q4KM,
    Q5KM,
    Q6K,
    Q8_0,
    F16,
}

impl Quant {
    pub fn all() -> &'static [Quant] {
        &[
            Quant::Q2K,
            Quant::Q3KM,
            Quant::Q4KM,
            Quant::Q5KM,
            Quant::Q6K,
            Quant::Q8_0,
            Quant::F16,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Quant::Q2K => "Q2_K",
            Quant::Q3KM => "Q3_K_M",
            Quant::Q4KM => "Q4_K_M",
            Quant::Q5KM => "Q5_K_M",
            Quant::Q6K => "Q6_K",
            Quant::Q8_0 => "Q8_0",
            Quant::F16 => "F16",
        }
    }

    pub fn from_label(s: &str) -> Option<Quant> {
        match s.to_uppercase().as_str() {
            "Q2_K" | "Q2K" => Some(Quant::Q2K),
            "Q3_K_M" | "Q3_K_S" | "Q3KS" | "Q3KM" => Some(Quant::Q3KM),
            "Q4_K_M" | "Q4_K_S" | "Q4_0" | "Q4KM" => Some(Quant::Q4KM),
            "Q5_K_M" | "Q5_K_S" | "Q5_0" | "Q5KM" => Some(Quant::Q5KM),
            "Q6_K" | "Q6K" => Some(Quant::Q6K),
            "Q8_0" | "Q8" => Some(Quant::Q8_0),
            "F16" | "FP16" => Some(Quant::F16),
            _ => None,
        }
    }

    pub fn bytes_per_weight(&self) -> f64 {
        match self {
            Quant::Q2K => 0.3125,
            Quant::Q3KM => 0.4375,
            Quant::Q4KM => 0.5625,
            Quant::Q5KM => 0.6875,
            Quant::Q6K => 0.75,
            Quant::Q8_0 => 1.0,
            Quant::F16 => 2.0,
        }
    }
}

impl std::fmt::Display for Quant {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

pub fn parse_param_size(raw: &str) -> Option<u64> {
    let s = raw.trim().to_uppercase();
    if s.is_empty() {
        return None;
    }
    let s = s.trim_end_matches('B');
    if let Some(num) = s.strip_suffix('M') {
        return num.trim().parse::<f64>().ok().map(|n| (n * 1e6) as u64);
    }
    num_suffix(s.strip_suffix('K')?.trim(), 1e3).or_else(|| {
        let n: f64 = s.parse().ok()?;
        Some((n * 1e9) as u64)
    })
}

fn num_suffix(raw: &str, scale: f64) -> Option<u64> {
    raw.parse::<f64>().ok().map(|n| (n * scale) as u64)
}

fn ce(
    name: &str,
    family: &str,
    params: u64,
    ctx: u32,
    id: &str,
    ollama: Option<&str>,
) -> Model {
    Model {
        name: name.to_string(),
        family: family.to_string(),
        params,
        params_active: None,
        is_moe: false,
        context_length: ctx,
        id: id.to_string(),
        ollama_name: ollama.map(str::to_string),
        weight_bytes: None,
        fixed_quant: None,
        origin: ModelOrigin::Catalog,
    }
}

fn ce_moe(
    name: &str,
    family: &str,
    params: u64,
    active: u64,
    ctx: u32,
    id: &str,
    ollama: Option<&str>,
) -> Model {
    Model {
        name: name.to_string(),
        family: family.to_string(),
        params,
        params_active: Some(active),
        is_moe: true,
        context_length: ctx,
        id: id.to_string(),
        ollama_name: ollama.map(str::to_string),
        weight_bytes: None,
        fixed_quant: None,
        origin: ModelOrigin::Catalog,
    }
}

pub fn catalog() -> Vec<Model> {
    vec![
        ce("Qwen3 0.6B", "Qwen3", 600_000_000, 32768, "Qwen/Qwen3-0.6B", Some("qwen3:0.6b")),
        ce("Qwen3 1.7B", "Qwen3", 1_700_000_000, 32768, "Qwen/Qwen3-1.7B", Some("qwen3:1.7b")),
        ce("Qwen3 4B", "Qwen3", 4_000_000_000, 32768, "Qwen/Qwen3-4B", Some("qwen3:4b")),
        ce("Qwen3 8B", "Qwen3", 8_000_000_000, 128000, "Qwen/Qwen3-8B", Some("qwen3:8b")),
        ce("Qwen3 14B", "Qwen3", 14_000_000_000, 128000, "Qwen/Qwen3-14B", Some("qwen3:14b")),
        ce("Qwen3 32B", "Qwen3", 32_000_000_000, 128000, "Qwen/Qwen3-32B", Some("qwen3:32b")),
        ce_moe(
            "Qwen3 30B A3B",
            "Qwen3",
            30_000_000_000,
            3_000_000_000,
            128000,
            "Qwen/Qwen3-30B-A3B",
            Some("qwen3:30b-a3b"),
        ),
        ce_moe(
            "Qwen3 235B A22B",
            "Qwen3",
            235_000_000_000,
            22_000_000_000,
            128000,
            "Qwen/Qwen3-235B-A22B",
            Some("qwen3:235b-a22b"),
        ),
        ce(
            "Llama 3.2 1B",
            "Llama",
            1_235_000_000,
            131072,
            "meta-llama/Llama-3.2-1B",
            Some("llama3.2:1b"),
        ),
        ce(
            "Llama 3.2 3B",
            "Llama",
            3_210_000_000,
            131072,
            "meta-llama/Llama-3.2-3B",
            Some("llama3.2:3b"),
        ),
        ce(
            "Llama 3.1 8B",
            "Llama",
            8_030_000_000,
            131072,
            "meta-llama/Llama-3.1-8B",
            Some("llama3.1:8b"),
        ),
        ce(
            "Llama 3.3 70B",
            "Llama",
            70_600_000_000,
            131072,
            "meta-llama/Llama-3.3-70B-Instruct",
            Some("llama3.3:70b"),
        ),
        ce(
            "Llama 3.1 405B",
            "Llama",
            405_000_000_000,
            131072,
            "meta-llama/Llama-3.1-405B",
            Some("llama3.1:405b"),
        ),
        ce(
            "Mistral 7B",
            "Mistral",
            7_240_000_000,
            32768,
            "mistralai/Mistral-7B-Instruct-v0.3",
            Some("mistral:7b"),
        ),
        ce(
            "Mistral Nemo 12B",
            "Mistral",
            12_000_000_000,
            131072,
            "mistralai/Mistral-Nemo-Instruct-2407",
            Some("mistral-nemo:12b"),
        ),
        ce_moe(
            "Mixtral 8x7B",
            "Mistral",
            46_700_000_000,
            12_900_000_000,
            32768,
            "mistralai/Mixtral-8x7B-Instruct-v0.1",
            Some("mixtral:8x7b"),
        ),
        ce_moe(
            "Mixtral 8x22B",
            "Mistral",
            141_000_000_000,
            39_100_000_000,
            65536,
            "mistralai/Mixtral-8x22B-Instruct-v0.1",
            Some("mixtral:8x22b"),
        ),
        ce("Gemma 3 1B", "Gemma", 1_000_000_000, 32768, "google/gemma-3-1b-it", Some("gemma3:1b")),
        ce("Gemma 3 4B", "Gemma", 4_300_000_000, 131072, "google/gemma-3-4b-it", Some("gemma3:4b")),
        ce(
            "Gemma 3 12B",
            "Gemma",
            12_000_000_000,
            131072,
            "google/gemma-3-12b-it",
            Some("gemma3:12b"),
        ),
        ce(
            "Gemma 3 27B",
            "Gemma",
            27_000_000_000,
            131072,
            "google/gemma-3-27b-it",
            Some("gemma3:27b"),
        ),
        ce("Phi-4 14B", "Phi", 14_700_000_000, 16384, "microsoft/phi-4", Some("phi4:14b")),
        ce(
            "Phi-4 Mini 3.8B",
            "Phi",
            3_800_000_000,
            128000,
            "microsoft/Phi-4-mini-instruct",
            Some("phi4-mini:3.8b"),
        ),
        ce(
            "DeepSeek R1 1.5B",
            "DeepSeek",
            1_500_000_000,
            65536,
            "deepseek-ai/DeepSeek-R1-Distill-Qwen-1.5B",
            Some("deepseek-r1:1.5b"),
        ),
        ce(
            "DeepSeek R1 7B",
            "DeepSeek",
            7_000_000_000,
            65536,
            "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B",
            Some("deepseek-r1:7b"),
        ),
        ce(
            "DeepSeek R1 14B",
            "DeepSeek",
            14_000_000_000,
            65536,
            "deepseek-ai/DeepSeek-R1-Distill-Qwen-14B",
            Some("deepseek-r1:14b"),
        ),
        ce(
            "DeepSeek R1 32B",
            "DeepSeek",
            32_000_000_000,
            65536,
            "deepseek-ai/DeepSeek-R1-Distill-Qwen-32B",
            Some("deepseek-r1:32b"),
        ),
        ce(
            "DeepSeek R1 70B",
            "DeepSeek",
            70_600_000_000,
            65536,
            "deepseek-ai/DeepSeek-R1-Distill-Llama-70B",
            Some("deepseek-r1:70b"),
        ),
        ce(
            "SmolLM2 135M",
            "SmolLM",
            135_000_000,
            8192,
            "HuggingFaceTB/SmolLM2-135M-Instruct",
            None,
        ),
        ce(
            "SmolLM2 360M",
            "SmolLM",
            360_000_000,
            8192,
            "HuggingFaceTB/SmolLM2-360M-Instruct",
            None,
        ),
        ce(
            "SmolLM2 1.7B",
            "SmolLM",
            1_700_000_000,
            8192,
            "HuggingFaceTB/SmolLM2-1.7B-Instruct",
            None,
        ),
    ]
}

pub fn catalog_by_ollama_tag() -> std::collections::HashMap<String, Model> {
    let mut map = std::collections::HashMap::new();
    for m in catalog() {
        if let Some(tag) = &m.ollama_name {
            map.insert(tag.to_lowercase(), m.clone());
            if let Some(base) = tag.split(':').next() {
                map.entry(base.to_lowercase()).or_insert_with(|| m.clone());
            }
        }
    }
    map
}
