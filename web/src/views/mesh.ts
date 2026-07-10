import type { FederatedOrg, MeshStatus, PeerCard } from "../types";
import {
  meshFederationList,
  meshInit,
  meshInvite,
  meshJoin,
  meshPause,
  meshPeerAdd,
  meshPeers,
  meshResume,
  meshRevoke,
  meshStatus,
} from "../platform";
import { escapeHtml } from "../util";

interface State {
  status: MeshStatus | null;
  peers: PeerCard[] | null;
  federation: FederatedOrg[] | null;
  error: string | null;
  busy: boolean;
  inviteAddr: string;
  inviteTicket: string | null;
  joinTicket: string;
  peerAddr: string;
  revokeNode: string;
}

const state: State = {
  status: null,
  peers: null,
  federation: null,
  error: null,
  busy: false,
  inviteAddr: "",
  inviteTicket: null,
  joinTicket: "",
  peerAddr: "",
  revokeNode: "",
};

function renderStatus(): string {
  const s = state.status;
  if (!s) return `<div class="status">Loading mesh status…</div>`;

  if (!s.is_member) {
    return `
      <div class="panel-box">
        <div class="panel-title">Not a member</div>
        <div class="hint">Node ${escapeHtml(s.node_id)} isn't part of an org mesh yet.</div>
        <div class="row">
          <button id="mesh-init" ${state.busy ? "disabled" : ""}>Create an org (become admin)</button>
        </div>
        <div class="row">
          <input type="text" id="join-ticket" placeholder="paste invite ticket" value="${escapeHtml(state.joinTicket)}" />
          <button id="mesh-join" ${state.busy ? "disabled" : ""}>Join</button>
        </div>
      </div>
    `;
  }

  return `
    <div class="panel-box">
      <div class="panel-title">Mesh status</div>
      <div class="kv-row"><span>Node</span><code>${escapeHtml(s.node_id)}</code></div>
      <div class="kv-row"><span>Org</span><code>${escapeHtml(s.org_id ?? "—")}</code></div>
      <div class="kv-row"><span>Role</span><span class="badge ${s.role === "admin" ? "fit-gpu" : "fit-cpu"}">${escapeHtml(s.role ?? "member")}</span></div>
      <div class="kv-row"><span>Cert</span><span class="dim">valid ${s.cert_valid_hours ?? "?"}h</span></div>
      <div class="kv-row"><span>Peers</span><span class="dim">${s.connected_peers} connected / ${s.known_peer_addrs.length} known</span></div>
      <div class="row">
        <button id="mesh-pause" ${state.busy ? "disabled" : ""}>Pause</button>
        <button id="mesh-resume" ${state.busy ? "disabled" : ""}>Resume</button>
      </div>
    </div>
  `;
}

function renderInvite(): string {
  if (!state.status?.is_member) return "";
  return `
    <div class="panel-box">
      <div class="panel-title">Invite a teammate</div>
      <div class="row">
        <input type="text" id="invite-addr" placeholder="your-host:4830" value="${escapeHtml(state.inviteAddr)}" />
        <button id="invite-go" ${state.busy ? "disabled" : ""}>Make ticket</button>
      </div>
      ${
        state.inviteTicket
          ? `<div class="ticket-box"><code>${escapeHtml(state.inviteTicket)}</code></div><div class="hint">Recipient runs: v2 mesh join &lt;ticket&gt;</div>`
          : ""
      }
    </div>
  `;
}

function renderPeers(): string {
  if (!state.status?.is_member) return "";
  const peers = state.peers ?? [];
  return `
    <div class="panel-box">
      <div class="panel-title">Peers
        <button class="mini" id="peers-refresh">Refresh</button>
      </div>
      <div class="row">
        <input type="text" id="peer-addr" placeholder="host:port" value="${escapeHtml(state.peerAddr)}" />
        <button id="peer-add" ${state.busy ? "disabled" : ""}>Add</button>
      </div>
      ${
        peers.length === 0
          ? `<div class="hint">no peers reachable — add one above</div>`
          : `
            <div class="table-wrap">
              <table>
                <thead><tr><th>Addr</th><th>Node</th><th>VRAM</th><th>Bandwidth</th><th>Busy</th><th>Models</th></tr></thead>
                <tbody>
                  ${peers
                    .map(
                      (p) => `
                        <tr>
                          <td>${escapeHtml(p.addr)}</td>
                          <td class="model-sub">${escapeHtml(p.card.node_pub.slice(0, 8))}</td>
                          <td>${p.card.vram_gb.toFixed(0)}G</td>
                          <td>${p.card.bandwidth_gbps.toFixed(0)} GB/s</td>
                          <td>${p.card.concurrent}/${p.card.max_concurrent}</td>
                          <td class="dim">${p.card.models.length + p.card.remote_models.length}</td>
                        </tr>
                      `,
                    )
                    .join("")}
                </tbody>
              </table>
            </div>
          `
      }
      <div class="row" style="margin-top:10px">
        <input type="text" id="revoke-node" placeholder="node id to revoke (admin only)" value="${escapeHtml(state.revokeNode)}" />
        <button class="danger" id="revoke-go" ${state.busy ? "disabled" : ""}>Revoke</button>
      </div>
    </div>
  `;
}

function renderFederation(): string {
  if (!state.status?.is_member) return "";
  const orgs = state.federation ?? [];
  return `
    <div class="panel-box">
      <div class="panel-title">Federation</div>
      ${
        orgs.length === 0
          ? `<div class="hint">no federated orgs</div>`
          : orgs
              .map(
                (o) => `
                  <div class="kv-row">
                    <span>${escapeHtml(o.org_pub.slice(0, 8))}</span>
                    <span class="dim">${escapeHtml(o.note || "—")}</span>
                    <span class="dim">${escapeHtml(o.allowed_models.join(", ") || "no models")}</span>
                  </div>
                `,
              )
              .join("")
      }
    </div>
  `;
}

function render(container: HTMLElement): void {
  container.innerHTML = `
    ${state.error ? `<div class="status error">${escapeHtml(state.error)}</div>` : ""}
    ${renderStatus()}
    ${renderInvite()}
    ${renderPeers()}
    ${renderFederation()}
  `;
  bind(container);
}

function bind(container: HTMLElement): void {
  container.querySelector("#mesh-init")?.addEventListener("click", () => void doInit(container));
  container.querySelector("#mesh-join")?.addEventListener("click", () => void doJoin(container));
  container.querySelector("#join-ticket")?.addEventListener("input", (e) => {
    state.joinTicket = (e.target as HTMLInputElement).value;
  });

  container.querySelector("#mesh-pause")?.addEventListener("click", () => void withBusy(container, meshPause));
  container.querySelector("#mesh-resume")?.addEventListener("click", () => void withBusy(container, meshResume));

  container.querySelector("#invite-addr")?.addEventListener("input", (e) => {
    state.inviteAddr = (e.target as HTMLInputElement).value;
  });
  container.querySelector("#invite-go")?.addEventListener("click", () => void doInvite(container));

  container.querySelector("#peer-addr")?.addEventListener("input", (e) => {
    state.peerAddr = (e.target as HTMLInputElement).value;
  });
  container.querySelector("#peer-add")?.addEventListener("click", () => void doPeerAdd(container));
  container.querySelector("#peers-refresh")?.addEventListener("click", () => void loadPeers(container));

  container.querySelector("#revoke-node")?.addEventListener("input", (e) => {
    state.revokeNode = (e.target as HTMLInputElement).value;
  });
  container.querySelector("#revoke-go")?.addEventListener("click", () => void doRevoke(container));
}

async function withBusy(container: HTMLElement, fn: () => Promise<void>): Promise<void> {
  state.busy = true;
  state.error = null;
  render(container);
  try {
    await fn();
    await loadStatus(container);
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Action failed";
  } finally {
    state.busy = false;
    render(container);
  }
}

async function doInit(container: HTMLElement): Promise<void> {
  await withBusy(container, meshInit);
}

async function doJoin(container: HTMLElement): Promise<void> {
  const ticket = state.joinTicket.trim();
  if (!ticket) return;
  await withBusy(container, () => meshJoin(ticket));
}

async function doInvite(container: HTMLElement): Promise<void> {
  state.busy = true;
  state.error = null;
  render(container);
  try {
    state.inviteTicket = await meshInvite(state.inviteAddr.trim() || undefined);
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Invite failed";
  } finally {
    state.busy = false;
    render(container);
  }
}

async function doPeerAdd(container: HTMLElement): Promise<void> {
  const addr = state.peerAddr.trim();
  if (!addr) return;
  await withBusy(container, () => meshPeerAdd(addr));
  state.peerAddr = "";
  await loadPeers(container);
}

async function doRevoke(container: HTMLElement): Promise<void> {
  const node = state.revokeNode.trim();
  if (!node) return;
  await withBusy(container, () => meshRevoke(node));
  state.revokeNode = "";
}

async function loadStatus(container: HTMLElement): Promise<void> {
  state.status = await meshStatus();
  render(container);
}

async function loadPeers(container: HTMLElement): Promise<void> {
  try {
    state.peers = await meshPeers();
  } catch {
    state.peers = [];
  }
  render(container);
}

async function loadFederation(container: HTMLElement): Promise<void> {
  try {
    state.federation = await meshFederationList();
  } catch {
    state.federation = [];
  }
  render(container);
}

export async function mount(container: HTMLElement): Promise<void> {
  render(container);
  try {
    await loadStatus(container);
    if (state.status?.is_member) {
      void loadPeers(container);
      void loadFederation(container);
    }
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Failed to load mesh status";
    render(container);
  }
}

export function unmount(): void {
  // no subscriptions to tear down
}
