//! Discovery: node cards and the known-peers list.
//!
//! Discovery here is **pull-based and advisory** (DESIGN.md H4): a node fetches
//! a peer's card on demand over the authenticated channel. Cards influence
//! scheduling only — every request is still re-checked by the admission gate, so
//! a stale card can never cause unsafe execution, only a suboptimal choice. A
//! future push-gossip layer can replace this without touching the safety model.

use serde::{Deserialize, Serialize};

use crate::bandwidth;
use crate::hardware::HardwareInfo;
use crate::paths;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A node's self-description, exchanged for scheduling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeCard {
    pub node_pub: String,
    pub hostname: String,
    pub os: String,
    pub gpu: String,
    pub vram_gb: f64,
    pub bandwidth_gbps: f64,
    /// Installed Ollama model tags.
    pub models: Vec<String>,
    /// Hosted endpoints this node can broker into the mesh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_models: Vec<RemoteModel>,
    /// Current remote jobs / configured ceiling.
    pub concurrent: u32,
    pub max_concurrent: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteModel {
    /// Friendly endpoint name chosen by the owner.
    pub name: String,
    /// Provider model id the endpoint expects.
    pub model: String,
    /// API shape, e.g. openai or ollama.
    pub kind: String,
    /// Compact host label for display.
    pub host: String,
}

impl NodeCard {
    pub fn serves_model(&self, model: &str) -> bool {
        let m = model.to_lowercase();
        self.models.iter().any(|t| {
            let t = t.to_lowercase();
            t == m || t.starts_with(&format!("{m}:")) || m.starts_with(&t)
        }) || self.remote_models.iter().any(|r| r.serves_model(model))
    }

    pub fn has_capacity(&self) -> bool {
        self.max_concurrent == 0 || self.concurrent < self.max_concurrent
    }
}

impl RemoteModel {
    pub fn serves_model(&self, model: &str) -> bool {
        let model = model.trim();
        self.model == model || self.name == model || self.name.eq_ignore_ascii_case(model)
    }
}

/// Build this machine's card. `installed` are the local Ollama tags (best-effort).
pub fn local_card(node_pub: &str, hw: &HardwareInfo, installed: &[String], concurrent: u32, max_concurrent: u32) -> NodeCard {
    let (gpu_name, vram_gb, bw) = hw
        .gpus
        .first()
        .map(|g| {
            let (bw, _) = bandwidth::gpu_bandwidth_gbps(g);
            (g.name.clone(), g.vram_bytes as f64 / GIB, bw)
        })
        .unwrap_or_else(|| ("cpu".into(), 0.0, bandwidth::system_ram_bandwidth_gbps(hw)));

    NodeCard {
        node_pub: node_pub.to_string(),
        hostname: hostname(),
        os: hw.os.to_string(),
        gpu: gpu_name,
        vram_gb: (vram_gb * 10.0).round() / 10.0,
        bandwidth_gbps: bw,
        models: installed.to_vec(),
        remote_models: Vec::new(),
        concurrent,
        max_concurrent,
    }
}

fn hostname() -> String {
    // Real system hostname, cross-platform.
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let h = s.trim().split('.').next().unwrap_or("").to_string();
                if !h.is_empty() {
                    return h;
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".into())
}

// ── Known peers ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub addr: String,
    #[serde(default)]
    pub node_pub: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeersFile {
    #[serde(default)]
    pub peers: Vec<PeerEntry>,
}

impl PeersFile {
    pub fn load() -> Self {
        let Ok(dir) = paths::subdir("mesh") else { return Self::default() };
        let Ok(raw) = std::fs::read_to_string(dir.join("peers.json")) else { return Self::default() };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        paths::write_private(&dir.join("peers.json"), raw.as_bytes()).map_err(|e| e.to_string())
    }

    pub fn add(&mut self, addr: &str) {
        if !self.peers.iter().any(|p| p.addr == addr) {
            self.peers.push(PeerEntry { addr: addr.to_string(), node_pub: None });
        }
    }

    pub fn add_with_node(&mut self, addr: &str, node_pub: &str) {
        if let Some(p) = self.peers.iter_mut().find(|p| p.addr == addr) {
            if p.node_pub.is_none() {
                p.node_pub = Some(node_pub.to_string());
            }
        } else {
            self.peers.push(PeerEntry { addr: addr.to_string(), node_pub: Some(node_pub.to_string()) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_card_matches_local_and_remote_models() {
        let card = NodeCard {
            models: vec!["qwen3:8b".into()],
            remote_models: vec![RemoteModel {
                name: "zo".into(),
                model: "meta-llama/Llama-3.1-8B-Instruct".into(),
                kind: "openai".into(),
                host: "zo.example".into(),
            }],
            max_concurrent: 1,
            ..NodeCard::default()
        };

        assert!(card.serves_model("qwen3"));
        assert!(card.serves_model("zo"));
        assert!(card.serves_model("ZO"));
        assert!(card.serves_model("meta-llama/Llama-3.1-8B-Instruct"));
        assert!(!card.serves_model("meta-llama/llama-3.1-8b-instruct"));
    }
}
