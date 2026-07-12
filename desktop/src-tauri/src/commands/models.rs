use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use v2::endpoints::{self, ApiKind};
use v2::manage::{self, FitCheck, InstalledModel};
use v2::mesh::client::{self, ChatTarget, RemoteChatResult};
use v2::ollama_api::{self, RunningModel};
use v2::usage::{self, UsageRecord};

fn host_or_default(host: Option<String>) -> String {
    host.filter(|h| !h.trim().is_empty())
        .unwrap_or_else(v2::ollama::default_host)
}

#[tauri::command]
pub fn models_installed(host: Option<String>, ctx: u32) -> Result<Vec<InstalledModel>, String> {
    let hw = v2::hardware::detect();
    manage::installed_with_fit(&host_or_default(host), &hw, ctx)
}

#[tauri::command]
pub fn models_loaded(host: Option<String>) -> Result<Vec<RunningModel>, String> {
    ollama_api::ps(&host_or_default(host))
}

#[tauri::command]
pub fn model_fit_check(query: String, ctx: u32) -> FitCheck {
    let hw = v2::hardware::detect();
    manage::fit_check(&query, &hw, ctx)
}

#[tauri::command]
pub fn model_rm(host: Option<String>, model: String) -> Result<(), String> {
    ollama_api::delete(&host_or_default(host), &model)
}

#[tauri::command]
pub fn model_stop(host: Option<String>, model: String) -> Result<(), String> {
    ollama_api::stop(&host_or_default(host), &model)
}

#[derive(Debug, Clone, Serialize)]
pub struct PullProgress {
    pub model: String,
    pub status: String,
    pub completed: u64,
    pub total: u64,
}

/// Streams progress as `pull-progress` events (frontend subscribes with
/// `listen()`), resolving once the download completes or errors.
#[tauri::command]
pub async fn model_pull(app: AppHandle, host: Option<String>, model: String) -> Result<(), String> {
    let host = host_or_default(host);
    let model_for_thread = model.clone();
    tauri::async_runtime::spawn_blocking(move || {
        ollama_api::pull(&host, &model_for_thread, |status, completed, total| {
            let _ = app.emit(
                "pull-progress",
                PullProgress {
                    model: model_for_thread.clone(),
                    status: status.to_string(),
                    completed,
                    total,
                },
            );
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatReply {
    pub content: String,
    pub tokens: u64,
    pub tps: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRoute {
    pub route: String,
    pub model: String,
    pub peer_addr: Option<String>,
}

#[tauri::command]
pub fn chat_targets(host: Option<String>, ctx: u32) -> Result<Vec<ChatTarget>, String> {
    let hw = v2::hardware::detect();
    client::chat_targets(&host_or_default(host), &hw, ctx)
}

/// Unified chat: local Ollama, mesh peer, or registered hosted endpoint.
#[tauri::command]
pub async fn chat_send(
    app: AppHandle,
    route: ChatRoute,
    messages: Vec<ChatMessage>,
    ctx: u32,
) -> Result<ChatReply, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let msgs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();
        let history = serde_json::Value::Array(msgs);

        match route.route.as_str() {
            "local" => local_chat(&app, &host_or_default(None), &route.model, &history),
            "mesh" => mesh_chat(&app, &route.model, ctx, &history, route.peer_addr.as_deref()),
            "endpoint" => endpoint_chat(&app, &route.model, &history),
            other => Err(format!("unknown chat route: {other}")),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

fn local_chat(app: &AppHandle, host: &str, model: &str, history: &serde_json::Value) -> Result<ChatReply, String> {
    let (content, stats) = ollama_api::chat_stream(host, model, history, |tok| {
        let _ = app.emit("chat-token", tok);
        true
    })?;
    let tps = if stats.total_duration > 0 {
        stats.eval_count as f64 / (stats.total_duration as f64 / 1e9)
    } else {
        0.0
    };
    Ok(ChatReply {
        content,
        tokens: stats.eval_count,
        tps,
        peer_id: None,
        tokens_in: Some(stats.prompt_eval_count),
    })
}

fn mesh_chat(
    app: &AppHandle,
    model: &str,
    ctx: u32,
    history: &serde_json::Value,
    peer_addr: Option<&str>,
) -> Result<ChatReply, String> {
    let result = client::remote_chat(model, ctx, history, peer_addr, |tok| {
        let _ = app.emit("chat-token", tok);
        true
    })?;
    Ok(mesh_result_to_reply(result))
}

fn mesh_result_to_reply(result: RemoteChatResult) -> ChatReply {
    ChatReply {
        content: result.content,
        tokens: result.tokens_out,
        tps: result.tps,
        peer_id: result.peer_id,
        tokens_in: Some(result.tokens_in),
    }
}

fn endpoint_chat(app: &AppHandle, model: &str, history: &serde_json::Value) -> Result<ChatReply, String> {
    let ep = endpoints::find_model(model).ok_or_else(|| format!("no endpoint registered for {model}"))?;
    let started = std::time::Instant::now();
    let (content, tin, tout) = match ep.kind {
        ApiKind::Openai => {
            let (reply, (tin, tout)) = endpoints::chat_openai(&ep, history, |tok| {
                let _ = app.emit("chat-token", tok);
                true
            })?;
            (reply, tin, tout)
        }
        ApiKind::Ollama => {
            let url = endpoints::normalize_base_url(&ep.url, ep.kind)?;
            let (reply, stats) = ollama_api::chat_stream(&url, &ep.model, history, |tok| {
                let _ = app.emit("chat-token", tok);
                true
            })?;
            (reply, stats.prompt_eval_count, stats.eval_count)
        }
    };
    let elapsed = started.elapsed().as_millis() as u64;
    usage::append(&UsageRecord {
        ts: usage::now_unix(),
        source: "remote".into(),
        kind: "chat".into(),
        model: ep.name.clone(),
        tokens_in: tin,
        tokens_out: tout,
        duration_ms: elapsed,
    });
    let tps = if elapsed > 0 {
        tout as f64 / (elapsed as f64 / 1000.0)
    } else {
        0.0
    };
    Ok(ChatReply {
        content,
        tokens: tout,
        tps,
        peer_id: None,
        tokens_in: Some(tin),
    })
}

/// Runs one chat turn against the full message history the frontend sends
/// (Ollama's chat API is stateless — no server-side conversation state).
/// Streams token deltas as `chat-token` events for a live-typing effect, then
/// resolves with the full reply once done.
#[tauri::command]
pub async fn model_chat(
    app: AppHandle,
    host: Option<String>,
    model: String,
    messages: Vec<ChatMessage>,
) -> Result<ChatReply, String> {
    let host = host_or_default(host);
    tauri::async_runtime::spawn_blocking(move || {
        let msgs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();
        local_chat(&app, &host, &model, &serde_json::Value::Array(msgs))
    })
    .await
    .map_err(|e| e.to_string())?
}
