//! The org mesh (Phase 3–4). A node is an ed25519 identity; membership is an
//! org-signed certificate; the wire is an encrypted, mutually-authenticated
//! Noise channel. See DESIGN.md §4–§6.
//!
//! Submodules:
//!   identity   keys, org root, membership certs, enroll tickets, revocation
//!   transport  Noise_XX channel + channel-bound cert authentication
//!   serve      H1/H2/H3 serving pipeline in front of Ollama
//!   client     peer ranking + remote run
//!   gossip     best-effort node cards (advisory only)

pub mod abuse;
pub mod client;
pub mod gossip;
pub mod identity;
pub mod proto;
pub mod relay;
pub mod serve;
pub mod transport;

#[cfg(test)]
mod itest;

use base64::Engine as _;

/// Standard base64 (used for all keys/sigs on disk and on the wire).
pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| format!("bad base64: {e}"))
}

/// Decode a base64 string into a fixed-size array, erroring on wrong length.
pub fn unb64_arr<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let v = unb64(s)?;
    if v.len() != N {
        return Err(format!("expected {N} bytes, got {}", v.len()));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&v);
    Ok(out)
}

/// A short, human-friendly id for a node public key (first 8 b64 chars).
pub fn short_id(node_pub_b64: &str) -> String {
    node_pub_b64.chars().take(8).collect()
}
