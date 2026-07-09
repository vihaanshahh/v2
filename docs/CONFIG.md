# v2 Configuration & Files

## File layout — `~/.v2/`

Everything v2 writes lives under `~/.v2/` (directories are kept `0700` on Unix):

```
~/.v2/
  key                     node identity (ed25519 seed, mode 0600)
  api_key                 OpenAI-compatible /v1 bearer key (mode 0600)
  endpoints.json          registered hosted/remote endpoints + optional keys (mode 0600)
  policy.toml             serving policy (optional)
  usage/day-<n>.jsonl     append-only metering log (one line per request, mode 0600)
  mesh/
    org_root.key          org signing key — admin only, mode 0600
    org.json              trusted org pubkey + this node's membership cert (mode 0600)
    revoked.json          revocation list (mode 0600)
    peers.json            known peer addresses + pinned node ids (mode 0600)
    federation.json       federated orgs and their scopes (mode 0600)
    used_nonces.json      spent invite-ticket nonces (admin, mode 0600)
    receipts/             signed usage receipts (files mode 0600)
```

Delete `~/.v2` to reset all state. The identity, org-root, API, endpoint, peer,
and receipt files are private on Unix.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `OLLAMA_HOST` | Ollama base URL (default `http://127.0.0.1:11434`) |
| `V2_ACCEPTED` | path to an enterprise allowlist file |
| `V2_PUBLIC_URL` | public/tunnel URL advertised by `v2 endpoint` |
| `V2_API_KEY` | explicit bearer token for the `/v1` OpenAI-compatible surface |
| `V2_OPEN` | `1`/`true` disables `/v1` auth only on loopback with no public URL |
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

[abuse]
# Flood / DoS controls, applied to EVERY connection — members included.
# The by-IP checks run before the Noise handshake, so a flood is dropped in
# microseconds without spawning a thread or doing crypto.
max_connections          = 256    # global simultaneous connections
max_connections_per_ip   = 16
handshake_rate_per_min   = 60     # new connections per IP, averaged
handshake_burst          = 20     # short-term burst allowance per IP
# A node whose requests are refused this many times within the window is
# temporarily banned (a self-inflicted cooldown for probing the limits).
strike_limit   = 10
strike_window_s = 60
ban_secs        = 300
# Ceiling on tokens served to all peers combined, per hour.
global_tokens_per_hour = 2_000_000
# Hard control by node id (base64), beyond revocation:
deny_nodes = []       # always refused
only_nodes = []       # if non-empty, ONLY these nodes may be served

[endpoint]
# Public/tunnel base URL to advertise. `/v1` is appended for clients; if you
# include `/v1` here it is normalized away.
public_url = ""
# Empty = auto-create a 256-bit bearer key at ~/.v2/api_key.
api_key = ""
# Disable the bearer gate entirely. Only safe for trusted loopback-only use.
open = false
# Advertise and broker registered hosted endpoints to mesh peers. Off by
# default because those calls may spend provider API keys.
share_in_mesh = false
```

When `public_url` / `V2_PUBLIC_URL` is set, or `serve --listen` binds a
non-loopback address, all proxy paths require the bearer key. `open = true` /
`V2_OPEN=1` is refused in those modes. Availability `hours` must be `always` or
`HH:MM-HH:MM`; malformed values make serving fail closed.

### Abuse-control layers

1. **By IP, pre-handshake:** a per-IP token bucket rate-limits new connections,
   and global + per-IP caps bound concurrency. Rejected here in microseconds.
2. **By node id, post-auth:** `deny_nodes` / `only_nodes` and active bans.
3. **Per request:** ban check, the global tokens/hour ceiling, and a strike on
   each admission refusal (which can escalate to a temporary ban).

The rate limit applies to members too — a stolen or compromised member key is
exactly the source that would flood you.

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
