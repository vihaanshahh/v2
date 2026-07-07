# v2 — Getting Started

v2 is a single binary that answers "which LLMs can I run here, how fast, and how
do I share that compute?" It works on top of [Ollama](https://ollama.com).

## 1. Install

From source (any recent Rust toolchain):

```bash
git clone https://github.com/vihaanshahh/v2
cd v2
cargo build --release
sudo cp target/release/v2 /usr/local/bin/     # or anywhere on your PATH
```

Verify:

```bash
v2 about
```

## 2. Scan — what fits, how fast

```bash
v2
```

You get a panel describing your hardware and a ranked table of models with the
best quantization that fits, the memory it needs, and an estimated decode speed:

```
╭─ v2 · which models fit ──────────────────────────────╮
│ gpu     AMD Radeon Pro 5300M · 4G vram               │
│ memory  16G RAM · macOS                              │
│ scan    ctx 4k · source catalog                      │
╰──────────────────────────────────────────────────────╯

models that fit  (7) ───────────────────────────────────
  fit  model            quant    mem     speed
  gpu  Qwen3 0.6B       F16      2.2G    127 tok/s
  ~58% Qwen3 8B         Q8_0     9.2G    5.2 tok/s
```

- **fit**: `gpu` (fully on GPU), `~NN%` (that share offloaded to CPU RAM), `cpu`, or too large.
- **speed** is estimated from a memory-bandwidth model. A `~` prefix means the GPU
  wasn't in the bandwidth table and the number is a vendor-class guess.

Useful flags:

| Flag | Meaning |
|------|---------|
| `--ctx <n>` | context length to size the KV cache for (default 4096) |
| `--source catalog\|ollama\|all\|auto` | where models come from |
| `--family <name>` | filter by family, e.g. `llama` |
| `-v` | show every quant per model, not just the best |
| `--json` | machine-readable output |

`v2 check qwen3:8b` inspects a single model at every quant.

## 3. Manage models

Everything here talks to a running Ollama (`ollama serve`).

```bash
v2 pull qwen3:8b     # shows the fit preview, then downloads
v2 run qwen3:8b      # ensures it's installed, then opens a chat
v2 ps                # installed models, with fit info for this machine
v2 top               # what's loaded in Ollama right now (GPU/CPU split)
v2 rm qwen3:8b
```

`pull` is the point: it tells you *before* downloading whether the model will fit
and roughly how fast it'll run, so you don't pull 40 GB that then crawls.

## 4. Meter local usage

Point your apps at v2 instead of Ollama and it records exact token counts:

```bash
v2 serve                      # proxy on :11435 -> Ollama on :11434
# ... your app calls http://localhost:11435/api/chat ...
v2 usage                      # summary by day / model / source
```

Usage is stored as append-only JSONL under `~/.v2/usage/` — no database, and prompt
content is never written to disk, only token counts.

## 5. Share compute across a team

See [MESH.md](MESH.md). In short:

```bash
# admin
v2 mesh init
v2 mesh invite YOUR_HOST:4830
v2 serve --mesh-listen 0.0.0.0:4830

# teammate
v2 mesh join <ticket>
v2 mesh run qwen3:32b "hello"
```

## Troubleshooting

Run `v2 doctor` — it prints one badged line per subsystem (Ollama, identity, mesh
membership, policy) with an actionable hint for anything that's wrong.

Configuration and file layout are documented in [CONFIG.md](CONFIG.md).
