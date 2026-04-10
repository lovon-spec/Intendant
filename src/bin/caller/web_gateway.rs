use crate::presence::{self, AgentStateSnapshot};
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::types::LogLevel;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

/// Monotonically increasing counter for assigning unique peer IDs to WebSocket
/// connections.  Used for WebRTC signaling so that each browser tab gets a
/// stable identity within a display session.
static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);

/// Tracks which WebSocket connection currently owns the voice model (is "active").
/// Only one connection can be active at a time; all others are "passive" (TUI-only).
struct ActivePresence {
    connection_id: String,
    direct_tx: mpsc::UnboundedSender<String>,
}

pub const DEFAULT_PORT: u16 = 8765;

/// Mint a short-lived vendor session token server-side so the browser
/// never handles (or stores) a long-lived API key.
async fn mint_session_token(provider: &str, model: &str) -> Result<String, String> {
    match provider {
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| "OPENAI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "model": model,
            });
            let resp = reqwest::Client::new()
                .post("https://api.openai.com/v1/realtime/sessions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("OpenAI request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("OpenAI HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("OpenAI parse failed: {}", e))?;
            // Response may have token at top level or nested under client_secret
            let token = data["client_secret"]["value"]
                .as_str()
                .or_else(|| data["value"].as_str())
                .ok_or_else(|| format!("No token in OpenAI response: {}", data))?;
            let expires_at = data["client_secret"]["expires_at"]
                .as_i64()
                .or_else(|| data["expires_at"].as_i64())
                .unwrap_or(0);
            Ok(serde_json::json!({
                "client_secret": { "value": token },
                "expires_at": expires_at
            }).to_string())
        }
        "gemini" => {
            let api_key = std::env::var("GEMINI_API_KEY")
                .map_err(|_| "GEMINI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "uses": 1,
                "bidi_generate_content_setup": {
                    "model": format!("models/{}", model),
                    "generation_config": {
                        "response_modalities": ["AUDIO"],
                        "speech_config": {
                            "voice_config": {
                                "prebuilt_voice_config": {
                                    "voice_name": "Aoede"
                                }
                            }
                        }
                    }
                }
            });
            let url = format!(
                "https://generativelanguage.googleapis.com/v1alpha/auth_tokens?key={}",
                api_key
            );
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Gemini request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("Gemini HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Gemini parse failed: {}", e))?;
            let token = data["name"]
                .as_str()
                .ok_or("No 'name' in Gemini response")?;
            Ok(serde_json::json!({ "token": token }).to_string())
        }
        _ => Err(format!("Unknown provider: {}", provider)),
    }
}

const APP_HTML: &str = include_str!("../../../static/app.html");
const AUDIO_PROCESSOR_JS: &str = include_str!("../../../static/audio-processor.js");
const WASM_WEB_JS: &str = include_str!("../../../static/wasm-web/presence_web.js");
const WASM_WEB_BIN: &[u8] = include_bytes!("../../../static/wasm-web/presence_web_bg.wasm");

/// Session-specific state that changes when a new agent session starts.
/// Wrapped in `Arc<tokio::sync::RwLock<...>>` so the web gateway can observe
/// session changes without restarting.
pub struct ActiveSessionState {
    pub query_ctx: Option<WebQueryCtx>,
    pub frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    pub session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    pub recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    pub session_registry: Option<crate::display::SharedSessionRegistry>,
}

impl ActiveSessionState {
    pub fn empty() -> SharedActiveSession {
        Arc::new(tokio::sync::RwLock::new(Self {
            query_ctx: None,
            frame_registry: None,
            session_log: None,
            recording_registry: None,
            session_registry: None,
        }))
    }
}

pub type SharedActiveSession = Arc<tokio::sync::RwLock<ActiveSessionState>>;

/// Context for answering presence tool queries from browser-side live models.
/// Shared across all WebSocket connections (read-only for query tools).
#[derive(Clone)]
pub struct WebQueryCtx {
    pub agent_state: Arc<Mutex<AgentStateSnapshot>>,
    pub project_root: PathBuf,
    pub log_dir: PathBuf,
    pub knowledge_path: PathBuf,
    /// Server-authoritative presence session (event window + checkpoint state).
    pub presence_session: Option<Arc<Mutex<crate::presence::PresenceSession>>>,
    /// Shared context injection queue for mid-task interjections.
    pub context_injection: Option<crate::event::ContextInjectionQueue>,
}

/// Debug state for the voice model, tracked server-side from WebSocket messages.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceDebugState {
    pub connected: bool,
    pub voice_log_count: u32,
    pub last_voice_log: String,
}

/// Configuration sent to the web frontend via `/config`.
#[derive(Clone, Debug, Serialize)]
pub struct WebGatewayConfig {
    pub provider: String,
    pub model: String,
    pub input_sample_rate: u32,
    pub output_sample_rate: u32,
    /// Whether server-side transcription is enabled (browser should send user_audio).
    #[serde(default)]
    pub transcription_enabled: bool,
    /// ICE servers for WebRTC peer connections (STUN/TURN).
    /// Empty by default (local-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<crate::display::IceServer>,
}

impl Default for WebGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
        }
    }
}

/// Spawn the web gateway HTTP/WebSocket server.
///
/// - `GET /config` returns a JSON `WebGatewayConfig`.
/// - `GET /` (and any other path) returns the web TUI page.
/// - WebSocket connections are bridged to the EventBus (inbound control
///   messages) and broadcast channel (outbound events), mirroring the
///   Unix control socket in `control.rs`.
/// Convert session.jsonl entries into OutboundEvent-compatible JSON objects
/// for replaying to late-connecting browsers.
fn replay_session_log(contents: &str, log_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = obj.get("ts").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let level = obj.get("level").and_then(|v| v.as_str()).unwrap_or("info");
        let event_type = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let turn = obj.get("turn").and_then(|v| v.as_u64());

        // Skip truly internal events that have no display value.
        match event_type {
            "messages_input" | "session_start" | "json_extracted" => continue,
            _ => {}
        }

        // Map every session log event type to (source, content, log_level).
        let (source, content, log_level) = match event_type {
            // ── Turn lifecycle ──
            "turn_start" => ("system", format!("Turn {} started", turn.unwrap_or(0)), "info"),

            // ── Model response ──
            "model_response" => {
                let tokens = obj.get("data")
                    .and_then(|d| d.get("tokens"))
                    .and_then(|t| t.get("total"))
                    .and_then(|v| v.as_u64());
                let summary = if message.is_empty() {
                    format!("Model response{}", tokens.map(|t| format!(" ({} tokens)", t)).unwrap_or_default())
                } else {
                    format!("{}{}", message, tokens.map(|t| format!(" ({} tokens)", t)).unwrap_or_default())
                };
                ("worker", summary, "model")
            }

            // ── Reasoning ──
            "reasoning" => {
                let summary = obj.get("data")
                    .and_then(|d| d.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(message);
                if summary.is_empty() { continue; }
                ("worker", format!("Reasoning: {}", summary), "detail")
            }

            // ── Approval lifecycle ──
            "approval" => {
                let decision = obj.get("data")
                    .and_then(|d| d.get("decision"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let preview = obj.get("data")
                    .and_then(|d| d.get("preview"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(message);
                match decision {
                    "waiting" => ("worker", format!("Approval required: {}", preview), "warn"),
                    "approved" => ("system", format!("Approved: {}", preview), "info"),
                    "denied" | "denied-no-approver" => ("system", format!("Denied: {}", preview), "warn"),
                    "skipped" => ("system", format!("Skipped: {}", preview), "info"),
                    _ => ("system", message.to_string(), "info"),
                }
            }

            // ── Agent I/O ──
            "agent_input" => continue, // Superseded by agent_started which has the full commands
            "agent_started" => {
                // New sessions have pre-formatted previews; old sessions have raw JSON.
                // Try to detect raw JSON and format it, otherwise use as-is.
                let display = if message.starts_with('{') {
                    crate::format_commands_preview(message)
                } else {
                    message.to_string()
                };
                if display.is_empty() { continue; }
                ("agent", display, "agent")
            }
            "agent_output" => {
                // Read full content from the turn file (message is a preview).
                let full_stdout = obj.get("file")
                    .and_then(|f| f.as_str())
                    .and_then(|f| std::fs::read_to_string(log_dir.join(f)).ok())
                    .unwrap_or_else(|| message.to_string());
                let full_stderr = obj.get("file2")
                    .and_then(|f| f.as_str())
                    .and_then(|f| std::fs::read_to_string(log_dir.join(f)).ok())
                    .unwrap_or_default();
                let formatted = crate::tui::app::format_agent_output_for_tui(&full_stdout, &full_stderr);
                if formatted.is_empty() { continue; }
                let level = if !full_stderr.is_empty() { "warn" } else { "agent" };
                ("agent", formatted, level)
            }

            // ── Voice / presence lifecycle ──
            "presence_log" => {
                if message.is_empty() { continue; }
                ("presence", message.to_string(), level)
            }
            "voice_log" => {
                let text = obj.get("data")
                    .and_then(|d| d.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(message);
                if text.is_empty() { continue; }
                ("live", text.to_string(), "presence")
            }
            "user_transcript" => {
                let text = obj.get("data")
                    .and_then(|d| d.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(message);
                if text.is_empty() { continue; }
                ("live", format!("[You] {}", text), "presence")
            }
            "presence_connected" => {
                let provider = obj.get("data")
                    .and_then(|d| d.get("provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                ("live", format!("Live connected ({})", provider), "info")
            }
            "presence_disconnected" => {
                ("live", "Live disconnected".to_string(), "detail")
            }
            "presence_checkpoint" => {
                let summary = obj.get("data")
                    .and_then(|d| d.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                ("live", format!("Checkpoint: {}", summary), "debug")
            }

            // ── Tool dispatch ──
            "tool_request" => {
                let tool = obj.get("data")
                    .and_then(|d| d.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                ("live", format!("Tool request: {}", tool), "debug")
            }
            "tool_response" => {
                let tool = obj.get("data")
                    .and_then(|d| d.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                ("live", format!("Tool response: {}", tool), "debug")
            }

            // ── Task lifecycle ──
            "task_complete" | "round_complete" => {
                ("worker", message.to_string(), "info")
            }
            "session_end" | "summary" => {
                if message.is_empty() { continue; }
                ("system", message.to_string(), "info")
            }
            "interrupted" => {
                ("system", "Session interrupted".to_string(), "warn")
            }

            // ── General log levels ──
            "info" => {
                // Detect presence vs system from message prefix
                let source = if message.starts_with("[presence]")
                    || message.starts_with("[model]")
                    || message.starts_with("Presence")
                {
                    "server"
                } else {
                    "system"
                };
                // Detect detail-level presence internals
                let lvl = if message.starts_with("[model] Thinking")
                    || message.starts_with("[model] Tool call:")
                {
                    "detail"
                } else {
                    "info"
                };
                (source, message.to_string(), lvl)
            }
            "debug" => {
                let source = if message.starts_with("[model]")
                    || message.starts_with("[ws]")
                {
                    "server"
                } else {
                    "system"
                };
                (source, message.to_string(), "debug")
            }
            "warn" => ("system", message.to_string(), "warn"),
            "error" => ("system", message.to_string(), "error"),

            // ── Typed voice events (new) + backward compat ──
            "voice_audio" | "voice_frame" => continue, // Skip telemetry in replay
            "voice_protocol" => {
                ("live", message.to_string(), "debug")
            }
            "voice_usage" => {
                ("live", message.to_string(), "debug")
            }
            "voice_error" => {
                ("live", message.to_string(), "warn")
            }
            // Legacy: old logs still have "voice_diagnostic"
            "voice_diagnostic" => {
                let kind = obj
                    .get("data")
                    .and_then(|d| d.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match kind {
                    "audio_send" | "video_send" => continue,
                    "error" | "gemini_close" => ("live", message.to_string(), "warn"),
                    _ => ("live", message.to_string(), "debug"),
                }
            }

            // ── CU (Computer Use) structured events ──
            "cu_task_start" => {
                let task = obj
                    .get("data")
                    .and_then(|d| d.get("task"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(message);
                let provider = obj
                    .get("data")
                    .and_then(|d| d.get("provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let model = obj
                    .get("data")
                    .and_then(|d| d.get("model"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (
                    "worker",
                    format!("CU task: {} ({}:{})", task, provider, model),
                    "info",
                )
            }
            "cu_turn" => {
                let data = obj.get("data");
                let t = data
                    .and_then(|d| d.get("turn"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let actions = data
                    .and_then(|d| d.get("actions"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                (
                    "worker",
                    format!("CU turn {}: {}", t, actions),
                    "detail",
                )
            }
            "cu_task_complete" => {
                let turns = obj
                    .get("data")
                    .and_then(|d| d.get("turns"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                (
                    "worker",
                    format!("CU complete ({} turns)", turns),
                    "info",
                )
            }
            "cu_task_error" => {
                ("worker", format!("CU error: {}", message), "warn")
            }

            // ── Catch-all ──
            _ => {
                if message.is_empty() { continue; }
                ("system", message.to_string(), level)
            }
        };

        entries.push(serde_json::json!({
            "ts": ts,
            "level": log_level,
            "source": source,
            "content": content,
            "turn": turn,
        }));
    }
    entries
}

/// Compute a short content hash for cache-busting embedded static assets.
/// When the WASM or JS changes (i.e. a new build), the hash changes,
/// the URL changes, and browsers fetch the new version.
fn asset_version_hash() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    WASM_WEB_BIN.hash(&mut hasher);
    WASM_WEB_JS.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// List session directories from `~/.intendant/logs/`, returning JSON metadata
/// for each session (newest first, capped at 100).
/// Return session detail: replayed log entries + metadata for a single session.
/// Resolve a session directory by exact ID or prefix match.
fn resolve_session_dir(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let logs_dir = PathBuf::from(format!("{}/.intendant/logs", home));

    if logs_dir.join(session_id).is_dir() {
        return Some(logs_dir.join(session_id));
    }
    // Prefix match
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(session_id) {
                return Some(entry.path());
            }
        }
    }
    None
}

/// List recording streams from a recordings directory on disk.
fn list_recording_streams(recordings_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(recordings_dir) {
        for entry in dirs.flatten() {
            if !entry.path().is_dir() { continue; }
            let name = entry.file_name().to_string_lossy().to_string();
            let stream_dir = entry.path();
            let manifest = std::fs::read_to_string(stream_dir.join("manifest.json"))
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .unwrap_or(serde_json::json!({}));
            let segments = crate::recording::parse_segment_csv_pub(
                &stream_dir.join("segments.csv"),
                &stream_dir,
            );
            let total_duration = segments.last().map(|s| s.end_secs).unwrap_or(0.0);
            let seg_json: Vec<serde_json::Value> = segments.iter().map(|s| {
                serde_json::json!({
                    "filename": s.filename,
                    "start_secs": s.start_secs,
                    "end_secs": s.end_secs,
                })
            }).collect();
            let mut e = manifest;
            e["stream_name"] = serde_json::json!(name);
            e["segments"] = serde_json::Value::Array(seg_json);
            e["total_duration_secs"] = serde_json::json!(total_duration);
            entries.push(e);
        }
    }
    entries.sort_by(|a, b| {
        a["stream_name"].as_str().cmp(&b["stream_name"].as_str())
    });
    entries
}

fn get_session_detail(session_id: &str) -> String {
    let session_dir = match resolve_session_dir(session_id) {
        Some(d) => d,
        None => return serde_json::json!({"error": "session not found"}).to_string(),
    };

    let jsonl_path = session_dir.join("session.jsonl");
    let entries = if let Ok(contents) = std::fs::read_to_string(&jsonl_path) {
        replay_session_log(&contents, &session_dir)
    } else {
        Vec::new()
    };

    // Check for screenshot frames
    let frames_dir = session_dir.join("frames");
    let mut frames: Vec<String> = Vec::new();
    if frames_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&frames_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".png") || name.ends_with(".jpg") {
                    frames.push(name);
                }
            }
        }
        frames.sort();
    }

    serde_json::json!({
        "session_id": session_dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "entries": entries,
        "frames": frames,
    }).to_string()
}

fn list_sessions() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let logs_dir = PathBuf::from(format!("{}/.intendant/logs", home));
    if !logs_dir.is_dir() {
        return "[]".to_string();
    }

    let mut sessions: Vec<serde_json::Value> = Vec::new();

    let entries = match std::fs::read_dir(&logs_dir) {
        Ok(e) => e,
        Err(_) => return "[]".to_string(),
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let session_id = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Try to read session_meta.json first (fast path)
        let meta_path = dir.join("session_meta.json");
        let mut task: Option<String> = None;
        let mut created_at: Option<String> = None;
        let mut provider: Option<String> = None;
        let mut model: Option<String> = None;
        let mut status = "in_progress".to_string();
        let mut turns: u64 = 0;
        let mut total_tokens: u64 = 0;
        let mut prompt_tokens: u64 = 0;
        let mut completion_tokens: u64 = 0;
        let mut cached_tokens: u64 = 0;
        let mut role: Option<String> = None;

        if let Ok(meta_str) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str) {
                task = meta.get("task").and_then(|v| v.as_str()).map(|s| s.to_string());
                created_at = meta.get("created_at").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(s) = meta.get("status").and_then(|v| v.as_str()) {
                    status = s.to_string();
                }
                if let Some(t) = meta.get("last_turn").and_then(|v| v.as_u64()) {
                    turns = t;
                }
                role = meta.get("role").and_then(|v| v.as_str()).map(|s| s.to_string());
            }
        }

        // Parse session.jsonl for provider, model, token totals, and any missing fields
        let jsonl_path = dir.join("session.jsonl");
        if let Ok(contents) = std::fs::read_to_string(&jsonl_path) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let event = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");

                match event {
                    "session_start" => {
                        if created_at.is_none() {
                            created_at = obj.get("ts").and_then(|v| v.as_str()).map(|s| s.to_string());
                        }
                    }
                    "info" => {
                        if message.starts_with("Provider: ") && provider.is_none() {
                            provider = Some(message.trim_start_matches("Provider: ").to_string());
                        } else if message.starts_with("Model: ") && model.is_none() {
                            model = Some(message.trim_start_matches("Model: ").to_string());
                        } else if message.starts_with("Task: ") && task.is_none() {
                            task = Some(message.trim_start_matches("Task: ").to_string());
                        }
                    }
                    "turn_start" => {
                        if let Some(t) = obj.get("turn").and_then(|v| v.as_u64()) {
                            if t > turns {
                                turns = t;
                            }
                        }
                    }
                    "model_response" => {
                        if let Some(tok) = obj.get("data").and_then(|d| d.get("tokens")) {
                            if let Some(t) = tok.get("total").and_then(|v| v.as_u64()) {
                                total_tokens += t;
                            }
                            if let Some(p) = tok.get("prompt").and_then(|v| v.as_u64()) {
                                prompt_tokens += p;
                            }
                            if let Some(c) = tok.get("completion").and_then(|v| v.as_u64()) {
                                completion_tokens += c;
                            }
                            if let Some(cached) = tok.get("cached").and_then(|v| v.as_u64()) {
                                cached_tokens += cached;
                            }
                        }
                    }
                    "task_complete" | "session_end" | "round_complete" => {
                        status = "completed".to_string();
                    }
                    "interrupted" => {
                        status = "interrupted".to_string();
                    }
                    _ => {}
                }
            }
        }

        // Check for summary.json (written on clean exit)
        if status != "completed" && dir.join("summary.json").exists() {
            status = "completed".to_string();
        }

        // Recording / annotation / clip stats from disk
        let mut recording_count: u64 = 0;
        let mut recording_bytes: u64 = 0;
        let mut annotation_count: u64 = 0;
        let mut clip_count: u64 = 0;
        let mut frames_bytes: u64 = 0;
        let mut turns_bytes: u64 = 0;
        let mut logs_bytes: u64 = 0;

        let recordings_dir = dir.join("recordings");
        if recordings_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&recordings_dir) {
                for re in rd.flatten() {
                    if re.path().is_dir() {
                        recording_count += 1;
                        if let Ok(files) = std::fs::read_dir(re.path()) {
                            for f in files.flatten() {
                                let name = f.file_name().to_string_lossy().to_string();
                                if name.starts_with("seg_") {
                                    if let Ok(m) = f.metadata() {
                                        recording_bytes += m.len();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let frames_dir = dir.join("frames");
        if frames_dir.is_dir() {
            if let Ok(fd) = std::fs::read_dir(&frames_dir) {
                let mut clip_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
                for fe in fd.flatten() {
                    let name = fe.file_name().to_string_lossy().to_string();
                    if name.starts_with("ann-") && name.ends_with(".jpg") {
                        annotation_count += 1;
                    } else if name.starts_with("clip-") && name.ends_with(".jpg") {
                        if let Some(pos) = name.rfind("-f") {
                            clip_ids.insert(name[..pos].to_string());
                        }
                    }
                    if let Ok(m) = fe.metadata() {
                        if m.is_file() {
                            frames_bytes += m.len();
                        }
                    }
                }
                clip_count = clip_ids.len() as u64;
            }
        }

        // Turns directory size
        let turns_dir = dir.join("turns");
        if turns_dir.is_dir() {
            if let Ok(td) = std::fs::read_dir(&turns_dir) {
                for te in td.flatten() {
                    if let Ok(m) = te.metadata() {
                        if m.is_file() {
                            turns_bytes += m.len();
                        }
                    }
                }
            }
        }

        // Root-level log files size
        for name in &["session.jsonl", "session_meta.json", "summary.json", "conversation.jsonl"] {
            if let Ok(m) = std::fs::metadata(dir.join(name)) {
                if m.is_file() {
                    logs_bytes += m.len();
                }
            }
        }

        let total_bytes = recording_bytes + frames_bytes + turns_bytes + logs_bytes;

        // Refine status for sessions that never did model work:
        // - "idle": had some activity (recordings, display, task) but no model turns
        // - "abandoned": no turns, no task, no media — MCP probes, brief connections
        // Also override "interrupted" → "idle" when no model work happened
        // (process was killed before any model interaction — nothing was interrupted)
        if status != "completed" {
            let has_model_work = turns > 0 || total_tokens > 0;
            if !has_model_work {
                let has_media = recording_count > 0 || annotation_count > 0 || clip_count > 0;
                if task.is_some() || has_media {
                    status = "idle".to_string();
                } else {
                    status = "abandoned".to_string();
                }
            }
        }

        // Fall back to directory mtime for created_at
        if created_at.is_none() {
            if let Ok(metadata) = std::fs::metadata(&dir) {
                if let Ok(modified) = metadata.modified() {
                    let dt: chrono::DateTime<chrono::Local> = modified.into();
                    created_at = Some(dt.format("%Y-%m-%d %H:%M:%S").to_string());
                }
            }
        }

        // Estimate cost using the model's pricing (blended rate without cache info)
        let estimated_cost = model.as_deref()
            .and_then(|m| crate::app_state_pricing::estimate_session_cost(m, prompt_tokens, completion_tokens))
            .unwrap_or(0.0);

        sessions.push(serde_json::json!({
            "session_id": session_id,
            "created_at": created_at.unwrap_or_default(),
            "task": task,
            "provider": provider,
            "model": model,
            "turns": turns,
            "status": status,
            "total_tokens": total_tokens,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "cached_tokens": cached_tokens,
            "estimated_cost": estimated_cost,
            "role": role,
            "recordings": recording_count,
            "recording_bytes": recording_bytes,
            "annotations": annotation_count,
            "clips": clip_count,
            "frames_bytes": frames_bytes,
            "turns_bytes": turns_bytes,
            "logs_bytes": logs_bytes,
            "total_bytes": total_bytes,
        }));
    }

    // Sort newest first by created_at
    sessions.sort_by(|a, b| {
        let a_ts = a.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let b_ts = b.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        b_ts.cmp(a_ts)
    });

    // Cap at 100
    sessions.truncate(100);

    serde_json::to_string(&sessions).unwrap_or_else(|_| "[]".to_string())
}

/// Delete session data: entire session, media, recordings, frames, or turns.
/// Returns a JSON result with `ok` and `bytes_freed`.
fn delete_session_data(session_id: &str, target: &str) -> String {
    // Path traversal protection
    if session_id.contains("..") || session_id.contains('/') || session_id.contains('\\') {
        return serde_json::json!({"ok": false, "error": "invalid session id"}).to_string();
    }

    let dir = match resolve_session_dir(session_id) {
        Some(d) => d,
        None => return serde_json::json!({"ok": false, "error": "session not found"}).to_string(),
    };

    let dir_byte_size = |path: &std::path::Path| -> u64 {
        let mut total = 0u64;
        if path.is_dir() {
            fn walk(dir: &std::path::Path, total: &mut u64) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for e in entries.flatten() {
                        let p = e.path();
                        if p.is_dir() {
                            walk(&p, total);
                        } else if let Ok(m) = p.metadata() {
                            *total += m.len();
                        }
                    }
                }
            }
            walk(path, &mut total);
        }
        total
    };

    match target {
        "session" => {
            let bytes = dir_byte_size(&dir);
            match std::fs::remove_dir_all(&dir) {
                Ok(_) => serde_json::json!({"ok": true, "deleted": "session", "bytes_freed": bytes}).to_string(),
                Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
            }
        }
        "media" => {
            let rec_dir = dir.join("recordings");
            let frames_dir = dir.join("frames");
            let bytes = dir_byte_size(&rec_dir) + dir_byte_size(&frames_dir);
            let mut errors = Vec::new();
            if rec_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&rec_dir) { errors.push(format!("recordings: {}", e)); }
            }
            if frames_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&frames_dir) { errors.push(format!("frames: {}", e)); }
            }
            if errors.is_empty() {
                serde_json::json!({"ok": true, "deleted": "media", "bytes_freed": bytes}).to_string()
            } else {
                serde_json::json!({"ok": false, "error": errors.join("; "), "bytes_freed": bytes}).to_string()
            }
        }
        "recordings" | "frames" | "turns" => {
            let target_dir = dir.join(target);
            let bytes = dir_byte_size(&target_dir);
            if !target_dir.is_dir() {
                serde_json::json!({"ok": true, "deleted": target, "bytes_freed": 0}).to_string()
            } else {
                match std::fs::remove_dir_all(&target_dir) {
                    Ok(_) => serde_json::json!({"ok": true, "deleted": target, "bytes_freed": bytes}).to_string(),
                    Err(e) => serde_json::json!({"ok": false, "error": e.to_string(), "bytes_freed": 0}).to_string(),
                }
            }
        }
        _ => serde_json::json!({"ok": false, "error": "invalid target"}).to_string(),
    }
}

/// Settings payload for GET/POST /api/settings.
/// Flattened view of intendant.toml sections relevant to the web dashboard.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsPayload {
    // Computer Use
    pub cu_provider: Option<String>,
    pub cu_model: Option<String>,
    pub cu_backend: String,
    // Presence
    pub presence_enabled: bool,
    pub presence_provider: Option<String>,
    pub presence_model: Option<String>,
    pub presence_live_provider: Option<String>,
    pub presence_live_model: Option<String>,
    // Transcription
    pub transcription_enabled: bool,
    pub transcription_provider: String,
    pub transcription_model: String,
    pub transcription_endpoint: Option<String>,
    pub transcription_language: Option<String>,
    // Recording
    pub recording_enabled: bool,
    pub recording_framerate: u32,
    pub recording_quality: String,
    // Live Audio
    pub live_audio_enabled: bool,
    pub live_audio_timeout_secs: u64,
    // Env var overrides (read-only, shown in UI)
    #[serde(default)]
    pub env_overrides: std::collections::HashMap<String, String>,
}

fn settings_payload_from_config(
    config: &crate::project::ProjectConfig,
) -> SettingsPayload {
    let mut env_overrides = std::collections::HashMap::new();
    for (key, var) in [
        ("CU_PROVIDER", "CU_PROVIDER"),
        ("CU_MODEL", "CU_MODEL"),
        ("PRESENCE_PROVIDER", "PRESENCE_PROVIDER"),
        ("PRESENCE_MODEL", "PRESENCE_MODEL"),
        ("PROVIDER", "PROVIDER"),
        ("MODEL_NAME", "MODEL_NAME"),
    ] {
        if let Ok(val) = std::env::var(var) {
            env_overrides.insert(key.to_string(), val);
        }
    }
    SettingsPayload {
        cu_provider: config.computer_use.provider.clone(),
        cu_model: config.computer_use.model.clone(),
        cu_backend: config.computer_use.backend.clone(),
        presence_enabled: config.presence.enabled,
        presence_provider: config.presence.provider.clone(),
        presence_model: config.presence.model.clone(),
        presence_live_provider: config.presence.live_provider.clone(),
        presence_live_model: config.presence.live_model.clone(),
        transcription_enabled: config.transcription.enabled,
        transcription_provider: config.transcription.provider.clone(),
        transcription_model: config.transcription.model.clone(),
        transcription_endpoint: config.transcription.endpoint.clone(),
        transcription_language: config.transcription.language.clone(),
        recording_enabled: config.recording.enabled,
        recording_framerate: config.recording.framerate,
        recording_quality: config.recording.quality.clone(),
        live_audio_enabled: config.live_audio.enabled,
        live_audio_timeout_secs: config.live_audio.default_timeout_secs,
        env_overrides,
    }
}

fn apply_settings_payload(
    config: &mut crate::project::ProjectConfig,
    payload: &SettingsPayload,
) {
    config.computer_use.provider = payload.cu_provider.clone();
    config.computer_use.model = payload.cu_model.clone();
    config.computer_use.backend = payload.cu_backend.clone();
    config.presence.enabled = payload.presence_enabled;
    config.presence.provider = payload.presence_provider.clone();
    config.presence.model = payload.presence_model.clone();
    config.presence.live_provider = payload.presence_live_provider.clone();
    config.presence.live_model = payload.presence_live_model.clone();
    config.transcription.enabled = payload.transcription_enabled;
    config.transcription.provider = payload.transcription_provider.clone();
    config.transcription.model = payload.transcription_model.clone();
    config.transcription.endpoint = payload.transcription_endpoint.clone();
    config.transcription.language = payload.transcription_language.clone();
    config.recording.enabled = payload.recording_enabled;
    config.recording.framerate = payload.recording_framerate;
    config.recording.quality = payload.recording_quality.clone();
    config.live_audio.enabled = payload.live_audio_enabled;
    config.live_audio.default_timeout_secs = payload.live_audio_timeout_secs;
}

/// Return JSON with boolean flags indicating which API keys are configured.
fn get_api_key_status_json() -> String {
    let openai = std::env::var("OPENAI_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let anthropic = std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let gemini = std::env::var("GEMINI_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    serde_json::json!({
        "openai": openai,
        "anthropic": anthropic,
        "gemini": gemini,
    })
    .to_string()
}

/// Payload for POST /api/api-keys.
#[derive(serde::Deserialize)]
struct SetApiKeysPayload {
    keys: std::collections::HashMap<String, String>,
}

/// Handle POST /api/api-keys: persist keys to ~/.config/intendant/.env and
/// set them in the current process.
fn handle_set_api_keys(body: &str) -> String {
    let payload: SetApiKeysPayload = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({"error": format!("Invalid payload: {}", e)}).to_string();
        }
    };

    // Only allow known key names.
    const ALLOWED: &[&str] = &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];
    for key in payload.keys.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return serde_json::json!({"error": format!("Unknown key: {}", key)}).to_string();
        }
    }

    // Resolve config dir.
    let config_dir = match dirs::config_dir() {
        Some(d) => d.join("intendant"),
        None => {
            return serde_json::json!({"error": "Cannot determine config directory"}).to_string();
        }
    };

    // Ensure the directory exists.
    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        return serde_json::json!({"error": format!("Cannot create config dir: {}", e)})
            .to_string();
    }

    let env_path = config_dir.join(".env");

    // Read existing content (may not exist yet).
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();

    // Build updated content: replace existing lines, append new ones.
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut written_keys = std::collections::HashSet::new();

    for line in &mut lines {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let var_name = trimmed[..eq_pos].trim().to_string();
            if let Some(new_val) = payload.keys.get(&var_name) {
                *line = format!("{}={}", var_name, new_val);
                written_keys.insert(var_name);
            }
        }
    }

    // Append keys that weren't already in the file.
    for (key, val) in &payload.keys {
        if !written_keys.contains(key.as_str()) {
            lines.push(format!("{}={}", key, val));
        }
    }

    let new_content = lines.join("\n") + "\n";

    // Atomic write: temp file + rename.
    let tmp_path = config_dir.join(".env.tmp");
    if let Err(e) = std::fs::write(&tmp_path, &new_content) {
        return serde_json::json!({"error": format!("Write failed: {}", e)}).to_string();
    }
    if let Err(e) = std::fs::rename(&tmp_path, &env_path) {
        return serde_json::json!({"error": format!("Rename failed: {}", e)}).to_string();
    }

    // Set env vars in the current process so future provider instantiations
    // pick them up without requiring a restart.
    for (key, val) in &payload.keys {
        std::env::set_var(key, val);
    }

    serde_json::json!({"ok": true}).to_string()
}

// ---------------------------------------------------------------------------
// MCP-over-HTTP (Streamable HTTP) types
// ---------------------------------------------------------------------------
//
// rmcp's Streamable HTTP transport expects:
//   - Requests (with `id`):   200 OK + application/json body
//   - Notifications (no `id`): 202 Accepted + empty body
//
// Returning 200+JSON for notifications causes rmcp to try deserializing the
// body as ServerJsonRpcMessage, which fails because there's no valid `id`.

#[derive(Deserialize)]
struct McpHttpRequest {
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct McpHttpResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpHttpError>,
}

#[derive(Serialize)]
struct McpHttpError {
    code: i64,
    message: String,
}

/// Result from handling an MCP-over-HTTP request.
enum McpHttpOutcome {
    /// JSON-RPC response (requests with `id`) -- return 200 OK + JSON body.
    Response(McpHttpResponse),
    /// Notification acknowledged -- return 202 Accepted with empty body.
    Accepted,
}

async fn handle_mcp_http_request(
    body: &str,
    server: &crate::mcp::IntendantServer,
) -> McpHttpOutcome {
    let request: McpHttpRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return McpHttpOutcome::Response(McpHttpResponse {
                jsonrpc: "2.0".into(),
                id: None,
                result: None,
                error: Some(McpHttpError {
                    code: -32700,
                    message: format!("Parse error: {}", e),
                }),
            });
        }
    };

    // JSON-RPC notifications have no `id` and expect no response body.
    // The MCP Streamable HTTP spec requires 202 Accepted for these.
    let is_notification = request.id.is_none();

    let result = match request.method.as_str() {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "intendant",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "notifications/initialized" | "notifications/cancelled" | "notifications/progress"
        | "notifications/roots/list_changed" => {
            // All notification methods: acknowledge and return 202.
            return McpHttpOutcome::Accepted;
        }
        "tools/list" => Ok(server.list_tools_json()),
        "tools/call" => {
            let params = request.params.unwrap_or_default();
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            match server.call_tool_by_name(name, args).await {
                Ok(result_text) => {
                    // Split screenshot data URIs into MCP image content blocks
                    let mut content = Vec::new();
                    let mut text_lines = Vec::new();
                    for line in result_text.lines() {
                        if let Some(data_uri) = line.strip_prefix("screenshot: data:image/png;base64,") {
                            // Flush accumulated text
                            if !text_lines.is_empty() {
                                content.push(serde_json::json!({"type": "text", "text": text_lines.join("\n")}));
                                text_lines.clear();
                            }
                            content.push(serde_json::json!({"type": "image", "data": data_uri, "mimeType": "image/png"}));
                        } else if let Some(data_uri) = line.strip_prefix("data:image/png;base64,") {
                            if !text_lines.is_empty() {
                                content.push(serde_json::json!({"type": "text", "text": text_lines.join("\n")}));
                                text_lines.clear();
                            }
                            content.push(serde_json::json!({"type": "image", "data": data_uri, "mimeType": "image/png"}));
                        } else {
                            text_lines.push(line.to_string());
                        }
                    }
                    if !text_lines.is_empty() {
                        content.push(serde_json::json!({"type": "text", "text": text_lines.join("\n")}));
                    }
                    if content.is_empty() {
                        content.push(serde_json::json!({"type": "text", "text": result_text}));
                    }
                    Ok(serde_json::json!({ "content": content }))
                }
                Err(e) => Err(McpHttpError {
                    code: -32603,
                    message: e,
                }),
            }
        }
        other => {
            // Unknown notification (no id): accept silently per spec.
            if is_notification {
                return McpHttpOutcome::Accepted;
            }
            Err(McpHttpError {
                code: -32601,
                message: format!("Method not found: {}", other),
            })
        }
    };

    McpHttpOutcome::Response(McpHttpResponse {
        jsonrpc: "2.0".into(),
        id: request.id,
        result: result.as_ref().ok().cloned(),
        error: result.err(),
    })
}

pub fn spawn_web_gateway(
    listener: TcpListener,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    shared_session: SharedActiveSession,
    transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
    task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,
    project_root: Option<std::path::PathBuf>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());

    // Pre-build ICE config for WebRTC display sessions from the gateway config.
    let ice_config = crate::display::IceConfig {
        ice_servers: config.ice_servers.clone(),
    };

    // Inject content-hash version into WASM/JS URLs for cache-busting.
    let v = asset_version_hash();
    let session_provider = config.provider.clone();
    let session_model = config.model.clone();
    let voice_debug = Arc::new(Mutex::new(VoiceDebugState::default()));
    let active_presence: Arc<Mutex<Option<ActivePresence>>> = Arc::new(Mutex::new(None));

    // Cache the latest usage_update JSON so late-connecting browsers get it
    // without sending ControlMsg (which would pollute the event log).
    let last_usage_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest live_usage_update JSON for late-connecting browsers.
    let last_live_usage_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest status event (has autonomy, session_id, task).
    let last_status_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache display_ready JSON per display_id for late-connecting browsers.
    // Using a HashMap so multiple concurrent display sessions are all replayed.
    let display_ready_cache: Arc<Mutex<HashMap<u32, String>>> = Arc::new(Mutex::new(HashMap::new()));
    {
        let usage_cache = last_usage_json.clone();
        let live_usage_cache = last_live_usage_json.clone();
        let status_cache = last_status_json.clone();
        let display_cache = display_ready_cache.clone();
        let mut usage_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match usage_rx.recv().await {
                    Ok(line) => {
                        // Cache display_ready events per display_id for
                        // late-connecting browsers.
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.insert(did, line.clone());
                                }
                            }
                        }
                        // Evict display_ready cache when display is revoked.
                        if line.contains("\"event\":\"user_display_revoked\"")
                            || line.contains("\"event\":\"display_capture_lost\"")
                        {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.remove(&did);
                                }
                            }
                        }
                        if line.contains("\"event\":\"usage_update\"")
                            || line.contains("\"event\":\"usage\"")
                        {
                            if let Ok(mut guard) = usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"live_usage_update\"") {
                            if let Ok(mut guard) = live_usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"status\"") {
                            if let Ok(mut guard) = status_cache.lock() {
                                *guard = Some(line);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let app_html = Arc::new(
        APP_HTML
            .replace(
                "/wasm-web/presence_web.js",
                &format!("/wasm-web/presence_web.js?v={}", v),
            )
            .replace(
                "/wasm-web/presence_web_bg.wasm",
                &format!("/wasm-web/presence_web_bg.wasm?v={}", v),
            ),
    );

    tokio::spawn(async move {
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);

        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();
            let ice_config = ice_config.clone();
            let shared_session = shared_session.clone();
            let voice_debug = voice_debug.clone();
            let session_provider = session_provider.clone();
            let session_model = session_model.clone();
            let app_html = app_html.clone();
            let transcriber = transcriber.clone();
            let active_presence = active_presence.clone();
            let last_usage_json = last_usage_json.clone();
            let last_live_usage_json = last_live_usage_json.clone();
            let last_status_json = last_status_json.clone();
            let display_ready_cache = display_ready_cache.clone();
            let web_tui_tx = web_tui_tx.clone();
            let task_tx = task_tx.clone();
            let project_root = project_root.clone();
            let mcp_server = mcp_server.clone();

            tokio::spawn(async move {
                // Snapshot session state at connection time
                let session_snap = shared_session.read().await;
                let query_ctx = session_snap.query_ctx.clone();
                let frame_registry = session_snap.frame_registry.clone();
                let session_log = session_snap.session_log.clone();
                let recording_registry = session_snap.recording_registry.clone();
                let session_registry = session_snap.session_registry.clone();
                drop(session_snap);
                // Peek at the first bytes to detect WebSocket upgrade.
                // peek() does not consume the data, so tokio_tungstenite
                // can still read the full handshake.
                let mut buf = [0u8; 2048];
                let mut stream = stream;
                let n = match stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let header_text = String::from_utf8_lossy(&buf[..n]);
                let is_websocket = header_text
                    .lines()
                    .any(|l| l.to_lowercase().contains("upgrade: websocket"));

                if is_websocket {
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (mut ws_tx, mut ws_rx) = ws_stream.split();
                    let mut outbound_rx = broadcast_tx.subscribe();

                    // Per-connection identity for active/passive tracking
                    let connection_id = uuid::Uuid::new_v4().to_string();

                    // Direct response channel: tool_response and state_snapshot
                    // messages for this specific connection (not broadcast).
                    let (direct_tx, mut direct_rx) =
                        mpsc::unbounded_channel::<String>();

                    // Register connection with WebTui for per-connection rendering
                    if let Some(ref tx) = web_tui_tx {
                        let _ = tx.send(crate::tui::web::WebTuiCommand::AddConnection {
                            id: connection_id.clone(),
                            direct_tx: direct_tx.clone(),
                            cols: 120,
                            rows: 40,
                        });
                    }

                    // Send bootstrap state snapshot on connect (with connection_id).
                    // Include config (provider/model) since AgentStateSnapshot
                    // doesn't carry those.
                    if let Some(ref ctx) = query_ctx {
                        let state = ctx.agent_state.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let config: serde_json::Value = serde_json::from_str(&config_json)
                            .unwrap_or_default();
                        // Extract session_id from log_dir path name
                        let session_id = ctx.log_dir.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("");
                        let bootstrap = serde_json::json!({
                            "t": "state_snapshot",
                            "state": state,
                            "connection_id": connection_id,
                            "config": config,
                            "session_id": session_id,
                        });
                        let _ = direct_tx.send(bootstrap.to_string());
                    }

                    // Send cached usage data so late-connecting browsers
                    // populate the Usage tab without sending ControlMsg.
                    if let Ok(guard) = last_usage_json.lock() {
                        if let Some(ref usage_json) = *guard {
                            let _ = direct_tx.send(usage_json.clone());
                        }
                    }

                    // Send cached live usage data.
                    if let Ok(guard) = last_live_usage_json.lock() {
                        if let Some(ref live_json) = *guard {
                            let _ = direct_tx.send(live_json.clone());
                        }
                    }

                    // Send cached status (autonomy, session_id, task).
                    if let Ok(guard) = last_status_json.lock() {
                        if let Some(ref status_json) = *guard {
                            let _ = direct_tx.send(status_json.clone());
                        }
                    }

                    // Replay display_ready for every active display session so
                    // late-connecting browsers (including refreshes) recreate
                    // their DisplaySlots and initiate WebRTC.  Prefer the
                    // live session registry over the broadcast cache — it is
                    // authoritative and handles multiple concurrent displays.
                    if let Some(ref sr) = session_registry {
                        let reg = sr.read().await;
                        for did in reg.display_ids() {
                            if let Some(session) = reg.get(did) {
                                let (w, h) = session.resolution();
                                let msg = serde_json::json!({
                                    "event": "display_ready",
                                    "display_id": did,
                                    "width": w,
                                    "height": h,
                                });
                                let _ = direct_tx.send(msg.to_string());
                            }
                        }
                    } else {
                        // Fallback: use the broadcast-derived cache when no
                        // session registry is available (shouldn't happen in
                        // practice, but keeps the old behaviour as safety net).
                        if let Ok(guard) = display_ready_cache.lock() {
                            for display_json in guard.values() {
                                let _ = direct_tx.send(display_json.clone());
                            }
                        }
                    }

                    // Replay session log so late-connecting browsers see
                    // historical events (not just real-time from now on).
                    if let Some(ref ctx) = query_ctx {
                        let session_jsonl = ctx.log_dir.join("session.jsonl");
                        if let Ok(contents) = std::fs::read_to_string(&session_jsonl) {
                            let replay = serde_json::json!({
                                "t": "log_replay",
                                "entries": replay_session_log(&contents, &ctx.log_dir),
                            });
                            let _ = direct_tx.send(replay.to_string());
                        }
                    }

                    // Inbound: WebSocket → EventBus
                    // Handles message types:
                    //   {"t":"key", "key":"Enter", ...}  → AppEvent::Key
                    //   {"t":"resize", "cols":N, "rows":N} → AppEvent::Resize
                    //   {"t":"presence_connect",...}     → AppEvent::PresenceConnected
                    //   {"t":"presence_disconnect"}      → AppEvent::PresenceDisconnected
                    //   {"t":"voice_log",...}             → AppEvent::VoiceLog
                    //   {"t":"presence_checkpoint",...}   → AppEvent::PresenceCheckpointReceived
                    //   {"t":"voice_diagnostic",...}      → AppEvent::VoiceDiagnostic
                    //   {"t":"tool_request", "id":"...", "tool":"...", "args":{}} → tool_response
                    //   {"action":"status", ...}         → AppEvent::ControlCommand
                    // Assign a unique peer ID for WebRTC signaling
                    let peer_id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);

                    let bus_inbound = bus.clone();
                    let query_ctx_inbound = query_ctx.clone();
                    let direct_tx_inbound = direct_tx.clone();
                    let voice_debug_inbound = voice_debug.clone();
                    let live_provider = session_provider.clone();
                    let live_model = session_model.clone();
                    let transcriber_inbound = transcriber.clone();
                    let active_presence_inbound = active_presence.clone();
                    let connection_id_inbound = connection_id.clone();
                    let web_tui_tx_inbound = web_tui_tx.clone();
                    let frame_registry_inbound = frame_registry.clone();
                    let recording_registry_inbound = recording_registry.clone();
                    let session_log_inbound = session_log.clone();
                    let session_registry_inbound = session_registry.clone();
                    let task_tx_inbound = task_tx.clone();
                    let inbound = tokio::spawn(async move {
                        // Track whether this connection has an active presence model,
                        // so we can auto-send PresenceDisconnected if the WebSocket drops
                        // without a clean presence_disconnect message (e.g. tab close
                        // before beforeunload fires, network failure).
                        let mut is_presence_connected = false;
                        // Whether this connection is the active voice owner
                        let mut is_active = false;

                        // Per-connection clip accumulators for batched clip_frame messages
                        struct ClipAccumulator {
                            stream: String,
                            note: String,
                            inject: bool,
                            in_secs: f64,
                            out_secs: f64,
                            fps: u32,
                            expected: usize,
                            frames: Vec<(String, String)>, // (frame_id, base64_data)
                        }
                        let mut clip_accumulators: std::collections::HashMap<String, ClipAccumulator> = std::collections::HashMap::new();

                        // Display IDs this peer has WebRTC connections to,
                        // used for cleanup when the WebSocket disconnects.
                        let mut peer_display_ids: Vec<u32> = Vec::new();

                        // Per-connection audio transcription buffer.
                        // PCM16 bytes are accumulated and drained every ~3s.
                        let mut audio_buf: Vec<u8> = Vec::new();
                        let mut audio_seq: u64 = 0;
                        // Input sample rate (known from config, default 16kHz)
                        let audio_sample_rate: u32 = 16000;

                        while let Some(Ok(msg)) = ws_rx.next().await {
                            if let Message::Text(text) = msg {
                                let trimmed = text.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // Try to parse as JSON for type-tagged messages
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                                    match json.get("t").and_then(|v| v.as_str()) {
                                        Some("key") => {
                                            // Route key events to this connection's
                                            // ViewState via WebTuiCommand (not EventBus).
                                            if let Some(key_event) = crate::tui::web::parse_web_key(&json) {
                                                if let Some(ref tx) = web_tui_tx {
                                                    let _ = tx.send(crate::tui::web::WebTuiCommand::Key {
                                                        id: connection_id_inbound.clone(),
                                                        key: key_event,
                                                    });
                                                } else if is_active {
                                                    // Fallback: no WebTui (headless web mode)
                                                    bus_inbound.send(AppEvent::Key(key_event));
                                                }
                                            }
                                        }
                                        Some("resize") => {
                                            // Route resize to this connection's terminal
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(crate::tui::web::WebTuiCommand::Resize {
                                                    id: connection_id_inbound.clone(),
                                                    cols,
                                                    rows,
                                                });
                                            } else if is_active {
                                                bus_inbound.send(AppEvent::Resize(cols, rows));
                                            }
                                        }
                                        Some("presence_connect") => {
                                            is_presence_connected = true;
                                            voice_debug_inbound.lock().unwrap_or_else(|e| e.into_inner()).connected = true;
                                            let server_session_id = json.get("server_session_id")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            let last_event_seq = json.get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);
                                            // Use provider/model from the browser if sent,
                                            // fall back to config defaults.
                                            let msg_provider = json.get("provider")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_provider.clone());
                                            let msg_model = json.get("model")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_model.clone());

                                            // Determine if this connection becomes active or passive.
                                            // Browsers can request always-passive mode (observer/follow-along).
                                            let force_passive = json.get("passive")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(false);
                                            let becomes_active = if force_passive {
                                                false
                                            } else {
                                                let slot = active_presence_inbound.lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                // Empty slot → first connect wins.
                                                // Slot occupied by THIS connection → already active
                                                // (happens when active browser reconnects voice after handover).
                                                slot.is_none()
                                                    || slot.as_ref()
                                                        .map(|a| a.connection_id == connection_id_inbound)
                                                        .unwrap_or(false)
                                            };

                                            let was_already_active = is_active;
                                            if becomes_active {
                                                // First-connect wins (or re-confirm already-active)
                                                *active_presence_inbound.lock()
                                                    .unwrap_or_else(|e| e.into_inner()) = Some(ActivePresence {
                                                    connection_id: connection_id_inbound.clone(),
                                                    direct_tx: direct_tx_inbound.clone(),
                                                });
                                                is_active = true;
                                            }

                                            // Send welcome with replay window if presence session is available
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                // Build conversation context from recent voice transcripts
                                                let conversation_ctx = presence::build_conversation_context(&ctx.log_dir, 20);

                                                if let Some(ref ps) = ctx.presence_session {
                                                    let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
                                                    if becomes_active {
                                                        session.set_connected(true);
                                                    }
                                                    let state = ctx.agent_state.lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone();
                                                    let welcome = session.build_welcome(last_event_seq, &state);
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "session_id": welcome.session_id,
                                                        "state": welcome.state,
                                                        "events": welcome.events,
                                                        "last_checkpoint_summary": welcome.last_checkpoint_summary,
                                                        "current_seq": welcome.current_seq,
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound.send(welcome_msg.to_string());
                                                } else {
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound.send(welcome_msg.to_string());
                                                }
                                            } else {
                                                // No presence session — still send a minimal welcome with is_active
                                                let welcome_msg = serde_json::json!({
                                                    "t": "presence_welcome",
                                                    "is_active": becomes_active,
                                                });
                                                let _ = direct_tx_inbound.send(welcome_msg.to_string());
                                            }

                                            // Only emit PresenceConnected for the active browser
                                            // (passive browsers don't pause server-side presence).
                                            // Skip if already active (e.g. voice reconnect after make_active
                                            // handover — PresenceConnected was already emitted by make_active).
                                            if becomes_active && !was_already_active {
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_connected(Some(&msg_provider), Some(&msg_model));
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceConnected {
                                                    server_session_id,
                                                    last_event_seq,
                                                    live_provider: Some(msg_provider),
                                                    live_model: Some(msg_model),
                                                });
                                            }
                                        }
                                        Some("presence_disconnect") => {
                                            is_presence_connected = false;
                                            voice_debug_inbound.lock().unwrap_or_else(|e| e.into_inner()).connected = false;
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    ps.lock().unwrap_or_else(|e| e.into_inner())
                                                        .set_connected(false);
                                                }
                                            }
                                            // Only emit PresenceDisconnected if this was the active browser
                                            if is_active {
                                                // Clear the active slot
                                                let mut slot = active_presence_inbound.lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                if slot.as_ref().map(|a| a.connection_id == connection_id_inbound).unwrap_or(false) {
                                                    *slot = None;
                                                }
                                                is_active = false;
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_disconnected();
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceDisconnected);
                                            }
                                        }
                                        Some("make_active") => {
                                            // Request to become the active voice owner
                                            let mut slot = active_presence_inbound.lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            let previous_active = slot.as_ref()
                                                .map(|active| active.connection_id.clone());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_received_gateway",
                                                        &format!(
                                                            "request from connection={} previous_active={}",
                                                            connection_id_inbound,
                                                            previous_active.as_deref().unwrap_or("none"),
                                                        ),
                                                    );
                                                }
                                            }

                                            // Tell old active to disconnect voice
                                            if let Some(ref old) = *slot {
                                                if old.connection_id != connection_id_inbound {
                                                    let force_msg = serde_json::json!({
                                                        "t": "force_disconnect_voice",
                                                        "reason": "handover",
                                                    });
                                                    let _ = old.direct_tx.send(force_msg.to_string());
                                                    if let Some(ref sl) = session_log_inbound {
                                                        if let Ok(mut l) = sl.lock() {
                                                            l.voice_diagnostic(
                                                                "make_active_force_disconnect_gateway",
                                                                &format!(
                                                                    "old_active={} new_active={}",
                                                                    old.connection_id, connection_id_inbound,
                                                                ),
                                                            );
                                                        }
                                                    }
                                                } else if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.voice_diagnostic(
                                                            "make_active_noop_gateway",
                                                            &format!(
                                                                "request from already-active connection={}",
                                                                connection_id_inbound,
                                                            ),
                                                        );
                                                    }
                                                }
                                            }

                                            // Install this connection as new active
                                            *slot = Some(ActivePresence {
                                                connection_id: connection_id_inbound.clone(),
                                                direct_tx: direct_tx_inbound.clone(),
                                            });
                                            drop(slot);

                                            is_active = true;
                                            is_presence_connected = true;
                                            voice_debug_inbound.lock().unwrap_or_else(|e| e.into_inner()).connected = true;

                                            // Build handover context from latest checkpoint
                                            let handover_context = query_ctx_inbound.as_ref()
                                                .and_then(|ctx| ctx.presence_session.as_ref())
                                                .and_then(|ps| {
                                                    let session = ps.lock().unwrap_or_else(|e| e.into_inner());
                                                    session.last_checkpoint_summary()
                                                })
                                                .unwrap_or_default();

                                            // Build conversation context from recent voice transcripts
                                            let conversation_ctx = query_ctx_inbound.as_ref()
                                                .and_then(|ctx| presence::build_conversation_context(&ctx.log_dir, 20));
                                            let has_handover_context = !handover_context.is_empty();
                                            let has_conversation_context = conversation_ctx.as_deref()
                                                .map(|s| !s.is_empty())
                                                .unwrap_or(false);

                                            // Send active_granted to this connection
                                            let granted_msg = serde_json::json!({
                                                "t": "active_granted",
                                                "is_active": true,
                                                "handover_context": handover_context,
                                                "conversation_context": conversation_ctx,
                                            });
                                            let _ = direct_tx_inbound.send(granted_msg.to_string());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_granted_gateway",
                                                        &format!(
                                                            "connection={} handover_context={} conversation_context={}",
                                                            connection_id_inbound,
                                                            if has_handover_context { "yes" } else { "no" },
                                                            if has_conversation_context { "yes" } else { "no" },
                                                        ),
                                                    );
                                                }
                                            }

                                            // Emit PresenceConnected for the new active browser
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_connected(Some(&live_provider), Some(&live_model));
                                                }
                                            }
                                            bus_inbound.send(AppEvent::PresenceConnected {
                                                server_session_id: None,
                                                last_event_seq: 0,
                                                live_provider: Some(live_provider.clone()),
                                                live_model: Some(live_model.clone()),
                                            });
                                        }
                                        Some("voice_log") => {
                                            let text = json["text"].as_str().unwrap_or("").to_string();
                                            let seq = json["seq"].as_u64().unwrap_or(0);
                                            let tool_context = json.get("tool_context")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            {
                                                let mut vd = voice_debug_inbound.lock().unwrap_or_else(|e| e.into_inner());
                                                vd.voice_log_count += 1;
                                                vd.last_voice_log = text.clone();
                                            }
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_log(&text, seq, tool_context.as_deref());
                                                }
                                            }
                                            bus_inbound.send(AppEvent::VoiceLog {
                                                text,
                                                seq,
                                                tool_context,
                                            });
                                        }
                                        Some("live_usage_update") => {
                                            bus_inbound.send(AppEvent::LiveUsageUpdate {
                                                provider: json["provider"].as_str().unwrap_or("").to_string(),
                                                model: json["model"].as_str().unwrap_or("").to_string(),
                                                input_tokens: json["input_tokens"].as_u64().unwrap_or(0),
                                                output_tokens: json["output_tokens"].as_u64().unwrap_or(0),
                                                cached_tokens: json["cached_tokens"].as_u64().unwrap_or(0),
                                                total_tokens: json["total_tokens"].as_u64().unwrap_or(0),
                                                thinking_tokens: json["thinking_tokens"].as_u64().unwrap_or(0),
                                            });
                                        }
                                        Some("presence_checkpoint") => {
                                            let summary = json["summary"].as_str().unwrap_or("").to_string();
                                            let last_event_seq = json.get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);

                                            // Record checkpoint and send ack
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    let checkpoint = presence_core::PresenceCheckpoint {
                                                        summary: summary.clone(),
                                                        last_event_seq,
                                                    };
                                                    let ack = ps.lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .record_checkpoint(checkpoint);
                                                    let ack_msg = serde_json::json!({
                                                        "t": "presence_checkpoint_ack",
                                                        "seq": ack.seq,
                                                    });
                                                    let _ = direct_tx_inbound.send(ack_msg.to_string());
                                                }
                                            }

                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_checkpoint(&summary, last_event_seq);
                                                }
                                            }
                                            bus_inbound.send(AppEvent::PresenceCheckpointReceived {
                                                summary,
                                                last_event_seq,
                                            });
                                        }
                                        Some("voice_diagnostic") => {
                                            let kind = json["kind"].as_str().unwrap_or("unknown").to_string();
                                            let detail = json["detail"].as_str().unwrap_or("").to_string();
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(&kind, &detail);
                                                }
                                            }
                                            bus_inbound.send(AppEvent::VoiceDiagnostic {
                                                kind,
                                                detail,
                                            });
                                        }
                                        Some("user_audio") => {
                                            // Browser sends base64-encoded PCM16 audio for server-side transcription.
                                            if let Some(ref transcriber) = transcriber_inbound {
                                                if let Some(data_b64) = json["data"].as_str() {
                                                    use base64::Engine;
                                                    if let Ok(pcm_bytes) = base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                    {
                                                        audio_buf.extend_from_slice(&pcm_bytes);
                                                        // Drain at ~3s of audio (16kHz * 2 bytes/sample * 1 channel * 3s = 96000)
                                                        let threshold = (audio_sample_rate as usize) * 2 * 3;
                                                        if audio_buf.len() >= threshold {
                                                            // Skip silent buffers — compute RMS energy of PCM16 samples.
                                                            // Whisper hallucinates on silence (outputs "you", ".", etc).
                                                            let rms = {
                                                                let samples = audio_buf.chunks_exact(2)
                                                                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64);
                                                                let sum_sq: f64 = samples.map(|s| s * s).sum();
                                                                let n = audio_buf.len() / 2;
                                                                if n > 0 { (sum_sq / n as f64).sqrt() } else { 0.0 }
                                                            };
                                                            if rms < 1000.0 {
                                                                // Below speech threshold — skip transcription.
                                                                // Whisper hallucinates aggressively on low-energy
                                                                // audio ("Thank you", "Bye bye", etc).
                                                                audio_buf.clear();
                                                                continue;
                                                            }
                                                            let wav = crate::transcription::encode_wav(
                                                                &audio_buf,
                                                                audio_sample_rate,
                                                                1,
                                                            );
                                                            audio_buf.clear();
                                                            audio_seq += 1;
                                                            let seq = audio_seq;
                                                            let t = transcriber.clone();
                                                            let bus_tx = bus_inbound.clone();
                                                            let session_log_tx = session_log_inbound.clone();
                                                            tokio::spawn(async move {
                                                                match t.transcribe(&wav).await {
                                                                    Ok(segment) => {
                                                                        let text = segment.text.trim().to_string();
                                                                        if !text.is_empty() {
                                                                            if let Some(ref sl) = session_log_tx {
                                                                                if let Ok(mut l) = sl.lock() {
                                                                                    l.user_transcript(&text, seq);
                                                                                }
                                                                            }
                                                                            bus_tx.send(AppEvent::UserTranscript { text, seq });
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        eprintln!("transcription failed: {}", e);
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("video_frame") => {
                                            // Browser sends a video frame for HQ archival in the frame registry.
                                            let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"].as_str().unwrap_or("cam0").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                                    // Register in frame registry
                                                    if let Some(ref registry) = frame_registry_inbound {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now().to_rfc3339(),
                                                            sent_to_live: true,
                                                            live_resolution: Some("768x768".to_string()),
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) = reg.register(meta, &jpeg_bytes) {
                                                            eprintln!("frame registry write failed: {}", e);
                                                        }
                                                    }
                                                    // Feed into recording pipeline (auto-starts on first frame)
                                                    if let Some(ref rec_reg) = recording_registry_inbound {
                                                        let mut rreg = rec_reg.write().await;
                                                        if rreg.is_enabled() {
                                                            if !rreg.is_recording(&stream) {
                                                                if crate::recording::is_ffmpeg_available() {
                                                                    if let Err(e) = rreg.start_stream(&stream).await {
                                                                        eprintln!("camera recording start failed: {}", e);
                                                                    } else {
                                                                        bus_inbound.send(AppEvent::RecordingStarted {
                                                                            stream_name: stream.clone(),
                                                                        });
                                                                    }
                                                                }
                                                            }
                                                            let _ = rreg.feed_frame(&stream, &jpeg_bytes).await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("annotation_submit") => {
                                            // User drew annotations on a frame and submitted it with a note.
                                            let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"].as_str().unwrap_or("annotation").to_string();
                                            let note = json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                                    // Register in frame registry
                                                    let mut saved_path = String::new();
                                                    if let Some(ref registry) = frame_registry_inbound {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now().to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() { None } else { Some(note.clone()) },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => saved_path = path.display().to_string(),
                                                            Err(e) => eprintln!("annotation frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    // Optionally inject into agent conversation
                                                    if inject {
                                                        if let Some(ref ctx) = query_ctx_inbound {
                                                            if let Some(ref ciq) = ctx.context_injection {
                                                                if let Ok(mut q) = ciq.lock() {
                                                                    let label = if note.is_empty() {
                                                                        "[User Annotation] User highlighted something on the screen.".to_string()
                                                                    } else {
                                                                        format!("[User Annotation] {}", note)
                                                                    };
                                                                    q.push(crate::event::ContextInjection {
                                                                        text: label,
                                                                        images: vec![crate::conversation::ImageData {
                                                                            media_type: "image/jpeg".to_string(),
                                                                            data: data_b64.to_string(),
                                                                        }],
                                                                    });
                                                                }
                                                            }
                                                        }
                                                    }
                                                    // Send path back to browser
                                                    let _ = direct_tx_inbound.send(serde_json::json!({
                                                        "t": "annotation_saved",
                                                        "frame_id": frame_id,
                                                        "path": saved_path,
                                                        "injected": inject,
                                                    }).to_string());
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} on {}{}",
                                                            frame_id, stream,
                                                            if inject { " (sent to agent)" } else { "" }
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("clip_start") => {
                                            let clip_id = json["clip_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"].as_str().unwrap_or("recording").to_string();
                                            let note = json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            let in_secs = json["in_secs"].as_f64().unwrap_or(0.0);
                                            let out_secs = json["out_secs"].as_f64().unwrap_or(0.0);
                                            let fps = json["fps"].as_u64().unwrap_or(2) as u32;
                                            let total = json["total_frames"].as_u64().unwrap_or(0) as usize;
                                            clip_accumulators.insert(clip_id.clone(), ClipAccumulator {
                                                stream, note, inject, in_secs, out_secs, fps,
                                                expected: total,
                                                frames: Vec::with_capacity(total),
                                            });
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[clip] started {} ({} frames, {}fps)", clip_id, total, fps),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });
                                        }
                                        Some("clip_frame") => {
                                            let clip_id = json["clip_id"].as_str().unwrap_or("").to_string();
                                            let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                                            let timestamp_secs = json["timestamp_secs"].as_f64().unwrap_or(0.0);
                                            if let Some(data_b64) = json["data"].as_str() {
                                                // Register frame in frame registry
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                                    if let Some(ref registry) = frame_registry_inbound {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: format!("clip:{}", clip_id),
                                                            timestamp: chrono::Utc::now().to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) = reg.register(meta, &jpeg_bytes) {
                                                            eprintln!("clip frame registry write failed: {}", e);
                                                        }
                                                    }
                                                }
                                                // Accumulate for context injection
                                                if let Some(acc) = clip_accumulators.get_mut(&clip_id) {
                                                    acc.frames.push((frame_id, data_b64.to_string()));
                                                }
                                            }
                                        }
                                        Some("clip_end") => {
                                            let clip_id = json["clip_id"].as_str().unwrap_or("").to_string();
                                            let frames_sent = json["frames_sent"].as_u64().unwrap_or(0) as usize;
                                            let mut injected = false;

                                            if let Some(acc) = clip_accumulators.remove(&clip_id) {
                                                let frames_registered = acc.frames.len();
                                                if acc.inject {
                                                    if let Some(ref ctx) = query_ctx_inbound {
                                                        if let Some(ref ciq) = ctx.context_injection {
                                                            if let Ok(mut q) = ciq.lock() {
                                                                let label = if acc.note.is_empty() {
                                                                    format!(
                                                                        "[Video Clip] {} {}-{} ({} frames, {}fps)",
                                                                        acc.stream,
                                                                        format!("{:.1}s", acc.in_secs),
                                                                        format!("{:.1}s", acc.out_secs),
                                                                        frames_registered, acc.fps,
                                                                    )
                                                                } else {
                                                                    format!(
                                                                        "[Video Clip] {} {}-{} ({} frames, {}fps). {}",
                                                                        acc.stream,
                                                                        format!("{:.1}s", acc.in_secs),
                                                                        format!("{:.1}s", acc.out_secs),
                                                                        frames_registered, acc.fps, acc.note,
                                                                    )
                                                                };
                                                                let images: Vec<crate::conversation::ImageData> = acc.frames.iter().map(|(_, b64)| {
                                                                    crate::conversation::ImageData {
                                                                        media_type: "image/jpeg".to_string(),
                                                                        data: b64.clone(),
                                                                    }
                                                                }).collect();
                                                                q.push(crate::event::ContextInjection {
                                                                    text: label,
                                                                    images,
                                                                });
                                                                injected = true;
                                                            }
                                                        }
                                                    }
                                                }

                                                let _ = direct_tx_inbound.send(serde_json::json!({
                                                    "t": "clip_saved",
                                                    "clip_id": clip_id,
                                                    "frames_registered": frames_registered,
                                                    "injected": injected,
                                                }).to_string());

                                                bus_inbound.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "[clip] {} — {} frames{}",
                                                        clip_id, frames_registered,
                                                        if injected { " (sent to agent)" } else { " (saved)" }
                                                    ),
                                                    level: Some(LogLevel::Info),
                                                    turn: None,
                                                });
                                            }
                                        }
                                        Some("tool_request") => {
                                            let req_id = json["id"].as_str().unwrap_or("").to_string();
                                            let tool = json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned()
                                                .unwrap_or(serde_json::Value::Object(Default::default()));

                                            // Log the incoming tool request at Debug level
                                            let args_preview = {
                                                let s = serde_json::to_string(&args).unwrap_or_default();
                                                if s.len() > 200 { format!("{}...", &s[..200]) } else { s }
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[tool_request] {}({})", tool, args_preview),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            // Dispatch through presence-core (single canonical layer)
                                            let state = query_ctx_inbound.as_ref()
                                                .map(|ctx| ctx.agent_state.lock().unwrap_or_else(|e| e.into_inner()).clone())
                                                .unwrap_or_default();
                                            let action = presence::dispatch_tool_call(&tool, &args, &state);

                                            // SubmitTask: send directly to task_tx (bypasses TUI)
                                            let query_result = if let presence::PresenceAction::SubmitTask(envelope) = action {
                                                let msg = format!("Task submitted: {}", envelope.task);
                                                if let Some(ref tx) = task_tx_inbound {
                                                    let _ = tx.send(envelope).await;
                                                } else {
                                                    // Fallback: dispatch via EventBus if no task_tx
                                                    let ctrl_action = presence::PresenceAction::SubmitTask(envelope);
                                                    if let Some((ctrl, _)) = presence::action_to_control_msg(&ctrl_action) {
                                                        bus_inbound.send(AppEvent::ControlCommand(ctrl));
                                                    }
                                                }
                                                presence::ToolQueryResult::text(msg)
                                            } else if let Some((ctrl, msg)) = presence::action_to_control_msg(&action) {
                                                // Other action tools: dispatch via EventBus
                                                bus_inbound.send(AppEvent::ControlCommand(ctrl));
                                                presence::ToolQueryResult::text(msg)
                                            } else {
                                                match action {
                                                    presence::PresenceAction::TextResult(text) => {
                                                        presence::ToolQueryResult::text(text)
                                                    }
                                                    presence::PresenceAction::NeedsIO { tool_name, args: io_args } => {
                                                        if let Some(ref ctx) = query_ctx_inbound {
                                                            if let Some(result) = presence::handle_tool_query(
                                                                &ctx.agent_state,
                                                                &ctx.project_root,
                                                                &ctx.log_dir,
                                                                &ctx.knowledge_path,
                                                                &tool_name,
                                                                &io_args,
                                                                frame_registry_inbound.as_ref(),
                                                                ctx.context_injection.as_ref(),
                                                            ).await {
                                                                result
                                                            } else {
                                                                presence::ToolQueryResult::text(format!("Unknown tool: {}", tool))
                                                            }
                                                        } else {
                                                            presence::ToolQueryResult::text("Presence query context not available".to_string())
                                                        }
                                                    }
                                                    _ => unreachable!(),
                                                }
                                            };

                                            // Log the tool response at Debug level
                                            let result_preview = if query_result.text.len() > 200 {
                                                format!("{}...", &query_result.text[..200])
                                            } else {
                                                query_result.text.clone()
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[tool_response] {} → {}", tool, result_preview),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "tool_response",
                                                "id": req_id,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> = query_result.images.iter().map(|img| {
                                                    serde_json::json!({
                                                        "mime_type": img.media_type,
                                                        "data": img.data,
                                                    })
                                                }).collect();
                                                response["images"] = serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("async_query") => {
                                            // Async query from browser — same dispatch as tool_request
                                            // but result goes back as async_query_result (injected into
                                            // voice session as text, not as a tool response).
                                            let req_id = json["id"].as_str().unwrap_or("").to_string();
                                            let tool = json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned()
                                                .unwrap_or(serde_json::Value::Object(Default::default()));

                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[async_query] {}", tool),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let query_result = if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(result) = presence::handle_tool_query(
                                                    &ctx.agent_state,
                                                    &ctx.project_root,
                                                    &ctx.log_dir,
                                                    &ctx.knowledge_path,
                                                    &tool,
                                                    &args,
                                                    frame_registry_inbound.as_ref(),
                                                    ctx.context_injection.as_ref(),
                                                ).await {
                                                    result
                                                } else {
                                                    presence::ToolQueryResult::text(format!("Unknown query tool: {}", tool))
                                                }
                                            } else {
                                                presence::ToolQueryResult::text("Presence query context not available".to_string())
                                            };

                                            let result_preview = if query_result.text.len() > 200 {
                                                format!("{}...", &query_result.text[..200])
                                            } else {
                                                query_result.text.clone()
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[async_query_result] {} → {}", tool, result_preview),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "async_query_result",
                                                "id": req_id,
                                                "tool": tool,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> = query_result.images.iter().map(|img| {
                                                    serde_json::json!({
                                                        "mime_type": img.media_type,
                                                        "data": img.data,
                                                    })
                                                }).collect();
                                                response["images"] = serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("display_offer") => {
                                            // WebRTC SDP offer from browser for a display session
                                            let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let sdp = json["sdp"].as_str().unwrap_or("").to_string();

                                            if let Some(ref sr) = session_registry_inbound {
                                                if let Some(session) = sr.read().await.get(display_id) {
                                                    let (ice_tx, mut ice_rx) = mpsc::channel::<(crate::display::PeerId, String)>(64);
                                                    match session.handle_offer(peer_id, &sdp, &ice_config, ice_tx).await {
                                                        Ok(answer_sdp) => {
                                                            peer_display_ids.push(display_id);
                                                            let answer = serde_json::json!({
                                                                "t": "display_answer",
                                                                "display_id": display_id,
                                                                "sdp": answer_sdp,
                                                            });
                                                            let _ = direct_tx_inbound.send(answer.to_string());

                                                            // Forward server ICE candidates to browser
                                                            let ice_direct_tx = direct_tx_inbound.clone();
                                                            tokio::spawn(async move {
                                                                while let Some((_pid, candidate_json)) = ice_rx.recv().await {
                                                                    let msg = serde_json::json!({
                                                                        "t": "display_ice",
                                                                        "display_id": display_id,
                                                                        "candidate": serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default(),
                                                                    });
                                                                    if ice_direct_tx.send(msg.to_string()).is_err() {
                                                                        break;
                                                                    }
                                                                }
                                                            });
                                                        }
                                                        Err(e) => {
                                                            eprintln!("[web_gateway] WebRTC offer failed for display {}: {}", display_id, e);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("display_ice") => {
                                            // Trickle ICE candidate from browser
                                            let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let candidate = json["candidate"].to_string();

                                            if let Some(ref sr) = session_registry_inbound {
                                                if let Some(session) = sr.read().await.get(display_id) {
                                                    if let Err(e) = session.add_ice_candidate(peer_id, &candidate).await {
                                                        eprintln!("[web_gateway] ICE candidate failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            }
                                        }
                                        Some("display_input") => {
                                            // Input event (keyboard/mouse) for a display session
                                            let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;
                                            if let Some(evt) = json.get("event") {
                                                if let Ok(input_event) = serde_json::from_value::<crate::display::InputEvent>(evt.clone()) {
                                                    if let Some(ref sr) = session_registry_inbound {
                                                        if let Some(session) = sr.read().await.get(display_id) {
                                                            if let Err(e) = session.inject_input(input_event).await {
                                                                eprintln!("[web_gateway] display input injection failed: {}", e);
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        _ => {
                                            // Fall through to ControlMsg parsing
                                            match serde_json::from_value::<ControlMsg>(json) {
                                                Ok(ctrl) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!("[ws] ControlMsg: {:?}",
                                                            match &ctrl {
                                                                ControlMsg::StartTask { task, .. } => format!("StartTask({})", &task[..task.len().min(60)]),
                                                                other => format!("{:?}", other),
                                                            }),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                    bus_inbound.send(AppEvent::ControlCommand(ctrl));
                                                }
                                                Err(e) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!("[ws] ControlMsg parse failed: {}", e),
                                                        level: Some(LogLevel::Warn),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // WebSocket closed — clean up active slot and auto-resume
                        // server presence if this was the active browser (covers tab
                        // close without beforeunload, network drops, etc.)
                        if is_active {
                            let mut slot = active_presence_inbound.lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if slot.as_ref().map(|a| a.connection_id == connection_id_inbound).unwrap_or(false) {
                                *slot = None;
                            }
                        }
                        if is_presence_connected && is_active {
                            bus_inbound.send(AppEvent::PresenceDisconnected);
                        }
                        // Remove this peer from display sessions it connected to
                        if !peer_display_ids.is_empty() {
                            if let Some(ref sr) = session_registry_inbound {
                                let reg = sr.read().await;
                                for did in &peer_display_ids {
                                    if let Some(session) = reg.get(*did) {
                                        session.remove_peer(peer_id).await;
                                    }
                                }
                            }
                        }
                        // Unregister from WebTui
                        if let Some(ref tx) = web_tui_tx_inbound {
                            let _ = tx.send(crate::tui::web::WebTuiCommand::RemoveConnection {
                                id: connection_id_inbound.clone(),
                            });
                        }
                    });

                    // Outbound: broadcast + direct responses → WebSocket
                    let outbound = tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                msg = outbound_rx.recv() => {
                                    match msg {
                                        Ok(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break,
                                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                    }
                                }
                                msg = direct_rx.recv() => {
                                    match msg {
                                        Some(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        None => break,
                                    }
                                }
                            }
                        }
                    });

                    let _ = tokio::join!(inbound, outbound);
                } else {
                    // Plain HTTP: consume the peeked request bytes, then send response.
                    let mut discard = vec![0u8; n];
                    use tokio::io::AsyncReadExt;
                    let _ = stream.read_exact(&mut discard).await;

                    // Route by request path
                    let request_line = header_text.lines().next().unwrap_or("");

                    // CORS preflight: respond to OPTIONS with permissive headers.
                    // Needed when the page is served from a custom scheme (intendant://)
                    // in the macOS app bundle — API fetches become cross-origin.
                    if request_line.starts_with("OPTIONS") {
                        use tokio::io::AsyncWriteExt;
                        let response = "HTTP/1.1 204 No Content\r\n\
                            Access-Control-Allow-Origin: *\r\n\
                            Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n\
                            Access-Control-Allow-Headers: Content-Type\r\n\
                            Access-Control-Max-Age: 86400\r\n\
                            Connection: close\r\n\
                            \r\n";
                        let _ = stream.write_all(response.as_bytes()).await;
                        return;
                    }

                    // Route WASM binaries (need async write_all for large payloads)
                    let wasm_binary = if request_line.contains("/wasm-web/presence_web_bg.wasm") {
                        Some(WASM_WEB_BIN)
                    } else {
                        None
                    };

                    if let Some(wasm_data) = wasm_binary {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/wasm\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache, must-revalidate\r\n\
                             Connection: close\r\n\
                             \r\n",
                            wasm_data.len()
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(wasm_data).await;
                    } else if request_line.contains(" /frames/") {
                        // Serve HQ frame images from the frame registry.
                        // URL format: /frames/<frame_id> (not /api/session/*/frames/*)
                        use tokio::io::AsyncWriteExt;
                        let frame_id = request_line
                            .split("/frames/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let data = if let Some(ref reg) = frame_registry {
                            let reg = reg.read().await;
                            reg.read_hq(frame_id).ok()
                        } else {
                            None
                        };
                        if let Some(jpeg_data) = data {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: image/jpeg\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: public, max-age=31536000, immutable\r\n\
                                 Connection: close\r\n\
                                 \r\n",
                                jpeg_data.len()
                            );
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&jpeg_data).await;
                        } else {
                            let body = "Frame not found";
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\n\
                                 Content-Type: text/plain\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(), body
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.starts_with("POST") && request_line.contains("/api/settings") {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        // Read POST body — may be partially or fully outside the peek buffer
                        let content_length: usize = header_text
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                        let body_owned;
                        let body_text = if peeked_body.len() >= content_length {
                            &peeked_body[..content_length]
                        } else {
                            let remaining = content_length.saturating_sub(peeked_body.len());
                            let mut full = peeked_body.to_string();
                            if remaining > 0 {
                                let mut rest = vec![0u8; remaining];
                                if stream.read_exact(&mut rest).await.is_ok() {
                                    full.push_str(&String::from_utf8_lossy(&rest));
                                }
                            }
                            body_owned = full;
                            &body_owned
                        };
                        let result = match &project_root {
                            Some(root) => {
                                match serde_json::from_str::<SettingsPayload>(body_text) {
                                    Ok(payload) => {
                                        match crate::project::Project::from_root(root.clone()) {
                                            Ok(mut proj) => {
                                                apply_settings_payload(&mut proj.config, &payload);
                                                match proj.save_config() {
                                                    Ok(()) => serde_json::json!({"ok": true}).to_string(),
                                                    Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                                }
                                            }
                                            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                        }
                                    }
                                    Err(e) => serde_json::json!({"error": format!("Invalid settings: {}", e)}).to_string(),
                                }
                            }
                            None => serde_json::json!({"error": "No project root"}).to_string(),
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            result.len(), result
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/settings") {
                        use tokio::io::AsyncWriteExt;
                        let body = match &project_root {
                            Some(root) => {
                                match crate::project::Project::from_root(root.clone()) {
                                    Ok(proj) => {
                                        let payload = settings_payload_from_config(&proj.config);
                                        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
                                    }
                                    Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                }
                            }
                            None => serde_json::json!({"error": "No project root"}).to_string(),
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains("/api/api-keys") {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let content_length: usize = header_text
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                        let body_owned;
                        let body_text = if peeked_body.len() >= content_length {
                            &peeked_body[..content_length]
                        } else {
                            let remaining = content_length.saturating_sub(peeked_body.len());
                            let mut full = peeked_body.to_string();
                            if remaining > 0 {
                                let mut rest = vec![0u8; remaining];
                                if stream.read_exact(&mut rest).await.is_ok() {
                                    full.push_str(&String::from_utf8_lossy(&rest));
                                }
                            }
                            body_owned = full;
                            &body_owned
                        };
                        let result = handle_set_api_keys(body_text);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            result.len(), result
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/api-key-status") {
                        use tokio::io::AsyncWriteExt;
                        let body = get_api_key_status_json();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains(" /session") && !request_line.contains("/api/session/") {
                        let result = mint_session_token(&session_provider, &session_model).await;
                        let (status, body) = match result {
                            Ok(json) => ("200 OK", json),
                            Err(msg) => ("502 Bad Gateway", serde_json::json!({"error": msg}).to_string()),
                        };
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status, body.len(), body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/recordings/") && !request_line.contains("/api/session/") {
                        // Serve recording data: segment files and metadata.
                        use tokio::io::AsyncWriteExt;
                        let path_part = request_line
                            .split("/recordings/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let parts: Vec<&str> = path_part.split('/').collect();

                        if let Some(ref rec_reg) = recording_registry {
                            let reg = rec_reg.read().await;

                            if parts.len() == 2 && parts[1] == "segments" {
                                // GET /recordings/{stream}/segments — check session then daemon dir
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    // Fallback to daemon recordings dir
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let json: Vec<serde_json::Value> = segments.iter().map(|s| {
                                    serde_json::json!({
                                        "filename": s.filename,
                                        "start_secs": s.start_secs,
                                        "end_secs": s.end_secs,
                                    })
                                }).collect();
                                let body = serde_json::to_string(&json).unwrap_or("[]".to_string());
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(), body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 && parts[1] == "playlist.m3u8" {
                                // GET /recordings/{stream}/playlist.m3u8 — HLS playlist
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let mut m3u8 = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:0\n");
                                let max_dur = segments.iter().map(|s| s.end_secs - s.start_secs).fold(0.0f64, f64::max);
                                m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", max_dur.ceil() as u64));
                                for s in &segments {
                                    let dur = s.end_secs - s.start_secs;
                                    m3u8.push_str(&format!("#EXTINF:{:.3},\n{}\n", dur, s.filename));
                                }
                                m3u8.push_str("#EXT-X-ENDLIST\n");
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/vnd.apple.mpegurl\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    m3u8.len(), m3u8
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 {
                                // GET /recordings/{stream}/{filename} — serve segment file
                                let stream_name = parts[0];
                                let filename = parts[1];
                                // Validate filename to prevent path traversal
                                let valid = filename.starts_with("seg_")
                                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                                    && filename.len() < 30
                                    && !filename.contains("..");
                                if valid {
                                    // Check session dir first, then daemon dir
                                    let session_path = reg.session_dir()
                                        .join("recordings")
                                        .join(stream_name)
                                        .join(filename);
                                    let daemon_path = crate::debug::daemon_recordings_dir()
                                        .join(stream_name)
                                        .join(filename);
                                    let seg_path = if session_path.exists() {
                                        session_path
                                    } else {
                                        daemon_path
                                    };
                                    let content_type = if filename.ends_with(".ts") { "video/mp2t" } else { "video/mp4" };
                                    match tokio::fs::read(&seg_path).await {
                                        Ok(data) => {
                                            let header = format!(
                                                "HTTP/1.1 200 OK\r\n\
                                                 Content-Type: {}\r\n\
                                                 Content-Length: {}\r\n\
                                                 Cache-Control: public, max-age=3600\r\n\
                                                 Connection: close\r\n\
                                                 \r\n",
                                                content_type, data.len()
                                            );
                                            let _ = stream.write_all(header.as_bytes()).await;
                                            let _ = stream.write_all(&data).await;
                                        }
                                        Err(_) => {
                                            let body = "Segment not found";
                                            let response = format!(
                                                "HTTP/1.1 404 Not Found\r\n\
                                                 Content-Type: text/plain\r\n\
                                                 Content-Length: {}\r\n\
                                                 Connection: close\r\n\
                                                 \r\n\
                                                 {}",
                                                body.len(), body
                                            );
                                            let _ = stream.write_all(response.as_bytes()).await;
                                        }
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(), body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                let body = "Not found";
                                let response = format!(
                                    "HTTP/1.1 404 Not Found\r\n\
                                     Content-Type: text/plain\r\n\
                                     Content-Length: {}\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(), body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else {
                            let body = "Recording not available";
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\n\
                                 Content-Type: text/plain\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(), body
                            );
                            use tokio::io::AsyncWriteExt;
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.contains("/recordings") && !request_line.contains("/api/session/") {
                        // GET /recordings — list all streams (session + daemon-scoped)
                        use tokio::io::AsyncWriteExt;

                        let mut all_entries = Vec::new();

                        // Session-scoped recordings (from RecordingRegistry)
                        if let Some(ref rec_reg) = recording_registry {
                            let reg = rec_reg.read().await;
                            let streams = reg.all_streams();
                            for name in &streams {
                                let manifest = reg.manifest(name).unwrap_or(serde_json::json!({}));
                                let segments = reg.segments(name);
                                let total_duration = segments.last()
                                    .map(|s| s.end_secs)
                                    .unwrap_or(0.0);
                                let seg_json: Vec<serde_json::Value> = segments.iter().map(|s| {
                                    serde_json::json!({
                                        "filename": s.filename,
                                        "start_secs": s.start_secs,
                                        "end_secs": s.end_secs,
                                    })
                                }).collect();
                                let mut entry = manifest;
                                entry["segments"] = serde_json::Value::Array(seg_json);
                                entry["total_duration_secs"] = serde_json::json!(total_duration);
                                all_entries.push(entry);
                            }
                        }

                        // Daemon-scoped recordings (from ~/.intendant/recordings/)
                        let daemon_dir = crate::debug::daemon_recordings_dir();
                        let mut daemon_streams: std::collections::HashSet<String> = std::collections::HashSet::new();
                        for entry in list_recording_streams(&daemon_dir) {
                            if let Some(name) = entry["stream_name"].as_str() {
                                daemon_streams.insert(name.to_string());
                            }
                            all_entries.push(entry);
                        }

                        let body = serde_json::to_string(&all_entries).unwrap_or("[]".to_string());
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if (request_line.starts_with("DELETE") || request_line.starts_with("POST"))
                        && request_line.contains("/api/session/")
                        && request_line.contains("/delete")
                    {
                        // DELETE /api/session/{id}[/{target}]  (native DELETE)
                        // POST  /api/session/{id}/delete[/{target}]  (WKWebView fallback)
                        use tokio::io::AsyncWriteExt;
                        let rest = request_line
                            .split("/api/session/")
                            .nth(1)
                            .and_then(|r| r.split_whitespace().next())
                            .unwrap_or("");
                        let rest_parts: Vec<&str> = rest.split('/')
                            .filter(|s| !s.is_empty() && *s != "delete")
                            .collect();
                        let session_id = rest_parts.first().copied().unwrap_or("");
                        let target = rest_parts.get(1).copied().unwrap_or("session");
                        let body = delete_session_data(session_id, target);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("DELETE") && request_line.contains("/api/session/") {
                        // Plain DELETE without /delete in path (curl, regular browser)
                        use tokio::io::AsyncWriteExt;
                        let rest = request_line
                            .split("/api/session/")
                            .nth(1)
                            .and_then(|r| r.split_whitespace().next())
                            .unwrap_or("");
                        let rest_parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
                        let session_id = rest_parts.first().copied().unwrap_or("");
                        let target = rest_parts.get(1).copied().unwrap_or("session");
                        let body = delete_session_data(session_id, target);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/session/") {
                        use tokio::io::AsyncWriteExt;
                        // Extract the rest after /api/session/ and split into parts
                        let rest = request_line
                            .split("/api/session/")
                            .nth(1)
                            .and_then(|r| r.split_whitespace().next())
                            .unwrap_or("");
                        let rest_parts: Vec<&str> = rest.split('/').collect();

                        if rest_parts.len() >= 2 && rest_parts[1] == "recordings" {
                            // Session recording sub-routes: /api/session/{id}/recordings[/...]
                            let session_id = rest_parts[0];
                            let rec_rest = &rest_parts[2..]; // parts after "recordings"

                            if rec_rest.len() == 2 && rec_rest[1] == "segments" {
                                // GET /api/session/{id}/recordings/{stream}/segments
                                let stream_name = rec_rest[0];
                                let body = if let Some(session_dir) = resolve_session_dir(session_id) {
                                    let stream_dir = session_dir.join("recordings").join(stream_name);
                                    let segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                    let seg_json: Vec<serde_json::Value> = segments.iter().map(|s| {
                                        serde_json::json!({
                                            "filename": s.filename,
                                            "start_secs": s.start_secs,
                                            "end_secs": s.end_secs,
                                        })
                                    }).collect();
                                    serde_json::to_string(&seg_json).unwrap_or("[]".to_string())
                                } else {
                                    "[]".to_string()
                                };
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(), body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if rec_rest.len() == 2 {
                                // GET /api/session/{id}/recordings/{stream}/{filename}
                                let stream_name = rec_rest[0];
                                let filename = rec_rest[1];
                                let valid = filename.starts_with("seg_")
                                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                                    && filename.len() < 30
                                    && !filename.contains("..");
                                if valid {
                                    let seg_ct = if filename.ends_with(".ts") { "video/mp2t" } else { "video/mp4" };
                                    let seg_path = resolve_session_dir(session_id)
                                        .map(|d| d.join("recordings").join(stream_name).join(filename));
                                    if let Some(path) = seg_path.filter(|p| p.exists()) {
                                        match tokio::fs::read(&path).await {
                                            Ok(data) => {
                                                let header = format!(
                                                    "HTTP/1.1 200 OK\r\n\
                                                     Content-Type: {}\r\n\
                                                     Content-Length: {}\r\n\
                                                     Cache-Control: public, max-age=3600\r\n\
                                                     Connection: close\r\n\
                                                     \r\n",
                                                    seg_ct, data.len()
                                                );
                                                let _ = stream.write_all(header.as_bytes()).await;
                                                let _ = stream.write_all(&data).await;
                                            }
                                            Err(_) => {
                                                let body = "Failed to read segment";
                                                let response = format!(
                                                    "HTTP/1.1 500 Internal Server Error\r\n\
                                                     Content-Type: text/plain\r\n\
                                                     Content-Length: {}\r\n\
                                                     Connection: close\r\n\
                                                     \r\n\
                                                     {}",
                                                    body.len(), body
                                                );
                                                let _ = stream.write_all(response.as_bytes()).await;
                                            }
                                        }
                                    } else {
                                        let body = "Segment not found";
                                        let response = format!(
                                            "HTTP/1.1 404 Not Found\r\n\
                                             Content-Type: text/plain\r\n\
                                             Content-Length: {}\r\n\
                                             Connection: close\r\n\
                                             \r\n\
                                             {}",
                                            body.len(), body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(), body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                // GET /api/session/{id}/recordings — list streams
                                let body = if let Some(session_dir) = resolve_session_dir(session_id) {
                                    let recordings_dir = session_dir.join("recordings");
                                    let entries = list_recording_streams(&recordings_dir);
                                    serde_json::to_string(&entries).unwrap_or("[]".to_string())
                                } else {
                                    "[]".to_string()
                                };
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(), body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else if rest_parts.len() >= 2 && rest_parts[1] == "frames" {
                            // Session frame sub-routes: /api/session/{id}/frames[/{filename}]
                            use tokio::io::AsyncWriteExt;
                            let session_id = rest_parts[0];
                            let frame_rest = &rest_parts[2..];

                            if frame_rest.len() == 1 {
                                // GET /api/session/{id}/frames/{filename}
                                let filename = frame_rest[0];
                                let valid = (filename.ends_with(".jpg") || filename.ends_with(".png"))
                                    && filename.len() < 80
                                    && !filename.contains("..");
                                if valid {
                                    let ct = if filename.ends_with(".png") { "image/png" } else { "image/jpeg" };
                                    let frame_path = resolve_session_dir(session_id)
                                        .map(|d| d.join("frames").join(filename));
                                    if let Some(path) = frame_path.filter(|p| p.exists()) {
                                        match tokio::fs::read(&path).await {
                                            Ok(data) => {
                                                let header = format!(
                                                    "HTTP/1.1 200 OK\r\n\
                                                     Content-Type: {}\r\n\
                                                     Content-Length: {}\r\n\
                                                     Cache-Control: public, max-age=3600\r\n\
                                                     Connection: close\r\n\
                                                     \r\n",
                                                    ct, data.len()
                                                );
                                                let _ = stream.write_all(header.as_bytes()).await;
                                                let _ = stream.write_all(&data).await;
                                            }
                                            Err(_) => {
                                                let body = "Failed to read frame";
                                                let response = format!(
                                                    "HTTP/1.1 500 Internal Server Error\r\n\
                                                     Content-Type: text/plain\r\n\
                                                     Content-Length: {}\r\n\
                                                     Connection: close\r\n\
                                                     \r\n\
                                                     {}",
                                                    body.len(), body
                                                );
                                                let _ = stream.write_all(response.as_bytes()).await;
                                            }
                                        }
                                    } else {
                                        let body = "Frame not found";
                                        let response = format!(
                                            "HTTP/1.1 404 Not Found\r\n\
                                             Content-Type: text/plain\r\n\
                                             Content-Length: {}\r\n\
                                             Connection: close\r\n\
                                             \r\n\
                                             {}",
                                            body.len(), body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(), body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                // GET /api/session/{id}/frames — list frame filenames
                                let body = if let Some(session_dir) = resolve_session_dir(session_id) {
                                    let frames_dir = session_dir.join("frames");
                                    let mut names: Vec<String> = Vec::new();
                                    if frames_dir.is_dir() {
                                        if let Ok(entries) = std::fs::read_dir(&frames_dir) {
                                            for e in entries.flatten() {
                                                let n = e.file_name().to_string_lossy().to_string();
                                                if n.ends_with(".jpg") || n.ends_with(".png") {
                                                    names.push(n);
                                                }
                                            }
                                        }
                                        names.sort();
                                    }
                                    serde_json::to_string(&names).unwrap_or("[]".to_string())
                                } else {
                                    "[]".to_string()
                                };
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(), body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else {
                            // GET /api/session/{id} — session detail
                            let session_id = rest_parts[0].split('?').next().unwrap_or(rest_parts[0]);
                            let body = get_session_detail(session_id);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: no-cache\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(), body
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.contains("/api/displays") {
                        // Display enumeration endpoint
                        use tokio::io::AsyncWriteExt;
                        let displays = crate::display::enumerate_displays().await;
                        let body = serde_json::to_string(&displays).unwrap_or_else(|_| "[]".to_string());
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/sessions") {
                        // Session listing endpoint
                        let body = list_sessions();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(), body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/debug") {
                        // Debug endpoint: returns agent state + voice connection info
                        let state = query_ctx.as_ref()
                            .map(|ctx| ctx.agent_state.lock().unwrap_or_else(|e| e.into_inner()).clone());
                        let vd = voice_debug.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let active_id = active_presence.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                            .map(|a| a.connection_id.clone());
                        let debug_json = serde_json::json!({
                            "agent_state": state,
                            "voice": vd,
                            "active_connection_id": active_id,
                        }).to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            debug_json.len(),
                            debug_json
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains(" /mcp") {
                        // MCP Streamable HTTP endpoint.
                        //
                        // rmcp expects:
                        //   - Requests (has `id`):   200 OK + Content-Type: application/json
                        //   - Notifications (no `id`): 202 Accepted + empty body
                        //   - GET for SSE stream:    405 Method Not Allowed (we don't support SSE push)
                        //   - DELETE for session:    405 Method Not Allowed (stateless)
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        if let Some(ref mcp) = mcp_server {
                            let content_length: usize = header_text
                                .lines()
                                .find(|l| l.to_lowercase().starts_with("content-length:"))
                                .and_then(|l| l.split(':').nth(1))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                            let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                            let body_owned;
                            let body_text = if peeked_body.len() >= content_length {
                                &peeked_body[..content_length]
                            } else {
                                let remaining = content_length.saturating_sub(peeked_body.len());
                                let mut full = peeked_body.to_string();
                                if remaining > 0 {
                                    let mut rest = vec![0u8; remaining];
                                    if stream.read_exact(&mut rest).await.is_ok() {
                                        full.push_str(&String::from_utf8_lossy(&rest));
                                    }
                                }
                                body_owned = full;
                                &body_owned
                            };
                            let outcome = handle_mcp_http_request(body_text, mcp).await;
                            let http_response = match outcome {
                                McpHttpOutcome::Response(resp) => {
                                    let json = serde_json::to_string(&resp).unwrap_or_default();
                                    format!(
                                        "HTTP/1.1 200 OK\r\n\
                                         Content-Type: application/json\r\n\
                                         Access-Control-Allow-Origin: *\r\n\
                                         Content-Length: {}\r\n\
                                         \r\n\
                                         {}",
                                        json.len(),
                                        json,
                                    )
                                }
                                McpHttpOutcome::Accepted => {
                                    "HTTP/1.1 202 Accepted\r\n\
                                     Access-Control-Allow-Origin: *\r\n\
                                     Content-Length: 0\r\n\
                                     \r\n".to_string()
                                }
                            };
                            let _ = stream.write_all(http_response.as_bytes()).await;
                        } else {
                            let err = r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"MCP server not available"}}"#;
                            let http = format!(
                                "HTTP/1.1 503 Service Unavailable\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 \r\n\
                                 {}",
                                err.len(), err
                            );
                            let _ = stream.write_all(http.as_bytes()).await;
                        }
                    } else if (request_line.starts_with("GET") || request_line.starts_with("DELETE"))
                        && request_line.contains(" /mcp")
                    {
                        // MCP Streamable HTTP: GET (SSE stream) and DELETE (session cleanup)
                        // are not supported by our stateless endpoint.  Return 405 so rmcp
                        // gracefully falls back (skips SSE / ignores session delete).
                        use tokio::io::AsyncWriteExt;
                        let http = "HTTP/1.1 405 Method Not Allowed\r\n\
                                    Access-Control-Allow-Origin: *\r\n\
                                    Content-Length: 0\r\n\
                                    \r\n";
                        let _ = stream.write_all(http.as_bytes()).await;
                    } else {
                        let (content_type, body, cache) = if request_line.contains("/wasm-web/presence_web.js") {
                            ("application/javascript", WASM_WEB_JS.to_string(), "no-cache, must-revalidate")
                        } else if request_line.contains("/audio-processor.js") {
                            ("application/javascript", AUDIO_PROCESSOR_JS.to_string(), "no-cache")
                        } else if request_line.contains("/config") {
                            ("application/json", config_json.clone(), "no-cache")
                        } else {
                            // Default: serve app.html (also matches /app for backward compat)
                            ("text/html; charset=utf-8", app_html.to_string(), "no-cache")
                        };

                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: {}\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            content_type,
                            body.len(),
                            cache,
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                }
            });
        }
    })
}

/// Build a `WebGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
pub fn build_config(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_servers: Vec<crate::display::IceServer>,
) -> WebGatewayConfig {
    // If an explicit provider is given, use it directly.
    if let Some(provider) = live_provider {
        let model = live_model.unwrap_or_else(|| match provider {
            "openai" => "gpt-4o-realtime-preview",
            _ => "gemini-2.5-flash-native-audio-preview-12-2025",
        });
        let (input_rate, output_rate) = if provider == "openai" {
            (24000, 24000)
        } else {
            (16000, 24000)
        };
        return WebGatewayConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            input_sample_rate: input_rate,
            output_sample_rate: output_rate,
            transcription_enabled,
            ice_servers,
        };
    }

    // If an explicit live model is given, detect provider from the model name.
    if let Some(model) = live_model {
        if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
            return WebGatewayConfig {
                provider: "openai".to_string(),
                model: model.to_string(),
                input_sample_rate: 24000,
                output_sample_rate: 24000,
                transcription_enabled,
                ice_servers,
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
        };
    }

    // Fall back to env var detection
    if std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("GEMINI_API_KEY").is_err() {
        WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
        }
    } else {
        let mut cfg = WebGatewayConfig::default();
        cfg.transcription_enabled = transcription_enabled;
        cfg.ice_servers = ice_servers;
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutboundEvent;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn test_default_port() {
        assert_eq!(DEFAULT_PORT, 8765);
    }

    #[test]
    fn test_app_html_embedded() {
        assert!(!APP_HTML.is_empty());
        assert!(APP_HTML.contains("<!DOCTYPE html>"));
        assert!(APP_HTML.contains("tab-activity"));
        assert!(APP_HTML.contains("tab-stats"));
        assert!(APP_HTML.contains("tab-terminal"));
        assert!(APP_HTML.contains("tab-displays"));
    }

    #[test]
    fn test_web_gateway_config_default() {
        let config = WebGatewayConfig::default();
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
        assert_eq!(config.output_sample_rate, 24000);
    }

    #[test]
    fn test_web_gateway_config_serialize() {
        let config = WebGatewayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"provider\":\"gemini\""));
        assert!(json.contains("\"input_sample_rate\":16000"));
    }

    #[test]
    fn test_build_config_gemini_model() {
        let config = build_config(None, Some("gemini-2.5-flash-native-audio-preview-12-2025"), false, Vec::new());
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(None, Some("gpt-4o-realtime-preview"), false, Vec::new());
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(Some("openai"), None, false, Vec::new());
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(None, None, false, Vec::new());
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_websocket_echo() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        // Bind to port 0 for a random free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a Status control message
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Verify the EventBus receives the ControlCommand
        // (may be preceded by a PresenceLog debug event from the diagnostic logging)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(
                tokio::time::Duration::from_secs(2),
                rx.recv(),
            )
            .await
            .expect("timeout")
            .expect("channel closed");

            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status)) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        handle.abort();
    }

    #[tokio::test]
    async fn test_broadcast_to_websocket() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx.clone(), config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx, mut ws_rx) = ws.split();

        // Give the subscription a moment to register
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Broadcast an event
        let event = OutboundEvent::Status {
            turn: 1,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
            session_id: "test-session".to_string(),
            task: "test task".to_string(),
        };
        crate::control::broadcast_event(&broadcast_tx, &event);

        // Verify the WebSocket client receives it
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            ws_rx.next(),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            assert!(text.contains("\"event\":\"status\""));
            assert!(text.contains("\"turn\":1"));
        } else {
            panic!("expected text message");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_html() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Plain HTTP GET
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        // Read with timeout
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("<!DOCTYPE html>"));

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_config() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
        };
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // GET /config
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("application/json"));
        assert!(response_str.contains("\"provider\":\"openai\""));

        handle.abort();
    }

    #[tokio::test]
    async fn test_presence_connect_disconnect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect (new protocol)
        ws.send(Message::Text(r#"{"t":"presence_connect","server_session_id":"sess-1","last_event_seq":5}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");

        match event {
            AppEvent::PresenceConnected { server_session_id, last_event_seq, .. } => {
                assert_eq!(server_session_id.as_deref(), Some("sess-1"));
                assert_eq!(last_event_seq, 5);
            }
            _ => panic!("expected PresenceConnected, got {:?}", event),
        }

        // Send presence_disconnect
        ws.send(Message::Text(r#"{"t":"presence_disconnect"}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    #[tokio::test]
    async fn test_voice_log_forwarding() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        ws.send(Message::Text(r#"{"t":"voice_log","text":"hello","seq":3,"tool_context":"check_status"}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");

        match event {
            AppEvent::VoiceLog { text, seq, tool_context } => {
                assert_eq!(text, "hello");
                assert_eq!(seq, 3);
                assert_eq!(tool_context.as_deref(), Some("check_status"));
            }
            _ => panic!("expected VoiceLog"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_check_status() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Create a query context with a known agent state
        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "thinking".to_string(),
            turn: 3,
            budget_pct: 0.15,
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
                let ss = ActiveSessionState::empty();
                ss.write().await.query_ctx = query_ctx;
                spawn_web_gateway(listener, bus, broadcast_tx, config, ss, None, None, None, None, None)
            };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx_split, mut ws_rx) = ws.split();

        // First message should be the bootstrap state_snapshot
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            ws_rx.next(),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["state"]["phase"], "thinking");
            assert_eq!(json["state"]["turn"], 3);
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_response_roundtrip() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "running_agent".to_string(),
            turn: 5,
            budget_pct: 0.42,
            last_command_preview: "cargo test".to_string(),
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
                let ss = ActiveSessionState::empty();
                ss.write().await.query_ctx = query_ctx;
                spawn_web_gateway(listener, bus, broadcast_tx, config, ss, None, None, None, None, None)
            };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Drain the bootstrap message
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            futures_util::StreamExt::next(&mut ws),
        )
        .await;

        // Send a check_status tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_1","tool":"check_status","args":{}}"#.into(),
        ))
        .await
        .unwrap();

        // Read the tool_response
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "tool_response");
            assert_eq!(json["id"], "req_1");
            let result = json["result"].as_str().unwrap();
            assert!(result.contains("running_agent"), "result: {}", result);
            assert!(result.contains("Turn: 5"), "result: {}", result);
        } else {
            panic!("expected text message for tool_response");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_action_dispatches_control() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot::default()));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
                let ss = ActiveSessionState::empty();
                ss.write().await.query_ctx = query_ctx;
                spawn_web_gateway(listener, bus, broadcast_tx, config, ss, None, None, None, None, None)
            };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send an approve_action tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_2","tool":"approve_action","args":{"id":42}}"#.into(),
        ))
        .await
        .unwrap();

        // Should emit a ControlCommand(Approve) on the EventBus
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(
                tokio::time::Duration::from_secs(2),
                rx.recv(),
            )
            .await
            .expect("timeout")
            .expect("channel closed");
            if let AppEvent::ControlCommand(ControlMsg::Approve { id }) = event {
                assert_eq!(id, 42);
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Approve)");

        // Should also get a tool_response back
        // Drain bootstrap first
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            futures_util::StreamExt::next(&mut ws),
        )
        .await;

        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "tool_response");
            assert_eq!(json["id"], "req_2");
            assert!(json["result"].as_str().unwrap().contains("Approved"));
        } else {
            panic!("expected text message");
        }

        handle.abort();
    }

    /// When a WebSocket client that sent `presence_connect` drops without
    /// sending `presence_disconnect`, the server should auto-emit
    /// `PresenceDisconnected` to resume server-side presence.
    #[tokio::test]
    async fn test_ws_drop_auto_sends_presence_disconnected() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the WebSocket WITHOUT sending presence_disconnect
        ws.close(None).await.unwrap();

        // Server should auto-send PresenceDisconnected
        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout waiting for auto PresenceDisconnected")
        .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    /// When a client that never sent `presence_connect` drops, no
    /// `PresenceDisconnected` should be emitted.
    #[tokio::test]
    async fn test_ws_drop_no_auto_disconnect_without_presence() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a control action (routes through EventBus regardless of active state)
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Drain events until we see the Status control event
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(
                tokio::time::Duration::from_secs(2),
                rx.recv(),
            )
            .await
            .expect("timeout")
            .expect("channel closed");
            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status)) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        // Drop the WebSocket
        ws.close(None).await.unwrap();

        // Should NOT receive PresenceDisconnected — only a timeout
        let result = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            rx.recv(),
        )
        .await;
        assert!(result.is_err(), "expected timeout, got {:?}", result);

        handle.abort();
    }

    /// POST /session returns 502 when no API key is configured.
    #[tokio::test]
    async fn test_post_session_no_api_key() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // POST /session without any API key env var set
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("502 Bad Gateway"), "response: {}", response_str);
        assert!(response_str.contains("not set on server"), "response: {}", response_str);

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_audio_processor_js() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /audio-processor.js HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"), "response: {}", response_str);
        assert!(response_str.contains("application/javascript"), "response: {}", response_str);
        assert!(response_str.contains("AudioCaptureProcessor"), "response: {}", response_str);

        handle.abort();
    }

    /// First browser to send presence_connect should become active.
    #[tokio::test]
    async fn test_first_browser_becomes_active() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Should get PresenceConnected on the bus (active browser emits it)
        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive a presence_welcome with is_active: true via direct channel
        // (We need to read WS messages to find it)
        let (_ws_tx_split, mut ws_rx) = ws.split();
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            ws_rx.next(),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "presence_welcome");
            assert_eq!(json["is_active"], true);
        } else {
            panic!("expected text message");
        }

        handle.abort();
    }

    /// Second browser to send presence_connect should be passive (no PresenceConnected emitted).
    #[tokio::test]
    async fn test_second_browser_is_passive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects — becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Drain PresenceConnected from first browser
        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Second browser connects — should be passive
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Should NOT receive PresenceConnected on bus (passive)
        let result = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            rx.recv(),
        )
        .await;
        assert!(result.is_err(), "passive browser should not emit PresenceConnected");

        // Second browser should receive welcome with is_active: false
        // Drain bootstrap state_snapshot first
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(json["is_active"], false, "second browser should be passive");
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_welcome, "second browser should receive presence_welcome");

        handle.abort();
    }

    /// When second browser sends make_active, the first should receive force_disconnect_voice.
    #[tokio::test]
    async fn test_make_active_handover() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // Browser 1 connects and becomes active
        let (ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws1_tx, mut ws1_rx) = ws1.split();
        ws1_tx.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await.expect("timeout").expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain ws1's bootstrap + welcome messages
        for _ in 0..3 {
            let _ = tokio::time::timeout(tokio::time::Duration::from_millis(300), ws1_rx.next()).await;
        }

        // Browser 2 connects (passive — no presence_connect yet, just make_active)
        let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws2_tx, mut ws2_rx) = ws2.split();

        // Drain ws2's bootstrap state_snapshot
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(300), ws2_rx.next()).await;

        // Browser 2 sends make_active
        ws2_tx.send(Message::Text(r#"{"t":"make_active"}"#.into()))
            .await
            .unwrap();

        // Browser 1 should receive force_disconnect_voice
        let mut found_force_disconnect = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws1_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "force_disconnect_voice" {
                            assert_eq!(json["reason"], "handover");
                            found_force_disconnect = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_force_disconnect, "browser 1 should receive force_disconnect_voice");

        // Browser 2 should receive active_granted
        let mut found_active_granted = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "active_granted" {
                            assert_eq!(json["is_active"], true);
                            found_active_granted = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_active_granted, "browser 2 should receive active_granted");

        // EventBus should have received a new PresenceConnected for browser 2
        let mut found_connected = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(AppEvent::PresenceConnected { .. })) => {
                    found_connected = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(found_connected, "make_active should emit PresenceConnected");

        handle.abort();
    }

    /// When the active browser drops, the next browser to connect should get active.
    #[tokio::test]
    async fn test_active_drop_clears_slot() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects and becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await.expect("timeout").expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the active browser
        ws1.close(None).await.unwrap();

        // Should get PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await.expect("timeout").expect("closed");
        assert!(matches!(event, AppEvent::PresenceDisconnected));

        // Give server a moment to process the drop
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second browser connects — should now become active
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Should get PresenceConnected (new active)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await.expect("timeout").expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive welcome with is_active: true
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(json["is_active"], true);
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_welcome, "new browser should be active after old one dropped");

        handle.abort();
    }

    /// An already-active browser re-sending presence_connect (e.g. after voice reconnect)
    /// should receive is_active: true and NOT emit a duplicate PresenceConnected.
    #[tokio::test]
    async fn test_active_browser_resend_presence_connect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws_tx, mut ws_rx) = ws.split();

        // First presence_connect — becomes active
        ws_tx.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Drain PresenceConnected from first connect
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await.expect("timeout").expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain welcome + bootstrap messages
        for _ in 0..5 {
            let _ = tokio::time::timeout(tokio::time::Duration::from_millis(200), ws_rx.next()).await;
        }

        // Re-send presence_connect (simulates voice reconnect after handover)
        ws_tx.send(Message::Text(r#"{"t":"presence_connect","last_event_seq":0}"#.into()))
            .await
            .unwrap();

        // Should receive welcome with is_active: true (still active)
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(json["is_active"], true,
                                "already-active browser should still be active on re-connect");
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_welcome, "should receive presence_welcome");

        // Should NOT get a duplicate PresenceConnected on the bus
        let result = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            rx.recv(),
        )
        .await;
        assert!(result.is_err(), "should not emit duplicate PresenceConnected for already-active browser");

        handle.abort();
    }
}
