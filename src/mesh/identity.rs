//! Cryptographic identity and authorization for the mesh.
//!
//! Trust model (DESIGN.md §5):
//!   - A **node** is an ed25519 keypair. Its public key is its mesh identity.
//!   - The **org root** is an ed25519 keypair held by the admin. It signs
//!     membership certs and revocations, and never leaves the admin's machine.
//!   - A **membership cert** is an org signature over (node_pub, expiry, caps).
//!     Short-lived (default 24h) so revocation cannot fail silently — an
//!     un-renewed cert simply stops working (expiry-beats-revocation, §4).
//!
//! Channel binding lives in `transport.rs`: after the Noise handshake, each side
//! signs the unique handshake hash with its node key, proving the peer on *this*
//! encrypted channel is the same key the cert authorizes.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{b64, unb64_arr};
use crate::paths;
use crate::usage::now_unix;

const CERT_DOMAIN: &[u8] = b"v2-membership-v1";
const REVOKE_DOMAIN: &[u8] = b"v2-revocation-v1";
const TICKET_DOMAIN: &[u8] = b"v2-enroll-v1";
const DEFAULT_CERT_TTL: u64 = 24 * 3600;
/// Allow small clock skew when checking issued-time.
const CLOCK_SKEW: u64 = 300;

fn rand_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).map_err(|e| format!("rng failure: {e}"))?;
    Ok(buf)
}

// ── Node identity ────────────────────────────────────────────────────────────

/// This machine's ed25519 identity, persisted as a 32-byte seed at `~/.v2/key`.
pub struct NodeKey {
    signing: SigningKey,
}

impl NodeKey {
    pub fn load_or_create() -> Result<Self, String> {
        let path = paths::file("key").map_err(|e| e.to_string())?;
        if path.exists() {
            let raw = std::fs::read(&path).map_err(|e| format!("read key: {e}"))?;
            if raw.len() != 32 {
                return Err(format!("corrupt key file ({} bytes)", raw.len()));
            }
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&raw);
            return Ok(Self { signing: SigningKey::from_bytes(&seed) });
        }
        let seed: [u8; 32] = rand_bytes()?;
        let signing = SigningKey::from_bytes(&seed);
        write_secret(&path, &seed)?;
        Ok(Self { signing })
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn public_b64(&self) -> String {
        b64(&self.public_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    #[cfg(test)]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(&seed) }
    }
}

/// Write 0600 where the platform supports it.
fn write_secret(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        f.write_all(bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        f.write_all(bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

// ── Org root ─────────────────────────────────────────────────────────────────

/// The org signing key (admin only), persisted at `~/.v2/mesh/org_root.key`.
pub struct OrgRoot {
    signing: SigningKey,
}

impl OrgRoot {
    pub fn create() -> Result<Self, String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let path = dir.join("org_root.key");
        if path.exists() {
            return Err("org already initialised (mesh/org_root.key exists)".into());
        }
        let seed: [u8; 32] = rand_bytes()?;
        write_secret(&path, &seed)?;
        Ok(Self { signing: SigningKey::from_bytes(&seed) })
    }

    pub fn load() -> Result<Self, String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let raw = std::fs::read(dir.join("org_root.key"))
            .map_err(|_| "not an org admin (no mesh/org_root.key); run `v2 mesh init`".to_string())?;
        if raw.len() != 32 {
            return Err("corrupt org_root.key".into());
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&raw);
        Ok(Self { signing: SigningKey::from_bytes(&seed) })
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn public_b64(&self) -> String {
        b64(&self.public_bytes())
    }

    #[cfg(test)]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(&seed) }
    }

    /// Issue a membership cert for a node public key.
    pub fn issue_cert(&self, node_pub: [u8; 32], ttl_secs: u64, caps: Vec<String>) -> MembershipCert {
        let issued = now_unix();
        let expiry = issued + if ttl_secs == 0 { DEFAULT_CERT_TTL } else { ttl_secs };
        let org_pub = self.public_bytes();
        let msg = cert_signing_bytes(&node_pub, &org_pub, issued, expiry, &caps);
        let sig = self.signing.sign(&msg).to_bytes();
        MembershipCert {
            node_pub: b64(&node_pub),
            org_pub: b64(&org_pub),
            issued,
            expiry,
            caps,
            sig: b64(&sig),
        }
    }

    /// Sign a revocation for a node public key.
    pub fn revoke(&self, node_pub: [u8; 32]) -> Revocation {
        let issued = now_unix();
        let msg = revoke_signing_bytes(&node_pub, issued);
        let sig = self.signing.sign(&msg).to_bytes();
        Revocation { node_pub: b64(&node_pub), issued, sig: b64(&sig) }
    }

    /// Mint a one-time enrollment ticket pointing at the admin's enroll address.
    pub fn make_ticket(&self, addr: &str, ttl_secs: u64) -> Result<EnrollTicket, String> {
        let nonce: [u8; 16] = rand_bytes()?;
        let expiry = now_unix() + ttl_secs;
        let org_pub = self.public_bytes();
        let msg = ticket_signing_bytes(&org_pub, addr, &nonce, expiry);
        let sig = self.signing.sign(&msg).to_bytes();
        Ok(EnrollTicket {
            org_pub: b64(&org_pub),
            addr: addr.to_string(),
            nonce: b64(&nonce),
            expiry,
            sig: b64(&sig),
        })
    }
}

// ── Signing-byte canonicalisation ────────────────────────────────────────────

fn cert_signing_bytes(node_pub: &[u8; 32], org_pub: &[u8; 32], issued: u64, expiry: u64, caps: &[String]) -> Vec<u8> {
    let mut m = Vec::with_capacity(128);
    m.extend_from_slice(CERT_DOMAIN);
    m.extend_from_slice(node_pub);
    m.extend_from_slice(org_pub);
    m.extend_from_slice(&issued.to_be_bytes());
    m.extend_from_slice(&expiry.to_be_bytes());
    for c in caps {
        m.push(0x1f); // unit separator — caps cannot contain it
        m.extend_from_slice(c.as_bytes());
    }
    m
}

fn revoke_signing_bytes(node_pub: &[u8; 32], issued: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(REVOKE_DOMAIN);
    m.extend_from_slice(node_pub);
    m.extend_from_slice(&issued.to_be_bytes());
    m
}

fn ticket_signing_bytes(org_pub: &[u8; 32], addr: &str, nonce: &[u8; 16], expiry: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(96);
    m.extend_from_slice(TICKET_DOMAIN);
    m.extend_from_slice(org_pub);
    m.extend_from_slice(addr.as_bytes());
    m.push(0x1f);
    m.extend_from_slice(nonce);
    m.extend_from_slice(&expiry.to_be_bytes());
    m
}

fn verify_sig(pub_bytes: &[u8; 32], msg: &[u8], sig_bytes: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pub_bytes) else { return false };
    let sig = Signature::from_bytes(sig_bytes);
    vk.verify_strict(msg, &sig).is_ok()
}

// ── Membership cert ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MembershipCert {
    pub node_pub: String,
    pub org_pub: String,
    pub issued: u64,
    pub expiry: u64,
    #[serde(default)]
    pub caps: Vec<String>,
    pub sig: String,
}

impl MembershipCert {
    /// Verify signature, org binding, and freshness. Fail closed (I2): any error
    /// returns Err with a reason and the caller drops the connection.
    pub fn verify(&self, trusted_org_pub: &[u8; 32], now: u64) -> Result<(), String> {
        self.verify_with_trust(std::slice::from_ref(trusted_org_pub), now).map(|_| ())
    }

    /// Verify against a *set* of trusted orgs (home + federated). Returns the org
    /// that signed it. This is the one extra lookup federation adds (DESIGN §5).
    pub fn verify_with_trust(&self, trusted: &[[u8; 32]], now: u64) -> Result<[u8; 32], String> {
        let node_pub = unb64_arr::<32>(&self.node_pub)?;
        let org_pub = unb64_arr::<32>(&self.org_pub)?;
        let sig = unb64_arr::<64>(&self.sig)?;

        if !trusted.iter().any(|t| t == &org_pub) {
            return Err("cert signed by an untrusted org".into());
        }
        let msg = cert_signing_bytes(&node_pub, &org_pub, self.issued, self.expiry, &self.caps);
        if !verify_sig(&org_pub, &msg, &sig) {
            return Err("cert signature invalid".into());
        }
        if now + CLOCK_SKEW < self.issued {
            return Err("cert not yet valid".into());
        }
        if now >= self.expiry {
            return Err("cert expired".into());
        }
        Ok(org_pub)
    }
}

// ── Federation: additional trusted orgs, each with a scope ───────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedOrg {
    pub org_pub: String,
    #[serde(default)]
    pub note: String,
    /// Model globs this org's members may use here (default: none — deny).
    #[serde(default)]
    pub allowed_models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FederationList {
    #[serde(default)]
    pub orgs: Vec<FederatedOrg>,
}

impl FederationList {
    pub fn load() -> Self {
        let Ok(dir) = paths::subdir("mesh") else { return Self::default() };
        let Ok(raw) = std::fs::read_to_string(dir.join("federation.json")) else { return Self::default() };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        paths::write_private(&dir.join("federation.json"), raw.as_bytes()).map_err(|e| e.to_string())
    }

    /// Trusted org public keys (decoded), for cert verification.
    pub fn trusted_bytes(&self) -> Vec<[u8; 32]> {
        self.orgs.iter().filter_map(|o| unb64_arr::<32>(&o.org_pub).ok()).collect()
    }

    /// The scope (allowed model globs) for a given org, if federated.
    pub fn scope_for(&self, org_pub_b64: &str) -> Option<&[String]> {
        self.orgs.iter().find(|o| o.org_pub == org_pub_b64).map(|o| o.allowed_models.as_slice())
    }
}

// ── Revocation ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revocation {
    pub node_pub: String,
    pub issued: u64,
    pub sig: String,
}

impl Revocation {
    pub fn verify(&self, trusted_org_pub: &[u8; 32]) -> bool {
        let (Ok(node_pub), Ok(sig)) = (unb64_arr::<32>(&self.node_pub), unb64_arr::<64>(&self.sig)) else {
            return false;
        };
        verify_sig(trusted_org_pub, &revoke_signing_bytes(&node_pub, self.issued), &sig)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevocationList {
    #[serde(default)]
    pub revoked: Vec<Revocation>,
}

impl RevocationList {
    pub fn load() -> Self {
        let Ok(dir) = paths::subdir("mesh") else { return Self::default() };
        let Ok(raw) = std::fs::read_to_string(dir.join("revoked.json")) else { return Self::default() };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        paths::write_private(&dir.join("revoked.json"), raw.as_bytes()).map_err(|e| e.to_string())
    }

    /// Add a revocation (verified against the org key before storing).
    pub fn add(&mut self, rev: Revocation, trusted_org_pub: &[u8; 32]) -> Result<(), String> {
        if !rev.verify(trusted_org_pub) {
            return Err("revocation signature invalid".into());
        }
        if !self.revoked.iter().any(|r| r.node_pub == rev.node_pub) {
            self.revoked.push(rev);
        }
        Ok(())
    }

    pub fn is_revoked(&self, node_pub_b64: &str) -> bool {
        self.revoked.iter().any(|r| r.node_pub == node_pub_b64)
    }
}

// ── Enroll ticket ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTicket {
    pub org_pub: String,
    pub addr: String,
    pub nonce: String,
    pub expiry: u64,
    pub sig: String,
}

impl EnrollTicket {
    pub fn encode(&self) -> String {
        b64(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    pub fn decode(s: &str) -> Result<Self, String> {
        let json = super::unb64(s)?;
        serde_json::from_slice(&json).map_err(|e| format!("bad ticket: {e}"))
    }

    /// Verify the org signature and expiry (nonce single-use is checked serve-side).
    pub fn verify(&self, now: u64) -> Result<[u8; 32], String> {
        let org_pub = unb64_arr::<32>(&self.org_pub)?;
        let nonce = unb64_arr::<16>(&self.nonce)?;
        let sig = unb64_arr::<64>(&self.sig)?;
        if now >= self.expiry {
            return Err("ticket expired".into());
        }
        let msg = ticket_signing_bytes(&org_pub, &self.addr, &nonce, self.expiry);
        if !verify_sig(&org_pub, &msg, &sig) {
            return Err("ticket signature invalid".into());
        }
        Ok(org_pub)
    }
}

// ── Member identity on disk (org.json) ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshIdentity {
    pub org_pub: String,
    pub cert: MembershipCert,
}

impl MeshIdentity {
    pub fn load() -> Result<Option<Self>, String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let path = dir.join("org.json");
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&raw).map(Some).map_err(|e| format!("corrupt org.json: {e}"))
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = paths::subdir("mesh").map_err(|e| e.to_string())?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        paths::write_private(&dir.join("org.json"), raw.as_bytes()).map_err(|e| e.to_string())
    }

    pub fn org_pub_bytes(&self) -> Result<[u8; 32], String> {
        unb64_arr::<32>(&self.org_pub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org_and_node() -> (SigningKey, [u8; 32], [u8; 32]) {
        // Deterministic keys for tests (no rng).
        let org = SigningKey::from_bytes(&[7u8; 32]);
        let node = SigningKey::from_bytes(&[9u8; 32]);
        (org.clone(), org.verifying_key().to_bytes(), node.verifying_key().to_bytes())
    }

    fn issue(org: &SigningKey, node_pub: [u8; 32], issued: u64, expiry: u64) -> MembershipCert {
        let org_pub = org.verifying_key().to_bytes();
        let caps = vec![];
        let msg = cert_signing_bytes(&node_pub, &org_pub, issued, expiry, &caps);
        MembershipCert {
            node_pub: b64(&node_pub),
            org_pub: b64(&org_pub),
            issued,
            expiry,
            caps,
            sig: b64(&org.sign(&msg).to_bytes()),
        }
    }

    #[test]
    fn valid_cert_verifies() {
        let (org, org_pub, node_pub) = org_and_node();
        let cert = issue(&org, node_pub, 1000, 100_000);
        assert!(cert.verify(&org_pub, 5000).is_ok());
    }

    #[test]
    fn expired_cert_rejected() {
        let (org, org_pub, node_pub) = org_and_node();
        let cert = issue(&org, node_pub, 1000, 2000);
        assert!(cert.verify(&org_pub, 5000).is_err());
    }

    #[test]
    fn wrong_org_rejected() {
        let (org, _org_pub, node_pub) = org_and_node();
        let cert = issue(&org, node_pub, 1000, 100_000);
        let other_org = SigningKey::from_bytes(&[42u8; 32]).verifying_key().to_bytes();
        assert!(cert.verify(&other_org, 5000).is_err());
    }

    #[test]
    fn tampered_expiry_rejected() {
        let (org, org_pub, node_pub) = org_and_node();
        let mut cert = issue(&org, node_pub, 1000, 2000);
        // Attacker extends expiry without a fresh signature.
        cert.expiry = 999_999;
        assert!(cert.verify(&org_pub, 5000).is_err(), "signature must cover expiry");
    }

    #[test]
    fn tampered_nodepub_rejected() {
        let (org, org_pub, node_pub) = org_and_node();
        let mut cert = issue(&org, node_pub, 1000, 100_000);
        let attacker = SigningKey::from_bytes(&[13u8; 32]).verifying_key().to_bytes();
        cert.node_pub = b64(&attacker);
        assert!(cert.verify(&org_pub, 5000).is_err(), "signature must cover node_pub");
    }

    #[test]
    fn revocation_roundtrip() {
        let (org, org_pub, node_pub) = org_and_node();
        let issued = 1234;
        let msg = revoke_signing_bytes(&node_pub, issued);
        let rev = Revocation { node_pub: b64(&node_pub), issued, sig: b64(&org.sign(&msg).to_bytes()) };
        assert!(rev.verify(&org_pub));
        let mut list = RevocationList::default();
        list.add(rev, &org_pub).unwrap();
        assert!(list.is_revoked(&b64(&node_pub)));
    }

    #[test]
    fn ticket_encode_decode_verify() {
        let (org, org_pub, _node) = org_and_node();
        let nonce = [3u8; 16];
        let expiry = 999_999;
        let addr = "10.0.0.5:4830";
        let msg = ticket_signing_bytes(&org_pub, addr, &nonce, expiry);
        let ticket = EnrollTicket {
            org_pub: b64(&org_pub), addr: addr.into(), nonce: b64(&nonce), expiry,
            sig: b64(&org.sign(&msg).to_bytes()),
        };
        let wire = ticket.encode();
        let back = EnrollTicket::decode(&wire).unwrap();
        assert_eq!(back.verify(1000).unwrap(), org_pub);
        assert!(back.verify(1_000_000).is_err(), "expired ticket rejected");
    }
}
