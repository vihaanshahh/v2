import type { DoctorReport, DoctorStatus, EndpointInfo, ServeStatus, UsageSummary } from "../types";
import { doctor, endpointBanner, serveStart, serveStatus, serveStop, usageSummary } from "../platform";
import { escapeHtml } from "../util";

interface State {
  status: ServeStatus | null;
  busy: boolean;
  error: string | null;
  doctor: DoctorReport | null;
  usage: UsageSummary | null;
  endpoint: EndpointInfo | null;
  endpointError: string | null;
}

const state: State = {
  status: null,
  busy: false,
  error: null,
  doctor: null,
  usage: null,
  endpoint: null,
  endpointError: null,
};

function badgeClass(s: DoctorStatus): string {
  return s === "ok" ? "fit-gpu" : s === "warn" ? "fit-partial" : "fit-nope";
}

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1e3).toFixed(1)}K`;
  return String(n);
}

function renderServeToggle(): string {
  const running = state.status?.running ?? false;
  return `
    <div class="panel-box">
      <div class="panel-title">Metering proxy</div>
      <div class="row">
        <span class="badge ${running ? "fit-gpu" : "fit-cpu"}">${running ? "running" : "stopped"}</span>
        <button id="serve-toggle" ${state.busy ? "disabled" : ""}>
          ${state.busy ? "…" : running ? "Stop serving" : "Start serving"}
        </button>
      </div>
      ${state.error ? `<div class="status error">${escapeHtml(state.error)}</div>` : ""}
    </div>
  `;
}

function renderDoctor(): string {
  if (!state.doctor) return "";
  const lines: [string, { status: DoctorStatus; label: string; message: string }][] = [
    ["ollama", state.doctor.ollama],
    ["identity", state.doctor.identity],
    ["mesh", state.doctor.mesh],
    ["policy", state.doctor.policy],
    ["abuse", state.doctor.abuse],
  ];
  return `
    <div class="panel-box">
      <div class="panel-title">Doctor</div>
      ${lines
        .map(
          ([, l]) => `
            <div class="doctor-line">
              <span class="badge ${badgeClass(l.status)}">${l.status}</span>
              <span class="doctor-label">${escapeHtml(l.label)}</span>
              <span class="dim">${escapeHtml(l.message)}</span>
            </div>
          `,
        )
        .join("")}
    </div>
  `;
}

function renderUsage(): string {
  const u = state.usage;
  if (!u) return "";
  if (u.total_requests === 0) {
    return `<div class="panel-box"><div class="panel-title">Usage</div><div class="hint">no records yet — start serving and route apps through the proxy</div></div>`;
  }
  const group = (title: string, rows: UsageSummary["by_model"]) => `
    <div class="usage-group">
      <div class="usage-group-title">${title}</div>
      ${rows
        .map(
          (r) => `
            <div class="usage-row">
              <span>${escapeHtml(r.key)}</span>
              <span class="dim">${r.requests} req</span>
              <span class="dim">${fmtTokens(r.tokens_out)} out</span>
              <span class="dim">${r.tps.toFixed(0)} tok/s</span>
            </div>
          `,
        )
        .join("")}
    </div>
  `;
  return `
    <div class="panel-box">
      <div class="panel-title">Usage — ${u.total_requests} requests · ${fmtTokens(u.total_tokens_in)} in · ${fmtTokens(u.total_tokens_out)} out</div>
      ${group("by model", u.by_model)}
      ${group("by day", u.by_day)}
    </div>
  `;
}

function renderEndpoint(): string {
  return `
    <div class="panel-box">
      <div class="panel-title">OpenAI-compatible endpoint
        <button class="mini" id="endpoint-refresh">Refresh</button>
      </div>
      ${
        state.endpointError
          ? `<div class="status error">${escapeHtml(state.endpointError)}</div>`
          : state.endpoint
            ? `
              <div class="kv-row"><span>Base URL</span><code>${escapeHtml(state.endpoint.base_url)}</code></div>
              ${state.endpoint.local_url ? `<div class="kv-row"><span>(local)</span><code>${escapeHtml(state.endpoint.local_url)}</code></div>` : ""}
              <div class="kv-row"><span>API key</span><code>${escapeHtml(state.endpoint.api_key)}</code></div>
              <div class="kv-row"><span>Models</span><span class="dim">${escapeHtml(state.endpoint.models.join(", ") || "none installed")}</span></div>
            `
            : `<div class="hint">not loaded yet</div>`
      }
    </div>
  `;
}

function render(container: HTMLElement): void {
  container.innerHTML = `
    ${renderServeToggle()}
    ${renderEndpoint()}
    ${renderUsage()}
    ${renderDoctor()}
  `;
  bind(container);
}

function bind(container: HTMLElement): void {
  container.querySelector("#serve-toggle")?.addEventListener("click", () => void toggleServe(container));
  container.querySelector("#endpoint-refresh")?.addEventListener("click", () => void loadEndpoint(container));
}

async function toggleServe(container: HTMLElement): Promise<void> {
  state.busy = true;
  state.error = null;
  render(container);
  try {
    if (state.status?.running) {
      await serveStop();
    } else {
      await serveStart();
    }
    state.status = await serveStatus();
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Failed to toggle serving";
  } finally {
    state.busy = false;
    render(container);
    void loadUsage(container);
    void loadDoctor(container);
  }
}

async function loadDoctor(container: HTMLElement): Promise<void> {
  try {
    state.doctor = await doctor();
    render(container);
  } catch {
    // desktop-only feature outside Tauri; leave doctor panel empty
  }
}

async function loadUsage(container: HTMLElement): Promise<void> {
  try {
    state.usage = await usageSummary();
    render(container);
  } catch {
    // ignore
  }
}

async function loadEndpoint(container: HTMLElement): Promise<void> {
  state.endpointError = null;
  try {
    state.endpoint = await endpointBanner();
  } catch (err) {
    state.endpointError = err instanceof Error ? err.message : "Failed to load endpoint info";
  }
  render(container);
}

export async function mount(container: HTMLElement): Promise<void> {
  try {
    state.status = await serveStatus();
  } catch {
    state.status = null;
  }
  render(container);
  void loadDoctor(container);
  void loadUsage(container);
  void loadEndpoint(container);
}

export function unmount(): void {
  // no subscriptions to tear down
}
