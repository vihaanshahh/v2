# v2 Configuration & Files

## File layout — `~/.v2/`

Everything v2 writes lives under `~/.v2/`:

```
~/.v2/
  key                     node identity (ed25519 seed, mode 0600)
  policy.toml             serving policy (optional)
  usage/day-<n>.jsonl     append-only metering log (one line per request)
  mesh/
    org_root.key          org signing key — admin only, mode 0600
    org.json              trusted org pubkey + this node's membership cert
    revoked.json          revocation list
    peers.json            known peer addresses
    federation.json       federated orgs and their scopes
    used_nonces.json      spent invite-ticket nonces (admin)
    receipts/             signed usage receipts
```

Delete `~/.v2` to reset all state. The identity and org-root keys are secrets —
they're written with `0600` permissions on Unix.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `OLLAMA_HOST` | Ollama base URL (default `http://127.0.0.1:11434`) |
| `V2_ACCEPTED` | path to an enterprise allowlist file |
| `COLUMNS` | overrides detected terminal width for the UI |

## `policy.toml`

Governs what your machine will do for mesh peers. Absent file = the defaults below,
which are already safe. A parse error makes `serve` refuse to start (fail closed),
but the plain CLI keeps working.

```toml
[serve]
# Most remote jobs to run at once.
max_concurrent_remote = 1
# Ceiling on the fraction of total memory remote jobs may use (0.0–1.0).
max_vram_fraction = 0.5
# Model globs peers may request. ["*"] allows any installed model.
allowed_models = ["qwen3:8b", "llama3.2:*"]
# Largest context length a peer may request.
max_ctx = 8192
# Hard per-request wall-clock timeout, seconds.
request_timeout_s = 120

[quota]
# Per-peer sliding-window token budget.
per_peer_tokens_per_hour = 200_000

[availability]
# "always" or a UTC window like "09:00-18:00" (overnight ranges allowed).
hours = "always"
# Refuse to serve on battery.
require_ac_power = true
# Preempt remote work the instant the owner uses the machine.
yield_to_local = true
# Seconds of local inactivity before remote work is allowed again.
local_cooldown_s = 60
```

### Admission order

A remote request is checked in this order — cheapest and most security-critical
first — and is **accepted**, **queued** (temporarily full, try later), or **refused**
(policy said no, don't retry the same request here):

1. Certificate valid, unexpired, not revoked *(transport layer)*
2. Model in `allowed_models`
3. Context ≤ `max_ctx`
4. Peer under `per_peer_tokens_per_hour`
5. On AC power (if required), within `hours`, owner idle (if `yield_to_local`)
6. A concurrency slot free **and** projected VRAM within `max_vram_fraction`

Steps 2–5 refuse; step 6 queues.

## Enterprise allowlist

Independent of the mesh, you can restrict which models the scan/catalog will show:

```bash
v2 --accepted my-allowlist.txt
v2 --enterprise --accepted my-allowlist.txt   # only allowlisted models
```

The file is one model or glob per line (`qwen3*`, `*:8b`), or a JSON
`{ "accepted": [...] }`. See `accepted-models.example`.
