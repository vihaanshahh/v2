//! Relay: an org-agnostic rendezvous that lets two nodes connect without either
//! one exposing an inbound IP:port.
//!
//! **Zero-trust by construction.** The relay only splices two TCP streams and
//! copies bytes. The Noise_XX handshake and channel-bound cert exchange in
//! `transport.rs` run *end-to-end through* the splice, so the relay sees only
//! ciphertext: it cannot read content (no session key), impersonate a node (no
//! node key), or forge a cert (no org key). It is deliberately unaware of orgs —
//! trust is enforced entirely at the endpoints (DESIGN.md §5, fail-closed I2).
//!
//! Addressing switches from `host:port` to `relay://<relay-addr>/<node_pub>`:
//! you dial a *public key*, never an address. Both nodes dial *out* to the relay
//! and stay connected, so neither needs an open inbound port (NAT-friendly) and
//! raw IPs never appear in tickets or peer lists.
//!
//! Wire dance (all pre-Noise framing is plaintext JSON; everything after the
//! splice is opaque):
//! ```text
//!   server ─Register(node_pub) ─► relay      relay ─Challenge(nonce)─► server
//!   server ─Proof(sig over nonce)► relay      relay ─Registered──────► server   (control conn parked)
//!   client ─Connect(node_pub) ──► relay      relay ─Dial(session)───► server   (down the control conn)
//!   server ─Accept(session) ────► relay      relay ─Go──────────────► both, then splices raw
//! ```
//! Registration is signed so nobody can squat another node's pubkey slot and
//! intercept/deny its inbound sessions (griefing). Content trust is still 100%
//! end-to-end regardless.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::identity::NodeKey;
use super::{b64, unb64, unb64_arr};

/// Domain-separates the registration proof signature from every other signature
/// a node key ever makes (certs, receipts, channel binding).
const REGISTER_DOMAIN: &[u8] = b"v2-relay-register-v1";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Applied to the short plaintext control exchange (defeats slowloris on the relay).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
/// A parked client with no matching `Accept` is reaped after this long.
const PARK_TTL: Duration = Duration::from_secs(20);
/// Control frames are tiny; reject anything that isn't.
const MAX_CONTROL_FRAME: usize = 4096;

// ── Plaintext control protocol (framed JSON, u32-BE length prefix) ────────────

/// Endpoint -> relay.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
enum Hello {
    /// A serving node parks a control connection, keyed by its node id.
    Register { node_pub: String },
    /// The registrant proves it holds the private key for `node_pub`.
    Proof { sig: String },
    /// A caller asks to be connected to `node_pub`.
    Connect { node_pub: String },
    /// The serving node dials back to fulfil a specific session.
    Accept { session: String },
}

/// Relay -> endpoint.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
enum Reply {
    /// Sign these bytes to prove key ownership.
    Challenge { nonce: String },
    /// Registration accepted; the control connection is now live.
    Registered,
    /// A caller arrived — open a fresh `Accept(session)` connection.
    Dial { session: String },
    /// No node is registered under that id right now.
    NoRoute,
    /// The pipe is end-to-end; start speaking Noise now.
    Go,
    /// Something went wrong; the connection will close.
    Error { reason: String },
}

fn write_frame(s: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    s.write_all(&(data.len() as u32).to_be_bytes())?;
    s.write_all(data)?;
    s.flush()
}

fn read_frame(s: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_CONTROL_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "control frame too large"));
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

fn send<T: Serialize>(s: &mut TcpStream, v: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(s, &bytes)
}

fn recv<T: for<'de> Deserialize<'de>>(s: &mut TcpStream) -> io::Result<T> {
    let bytes = read_frame(s)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn rand_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| format!("rng failure: {e}"))?;
    Ok(b64(&b))
}

// ── relay:// route addressing ────────────────────────────────────────────────

/// A peer/ticket address is either a direct socket address or a relay route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct(String),
    Relay { relay: String, node_pub: String },
}

/// Parse an address. `relay://<relay-addr>/<node_pub>` is a mediated route;
/// anything else is a direct `host:port`. `node_pub` is base64 and never
/// contains `/` after url-safe callers, so we split on the *last* `/`.
pub fn parse_route(addr: &str) -> Route {
    if let Some(rest) = addr.strip_prefix("relay://") {
        if let Some((relay, node_pub)) = rest.rsplit_once('/') {
            if !relay.is_empty() && !node_pub.is_empty() {
                return Route::Relay { relay: relay.to_string(), node_pub: node_pub.to_string() };
            }
        }
    }
    Route::Direct(addr.to_string())
}

/// Build a relay route string for a node reachable via `relay`.
pub fn make_route(relay: &str, node_pub: &str) -> String {
    format!("relay://{relay}/{node_pub}")
}

// ── Dialing (client side) ────────────────────────────────────────────────────

fn connect(addr: &str) -> Result<TcpStream, String> {
    let mut last = String::from("no address resolved");
    let addrs = addr.to_socket_addrs().map_err(|e| format!("resolve {addr}: {e}"))?;
    for sa in addrs {
        match TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("connect {addr}: {last}"))
}

/// Open a mediated connection to `node_pub` through `relay`. Returns a stream
/// that is spliced end-to-end to the target; the caller then runs the normal
/// Noise handshake on it exactly as for a direct dial.
pub fn dial_via_relay(relay: &str, node_pub: &str) -> Result<TcpStream, String> {
    let mut s = connect(relay)?;
    s.set_read_timeout(Some(CONTROL_TIMEOUT)).ok();
    s.set_write_timeout(Some(CONTROL_TIMEOUT)).ok();

    send(&mut s, &Hello::Connect { node_pub: node_pub.to_string() })
        .map_err(|e| format!("relay connect: {e}"))?;

    // Blocks until the server dials back and the relay splices us (`Go`), or the
    // relay reports there is no such registered node.
    match recv::<Reply>(&mut s) {
        Ok(Reply::Go) => {
            // Clear the short control timeout — the Noise session may be long.
            s.set_read_timeout(None).ok();
            s.set_write_timeout(None).ok();
            Ok(s)
        }
        Ok(Reply::NoRoute) => Err(format!("relay: node {} not reachable (not registered)", super::short_id(node_pub))),
        Ok(Reply::Error { reason }) => Err(format!("relay: {reason}")),
        Ok(_) => Err("relay: unexpected reply".into()),
        Err(e) => Err(format!("relay: {e}")),
    }
}

// ── Registration (serving side) ──────────────────────────────────────────────

/// Register `node` at `relay` and serve inbound sessions forever. For each
/// caller the relay splices to us, `on_session` is invoked (on its own thread)
/// with a ready stream — feed it straight to `transport::accept`.
///
/// Reconnects with a fixed backoff if the control connection drops, so a relay
/// restart doesn't permanently unlink the node.
pub fn register<F>(relay: &str, node: Arc<NodeKey>, on_session: F) -> Result<(), String>
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    let on_session = Arc::new(on_session);
    loop {
        if let Err(e) = register_once(relay, &node, &on_session) {
            eprintln!("v2 relay: control link to {relay} lost: {e}; retrying in 5s");
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

fn register_once<F>(relay: &str, node: &Arc<NodeKey>, on_session: &Arc<F>) -> Result<(), String>
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    let mut ctrl = connect(relay)?;
    ctrl.set_read_timeout(Some(CONTROL_TIMEOUT)).ok();
    ctrl.set_write_timeout(Some(CONTROL_TIMEOUT)).ok();

    send(&mut ctrl, &Hello::Register { node_pub: node.public_b64() }).map_err(|e| e.to_string())?;
    let nonce = match recv::<Reply>(&mut ctrl).map_err(|e| e.to_string())? {
        Reply::Challenge { nonce } => nonce,
        Reply::Error { reason } => return Err(reason),
        _ => return Err("expected challenge".into()),
    };
    let nonce_bytes = unb64(&nonce)?;
    let sig = node.sign(&register_signing_bytes(&nonce_bytes));
    send(&mut ctrl, &Hello::Proof { sig: b64(&sig) }).map_err(|e| e.to_string())?;
    match recv::<Reply>(&mut ctrl).map_err(|e| e.to_string())? {
        Reply::Registered => {}
        Reply::Error { reason } => return Err(reason),
        _ => return Err("registration refused".into()),
    }

    // The control link is idle except for `Dial` pushes, which may be far apart —
    // drop the read timeout so we block waiting for callers.
    ctrl.set_read_timeout(None).ok();
    println!("v2 relay: registered at {relay} as {}", super::short_id(&node.public_b64()));

    loop {
        match recv::<Reply>(&mut ctrl) {
            Ok(Reply::Dial { session }) => {
                let relay = relay.to_string();
                let on_session = on_session.clone();
                std::thread::spawn(move || {
                    if let Err(e) = accept_session(&relay, &session, &on_session) {
                        eprintln!("v2 relay: session {}: {e}", super::short_id(&session));
                    }
                });
            }
            Ok(_) => {} // ignore anything unexpected on the control link
            Err(e) => return Err(e.to_string()), // link dropped -> outer loop reconnects
        }
    }
}

fn accept_session<F>(relay: &str, session: &str, on_session: &Arc<F>) -> Result<(), String>
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    let mut s = connect(relay)?;
    s.set_read_timeout(Some(CONTROL_TIMEOUT)).ok();
    s.set_write_timeout(Some(CONTROL_TIMEOUT)).ok();
    send(&mut s, &Hello::Accept { session: session.to_string() }).map_err(|e| e.to_string())?;
    match recv::<Reply>(&mut s).map_err(|e| e.to_string())? {
        Reply::Go => {}
        Reply::Error { reason } => return Err(reason),
        _ => return Err("relay declined session".into()),
    }
    s.set_read_timeout(None).ok();
    s.set_write_timeout(None).ok();
    on_session(s);
    Ok(())
}

fn register_signing_bytes(nonce: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(REGISTER_DOMAIN.len() + nonce.len());
    m.extend_from_slice(REGISTER_DOMAIN);
    m.extend_from_slice(nonce);
    m
}

// ── The relay server ─────────────────────────────────────────────────────────

/// A parked caller awaiting its server dial-back, with the time it was parked.
struct Parked {
    stream: TcpStream,
    since: Instant,
}

#[derive(Default)]
struct RelayState {
    /// node_pub -> a write handle on that node's control connection.
    controls: HashMap<String, TcpStream>,
    /// session id -> the caller waiting to be spliced.
    pending: HashMap<String, Parked>,
}

/// Run a relay server on `listen` until the process stops. Blocks.
pub fn run_relay(listen: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    println!("v2 relay: rendezvous listening on {listen} (zero-trust; forwards ciphertext only)");
    let state = Arc::new(Mutex::new(RelayState::default()));
    spawn_reaper(state.clone());

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || handle_conn(state, stream));
    }
    Ok(())
}

/// Periodically drop parked callers whose server never dialed back, so a caller
/// blocked on `Go` fails instead of hanging forever and the map can't grow.
fn spawn_reaper(state: Arc<Mutex<RelayState>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        if let Ok(mut st) = state.lock() {
            let expired: Vec<String> = st
                .pending
                .iter()
                .filter(|(_, p)| p.since.elapsed() > PARK_TTL)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired {
                if let Some(p) = st.pending.remove(&k) {
                    let _ = p.stream.shutdown(Shutdown::Both); // unblocks the caller
                }
            }
        }
    });
}

fn handle_conn(state: Arc<Mutex<RelayState>>, mut stream: TcpStream) {
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(CONTROL_TIMEOUT)).ok();
    stream.set_write_timeout(Some(CONTROL_TIMEOUT)).ok();

    let hello: Hello = match recv(&mut stream) {
        Ok(h) => h,
        Err(_) => return, // not a v2 endpoint; drop
    };
    match hello {
        Hello::Register { node_pub } => handle_register(state, stream, node_pub),
        Hello::Connect { node_pub } => handle_connect(state, stream, node_pub),
        Hello::Accept { session } => handle_accept(state, stream, session),
        Hello::Proof { .. } => {} // only valid mid-registration; ignore stray
    }
}

fn handle_register(state: Arc<Mutex<RelayState>>, mut stream: TcpStream, node_pub: String) {
    // Prove key ownership so nobody can squat another node's slot.
    let nonce = match rand_id() {
        Ok(n) => n,
        Err(_) => return,
    };
    if send(&mut stream, &Reply::Challenge { nonce: nonce.clone() }).is_err() {
        return;
    }
    let proof: Hello = match recv(&mut stream) {
        Ok(p) => p,
        Err(_) => return,
    };
    let Hello::Proof { sig } = proof else { return };
    if !verify_registration(&node_pub, &nonce, &sig) {
        let _ = send(&mut stream, &Reply::Error { reason: "registration proof invalid".into() });
        return;
    }

    // Keep a write handle for pushing `Dial`s; the original detects disconnect.
    let write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    if send(&mut stream, &Reply::Registered).is_err() {
        return;
    }
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        // Last registration wins (e.g. a reconnect after a network blip).
        st.controls.insert(node_pub.clone(), write_half);
    }
    println!("v2 relay: + {}", super::short_id(&node_pub));

    // Block reading the control conn purely to detect disconnect. A registered
    // server never sends more control frames, so any read result means "gone".
    stream.set_read_timeout(None).ok();
    let mut scratch = [0u8; 1];
    let _ = stream.read(&mut scratch);

    // Deregister — but only if we're still the current holder (a fresh reconnect
    // may have replaced us).
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cur) = st.controls.get(&node_pub) {
        if cur.peer_addr().ok() == stream.peer_addr().ok() {
            st.controls.remove(&node_pub);
            println!("v2 relay: - {}", super::short_id(&node_pub));
        }
    }
}

fn handle_connect(state: Arc<Mutex<RelayState>>, mut stream: TcpStream, node_pub: String) {
    let session = match rand_id() {
        Ok(s) => s,
        Err(_) => return,
    };

    // Notify the target's control connection, then park this caller for splicing.
    let notified = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ctrl) = st.controls.get_mut(&node_pub) {
            send(ctrl, &Reply::Dial { session: session.clone() }).is_ok()
        } else {
            false
        }
    };
    if !notified {
        let _ = send(&mut stream, &Reply::NoRoute);
        return;
    }

    // Park; the matching `Accept` (or the reaper) takes it from here. Do not send
    // anything yet — the caller blocks reading `Go`, which the accept path sends.
    stream.set_read_timeout(None).ok();
    stream.set_write_timeout(None).ok();
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.pending.insert(session, Parked { stream, since: Instant::now() });
}

fn handle_accept(state: Arc<Mutex<RelayState>>, mut server: TcpStream, session: String) {
    let caller = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.pending.remove(&session)
    };
    let Some(Parked { stream: mut caller, .. }) = caller else {
        let _ = send(&mut server, &Reply::Error { reason: "unknown or expired session".into() });
        return;
    };

    // Release both endpoints simultaneously, then hand off to a raw byte pump.
    server.set_read_timeout(None).ok();
    server.set_write_timeout(None).ok();
    if send(&mut server, &Reply::Go).is_err() || send(&mut caller, &Reply::Go).is_err() {
        return;
    }
    splice(caller, server);
}

/// Bidirectionally copy between two streams until either side ends, then tear
/// both down. The relay never inspects the bytes — this is the whole point.
fn splice(a: TcpStream, b: TcpStream) {
    let (Ok(mut a_rd), Ok(mut b_wr)) = (a.try_clone(), b.try_clone()) else { return };
    let (mut b_rd, mut a_wr) = (b, a);

    let t = std::thread::spawn(move || {
        let _ = io::copy(&mut a_rd, &mut b_wr);
        let _ = b_wr.shutdown(Shutdown::Both);
        let _ = a_rd.shutdown(Shutdown::Both);
    });
    let _ = io::copy(&mut b_rd, &mut a_wr);
    let _ = a_wr.shutdown(Shutdown::Both);
    let _ = b_rd.shutdown(Shutdown::Both);
    let _ = t.join();
}

fn verify_registration(node_pub: &str, nonce: &str, sig: &str) -> bool {
    let (Ok(pk), Ok(nonce_bytes), Ok(sig_bytes)) =
        (unb64_arr::<32>(node_pub), unb64(nonce), unb64_arr::<64>(sig))
    else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    vk.verify_strict(&register_signing_bytes(&nonce_bytes), &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_parsing() {
        assert_eq!(parse_route("1.2.3.4:4830"), Route::Direct("1.2.3.4:4830".into()));
        assert_eq!(
            parse_route("relay://relay.example:4840/AbCdEf=="),
            Route::Relay { relay: "relay.example:4840".into(), node_pub: "AbCdEf==".into() }
        );
        // Malformed relay routes fall back to Direct rather than erroring.
        assert_eq!(parse_route("relay://oops"), Route::Direct("relay://oops".into()));
    }

    #[test]
    fn round_trips_make_and_parse() {
        let r = make_route("host:9", "PUBKEY");
        assert_eq!(parse_route(&r), Route::Relay { relay: "host:9".into(), node_pub: "PUBKEY".into() });
    }

    /// End-to-end: a server registers at a real relay, a client dials it by
    /// pubkey through the relay, and the full Noise_XX + channel-bound cert auth
    /// completes over the spliced pipe — proving the relay is transparent and the
    /// endpoints still mutually authenticate. The relay only ever sees ciphertext.
    #[test]
    fn member_auth_and_messaging_through_relay() {
        use crate::mesh::identity::{NodeKey, OrgRoot, RevocationList};
        use crate::mesh::transport;
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;

        // One org, two members.
        let org = OrgRoot::from_seed([7u8; 32]);
        let org_pub = org.public_bytes();
        let server = Arc::new(NodeKey::from_seed([8u8; 32]));
        let client = NodeKey::from_seed([9u8; 32]);
        let server_cert = org.issue_cert(server.public_bytes(), 0, vec![]);
        let client_cert = org.issue_cert(client.public_bytes(), 0, vec![]);
        let revs = RevocationList::default();
        let server_pub_b64 = b64(&server.public_bytes());
        let client_pub_b64 = b64(&client.public_bytes());

        // Boot the relay on an ephemeral port.
        let relay_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_addr = relay_listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(RelayState::default()));
        spawn_reaper(state.clone());
        {
            let state = state.clone();
            thread::spawn(move || {
                for stream in relay_listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let state = state.clone();
                    thread::spawn(move || handle_conn(state, stream));
                }
            });
        }

        // Server: register and, for each mediated session, run transport::accept
        // and echo one message. Report the client identity it authenticated.
        let (tx, rx) = mpsc::channel::<String>();
        {
            let (server, org_pub, server_cert) = (server.clone(), org_pub, server_cert.clone());
            let relay_addr = relay_addr.clone();
            thread::spawn(move || {
                let _ = register(&relay_addr, server.clone(), move |stream| {
                    let revs = RevocationList::default();
                    if let Ok((mut ch, peer)) =
                        transport::accept(stream, &server, server_cert.clone(), &org_pub, &revs)
                    {
                        if let Ok(got) = ch.recv_msg() {
                            let _ = ch.send_msg(b"pong");
                            let _ = tx.send(format!("{}|{}", peer.node_pub(), String::from_utf8_lossy(&got)));
                        }
                    }
                });
            });
        }

        // Give registration a moment to land on the relay.
        for _ in 0..50 {
            if state.lock().unwrap().controls.contains_key(&server_pub_b64) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        // Client: dial the server BY PUBKEY through the relay route.
        let route = make_route(&relay_addr, &server_pub_b64);
        let (mut ch, peer) = transport::connect_member(&route, &client, client_cert, &org_pub, &revs)
            .expect("relay-mediated member connect");
        // The client is channel-bound to the server's real identity, via the relay.
        assert_eq!(peer.node_pub(), server_pub_b64);
        ch.send_msg(b"ping").unwrap();
        assert_eq!(ch.recv_msg().unwrap(), b"pong");

        let seen = rx.recv_timeout(Duration::from_secs(5)).expect("server handled the session");
        assert_eq!(seen, format!("{client_pub_b64}|ping"));
    }
}
