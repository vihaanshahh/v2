# v2

**Which LLMs can you run on this machine — and how do you share that compute safely?**

`v2` is a single static Rust binary. It starts by answering the first question
(detect your hardware, tell you which models fit and how fast), then grows into a
metering proxy, fit-aware model management, and a secure org **mesh** for pooling
compute — all over [Ollama](https://ollama.com), with no Docker and no hosted
coordination server. Ollama stays bound to localhost; the mesh only ever talks to `v2`.

```
$ v2 --source catalog --family qwen3
v2  24G vram · 64G ram · Linux · 4k ctx · catalog
  fit  model                   quant    mem      speed
  gpu   Qwen3 8B                Q8_0     9.2G     48 tok/s
  gpu   Qwen3 14B               Q5_K_M   11.2G    31 tok/s
  ~40%  Qwen3 32B               Q4_K_M   22.1G    9 tok/s
```

---

## Install

> 🤖 **Using an AI agent?** Hand it [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) —
> a copy-paste, agent-executable guide that installs v2, wires it to Ollama, picks
> the right model for the machine, and sets up the mesh, a relay, and the
> OpenAI-compatible endpoint end to end.

**From source** (needs a recent Rust toolchain):

```bash
git clone https://github.com/vihaanshahh/v2
cd v2
cargo build --release
# binary at ./target/release/v2  — copy it onto your PATH
```

**Installer** (once a release is tagged):

```bash
curl -fsSL https://raw.githubusercontent.com/vihaanshahh/v2/main/install.sh | bash
```

The scan works on its own. Everything under **Serve / manage / mesh** below also needs
[Ollama](https://ollama.com) running locally (`ollama serve`).

---

## Scan — what fits, how fast

```bash
v2                      # detect hardware + rank models with fit and tok/s
v2 models               # list models from the configured source
v2 check qwen3:8b       # check one model at every quant
v2 --json               # machine-readable output
```

Useful flags: `--ctx <n>` (context length), `--source catalog|ollama|all|auto`,
`--family <name>`, `--accepted <file>` / `--enterprise` (allowlist), `-v` (per-quant detail).

Speed is estimated from a memory-bandwidth model (decode is bandwidth-bound); a `~`
prefix means the GPU/SoC wasn't in the table and the number is a vendor-class guess.

---

## Serve, meter, and manage

```bash
v2 serve                # metering proxy on :11435 in front of Ollama
v2 top                  # what's currently loaded in Ollama
v2 usage                # exact token usage by day / model / source
v2 doctor               # one actionable line per subsystem

v2 pull qwen3:8b        # fit-check ("fits fully on GPU, est 48 tok/s") then download
v2 run qwen3:8b         # ensure installed, then chat
v2 ps                   # installed models with fit info
v2 rm qwen3:8b
```

Point your apps at `http://localhost:11435` instead of Ollama's `:11434` and `v2`
records exact token counts (from Ollama's own stream stats) to append-only JSONL under
`~/.v2/usage/` — no database, no content ever written to disk.

---

## Mesh — share compute across your team

A node is an ed25519 identity; membership is an **org-signed certificate**; the wire is
an **encrypted, mutually-authenticated Noise channel**. Setup is two commands.

**Admin — create an org and invite people:**

```bash
v2 mesh init                      # you become the admin
v2 mesh invite YOUR_HOST:4830     # prints a one-time invite ticket
v2 serve --mesh-listen 0.0.0.0:4830
```

**Teammate — join and use the mesh:**

```bash
v2 mesh join <ticket>             # the entire member setup
v2 mesh run qwen3:32b "explain quicksort"
```

**Share your own machine safely** — `v2 serve --mesh-listen …`, governed by
`~/.v2/policy.toml`. A machine with **no config is already safe**: one remote job at a
time, half the VRAM, AC-power required, and instant yield to you:

```toml
[serve]
max_concurrent_remote = 1
max_vram_fraction     = 0.5
allowed_models        = ["qwen3:8b", "llama3.2:*"]
max_ctx               = 8192

[quota]
per_peer_tokens_per_hour = 200_000

[availability]
hours            = "09:00-18:00"
require_ac_power = true
yield_to_local   = true    # the moment you use the machine, remote work is evicted

[endpoint]                       # the OpenAI-compatible /v1 surface (v2 serve)
public_url = "https://your-host" # advertise this as the Base URL (clients get <url>/v1)
# api_key  = ""                  # empty → auto-persisted key at ~/.v2/api_key
# open     = false               # true → no bearer gate (loopback-only trust)
```

The `/v1` surface is **key-gated by default** — v2 auto-creates a key at
`~/.v2/api_key` on first serve, so exposing it needs no setup. Run
`v2 endpoint` any time to print the paste-ready Base URL + key + model list.

**Get your laptop back, instantly:**

```bash
v2 mesh pause             # stop accepting, cancel in-flight jobs, seconds
v2 mesh resume
```

Other controls: `v2 mesh status`, `v2 mesh peers`, `v2 mesh peer-add HOST:PORT`,
`v2 mesh revoke <node-id>`, and `v2 mesh federation-add <org-id> --models qwen3:*`
to trust another org with a scoped allowlist.

### Why it's safe

The safety isn't monitoring code that could itself fail — it's structural:

- **Deadman by design.** Every remote generation is a held-open stream. If the daemon
  dies, the peer disconnects, or you reclaim, the connection drops and Ollama stops
  generating. The failure state *is* the safe state.
- **Expiry beats revocation.** Membership certs live 24h and auto-renew while trusted.
  Even if a revocation message never arrives, a revoked node stops working within the TTL.
- **Fail closed on trust, fail open on function.** Any doubt about identity drops the
  connection; any mesh failure leaves your local CLI untouched.
- **No peer content at rest.** Prompts and outputs live in memory only; disk sees token
  counts and signed receipts, never content.

The full architecture, invariants, and failure harnesses are in
[`DESIGN.md`](DESIGN.md).

---

## See it work

`demo/two-node.sh` acts out two laptops on one machine (two isolated `~/.v2`
homes, one shared Ollama) and walks the whole lifecycle — enrol, **accept** and
stream a job, then terminate it three ways (**pause**, **resume**, **revoke**) —
and prints the signed receipts at the end. Needs Ollama running.

```bash
bash demo/two-node.sh
```

The same lifecycle is covered by automated tests in `src/mesh/itest.rs`,
including a job terminated **mid-generation** by owner reclaim.

## Documentation

- **[AGENT_INSTRUCTIONS.md](AGENT_INSTRUCTIONS.md)** — copy-paste setup for AI agents: install → model pick → serve → OpenAI endpoint → mesh → relay.
- **[docs/GUIDE.md](docs/GUIDE.md)** — getting started: scan, manage, meter.
- **[docs/MESH.md](docs/MESH.md)** — the org mesh: setup, trust model, ops, federation.
- **[docs/CONFIG.md](docs/CONFIG.md)** — `policy.toml`, the `~/.v2` file layout, env vars.
- **[DESIGN.md](DESIGN.md)** — architecture, invariants, and failure harnesses.

Or run `v2 about` for a guided overview and `v2 <command> --help` for any command.

## Command reference

| Command | Does |
|---------|------|
| `v2` | scan hardware, rank models by fit + speed |
| `v2 check <model>` | check one model at every quant |
| `v2 models` | list models from the configured source |
| `v2 pull <model>` | fit-check, then download |
| `v2 run <model>` | ensure installed, then chat |
| `v2 ps` / `v2 top` | installed models / what's loaded now |
| `v2 rm <model>` | remove an installed model |
| `v2 serve` | metering proxy (`--mesh-listen` also serves the mesh) |
| `v2 usage` | recorded token usage |
| `v2 doctor` | diagnose Ollama / identity / policy |
| `v2 mesh init\|invite\|join` | create / invite to / join an org mesh |
| `v2 mesh run <model> <prompt>` | run on the best org peer |
| `v2 mesh status\|peers\|pause\|resume` | inspect and control the mesh |
| `v2 about` | logo, version, command overview |

## Build & test

```bash
make check                          # tests + release build
cargo test                          # 29 tests
cargo build --no-default-features   # CLI scan only — no daemon/mesh code compiled
```

The default `daemon` feature adds the proxy, model management, and mesh. The scan path
stays free of that code, so `--no-default-features` builds a lean scan-only binary.

State lives under `~/.v2/` (identity key stored `0600`). Env vars: `OLLAMA_HOST`,
`V2_ACCEPTED`.

---

## Status & limits

All four phases (bandwidth, management, mesh, federation) are implemented and tested.
Two things to know:

- The mesh transport is **direct Noise-over-TCP** — great on a LAN or with a reachable
  `host:port`. NAT hole-punching (swapping in [iroh](https://github.com/n0-computer/iroh))
  is the one planned transport change and is isolated to `src/mesh/transport.rs`.
- Actually running models (locally or over the mesh) requires Ollama; the scan does not.
