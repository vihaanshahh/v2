import "./style.css";
import * as scanView from "./views/scan";
import * as modelsView from "./views/models";
import * as serveView from "./views/serve";
import * as meshView from "./views/mesh";
import { isTauri } from "./platform";

type Tab = "scan" | "models" | "serve" | "mesh";

interface ViewModule {
  mount(container: HTMLElement): void | Promise<void>;
  unmount?(): void;
}

const views: Record<Tab, ViewModule> = {
  scan: scanView,
  models: modelsView,
  serve: serveView,
  mesh: meshView,
};

const TABS: { id: Tab; label: string; desktopOnly: boolean }[] = [
  { id: "scan", label: "Scan", desktopOnly: false },
  { id: "models", label: "Models", desktopOnly: true },
  { id: "serve", label: "Serve", desktopOnly: true },
  { id: "mesh", label: "Mesh", desktopOnly: true },
];

const app = document.getElementById("app")!;
let activeTab: Tab = "scan";
let activeView: ViewModule | null = null;

function renderShell(): void {
  const desktop = isTauri();
  app.innerHTML = `
    <div class="shell">
      <header>
        <div class="brand">
          <h1>v2</h1>
          <p>Which LLMs can run on this machine — scan, manage, serve, and share compute.</p>
        </div>
        <nav class="tabs">
          ${TABS.map(
            (t) => `
              <button class="tab ${activeTab === t.id ? "active" : ""}" data-tab="${t.id}" ${t.desktopOnly && !desktop ? "disabled title=\"desktop app only\"" : ""}>
                ${t.label}
              </button>
            `,
          ).join("")}
        </nav>
      </header>
      <div id="view"></div>
      ${!desktop ? `<div class="desktop-hint">Running in a browser — only Scan works here. Model management, serving, and mesh need the packaged desktop app.</div>` : ""}
    </div>
  `;

  document.querySelectorAll<HTMLButtonElement>("[data-tab]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const tab = btn.dataset.tab as Tab;
      if (tab !== activeTab) switchTab(tab);
    });
  });
}

function switchTab(tab: Tab): void {
  activeView?.unmount?.();
  activeTab = tab;
  renderShell();
  const container = document.getElementById("view")!;
  activeView = views[tab];
  void activeView.mount(container);
}

switchTab("scan");
