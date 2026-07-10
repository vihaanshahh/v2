use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;
use v2::activity::Activity;
use v2::doctor::DoctorReport;
use v2::proxy::{self, CpuLimit, EndpointInfo};
use v2::usage::{self, UsageSummary};

struct ProxyHandle {
    running: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub struct ProxyState(Mutex<Option<ProxyHandle>>);

#[derive(Debug, Clone, Serialize)]
pub struct ServeStatus {
    pub running: bool,
    pub listen: Option<String>,
}

/// Starts the metering proxy on a background thread. Idempotent-ish: refuses
/// if already running rather than leaking a second listener on the same port.
#[tauri::command]
pub fn serve_start(
    state: State<ProxyState>,
    listen: Option<String>,
    host: Option<String>,
    cpu: Option<String>,
) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|_| "proxy state poisoned".to_string())?;
    if guard.is_some() {
        return Err("already serving".into());
    }

    let listen = listen.unwrap_or_else(|| "127.0.0.1:11435".to_string());
    let host = host.unwrap_or_else(v2::ollama::default_host);
    let cores = proxy::cpu_cores();
    let cpu_count = proxy::parse_cpu_spec(cpu.as_deref().unwrap_or(""), cores)?;
    let cpu_limit: CpuLimit = Arc::new(std::sync::atomic::AtomicUsize::new(cpu_count));
    let running = Arc::new(AtomicBool::new(true));
    let running_thread = running.clone();
    let activity = Activity::new();
    let listen_thread = listen.clone();

    let thread = std::thread::spawn(move || {
        if let Err(e) = proxy::serve_with_shutdown(&listen_thread, &host, activity, cpu_limit, running_thread) {
            eprintln!("v2 desktop: proxy stopped: {e}");
        }
    });

    *guard = Some(ProxyHandle { running, thread });
    Ok(())
}

/// Flips the shutdown flag and blocks until the proxy thread actually exits,
/// so the caller can rely on the port being free the moment this returns.
#[tauri::command]
pub fn serve_stop(state: State<ProxyState>) -> Result<(), String> {
    let handle = state
        .0
        .lock()
        .map_err(|_| "proxy state poisoned".to_string())?
        .take();
    if let Some(handle) = handle {
        handle.running.store(false, Ordering::Relaxed);
        let _ = handle.thread.join();
    }
    Ok(())
}

#[tauri::command]
pub fn serve_status(state: State<ProxyState>) -> Result<ServeStatus, String> {
    let guard = state.0.lock().map_err(|_| "proxy state poisoned".to_string())?;
    Ok(ServeStatus {
        running: guard.is_some(),
        listen: None,
    })
}

#[tauri::command]
pub fn usage_summary() -> UsageSummary {
    usage::summarize(&usage::read_all())
}

#[tauri::command]
pub fn doctor(host: Option<String>) -> DoctorReport {
    let host = host.unwrap_or_else(v2::ollama::default_host);
    v2::doctor::doctor_report(&host)
}

#[tauri::command]
pub fn endpoint_banner(listen: Option<String>, host: Option<String>) -> Result<EndpointInfo, String> {
    let listen = listen.unwrap_or_else(|| "127.0.0.1:11435".to_string());
    let host = host.unwrap_or_else(v2::ollama::default_host);
    proxy::endpoint_info(&listen, &host, true)
}
