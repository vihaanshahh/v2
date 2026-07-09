//! Serving policy and the H1 admission gate (DESIGN.md §6, §9).
//!
//! Policy lives in `~/.v2/policy.toml`. A machine with no policy file uses the
//! defaults below, which are deliberately safe (invariant I5): one remote job,
//! half the VRAM, instant yield to the owner, AC power required. `evaluate()` is
//! pure — no I/O, no clock — so every admission path is exhaustively testable.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub serve: ServePolicy,
    pub quota: QuotaPolicy,
    pub availability: AvailabilityPolicy,
    pub abuse: AbusePolicy,
    pub endpoint: EndpointPolicy,
}

/// The OpenAI-compatible `/v1` surface exposed by `v2 serve`. Everything here is
/// optional and safe by default: the surface is key-gated, local-only, and
/// advertises no public address unless you set one.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EndpointPolicy {
    /// Public/tunnel base URL to advertise as the Base URL (e.g.
    /// "https://host"). Empty = local bind only. `/v1` is appended for clients.
    pub public_url: String,
    /// Pin a specific API key. Empty = use the auto-persisted `~/.v2/api_key`.
    pub api_key: String,
    /// Disable the `/v1` bearer gate entirely. Only for trusted, loopback-only
    /// use — never with a `public_url` set.
    pub open: bool,
    /// Advertise and broker registered hosted endpoints to mesh peers. Off by
    /// default because those calls may spend provider API keys.
    pub share_in_mesh: bool,
}

impl Default for EndpointPolicy {
    fn default() -> Self {
        Self { public_url: String::new(), api_key: String::new(), open: false, share_in_mesh: false }
    }
}

/// Flood / DoS controls, applied to *every* connection (members included).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AbusePolicy {
    /// Global cap on simultaneous connections.
    pub max_connections: u32,
    /// Simultaneous connections allowed from a single IP.
    pub max_connections_per_ip: u32,
    /// New-connection (handshake) rate per IP, averaged per minute.
    pub handshake_rate_per_min: u32,
    /// Burst allowance for the per-IP handshake bucket.
    pub handshake_burst: u32,
    /// Admission refusals from one node within `strike_window_s` before a ban.
    pub strike_limit: u32,
    pub strike_window_s: u64,
    /// How long a banned node is refused, seconds.
    pub ban_secs: u64,
    /// Ceiling on tokens served to all peers combined, per hour.
    pub global_tokens_per_hour: u64,
    /// Node ids (base64) always refused.
    pub deny_nodes: Vec<String>,
    /// If non-empty, only these node ids may be served (allowlist).
    pub only_nodes: Vec<String>,
}

impl Default for AbusePolicy {
    fn default() -> Self {
        Self {
            max_connections: 256,
            max_connections_per_ip: 16,
            handshake_rate_per_min: 60,
            handshake_burst: 20,
            strike_limit: 10,
            strike_window_s: 60,
            ban_secs: 300,
            global_tokens_per_hour: 2_000_000,
            deny_nodes: vec![],
            only_nodes: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServePolicy {
    pub max_concurrent_remote: u32,
    pub max_vram_fraction: f64,
    pub allowed_models: Vec<String>,
    pub max_ctx: u32,
    pub request_timeout_s: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaPolicy {
    pub per_peer_tokens_per_hour: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AvailabilityPolicy {
    /// "always" or "HH:MM-HH:MM" (UTC).
    pub hours: String,
    pub require_ac_power: bool,
    pub yield_to_local: bool,
    /// Seconds of local inactivity before remote work is allowed again.
    pub local_cooldown_s: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            serve: ServePolicy::default(),
            quota: QuotaPolicy::default(),
            availability: AvailabilityPolicy::default(),
            abuse: AbusePolicy::default(),
            endpoint: EndpointPolicy::default(),
        }
    }
}

impl Default for ServePolicy {
    fn default() -> Self {
        Self {
            max_concurrent_remote: 1,
            max_vram_fraction: 0.5,
            allowed_models: vec!["*".into()],
            max_ctx: 8192,
            request_timeout_s: 120,
        }
    }
}

impl Default for QuotaPolicy {
    fn default() -> Self {
        Self { per_peer_tokens_per_hour: 200_000 }
    }
}

impl Default for AvailabilityPolicy {
    fn default() -> Self {
        Self {
            hours: "always".into(),
            require_ac_power: true,
            yield_to_local: true,
            local_cooldown_s: 60,
        }
    }
}

impl Policy {
    /// Load `~/.v2/policy.toml`, or defaults if absent. A parse error is a hard
    /// failure (fail closed): the caller must refuse to serve.
    pub fn load() -> Result<Self, String> {
        let path = match crate::paths::file("policy.toml") {
            Ok(p) => p,
            Err(_) => return Ok(Policy::default()),
        };
        if !path.exists() {
            return Ok(Policy::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("read policy.toml: {e}"))?;
        let policy: Policy = toml::from_str(&raw).map_err(|e| format!("invalid policy.toml: {e}"))?;
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), String> {
        let spec = self.availability.hours.trim();
        if spec.eq_ignore_ascii_case("always") || spec.is_empty() || parse_window(spec).is_some() {
            Ok(())
        } else {
            Err(format!("invalid availability.hours: {spec} (use `always` or `HH:MM-HH:MM`)"))
        }
    }

    /// Is `now` (unix seconds, treated as UTC) within the serving window?
    pub fn within_hours(&self, now: u64) -> bool {
        let spec = self.availability.hours.trim();
        if spec.eq_ignore_ascii_case("always") || spec.is_empty() {
            return true;
        }
        let Some((start, end)) = parse_window(spec) else { return false };
        let minute_of_day = ((now % 86_400) / 60) as u32;
        if start <= end {
            minute_of_day >= start && minute_of_day < end
        } else {
            // Overnight window, e.g. 22:00-06:00.
            minute_of_day >= start || minute_of_day < end
        }
    }
}

fn parse_window(spec: &str) -> Option<(u32, u32)> {
    let (a, b) = spec.split_once('-')?;
    Some((parse_hm(a)?, parse_hm(b)?))
}

fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

// ── Admission gate (H1) ──────────────────────────────────────────────────────

/// What the peer is asking us to run.
pub struct AdmissionRequest {
    pub model: String,
    pub ctx: u32,
    /// This job's projected VRAM as a fraction of total (0.0–1.0).
    pub projected_vram_fraction: f64,
}

/// Current serving state on this node.
pub struct AdmissionState {
    pub concurrent_remote: u32,
    pub used_vram_fraction: f64,
    pub peer_tokens_last_hour: u64,
    pub owner_active: bool,
    pub on_ac_power: bool,
    pub within_hours: bool,
}

#[derive(Debug, PartialEq)]
pub enum Admit {
    /// Accept and run now.
    Ok,
    /// Temporarily full; the peer should retry or try another node.
    Queue(String),
    /// Rejected by policy; do not retry with the same request.
    Refuse(String),
}

/// The ordered admission gate. Cheapest / most security-critical checks first.
/// Note: certificate validity is checked earlier, in the transport layer — by
/// the time a request reaches here the peer is already authenticated.
pub fn evaluate(policy: &Policy, req: &AdmissionRequest, st: &AdmissionState) -> Admit {
    // 1. Model allowed?
    let allowed = policy
        .serve
        .allowed_models
        .iter()
        .any(|p| glob(&p.to_lowercase(), &req.model.to_lowercase()));
    if !allowed {
        return Admit::Refuse(format!("model {} not allowed by policy", req.model));
    }

    // 2. Context bound.
    if req.ctx > policy.serve.max_ctx {
        return Admit::Refuse(format!(
            "ctx {} exceeds max {}",
            req.ctx, policy.serve.max_ctx
        ));
    }

    // 3. Per-peer quota.
    if st.peer_tokens_last_hour >= policy.quota.per_peer_tokens_per_hour {
        return Admit::Refuse(format!(
            "hourly token quota reached ({}/{})",
            st.peer_tokens_last_hour, policy.quota.per_peer_tokens_per_hour
        ));
    }

    // 4. Availability — the owner always wins (I1).
    if policy.availability.require_ac_power && !st.on_ac_power {
        return Admit::Refuse("node on battery".into());
    }
    if !st.within_hours {
        return Admit::Refuse("outside serving hours".into());
    }
    if policy.availability.yield_to_local && st.owner_active {
        return Admit::Refuse("owner is using the machine".into());
    }

    // 5. Resource gate — concurrency then VRAM. These are "try later", not "no".
    if policy.serve.max_concurrent_remote > 0 && st.concurrent_remote >= policy.serve.max_concurrent_remote {
        return Admit::Queue(format!(
            "at concurrency limit ({}/{})",
            st.concurrent_remote, policy.serve.max_concurrent_remote
        ));
    }
    if st.used_vram_fraction + req.projected_vram_fraction > policy.serve.max_vram_fraction {
        return Admit::Queue(format!(
            "would exceed VRAM budget ({:.0}% + {:.0}% > {:.0}%)",
            st.used_vram_fraction * 100.0,
            req.projected_vram_fraction * 100.0,
            policy.serve.max_vram_fraction * 100.0
        ));
    }

    Admit::Ok
}

/// Small glob: `*` alone matches everything; `pre*`, `*suf`, and exact/substring.
pub(crate) fn glob(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(rest) = pattern.strip_suffix('*') {
        return text.starts_with(rest);
    }
    if let Some(rest) = pattern.strip_prefix('*') {
        return text.ends_with(rest);
    }
    text == pattern || text.starts_with(&format!("{pattern}:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_state() -> AdmissionState {
        AdmissionState {
            concurrent_remote: 0,
            used_vram_fraction: 0.0,
            peer_tokens_last_hour: 0,
            owner_active: false,
            on_ac_power: true,
            within_hours: true,
        }
    }

    fn req(model: &str, ctx: u32, vram: f64) -> AdmissionRequest {
        AdmissionRequest { model: model.into(), ctx, projected_vram_fraction: vram }
    }

    #[test]
    fn defaults_are_safe() {
        let p = Policy::default();
        assert_eq!(p.serve.max_concurrent_remote, 1);
        assert_eq!(p.serve.max_vram_fraction, 0.5);
        assert!(p.availability.yield_to_local);
        assert!(p.availability.require_ac_power);
    }

    #[test]
    fn happy_path_admits() {
        let p = Policy::default();
        assert_eq!(evaluate(&p, &req("qwen3:8b", 4096, 0.3), &base_state()), Admit::Ok);
    }

    #[test]
    fn owner_activity_always_wins() {
        let p = Policy::default();
        let mut st = base_state();
        st.owner_active = true;
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 4096, 0.1), &st), Admit::Refuse(_)));
    }

    #[test]
    fn battery_refused_by_default() {
        let p = Policy::default();
        let mut st = base_state();
        st.on_ac_power = false;
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 1024, 0.1), &st), Admit::Refuse(_)));
    }

    #[test]
    fn disallowed_model_refused() {
        let mut p = Policy::default();
        p.serve.allowed_models = vec!["llama3.2:*".into()];
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 1024, 0.1), &base_state()), Admit::Refuse(_)));
        assert_eq!(evaluate(&p, &req("llama3.2:3b", 1024, 0.1), &base_state()), Admit::Ok);
    }

    #[test]
    fn ctx_over_max_refused() {
        let p = Policy::default();
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 32768, 0.1), &base_state()), Admit::Refuse(_)));
    }

    #[test]
    fn quota_exhausted_refused() {
        let p = Policy::default();
        let mut st = base_state();
        st.peer_tokens_last_hour = 200_000;
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 1024, 0.1), &st), Admit::Refuse(_)));
    }

    #[test]
    fn concurrency_and_vram_queue_not_refuse() {
        let p = Policy::default();
        let mut st = base_state();
        st.concurrent_remote = 1;
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 1024, 0.1), &st), Admit::Queue(_)));

        let mut st2 = base_state();
        st2.used_vram_fraction = 0.4;
        assert!(matches!(evaluate(&p, &req("qwen3:8b", 1024, 0.3), &st2), Admit::Queue(_)));
    }

    #[test]
    fn hours_window_utc() {
        let mut p = Policy::default();
        p.availability.hours = "09:00-17:00".into();
        // 12:00 UTC on day 0 = 43200 s
        assert!(p.within_hours(43_200));
        // 20:00 UTC = 72000 s
        assert!(!p.within_hours(72_000));
        // overnight
        p.availability.hours = "22:00-06:00".into();
        assert!(p.within_hours(3_600)); // 01:00
        assert!(!p.within_hours(43_200)); // 12:00
    }

    #[test]
    fn invalid_hours_fail_closed() {
        let mut p = Policy::default();
        p.availability.hours = "9 to 5".into();
        assert!(!p.within_hours(43_200));
        assert!(p.validate().is_err());
    }
}
