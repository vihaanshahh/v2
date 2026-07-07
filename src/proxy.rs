//! Local metering proxy (Phase 1). Sits on :11435 in front of Ollama on :11434,
//! forwards every request unchanged, and meters exact token counts from Ollama's
//! own stream stats. Ollama stays bound to localhost; apps point at v2 instead.
//!
//! Deadman by design (DESIGN.md §4): the response is streamed, not buffered. If
//! the client disconnects or the daemon dies, the write fails, the upstream
//! reader drops, and Ollama aborts generation. The metering record is still
//! written from the reader's Drop, so partial usage is never lost.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Instant;

use crate::activity::Activity;
use crate::ollama_api::GenStats;
use crate::usage::{self, UsageRecord};

/// Run the metering proxy until the process is stopped. Blocks.
pub fn serve(listen: &str, ollama_host: &str, activity: Activity) -> Result<(), String> {
    let server = tiny_http::Server::http(listen)
        .map_err(|e| format!("cannot bind {listen}: {e}"))?;
    let ollama_host = Arc::new(ollama_host.trim_end_matches('/').to_string());

    println!("v2 proxy  {listen} -> {ollama_host}  (metering local usage)");

    for request in server.incoming_requests() {
        let host = ollama_host.clone();
        let act = activity.clone();
        // Thread per request: concurrent local apps, blocking I/O, no async.
        std::thread::spawn(move || {
            if let Err(e) = handle(request, &host, &act) {
                eprintln!("v2 proxy: {e}");
            }
        });
    }
    Ok(())
}

fn handle(mut request: tiny_http::Request, ollama_host: &str, activity: &Activity) -> Result<(), String> {
    activity.touch();

    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let model = String::new(); // filled in below if we can see it in the body

    // Read the incoming body (the prompt). In memory only — never written to disk.
    let mut body = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("read body: {e}"))?;

    let model = detect_model(&body).unwrap_or(model);

    let upstream_url = format!("{ollama_host}{url}");
    let mut req = ureq::request(&method, &upstream_url);
    // Forward content-type so Ollama parses JSON bodies.
    if let Some(ct) = header_value(&request, "content-type") {
        req = req.set("Content-Type", &ct);
    }

    let resp = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(&body)
    };

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            // Forward upstream error responses verbatim.
            let bytes = crate::ollama_api::drain(r.into_reader());
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

/// Best-effort model name from a request body ({"model": "..."}).
fn detect_model(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(|s| s.to_string())
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

/// A minimal write helper used by `v2 serve` startup to report readiness.
pub fn banner(msg: &str) {
    let _ = std::io::stdout().write_all(msg.as_bytes());
    let _ = std::io::stdout().write_all(b"\n");
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
    fn meters_tokens_from_stream_and_passes_bytes_through() {
        let mut r = MeteringReader::new(STREAM, "qwen3:8b".into(), "local", "local");
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        // Bytes passed through unchanged (transparent proxy).
        assert_eq!(out, STREAM);
        // Stats extracted from the final line.
        let stats = r.last_stats.clone().expect("stats parsed");
        assert_eq!(stats.prompt_eval_count, 11);
        assert_eq!(stats.eval_count, 42);
        assert!(stats.done);
        // Suppress the on-drop usage append in tests (no ~/.v2 writes).
        r.logged = true;
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
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(r.last_stats.as_ref().unwrap().eval_count, 42);
        r.logged = true;
    }
}
