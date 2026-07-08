//! Daemon-side Ollama client: the endpoints the plain scan doesn't need
//! (`/api/ps`, `/api/pull`, `/api/chat`, `/api/delete`). Blocking, streaming.

use std::io::{BufRead, BufReader, Read};

use serde::Deserialize;

/// One running model as reported by `GET /api/ps`.
#[derive(Debug, Clone, Deserialize)]
pub struct RunningModel {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub size_vram: u64,
    #[serde(default)]
    pub expires_at: String,
}

#[derive(Debug, Deserialize)]
struct PsResponse {
    #[serde(default)]
    models: Vec<RunningModel>,
}

pub fn ps(host: &str) -> Result<Vec<RunningModel>, String> {
    let url = format!("{}/api/ps", host.trim_end_matches('/'));
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("ollama unreachable at {url}: {e}"))?;
    let payload: PsResponse = resp
        .into_json()
        .map_err(|e| format!("invalid /api/ps response: {e}"))?;
    Ok(payload.models)
}

/// Stats block Ollama appends to the final line of a generate/chat stream.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct GenStats {
    #[serde(default)]
    pub prompt_eval_count: u64,
    #[serde(default)]
    pub eval_count: u64,
    #[serde(default)]
    pub total_duration: u64, // nanoseconds
    #[serde(default)]
    pub done: bool,
}

/// Stream `POST /api/pull`, invoking `on_status` for each progress line.
pub fn pull<F: FnMut(&str, u64, u64)>(host: &str, model: &str, mut on_status: F) -> Result<(), String> {
    let url = format!("{}/api/pull", host.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .send_json(ureq::json!({ "model": model, "stream": true }))
        .map_err(|e| format!("pull failed: {e}"))?;

    let reader = BufReader::new(resp.into_reader());
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        #[derive(Deserialize)]
        struct Prog {
            #[serde(default)]
            status: String,
            #[serde(default)]
            completed: u64,
            #[serde(default)]
            total: u64,
            #[serde(default)]
            error: Option<String>,
        }
        if let Ok(p) = serde_json::from_str::<Prog>(&line) {
            if let Some(err) = p.error {
                return Err(err);
            }
            on_status(&p.status, p.completed, p.total);
        }
    }
    Ok(())
}

pub fn delete(host: &str, model: &str) -> Result<(), String> {
    let url = format!("{}/api/delete", host.trim_end_matches('/'));
    ureq::delete(&url)
        .send_json(ureq::json!({ "model": model }))
        .map_err(|e| format!("delete failed: {e}"))?;
    Ok(())
}

/// Unload a model from memory now, without deleting its weights. Ollama frees a
/// model when `keep_alive` reaches 0, so a zero-length generate with
/// `keep_alive: 0` closes it immediately. This is "close", not "delete".
pub fn stop(host: &str, model: &str) -> Result<(), String> {
    let url = format!("{}/api/generate", host.trim_end_matches('/'));
    ureq::post(&url)
        .send_json(ureq::json!({ "model": model, "keep_alive": 0 }))
        .map_err(|e| format!("close failed: {e}"))?;
    Ok(())
}

/// One turn of `POST /api/chat` (streamed). `on_token` receives each content
/// delta and returns `false` to abort the stream (dropping the upstream
/// connection, which makes Ollama stop generating — the reclaim/deadman path).
/// `messages` is the full history — Ollama's chat API is stateless, so no
/// context is retained server-side.
pub fn chat_stream<F: FnMut(&str) -> bool>(
    host: &str,
    model: &str,
    messages: &serde_json::Value,
    mut on_token: F,
) -> Result<(String, GenStats), String> {
    let url = format!("{}/api/chat", host.trim_end_matches('/'));
    // Bound a stalled upstream: if Ollama goes silent between reads for this
    // long, the read errors out instead of hanging the serving thread forever.
    let agent = ureq::AgentBuilder::new()
        .timeout_read(std::time::Duration::from_secs(60))
        .build();
    let resp = agent
        .post(&url)
        .send_json(ureq::json!({ "model": model, "messages": messages, "stream": true }))
        .map_err(|e| format!("chat failed: {e}"))?;

    let reader = BufReader::new(resp.into_reader());
    let mut full = String::new();
    let mut stats = GenStats::default();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        #[derive(Deserialize)]
        struct ChatChunk {
            #[serde(default)]
            message: Option<ChatMsg>,
            #[serde(flatten)]
            stats: GenStats,
        }
        #[derive(Deserialize)]
        struct ChatMsg {
            #[serde(default)]
            content: String,
        }
        if let Ok(chunk) = serde_json::from_str::<ChatChunk>(&line) {
            if let Some(m) = chunk.message {
                if !m.content.is_empty() {
                    full.push_str(&m.content);
                    if !on_token(&m.content) {
                        // Caller aborted: drop the reader here, which closes the
                        // upstream connection and stops Ollama mid-generation.
                        return Ok((full, stats));
                    }
                }
            }
            if chunk.stats.done {
                stats = chunk.stats;
            }
        }
    }
    Ok((full, stats))
}

/// Read a full body into bytes (used by the proxy for non-streamed passthrough).
pub fn drain(mut r: impl Read) -> Vec<u8> {
    let mut buf = vec![];
    let _ = r.read_to_end(&mut buf);
    buf
}
