//! Serving pipeline: admission (H1) -> Ollama execution (H2) -> receipt (H5),
//! with reclaim (H3) enforced throughout. One thread per connection, blocking.
//!
//! The owner always wins: a paused daemon or an active owner preempts in-flight
//! generation by returning `false` from the stream callback, which drops the
//! upstream connection and stops Ollama immediately (deadman, DESIGN.md §4).

use std::collections::{HashMap, HashSet};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::activity::Activity;
use crate::engine;
use crate::hardware::HardwareInfo;
use crate::ollama;
use crate::ollama_api;
use crate::policy::{evaluate, Admit, AdmissionRequest, AdmissionState, Policy};
use crate::usage::{self, now_unix, UsageRecord};

use super::abuse::AbuseControl;
use super::gossip::{self, NodeCard};
use super::identity::{MembershipCert, MeshIdentity, NodeKey, OrgRoot, RevocationList};
use super::proto::{CoSign, EnrollResponse, Frame, Receipt, Request};
use super::transport::{self, Channel, Peer};
use super::{b64, short_id, unb64_arr};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const QUOTA_WINDOW: u64 = 3600;

/// Everything a connection thread needs. All fields are cheap to clone (Arc).
#[derive(Clone)]
pub struct ServeCtx {
    pub node: Arc<NodeKey>,
    pub org_pub: [u8; 32],
    pub cert: MembershipCert,
    pub policy: Policy,
    pub ollama_host: String,
    pub hw: Arc<HardwareInfo>,
    pub activity: Activity,
    pub paused: Arc<AtomicBool>,
    pub concurrent: Arc<AtomicU32>,
    pub used_vram_milli: Arc<AtomicU32>,
    pub abuse: Arc<AbuseControl>,
    pub quota: Arc<Mutex<HashMap<String, Vec<(u64, u64)>>>>,
    /// Admin only: lets this node enroll new members.
    pub org_root: Option<Arc<OrgRoot>>,
    pub used_nonces: Arc<Mutex<HashSet<String>>>,
}

/// RAII reservation: releases the concurrency slot and VRAM budget on any exit.
struct Slot {
    concurrent: Arc<AtomicU32>,
    used_vram_milli: Arc<AtomicU32>,
    milli: u32,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        self.used_vram_milli.fetch_sub(self.milli, Ordering::SeqCst);
    }
}

/// Bind and serve the mesh port until the process stops. Blocks.
pub fn run(ctx: ServeCtx, listen: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    let role = if ctx.org_root.is_some() { "admin" } else { "member" };
    println!(
        "v2 mesh  serving on {listen} as {} ({})",
        short_id(&ctx.node.public_b64()),
        role
    );
    serve_loop(ctx, listener);
    Ok(())
}

/// Accept + dispatch connections on an already-bound listener. Blocks.
pub fn serve_loop(ctx: ServeCtx, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };

        // Pre-handshake flood gate, by IP — the cheapest possible rejection.
        // A rate-limited or over-cap connection is dropped here, before any
        // crypto or a thread is committed.
        let ip = match stream.peer_addr() {
            Ok(a) => a.ip(),
            Err(_) => continue,
        };
        let permit = match ctx.abuse.allow_connection(ip, now_unix()) {
            Ok(p) => p,
            Err(_) => continue, // drop the stream silently
        };

        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _permit = permit; // released when the connection ends
            handle(ctx, stream);
        });
    }
}

fn handle(ctx: ServeCtx, stream: TcpStream) {
    // Re-read the revocation list per connection so `v2 mesh revoke` takes effect
    // immediately, without restarting the daemon. (Cross-node distribution still
    // relies on the file being updated on each node + the 24h cert TTL.)
    let revs = RevocationList::load();
    let (mut ch, peer) = match transport::accept(stream, &ctx.node, ctx.cert.clone(), &ctx.org_pub, &revs) {
        Ok(v) => v,
        Err(e) => {
            // Fail closed (I2): unauthenticated / untrusted peers get nothing.
            eprintln!("v2 mesh: connection rejected: {e}");
            return;
        }
    };

    // Post-auth node checks: deny/allow lists and active bans (by node id).
    let is_member = matches!(peer, Peer::Member { .. });
    if let Err(reason) = ctx.abuse.check_node(peer.node_pub(), is_member, now_unix()) {
        let _ = ch.send_json(&Frame::Refused { reason });
        return;
    }

    match peer {
        Peer::Member { node_pub, cert } => serve_member(&ctx, &mut ch, &node_pub, &cert.org_pub),
        Peer::Enrolling { node_pub, ticket } => {
            if let Err(e) = handle_enroll(&ctx, &mut ch, &node_pub, &ticket) {
                let _ = ch.send_json(&Frame::Error { reason: e });
            }
        }
    }
}

fn serve_member(ctx: &ServeCtx, ch: &mut Channel, peer_pub: &str, peer_org: &str) {
    loop {
        let req: Request = match ch.recv_json() {
            Ok(r) => r,
            Err(_) => return, // client closed the channel
        };
        let result = match req {
            Request::Ping => ch.send_json(&Frame::Pong { cert: ctx.cert.clone() }).map_err(|e| e.to_string()),
            Request::Card => {
                let card = build_card(ctx);
                ch.send_json(&Frame::Card { card }).map_err(|e| e.to_string())
            }
            Request::Chat { model, ctx: cctx, messages } => {
                serve_chat(ctx, ch, peer_pub, peer_org, &model, cctx, &messages)
            }
        };
        if result.is_err() {
            return; // channel broke; peer is gone
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_chat(
    ctx: &ServeCtx,
    ch: &mut Channel,
    peer_pub: &str,
    peer_org: &str,
    model: &str,
    cctx: u32,
    messages: &serde_json::Value,
) -> Result<(), String> {
    // ── Federation scope: a peer from another org may only use models that org
    //    is explicitly scoped for here (default deny). Home-org peers skip this.
    if peer_org != b64(&ctx.org_pub) {
        let fed = super::identity::FederationList::load();
        let allowed = fed.scope_for(peer_org).unwrap_or(&[]);
        let ml = model.to_lowercase();
        if !allowed.iter().any(|p| crate::policy::glob(&p.to_lowercase(), &ml)) {
            return ch
                .send_json(&Frame::Refused { reason: "model not in this org's federation scope".into() })
                .map_err(|e| e.to_string());
        }
    }

    // ── Abuse gate: active ban (may have been applied mid-connection) and the
    //    global tokens/hour ceiling across all peers. ──────────────────────────
    if let Some(retry) = ctx.abuse.banned_for(peer_pub, now_unix()) {
        return ch
            .send_json(&Frame::Refused { reason: format!("temporarily banned; retry in {retry}s") })
            .map_err(|e| e.to_string());
    }
    if ctx.abuse.global_over_budget(now_unix()) {
        return ch
            .send_json(&Frame::Refused { reason: "server hourly capacity reached".into() })
            .map_err(|e| e.to_string());
    }

    // ── H1: admission ────────────────────────────────────────────────────────
    let projected = vram_fraction(&ctx.hw, model, cctx);
    let state = AdmissionState {
        concurrent_remote: ctx.concurrent.load(Ordering::SeqCst),
        used_vram_fraction: ctx.used_vram_milli.load(Ordering::SeqCst) as f64 / 1000.0,
        peer_tokens_last_hour: peer_tokens_last_hour(ctx, peer_pub),
        owner_active: ctx.paused.load(Ordering::SeqCst)
            || ctx.activity.owner_active(ctx.policy.availability.local_cooldown_s),
        on_ac_power: on_ac_power(),
        within_hours: ctx.policy.within_hours(now_unix()),
    };
    let req = AdmissionRequest { model: model.to_string(), ctx: cctx, projected_vram_fraction: projected };

    match evaluate(&ctx.policy, &req, &state) {
        Admit::Refuse(reason) => {
            // A refusal is a strike: enough of them in a window earns a cooldown,
            // so a peer can't hammer the gate for free.
            ctx.abuse.record_strike(peer_pub, now_unix());
            return ch.send_json(&Frame::Refused { reason }).map_err(|e| e.to_string());
        }
        Admit::Queue(reason) => return ch.send_json(&Frame::Queued { reason }).map_err(|e| e.to_string()),
        Admit::Ok => {}
    }

    // Reserve resources (released on any return via Drop).
    let milli = (projected * 1000.0) as u32;
    ctx.concurrent.fetch_add(1, Ordering::SeqCst);
    ctx.used_vram_milli.fetch_add(milli, Ordering::SeqCst);
    let _slot = Slot {
        concurrent: ctx.concurrent.clone(),
        used_vram_milli: ctx.used_vram_milli.clone(),
        milli,
    };

    ch.send_json(&Frame::Accepted).map_err(|e| e.to_string())?;

    // ── H2: execute + stream, enforcing reclaim + timeout on every token ─────
    let start = Instant::now();
    let paused = ctx.paused.clone();
    let activity = ctx.activity.clone();
    let cooldown = ctx.policy.availability.local_cooldown_s;
    let timeout = std::time::Duration::from_secs(ctx.policy.serve.request_timeout_s.max(1));

    let mut abort: Option<String> = None;
    let stream_res = {
        let ch_ref = &mut *ch;
        ollama_api::chat_stream(&ctx.ollama_host, model, messages, |tok| {
            if start.elapsed() > timeout {
                abort = Some("preempted: request timeout".into());
                return false;
            }
            if paused.load(Ordering::SeqCst) {
                abort = Some("preempted: node paused".into());
                return false;
            }
            if activity.owner_active(cooldown) {
                abort = Some("preempted: owner active".into());
                return false;
            }
            // Deadman: if the client is gone this send fails and we abort,
            // which drops the Ollama connection and stops generation.
            if ch_ref.send_json(&Frame::Token { c: tok.to_string() }).is_err() {
                abort = Some("client disconnected".into());
                return false;
            }
            true
        })
    };

    let (tokens_in, tokens_out) = match &stream_res {
        Ok((_full, stats)) => (stats.prompt_eval_count, stats.eval_count),
        Err(_) => (0, 0),
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    // Record what we served regardless of outcome (partial usage still counts).
    record_served(ctx, peer_pub, model, tokens_in, tokens_out, duration_ms);
    ctx.abuse.record_tokens(tokens_out, now_unix());

    if let Some(reason) = abort {
        return ch.send_json(&Frame::Error { reason }).map_err(|e| e.to_string());
    }
    if let Err(e) = stream_res {
        return ch.send_json(&Frame::Error { reason: e }).map_err(|e| e.to_string());
    }

    // ── H5: signed receipt, co-signed by the client ─────────────────────────
    let mut receipt = Receipt {
        server_pub: ctx.node.public_b64(),
        client_pub: peer_pub.to_string(),
        model: model.to_string(),
        tokens_in,
        tokens_out,
        ts: now_unix(),
        server_sig: String::new(),
        client_sig: String::new(),
    };
    receipt.server_sig = b64(&ctx.node.sign(&receipt.signing_bytes()));

    ch.send_json(&Frame::Done { tokens_in, tokens_out, duration_ms, receipt: receipt.clone() })
        .map_err(|e| e.to_string())?;

    // Best-effort co-signature (client may have already left).
    if let Ok(cosign) = ch.recv_json::<CoSign>() {
        receipt.client_sig = cosign.client_sig;
    }
    store_receipt(&receipt);
    Ok(())
}

fn handle_enroll(ctx: &ServeCtx, ch: &mut Channel, node_pub: &str, ticket: &super::identity::EnrollTicket) -> Result<(), String> {
    let org_root = ctx.org_root.as_ref().ok_or("this node cannot enroll members (not the admin)")?;

    // Ticket signature + expiry were verified in the transport layer. Enforce
    // one-time use here.
    {
        let mut used = ctx.used_nonces.lock().map_err(|_| "lock")?;
        if used.contains(&ticket.nonce) {
            return Err("enrollment ticket already used".into());
        }
        used.insert(ticket.nonce.clone());
        save_nonces(&used);
    }

    let node_bytes = unb64_arr::<32>(node_pub)?;
    let cert = org_root.issue_cert(node_bytes, 0, vec![]);
    ch.send_json(&EnrollResponse { org_pub: org_root.public_b64(), cert })
        .map_err(|e| e.to_string())?;
    println!("v2 mesh: enrolled {}", short_id(node_pub));
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_card(ctx: &ServeCtx) -> NodeCard {
    let installed: Vec<String> = ollama::fetch_local(&ctx.ollama_host)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.ollama_name)
        .collect();
    gossip::local_card(
        &ctx.node.public_b64(),
        &ctx.hw,
        &installed,
        ctx.concurrent.load(Ordering::SeqCst),
        ctx.policy.serve.max_concurrent_remote,
    )
}

/// Estimate a job's VRAM as a fraction of this node's total memory pool.
fn vram_fraction(hw: &HardwareInfo, model: &str, ctx: u32) -> f64 {
    let total = total_memory_bytes(hw);
    if total == 0 {
        return 1.0;
    }
    let Some(m) = crate::manage::resolve(model) else { return 0.5 };
    match engine::best_quant(&m, hw, ctx) {
        Some((_, r)) => (r.vram_required as f64 / total as f64).clamp(0.0, 1.0),
        None => 1.0,
    }
}

fn total_memory_bytes(hw: &HardwareInfo) -> u64 {
    if let Some(g) = hw.gpus.iter().find(|g| g.shared_memory) {
        // Unified memory (Apple): usable GPU share.
        let _ = g;
        return (hw.ram_bytes as f64 * 0.75) as u64;
    }
    if let Some(g) = hw.gpus.iter().find(|g| !g.shared_memory) {
        return g.vram_bytes;
    }
    hw.ram_bytes
}

fn peer_tokens_last_hour(ctx: &ServeCtx, peer_pub: &str) -> u64 {
    let now = now_unix();
    let mut q = match ctx.quota.lock() {
        Ok(q) => q,
        Err(_) => return 0,
    };
    if let Some(entries) = q.get_mut(peer_pub) {
        entries.retain(|(ts, _)| now.saturating_sub(*ts) < QUOTA_WINDOW);
        entries.iter().map(|(_, t)| t).sum()
    } else {
        0
    }
}

fn record_served(ctx: &ServeCtx, peer_pub: &str, model: &str, tokens_in: u64, tokens_out: u64, duration_ms: u64) {
    if tokens_in == 0 && tokens_out == 0 {
        return;
    }
    let now = now_unix();
    if let Ok(mut q) = ctx.quota.lock() {
        q.entry(peer_pub.to_string()).or_default().push((now, tokens_out));
    }
    usage::append(&UsageRecord {
        ts: now,
        source: short_id(peer_pub),
        kind: "served".into(),
        model: model.to_string(),
        tokens_in,
        tokens_out,
        duration_ms,
    });
}

fn store_receipt(receipt: &Receipt) {
    let Ok(dir) = crate::paths::subdir("mesh/receipts") else { return };
    let name = format!("{}-{}.json", receipt.ts, short_id(&receipt.client_pub));
    if let Ok(raw) = serde_json::to_string_pretty(receipt) {
        let _ = std::fs::write(dir.join(name), raw);
    }
}

fn save_nonces(used: &HashSet<String>) {
    let Ok(dir) = crate::paths::subdir("mesh") else { return };
    let list: Vec<&String> = used.iter().collect();
    if let Ok(raw) = serde_json::to_string(&list) {
        let _ = std::fs::write(dir.join("used_nonces.json"), raw);
    }
}

pub fn load_nonces() -> HashSet<String> {
    let Ok(dir) = crate::paths::subdir("mesh") else { return HashSet::new() };
    let Ok(raw) = std::fs::read_to_string(dir.join("used_nonces.json")) else { return HashSet::new() };
    serde_json::from_str::<Vec<String>>(&raw).map(|v| v.into_iter().collect()).unwrap_or_default()
}

// ── Daemon assembly + cross-process pause ────────────────────────────────────

/// Path of the pause flag file. Its presence means "accept no remote work".
pub fn pause_flag() -> Result<std::path::PathBuf, String> {
    Ok(crate::paths::subdir("mesh").map_err(|e| e.to_string())?.join("paused"))
}

/// Assemble a `ServeCtx` from on-disk identity/policy and start serving on
/// `listen`. Requires membership. Spawns a watcher that mirrors the pause flag
/// file into the shared atomic so `v2 mesh pause` (a different process) is
/// honored within ~1s (H3).
pub fn daemon(ollama_host: &str, hw: Arc<HardwareInfo>, activity: Activity, listen: &str) -> Result<(), String> {
    let node = Arc::new(NodeKey::load_or_create()?);
    let ident = MeshIdentity::load()?
        .ok_or("not a mesh member; run `v2 mesh join <ticket>` or `v2 mesh init`")?;
    let org_pub = ident.org_pub_bytes()?;
    let policy = Policy::load()?;
    let policy_abuse = policy.abuse.clone();

    let paused = Arc::new(AtomicBool::new(false));
    {
        let paused = paused.clone();
        let flag = pause_flag()?;
        std::thread::spawn(move || loop {
            paused.store(flag.exists(), Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(1000));
        });
    }

    let ctx = ServeCtx {
        node,
        org_pub,
        cert: ident.cert.clone(),
        policy,
        ollama_host: ollama_host.trim_end_matches('/').to_string(),
        hw,
        activity,
        paused,
        concurrent: Arc::new(AtomicU32::new(0)),
        used_vram_milli: Arc::new(AtomicU32::new(0)),
        abuse: Arc::new(AbuseControl::new(policy_abuse)),
        quota: Arc::new(Mutex::new(HashMap::new())),
        org_root: OrgRoot::load().ok().map(Arc::new),
        used_nonces: Arc::new(Mutex::new(load_nonces())),
    };
    run(ctx, listen)
}

/// Best-effort AC-power detection. Unknown -> true (most serving nodes are
/// desktops/servers with no battery, and we must not lock them out).
pub fn on_ac_power() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("pmset").args(["-g", "batt"]).output() {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.contains("AC Power") {
                return true;
            }
            if s.contains("Battery Power") {
                return false;
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") {
            let mut saw_ac = false;
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name.starts_with("AC") || name.starts_with("ADP") {
                    saw_ac = true;
                    if let Ok(v) = std::fs::read_to_string(p.join("online")) {
                        if v.trim() == "1" {
                            return true;
                        }
                    }
                }
            }
            if saw_ac {
                return false; // an AC adapter exists and reported offline
            }
        }
    }
    true
}
