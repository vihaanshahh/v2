//! Encrypted, mutually-authenticated channel over blocking TCP.
//!
//! Two layers:
//!   1. **Noise_XX** (snow, pure Rust) gives an end-to-end encrypted channel and
//!      a unique per-session handshake hash. Static keys are ephemeral per
//!      connection — identity is not the Noise key.
//!   2. **Channel binding**: after the handshake each side sends its membership
//!      cert plus an ed25519 signature over the handshake hash. Verifying that
//!      signature proves the peer on *this* channel holds the exact node key the
//!      org authorized. Any failure drops the connection (fail closed, I2).
//!
//! No async, no TLS PKI: one connection = one thread, framed messages.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use serde::{Deserialize, Serialize};

use super::identity::{EnrollTicket, MembershipCert, NodeKey, RevocationList};
use super::{b64, unb64_arr};
use crate::usage::now_unix;

const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// Max plaintext per Noise message (64KiB - 16B tag).
const MAX_CHUNK: usize = 65_519;
/// Reject absurd frames (defends against a peer claiming a huge length).
const MAX_FRAME: usize = 1 << 20;

fn noise_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("noise: {e}"))
}

// ── TCP frame helpers (u32-BE length prefix) ─────────────────────────────────

fn write_frame(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

// ── Encrypted channel ────────────────────────────────────────────────────────

/// An established, encrypted channel. `send_msg`/`recv_msg` move length-delimited
/// application messages; Noise chunking underneath is invisible to callers.
pub struct Channel {
    stream: TcpStream,
    noise: snow::TransportState,
    rbuf: Vec<u8>,
    hash: Vec<u8>,
}

impl Channel {
    pub fn handshake_hash(&self) -> &[u8] {
        &self.hash
    }

    pub fn peer_addr(&self) -> String {
        self.stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into())
    }

    /// Close the underlying socket now. Used by the reclaim path (H3): dropping
    /// the connection aborts any in-flight upstream generation.
    pub fn shutdown(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn recv_chunk(&mut self) -> io::Result<Vec<u8>> {
        let frame = read_frame(&mut self.stream)?;
        let mut out = vec![0u8; frame.len()];
        let n = self.noise.read_message(&frame, &mut out).map_err(noise_err)?;
        out.truncate(n);
        Ok(out)
    }

    fn fill(&mut self, need: usize) -> io::Result<()> {
        while self.rbuf.len() < need {
            let chunk = self.recv_chunk()?;
            if chunk.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
            }
            self.rbuf.extend_from_slice(&chunk);
        }
        Ok(())
    }

    pub fn recv_msg(&mut self) -> io::Result<Vec<u8>> {
        self.fill(4)?;
        let len = u32::from_be_bytes(self.rbuf[..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
        }
        self.fill(4 + len)?;
        let msg = self.rbuf[4..4 + len].to_vec();
        self.rbuf.drain(..4 + len);
        Ok(msg)
    }

    pub fn send_msg(&mut self, data: &[u8]) -> io::Result<()> {
        let mut framed = Vec::with_capacity(4 + data.len());
        framed.extend_from_slice(&(data.len() as u32).to_be_bytes());
        framed.extend_from_slice(data);
        for chunk in framed.chunks(MAX_CHUNK) {
            let mut ct = vec![0u8; chunk.len() + 16];
            let n = self.noise.write_message(chunk, &mut ct).map_err(noise_err)?;
            ct.truncate(n);
            write_frame(&mut self.stream, &ct)?;
        }
        Ok(())
    }

    pub fn send_json<T: Serialize>(&mut self, v: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(v).map_err(noise_err)?;
        self.send_msg(&bytes)
    }

    pub fn recv_json<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<T> {
        let bytes = self.recv_msg()?;
        serde_json::from_slice(&bytes).map_err(noise_err)
    }
}

// ── Handshake ────────────────────────────────────────────────────────────────

fn handshake(stream: &mut TcpStream, initiator: bool) -> Result<(snow::TransportState, Vec<u8>), String> {
    let params = PARAMS.parse().map_err(|e| format!("noise params: {e}"))?;
    let builder = snow::Builder::new(params);
    let kp = builder.generate_keypair().map_err(|e| e.to_string())?;
    let mut hs = if initiator {
        builder.local_private_key(&kp.private).build_initiator()
    } else {
        builder.local_private_key(&kp.private).build_responder()
    }
    .map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; 1024];
    if initiator {
        let n = hs.write_message(&[], &mut buf).map_err(|e| e.to_string())?;
        write_frame(stream, &buf[..n]).map_err(|e| e.to_string())?;
        let msg = read_frame(stream).map_err(|e| e.to_string())?;
        hs.read_message(&msg, &mut buf).map_err(|e| e.to_string())?;
        let n = hs.write_message(&[], &mut buf).map_err(|e| e.to_string())?;
        write_frame(stream, &buf[..n]).map_err(|e| e.to_string())?;
    } else {
        let msg = read_frame(stream).map_err(|e| e.to_string())?;
        hs.read_message(&msg, &mut buf).map_err(|e| e.to_string())?;
        let n = hs.write_message(&[], &mut buf).map_err(|e| e.to_string())?;
        write_frame(stream, &buf[..n]).map_err(|e| e.to_string())?;
        let msg = read_frame(stream).map_err(|e| e.to_string())?;
        hs.read_message(&msg, &mut buf).map_err(|e| e.to_string())?;
    }

    let hash = hs.get_handshake_hash().to_vec();
    let transport = hs.into_transport_mode().map_err(|e| e.to_string())?;
    Ok((transport, hash))
}

// ── Authentication payload ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMsg {
    /// "member" (has a cert) or "enroll" (joining, presents a ticket instead).
    pub kind: String,
    pub node_pub: String,
    /// ed25519 signature over the Noise handshake hash — the channel binding.
    pub hash_sig: String,
    #[serde(default)]
    pub cert: Option<MembershipCert>,
    #[serde(default)]
    pub ticket: Option<EnrollTicket>,
}

/// Who we authenticated on the far end.
#[derive(Debug, Clone)]
pub enum Peer {
    /// A verified org member.
    Member { node_pub: String, cert: MembershipCert },
    /// A node asking to enroll (presented a valid, unexpired ticket).
    Enrolling { node_pub: String, ticket: EnrollTicket },
}

impl Peer {
    pub fn node_pub(&self) -> &str {
        match self {
            Peer::Member { node_pub, .. } | Peer::Enrolling { node_pub, .. } => node_pub,
        }
    }
}

fn build_auth(kind: &str, node: &NodeKey, hash: &[u8], cert: Option<MembershipCert>, ticket: Option<EnrollTicket>) -> AuthMsg {
    AuthMsg {
        kind: kind.to_string(),
        node_pub: node.public_b64(),
        hash_sig: b64(&node.sign(hash)),
        cert,
        ticket,
    }
}

/// Verify a received AuthMsg: channel binding first, then authorization.
fn verify_auth(auth: &AuthMsg, hash: &[u8], trusted_org_pub: &[u8; 32], revocations: &RevocationList) -> Result<Peer, String> {
    // Channel binding: the peer must prove it holds `node_pub` on THIS channel.
    let node_pub = unb64_arr::<32>(&auth.node_pub)?;
    let sig = unb64_arr::<64>(&auth.hash_sig)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&node_pub).map_err(|e| e.to_string())?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig);
    vk.verify_strict(hash, &signature)
        .map_err(|_| "channel binding signature invalid".to_string())?;

    let now = now_unix();
    match auth.kind.as_str() {
        "member" => {
            let cert = auth.cert.clone().ok_or("member auth missing cert")?;
            if cert.node_pub != auth.node_pub {
                return Err("cert does not match presenting node".into());
            }
            // Trust set = home org + any federated orgs (the one extra lookup).
            let mut trust = vec![*trusted_org_pub];
            trust.extend(super::identity::FederationList::load().trusted_bytes());
            cert.verify_with_trust(&trust, now)?;
            if revocations.is_revoked(&auth.node_pub) {
                return Err("node is revoked".into());
            }
            Ok(Peer::Member { node_pub: auth.node_pub.clone(), cert })
        }
        "enroll" => {
            let ticket = auth.ticket.clone().ok_or("enroll auth missing ticket")?;
            let org = ticket.verify(now)?;
            if &org != trusted_org_pub {
                return Err("ticket is for a different org".into());
            }
            Ok(Peer::Enrolling { node_pub: auth.node_pub.clone(), ticket })
        }
        other => Err(format!("unknown auth kind {other}")),
    }
}

/// Dial a peer as an authenticated member. Returns the channel and who answered.
pub fn connect_member(
    addr: &str,
    node: &NodeKey,
    my_cert: MembershipCert,
    trusted_org_pub: &[u8; 32],
    revocations: &RevocationList,
) -> Result<(Channel, Peer), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream.set_nodelay(true).ok();
    let (noise, hash) = handshake(&mut stream, true)?;
    let mut ch = Channel { stream, noise, rbuf: Vec::new(), hash };

    let h = ch.hash.clone();
    ch.send_json(&build_auth("member", node, &h, Some(my_cert), None))
        .map_err(|e| format!("send auth: {e}"))?;
    let peer_auth: AuthMsg = ch.recv_json().map_err(|e| format!("recv auth: {e}"))?;
    let peer = verify_auth(&peer_auth, &h, trusted_org_pub, revocations)?;
    Ok((ch, peer))
}

/// Dial a peer to enroll (we have no cert yet — present a ticket).
pub fn connect_enroll(
    addr: &str,
    node: &NodeKey,
    ticket: EnrollTicket,
    trusted_org_pub: &[u8; 32],
    revocations: &RevocationList,
) -> Result<(Channel, Peer), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream.set_nodelay(true).ok();
    let (noise, hash) = handshake(&mut stream, true)?;
    let mut ch = Channel { stream, noise, rbuf: Vec::new(), hash };

    let h = ch.hash.clone();
    ch.send_json(&build_auth("enroll", node, &h, None, Some(ticket)))
        .map_err(|e| format!("send auth: {e}"))?;
    let peer_auth: AuthMsg = ch.recv_json().map_err(|e| format!("recv auth: {e}"))?;
    // The admin answering enrollment authenticates as a normal member.
    let peer = verify_auth(&peer_auth, &h, trusted_org_pub, revocations)?;
    Ok((ch, peer))
}

/// Accept an inbound connection: complete the handshake, exchange auth, and
/// tell the caller whether the peer is a member or is trying to enroll.
pub fn accept(
    stream: TcpStream,
    node: &NodeKey,
    my_cert: MembershipCert,
    trusted_org_pub: &[u8; 32],
    revocations: &RevocationList,
) -> Result<(Channel, Peer), String> {
    let mut stream = stream;
    stream.set_nodelay(true).ok();
    let (noise, hash) = handshake(&mut stream, false)?;
    let mut ch = Channel { stream, noise, rbuf: Vec::new(), hash };

    // Read the peer's auth first (so an enrolling peer is recognised), then send ours.
    let h = ch.hash.clone();
    let peer_auth: AuthMsg = ch.recv_json().map_err(|e| format!("recv auth: {e}"))?;
    let peer = verify_auth(&peer_auth, &h, trusted_org_pub, revocations)?;
    ch.send_json(&build_auth("member", node, &h, Some(my_cert), None))
        .map_err(|e| format!("send auth: {e}"))?;
    Ok((ch, peer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::identity::{NodeKey, OrgRoot};
    use std::net::TcpListener;
    use std::thread;

    /// Two members complete the encrypted handshake, mutually authenticate, and
    /// exchange application messages over a real loopback socket.
    #[test]
    fn loopback_mutual_auth_and_messaging() {
        let org = OrgRoot::from_seed([1u8; 32]);
        let org_pub = org.public_bytes();
        let server = NodeKey::from_seed([2u8; 32]);
        let client = NodeKey::from_seed([3u8; 32]);
        let server_cert = org.issue_cert(server.public_bytes(), 0, vec![]);
        let client_cert = org.issue_cert(client.public_bytes(), 0, vec![]);
        let revs = RevocationList::default();
        let server_pub_b64 = b64(&server.public_bytes());
        let client_pub_b64 = b64(&client.public_bytes());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (op, sc, rv) = (org_pub, server_cert.clone(), revs.clone());
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (mut ch, peer) = accept(stream, &server, sc, &op, &rv).unwrap();
            let got = ch.recv_msg().unwrap();
            ch.send_msg(b"pong").unwrap();
            (peer.node_pub().to_string(), String::from_utf8(got).unwrap())
        });

        let (mut ch, peer) = connect_member(&addr, &client, client_cert, &org_pub, &revs).unwrap();
        // The client sees the server's real identity (channel-bound).
        assert_eq!(peer.node_pub(), server_pub_b64);
        ch.send_msg(b"ping").unwrap();
        assert_eq!(ch.recv_msg().unwrap(), b"pong");

        let (server_saw, msg) = handle.join().unwrap();
        assert_eq!(server_saw, client_pub_b64);
        assert_eq!(msg, "ping");
    }

    /// A cert from a different org is rejected at the transport layer (I2).
    #[test]
    fn foreign_org_rejected() {
        let org = OrgRoot::from_seed([1u8; 32]);
        let evil = OrgRoot::from_seed([9u8; 32]);
        let org_pub = org.public_bytes();
        let server = NodeKey::from_seed([2u8; 32]);
        let client = NodeKey::from_seed([3u8; 32]);
        let server_cert = org.issue_cert(server.public_bytes(), 0, vec![]);
        // Client's cert is signed by a DIFFERENT org.
        let evil_cert = evil.issue_cert(client.public_bytes(), 0, vec![]);
        let revs = RevocationList::default();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (op, sc, rv) = (org_pub, server_cert, revs.clone());
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Server must refuse the foreign-org client.
            accept(stream, &server, sc, &op, &rv).is_err()
        });

        // Client will also fail (server won't complete), but the security
        // assertion is that the SERVER rejected it.
        let _ = connect_member(&addr, &client, evil_cert, &org_pub, &revs);
        assert!(handle.join().unwrap(), "server must reject foreign-org cert");
    }
}
