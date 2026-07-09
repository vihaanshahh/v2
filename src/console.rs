//! Interactive control panel for `v2 serve` (TTY only).
//!
//! `v2 serve` runs the metering proxy in a background thread and drops you into
//! this panel. It's built around the day-to-day lifecycle of a local model:
//!
//!   find → install → open → close → limit → delete
//!
//! One numbered list shows every model — installed, running, and the ones that
//! fit this machine but aren't installed yet. Type a number and the panel offers
//! exactly the next steps that make sense for that model. It's line-based (no raw
//! mode, no extra deps) and re-measures the terminal on every redraw, so it stays
//! clean when the window resizes. `--headless` skips it entirely.

use std::io::{self, Write};
use std::process::Command;
use std::sync::atomic::Ordering;

use colored::Colorize;

use crate::bandwidth;
use crate::endpoints::{self, Endpoint};
use crate::engine::{self, FitType};
use crate::hardware::HardwareInfo;
use crate::mesh::{self, client, identity};
use crate::models::{self, Model};
use crate::ollama;
use crate::ollama_api;
use crate::proxy::{self, CpuLimit};
use crate::ui;
use crate::usage;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Cap on how many not-yet-installed models to surface (keeps the list clean —
/// `p` finds anything else by name).
const MAX_AVAILABLE: usize = 8;

/// One line in the unified model list.
struct Row {
    tag: String,     // ollama pull/run tag, e.g. "qwen3:8b" (or endpoint name)
    installed: bool,
    running: bool,
    bytes: u64,      // on-disk / download size in bytes (0 if unknown)
    size: String,    // human size, e.g. "4.8G"
    fit: String,     // coloured fit word (or host, for a remote endpoint)
    speed: String,   // e.g. "~60 tok/s"
    /// Set when this row is a user-registered remote endpoint (e.g. Modal),
    /// not a local Ollama model.
    endpoint: Option<Endpoint>,
}

/// Disk headroom we insist on keeping free after a download, so an install never
/// fills the volume to 0 (which wedges the OS, Ollama, and our own metering log).
const DISK_HEADROOM: u64 = 2 * 1024 * 1024 * 1024;

/// Whether a `need`-byte download safely fits in `free` bytes of disk, keeping
/// `DISK_HEADROOM` in reserve. Unknown sizes (0) always pass — we don't block on
/// a number we don't have.
fn disk_ok(need: u64, free: u64) -> bool {
    need == 0 || free >= need.saturating_add(DISK_HEADROOM)
}

/// Drive the interactive panel until the user quits or stdin closes. The proxy
/// keeps serving on `listen` from its own thread the whole time. `cpu` is the
/// live thread cap shared with the proxy; `cores` is this machine's logical CPU
/// count (for turning a percentage into a thread count).
pub fn run(
    host: &str,
    hw: &HardwareInfo,
    ctx: u32,
    listen: &str,
    mesh_listen: Option<&str>,
    cpu: &CpuLimit,
    cores: usize,
) -> Result<(), String> {
    loop {
        let rows = render(host, hw, ctx, listen, cpu, cores);
        let Some(line) = prompt("\n> ") else { break }; // Ctrl-D
        let line = line.trim();
        let (cmd, arg) = split_first(line);
        match cmd.to_lowercase().as_str() {
            "" | "g" | "refresh" => {} // redraw
            "q" | "quit" | "exit" => break,
            "p" | "find" | "pull" => pull_flow(host, hw, ctx, arg),
            "a" | "add" => add_endpoint_flow(host),
            "s" | "share" => share_menu(mesh_listen),
            "l" | "c" | "limit" | "cpu" => set_cpu_flow(arg, cpu, cores),
            other => match other.parse::<usize>() {
                Ok(n) if n >= 1 && n <= rows.len() => model_menu(host, hw, ctx, &rows[n - 1]),
                _ => warn("type a model number, or p / s / l / q"),
            },
        }
    }
    println!("{}", "the proxy stops when you leave. bye.".dimmed());
    Ok(())
}

/// Draw the whole panel and return the model rows in display order, so a typed
/// number maps to the right model.
fn render(host: &str, hw: &HardwareInfo, ctx: u32, listen: &str, cpu: &CpuLimit, cores: usize) -> Vec<Row> {
    println!();

    // Measured once per redraw (it shells out), then reused for the memory line
    // and the per-model "won't fit on disk" flags.
    let free = disk_free();

    // ── This machine ─────────────────────────────────────────────────────────
    let mut panel: Vec<(String, String)> = Vec::new();
    if hw.gpus.is_empty() {
        panel.push(("gpu".into(), "none — running on CPU".dimmed().to_string()));
    } else {
        for g in &hw.gpus {
            panel.push(("gpu".into(), format!("{} · {:.0} GB VRAM", g.name, g.vram_bytes as f64 / GIB)));
        }
    }
    let mem = match free {
        Some(free) => format!("{:.0} GB RAM · {:.0} GB free", hw.ram_bytes as f64 / GIB, free as f64 / GIB),
        None => format!("{:.0} GB RAM", hw.ram_bytes as f64 / GIB),
    };
    panel.push(("memory".into(), mem));
    panel.push(("cpu".into(), cpu_line(cpu, cores)));
    panel.push(("access".into(), access_line(listen)));
    panel.push(("sharing".into(), share_state()));
    ui::panel("this machine", &panel);

    // ── Models ───────────────────────────────────────────────────────────────
    let rows = build_rows(host, hw, ctx);
    ui::section("models");
    if rows.is_empty() {
        warn(&format!("can't reach ollama at {host} — start it with `ollama serve`"));
    } else {
        let w = ui::cols();
        let show_speed = w >= 64;
        let name_w = w.saturating_sub(if show_speed { 34 } else { 22 }).clamp(8, 34);
        for (i, r) in rows.iter().enumerate() {
            let marker = if r.endpoint.is_some() {
                "◆".cyan()
            } else if r.running {
                "▶".green()
            } else if r.installed {
                "●".green()
            } else {
                "○".dimmed()
            };
            let name = ui::pad(&ui::truncate(&r.tag, name_w), name_w);
            let mut tail = if show_speed && !r.speed.is_empty() {
                format!("{} · {}", r.fit, r.speed.dimmed())
            } else {
                r.fit.clone()
            };
            // Flag not-yet-installed models the disk can't hold (with headroom).
            if !r.installed {
                if let Some(free) = free {
                    if !disk_ok(r.bytes, free) {
                        tail.push_str(&format!("  {}", "· no disk space".red()));
                    }
                }
            }
            println!("  {:>2}  {} {}  {:>6}  {}", (i + 1).to_string().cyan(), marker, name, r.size, tail);
        }
        println!(
            "  {}",
            "◆ remote   ▶ running   ● installed   ○ fits, not installed yet".dimmed()
        );
        if let Some(line) = usage_glance() {
            println!("  {}", line.dimmed());
        }
    }

    // ── Help bar ─────────────────────────────────────────────────────────────
    println!(
        "\n  {}  pick a number to {} (●) or {} (○) a model",
        "▸".cyan(),
        "open".bold(),
        "install".bold(),
    );
    println!(
        "    {} find more    {} add a hosted model    {} share    {} limit cpu    {} quit",
        "p".cyan(),
        "a".cyan(),
        "s".cyan(),
        "l".cyan(),
        "q".cyan(),
    );

    rows
}

/// Build the unified, ordered model list: running first, then other installed,
/// then not-yet-installed catalog models that fit this machine (smallest first).
fn build_rows(host: &str, hw: &HardwareInfo, ctx: u32) -> Vec<Row> {
    // Reaching ollama is what tells us "installed" and "running". If it's down,
    // return nothing so the panel shows a single clear hint instead of a
    // misleading catalog the user can't actually install.
    let Ok(installed) = ollama::fetch_local(host) else {
        return Vec::new();
    };
    let running: Vec<String> = ollama_api::ps(host)
        .unwrap_or_default()
        .into_iter()
        .map(|m| norm_tag(&m.name))
        .collect();
    let is_running = |tag: &str| running.iter().any(|r| r == &norm_tag(tag));

    let mut have: Vec<Row> = installed
        .iter()
        .map(|m| {
            let tag = m.ollama_name.clone().unwrap_or_else(|| m.display_name().to_string());
            let (fit, speed) = fit_and_speed(m, hw, ctx);
            Row {
                running: is_running(&tag),
                installed: true,
                bytes: m.weight_bytes.unwrap_or(0),
                size: m.weight_bytes.map(fmt_gib).unwrap_or_else(|| "?".into()),
                fit,
                speed,
                endpoint: None,
                tag,
            }
        })
        .collect();
    // Running models float to the top; everything else stays alphabetical.
    have.sort_by(|a, b| b.running.cmp(&a.running).then(a.tag.cmp(&b.tag)));

    let installed_tags: std::collections::HashSet<String> =
        have.iter().map(|r| norm_tag(&r.tag)).collect();

    let mut available: Vec<(u64, Row)> = models::catalog()
        .into_iter()
        .filter_map(|m| {
            let tag = m.ollama_name.clone()?;
            if installed_tags.contains(&norm_tag(&tag)) {
                return None;
            }
            let (quant, res) = engine::best_quant(&m, hw, ctx)?;
            if matches!(res.fit, FitType::TooBig) {
                return None; // only show what this machine can actually run
            }
            let bytes = engine::weight_bytes(&m, quant);
            let (fit, speed) = fit_and_speed(&m, hw, ctx);
            Some((bytes, Row {
                tag, installed: false, running: false, bytes,
                size: fmt_gib(bytes), fit, speed, endpoint: None,
            }))
        })
        .collect();
    available.sort_by_key(|(bytes, _)| *bytes); // smallest download first

    // Registered remote endpoints (Modal, etc.) lead the list — they're the ones
    // you deliberately added, always reachable regardless of local hardware.
    let remote: Vec<Row> = endpoints::load()
        .into_iter()
        .map(|ep| Row {
            tag: ep.name.clone(),
            installed: false,
            running: false,
            bytes: 0,
            size: String::new(),
            fit: format!("remote · {}", endpoints::host_of(&ep.url)),
            speed: String::new(),
            endpoint: Some(ep),
        })
        .collect();

    remote
        .into_iter()
        .chain(have)
        .chain(available.into_iter().take(MAX_AVAILABLE).map(|(_, r)| r))
        .collect()
}

/// A model number was picked — offer only the steps that make sense for it.
fn model_menu(host: &str, hw: &HardwareInfo, ctx: u32, row: &Row) {
    if let Some(ep) = &row.endpoint {
        // ── remote endpoint: open (chat) / remove ─────────────────────────────
        println!("\n  {}  ·  {} · model {}", ep.name.bold(), endpoints::host_of(&ep.url).dimmed(), ep.model.dimmed());
        println!("  [o] open   [t] test   [x] remove   ·   [enter] back");
        let Some(a) = prompt("  > ") else { return };
        match a.trim().to_lowercase().as_str() {
            "o" | "open" | "run" => remote_chat(ep),
            "t" | "test" => test_endpoint(ep),
            "x" | "remove" | "rm" | "delete" => {
                if confirm(&format!("remove endpoint {}?", ep.name)) {
                    match endpoints::remove(&ep.name) {
                        Ok(_) => println!("  {}", format!("removed {}.", ep.name).green()),
                        Err(e) => warn(&e),
                    }
                }
            }
            _ => {}
        }
        return;
    }
    if !row.installed {
        // ── install (then optionally open) ────────────────────────────────────
        println!("\n  {}  ·  {} download  ·  {}", row.tag.bold(), row.size, plain_fit(&row.fit));
        // Guard the disk: refuse (with an override) if the download wouldn't fit
        // while keeping headroom, so an install can't wedge the machine.
        if let Some(free) = disk_free() {
            if !disk_ok(row.bytes, free) {
                warn(&format!(
                    "only {:.0} GB free — {} needs ~{:.0} GB plus headroom",
                    free as f64 / GIB,
                    row.tag,
                    row.bytes as f64 / GIB,
                ));
                if !confirm("install anyway?") {
                    return;
                }
            }
        }
        if !confirm(&format!("install {}?", row.tag)) {
            return;
        }
        match crate::manage::pull(host, hw, ctx, &row.tag, true) {
            Ok(()) => {
                if confirm(&format!("open {} now?", row.tag)) {
                    open(host, hw, ctx, &row.tag);
                }
            }
            Err(e) => warn(&e),
        }
        return;
    }

    // ── installed: open / close / delete ─────────────────────────────────────
    let state = if row.running { "running".green() } else { "ready".dimmed() };
    println!("\n  {}  ·  {}", row.tag.bold(), state);
    let close = if row.running { "  [c] close" } else { "" };
    println!("  [o] open{}  [d] delete   ·   [enter] back", close);
    let Some(a) = prompt("  > ") else { return };
    match a.trim().to_lowercase().as_str() {
        "o" | "open" | "run" => open(host, hw, ctx, &row.tag),
        "c" | "close" | "stop" if row.running => match ollama_api::stop(host, &row.tag) {
            Ok(()) => println!("  {}", format!("closed {} — memory freed.", row.tag).green()),
            Err(e) => warn(&e),
        },
        "d" | "delete" | "rm" => {
            if confirm(&format!("delete {} from disk?", row.tag)) {
                match crate::manage::rm(host, &row.tag) {
                    Ok(()) => println!("  {}", format!("deleted {}.", row.tag).green()),
                    Err(e) => warn(&e),
                }
            }
        }
        _ => {}
    }
}

/// Open (run/chat with) a model. Skips the fit prompt — it's already installed.
fn open(host: &str, hw: &HardwareInfo, ctx: u32, tag: &str) {
    if let Err(e) = crate::manage::run(host, hw, ctx, tag, true) {
        warn(&e);
    }
}

/// `p` — install a model by name (anything, not just what's listed).
fn pull_flow(host: &str, hw: &HardwareInfo, ctx: u32, arg: &str) {
    let model = if arg.is_empty() { prompt_line("model to install (e.g. qwen3:8b): ") } else { arg.to_string() };
    if model.is_empty() {
        return;
    }
    match crate::manage::pull(host, hw, ctx, &model, false) {
        Ok(()) => {
            if confirm(&format!("open {model} now?")) {
                open(host, hw, ctx, &model);
            }
        }
        Err(e) => warn(&e),
    }
}

/// `a` — register a hosted model (e.g. a Modal endpoint). Paste the URL and v2
/// probes it, lists the models it serves, and lets you pick one.
fn add_endpoint_flow(host: &str) {
    println!("\n  {}", "add a hosted model".bold());
    println!("  {}", "point v2 at any OpenAI-compatible endpoint (Modal, vLLM, TGI, …) or a remote Ollama.".dimmed());
    let raw_url = prompt_line("  endpoint url (e.g. https://you--app.modal.run): ");
    if raw_url.is_empty() {
        return;
    }
    let kind = endpoints::guess_kind(&raw_url);
    let url = match endpoints::normalize_base_url(&raw_url, kind) {
        Ok(url) => url,
        Err(e) => {
            warn(&e);
            return;
        }
    };
    if url != raw_url.trim().trim_end_matches('/') {
        println!("  {} {}", "normalized".dimmed(), url.dimmed());
    }
    let key = prompt_secret("  api key (optional, blank for none): ");
    let api_key = if key.is_empty() { None } else { Some(key) };

    // Probe once so we can offer a pick-list and confirm reachability up front.
    println!("  {}", "checking the endpoint…".dimmed());
    let model = match endpoints::probe(&url, kind, api_key.as_deref()) {
        Ok(models) if !models.is_empty() => pick_model(&models),
        Ok(_) => {
            warn("endpoint is reachable but lists no models");
            prompt_line("  model id to use: ")
        }
        Err(e) => {
            warn(&e);
            if !confirm("save it anyway?") {
                return;
            }
            prompt_line("  model id the endpoint serves: ")
        }
    };
    if model.is_empty() {
        return;
    }
    let mut name = prompt_line(&format!("  name [{model}]: "));
    if name.is_empty() {
        name = model.clone();
    }
    if let Some(reason) = endpoint_alias_collision(host, &name, &model) {
        warn(&reason);
        return;
    }
    let ep = Endpoint { name: name.clone(), url, model, kind, api_key };
    match endpoints::add(ep) {
        Ok(()) => println!("  {}", format!("added {name} — open it from the list to chat.").green()),
        Err(e) => warn(&e),
    }
}

fn endpoint_alias_collision(host: &str, name: &str, model: &str) -> Option<String> {
    let aliases = endpoints::aliases(&Endpoint {
        name: name.to_string(),
        url: "http://localhost".into(),
        model: model.to_string(),
        kind: endpoints::ApiKind::Openai,
        api_key: None,
    });
    for ep in endpoints::load() {
        if ep.name == name {
            continue;
        }
        for alias in &aliases {
            if endpoints::matches_model(&ep, alias) {
                return Some(format!("endpoint alias `{alias}` already belongs to `{}`", ep.name));
            }
        }
    }
    let local = ollama::fetch_local(host).unwrap_or_default();
    for tag in local.into_iter().filter_map(|m| m.ollama_name) {
        for alias in &aliases {
            let bare = tag.split(':').next().unwrap_or(&tag);
            if tag == *alias || bare == alias || norm_tag(alias) == tag {
                return Some(format!("endpoint alias `{alias}` collides with local Ollama model `{tag}`"));
            }
        }
    }
    None
}

/// Present the endpoint's models as a numbered pick-list (or accept a typed id).
fn pick_model(models: &[String]) -> String {
    println!("  {} model{} available:", models.len(), if models.len() == 1 { "" } else { "s" });
    for (i, m) in models.iter().enumerate().take(20) {
        println!("    {}  {}", (i + 1).to_string().cyan(), m);
    }
    let choice = prompt_line("  pick a number (or type a model id): ");
    match choice.parse::<usize>() {
        Ok(n) if n >= 1 && n <= models.len() => models[n - 1].clone(),
        _ => choice, // typed an id directly (or empty to cancel)
    }
}

/// Health-check a remote endpoint: reachable? does it still serve our model?
fn test_endpoint(ep: &Endpoint) {
    println!("  {}", "checking…".dimmed());
    match endpoints::probe(&ep.url, ep.kind, ep.api_key.as_deref()) {
        Ok(models) => {
            let has = models.iter().any(|m| m == &ep.model);
            let reachable = format!("reachable · {} model{}", models.len(), if models.len() == 1 { "" } else { "s" });
            println!("  {}", reachable.green());
            if has {
                println!("  {}", format!("{} is available.", ep.model).green());
            } else if !models.is_empty() {
                warn(&format!("{} is not in the endpoint's list — chat may fail", ep.model));
            }
        }
        Err(e) => warn(&e),
    }
}

/// Chat with a remote endpoint, metering tokens like everything else.
fn remote_chat(ep: &Endpoint) {
    println!("v2 open {}  (empty line or Ctrl-D to exit)", ep.name.bold());
    let mut messages: Vec<serde_json::Value> = Vec::new();
    loop {
        let Some(input) = prompt(&format!("{} ", ">".cyan())) else { break };
        let input = input.trim();
        if input.is_empty() {
            break;
        }
        messages.push(serde_json::json!({ "role": "user", "content": input }));
        let started = std::time::Instant::now();
        let result = match ep.kind {
            endpoints::ApiKind::Openai => endpoints::chat_openai(ep, &serde_json::Value::Array(messages.clone()), |tok| {
                print!("{tok}");
                io::stdout().flush().ok();
                true
            }),
            // A remote Ollama speaks the same streaming API as the local one.
            endpoints::ApiKind::Ollama => endpoints::normalize_base_url(&ep.url, ep.kind)
                .and_then(|url| ollama_api::chat_stream(&url, &ep.model, &serde_json::Value::Array(messages.clone()), |tok| {
                print!("{tok}");
                io::stdout().flush().ok();
                true
            }))
            .map(|(reply, stats)| (reply, (stats.prompt_eval_count, stats.eval_count))),
        };
        println!();
        match result {
            Ok((reply, (tin, tout))) => {
                if tout > 0 {
                    usage::append(&usage::UsageRecord {
                        ts: usage::now_unix(),
                        source: "remote".into(),
                        kind: "chat".into(),
                        model: ep.name.clone(),
                        tokens_in: tin,
                        tokens_out: tout,
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                    println!("{}", format!("  {tout} tok").dimmed());
                }
                messages.push(serde_json::json!({ "role": "assistant", "content": reply }));
            }
            Err(e) => {
                warn(&e);
                messages.pop(); // drop the unanswered turn
            }
        }
    }
    println!("bye.");
}

/// `l` — set the live CPU cap (thread count or percent). "0"/empty removes it.
fn set_cpu_flow(arg: &str, cpu: &CpuLimit, cores: usize) {
    let spec = if arg.is_empty() {
        prompt_line(&format!("cpu limit — threads or percent (0 = unlimited, {cores} cores): "))
    } else {
        arg.to_string()
    };
    match proxy::parse_cpu_spec(&spec, cores) {
        Ok(0) => {
            cpu.store(0, Ordering::Relaxed);
            println!("  {}", format!("no cap — Ollama may use all {cores} threads.").green());
        }
        Ok(n) => {
            cpu.store(n, Ordering::Relaxed);
            println!("  {}", format!("capped at {n} / {cores} threads for new requests.").green());
        }
        Err(e) => warn(&e),
    }
}

/// `s` — make your compute public and share it over the org mesh. The mesh is
/// mutually authenticated (Noise + signed membership certs) and only ever speaks
/// v2 — Ollama itself stays on loopback and is never exposed.
fn share_menu(mesh_listen: Option<&str>) {
    println!("\n  {}", "share your compute (org mesh)".bold());
    println!("  {}", "encrypted + mutually authenticated · teammates never touch your Ollama directly".dimmed());
    println!("    1  create an org (you become the admin)");
    println!("    2  invite a teammate  (prints a one-time ticket)");
    println!("    3  join an org with a ticket");
    println!("    4  status   ·   5  pause   ·   6  resume");
    println!("    [enter] back");
    let Some(choice) = prompt("  > ") else { return };
    let result = match choice.trim() {
        "1" => client::init(),
        "2" => return invite_flow(mesh_listen),
        "3" => {
            let ticket = prompt_line("paste the invite ticket: ");
            if ticket.is_empty() {
                return;
            }
            client::join(&ticket)
        }
        "4" => client::status(),
        "5" => client::pause(),
        "6" => client::resume(),
        _ => return,
    };
    if let Err(e) = result {
        warn(&e);
    }
}

/// Invite a teammate without making anyone hunt for an IP: derive the port from
/// the mesh listener and let the user pick from auto-detected local addresses.
fn invite_flow(mesh_listen: Option<&str>) {
    let port = mesh_listen.and_then(port_of);
    if port.is_none() {
        warn("you're not serving to the mesh yet — restart with `v2 serve --mesh-listen 0.0.0.0:4830`");
        println!("  {}", "you can still mint a ticket if you know your reachable address.".dimmed());
    }
    let addr = pick_reachable_addr(port);
    if addr.is_empty() {
        return;
    }
    println!("  {}", "share this one-time ticket with your teammate:".dimmed());
    if let Err(e) = client::invite(Some(&addr), None, 86_400) {
        warn(&e);
    }
}

/// Offer detected LAN addresses as a numbered menu (plus a manual fallback), so
/// the reachable address is a keystroke instead of an ip lookup.
fn pick_reachable_addr(port: Option<&str>) -> String {
    let addrs = detect_local_addrs();
    let (Some(port), false) = (port, addrs.is_empty()) else {
        return prompt_line("your reachable address (host:port): ");
    };
    println!("  where can teammates reach you?");
    for (i, a) in addrs.iter().enumerate() {
        println!("    {}  {}:{}", (i + 1).to_string().cyan(), a, port);
    }
    println!("    {}  enter another address", "0".cyan());
    match prompt_line("  > ").parse::<usize>() {
        Ok(0) => prompt_line("host:port: "),
        Ok(n) if n >= 1 && n <= addrs.len() => format!("{}:{}", addrs[n - 1], port),
        _ => String::new(),
    }
}

/// Best-effort private/LAN IPv4 addresses of this machine, loopback excluded.
fn detect_local_addrs() -> Vec<String> {
    // Linux: `hostname -I` lists every address, space-separated.
    if let Ok(out) = Command::new("hostname").arg("-I").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let v: Vec<String> = text
            .split_whitespace()
            .filter(|a| !a.contains(':') && !a.starts_with("127.")) // IPv4, non-loopback
            .map(str::to_string)
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    // macOS: query the usual Wi-Fi / Ethernet interfaces.
    for ifc in ["en0", "en1", "en2"] {
        if let Ok(out) = Command::new("ipconfig").args(["getifaddr", ifc]).output() {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !ip.is_empty() && !ip.starts_with("127.") {
                return vec![ip];
            }
        }
    }
    Vec::new()
}

/// The port portion of a `host:port` string.
fn port_of(addr: &str) -> Option<&str> {
    addr.rsplit_once(':').map(|(_, p)| p).filter(|p| !p.is_empty())
}

// ── Small view helpers ──────────────────────────────────────────────────────

fn cpu_line(cpu: &CpuLimit, cores: usize) -> String {
    match cpu.load(Ordering::Relaxed) {
        0 => format!("all {cores} threads  ·  press l to limit"),
        n => format!("{} of {cores} threads", format!("{n} capped").cyan()),
    }
}

/// Security posture of the local proxy — a lock, not an IP. Loopback binds are
/// private to this machine; anything else is called out as exposed.
fn access_line(listen: &str) -> String {
    if proxy::is_loopback(listen) {
        format!("{} — nothing on the network can reach it", "local only".green())
    } else {
        format!("{} on {} — reachable from your network", "EXPOSED".red().bold(), listen)
    }
}

/// Whether — and how — this node's compute is shared over the mesh.
fn share_state() -> String {
    match identity::MeshIdentity::load() {
        Ok(Some(ident)) => {
            let role = if identity::OrgRoot::load().is_ok() { "admin" } else { "member" };
            let known = mesh::gossip::PeersFile::load().peers.len();
            format!(
                "shared · {role} of org {} · {known} peer{} known",
                mesh::short_id(&ident.org_pub),
                if known == 1 { "" } else { "s" },
            )
        }
        _ => "private · press s to share".to_string(),
    }
}

fn fit_and_speed(m: &Model, hw: &HardwareInfo, ctx: u32) -> (String, String) {
    match engine::best_quant(m, hw, ctx) {
        Some((q, r)) => {
            let speed = bandwidth::estimate_tps(m, q, ctx, hw, &r.fit)
                .map(|(t, rough)| bandwidth::tps_label(t, rough))
                .unwrap_or_default();
            (fit_word(&r.fit), speed)
        }
        None => ("n/a".red().to_string(), String::new()),
    }
}

fn fit_word(fit: &FitType) -> String {
    match fit {
        FitType::FullGpu => "gpu".green().to_string(),
        FitType::PartialOffload { offload_pct } => format!("~{}% offload", offload_pct).cyan().to_string(),
        FitType::CpuOnly => "cpu".dimmed().to_string(),
        FitType::TooBig => "n/a".red().to_string(),
    }
}

/// Strip colour from a fit word for use in a plain sentence.
fn plain_fit(fit: &str) -> String {
    let plain: String = {
        let mut s = String::new();
        let mut in_esc = false;
        for c in fit.chars() {
            if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
            } else if c == '\x1b' {
                in_esc = true;
            } else {
                s.push(c);
            }
        }
        s
    };
    match plain.as_str() {
        "gpu" => "fits on GPU".to_string(),
        "cpu" => "runs on CPU (slower)".to_string(),
        other => other.to_string(),
    }
}

fn fmt_gib(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / GIB)
}

/// Normalise a tag for equality checks (Ollama treats a bare tag as `:latest`).
fn norm_tag(tag: &str) -> String {
    let t = tag.trim();
    if t.contains(':') { t.to_string() } else { format!("{t}:latest") }
}

/// A one-line summary of what the proxy has metered so far, or None if nothing
/// has flowed through yet.
fn usage_glance() -> Option<String> {
    let recs = usage::read_all();
    if recs.is_empty() {
        return None;
    }
    let (tin, tout) = recs
        .iter()
        .fold((0u64, 0u64), |(i, o), r| (i + r.tokens_in, o + r.tokens_out));
    Some(format!("metered {} requests · {} in / {} out tokens", recs.len(), tin, tout))
}

/// Free space on the volume Ollama writes weights to (best effort).
fn disk_free() -> Option<u64> {
    let dir = std::env::var_os("OLLAMA_MODELS")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    crate::hardware::disk_free_bytes(&dir)
}

// ── Line-based input helpers ────────────────────────────────────────────────

/// Print a prompt and read one line. `None` on EOF (Ctrl-D) so callers can quit.
fn prompt(p: &str) -> Option<String> {
    print!("{p}");
    io::stdout().flush().ok();
    let mut s = String::new();
    match io::stdin().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s),
    }
}

/// Like `prompt` but trims and treats EOF as an empty answer (caller aborts).
fn prompt_line(p: &str) -> String {
    prompt(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn prompt_secret(p: &str) -> String {
    print!("{p}");
    io::stdout().flush().ok();
    set_echo(false);
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    set_echo(true);
    println!();
    s.trim().to_string()
}

fn set_echo(on: bool) {
    #[cfg(unix)]
    {
        let arg = if on { "echo" } else { "-echo" };
        let _ = Command::new("stty").arg(arg).status();
    }
    #[cfg(not(unix))]
    {
        let _ = on;
    }
}

fn confirm(question: &str) -> bool {
    match prompt(&format!("{question} [Y/n] ")) {
        Some(a) => {
            let a = a.trim().to_lowercase();
            a.is_empty() || a == "y" || a == "yes"
        }
        None => false,
    }
}

fn warn(msg: &str) {
    println!("  {} {}", "!".red(), msg);
}

/// Split a line into (first token, remainder) on the first whitespace run.
fn split_first(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim_start()),
        None => (line, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_first_separates_command_and_argument() {
        assert_eq!(split_first("p qwen3:8b"), ("p", "qwen3:8b"));
        assert_eq!(split_first("  p   qwen3:8b  ".trim()), ("p", "qwen3:8b"));
        assert_eq!(split_first("refresh"), ("refresh", ""));
        assert_eq!(split_first(""), ("", ""));
    }

    #[test]
    fn norm_tag_defaults_to_latest() {
        assert_eq!(norm_tag("qwen3"), "qwen3:latest");
        assert_eq!(norm_tag("qwen3:8b"), "qwen3:8b");
        assert_eq!(norm_tag(" qwen3:8b "), "qwen3:8b");
    }

    #[test]
    fn disk_ok_keeps_headroom_and_ignores_unknown() {
        let gib = 1024u64 * 1024 * 1024;
        assert!(disk_ok(0, 0)); // unknown download size never blocks
        assert!(disk_ok(4 * gib, 10 * gib)); // 4G download, 10G free — fine
        assert!(!disk_ok(4 * gib, 5 * gib)); // 4G + 2G headroom > 5G free — blocked
        assert!(!disk_ok(9 * gib, 10 * gib)); // headroom pushes it over
    }

    #[test]
    fn plain_fit_reads_as_a_sentence() {
        assert_eq!(plain_fit(&fit_word(&FitType::FullGpu)), "fits on GPU");
        assert_eq!(plain_fit(&fit_word(&FitType::CpuOnly)), "runs on CPU (slower)");
        assert!(plain_fit(&fit_word(&FitType::PartialOffload { offload_pct: 40 })).contains("40"));
    }
}
