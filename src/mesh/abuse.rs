//! Flood / DoS control, applied to every connection — members included.
//!
//! Layered so the cheapest checks run first:
//!   - **Pre-handshake, by IP:** a per-IP token bucket rate-limits new
//!     connections, and global + per-IP caps bound concurrency. A flood is
//!     dropped at `accept()` in microseconds, before any Noise crypto.
//!   - **Post-auth, by node id:** deny/allow lists and active bans.
//!   - **Per-request, by node id:** strike-based bans (too many refusals ->
//!     temporary cooldown) and a global tokens/hour ceiling.
//!
//! All methods take an explicit `now` (unix seconds) so the logic is pure and
//! deterministically testable; callers pass `usage::now_unix()`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::policy::AbusePolicy;

struct Bucket {
    tokens: f64,
    last: u64,
}

#[derive(Default)]
struct Inner {
    buckets: HashMap<IpAddr, Bucket>,
    conns_per_ip: HashMap<IpAddr, u32>,
    total_conns: u32,
    strikes: HashMap<String, Vec<u64>>,
    bans: HashMap<String, u64>, // node -> unban timestamp
    global_tokens: Vec<(u64, u64)>,
}

pub struct AbuseControl {
    policy: AbusePolicy,
    inner: Mutex<Inner>,
}

/// Held for the lifetime of a connection; releases its slot on drop.
pub struct ConnPermit {
    ctl: Arc<AbuseControl>,
    ip: IpAddr,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.ctl.inner.lock() {
            inner.total_conns = inner.total_conns.saturating_sub(1);
            if let Some(n) = inner.conns_per_ip.get_mut(&self.ip) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    inner.conns_per_ip.remove(&self.ip);
                }
            }
        }
    }
}

impl AbuseControl {
    pub fn new(policy: AbusePolicy) -> Self {
        Self { policy, inner: Mutex::new(Inner::default()) }
    }

    /// Gate a new inbound connection by IP. `Ok(permit)` to proceed (the permit
    /// must be kept for the connection's lifetime); `Err(reason)` to drop it now.
    pub fn allow_connection(self: &Arc<Self>, ip: IpAddr, now: u64) -> Result<ConnPermit, String> {
        let mut inner = self.inner.lock().map_err(|_| "lock".to_string())?;

        if inner.total_conns >= self.policy.max_connections {
            return Err("server at global connection limit".into());
        }
        if *inner.conns_per_ip.get(&ip).unwrap_or(&0) >= self.policy.max_connections_per_ip {
            return Err("too many connections from this address".into());
        }

        // Per-IP token bucket for handshake rate.
        let cap = self.policy.handshake_burst.max(1) as f64;
        let refill = self.policy.handshake_rate_per_min.max(1) as f64 / 60.0;
        let b = inner.buckets.entry(ip).or_insert(Bucket { tokens: cap, last: now });
        let elapsed = now.saturating_sub(b.last) as f64;
        b.tokens = (b.tokens + elapsed * refill).min(cap);
        b.last = now;
        if b.tokens < 1.0 {
            return Err("connection rate limit exceeded".into());
        }
        b.tokens -= 1.0;

        inner.total_conns += 1;
        *inner.conns_per_ip.entry(ip).or_insert(0) += 1;
        Ok(ConnPermit { ctl: self.clone(), ip })
    }

    /// Deny/allow-list and active-ban check for an authenticated node.
    /// `is_member` gates the allowlist (enrolling nodes aren't on it yet).
    pub fn check_node(&self, node_pub: &str, is_member: bool, now: u64) -> Result<(), String> {
        if self.policy.deny_nodes.iter().any(|d| d == node_pub) {
            return Err("node is denied by policy".into());
        }
        if is_member
            && !self.policy.only_nodes.is_empty()
            && !self.policy.only_nodes.iter().any(|a| a == node_pub)
        {
            return Err("node not on the serving allowlist".into());
        }
        if let Some(retry) = self.banned_for(node_pub, now) {
            return Err(format!("temporarily banned; retry in {retry}s"));
        }
        Ok(())
    }

    /// Seconds remaining on a node's ban, if any (prunes expired bans).
    pub fn banned_for(&self, node_pub: &str, now: u64) -> Option<u64> {
        let mut inner = self.inner.lock().ok()?;
        match inner.bans.get(node_pub).copied() {
            Some(until) if until > now => Some(until - now),
            Some(_) => {
                inner.bans.remove(node_pub);
                None
            }
            None => None,
        }
    }

    /// Record an admission refusal; ban the node if it crosses the strike limit.
    pub fn record_strike(&self, node_pub: &str, now: u64) {
        let Ok(mut inner) = self.inner.lock() else { return };
        let window = self.policy.strike_window_s;
        let list = inner.strikes.entry(node_pub.to_string()).or_default();
        list.push(now);
        list.retain(|t| now.saturating_sub(*t) < window);
        if list.len() as u32 >= self.policy.strike_limit {
            inner.bans.insert(node_pub.to_string(), now + self.policy.ban_secs);
            inner.strikes.remove(node_pub);
        }
    }

    /// Tokens served to all peers in the last hour.
    pub fn global_tokens_last_hour(&self, now: u64) -> u64 {
        let Ok(mut inner) = self.inner.lock() else { return 0 };
        inner.global_tokens.retain(|(t, _)| now.saturating_sub(*t) < 3600);
        inner.global_tokens.iter().map(|(_, n)| n).sum()
    }

    pub fn global_over_budget(&self, now: u64) -> bool {
        self.global_tokens_last_hour(now) >= self.policy.global_tokens_per_hour
    }

    pub fn record_tokens(&self, tokens: u64, now: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.global_tokens.push((now, tokens));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctl(policy: AbusePolicy) -> Arc<AbuseControl> {
        Arc::new(AbuseControl::new(policy))
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn rate_limit_allows_burst_then_denies() {
        let mut p = AbusePolicy::default();
        p.handshake_burst = 3;
        p.handshake_rate_per_min = 60; // 1/sec
        let c = ctl(p);
        // Same second: burst of 3 allowed, 4th denied.
        let mut permits = vec![];
        for _ in 0..3 {
            permits.push(c.allow_connection(ip(1), 1000).expect("within burst"));
        }
        assert!(c.allow_connection(ip(1), 1000).is_err(), "burst exhausted");
        // One second later, ~1 token refilled -> one more allowed.
        assert!(c.allow_connection(ip(1), 1001).is_ok(), "refilled after 1s");
    }

    #[test]
    fn per_ip_connection_cap() {
        let mut p = AbusePolicy::default();
        p.max_connections_per_ip = 2;
        p.handshake_burst = 100;
        let c = ctl(p);
        let _a = c.allow_connection(ip(1), 0).unwrap();
        let _b = c.allow_connection(ip(1), 0).unwrap();
        assert!(c.allow_connection(ip(1), 0).is_err(), "3rd conn from same ip blocked");
        // A different IP is unaffected.
        assert!(c.allow_connection(ip(2), 0).is_ok());
    }

    #[test]
    fn permit_release_frees_the_slot() {
        let mut p = AbusePolicy::default();
        p.max_connections_per_ip = 1;
        p.handshake_burst = 100;
        let c = ctl(p);
        {
            let _a = c.allow_connection(ip(1), 0).unwrap();
            assert!(c.allow_connection(ip(1), 0).is_err());
        } // _a dropped here
        assert!(c.allow_connection(ip(1), 0).is_ok(), "slot freed on drop");
    }

    #[test]
    fn strikes_lead_to_a_ban_then_expire() {
        let mut p = AbusePolicy::default();
        p.strike_limit = 3;
        p.strike_window_s = 60;
        p.ban_secs = 100;
        let c = ctl(p);
        let node = "abc";
        c.record_strike(node, 10);
        c.record_strike(node, 11);
        assert!(c.check_node(node, true, 12).is_ok(), "under strike limit");
        c.record_strike(node, 12); // 3rd -> ban
        assert!(c.check_node(node, true, 13).is_err(), "banned after strike limit");
        assert!(c.check_node(node, true, 200).is_ok(), "ban expired");
    }

    #[test]
    fn strikes_outside_window_dont_accumulate() {
        let mut p = AbusePolicy::default();
        p.strike_limit = 2;
        p.strike_window_s = 30;
        let c = ctl(p);
        c.record_strike("n", 0);
        c.record_strike("n", 100); // old strike pruned
        assert!(c.check_node("n", true, 101).is_ok(), "stale strike doesn't count");
    }

    #[test]
    fn global_token_budget() {
        let mut p = AbusePolicy::default();
        p.global_tokens_per_hour = 1000;
        let c = ctl(p);
        c.record_tokens(600, 0);
        assert!(!c.global_over_budget(10));
        c.record_tokens(600, 10);
        assert!(c.global_over_budget(20), "over the hourly ceiling");
        assert!(!c.global_over_budget(3700), "window slid, old tokens expired");
    }

    #[test]
    fn deny_and_allow_lists() {
        let mut p = AbusePolicy::default();
        p.deny_nodes = vec!["bad".into()];
        p.only_nodes = vec!["good".into()];
        let c = ctl(p);
        assert!(c.check_node("bad", true, 0).is_err(), "denied node blocked");
        assert!(c.check_node("other", true, 0).is_err(), "not on allowlist");
        assert!(c.check_node("good", true, 0).is_ok(), "allowlisted node ok");
        // Allowlist doesn't apply to enrolling (not-yet-member) nodes.
        assert!(c.check_node("newcomer", false, 0).is_ok());
    }
}
