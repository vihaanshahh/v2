# CLAUDE.md

Read `AGENTS.md` first. It is the source of truth for repo layout, commands, and editing rules.

## Quick orientation

**v2** is a Rust CLI that answers: *given this machine, which LLMs can run?*

```
src/main.rs       CLI entry
src/hardware.rs   detect GPU/RAM/OS
src/models.rs     model catalog + types
src/engine.rs     VRAM estimation + fit logic
src/display.rs    terminal/JSON output
src/ollama.rs     local Ollama /api/tags
src/sources.rs    merge sources + enterprise filter
src/accepted.rs   allowlist loader
```

## Before committing

```bash
make check
```

## Common tasks

| Task | Where to edit |
|------|----------------|
| Add a model | `src/models.rs` catalog + Ollama tag |
| Change VRAM math | `src/engine.rs` |
| Change output format | `src/display.rs` |
| Ollama parsing | `src/ollama.rs` |
| Enterprise policy | `src/accepted.rs`, `src/sources.rs` |
| New CLI flag | `src/main.rs` |

## Install / ship

- Local dev: `cargo build --release`
- Release binary: tag `v*` → `.github/workflows/release.yml`
- End-user install: `curl -fsSL …/install.sh | bash`

Do not create extra README or doc files unless requested.
