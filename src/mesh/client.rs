//! Client side + mesh control surface: enrollment, remote inference, and the
//! admin/member CLI operations. Outbound connections only.

use colored::Colorize;

use crate::hardware::HardwareInfo;

use super::gossip::{NodeCard, PeersFile};
use super::identity::{EnrollTicket, MeshIdentity, NodeKey, OrgRoot, RevocationList};
use super::proto::{CoSign, EnrollResponse, Frame, Receipt, Request};
use super::transport::{self, Peer};
use super::{b64, short_id, unb64_arr};
use crate::usage::now_unix;

// ── Admin: create an org ─────────────────────────────────────────────────────

pub fn init() -> Result<(), String> {
    let node = NodeKey::load_or_create()?;
    let org = OrgRoot::create()?;
    // The admin is also a member: self-issue a cert.
    let cert = org.issue_cert(node.public_bytes(), 0, vec![]);
    MeshIdentity { org_pub: org.public_b64(), cert }.save()?;
    println!("{}", "mesh initialised".green());
    println!("  org id   {}", short_id(&org.public_b64()));
    println!("  node id  {}", short_id(&node.public_b64()));
    println!("  you are the admin. Invite others with `v2 mesh invite <your-host:port>`.");
    Ok(())
}

pub fn invite(addr: &str, ttl_secs: u64) -> Result<(), String> {
    let org = OrgRoot::load()?;
    let ticket = org.make_ticket(addr, ttl_secs)?;
    println!("{}", "one-time invite ticket (expires in {}h):".replace("{}", &(ttl_secs / 3600).to_string()));
    println!("\n{}\n", ticket.encode());
    println!("Recipient runs:  v2 mesh join <ticket>");
    Ok(())
}

pub fn revoke(node_pub_b64: &str) -> Result<(), String> {
    let org = OrgRoot::load()?;
    let node_bytes = unb64_arr::<32>(node_pub_b64)
        .map_err(|_| "node id must be a base64 ed25519 public key".to_string())?;
    let rev = org.revoke(node_bytes);
    let mut list = RevocationList::load();
    list.add(rev, &org.public_bytes())?;
    list.save()?;
    println!("revoked {} — it will be rejected immediately, and its cert expires within 24h.", short_id(node_pub_b64));
    Ok(())
}

// ── Federation: trust another org with a scope ───────────────────────────────

pub fn federation_add(org_pub: &str, note: &str, models: &[String]) -> Result<(), String> {
    unb64_arr::<32>(org_pub).map_err(|_| "org id must be a base64 ed25519 public key".to_string())?;
    let mut fed = super::identity::FederationList::load();
    if fed.orgs.iter().any(|o| o.org_pub == org_pub) {
        return Err("that org is already federated".into());
    }
    let allowed = if models.is_empty() { vec![] } else { models.to_vec() };
    fed.orgs.push(super::identity::FederatedOrg {
        org_pub: org_pub.to_string(),
        note: note.to_string(),
        allowed_models: allowed.clone(),
    });
    fed.save()?;
    println!("federated org {} (scope: {})", short_id(org_pub),
        if allowed.is_empty() { "none — nothing allowed until you set --models".to_string() } else { allowed.join(", ") });
    Ok(())
}

pub fn federation_list() -> Result<(), String> {
    let fed = super::identity::FederationList::load();
    if fed.orgs.is_empty() {
        println!("v2 mesh federation  none");
        return Ok(());
    }
    println!("v2 mesh federation  {} orgs", fed.orgs.len());
    for o in fed.orgs {
        let scope = if o.allowed_models.is_empty() { "(no models)".to_string() } else { o.allowed_models.join(", ") };
        println!("  {}  {}  scope: {}", short_id(&o.org_pub), o.note, scope);
    }
    Ok(())
}

// ── Member: join an org ──────────────────────────────────────────────────────

pub fn join(ticket_str: &str) -> Result<(), String> {
    let node = NodeKey::load_or_create()?;
    let ticket = EnrollTicket::decode(ticket_str)?;
    let org_pub = ticket.verify(now_unix())?; // sig + expiry
    let addr = ticket.addr.clone();
    let revs = RevocationList::load();

    let (mut ch, _peer) = transport::connect_enroll(&addr, &node, ticket, &org_pub, &revs)?;
    let resp: EnrollResponse = ch.recv_json().map_err(|e| format!("enrollment failed: {e}"))?;

    // The issued cert must be for us and signed by the org in the ticket.
    let cert_org = unb64_arr::<32>(&resp.org_pub)?;
    if cert_org != org_pub {
        return Err("enrollment response signed by unexpected org".into());
    }
    resp.cert.verify(&org_pub, now_unix())?;
    if resp.cert.node_pub != node.public_b64() {
        return Err("issued cert is for a different node".into());
    }

    MeshIdentity { org_pub: resp.org_pub, cert: resp.cert }.save()?;
    let mut peers = PeersFile::load();
    peers.add(&addr);
    peers.save()?;

    println!("{}", "joined the mesh".green());
    println!("  node id  {}", short_id(&node.public_b64()));
    println!("  admin at {addr} added as a peer.");
    println!("  run remotely:  v2 mesh run <model>   ·   share yours:  v2 serve --mesh-listen 0.0.0.0:4830");
    Ok(())
}

// ── Status / peers ───────────────────────────────────────────────────────────

pub fn status() -> Result<(), String> {
    let node = NodeKey::load_or_create()?;
    let peers = PeersFile::load();
    let mut rows = vec![("node".to_string(), short_id(&node.public_b64()))];
    match MeshIdentity::load()? {
        None => rows.push((
            "member".into(),
            format!("{}  — run `v2 mesh init` or `v2 mesh join`", "no".yellow()),
        )),
        Some(ident) => {
            let remaining = ident.cert.expiry.saturating_sub(now_unix()) / 3600;
            let admin = OrgRoot::load().is_ok();
            rows.push(("org".into(), short_id(&ident.org_pub)));
            rows.push(("role".into(), if admin { "admin".green().to_string() } else { "member".to_string() }));
            rows.push(("cert".into(), format!("valid {remaining}h")));
        }
    }
    rows.push(("peers".into(), peers.peers.len().to_string()));
    crate::ui::panel("mesh status", &rows);
    for p in peers.peers {
        println!("  · {}", p.addr.dimmed());
    }
    Ok(())
}

pub fn peer_add(addr: &str) -> Result<(), String> {
    let mut peers = PeersFile::load();
    peers.add(addr);
    peers.save()?;
    println!("added peer {addr}");
    Ok(())
}

/// Fetch and print each peer's node card (pull-based discovery).
pub fn peers() -> Result<(), String> {
    let cards = collect_cards()?;
    if cards.is_empty() {
        println!("v2 mesh peers  none reachable  (add with `v2 mesh peer add host:port`)");
        return Ok(());
    }
    crate::ui::section(&format!("mesh peers  ({} reachable)", cards.len()));
    for (addr, card) in &cards {
        println!(
            "  {:<22} {:<10} {:>5.0}G  {:>4.0} GB/s  {}/{} busy  {} models",
            addr,
            short_id(&card.node_pub),
            card.vram_gb,
            card.bandwidth_gbps,
            card.concurrent,
            card.max_concurrent,
            card.models.len(),
        );
    }
    Ok(())
}

fn collect_cards() -> Result<Vec<(String, NodeCard)>, String> {
    let node = NodeKey::load_or_create()?;
    let ident = MeshIdentity::load()?.ok_or("not a mesh member")?;
    let org_pub = ident.org_pub_bytes()?;
    let revs = RevocationList::load();
    let peers = PeersFile::load();

    let mut out = vec![];
    for p in peers.peers {
        match transport::connect_member(&p.addr, &node, ident.cert.clone(), &org_pub, &revs) {
            Ok((mut ch, _)) => {
                if ch.send_json(&Request::Card).is_ok() {
                    if let Ok(Frame::Card { card }) = ch.recv_json::<Frame>() {
                        out.push((p.addr.clone(), card));
                    }
                }
            }
            Err(e) => eprintln!("  {} unreachable: {}", p.addr, e),
        }
    }
    Ok(out)
}

// ── Pause / resume (cross-process reclaim, H3) ──────────────────────────────

pub fn pause() -> Result<(), String> {
    let path = super::serve::pause_flag()?;
    std::fs::write(&path, b"paused").map_err(|e| e.to_string())?;
    println!("{} — accepting no remote work; in-flight jobs are being cancelled.", "paused".yellow());
    Ok(())
}

pub fn resume() -> Result<(), String> {
    let path = super::serve::pause_flag()?;
    let _ = std::fs::remove_file(&path);
    println!("{} — offering compute to the mesh again.", "resumed".green());
    Ok(())
}

// ── Remote inference ─────────────────────────────────────────────────────────

/// Score a peer for a request: higher is better; `None` if it can't serve.
fn score(card: &NodeCard, model: &str) -> Option<f64> {
    if !card.serves_model(model) || !card.has_capacity() {
        return None;
    }
    Some(card.bandwidth_gbps)
}

/// Run `model` on the best available org peer, streaming the reply to stdout.
pub fn remote_run(_hw: &HardwareInfo, model: &str, ctx: u32, prompt: &str) -> Result<(), String> {
    let node = NodeKey::load_or_create()?;
    let ident = MeshIdentity::load()?.ok_or("not a mesh member; run `v2 mesh join <ticket>`")?;
    let org_pub = ident.org_pub_bytes()?;
    let revs = RevocationList::load();
    let peers = PeersFile::load();
    if peers.peers.is_empty() {
        return Err("no known peers; add one with `v2 mesh peer add host:port`".into());
    }

    // Rank peers by card, best first; peers we can't card-check go last.
    let cards = collect_cards().unwrap_or_default();
    let mut ranked: Vec<(String, f64)> = cards
        .iter()
        .filter_map(|(addr, c)| score(c, model).map(|s| (addr.clone(), s)))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut order: Vec<String> = ranked.into_iter().map(|(a, _)| a).collect();
    for p in &peers.peers {
        if !order.contains(&p.addr) {
            order.push(p.addr.clone());
        }
    }

    let messages = serde_json::json!([{ "role": "user", "content": prompt }]);

    for addr in order {
        match try_peer(&addr, &node, &ident, &org_pub, &revs, model, ctx, &messages) {
            Ok(true) => return Ok(()),         // served
            Ok(false) => continue,              // refused/queued: try next
            Err(e) => eprintln!("  {addr}: {e}"),
        }
    }
    Err(format!("no peer could serve {model} right now"))
}

#[allow(clippy::too_many_arguments)]
fn try_peer(
    addr: &str,
    node: &NodeKey,
    ident: &MeshIdentity,
    org_pub: &[u8; 32],
    revs: &RevocationList,
    model: &str,
    ctx: u32,
    messages: &serde_json::Value,
) -> Result<bool, String> {
    let (mut ch, peer) = transport::connect_member(addr, node, ident.cert.clone(), org_pub, revs)?;
    let server_pub = match &peer {
        Peer::Member { node_pub, .. } => node_pub.clone(),
        _ => return Err("peer is not a member".into()),
    };

    ch.send_json(&Request::Chat { model: model.to_string(), ctx, messages: messages.clone() })
        .map_err(|e| e.to_string())?;

    println!("{} {} via {}", "»".cyan(), model.bold(), short_id(&server_pub).dimmed());
    loop {
        let frame: Frame = ch.recv_json().map_err(|e| e.to_string())?;
        match frame {
            Frame::Accepted => {}
            Frame::Token { c } => {
                print!("{c}");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            Frame::Queued { reason } | Frame::Refused { reason } => {
                println!("{}", format!("  {addr} declined: {reason}").dimmed());
                return Ok(false);
            }
            Frame::Error { reason } => {
                println!();
                return Err(reason);
            }
            Frame::Done { tokens_out, duration_ms, receipt, .. } => {
                println!();
                let tps = if duration_ms > 0 { tokens_out as f64 / (duration_ms as f64 / 1000.0) } else { 0.0 };
                println!("{}", format!("  {tokens_out} tok · {tps:.0} tok/s · {}", short_id(&server_pub)).dimmed());
                cosign_and_store(node, &mut ch, receipt);
                return Ok(true);
            }
            _ => {}
        }
    }
}

fn cosign_and_store(node: &NodeKey, ch: &mut super::transport::Channel, mut receipt: Receipt) {
    // Co-sign the receipt so both sides hold a tamper-evident record (H5).
    let sig = b64(&node.sign(&receipt.signing_bytes()));
    receipt.client_sig = sig.clone();
    let _ = ch.send_json(&CoSign { client_sig: sig });
    if let Ok(dir) = crate::paths::subdir("mesh/receipts") {
        let name = format!("{}-used-{}.json", receipt.ts, short_id(&receipt.server_pub));
        if let Ok(raw) = serde_json::to_string_pretty(&receipt) {
            let _ = std::fs::write(dir.join(name), raw);
        }
    }
    // Record what we consumed elsewhere.
    crate::usage::append(&crate::usage::UsageRecord {
        ts: receipt.ts,
        source: short_id(&receipt.server_pub),
        kind: "used".into(),
        model: receipt.model,
        tokens_in: receipt.tokens_in,
        tokens_out: receipt.tokens_out,
        duration_ms: 0,
    });
}
