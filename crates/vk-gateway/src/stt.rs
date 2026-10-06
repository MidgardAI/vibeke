//! Optional local speech-to-text (spec 16 §8.4): runs the host-configured command on a temp file.

use std::time::Duration;

use serde_json::{Value, json};

use crate::Gateway;
use crate::api::{ApiError, ApiResult};

const MAX_AUDIO: usize = 10 * 1024 * 1024;

pub async fn transcribe(gw: &Gateway, p: &Value) -> ApiResult {
    let Some(stt) = &gw.cfg.stt else {
        return Err(ApiError::new(
            "unsupported",
            "speech-to-text is not configured on this host (gateway.toml [stt])",
        ));
    };
    let data = p
        .get("data_b64")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::invalid("data_b64 is required"))?;
    let audio = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
        .map_err(|_| ApiError::invalid("data_b64"))?;
    if audio.len() > MAX_AUDIO {
        return Err(ApiError::new("too_large", "audio is limited to 10 MiB"));
    }
    let ext = match p.get("mime").and_then(|v| v.as_str()).unwrap_or("") {
        m if m.contains("mp4") || m.contains("m4a") => "m4a",
        m if m.contains("ogg") => "ogg",
        m if m.contains("wav") => "wav",
        _ => "webm",
    };
    let dir = tempfile::Builder::new()
        .prefix("vibeke-stt")
        .tempdir()
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    let file = dir.path().join(format!("audio.{ext}"));
    std::fs::write(&file, &audio).map_err(|e| ApiError::new("internal", e.to_string()))?;
    let argv: Vec<String> = stt
        .command
        .iter()
        .map(|a| a.replace("{file}", &file.display().to_string()))
        .collect();
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| ApiError::new("unsupported", "stt.command is empty"))?;
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ApiError::unavailable("transcription timed out"))?
    .map_err(|e| ApiError::unavailable(format!("transcription failed: {e}")))?;
    drop(dir); // deletes the audio
    if !out.status.success() {
        return Err(ApiError::unavailable("transcription command failed"));
    }
    Ok(json!({"text": String::from_utf8_lossy(&out.stdout).trim()}))
}
