# v2 Master Design — Local-First Decentralized AI Compute

Status: implemented (Phases 1–4). `AGENTS.md` has the module map; this file is the
architecture, invariants, and failure harnesses every change must continue to obey.
The transport uses direct Noise_XX over TCP (LAN / reachable host:port); swapping in
iroh for NAT hole-punching is the one planned transport change and is isolated to
`mesh/transport.rs`.

---

## 1. North star

One static binary that answers three questions, in order of maturity:

1. **What can this machine run?** (exists today)
2. **What is it running, how fast, and who used it?** (bandwidth + usage)
3. **Can my org safely share it?** (the mesh)

Non-negotiable product constraints:

- **One binary.** No Docker, no sidecars, no hosted coordination server. Ollama is
  the only external dependency, and it stays bound to localhost.
- **Two-command setup.** Owner: `install.sh` then `v2 serve`. Org member:
  `install.sh` then `v2 mesh join <ticket>`. Everything else is a safe default.
- **The plain CLI never regresses.** `v2` (scan) must work with the mesh code
  absent, broken, or misconfigured.

---

## 2. System overview

```
                    ┌────────────────────────── this machine ─────────────────────────┐
                    │                                                                  │
  teammate ══QUIC══▶│  v2 daemon (v2 serve)                                          │
  (e2e encrypted,   │  ┌──────────┐  ┌──────────┐  ┌───────────┐  ┌────────────────┐  │
   cert-verified)   │  │  mesh    │→ │ admission│→ │ execution │→ │ Ollama         │  │
                    │  │ transport│  │ harness  │  │ harness   │  │ 127.0.0.1:11434│  │
                    │  └──────────┘  └──────────┘  └───────────┘  └────────────────┘  │
                    │        │             │             │                             │
  local apps ──────▶│  :11435 metering proxy             │                             │
                    │        │             │             │                             │
                    │        ▼             ▼             ▼                             │
                    │   usage JSONL   policy.toml   activity monitor (reclaim)         │
                    │   (~/.v2/)     (~/.v2/)     (yield-to-local)                   │
                    └──────────────────────────────────────────────────────────────────┘

  v2 CLI (scan / pull / run / top / usage) — works with or without the daemon.
```

The daemon is a **proxy in front of Ollama** for both local apps (metering) and
mesh peers (metering + auth + policy). Ollama is never reachable from the network.

---

## 3. Invariants

Every feature, PR, and failure response is judged against these. If a change can
violate one, it does not merge.

- **I1 — Owner sovereignty.** The machine's owner always wins. Any local activity
  preempts remote work; full reclaim (`v2 mesh pause`) completes in ≤ 2 seconds.
- **I2 — Fail closed on trust.** Any doubt in identity, certificate, signature, or
  message shape → drop the connection. There is no "warn and continue" path.
- **I3 — Fail open on local function.** No mesh/daemon failure may break the local
  CLI. Corrupt mesh state is quarantined, never repaired in-band.
- **I4 — No content at rest.** Prompts and outputs from peers exist only in memory.
  Disk sees token *counts* and signed receipts, never content.
- **I5 — Bounded resource grant.** Remote work can never exceed the policy caps
  (VRAM fraction, concurrency, ctx, tokens/hour) in any state, including crash states.

---

## 4. The core trick: safety by construction

The strongest harness is one that cannot itself fail. Two structural decisions do
most of the safety work, with zero monitoring code:

**Deadman by design.** Every remote generation is an HTTP stream held open by the
daemon. Ollama aborts generation the moment its client connection drops. Therefore:
daemon crash → connections drop → all remote work stops → compute returns to the
owner. The failure state *is* the safe state. No watchdog needed for I1/I5.

**Expiry beats revocation.** Membership certs are short-lived (24h TTL,
auto-renewed while trusted). Revocation is gossiped as an optimization, but even if
gossip never delivers, a revoked node dies within the TTL. Security does not depend
on message delivery. (I2)

Everything else in section 6 is defense in depth on top of these two.

---

## 5. Identity & trust model

- **Node identity** = ed25519 keypair (generated on first run, `~/.v2/key`).
  The public key *is* the node's mesh address (iroh NodeId).
- **Org root key** = created by `v2 mesh init`. Held by the admin. Signs
  membership certs and revocations. Never leaves the admin machine.
- **Membership cert** = org signature over (node pubkey, expiry, capabilities).
  Issued via `v2 mesh invite` → one-time ticket → `v2 mesh join <ticket>`.
- **Every connection** is mutually authenticated: both sides present certs, both
  verify org signature + expiry + revocation list. iroh gives e2e-encrypted QUIC
  with hole-punching; no inbound firewall ports ever open.
- **Federation (later):** a second org's root key can be trusted with a scoped
  policy. Additive — the cert check gains one lookup, nothing is redesigned.

Transport crate: **iroh** (pure Rust). Chosen over libp2p (too heavy) and
Tailscale-underneath (external product dependency; breaks the one-binary promise).

---

## 6. Harnesses

Each harness is a small, independently testable gate. A request from a peer passes
through all of them, in order. Any failure at any gate resolves per I2/I3.

### H1 — Admission harness (before any work)
Order matters: cheapest and most security-critical checks first.
1. Cert valid, unexpired, not revoked → else drop (I2).
2. Model allowed by `allowed_models` globs → else refuse with reason.
3. `max_ctx`, request-shape sanity → else refuse.
4. Peer quota (`tokens_per_hour`, sliding window) → else refuse with retry-after.
5. Resource gate: concurrency slot free AND projected VRAM (existing `engine.rs`
   math) within `max_vram_fraction` → else queue (bounded) or refuse.
6. Availability: schedule window, AC power, owner not active → else refuse.

### H2 — Execution harness (during work)
- Hard wall-clock timeout per request (default 120s, policy-tunable).
- Token budget enforced **mid-stream**: output tokens counted as they stream; at
  the cap the connection to Ollama is dropped, which aborts generation instantly.
- Cancellation is edge-triggered: peer disconnect, owner activity, pause command,
  or daemon shutdown all resolve to the same action — drop the Ollama connection.
  One cancellation path, exercised by every trigger, so it cannot rot.

### H3 — Reclaim harness (owner sovereignty, I1)
- **Explicit:** `v2 mesh pause` → stop admitting, cancel in-flight (drop
  connections), `keep_alive: 0` request to unload models. Target ≤ 2s.
- **Automatic (`yield_to_local`):** local request arriving at the :11435 proxy, or
  GPU-pressure signal, immediately cancels remote work and holds admission for a
  cooldown window. Local use never queues behind remote use.
- **Crash:** covered by deadman-by-design (section 4). No code required.

### H4 — Mesh state harness (I3)
- All mesh state lives under `~/.v2/mesh/`. On any load error: rename the
  directory to `mesh.quarantine.<n>`, log one line, continue as a solo node.
  Rejoining is two commands; debugging a half-corrupt trust store is never
  attempted at runtime.
- Gossip (node cards, revocations) is best-effort. Correctness never depends on
  it (see expiry-beats-revocation). Stale node cards only cause suboptimal
  scheduling, never unsafe execution — H1 re-verifies everything at admission.

### H5 — Data harness (I4, accounting integrity)
- Usage log: append-only JSONL, one line per completed request
  (`ts, peer, model, tokens_in, tokens_out, duration_ms`). Crash-safe by format —
  a torn final line is skipped on read. Rotated daily, compacted on `v2 usage`.
- Mesh requests additionally produce a **signed receipt** (both node keys over the
  usage line). Both sides store receipts; `v2 mesh usage` reconciles. Disputes
  are detectable because forging requires both keys.
- Content is never written to disk anywhere in the daemon. Enforced by
  construction: the proxy's request/response types implement no serialization to
  the storage layer; the usage record type contains only counts.

### H6 — Compatibility harness (I3, "never regresses")
- The scan/CLI path (`main.rs → hardware/models/engine/display`) keeps **zero**
  imports from mesh/daemon modules. Enforced by a compile-time feature boundary:
  `--no-default-features` builds the CLI alone, and CI builds both.
- `v2 doctor` checks the full stack (binary, Ollama reachable, key present, cert
  validity, policy parse, port free) and prints one actionable line per problem.

---

## 7. Node lifecycle

```
                 ┌────────┐  v2 mesh join   ┌─────────┐  v2 serve  ┌─────────┐
                 │  SOLO  │ ───────────────▶ │ MEMBER  │ ──────────▶ │ SERVING │
                 │(CLI ok)│ ◀─────────────── │(no serve)│ ◀────────── │         │
                 └────────┘  leave/quarantine└─────────┘   stop      └─────────┘
                                                              ▲  │ owner activity /
                                                     resume / │  │ pause / schedule
                                                     cooldown │  ▼
                                                           ┌──────────┐
                                                           │ YIELDING │  admits nothing,
                                                           └──────────┘  cancels in-flight
```

Every state degrades leftward on failure. There is no state in which remote work
runs without an admission pass, and no state that disables the local CLI.

## 8. Request lifecycle (mesh)

```
peer ──▶ QUIC accept ──▶ H1 admission ──▶ open Ollama stream ──▶ H2 stream/count
              │drop            │refuse(reason)        │                │
              ▼ (I2)           ▼                      ▼ any trigger    ▼ done
            closed          peer retries         drop conn = abort   receipt signed,
            silently        elsewhere            (single cancel path) JSONL appended
```

Scheduling on the client side: `v2 run --mesh <model>` ranks org peers by
fit (reusing `engine.rs`), advertised load, and latency; tries the best, falls
through to the next on refusal. Refusals are cheap and expected — the mesh
self-balances through them.

---

## 9. Policy file

`~/.v2/policy.toml` — optional; absent file = these defaults:

```toml
[serve]
max_concurrent_remote = 1
max_vram_fraction = 0.5
allowed_models = ["*"]        # every admitted org member may use installed models
max_ctx = 8192
request_timeout_s = 120

[quota]
per_peer_tokens_per_hour = 200_000

[availability]
hours = "always"              # e.g. "09:00-18:00"
require_ac_power = true
yield_to_local = true
```

Defaults are chosen so that a node with no config is already safe (I5): one remote
job at a time, half the VRAM, instant yield. Policy parse error → daemon refuses
to start serving (fail closed) but CLI and MEMBER mode still work (I3).

---

## 10. Bandwidth & throughput model

Decode speed is memory-bandwidth bound: **est. tok/s ≈ effective memory bandwidth
÷ bytes moved per token** (weight bytes of active params, already computed in
`engine.rs`, plus KV read). Bandwidth comes from a static table keyed on GPU/SoC
name (`src/bandwidth.rs`): Apple M-series, NVIDIA/AMD desktop and laptop parts;
unknown hardware falls back to vendor-class estimates and is labeled `~`.

Estimates appear in the scan table and in gossiped node cards, so the mesh
scheduler can rank peers by *expected speed*, not just fit. Actual tok/s from
completed requests (Ollama's `eval_count`/`eval_duration`) is fed back into the
node card, replacing the estimate with measured truth over time.

---

## 11. Module map & phases

```
src/bandwidth.rs   Phase 1  bandwidth table + tok/s estimate (pure, no I/O)
src/usage.rs       Phase 1  JSONL store + summaries (pure I/O, no network)
src/proxy.rs       Phase 1  :11435 metering proxy (first daemon code)
src/manage.rs      Phase 2  pull / run / rm / ps / top — fit-aware Ollama wrappers
src/policy.rs      Phase 3  policy.toml load + H1 gates (pure logic, heavily tested)
src/mesh/
  identity.rs      Phase 3  keys, certs, tickets, revocation
  transport.rs     Phase 3  iroh connect/accept, mutual cert verification
  gossip.rs        Phase 3  node cards, best-effort broadcast
  serve.rs         Phase 3  H1→H2→H3 pipeline around Ollama
  client.rs        Phase 3  peer ranking + remote run
```

Dependency rule: `bandwidth`, `usage`, `policy`, `mesh/identity` are pure
(deterministic, no I/O in core logic) so their harness tests are exhaustive and
fast. Async runtime (`tokio`) and `iroh` are confined behind the daemon feature
flag (H6).

### Phase gates (each phase ships only when its harness holds)

| Phase | Ships | Gate — must demonstrate |
|-------|-------|-------------------------|
| 1 | tok/s column, `v2 top`, `v2 serve` proxy, `v2 usage` | kill -9 the daemon mid-generation → local Ollama unaffected; torn JSONL line → `usage` still reads |
| 2 | `pull/run/rm/ps` | pulling a model that doesn't fit requires explicit confirm; `run` picks the documented best quant |
| 3a | identity + join/invite/revoke | tampered ticket, expired cert, revoked node → all refused (test vectors in repo) |
| 3b | remote inference + H1/H2 | token cap aborts mid-stream; over-quota peer refused with retry-after; VRAM gate uses engine math |
| 3c | reclaim + yield | `pause` ≤ 2s with a generation in flight; local request cancels remote within 1 poll tick; daemon kill = all remote work stops |
| 4 | federation | scoped cross-org trust behind one extra cert lookup |

### Failure-mode table (spot checks for review, not exhaustive)

| Failure | Detected by | Response | Invariant |
|---------|-------------|----------|-----------|
| Daemon crash mid-job | connection drop (structural) | Ollama aborts; owner has machine | I1, I5 |
| Cert expired / revoked | H1 step 1 | drop connection | I2 |
| Gossip partition | none needed | certs expire on TTL | I2 |
| Corrupt `~/.v2/mesh/` | load error | quarantine dir, run SOLO | I3 |
| Policy file typo | parse error at start | serving refused; CLI fine | I3, I5 |
| Peer floods requests | H1 quota + bounded queue | refuse with retry-after | I5 |
| Runaway generation | H2 token/time caps | drop Ollama conn | I1, I5 |
| Ollama down | proxy health check | node card marks unavailable; peers route elsewhere | I3 |

---

## 12. CLI surface (end state)

```
v2                      scan (unchanged, + tok/s column)
v2 pull|run|rm|ps|top   fit-aware model management
v2 serve                daemon: metering proxy + (if member) mesh serving
v2 usage [--json]       local + mesh usage summaries
v2 doctor               one line per problem, actionable
v2 mesh init            create org (admin)
v2 mesh invite          one-time join ticket
v2 mesh join <ticket>   become a member (the entire member setup)
v2 mesh status|peers    roster, node cards, who's serving
v2 mesh pause|resume    instant reclaim / re-offer
v2 mesh revoke <node>   admin revocation
v2 mesh usage           signed-receipt reconciliation
v2 run --mesh <model>   run on the best org peer
```

Setup story, in full:
- **Owner sharing a machine:** `curl … | bash` → `v2 mesh join <ticket>` → `v2 serve`.
- **Teammate using the mesh:** `curl … | bash` → `v2 mesh join <ticket>` → `v2 run --mesh qwen3:32b`.
- **Admin:** the above plus `v2 mesh init` once and `v2 mesh invite` per person.
