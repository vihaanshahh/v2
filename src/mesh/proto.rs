//! Application protocol spoken over an authenticated [`super::transport::Channel`].
//!
//! One `Request` from the client, then a stream of `Frame`s from the server
//! until `Done`/`Refused`/`Error`. Content lives only in these in-memory
//! messages — never written to disk (invariant I4).

use serde::{Deserialize, Serialize};

use super::identity::MembershipCert;

/// Client -> server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Request {
    /// Streamed chat completion. `messages` is the full history (stateless).
    Chat {
        model: String,
        ctx: u32,
        messages: serde_json::Value,
    },
    /// Ask the peer to describe itself (advisory discovery).
    Card,
    /// Liveness check.
    Ping,
}

/// Server -> client. Sent as a sequence; the terminal frame is `Done`,
/// `Refused`, or `Error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Frame {
    /// Admission passed; generation started.
    Accepted,
    /// A content delta.
    Token { c: String },
    /// Terminal: completed. Carries the server-signed receipt to co-sign.
    Done {
        tokens_in: u64,
        tokens_out: u64,
        duration_ms: u64,
        receipt: Receipt,
    },
    /// Terminal: policy rejected (do not retry same request here).
    Refused { reason: String },
    /// Non-terminal-intent: temporarily full; try another node.
    Queued { reason: String },
    /// Terminal: an error or a mid-stream preemption (owner reclaimed).
    Error { reason: String },
    /// Response to `Card`.
    Card { card: super::gossip::NodeCard },
    /// Response to `Ping`.
    Pong { cert: MembershipCert },
}

/// A signed record of one served request. The server signs the canonical bytes;
/// the client co-signs and both store it, so neither side can later forge or
/// deny usage (H5). Forging requires both node keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Receipt {
    pub server_pub: String,
    pub client_pub: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub ts: u64,
    #[serde(default)]
    pub server_sig: String,
    #[serde(default)]
    pub client_sig: String,
}

impl Receipt {
    /// Canonical bytes both parties sign (excludes the signature fields).
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut m = Vec::with_capacity(128);
        m.extend_from_slice(b"v2-receipt-v1");
        m.extend_from_slice(self.server_pub.as_bytes());
        m.push(0x1f);
        m.extend_from_slice(self.client_pub.as_bytes());
        m.push(0x1f);
        m.extend_from_slice(self.model.as_bytes());
        m.push(0x1f);
        m.extend_from_slice(&self.tokens_in.to_be_bytes());
        m.extend_from_slice(&self.tokens_out.to_be_bytes());
        m.extend_from_slice(&self.ts.to_be_bytes());
        m
    }

    /// Verify both signatures over the canonical bytes. Returns
    /// `(server_sig_ok, client_sig_present_and_ok)`. Forging either requires the
    /// corresponding node's secret key, so a tampered receipt fails to verify.
    pub fn verify(&self) -> (bool, bool) {
        let msg = self.signing_bytes();
        let server_ok = sig_ok(&self.server_pub, &self.server_sig, &msg);
        let client_ok = !self.client_sig.is_empty() && sig_ok(&self.client_pub, &self.client_sig, &msg);
        (server_ok, client_ok)
    }
}

fn sig_ok(pub_b64: &str, sig_b64: &str, msg: &[u8]) -> bool {
    let (Ok(pk), Ok(sig)) = (super::unb64_arr::<32>(pub_b64), super::unb64_arr::<64>(sig_b64)) else {
        return false;
    };
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&pk) else { return false };
    vk.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// The client's co-signature, sent back after `Done`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoSign {
    pub client_sig: String,
}

/// Sent by the admin to a joining node after a valid enrollment ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub org_pub: String,
    pub cert: MembershipCert,
}
