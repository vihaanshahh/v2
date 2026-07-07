#!/usr/bin/env bash
#
# Two-node mesh demo — runs "laptop A" (admin/server) and "laptop B" (member/
# client) on one machine using two isolated ~/.v2 homes and one shared Ollama.
# It walks the whole lifecycle: enrol, ACCEPT + serve a job, then three kinds of
# TERMINATION (pause, resume, revoke), and finally the signed receipts.
#
# Usage:  bash demo/two-node.sh
# Needs:  a built ./target/release/v2 (or ./target/debug/v2) and Ollama running.

set -euo pipefail
cd "$(dirname "$0")/.."

V2="./target/release/v2"; [ -x "$V2" ] || V2="./target/debug/v2"
[ -x "$V2" ] || { echo "build first: cargo build --release"; exit 1; }

MODEL="qwen3:0.6b"
MESH_PORT="47600"
PROXY_PORT="11599"       # non-default so it won't clash with a real proxy
OLLAMA="${OLLAMA_HOST:-http://127.0.0.1:11434}"

HOME_A="$(mktemp -d)"; HOME_B="$(mktemp -d)"
SERVE_PID=""
cleanup() {
  [ -n "$SERVE_PID" ] && kill "$SERVE_PID" 2>/dev/null || true
  rm -rf "$HOME_A" "$HOME_B"
}
trap cleanup EXIT

a() { HOME="$HOME_A" "$V2" "$@"; }   # laptop A
b() { HOME="$HOME_B" "$V2" "$@"; }   # laptop B

hr() { printf '\n\033[36m── %s ──────────────────────────────────────────\033[0m\n' "$1"; }

# ── Preflight ────────────────────────────────────────────────────────────────
curl -s -m 3 "$OLLAMA/api/tags" >/dev/null || { echo "Ollama not reachable at $OLLAMA — run 'ollama serve'"; exit 1; }
curl -s "$OLLAMA/api/tags" | grep -q "$MODEL" || { echo "pulling $MODEL ..."; ollama pull "$MODEL"; }

# ── 1. Laptop A creates the org and starts serving ───────────────────────────
hr "SETUP · laptop A creates an org and serves"
a mesh init >/dev/null
# A chooses to share this machine generously for the demo. (The safe DEFAULT is
# half the VRAM, which on a small GPU won't fit even a tiny model — that's the
# conservative default doing its job, not a bug.)
cat > "$HOME_A/.v2/policy.toml" <<'POL'
[serve]
max_vram_fraction = 1.0
allowed_models    = ["*"]
[availability]
require_ac_power = false
yield_to_local   = true
POL
TICKET="$(a mesh invite "127.0.0.1:$MESH_PORT" --ttl 3600 | grep -E '^[A-Za-z0-9+/=]{40,}$')"
echo "A: org created, sharing policy written, invite ticket minted (${#TICKET} chars)"

HOME="$HOME_A" "$V2" serve --listen "127.0.0.1:$PROXY_PORT" --mesh-listen "127.0.0.1:$MESH_PORT" \
  >/tmp/v2-demo-serve.log 2>&1 &
SERVE_PID=$!
disown "$SERVE_PID" 2>/dev/null || true   # don't print a job-control notice on cleanup
for _ in $(seq 1 20); do (echo >"/dev/tcp/127.0.0.1/$MESH_PORT") 2>/dev/null && break; sleep 0.2; done
echo "A: serving on 127.0.0.1:$MESH_PORT (pid $SERVE_PID)"

# ── 2. Laptop B joins ────────────────────────────────────────────────────────
hr "JOIN · laptop B joins the org"
b mesh join "$TICKET" 2>/dev/null | sed 's/^/B: /'
B_ID="$(b mesh id)"
echo "B: node id $B_ID"

# ── 3. ACCEPT · B runs a job on A, A accepts and streams it back ──────────────
hr "ACCEPT + RUN · B runs a job on A"
b mesh run "$MODEL" "In one short sentence, what is a mesh network?" || true

# ── 4. TERMINATION by pause · A stops offering, in-flight/new work is refused ─
hr "TERMINATE (pause) · A reclaims its machine"
a mesh pause | sed 's/^/A: /'
sleep 2  # let A's serve pick up the pause flag
echo "B: trying again while A is paused —"
b mesh run "$MODEL" "this should be refused" || true

# ── 5. Resume · A offers compute again ───────────────────────────────────────
hr "RESUME · A offers compute again"
a mesh resume | sed 's/^/A: /'
sleep 2
b mesh run "$MODEL" "In three words, say you are back." || true

# ── 6. TERMINATION by revoke · A permanently cuts B off ──────────────────────
hr "TERMINATE (revoke) · A revokes B"
a mesh revoke "$B_ID" | sed 's/^/A: /'
echo "B: trying after being revoked —"
b mesh run "$MODEL" "this should be rejected" || true

# ── 7. Signed receipts on A ──────────────────────────────────────────────────
hr "RECEIPTS · A verifies what it served"
a mesh receipts

hr "DONE"
echo "Both homes and the server were temporary; cleaning up."
