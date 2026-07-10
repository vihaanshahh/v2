//! Local metering proxy (Phase 1). Sits on :11435 in front of Ollama on :11434,
//! forwards every request unchanged, and meters exact token counts from Ollama's
//! own stream stats. Ollama stays bound to localhost; apps point at v2 instead.
//!
//! Deadman by design (DESIGN.md §4): the response is streamed, not buffered. If
//! the client disconnects or the daemon dies, the write fails, the upstream
//! reader drops, and Ollama aborts generation. The metering record is still
//! written from the reader's Drop, so partial usage is never lost.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::activity::Activity;
use crate::endpoints;
use crate::ollama_api::GenStats;
use crate::usage::{self, UsageRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthMode {
    Open,
    Key(String),
}

/// The `[endpoint]` section of `~/.v2/policy.toml` (defaults if absent).
fn endpoint_cfg() -> Result<crate::policy::EndpointPolicy, String> {
    crate::policy::Policy::load().map(|p| p.endpoint)
}

/// Bearer token guarding the OpenAI-compatible `/v1/*` surface. Resolution order
/// (env overrides config so a managed platform can inject values):
///   1. `V2_OPEN=1` env or `endpoint.open = true` -> no gate, but only on
///      loopback with no public URL advertised.
///   2. `V2_API_KEY` env, else `endpoint.api_key` config → use it verbatim.
///   3. otherwise → load-or-create a persisted key at `~/.v2/api_key`.
/// So `/v1` is **key-gated by default** and safe to expose with no setup — you
/// never have to invent or wire a key yourself.
fn auth_mode(listen: &str) -> Result<AuthMode, String> {
    let cfg = endpoint_cfg()?;
    if open_requested(&cfg) {
        if !is_loopback(listen) || public_url_raw(&cfg).is_some() {
            return Err("endpoint.open/V2_OPEN is only allowed on loopback with no public URL".into());
        }
        return Ok(AuthMode::Open);
    }
    if let Ok(k) = std::env::var("V2_API_KEY") {
        if !k.trim().is_empty() {
            return Ok(AuthMode::Key(k));
        }
    }
    if !cfg.api_key.trim().is_empty() {
        return Ok(AuthMode::Key(cfg.api_key));
    }
    load_or_create_key().map(AuthMode::Key)
}

fn open_requested(cfg: &crate::policy::EndpointPolicy) -> bool {
    std::env::var("V2_OPEN")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
        || cfg.open
}

fn public_url_raw(cfg: &crate::policy::EndpointPolicy) -> Option<String> {
    std::env::var("V2_PUBLIC_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| (!cfg.public_url.trim().is_empty()).then_some(cfg.public_url.clone()))
}

/// Read the persisted key, generating (and 0600-storing) one on first use.
fn load_or_create_key() -> Result<String, String> {
    let path = crate::paths::file("api_key").map_err(|e| e.to_string())?;
    match std::fs::read_to_string(&path) {
        Ok(k) => {
            let k = k.trim().to_string();
            if !k.is_empty() {
                return Ok(k);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    }
    let key = generate_api_key()?;
    write_api_key(&path, &key).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(key)
}

fn generate_api_key() -> Result<String, String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| format!("rng failure: {e}"))?;
    Ok(format!("sk-v2-{}", raw.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

fn write_api_key(path: &std::path::Path, key: &str) -> std::io::Result<()> {
    crate::paths::write_private(path, key.as_bytes())
}

fn auth_description(auth: &AuthMode, reveal_key: bool) -> String {
    match auth {
        AuthMode::Open => "open (loopback only)".into(),
        AuthMode::Key(key) if reveal_key => key.clone(),
        AuthMode::Key(key) => format!("{} (hidden; run `v2 endpoint` to show)", mask_key(key)),
    }
}

fn mask_key(key: &str) -> String {
    if key.len() <= 12 {
        "set".into()
    } else {
        format!("{}...{}", &key[..8], &key[key.len() - 4..])
    }
}

fn protect_all_paths(listen: &str) -> Result<bool, String> {
    let cfg = endpoint_cfg()?;
    Ok(!is_loopback(listen) || public_url_raw(&cfg).is_some())
}

fn require_bearer(request: &tiny_http::Request, auth: &AuthMode) -> bool {
    match auth {
        AuthMode::Open => true,
        AuthMode::Key(key) => bearer_ok(header_value(request, "authorization").as_deref(), key),
    }
}

/// Print a paste-ready description of the OpenAI-compatible endpoint: the Base
/// URL(s), the API key, and installed model ids. `V2_PUBLIC_URL` (if set) is
/// Pure data behind the endpoint banner — shared by `v2 endpoint`/`v2 serve`'s
/// printed panel and the desktop app's endpoint info command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EndpointInfo {
    pub base_url: String,
    pub local_url: Option<String>,
    pub api_key: String,
    pub models: Vec<String>,
}

pub fn endpoint_info(listen: &str, ollama_host: &str, reveal_key: bool) -> Result<EndpointInfo, String> {
    // Public URL: env wins, then the `[endpoint] public_url` config.
    let cfg = endpoint_cfg()?;
    let public = match public_url_raw(&cfg) {
        Some(raw) => Some(normalize_public_base_url(&raw)?),
        None => None,
    };
    let auth = auth_mode(listen)?;
    let mut models: Vec<String> = crate::ollama::fetch_local(ollama_host)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.ollama_name)
        .collect();
    for ep in endpoints::load() {
        models.extend(endpoints::aliases(&ep));
    }

    let (base_url, local_url) = match &public {
        Some(p) => (format!("{p}/v1"), Some(format!("{}/v1", local_base_url(listen)))),
        None => (format!("{}/v1", local_base_url(listen)), None),
    };

    Ok(EndpointInfo {
        base_url,
        local_url,
        api_key: auth_description(&auth, reveal_key),
        models,
    })
}

/// Rendered from `endpoint_info` — the primary Base URL is the public one when
/// a reverse-proxied/tunnelled deployment advertises one. Callable standalone
/// via `v2 endpoint`.
pub fn print_endpoint_banner(listen: &str, ollama_host: &str, reveal_key: bool) -> Result<(), String> {
    let info = endpoint_info(listen, ollama_host, reveal_key)?;

    let mut rows: Vec<(String, String)> = vec![("Base URL".into(), info.base_url)];
    if let Some(local) = info.local_url {
        rows.push(("(local)".into(), local));
    }
    rows.push(("API key".into(), info.api_key));
    rows.push((
        "Models".into(),
        if info.models.is_empty() { "(none installed — run `v2 pull <model>`)".into() } else { info.models.join(", ") },
    ));
    crate::ui::panel("OpenAI-compatible endpoint", &rows);
    println!("  Point any OpenAI tool here: Base URL + API key + a Model ID.");
    Ok(())
}

fn normalize_public_base_url(raw: &str) -> Result<String, String> {
    crate::endpoints::normalize_base_url(raw, crate::endpoints::ApiKind::Openai)
}

fn local_base_url(listen: &str) -> String {
    let (raw_host, port) = listen_host_port(listen);
    let host = match raw_host.trim().trim_start_matches('[').trim_end_matches(']') {
        "" | "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        h => h.to_string(),
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    format!("http://{host}:{port}")
}

fn listen_host_port(listen: &str) -> (&str, &str) {
    if let Some(rest) = listen.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').unwrap_or("11435");
            return (host, port);
        }
    }
    listen.rsplit_once(':').unwrap_or((listen, "11435"))
}

/// A live, runtime-adjustable cap on the CPU threads Ollama may use per request.
/// `0` means no cap. Shared with the interactive panel so you can dial it while
/// serving. Applied by injecting `options.num_thread` into requests that don't
/// already set it — the one lever a request-level wrapper has over Ollama.
pub type CpuLimit = Arc<AtomicUsize>;

/// Hard ceiling on a single request body we'll buffer in memory. Generous enough
/// for base64-encoded vision images, but bounded so a runaway or malicious client
/// can't OOM the daemon by streaming an endless (or decompression-bombed) body.
/// Anything larger is rejected with 413 before we read it all.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// Ceiling on an upstream error body we buffer to forward verbatim. Error
/// payloads are tiny JSON; cap them so a broken upstream can't balloon memory.
const MAX_ERROR_BODY: u64 = 1024 * 1024;

/// Run the metering proxy until the process is stopped. Blocks.
pub fn serve(listen: &str, ollama_host: &str, activity: Activity, cpu_limit: CpuLimit) -> Result<(), String> {
    // Never flipped, so this behaves exactly like the old unconditional loop —
    // the only way to stop is killing the process (deadman by design).
    serve_with_shutdown(listen, ollama_host, activity, cpu_limit, Arc::new(AtomicBool::new(true)))
}

/// Same as `serve`, but polls `running` between requests so a caller — e.g. the
/// desktop app's "stop serving" button — can end the loop in-process without
/// killing the whole app. Doesn't touch the mesh deadman path (DESIGN.md §4):
/// in-flight mesh connections still only die on disconnect/timeout, unaffected.
pub fn serve_with_shutdown(
    listen: &str,
    ollama_host: &str,
    activity: Activity,
    cpu_limit: CpuLimit,
    running: Arc<AtomicBool>,
) -> Result<(), String> {
    let auth = auth_mode(listen)?;
    let protect_all = protect_all_paths(listen)?;
    let server = tiny_http::Server::http(listen)
        .map_err(|e| format!("cannot bind {listen}: {e}"))?;
    let ollama_host = Arc::new(ollama_host.trim_end_matches('/').to_string());

    println!("v2 proxy  {listen} -> {ollama_host}  (metering local usage)");
    print_endpoint_banner(listen, &ollama_host, false)?;

    while running.load(Ordering::Relaxed) {
        match server.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(Some(request)) => {
                let host = ollama_host.clone();
                let act = activity.clone();
                let auth = auth.clone();
                // Snapshot the cap per request so panel changes take effect immediately.
                let threads = cpu_limit.load(Ordering::Relaxed);
                // Thread per request: concurrent local apps, blocking I/O, no async.
                std::thread::spawn(move || {
                    if let Err(e) = handle(request, &host, &act, threads, &auth, protect_all) {
                        eprintln!("v2 proxy: {e}");
                    }
                });
            }
            Ok(None) => continue, // timed out — loop back around to re-check `running`
            Err(e) => return Err(format!("proxy accept error: {e}")),
        }
    }
    Ok(())
}

fn handle(
    mut request: tiny_http::Request,
    ollama_host: &str,
    activity: &Activity,
    cpu_threads: usize,
    auth: &AuthMode,
    protect_all: bool,
) -> Result<(), String> {
    activity.touch();

    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let path = request_path(&url).to_string();
    let model = String::new(); // filled in below if we can see it in the body

    // ── OpenAI-compatible surface (`/v1/*`) ──────────────────────────────────
    // Lets any OpenAI SDK / tool point its Base URL at v2. Local models flow
    // straight to Ollama's native `/v1`; a model id registered as a remote
    // endpoint is reverse-proxied there with its stored key. Guarded by an
    // bearer token so an exposed bind isn't an open relay to your keys.
    let is_openai = path == "/v1" || path.starts_with("/v1/");
    if is_openai || protect_all {
        if !require_bearer(&request, auth) {
            let response = tiny_http::Response::from_string("{\"error\":{\"message\":\"invalid or missing api key\",\"type\":\"invalid_request_error\"}}")
                .with_status_code(401);
            let _ = request.respond(response);
            return Ok(());
        }
        if method == "GET" && path == "/v1/models" {
            return respond_openai_models(request, ollama_host);
        }
    }

    // Read the incoming body (the prompt). In memory only — never written to disk.
    // Bounded by MAX_REQUEST_BODY: we read one byte past the limit so we can tell
    // "exactly at the cap" from "over it", then refuse anything over with 413
    // instead of buffering an unbounded (or bomb-sized) payload.
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_REQUEST_BODY as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("read body: {e}"))?;
    if body.len() > MAX_REQUEST_BODY {
        let response = tiny_http::Response::from_string("request body too large")
            .with_status_code(413);
        let _ = request.respond(response);
        return Ok(());
    }

    let model = detect_model(&body).unwrap_or(model);

    // Choose the upstream. Default is local Ollama. On the `/v1` surface, a model
    // id matching a registered OpenAI endpoint (by model id or friendly name) is
    // reverse-proxied to that host with its stored key and canonical model id.
    let mut upstream_base = ollama_host.to_string();
    let mut upstream_auth: Option<String> = None;
    let mut body = body;
    if is_openai {
        if let Some(ep) = endpoints::find_model(&model)
            .filter(|e| e.kind == endpoints::ApiKind::Openai)
        {
            if method == "POST" && path == "/v1/chat/completions" {
                // Base may or may not already end in `/v1`; the request path carries
                // its own `/v1`, so strip a trailing one to avoid `/v1/v1`.
                upstream_base = endpoints::normalize_base_url(&ep.url, ep.kind)?;
                upstream_auth = ep.api_key.clone();
                body = rewrite_model(body, &ep.model);
            } else {
                let response = tiny_http::Response::from_string("{\"error\":{\"message\":\"remote endpoints only support /v1/chat/completions\",\"type\":\"invalid_request_error\"}}")
                    .with_status_code(404);
                let _ = request.respond(response);
                return Ok(());
            }
        }
    }
    let routed_remote = upstream_base != ollama_host;

    // Cap CPU only for local Ollama jobs (a remote endpoint has no num_thread).
    let body = if cpu_threads > 0 && !routed_remote { cap_cpu_threads(body, cpu_threads) } else { body };

    let upstream_url = format!("{upstream_base}{url}");
    let mut req = ureq::request(&method, &upstream_url);
    // Forward content-type so the upstream parses JSON bodies.
    if let Some(ct) = header_value(&request, "content-type") {
        req = req.set("Content-Type", &ct);
    }
    // Inject the endpoint's own key (never the caller's) when routed remotely.
    if let Some(key) = upstream_auth.as_deref().filter(|k| !k.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }

    let resp = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(&body)
    };

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            // Forward upstream error responses verbatim, but bounded — an error
            // body should be tiny JSON, never megabytes.
            let bytes = crate::ollama_api::drain(r.into_reader().take(MAX_ERROR_BODY));
            let response = tiny_http::Response::from_data(bytes).with_status_code(code);
            let _ = request.respond(response);
            return Ok(());
        }
        Err(e) => return Err(format!("upstream {upstream_url}: {e}")),
    };

    let status = resp.status();
    let content_type = resp.header("Content-Type").unwrap_or("application/json").to_string();

    // Wrap the upstream reader so we meter tokens as they stream through.
    let meter = MeteringReader::new(resp.into_reader(), model, "local", "local");

    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .map_err(|_| "bad content-type header".to_string())?;
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(status),
        vec![header],
        meter,
        None, // unknown length -> chunked streaming, reads until EOF
        None,
    );
    request.respond(response).map_err(|e| format!("respond: {e}"))
}

fn header_value(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn request_path(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url).trim_end_matches('/');
    if path.is_empty() { "/" } else { path }
}

fn bearer_ok(header: Option<&str>, key: &str) -> bool {
    let Some(header) = header else { return false };
    let Some(token) = header.trim().strip_prefix("Bearer ") else { return false };
    constant_time_eq(token.as_bytes(), key.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Best-effort model name from a request body ({"model": "..."}).
fn detect_model(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(|s| s.to_string())
}

/// Replace the `model` field so a request addressed by an endpoint's friendly
/// name (or id) reaches the upstream with the id it actually expects.
fn rewrite_model(body: Vec<u8>, model: &str) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else { return body };
    let Some(obj) = v.as_object_mut() else { return body };
    obj.insert("model".into(), serde_json::json!(model));
    serde_json::to_vec(&v).unwrap_or(body)
}

/// `GET /v1/models`: OpenAI-shaped catalog merging local Ollama tags with every
/// registered remote endpoint, so a client's model picker sees one unified list.
fn respond_openai_models(request: tiny_http::Request, ollama_host: &str) -> Result<(), String> {
    let mut data: Vec<serde_json::Value> = Vec::new();
    let mut push = |id: &str, owner: &str| {
        data.push(serde_json::json!({ "id": id, "object": "model", "owned_by": owner }));
    };
    for tag in crate::ollama::fetch_local(ollama_host)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.ollama_name)
    {
        push(&tag, "ollama");
    }
    for ep in endpoints::load() {
        let owner = format!("endpoint:{}", ep.name);
        for id in endpoints::aliases(&ep) {
            push(&id, &owner);
        }
    }
    let payload = serde_json::json!({ "object": "list", "data": data }).to_string();
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .map_err(|_| "bad content-type header".to_string())?;
    let response = tiny_http::Response::from_string(payload).with_header(header);
    request.respond(response).map_err(|e| format!("respond: {e}"))
}

/// Inject `options.num_thread = threads` into a generate/chat body so Ollama
/// doesn't peg every core. Only touches JSON objects carrying a "model" and
/// never overrides a num_thread the caller set. Any non-JSON or unexpected body
/// is forwarded byte-for-byte, so the proxy stays transparent.
fn cap_cpu_threads(body: Vec<u8>, threads: usize) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    if v.get("model").is_none() {
        return body;
    }
    let Some(obj) = v.as_object_mut() else { return body };
    let options = obj.entry("options").or_insert_with(|| serde_json::json!({}));
    match options.as_object_mut() {
        Some(opts) if !opts.contains_key("num_thread") => {
            opts.insert("num_thread".into(), serde_json::json!(threads));
        }
        _ => return body, // already set, or options isn't an object — leave it
    }
    serde_json::to_vec(&v).unwrap_or(body)
}

/// Whether a `host:port` listen address binds to loopback only — i.e. the proxy
/// is reachable from this machine but not from the network. Anything else
/// (`0.0.0.0`, a LAN IP, `::`) is exposed and gets a warning.
pub fn is_loopback(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
}

/// Logical CPU count on this machine (fallback 1), used to turn a percentage
/// CPU budget into a concrete thread cap.
pub fn cpu_cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Parse a `--cpu` spec into a thread cap. Accepts a percentage ("50%") or an
/// absolute thread count ("4"). Empty or "0" means unlimited. Result is clamped
/// to `[1, cores]` for any positive request.
pub fn parse_cpu_spec(spec: &str, cores: usize) -> Result<usize, String> {
    let s = spec.trim();
    if s.is_empty() || s == "0" {
        return Ok(0);
    }
    if let Some(pct) = s.strip_suffix('%') {
        let pct: f64 = pct.trim().parse().map_err(|_| format!("bad cpu percent: {spec}"))?;
        if pct <= 0.0 {
            return Ok(0);
        }
        let n = ((pct / 100.0) * cores as f64).round() as usize;
        return Ok(n.clamp(1, cores.max(1)));
    }
    let n: usize = s
        .parse()
        .map_err(|_| format!("cpu limit must be a thread count or percent (e.g. 4 or 50%), got: {spec}"))?;
    Ok(n.min(cores.max(1)))
}

/// A Read that passes bytes through unchanged while extracting Ollama's
/// end-of-stream token stats from the JSONL body. Writes a usage record on Drop
/// so metering survives client disconnects (deadman).
pub struct MeteringReader<R: Read> {
    inner: R,
    line: Vec<u8>,
    last_stats: Option<GenStats>,
    model: String,
    source: String,
    kind: String,
    start: Instant,
    logged: bool,
    /// When false, metering is parsed but not persisted (used by tests so they
    /// never write to the real ~/.v2/usage).
    persist: bool,
}

impl<R: Read> MeteringReader<R> {
    pub fn new(inner: R, model: String, source: &str, kind: &str) -> Self {
        Self {
            inner,
            line: Vec::with_capacity(256),
            last_stats: None,
            model,
            source: source.to_string(),
            kind: kind.to_string(),
            start: Instant::now(),
            logged: false,
            persist: true,
        }
    }

    fn scan(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                self.try_parse_line();
                self.line.clear();
            } else if self.line.len() < 16 * 1024 {
                self.line.push(b);
            }
        }
    }

    fn try_parse_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        if let Ok(stats) = serde_json::from_slice::<GenStats>(&self.line) {
            if stats.eval_count > 0 || stats.prompt_eval_count > 0 || stats.done {
                self.last_stats = Some(stats);
            }
        }
    }

    fn log(&mut self) {
        if self.logged {
            return;
        }
        self.logged = true;
        // Parse any trailing line without a newline.
        self.try_parse_line();
        if !self.persist {
            return;
        }
        let Some(stats) = self.last_stats.clone() else { return };
        if stats.eval_count == 0 && stats.prompt_eval_count == 0 {
            return;
        }
        usage::append(&UsageRecord {
            ts: usage::now_unix(),
            source: self.source.clone(),
            kind: self.kind.clone(),
            model: if self.model.is_empty() { "unknown".into() } else { self.model.clone() },
            tokens_in: stats.prompt_eval_count,
            tokens_out: stats.eval_count,
            duration_ms: self.start.elapsed().as_millis() as u64,
        });
    }
}

impl<R: Read> Read for MeteringReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            self.log();
        } else {
            let chunk = buf[..n].to_vec();
            self.scan(&chunk);
        }
        Ok(n)
    }
}

impl<R: Read> Drop for MeteringReader<R> {
    fn drop(&mut self) {
        self.log();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // A realistic Ollama /api/chat stream: content deltas, then a final stats line.
    const STREAM: &[u8] = br#"{"message":{"content":"Hi"},"done":false}
{"message":{"content":" there"},"done":false}
{"message":{"content":""},"done":true,"prompt_eval_count":11,"eval_count":42,"total_duration":900000000}
"#;

    #[test]
    fn rewrite_model_swaps_only_the_model_field() {
        let body = br#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hi"}],"stream":true}"#.to_vec();
        let out = rewrite_model(body, "gpt-5.5-turbo");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.5-turbo");
        assert_eq!(v["messages"][0]["content"], "hi"); // rest untouched
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn rewrite_model_leaves_non_json_untouched() {
        let body = b"not json".to_vec();
        assert_eq!(rewrite_model(body.clone(), "x"), body);
    }

    #[test]
    fn meters_tokens_from_stream_and_passes_bytes_through() {
        let mut r = MeteringReader::new(STREAM, "qwen3:8b".into(), "local", "local");
        r.persist = false; // never touch ~/.v2 from tests
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        // Bytes passed through unchanged (transparent proxy).
        assert_eq!(out, STREAM);
        // Stats extracted from the final line.
        let stats = r.last_stats.clone().expect("stats parsed");
        assert_eq!(stats.prompt_eval_count, 11);
        assert_eq!(stats.eval_count, 42);
        assert!(stats.done);
    }

    #[test]
    fn cpu_cap_injects_num_thread_without_clobbering_callers() {
        // No options at all -> options.num_thread added.
        let out = cap_cpu_threads(br#"{"model":"m","prompt":"hi"}"#.to_vec(), 4);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["options"]["num_thread"], 4);
        // Caller already set num_thread -> untouched.
        let body = br#"{"model":"m","options":{"num_thread":16}}"#.to_vec();
        let out = cap_cpu_threads(body.clone(), 4);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["options"]["num_thread"], 16);
        // Non-model body (e.g. /api/tags) and non-JSON pass through verbatim.
        assert_eq!(cap_cpu_threads(b"not json".to_vec(), 4), b"not json");
        assert_eq!(cap_cpu_threads(br#"{"name":"x"}"#.to_vec(), 4), br#"{"name":"x"}"#);
    }

    #[test]
    fn loopback_detection_locks_down_local_binds() {
        assert!(is_loopback("127.0.0.1:11435"));
        assert!(is_loopback("localhost:11435"));
        assert!(is_loopback("[::1]:11435"));
        assert!(!is_loopback("0.0.0.0:11435"));
        assert!(!is_loopback("192.168.1.20:11435"));
    }

    #[test]
    fn base_url_display_normalizes_public_and_local_binds() {
        assert_eq!(
            normalize_public_base_url("https://example.com/v1").unwrap(),
            "https://example.com"
        );
        assert_eq!(local_base_url("0.0.0.0:11435"), "http://127.0.0.1:11435");
        assert_eq!(local_base_url("[::]:11435"), "http://127.0.0.1:11435");
        assert_eq!(local_base_url("192.168.1.20:11435"), "http://192.168.1.20:11435");
    }

    #[test]
    fn bearer_check_requires_exact_constant_time_token() {
        assert!(bearer_ok(Some("Bearer sk-v2-abc"), "sk-v2-abc"));
        assert!(!bearer_ok(Some("Bearer sk-v2-abd"), "sk-v2-abc"));
        assert!(!bearer_ok(Some("sk-v2-abc"), "sk-v2-abc"));
        assert!(!bearer_ok(None, "sk-v2-abc"));
    }

    #[test]
    fn cpu_spec_parses_percent_and_count() {
        assert_eq!(parse_cpu_spec("", 8).unwrap(), 0);
        assert_eq!(parse_cpu_spec("0", 8).unwrap(), 0);
        assert_eq!(parse_cpu_spec("50%", 8).unwrap(), 4);
        assert_eq!(parse_cpu_spec("4", 8).unwrap(), 4);
        assert_eq!(parse_cpu_spec("100", 8).unwrap(), 8); // clamped to cores
        assert!(parse_cpu_spec("banana", 8).is_err());
    }

    #[test]
    fn split_reads_still_capture_stats() {
        // Feed one byte at a time to prove line reassembly across read() calls.
        struct Drip<'a>(&'a [u8], usize);
        impl Read for Drip<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.1 >= self.0.len() { return Ok(0); }
                buf[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let mut r = MeteringReader::new(Drip(STREAM, 0), "m".into(), "local", "local");
        r.persist = false;
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(r.last_stats.as_ref().unwrap().eval_count, 42);
    }

    /// The desktop app's "stop serving" button relies on `serve_with_shutdown`
    /// actually exiting once `running` flips — not just on process kill (the old
    /// deadman-only story). Binds an OS-assigned port so this can't collide with
    /// a real `v2 serve` on the test machine.
    #[test]
    fn serve_with_shutdown_stops_once_flag_flips() {
        let _g = crate::test_support::lock();
        crate::test_support::set_temp_home("proxy");

        let running = Arc::new(AtomicBool::new(true));
        let running2 = running.clone();
        let cpu_limit: CpuLimit = Arc::new(AtomicUsize::new(0));
        let activity = Activity::new();

        let handle = std::thread::spawn(move || {
            serve_with_shutdown("127.0.0.1:0", "http://127.0.0.1:11434", activity, cpu_limit, running2)
        });

        // Let it bind and enter the poll loop, then ask it to stop.
        std::thread::sleep(std::time::Duration::from_millis(200));
        running.store(false, Ordering::Relaxed);

        // Bound the wait: if the shutdown flag were ignored, `join()` would hang
        // forever and this test would time out the whole suite instead of failing
        // cleanly, so hand the join off to a watchdog thread with a real deadline.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(handle.join());
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("serve_with_shutdown did not stop within 3s of the flag flipping")
            .expect("proxy thread panicked");
        assert!(result.is_ok(), "serve_with_shutdown returned an error: {result:?}");
    }
}
