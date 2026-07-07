# v2 Mesh — Share Compute Safely

The mesh lets a team pool GPU/CPU compute over Ollama. A node is an ed25519
identity; membership is an org-signed certificate; the wire is an encrypted,
mutually-authenticated channel. Ollama itself is never exposed — peers only ever
talk to v2, which enforces policy in front of it.

## Roles

- **Admin** — holds the org root key, invites and revokes members.
- **Member** — has a certificate signed by the org; can serve and/or consume.

The admin is also a member (it self-issues a cert on `mesh init`).

## Setup

### Admin: create the org

```bash
v2 mesh init                       # creates the org root; you're the admin
v2 mesh invite YOUR_HOST:4830      # prints a one-time invite ticket
v2 serve --mesh-listen 0.0.0.0:4830
```

`YOUR_HOST:4830` is the address teammates will reach you at (LAN IP, VPN address,
or a public host:port). `serve` runs the local metering proxy and, because you're a
member, also serves the mesh.

### Member: join and use

```bash
v2 mesh join <ticket>              # the whole member setup
v2 mesh run qwen3:32b "explain quicksort"
```

`join` connects to the admin over an encrypted channel, proves control of your node
key, and receives a signed membership certificate. The admin's address is saved as a
peer automatically.

To also share *your* machine, run `v2 serve --mesh-listen 0.0.0.0:PORT` and give
others your address with `v2 mesh peer add`.

## How trust works

- **Identity** — every node has an ed25519 keypair (`~/.v2/key`, mode 0600). The
  public key is the node's mesh id.
- **Membership cert** — an org signature over `(node pubkey, expiry, capabilities)`.
  Certs are short-lived (24h) and auto-renew while the node stays trusted.
- **Channel binding** — after the encrypted handshake, each side signs the unique
  handshake hash with its node key. Verifying that proves the peer on *this* channel
  holds exactly the key the org authorized — not a replay or a relay.
- **Revocation** — `v2 mesh revoke <node-id>` (admin) drops a node immediately and,
  because certs expire in 24h, a revoked node stops working even if the revocation
  message is never delivered.

Any doubt about identity, signature, or message shape drops the connection. There is
no "warn and continue".

## Controlling what you give

Your machine is governed by `~/.v2/policy.toml`. A machine with **no policy file is
already safe** (one remote job, half the VRAM, AC power required, instant yield).
Full reference: [CONFIG.md](CONFIG.md).

```toml
[serve]
max_concurrent_remote = 1
max_vram_fraction     = 0.5
allowed_models        = ["qwen3:8b", "llama3.2:*"]
max_ctx               = 8192

[availability]
require_ac_power = true
yield_to_local   = true
```

Every remote request passes an ordered admission gate: cert valid → model allowed →
context within limit → per-peer hourly quota → resources free → available (hours,
power, owner idle). It's accepted, queued (try later), or refused (don't retry here).

## Getting your machine back

```bash
v2 mesh pause     # stop accepting, cancel in-flight jobs, in seconds
v2 mesh resume
```

`yield_to_local` does this automatically: the moment you use the machine, remote work
is preempted. And if the daemon ever dies, every remote connection drops and Ollama
stops generating — the failure state is the safe state, so a crash returns your
compute too.

## Accounting

Each served request produces a receipt signed by both the server and the client, so
neither side can later forge or deny usage. `v2 usage` shows what you served and what
you consumed elsewhere; receipts are stored under `~/.v2/mesh/receipts/`.

## Federation (across orgs)

An admin can trust another org with a scoped allowlist:

```bash
v2 mesh federation-add <other-org-id> --note "team-b" --models "qwen3:*,llama3.2:*"
v2 mesh federation-list
```

Members of the federated org can then use only the models in that scope on your
machine (default deny). Everything else about the trust model is unchanged.

## Command reference

| Command | Who | Does |
|---------|-----|------|
| `v2 mesh init` | admin | create the org |
| `v2 mesh invite <addr>` | admin | mint a one-time invite ticket |
| `v2 mesh revoke <id>` | admin | revoke a node |
| `v2 mesh federation-add <id>` | admin | trust another org with a scope |
| `v2 mesh join <ticket>` | member | join an org |
| `v2 mesh run <model> <prompt>` | member | run on the best peer |
| `v2 mesh status` | any | identity, role, peers |
| `v2 mesh peers` | any | fetch reachable peers' node cards |
| `v2 mesh peer add <addr>` | any | remember a peer address |
| `v2 mesh pause` / `resume` | any | reclaim / re-offer this machine |
| `v2 mesh id` | any | print this node's public id |

## Transport note

The mesh currently uses direct encrypted TCP — ideal on a LAN or with a reachable
`host:port`. NAT hole-punching (via iroh) is the one planned transport change and is
isolated to `src/mesh/transport.rs`; the trust model above is unaffected by it.
