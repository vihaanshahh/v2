import type {
  ChatMessage,
  ChatReply,
  ChatRoute,
  ChatTarget,
  DoctorReport,
  EndpointInfo,
  FederatedOrg,
  FitCheck,
  InstalledModel,
  MeshStatus,
  PeerCard,
  PullProgress,
  RunningModel,
  ScanResult,
  ServeStatus,
  Source,
  UsageSummary,
} from "./types";

export const isTauri = () => "__TAURI_INTERNALS__" in window;

const DESKTOP_ONLY = "This feature is only available in the v2 desktop app.";

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) {
    throw new Error(DESKTOP_ONLY);
  }
  const { invoke: tauriInvoke } = await import("@tauri-apps/api/core");
  return tauriInvoke<T>(cmd, args);
}

/// Subscribes to a Tauri event; no-ops (returns a harmless unlisten) outside
/// the desktop app so views can call this unconditionally.
export async function on<T>(event: string, cb: (payload: T) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<T>(event, (e) => cb(e.payload));
}

export async function scan(ctx: number, source: Source, family: string): Promise<ScanResult> {
  if (isTauri()) {
    return invoke<ScanResult>("scan", { ctx, source, family: family.trim() || null });
  }

  const params = new URLSearchParams({ ctx: String(ctx), source });
  if (family.trim()) params.set("family", family.trim());

  const res = await fetch(`/api/scan?${params}`);
  const body = await res.json();
  if (!res.ok) {
    throw new Error(body.error ?? `Scan failed (${res.status})`);
  }
  return body as ScanResult;
}

// ── Model management ─────────────────────────────────────────────────────────

export const modelsInstalled = (ctx: number, host?: string) =>
  invoke<InstalledModel[]>("models_installed", { host: host ?? null, ctx });

export const modelsLoaded = (host?: string) =>
  invoke<RunningModel[]>("models_loaded", { host: host ?? null });

export const modelFitCheck = (query: string, ctx: number) =>
  invoke<FitCheck>("model_fit_check", { query, ctx });

export const modelPull = (model: string, host?: string) =>
  invoke<void>("model_pull", { model, host: host ?? null });

export const modelRm = (model: string, host?: string) =>
  invoke<void>("model_rm", { model, host: host ?? null });

export const modelStop = (model: string, host?: string) =>
  invoke<void>("model_stop", { model, host: host ?? null });

export const modelChat = (model: string, messages: ChatMessage[], host?: string) =>
  invoke<ChatReply>("model_chat", { model, messages, host: host ?? null });

export const chatTargets = (ctx: number, host?: string) =>
  invoke<ChatTarget[]>("chat_targets", { ctx, host: host ?? null });

export const chatSend = (route: ChatRoute, messages: ChatMessage[], ctx: number) =>
  invoke<ChatReply>("chat_send", { route, messages, ctx });

export const onPullProgress = (cb: (p: PullProgress) => void) => on<PullProgress>("pull-progress", cb);

export const onChatToken = (cb: (tok: string) => void) => on<string>("chat-token", cb);

// ── Serve / usage / doctor ───────────────────────────────────────────────────

export const serveStart = (listen?: string, host?: string, cpu?: string) =>
  invoke<void>("serve_start", { listen: listen ?? null, host: host ?? null, cpu: cpu ?? null });

export const serveStop = () => invoke<void>("serve_stop");

export const serveStatus = () => invoke<ServeStatus>("serve_status");

export const usageSummary = () => invoke<UsageSummary>("usage_summary");

export const doctor = (host?: string) => invoke<DoctorReport>("doctor", { host: host ?? null });

export const endpointBanner = (listen?: string, host?: string) =>
  invoke<EndpointInfo>("endpoint_banner", { listen: listen ?? null, host: host ?? null });

// ── Mesh ──────────────────────────────────────────────────────────────────────

export const meshStatus = () => invoke<MeshStatus>("mesh_status");

export const meshPeers = () => invoke<PeerCard[]>("mesh_peers");

export const meshId = () => invoke<string>("mesh_id");

export const meshInit = () => invoke<void>("mesh_init");

export const meshInvite = (addr?: string, viaRelay?: string, ttlSecs?: number) =>
  invoke<string>("mesh_invite", { addr: addr ?? null, viaRelay: viaRelay ?? null, ttlSecs: ttlSecs ?? null });

export const meshJoin = (ticket: string) => invoke<void>("mesh_join", { ticket });

export const meshPeerAdd = (addr: string) => invoke<void>("mesh_peer_add", { addr });

export const meshRevoke = (node: string) => invoke<void>("mesh_revoke", { node });

export const meshPause = () => invoke<void>("mesh_pause");

export const meshResume = () => invoke<void>("mesh_resume");

export const meshFederationList = () => invoke<FederatedOrg[]>("mesh_federation_list");

export const meshFederationAdd = (org: string, note: string, models: string[]) =>
  invoke<void>("mesh_federation_add", { org, note: note || null, models });
