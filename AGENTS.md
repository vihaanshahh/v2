# AGENTS.md

Scope: the whole `v2` repo. The core is a single Rust CLI binary — no hosted server.
`v2 serve` presents an interactive terminal control panel (line-based, no extra deps);
`--headless` keeps the old blocking proxy for systemd/daemons. `desktop/` adds an
optional native desktop app (Tauri) that reuses the `web/` UI and calls the `v2` lib
crate in-process — no HTTP, no subprocess. It's a separate consumer of the same core;
the CLI's own `serve` panel remains terminal-only and unaffected.

## What v2 does

Detects local hardware (GPU VRAM, RAM, OS) and estimates which LLMs can run, at which
quantization and how fast (tok/s), for a given context length. Models come from:

- a built-in **catalog** (`src/models.rs`) with Ollama tag mappings
- the local **Ollama** API (`GET /api/tags`) when `--source ollama|all|auto`
- an optional **enterprise allowlist** (`--accepted` or `V2_ACCEPTED`)

Beyond the scan, the `daemon` feature (on by default) adds a metering proxy, fit-aware
model management, and a secure org **mesh** for sharing compute. See `DESIGN.md` for the
full architecture, invariants, and failure harnesses. Ollama stays bound to localhost;
the mesh only ever talks to v2. No Docker, no hosted coordination server — one binary.

## Layout

| Path | Role |
|------|------|
| `src/lib.rs` | `v2` as a library — re-exports every module below as `pub mod` so `main.rs` and `desktop/src-tauri` can both consume them; the module list here mirrors `main.rs`'s old `mod` block exactly, including the `#[cfg(feature = "daemon")]` gates |
| `src/main.rs` | CLI (clap), command dispatch |
| `src/hardware.rs` | GPU/RAM/CPU detection (nvidia-smi, sysctl, /proc, wmic) |
| `src/models.rs` | `Model`, `Quant`, static catalog, param parsing |
| `src/engine.rs` | VRAM math + fit classification (`FullGpu`, partial offload, CPU, too big) |
| `src/bandwidth.rs` | Memory-bandwidth table + tok/s estimation (pure, always on) |
| `src/ui.rs` | Terminal UI primitives — panels, sections, badges, bars (width-aware) |
| `src/display.rs` | Scan output (framed panel + tables) and JSON |
| `src/ollama.rs` | Fetch/parse local Ollama models |
| `src/sources.rs` | Merge catalog + ollama, apply `--source` and allowlist |
| `src/accepted.rs` | Load/filter enterprise allowlist (line file or JSON) |

Daemon feature (`--features daemon`, default on; excluded by `--no-default-features`):

| Path | Role |
|------|------|
| `src/paths.rs` | `~/.v2` filesystem layout |
| `src/usage.rs` | Append-only JSONL metering store + summaries |
| `src/activity.rs` | Shared local-activity signal (yield-to-local) |
| `src/ollama_api.rs` | Ollama ps/pull/chat/delete (streaming) |
| `src/proxy.rs` | `serve` metering proxy on :11435 (loopback by default; `is_loopback` gates a lockdown warning) + `--cpu` thread cap (injects `options.num_thread`) |
| `src/console.rs` | interactive `serve` panel: one numbered model list driving find→install→open→close→limit→delete (TTY only, resize-aware, disk-guarded installs, usage glance) |
| `src/endpoints.rs` | registry for hosted remote models (e.g. Modal) in `~/.v2/endpoints.json`; OpenAI-compatible + remote-Ollama streaming chat. Needs the `remote` feature (ureq TLS) for HTTPS |
| `src/manage.rs` | `pull`/`run`/`rm`/`ps` — fit-aware wrappers |
| `src/policy.rs` | `policy.toml` + H1 admission gate (pure, tested) |
| `src/mesh/identity.rs` | ed25519 keys, org root, certs, tickets, revocation, federation |
| `src/mesh/transport.rs` | Noise_XX channel + channel-bound cert auth |
| `src/mesh/proto.rs` | Request/Frame/Receipt wire types |
| `src/mesh/serve.rs` | H1/H2/H3 serving pipeline + reclaim |
| `src/mesh/client.rs` | Enroll, remote run, admin/member control ops |
| `src/mesh/gossip.rs` | Node cards + known-peers list |

| `install.sh` | curl \| bash installer for release binaries |
| `.github/workflows/release.yml` | Cross-platform release builds on tag push |
| `Makefile` | `make check`, `make build`, `make package` |

Desktop app (native GUI, optional, separate from the CLI's own H6 boundary):

| Path | Role |
|------|------|
| `web/` | Vite + TypeScript frontend, four tabs: Scan (hardware/model fit), Models (pull/chat/remove, `views/models.ts`), Serve (proxy start/stop, usage, doctor, endpoint banner, `views/serve.ts`), Mesh (init/invite/join/peers/pause/resume/federation, `views/mesh.ts`). `web/src/platform.ts` calls `invoke()`/`listen()` when running inside Tauri; outside Tauri only Scan works (falls back to `fetch('/api/scan')`, the dev-server-only subprocess shim in `web/vite.config.ts`) — other tabs are disabled in the nav |
| `desktop/src-tauri/src/commands/` | `scan.rs`, `models.rs`, `serve.rs`, `mesh.rs` — one `#[tauri::command]` per CLI capability, each calling straight into the `v2` lib crate (daemon feature on) — no HTTP, no subprocess. `serve.rs` owns `ProxyState` (a `Mutex<Option<JoinHandle>>`) so start/stop work from the GUI without killing the app |

The scan path (`main` → `hardware`/`models`/`engine`/`bandwidth`/`display`) must stay
free of mesh/daemon imports so `--no-default-features` builds the CLI alone (H6).
`desktop/src-tauri` depending on `v2` with daemon on does not affect this — H6 only
constrains the plain CLI's `--no-default-features` build, which nothing here changes.

Known gap: chat-over-mesh (`v2 mesh run <model> <prompt>`, streaming through a
peer's Noise channel with receipt co-signing) is not yet wired into the desktop
app — the Mesh tab covers org admin (init/invite/join/peers/pause/resume/
federation) but not remote inference. Still CLI-only; a fast-follow if wanted.
Do not edit `target/` or `dist/`. Commit `Cargo.lock` (application crate).

## Commands (for agents)

```bash
make check              # test + release build
cargo run --              # default: hardware + model scan (now with tok/s)
cargo run -- models
cargo run -- check "qwen3:8b"
cargo run -- --source ollama
cargo run -- --accepted accepted-models.example
cargo build --no-default-features   # CLI only, no daemon/mesh (H6 boundary)

# daemon feature
cargo run -- serve --mesh-listen 0.0.0.0:4830   # metering proxy + mesh serving
cargo run -- top | usage | doctor
cargo run -- pull qwen3:8b        # fit-check then download
cargo run -- mesh init            # admin: create org
cargo run -- mesh invite HOST:4830
cargo run -- mesh join <ticket>   # member: two-command setup
cargo run -- mesh run qwen3:32b "hello"
```

Env vars: `OLLAMA_HOST`, `V2_ACCEPTED`. State lives under `~/.v2` (keys 0600).

## Editing rules

- Route human-facing output through `src/ui.rs` (panels, sections, badges) for a
  consistent look; keep `--json` output stable and machine-readable.
- Prefer real, detected values over hardcoded fallbacks: hardware via the OS,
  Ollama context length via `/api/show`, hostname via the system, version via
  `CARGO_PKG_VERSION`. Reference tables (model catalog, GPU bandwidth) are curated
  real data — mark estimates with a `~`.
- Prefer extending the catalog + Ollama tag map over one-off hacks in `engine.rs`.
- Ollama models with known `size` + `quantization_level` use exact weight bytes; catalog models estimate from params.
- Enterprise allowlist patterns support globs (`qwen3*`, `*:8b`).
- User-facing docs live in `docs/` and `README.md`; update this file when architecture changes.
- No emojis in code.

## Release

1. Bump `version` in `Cargo.toml`.
2. `make check`
3. Tag `v*` and push — GitHub Actions builds release assets.
4. Users install via `install.sh`.

See `CLAUDE.md` for the agent entrypoint summary.
