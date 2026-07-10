export type FitType =
  | "full_gpu"
  | "partial_offload"
  | "cpu_only"
  | "too_big";

export interface QuantResult {
  quant: string;
  vram_gb: string;
  fit: FitType;
  est_tps: number | null;
  notes: string[];
}

export interface ModelResult {
  name: string;
  display_name: string;
  family: string;
  id: string;
  ollama_name: string | null;
  origin: "catalog" | "ollama";
  is_moe: boolean;
  recommended_quant: string | null;
  can_run: boolean;
  quants: QuantResult[];
}

export interface GpuInfo {
  name: string;
  vendor: string;
  vram_gb: number;
  shared: boolean;
}

export interface ScanResult {
  hardware: {
    gpus: GpuInfo[];
    ram_gb: string;
    cpu: string;
    os: string;
  };
  models: ModelResult[];
}

export type Source = "auto" | "catalog" | "ollama" | "all";

export function bestQuant(model: ModelResult): QuantResult | null {
  if (!model.recommended_quant) return null;
  return model.quants.find((q) => q.quant === model.recommended_quant) ?? null;
}

export function fitLabel(fit: string): string {
  switch (fit) {
    case "full_gpu":
      return "GPU";
    case "partial_offload":
      return "partial";
    case "cpu_only":
      return "CPU";
    case "too_big":
      return "too big";
    default:
      return "n/a";
  }
}

export function fitClass(fit: string): string {
  switch (fit) {
    case "full_gpu":
      return "fit-gpu";
    case "partial_offload":
      return "fit-partial";
    case "cpu_only":
      return "fit-cpu";
    case "too_big":
      return "fit-nope";
    default:
      return "fit-nope";
  }
}

// ── Model management ─────────────────────────────────────────────────────────

export interface InstalledModel {
  name: string;
  display_name: string;
  size_gb: number | null;
  fit: string;
  offload_pct: number | null;
  quant: string | null;
  est_tps: number | null;
  tps_label: string | null;
}

export interface RunningModel {
  name: string;
  size: number;
  size_vram: number;
}

export interface FitCheck {
  display_name: string;
  in_catalog: boolean;
  fits: boolean;
  fit: string;
  quant: string | null;
  vram_gb: number | null;
  est_tps: number | null;
  notes: string[];
}

export interface PullProgress {
  model: string;
  status: string;
  completed: number;
  total: number;
}

export interface ChatMessage {
  role: "user" | "assistant";
  content: string;
}

export interface ChatReply {
  content: string;
  tokens: number;
  tps: number;
}

// ── Serve / usage / doctor ───────────────────────────────────────────────────

export type DoctorStatus = "ok" | "warn" | "bad";

export interface DoctorLine {
  status: DoctorStatus;
  label: string;
  message: string;
}

export interface DoctorReport {
  ollama: DoctorLine;
  identity: DoctorLine;
  mesh: DoctorLine;
  policy: DoctorLine;
  abuse: DoctorLine;
}

export interface AggRow {
  key: string;
  requests: number;
  tokens_in: number;
  tokens_out: number;
  tps: number;
}

export interface UsageSummary {
  total_requests: number;
  total_tokens_in: number;
  total_tokens_out: number;
  by_day: AggRow[];
  by_model: AggRow[];
  by_source: AggRow[];
}

export interface EndpointInfo {
  base_url: string;
  local_url: string | null;
  api_key: string;
  models: string[];
}

export interface ServeStatus {
  running: boolean;
  listen: string | null;
}

// ── Mesh ──────────────────────────────────────────────────────────────────────

export interface RemoteModelInfo {
  name: string;
  model: string;
  kind: string;
  host: string;
}

export interface NodeCard {
  node_pub: string;
  hostname: string;
  os: string;
  gpu: string;
  vram_gb: number;
  bandwidth_gbps: number;
  models: string[];
  remote_models: RemoteModelInfo[];
  concurrent: number;
  max_concurrent: number;
}

export interface PeerCard {
  addr: string;
  card: NodeCard;
}

export interface MeshStatus {
  node_id: string;
  is_member: boolean;
  org_id: string | null;
  role: string | null;
  cert_valid_hours: number | null;
  connected_peers: number;
  known_peer_addrs: string[];
}

export interface FederatedOrg {
  org_pub: string;
  note: string;
  allowed_models: string[];
}
