# v2 Desktop App Design

## Goal

Make v2 usable by non-technical people as a desktop app: install it, click to
join a mesh, chat with available models, and safely share a laptop's compute
without knowing ports, bearer tokens, policy files, or CLI commands.

The desktop app should be a thin product layer over the existing v2 daemon and
mesh. It should not fork the core serving, endpoint, metering, or trust logic.

## Recommendation

Use Tauri for the first desktop app.

Why:

- v2 is already Rust, so the desktop backend can reuse the current Rust code.
- Tauri is much lighter than Electron because it uses the system webview.
- It supports the desktop features this product needs: bundled binaries,
  native menus/windows, deep links, and updaters.
- The UI can be built with ordinary web tooling while keeping privileged work in
  Rust.

Avoid Electron for the first version unless the team decides web ecosystem speed
matters more than binary size and Rust integration. Avoid a pure egui app for the
first version unless we want a very utilitarian interface; this product needs a
polished onboarding and chat experience.

## Packaging Shape

Phase 1 should ship the current `v2` binary as a Tauri sidecar:

```text
v2-desktop
  Tauri shell
  web UI
  bundled v2 sidecar
  app-owned state under ~/.v2
```

The app launches:

```bash
v2 serve --headless --listen 127.0.0.1:11435
```

The UI talks to local v2 over loopback. This is the lowest-risk path because the
existing CLI, proxy, endpoint registry, mesh, policy, and metering code remain
the source of truth.

Phase 2 can split the repo into a shared library:

```text
v2-core      hardware scan, endpoint registry, policy, mesh, metering
v2-cli       current command-line interface
v2-desktop   Tauri app using v2-core directly
```

Do this after the desktop workflows stabilize. A premature core split would slow
down product validation.

## User Workflows

### First Run

The app opens to a status screen:

- Ollama found / missing.
- Local model count.
- Mesh membership status.
- Connected peer count.
- Sharing state: off, local-only, mesh serving, paused.

If Ollama is missing, the app explains the prerequisite and links to install it.
The desktop app should not silently install or start unrelated system services.

### Join a Mesh

The non-technical flow is:

1. User clicks an invite link, e.g. `v2://join/<ticket>`.
2. App opens the Join screen.
3. User clicks `Join`.
4. App calls the same join path as `v2 mesh join <ticket>`.
5. App shows connected peers and available models.

The fallback is a paste box for invite tickets.

### Invite Someone

The owner/admin flow is:

1. Click `Invite`.
2. Choose direct address or relay.
3. App creates a one-time ticket.
4. App displays copyable link and QR code.

The invite screen must explain whether the invite exposes a host address or uses
a relay route.

### Chat

The chat screen has:

- Model picker grouped by local, mesh peer, and hosted endpoint.
- Simple route label: `this laptop`, `Alex's MacBook`, `hosted endpoint`.
- Streaming chat transcript.
- Token and latency summary after each response.
- Visible stop button.

Users should not need to understand OpenAI endpoints. The app can still expose
the local OpenAI-compatible Base URL under advanced settings.

### Share This Laptop

Sharing is a top-level toggle, not a hidden setting.

Controls:

- Share on/off.
- Pause/resume.
- Max concurrent remote jobs.
- VRAM fraction.
- Serving hours.
- Require AC power.
- Hosted endpoint sharing toggle.

Defaults remain conservative:

- One remote job.
- Half VRAM.
- Require AC power.
- Yield to local activity.
- Hosted endpoint mesh sharing off.

### Endpoint Registry

The endpoint UI should collect:

- Friendly name.
- URL.
- API kind: OpenAI-compatible or remote Ollama.
- Provider model id.
- Optional provider key.

The app should reuse the daemon's normalization and validation rules:

- Strip common `/v1` and `/api` roots.
- Reject unsupported schemes.
- Reject username/password, query strings, and fragments.
- Block alias collisions with local Ollama tags or other endpoints.

Provider keys are never displayed by default. Revealing a key requires an
explicit advanced action.

## Local API Needed

The Tauri app can initially shell out to the CLI, but a small local admin API
will make the desktop app simpler and less brittle.

Add loopback-only JSON endpoints, protected by the same generated key:

```text
GET  /v2/status
GET  /v2/models
GET  /v2/peers
POST /v2/mesh/join
POST /v2/mesh/invite
POST /v2/mesh/pause
POST /v2/mesh/resume
POST /v2/endpoint
DELETE /v2/endpoint/:name
```

These endpoints are local control-plane APIs. They must not be exposed publicly
by default, and they should reuse existing command implementations instead of
introducing parallel state.

## Security Model

The desktop app does not weaken the daemon model:

- Ollama remains bound to localhost.
- Mesh peers talk only to v2.
- `/v1` remains key-gated by default.
- Network-exposed proxy paths require the bearer key.
- `V2_OPEN` / `endpoint.open` is loopback-only.
- Hosted endpoint sharing is opt-in with `endpoint.share_in_mesh = true`.
- App logs must not contain full bearer tokens or provider API keys.
- Deep links can carry invite tickets, not long-lived secrets.

The app should always provide a visible `Pause sharing` action. That is the
non-technical user's emergency control.

## Update Model

Ship signed installers for macOS, Windows, and Linux. Use the Tauri updater with
release metadata generated by CI.

Updates should be automatic or one-click, because this product has security
surface area. Old mesh and endpoint clients should not linger indefinitely.

## MVP Scope

Build the smallest useful desktop app in this order:

1. Tauri shell launches bundled `v2 serve --headless`.
2. Status screen with local models, peer count, and sharing state.
3. Join flow with paste ticket.
4. Chat screen using local `/v1/chat/completions`.
5. Mesh peer/model picker.
6. Pause/resume sharing.
7. Endpoint add/remove.
8. Deep link join.
9. Signed installer and updater.

Everything else is later: QR invites, richer usage charts, model downloads with
progress, relay setup wizards, and multi-org federation UI.

## Open Questions

- Should the app manage Ollama installation, or only detect and guide?
- Should the first release be macOS-only, or cross-platform from day one?
- Should chat history be stored locally? If yes, it needs an explicit privacy
  setting because mesh prompts currently stay out of v2's state files.
- Should the app expose the OpenAI-compatible endpoint by default, or keep it in
  advanced settings?
