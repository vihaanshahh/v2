# v2 — Agent Setup Instructions

Copy-paste these instructions to an AI coding agent (Claude Code, Cursor, etc.)
to have it **install v2, wire it to Ollama, pick the right model for the machine,
and — optionally — set up the mesh, a relay, and an OpenAI-compatible endpoint.**

> **What v2 is:** a single static Rust binary that detects your hardware, tells
> you which LLMs fit and how fast, meters and manages them through
> [Ollama](https://ollama.com), and can pool compute across machines over a
> secure mesh. No Docker, no hosted coordinator.

---

## For the agent — read first

- These steps are **idempotent** and safe to re-run. Verify with `v2 doctor` after each stage.
- v2 keeps all state in **`~/.v2/`** (node key, membership cert, policy, peers, receipts). Nothing else is touched.
- **Do not run blocking daemons in the foreground** of a tool call. `v2 serve …` and `v2 mesh relay …` block forever — start them backgrounded (`nohup … &`, `tmux`, or a systemd unit) and poll readiness with `curl`, never a foreground `sleep`.
- The bare scan (`v2`) needs nothing but the binary. Everything else needs **Ollama running** (`ollama serve`).
- Stop background v2 processes by **port** (`fuser -k 11435/tcp`), not `pkill -f "v2 serve"` — that pattern can match your own shell.

---

## 1. Prerequisites

```bash
# Ollama (the inference engine v2 drives)
curl -fsSL https://ollama.com/install.sh | sh   # or: brew install ollama
ollama serve >/dev/null 2>&1 &                   # start it if not already running
curl -s --retry 20 --retry-all-errors -o /dev/null http://127.0.0.1:11434/api/tags  # wait until up
```

## 2. Install v2

**Option A — released binary (once a release is tagged):**

```bash
curl -fsSL https://raw.githubusercontent.com/vihaanshahh/v2/main/install.sh | bash
# custom location: curl -fsSL … | PREFIX="$HOME/.local/bin" bash
```

**Option B — from source (needs a recent Rust toolchain):**

```bash
git clone https://github.com/vihaanshahh/v2 && cd v2
cargo build --release
install -m755 target/release/v2 "$HOME/.local/bin/v2"   # put it on PATH
```

Verify: `v2 --help` prints the command list, and `v2 doctor` shows Ollama reachable.

## 3. Scan the machine and pick a model

```bash
v2                       # rank every model by fit + speed on THIS machine
v2 check "Qwen3 30B A3B" # inspect one model at every quant
```

**How to pick (heuristics):**

- **CPU-only machine?** Token speed is memory-bandwidth-bound: `tok/s ≈ bandwidth ÷ bytes-per-token`, and bytes scale with model size. So **dense models above ~7B crawl (~1–2 tok/s)**.
- **Prefer MoE (mixture-of-experts, shown with a `*`)** — e.g. `Qwen3 30B A3B` activates only ~3B params/token, so it runs ~10× faster than a same-size dense model. On a 32 GB CPU box it does ~12 tok/s vs ~1–2 for dense.
- **Need interactive speed (>~30 tok/s) with no GPU?** You're limited to ~1–1.5B models. There is no way around this without a GPU — adding RAM/cores lets bigger models *fit*, not run faster.
- Read the `speed` column in `v2` output and filter by your own threshold.

## 4. Install and run the chosen model

```bash
v2 pull "Qwen3 30B A3B"                    # fit-check, then download via Ollama
v2 run  "Qwen3 30B A3B" "explain quicksort" # ensure installed, then chat
v2 ps                                       # installed models with fit info
```

## 5. Serve locally + OpenAI-compatible endpoint

`v2 serve` runs a metering proxy in front of Ollama and exposes an
**OpenAI-compatible API**, so any OpenAI SDK/tool can point its Base URL at v2.
The `/v1` surface is **key-gated by default** — v2 auto-creates and persists a
key at `~/.v2/api_key` on first run, so it's safe to expose with zero setup.

```bash
nohup v2 serve --listen 127.0.0.1:11435 --headless >/tmp/v2-serve.log 2>&1 &
curl -s --retry 20 --retry-all-errors -o /dev/null http://127.0.0.1:11435/api/tags  # wait
v2 endpoint                       # prints the paste-ready Base URL + key + models
```

Knobs (all optional):
- `V2_API_KEY=<key>` — pin a specific key instead of the auto-generated one.
- `V2_PUBLIC_URL=https://…` — advertise a public/tunnel URL as the Base URL (behind a reverse proxy or a platform service).
- `V2_OPEN=1` — disable the gate entirely (only for trusted loopback use).

Point a client at it (values come straight from `v2 endpoint`):

| Field         | Value                                  |
| ------------- | -------------------------------------- |
| **Base URL**  | `http://127.0.0.1:11435/v1` (or your `V2_PUBLIC_URL`) |
| **API Key**   | the key from `v2 endpoint` / `~/.v2/api_key` |
| **Model ID**  | any model from `GET /v1/models`        |

```bash
# List models (local Ollama tags + any registered remote endpoints, merged)
curl -s http://127.0.0.1:11435/v1/models -H "Authorization: Bearer $V2_API_KEY"

# Chat (OpenAI shape; streaming with "stream": true works too)
curl -s http://127.0.0.1:11435/v1/chat/completions \
  -H "Authorization: Bearer $V2_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"llama3.2:1b","messages":[{"role":"user","content":"hi"}]}'
```

**Unify remote models too.** Register any OpenAI-compatible host (Modal, vLLM,
OpenAI, Together, …) and v2 will route requests for its model id to it (with its
own key), so one Base URL fronts local + remote:

```bash
# In the interactive panel (v2 serve without --headless) → "add endpoint",
# or the endpoint is stored in ~/.v2/endpoints.json:
#   name=gpt-5.5  url=https://api.openai.com/v1  model=gpt-5.5  api_key=sk-…
# Then: curl … -d '{"model":"gpt-5.5", …}' is proxied to OpenAI with that key.
```

Without `V2_API_KEY`, `/v1` is open — fine for the default `127.0.0.1` bind, **not**
for a network-exposed one. Non-`/v1` paths always pass through to Ollama.

## 6. Mesh — pool compute across machines

**Admin (this becomes the org root — the key never leaves this machine):**

```bash
v2 mesh init                                  # create the org; you are the admin
nohup v2 serve --mesh-listen 0.0.0.0:4830 --headless >/tmp/v2-mesh.log 2>&1 &
v2 mesh invite YOUR_HOST:4830                 # print a one-time invite ticket
```

**Member (another laptop):**

```bash
v2 mesh join <ticket>                         # entire member setup
v2 mesh run "Qwen3 30B A3B" "explain quicksort"   # runs on the best org peer
```

Ops: `v2 mesh status`, `v2 mesh peers`, `v2 mesh pause` / `resume` (reclaim your
machine instantly), `v2 mesh revoke <node-id>`. A machine with no
`~/.v2/policy.toml` is **already safe**: one remote job, half the VRAM, yields to
you the moment you touch the keyboard. See [`docs/MESH.md`](docs/MESH.md).

## 7. Relay — connect without exposing any IP

A **relay** is a zero-trust rendezvous: nodes dial *out* to it and are addressed
by **public key** (`relay://<relay>/<node_pub>`), so nobody opens an inbound port
or leaks an IP. The relay only forwards **encrypted** traffic — it can't read
content, impersonate a node, or forge a cert (the Noise + cert auth runs
end-to-end through it).

```bash
# On a small always-reachable box (VPS): run the relay
nohup v2 mesh relay --listen 0.0.0.0:4840 >/tmp/v2-relay.log 2>&1 &

# On the serving node: register through the relay instead of opening a port
nohup v2 serve --relay RELAY_HOST:4840 --headless >/tmp/v2-serve.log 2>&1 &

# Admin: mint an invite that hides your IP (embeds relay://…/<your-node-id>)
v2 mesh invite --via-relay RELAY_HOST:4840
```

The member joins with that ticket exactly as in step 6 — dialing by pubkey is transparent.

## Verify the whole setup

```bash
v2 doctor        # ollama / identity / mesh / policy / abuse — all should be [ ok ]
v2 mesh status   # node id, org, role, cert validity, peer count
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `doctor` says Ollama unreachable | `ollama serve &`; check `curl http://127.0.0.1:11434/api/tags` |
| `not a member` | `v2 mesh init` (admin) or `v2 mesh join <ticket>` (member) |
| `/v1` returns 401 | send `Authorization: Bearer $V2_API_KEY` (or unset `V2_API_KEY`) |
| relay peer `not reachable (not registered)` | the target must be running `v2 serve --relay <same-relay>` |
| model won't fit | run `v2` and pick one from the "models that fit" list; prefer a `*` MoE model on CPU |

---

Full docs: [`README.md`](README.md) · [`docs/GUIDE.md`](docs/GUIDE.md) ·
[`docs/MESH.md`](docs/MESH.md) · [`docs/CONFIG.md`](docs/CONFIG.md) ·
[`DESIGN.md`](DESIGN.md). Or run `v2 about`.
