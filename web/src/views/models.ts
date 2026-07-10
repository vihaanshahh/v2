import type { ChatMessage, FitCheck, InstalledModel, PullProgress, RunningModel } from "../types";
import { fitClass, fitLabel } from "../types";
import {
  modelChat,
  modelFitCheck,
  modelPull,
  modelRm,
  modelStop,
  modelsInstalled,
  modelsLoaded,
  onChatToken,
  onPullProgress,
} from "../platform";
import { escapeHtml } from "../util";

const CTX = 4096;

interface State {
  loading: boolean;
  error: string | null;
  installed: InstalledModel[] | null;
  loaded: RunningModel[] | null;
  query: string;
  fitPreview: FitCheck | null;
  fitBusy: boolean;
  pulling: string | null;
  pullProgress: PullProgress | null;
  pullError: string | null;
  chatModel: string | null;
  chatMessages: ChatMessage[];
  chatInput: string;
  chatBusy: boolean;
  chatStream: string;
}

const state: State = {
  loading: false,
  error: null,
  installed: null,
  loaded: null,
  query: "",
  fitPreview: null,
  fitBusy: false,
  pulling: null,
  pullProgress: null,
  pullError: null,
  chatModel: null,
  chatMessages: [],
  chatInput: "",
  chatBusy: false,
  chatStream: "",
};

let unlistenPull: (() => void) | null = null;
let unlistenChat: (() => void) | null = null;
let fitTimer: ReturnType<typeof setTimeout> | null = null;
let currentContainer: HTMLElement | null = null;

function fmtGb(n: number | null): string {
  return n == null ? "—" : `${n.toFixed(1)}G`;
}

function renderPullPanel(): string {
  const preview = state.fitPreview;
  return `
    <div class="panel-box">
      <div class="panel-title">Pull a model</div>
      <div class="row">
        <input type="text" id="pull-query" placeholder="qwen3:8b" value="${escapeHtml(state.query)}" />
        <button id="pull-go" ${state.pulling ? "disabled" : ""}>${state.pulling ? "Pulling…" : "Pull"}</button>
      </div>
      ${
        state.fitBusy
          ? `<div class="hint">checking fit…</div>`
          : preview
            ? `
              <div class="fit-preview">
                <span class="model-name">${escapeHtml(preview.display_name)}</span>
                <span class="badge ${fitClass(preview.fit)}">${fitLabel(preview.fit)}</span>
                ${preview.quant ? `<span class="dim">${escapeHtml(preview.quant)}</span>` : ""}
                ${preview.vram_gb != null ? `<span class="dim">${fmtGb(preview.vram_gb)}</span>` : ""}
                ${preview.est_tps != null ? `<span class="dim">~${preview.est_tps.toFixed(1)} tok/s</span>` : ""}
              </div>
              ${preview.notes.length ? `<div class="hint">${preview.notes.map(escapeHtml).join(" · ")}</div>` : ""}
            `
            : ""
      }
      ${
        state.pulling
          ? `<div class="progress-line">${renderProgress()}</div>`
          : ""
      }
      ${state.pullError ? `<div class="status error">${escapeHtml(state.pullError)}</div>` : ""}
    </div>
  `;
}

function renderProgress(): string {
  const p = state.pullProgress;
  if (!p) return "starting…";
  if (p.total > 0) {
    const pct = (p.completed / p.total) * 100;
    return `${escapeHtml(p.status)} — ${pct.toFixed(1)}% (${(p.completed / 1e9).toFixed(1)}/${(p.total / 1e9).toFixed(1)}G)`;
  }
  return escapeHtml(p.status);
}

function renderInstalled(): string {
  if (state.loading) return `<div class="status">Loading installed models…</div>`;
  if (state.error) return `<div class="status error">${escapeHtml(state.error)}</div>`;
  if (!state.installed || state.installed.length === 0) {
    return `<div class="status">No models installed yet — pull one above.</div>`;
  }

  const loadedNames = new Set((state.loaded ?? []).map((m) => m.name));

  const rows = state.installed
    .map((m) => {
      const isLoaded = loadedNames.has(m.name);
      return `
        <tr>
          <td>
            <div class="model-name">${escapeHtml(m.display_name)}</div>
            <div class="model-sub">${escapeHtml(m.name)}</div>
          </td>
          <td>${fmtGb(m.size_gb)}</td>
          <td><span class="badge ${fitClass(m.fit)}">${fitLabel(m.fit)}</span></td>
          <td class="tps">${m.tps_label ? escapeHtml(m.tps_label) : "—"}</td>
          <td>${isLoaded ? `<span class="badge fit-gpu">loaded</span>` : ""}</td>
          <td class="row-actions">
            <button class="mini" data-chat="${escapeHtml(m.name)}">Chat</button>
            ${isLoaded ? `<button class="mini" data-stop="${escapeHtml(m.name)}">Unload</button>` : ""}
            <button class="mini danger" data-rm="${escapeHtml(m.name)}">Remove</button>
          </td>
        </tr>
      `;
    })
    .join("");

  return `
    <div class="table-wrap">
      <table>
        <thead>
          <tr><th>Model</th><th>Size</th><th>Fit</th><th>Speed</th><th></th><th></th></tr>
        </thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  `;
}

function renderChat(): string {
  if (!state.chatModel) {
    return `<div class="status">Pick "Chat" on an installed model to start a conversation.</div>`;
  }
  const msgs = state.chatMessages
    .map(
      (m) => `<div class="chat-msg chat-${m.role}"><span class="chat-role">${m.role}</span>${escapeHtml(m.content)}</div>`,
    )
    .join("");
  const streaming = state.chatStream
    ? `<div class="chat-msg chat-assistant"><span class="chat-role">assistant</span>${escapeHtml(state.chatStream)}</div>`
    : "";

  return `
    <div class="panel-box">
      <div class="panel-title">Chat — ${escapeHtml(state.chatModel)}
        <button class="mini" id="chat-close">Close</button>
      </div>
      <div class="chat-log">${msgs}${streaming}</div>
      <div class="row">
        <input type="text" id="chat-input" placeholder="Say something…" value="${escapeHtml(state.chatInput)}" ${state.chatBusy ? "disabled" : ""} />
        <button id="chat-send" ${state.chatBusy ? "disabled" : ""}>${state.chatBusy ? "…" : "Send"}</button>
      </div>
    </div>
  `;
}

function render(container: HTMLElement): void {
  container.innerHTML = `
    ${renderPullPanel()}
    ${renderInstalled()}
    ${renderChat()}
  `;
  bind(container);
}

function bind(container: HTMLElement): void {
  const queryInput = container.querySelector<HTMLInputElement>("#pull-query");
  queryInput?.addEventListener("input", (e) => {
    state.query = (e.target as HTMLInputElement).value;
    if (fitTimer) clearTimeout(fitTimer);
    const q = state.query.trim();
    if (!q) {
      state.fitPreview = null;
      return;
    }
    state.fitBusy = true;
    fitTimer = setTimeout(async () => {
      try {
        state.fitPreview = await modelFitCheck(q, CTX);
      } catch {
        state.fitPreview = null;
      } finally {
        state.fitBusy = false;
        render(container);
      }
    }, 350);
  });

  container.querySelector("#pull-go")?.addEventListener("click", () => void doPull(container));

  container.querySelectorAll<HTMLButtonElement>("[data-rm]").forEach((btn) => {
    btn.addEventListener("click", () => void doRemove(container, btn.dataset.rm!));
  });
  container.querySelectorAll<HTMLButtonElement>("[data-stop]").forEach((btn) => {
    btn.addEventListener("click", () => void doStop(container, btn.dataset.stop!));
  });
  container.querySelectorAll<HTMLButtonElement>("[data-chat]").forEach((btn) => {
    btn.addEventListener("click", () => {
      state.chatModel = btn.dataset.chat!;
      state.chatMessages = [];
      state.chatStream = "";
      render(container);
    });
  });

  container.querySelector("#chat-close")?.addEventListener("click", () => {
    state.chatModel = null;
    state.chatMessages = [];
    render(container);
  });

  const chatInput = container.querySelector<HTMLInputElement>("#chat-input");
  chatInput?.addEventListener("input", (e) => {
    state.chatInput = (e.target as HTMLInputElement).value;
  });
  chatInput?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") void sendChat(container);
  });
  container.querySelector("#chat-send")?.addEventListener("click", () => void sendChat(container));
}

async function refresh(container: HTMLElement): Promise<void> {
  state.loading = true;
  state.error = null;
  render(container);
  try {
    const [installed, loaded] = await Promise.all([modelsInstalled(CTX), modelsLoaded().catch(() => [])]);
    state.installed = installed;
    state.loaded = loaded;
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Failed to load models";
  } finally {
    state.loading = false;
    render(container);
  }
}

async function doPull(container: HTMLElement): Promise<void> {
  const model = state.query.trim();
  if (!model || state.pulling) return;
  state.pulling = model;
  state.pullProgress = null;
  state.pullError = null;
  render(container);

  try {
    await modelPull(model);
    state.query = "";
    state.fitPreview = null;
    await refresh(container);
  } catch (err) {
    state.pullError = err instanceof Error ? err.message : "Pull failed";
  } finally {
    state.pulling = null;
    state.pullProgress = null;
    render(container);
  }
}

async function doRemove(container: HTMLElement, model: string): Promise<void> {
  try {
    await modelRm(model);
    await refresh(container);
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Remove failed";
    render(container);
  }
}

async function doStop(container: HTMLElement, model: string): Promise<void> {
  try {
    await modelStop(model);
    await refresh(container);
  } catch (err) {
    state.error = err instanceof Error ? err.message : "Unload failed";
    render(container);
  }
}

async function sendChat(container: HTMLElement): Promise<void> {
  const model = state.chatModel;
  const text = state.chatInput.trim();
  if (!model || !text || state.chatBusy) return;

  state.chatMessages.push({ role: "user", content: text });
  state.chatInput = "";
  state.chatBusy = true;
  state.chatStream = "";
  render(container);

  try {
    const reply = await modelChat(model, state.chatMessages);
    state.chatMessages.push({ role: "assistant", content: reply.content });
    state.chatStream = "";
  } catch (err) {
    state.chatMessages.push({
      role: "assistant",
      content: `(error: ${err instanceof Error ? err.message : "chat failed"})`,
    });
  } finally {
    state.chatBusy = false;
    render(container);
  }
}

export async function mount(container: HTMLElement): Promise<void> {
  currentContainer = container;
  unlistenPull = await onPullProgress((p) => {
    state.pullProgress = p;
    if (currentContainer) render(currentContainer);
  });
  unlistenChat = await onChatToken((tok) => {
    state.chatStream += tok;
    if (currentContainer) render(currentContainer);
  });
  await refresh(container);
}

export function unmount(): void {
  unlistenPull?.();
  unlistenChat?.();
  unlistenPull = null;
  unlistenChat = null;
  currentContainer = null;
  if (fitTimer) clearTimeout(fitTimer);
}
