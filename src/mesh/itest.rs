//! Two-node integration test: drives the real transport, enrollment, admission,
//! streaming, receipts, and revocation over loopback sockets against a mock
//! Ollama. This is what proves the mesh actually works, not just compiles.
//!
//! Everything runs in one `#[test]` so the process-global `HOME` (pointed at a
//! temp dir) is set once and used sequentially.

use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::activity::Activity;
use crate::hardware::{GpuInfo, HardwareInfo, Os, Vendor};
use crate::policy::Policy;
use crate::usage::now_unix;

use super::identity::{NodeKey, OrgRoot, RevocationList};
use super::proto::{CoSign, EnrollResponse, Frame, Request};
use super::serve::{self, ServeCtx};
use super::transport;

// These tests each point the process-global HOME at a temp dir, so they must not
// run concurrently. This lock serialises them.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_temp_home() {
    let dir = std::env::temp_dir().join(format!("v2-itest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HOME", &dir);
}

/// A mock Ollama that streams tokens *slowly* (a delay per read), so a test can
/// reclaim the job mid-generation.
fn mock_ollama_slow() -> String {
    struct SlowReader {
        lines: Vec<Vec<u8>>,
        i: usize,
        buf: Vec<u8>,
    }
    impl std::io::Read for SlowReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.buf.is_empty() {
                if self.i >= self.lines.len() {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(120));
                self.buf = self.lines[self.i].clone();
                self.i += 1;
            }
            let n = self.buf.len().min(out.len());
            out[..n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            Ok(n)
        }
    }
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let url = format!("http://{addr}");
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            if req.url().contains("/api/tags") {
                let _ = req.respond(tiny_http::Response::from_string(r#"{"models":[]}"#));
                continue;
            }
            let mut lines: Vec<Vec<u8>> = (0..8)
                .map(|i| format!("{{\"message\":{{\"content\":\" t{i}\"}},\"done\":false}}\n").into_bytes())
                .collect();
            lines.push(
                br#"{"message":{"content":""},"done":true,"prompt_eval_count":5,"eval_count":8,"total_duration":1}"#
                    .to_vec(),
            );
            let reader = SlowReader { lines, i: 0, buf: Vec::new() };
            let resp = tiny_http::Response::new(tiny_http::StatusCode(200), vec![], reader, None, None);
            let _ = req.respond(resp);
        }
    });
    url
}

/// A mock Ollama that streams a fixed chat reply. Returns its base URL.
fn mock_ollama() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let url = format!("http://{addr}");
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let body = if req.url().contains("/api/tags") {
                r#"{"models":[]}"#.to_string()
            } else {
                // /api/chat: three content deltas then a final stats line.
                [
                    r#"{"message":{"content":"Hello"},"done":false}"#,
                    r#"{"message":{"content":" from"},"done":false}"#,
                    r#"{"message":{"content":" the mesh"},"done":false}"#,
                    r#"{"message":{"content":""},"done":true,"prompt_eval_count":9,"eval_count":3,"total_duration":5000000}"#,
                ]
                .join("\n")
            };
            let _ = req.respond(tiny_http::Response::from_string(body));
        }
    });
    url
}

fn test_policy() -> Policy {
    let mut p = Policy::default();
    p.serve.allowed_models = vec!["*".into()];
    p.serve.max_ctx = 131_072;
    p.serve.max_vram_fraction = 1.0;
    p.serve.max_concurrent_remote = 4;
    p.quota.per_peer_tokens_per_hour = 1_000_000_000;
    // Deterministic regardless of the test machine's power/time/activity.
    p.availability.require_ac_power = false;
    p.availability.yield_to_local = false;
    p.availability.hours = "always".into();
    p
}

fn test_hw() -> HardwareInfo {
    HardwareInfo {
        gpus: vec![GpuInfo {
            name: "RTX 4090".into(),
            vendor: Vendor::Nvidia,
            vram_bytes: 24 << 30,
            shared_memory: false,
        }],
        cpu_name: "test".into(),
        ram_bytes: 64 << 30,
        os: Os::Linux,
    }
}

#[test]
fn mesh_end_to_end() {
    let _g = lock();
    set_temp_home();

    // Org + an admin node (admin can both enroll members and serve inference).
    let org = OrgRoot::from_seed([1u8; 32]);
    let org_pub = org.public_bytes();
    let server_node = NodeKey::from_seed([2u8; 32]);
    let server_cert = org.issue_cert(server_node.public_bytes(), 0, vec![]);

    let ollama = mock_ollama();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    let ctx = ServeCtx {
        node: Arc::new(server_node),
        org_pub,
        cert: server_cert,
        policy: test_policy(),
        ollama_host: ollama,
        hw: Arc::new(test_hw()),
        activity: Activity::new(),
        paused: Arc::new(AtomicBool::new(false)),
        concurrent: Arc::new(AtomicU32::new(0)),
        used_vram_milli: Arc::new(AtomicU32::new(0)),
        abuse: Arc::new(super::abuse::AbuseControl::new({
            let mut a = test_policy().abuse;
            a.strike_limit = 3; // ban after 3 refusals, for the strike test below
            a
        })),
        quota: Arc::new(Mutex::new(HashMap::new())),
        org_root: Some(Arc::new(OrgRoot::from_seed([1u8; 32]))),
        used_nonces: Arc::new(Mutex::new(HashSet::new())),
    };
    std::thread::spawn(move || serve::serve_loop(ctx, listener));

    let client = NodeKey::from_seed([3u8; 32]);
    let no_revs = RevocationList::default();

    // ── 1. Enrollment: ticket in, membership cert out ────────────────────────
    let ticket = org.make_ticket(&addr, 3600).unwrap();
    let (mut ch, _admin) =
        transport::connect_enroll(&addr, &client, ticket, &org_pub, &no_revs).expect("enroll handshake");
    let resp: EnrollResponse = ch.recv_json().expect("enroll response");
    assert_eq!(resp.cert.node_pub, client.public_b64(), "cert issued for the joining node");
    assert!(resp.cert.verify(&org_pub, now_unix()).is_ok(), "issued cert must verify");
    let client_cert = resp.cert;
    drop(ch);

    // ── 2. Remote inference: request in, streamed tokens + signed receipt out ─
    let (mut ch, _peer) =
        transport::connect_member(&addr, &client, client_cert.clone(), &org_pub, &no_revs).expect("member connect");
    ch.send_json(&Request::Chat {
        model: "qwen3:0.6b".into(),
        ctx: 2048,
        messages: serde_json::json!([{ "role": "user", "content": "hi" }]),
    })
    .unwrap();

    let mut text = String::new();
    let mut tokens = (0u64, 0u64);
    let mut receipt_verified = false;
    loop {
        match ch.recv_json::<Frame>().expect("frame") {
            Frame::Accepted => {}
            Frame::Token { c } => text.push_str(&c),
            Frame::Done { tokens_in, tokens_out, receipt, .. } => {
                tokens = (tokens_in, tokens_out);
                receipt_verified = receipt.verify().0;
                let sig = super::b64(&client.sign(&receipt.signing_bytes()));
                let _ = ch.send_json(&CoSign { client_sig: sig });
                break;
            }
            Frame::Error { reason } => panic!("server returned error: {reason}"),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(text, "Hello from the mesh", "streamed content round-trips");
    assert_eq!(tokens, (9, 3), "exact token counts from the stream");
    assert!(receipt_verified, "server's receipt signature must verify");
    drop(ch);

    // ── 3. Receipt persisted and dual-signed ─────────────────────────────────
    let receipts_dir = crate::paths::subdir("mesh/receipts").unwrap();
    let mut receipt_file = None;
    for _ in 0..60 {
        if let Some(entry) = std::fs::read_dir(&receipts_dir).ok().and_then(|mut r| r.next()) {
            receipt_file = entry.ok().map(|e| e.path());
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let path = receipt_file.expect("server persisted a receipt");
    let stored: super::proto::Receipt =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let (server_ok, client_ok) = stored.verify();
    assert!(server_ok && client_ok, "stored receipt is dual-signed and valid");

    // ── 4. Repeat refusals earn a temporary ban (abuse control) ──────────────
    {
        let (mut ch, _p) =
            transport::connect_member(&addr, &client, client_cert.clone(), &org_pub, &no_revs).expect("member connect");
        let over_ctx = serde_json::json!([{ "role": "user", "content": "x" }]);
        let mut last = String::new();
        for _ in 0..4 {
            // ctx above max_ctx is refused; after 3 strikes the node is banned.
            ch.send_json(&Request::Chat { model: "qwen3:0.6b".into(), ctx: 999_999, messages: over_ctx.clone() })
                .unwrap();
            match ch.recv_json::<Frame>().expect("frame") {
                Frame::Refused { reason } => last = reason,
                other => panic!("expected refusal, got {other:?}"),
            }
        }
        assert!(last.contains("banned"), "repeated refusals must earn a ban, got: {last}");
        drop(ch);
    }

    // ── 5. Revocation takes effect without restarting the daemon ─────────────
    let mut list = RevocationList::load();
    list.add(org.revoke(client.public_bytes()), &org_pub).unwrap();
    list.save().unwrap();
    let rejected = transport::connect_member(&addr, &client, client_cert, &org_pub, &no_revs);
    assert!(rejected.is_err(), "revoked node must be rejected on its next connection");
}

/// Owner reclaim terminates an in-flight generation mid-stream (deadman / H3):
/// the client gets some tokens, then a preemption error — not a completion.
#[test]
fn preemption_terminates_inflight() {
    let _g = lock();
    set_temp_home();

    let org = OrgRoot::from_seed([4u8; 32]);
    let org_pub = org.public_bytes();
    let server_node = NodeKey::from_seed([5u8; 32]);
    let server_cert = org.issue_cert(server_node.public_bytes(), 0, vec![]);
    let client = NodeKey::from_seed([6u8; 32]);
    let client_cert = org.issue_cert(client.public_bytes(), 0, vec![]);

    let ollama = mock_ollama_slow();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    let paused = Arc::new(AtomicBool::new(false));
    let ctx = ServeCtx {
        node: Arc::new(server_node),
        org_pub,
        cert: server_cert,
        policy: test_policy(),
        ollama_host: ollama,
        hw: Arc::new(test_hw()),
        activity: Activity::new(),
        paused: paused.clone(),
        concurrent: Arc::new(AtomicU32::new(0)),
        used_vram_milli: Arc::new(AtomicU32::new(0)),
        abuse: Arc::new(super::abuse::AbuseControl::new(test_policy().abuse)),
        quota: Arc::new(Mutex::new(HashMap::new())),
        org_root: None,
        used_nonces: Arc::new(Mutex::new(HashSet::new())),
    };
    std::thread::spawn(move || serve::serve_loop(ctx, listener));

    let no_revs = RevocationList::default();
    let (mut ch, _p) =
        transport::connect_member(&addr, &client, client_cert, &org_pub, &no_revs).expect("member connect");
    ch.send_json(&Request::Chat {
        model: "qwen3:0.6b".into(),
        ctx: 2048,
        messages: serde_json::json!([{ "role": "user", "content": "hi" }]),
    })
    .unwrap();

    // The owner reclaims the machine partway through the (slow) generation.
    let p = paused.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        p.store(true, Ordering::SeqCst);
    });

    let mut tokens = 0;
    let mut preempted = false;
    loop {
        match ch.recv_json::<Frame>() {
            Ok(Frame::Accepted) => {}
            Ok(Frame::Token { .. }) => tokens += 1,
            Ok(Frame::Error { reason }) => {
                assert!(reason.contains("preempted"), "expected a preemption, got: {reason}");
                preempted = true;
                break;
            }
            Ok(Frame::Done { .. }) => panic!("job completed but the owner reclaimed mid-stream"),
            Ok(other) => panic!("unexpected frame: {other:?}"),
            Err(e) => panic!("connection died before the preemption frame: {e}"),
        }
    }
    assert!(preempted, "in-flight job must be terminated by owner reclaim");
    assert!(tokens < 9, "should be cut off before all 9 tokens, got {tokens}");
}
