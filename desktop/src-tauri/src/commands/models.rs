use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use v2::manage::{self, FitCheck, InstalledModel};
use v2::ollama_api::{self, RunningModel};

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
        let (content, stats) = ollama_api::chat_stream(&host, &model, &serde_json::Value::Array(msgs), |tok| {
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
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
