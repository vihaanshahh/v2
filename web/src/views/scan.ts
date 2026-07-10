import {
  bestQuant,
  fitClass,
  fitLabel,
  type ModelResult,
  type ScanResult,
  type Source,
} from "../types";
import { scan } from "../platform";
import { escapeHtml } from "../util";

interface State {
  loading: boolean;
  error: string | null;
  data: ScanResult | null;
  ctx: number;
  source: Source;
  family: string;
  query: string;
  runnableOnly: boolean;
}

const state: State = {
  loading: false,
  error: null,
  data: null,
  ctx: 4096,
  source: "auto",
  family: "",
  query: "",
  runnableOnly: false,
};

function formatTps(value: number | null | undefined): string {
  if (value == null) return "—";
  return `${value.toFixed(1)} tok/s`;
}

function filterModels(models: ModelResult[]): ModelResult[] {
  const q = state.query.trim().toLowerCase();
  return models.filter((m) => {
    if (state.runnableOnly && !m.can_run) return false;
    if (!q) return true;
    const hay = [m.display_name, m.name, m.family, m.ollama_name ?? "", m.id]
      .join(" ")
      .toLowerCase();
    return hay.includes(q);
  });
}

function renderHardware(data: ScanResult): string {
  const { hardware: hw } = data;
  const gpu =
    hw.gpus.length === 0
      ? "none — CPU inference"
      : hw.gpus
          .map((g) => {
            const mem = g.shared ? `${g.vram_gb.toFixed(0)}G unified` : `${g.vram_gb.toFixed(0)}G VRAM`;
            return `${g.name} · ${mem}`;
          })
          .join("; ");

  return `
    <div class="hw-grid">
      <div class="hw-card"><span>GPU</span><strong>${escapeHtml(gpu)}</strong></div>
      <div class="hw-card"><span>Memory</span><strong>${escapeHtml(hw.ram_gb)}G RAM · ${escapeHtml(hw.os)}</strong></div>
      <div class="hw-card"><span>CPU</span><strong>${escapeHtml(hw.cpu)}</strong></div>
      <div class="hw-card"><span>Scan</span><strong>ctx ${(state.ctx / 1000).toFixed(state.ctx >= 1000 ? 0 : 1)}k · ${escapeHtml(state.source)}</strong></div>
    </div>
  `;
}

function renderTable(models: ModelResult[]): string {
  if (models.length === 0) {
    return `<div class="status">No models match your filters.</div>`;
  }

  const rows = models
    .map((m) => {
      const best = bestQuant(m);
      const fit = best?.fit ?? "too_big";
      const moe = m.is_moe ? "*" : "";
      return `
        <tr>
          <td>
            <div class="model-name">${escapeHtml(m.display_name)}${moe}</div>
            <div class="model-sub">${escapeHtml(m.ollama_name ?? m.id)}</div>
          </td>
          <td>${escapeHtml(m.family)}</td>
          <td><span class="badge ${fitClass(fit)}">${fitLabel(fit)}</span></td>
          <td>${escapeHtml(best?.quant ?? "—")}</td>
          <td>${escapeHtml(best?.vram_gb ?? "—")}G</td>
          <td class="tps"><span class="tps-est">${formatTps(best?.est_tps ?? null)}</span></td>
          <td class="origin">${escapeHtml(m.origin)}</td>
        </tr>
      `;
    })
    .join("");

  return `
    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Model</th>
            <th>Family</th>
            <th>Fit</th>
            <th>Quant</th>
            <th>Mem</th>
            <th>Speed</th>
            <th>Src</th>
          </tr>
        </thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  `;
}

export function render(container: HTMLElement): void {
  const runnable = state.data ? state.data.models.filter((m) => m.can_run).length : 0;
  const total = state.data?.models.length ?? 0;
  const shown = state.data ? filterModels(state.data.models).length : 0;

  container.innerHTML = `
    <div class="controls-row">
      <label>Context
        <input type="number" id="ctx" min="512" max="131072" step="512" value="${state.ctx}" />
      </label>
      <label>Source
        <select id="source">
          ${(["auto", "catalog", "ollama", "all"] as Source[])
            .map((s) => `<option value="${s}" ${state.source === s ? "selected" : ""}>${s}</option>`)
            .join("")}
        </select>
      </label>
      <label>Family
        <input type="text" id="family" placeholder="qwen3" value="${escapeHtml(state.family)}" />
      </label>
      <button id="scan" ${state.loading ? "disabled" : ""}>
        ${state.loading ? "Scanning…" : "Scan"}
      </button>
    </div>

    ${
      state.error
        ? `<div class="status error">${escapeHtml(state.error)}</div>`
        : state.loading
          ? `<div class="status">Detecting hardware and ranking models…</div>`
          : state.data
            ? `
              ${renderHardware(state.data)}
              <div class="toolbar">
                <input type="search" id="query" placeholder="Filter models…" value="${escapeHtml(state.query)}" />
                <label><input type="checkbox" id="runnable" ${state.runnableOnly ? "checked" : ""} /> Runnable only</label>
              </div>
              ${renderTable(filterModels(state.data.models))}
              <div class="footer">
                <span>${shown} shown · ${runnable} runnable of ${total}</span>
                <span>Speed is estimated from memory bandwidth (~ prefix)</span>
              </div>
            `
            : `<div class="status">Hit Scan to research this machine.</div>`
    }
  `;

  bind(container);
}

function bind(container: HTMLElement): void {
  container.querySelector("#scan")?.addEventListener("click", () => void runScan(container));

  container.querySelector("#ctx")?.addEventListener("change", (e) => {
    state.ctx = Number((e.target as HTMLInputElement).value) || 4096;
  });

  container.querySelector("#source")?.addEventListener("change", (e) => {
    state.source = (e.target as HTMLSelectElement).value as Source;
  });

  container.querySelector("#family")?.addEventListener("input", (e) => {
    state.family = (e.target as HTMLInputElement).value;
  });

  container.querySelector("#query")?.addEventListener("input", (e) => {
    state.query = (e.target as HTMLInputElement).value;
    render(container);
  });

  container.querySelector("#runnable")?.addEventListener("change", (e) => {
    state.runnableOnly = (e.target as HTMLInputElement).checked;
    render(container);
  });
}

async function runScan(container: HTMLElement): Promise<void> {
  state.loading = true;
  state.error = null;
  render(container);

  try {
    state.data = await scan(state.ctx, state.source, state.family);
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Scan failed";
    state.data = null;
  } finally {
    state.loading = false;
    render(container);
  }
}

export function mount(container: HTMLElement): void {
  render(container);
  if (!state.data && !state.loading) void runScan(container);
}
