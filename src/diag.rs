use crate::config::Settings;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn diag_path(settings: &Settings) -> PathBuf {
    crate::config::expand_path(&settings.diag_dir)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn today_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn should_sample(settings: &Settings, event: &str) -> bool {
    // sampling only for high-volume per-request events
    if !event.starts_with("request.") {
        return true;
    }
    let every = settings.diag_sample_every.max(1);
    if every == 1 {
        return true;
    }
    COUNTER.fetch_add(1, Ordering::Relaxed) % every == 0
}

fn rotate_if_needed(path: &PathBuf, max_bytes: u64, max_files: usize) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() <= max_bytes {
        return;
    }
    let dir = match path.parent() {
        Some(d) => d.to_path_buf(),
        None => return,
    };
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("diag.jsonl")
        .to_string();
    for i in (1..max_files).rev() {
        let src = if i == 1 {
            path.clone()
        } else {
            dir.join(format!("{stem}.{i}"))
        };
        let dst = dir.join(format!("{stem}.{}", i + 1));
        if src.exists() {
            let _ = fs::rename(&src, &dst);
        }
    }
    let dst1 = dir.join(format!("{stem}.1"));
    let _ = fs::rename(path, &dst1);
    for i in (max_files + 1)..(max_files + 10) {
        let p = dir.join(format!("{stem}.{i}"));
        if p.exists() {
            let _ = fs::remove_file(&p);
        } else {
            break;
        }
    }
}

/// Best-effort append one JSONL line to today's diag file. Never panics, never logs secrets.
pub fn append(settings: &Settings, event: &str, mut payload: Value) {
    if settings.diag_max_files == 0 {
        return;
    }
    if !should_sample(settings, event) {
        return;
    }
    let dir = diag_path(settings);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("diag: create_dir_all {} failed: {e}", dir.display());
        return;
    }
    let file = dir.join(format!("diag-{}.jsonl", today_str()));
    rotate_if_needed(&file, settings.diag_max_bytes, settings.diag_max_files);
    if let Value::Object(ref mut map) = payload {
        map.insert("ts_ms".to_string(), json!(now_ms()));
        map.insert("event".to_string(), json!(event));
    }
    let line = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    match opts.open(&file) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        Err(e) => {
            eprintln!("diag: open {} failed: {e}", file.display());
        }
    }
}

pub fn diag_enabled(settings: &Settings) -> bool {
    settings.diag_max_files > 0
}

/// Redacted payload summary for logging (no message content, no keys).
pub fn payload_summary(payload: &Value) -> Value {
    let thinking = payload.get("thinking").cloned().unwrap_or(Value::Null);
    let reasoning_effort = payload
        .get("reasoning_effort")
        .cloned()
        .unwrap_or(Value::Null);
    let tool_choice = payload.get("tool_choice").cloned().unwrap_or(Value::Null);
    let response_format = payload
        .get("response_format")
        .cloned()
        .unwrap_or(Value::Null);
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let has_tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let n = payload.get("n").and_then(Value::as_i64).unwrap_or(1);
    // thinking details: type field
    let thinking_type = thinking
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(if thinking.is_null() { "" } else { "unknown" });
    json!({
        "model": payload.get("model").and_then(Value::as_str).unwrap_or(""),
        "stream": stream,
        "n": n,
        "messages": messages,
        "has_tools": has_tools,
        "thinking": thinking,
        "thinking_type": thinking_type,
        "reasoning_effort": reasoning_effort,
        "tool_choice": tool_choice,
        "response_format": response_format,
    })
}
