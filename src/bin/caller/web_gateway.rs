use crate::presence::{self, AgentStateSnapshot};
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::types::LogLevel;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    pub snapshot_dir: Option<PathBuf>,
    pub project_root_for_changes: Option<PathBuf>,
    /// Shared handle to the live `FileWatcher`, used to serve the per-round
    /// history endpoints (GET history, POST rollback/redo/prune). The same
    /// mutex guards snapshot creation so concurrent rollback from the web
    /// gateway and snapshot-on-round-complete can't race.
    pub file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
}

impl ActiveSessionState {
    pub fn empty() -> SharedActiveSession {
        Arc::new(tokio::sync::RwLock::new(Self {
            query_ctx: None,
            frame_registry: None,
            session_log: None,
            recording_registry: None,
            session_registry: None,
            snapshot_dir: None,
            project_root_for_changes: None,
            file_watcher: None,
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

/// Voice + WebRTC runtime config sent to the web frontend via `/config`.
///
/// Scoped to *runtime config only* — the voice provider, the active
/// model, audio sample rates, and WebRTC ICE servers. Identity-shaped
/// fields (host label, version, git sha) moved out of `/config` and
/// into the Agent Card served at `/.well-known/agent-card.json`: see
/// [`crate::peer::AgentCard`] and [`crate::peer::AgentCard::local_intendant`].
/// That's the single source of truth for who this daemon is and what
/// it can do, and keeping `/config` narrow makes it less likely that
/// future runtime config additions re-blur the boundary.
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
/// - `GET /config` returns a JSON `WebGatewayConfig` (voice/runtime only).
/// - `GET /.well-known/agent-card.json` returns a JSON `AgentCard` with
///   this daemon's identity, capabilities, transports, and auth scheme.
/// - `GET /` (and any other path) returns the web TUI page.
/// - WebSocket connections are bridged to the EventBus (inbound control
///   messages) and broadcast channel (outbound events), mirroring the
///   Unix control socket in `control.rs`.
/// Scan session.jsonl for persisted provider/model/autonomy values.
///
/// The agent loop writes these as plain log entries at startup
/// (`Provider: X`, `Model: Y`, `Autonomy: Z`).  Today the writer uses
/// `l.debug(...)`, so event_type is `debug` for newer sessions and
/// `info` for older ones — scan both.  Replay uses the result to seed
/// the status bar before any events are rendered, replacing the old
/// prefix-based parsing inside `handle_log_replay`.
fn scan_replay_status(
    contents: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut autonomy: Option<String> = None;
    for line in contents.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ev = v.get("event").and_then(|x| x.as_str()).unwrap_or("");
        if !matches!(ev, "info" | "debug" | "warn" | "error") {
            continue;
        }
        let Some(msg) = v.get("message").and_then(|x| x.as_str()) else {
            continue;
        };
        if provider.is_none() {
            if let Some(rest) = msg.strip_prefix("Provider: ") {
                provider = Some(
                    rest.split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
        if model.is_none() {
            if let Some(rest) = msg.strip_prefix("Model: ") {
                model = Some(rest.to_string());
            }
        }
        if autonomy.is_none() {
            if let Some(rest) = msg.strip_prefix("Autonomy: ") {
                autonomy = Some(rest.to_string());
            }
        }
        if provider.is_some() && model.is_some() && autonomy.is_some() {
            break;
        }
    }
    (provider, model, autonomy)
}

/// Convert session.jsonl contents into a stream of OutboundEvent-shaped
/// JSON objects ready to be sent as a `log_replay` message.
///
/// The first entry is always a `replay_start` marker carrying
/// provider/model/autonomy so the WASM `handle_log_replay` can seed the
/// status bar.  Subsequent entries are the result of running each JSONL
/// row through `session_log_entry_to_app_event` → `app_event_to_outbound`
/// and injecting the original `ts` field, so replay drives the exact
/// same rendering path as live broadcast.
fn replay_jsonl_to_outbound_entries(
    contents: &str,
    log_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    let (provider, model, autonomy) = scan_replay_status(contents);

    let mut entries: Vec<serde_json::Value> = Vec::new();
    entries.push(serde_json::json!({
        "event": "replay_start",
        "provider": provider,
        "model": model,
        "autonomy": autonomy,
    }));

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(app_event) =
            crate::session_log::session_log_entry_to_app_event(&entry_json, log_dir)
        else {
            continue;
        };
        let Some(outbound) = crate::event::app_event_to_outbound(&app_event) else {
            continue;
        };
        let Ok(mut value) = serde_json::to_value(&outbound) else {
            continue;
        };
        // Inject the historical timestamp so WASM's handle_event uses it
        // instead of wallclock when rendering log entries.
        if let Some(obj) = value.as_object_mut() {
            let ts = entry_json
                .get("ts")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            obj.insert("ts".to_string(), serde_json::Value::String(ts));
        }
        entries.push(value);
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

/// Build a zip containing the current session's text artifacts for the
/// Settings → "Download session report" feature. Includes session.jsonl,
/// session_meta.json, transcript.jsonl, summary.json, daemon.log,
/// panic.log, and everything under `turns/`. Excludes `frames/` and
/// `recordings/` since those can be hundreds of megabytes and are not
/// needed to diagnose controller-side bugs.
fn build_session_report_zip(session_dir: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    let buf = Cursor::new(Vec::<u8>::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    const FLAT_FILES: &[&str] = &[
        "session.jsonl",
        "session_meta.json",
        "transcript.jsonl",
        "summary.json",
        "daemon.log",
        "panic.log",
    ];

    for name in FLAT_FILES {
        let path = session_dir.join(name);
        if path.is_file() {
            let data = std::fs::read(&path)?;
            zip.start_file(*name, options)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            zip.write_all(&data)?;
        }
    }

    let turns_dir = session_dir.join("turns");
    if turns_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&turns_dir) {
            let mut files: Vec<PathBuf> =
                entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
            files.sort();
            for path in files {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    let zip_name = format!("turns/{}", fname);
                    let data = std::fs::read(&path)?;
                    zip.start_file(&zip_name, options)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                    zip.write_all(&data)?;
                }
            }
        }
    }

    let cursor = zip
        .finish()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(cursor.into_inner())
}

/// Parse a raw HTTP request blob for the `Host:` header and return its
/// hostname portion as an `IpAddr` if it's a literal IP (v4 or v6).
///
/// We need the address the browser is using to reach us — and the Host
/// header is the one piece of the HTTP handshake that actually contains
/// that. Loopback and unspecified addresses are rejected because they
/// don't survive Firefox's remote-candidate filter and wouldn't pair
/// anyway. Hostnames (like `localhost` or `dashboard.internal`) return
/// `None` — there's no ICE-TCP candidate we can usefully emit for those.
fn extract_host_header_ip(headers: &str) -> Option<std::net::IpAddr> {
    for line in headers.lines() {
        // Look for the Host: header line, case-insensitive. `strip_prefix`
        // returning None means "this isn't the Host line" — we must
        // continue the loop, not propagate with `?`.
        let Some(rest) = line
            .strip_prefix("Host: ")
            .or_else(|| line.strip_prefix("host: "))
            .or_else(|| line.strip_prefix("HOST: "))
        else {
            continue;
        };
        // `rest` is `host[:port]` where host can be:
        //   - IPv4 literal: 192.0.2.1
        //   - Bracketed IPv6 literal: [2001:db8::1]
        //   - Hostname: example.com
        let host_part = if let Some(inner) = rest.strip_prefix('[') {
            // IPv6 literal in brackets; chop at the closing bracket.
            match inner.split(']').next() {
                Some(s) => s,
                None => return None,
            }
        } else if let Some(colon) = rest.find(':') {
            &rest[..colon]
        } else {
            rest
        };
        let trimmed = host_part.trim();
        let ip = trimmed.parse::<std::net::IpAddr>().ok()?;
        if ip.is_loopback() || ip.is_unspecified() {
            return None;
        }
        return Some(ip);
    }
    None
}

#[cfg(test)]
mod host_header_tests {
    use super::extract_host_header_ip;
    use std::net::IpAddr;

    #[test]
    fn ipv4_with_port() {
        let headers = "GET / HTTP/1.1\r\nHost: 192.168.1.10:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("192.168.1.10".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn ipv6_bracketed() {
        let headers = "GET / HTTP/1.1\r\nHost: [2001:db8::1]:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("2001:db8::1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn hostname_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: dashboard.internal:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn localhost_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn loopback_ipv4_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn loopback_ipv6_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: [::1]:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn no_host_header() {
        let headers = "GET / HTTP/1.1\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn case_insensitive_header_name() {
        let headers = "GET / HTTP/1.1\r\nhost: 10.0.0.5:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("10.0.0.5".parse::<IpAddr>().unwrap())
        );
    }
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
        replay_jsonl_to_outbound_entries(&contents, &session_dir)
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

/// Handle `/api/session/current/changes[/{path}]` requests.
///
/// - No path suffix: list all changed files (baseline vs current).
/// - With path suffix: return unified diff for a single file.
fn handle_changes_request(
    request_line: &str,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
) -> String {
    let (snapshot_dir, project_root) = match (snapshot_dir, project_root) {
        (Some(s), Some(p)) => (s, p),
        _ => {
            return serde_json::json!({"error": "file watcher not active"}).to_string();
        }
    };

    let baseline_dir = snapshot_dir.join("baseline");
    if !baseline_dir.exists() {
        return serde_json::json!([]).to_string();
    }

    // Extract the path after /api/session/current/changes
    let file_path = request_line
        .split("/api/session/current/changes")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("")
        .trim_start_matches('/');

    if file_path.is_empty() {
        // List all changed files.
        handle_changes_list(&baseline_dir, project_root)
    } else {
        // Single-file diff.
        handle_changes_file_diff(file_path, &baseline_dir, project_root)
    }
}

/// List all files that have changed since the session baseline.
fn handle_changes_list(baseline_dir: &Path, project_root: &Path) -> String {
    let mut changes = Vec::new();
    let mut stack = vec![baseline_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = match path.strip_prefix(baseline_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().to_string();
            let current_path = project_root.join(rel);

            let baseline = std::fs::read_to_string(&path).unwrap_or_default();
            if current_path.exists() {
                let current = match std::fs::read_to_string(&current_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if baseline == current {
                    continue; // no change
                }
                let (lines_added, lines_removed) = {
                    let diff = similar::TextDiff::from_lines(baseline.as_str(), current.as_str());
                    let mut added = 0u32;
                    let mut removed = 0u32;
                    for change in diff.iter_all_changes() {
                        match change.tag() {
                            similar::ChangeTag::Insert => added += 1,
                            similar::ChangeTag::Delete => removed += 1,
                            similar::ChangeTag::Equal => {}
                        }
                    }
                    (added, removed)
                };
                let kind = if baseline.is_empty() { "created" } else { "modified" };
                changes.push(serde_json::json!({
                    "path": rel_str,
                    "kind": kind,
                    "lines_added": lines_added,
                    "lines_removed": lines_removed,
                }));
            } else {
                // File was deleted.
                let lines_removed = baseline.lines().count() as u32;
                changes.push(serde_json::json!({
                    "path": rel_str,
                    "kind": "deleted",
                    "lines_added": 0,
                    "lines_removed": lines_removed,
                }));
            }
        }
    }
    serde_json::to_string(&changes).unwrap_or_else(|_| "[]".to_string())
}

/// Return a unified diff for a single file.
fn handle_changes_file_diff(
    file_path: &str,
    baseline_dir: &Path,
    project_root: &Path,
) -> String {
    // Reject path traversal.
    let rel = Path::new(file_path);
    for component in rel.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return serde_json::json!({"error": "invalid path"}).to_string();
        }
    }

    let baseline_path = baseline_dir.join(rel);
    let current_path = project_root.join(rel);

    // Verify resolved paths stay within their roots.
    if let (Ok(resolved_baseline), Ok(resolved_root)) = (
        baseline_path.canonicalize().or_else(|_| Ok::<PathBuf, std::io::Error>(baseline_path.clone())),
        baseline_dir.canonicalize().or_else(|_| Ok::<PathBuf, std::io::Error>(baseline_dir.to_path_buf())),
    ) {
        if !resolved_baseline.starts_with(&resolved_root) {
            return serde_json::json!({"error": "invalid path"}).to_string();
        }
    }
    if let (Ok(resolved_current), Ok(resolved_root)) = (
        current_path.canonicalize().or_else(|_| Ok::<PathBuf, std::io::Error>(current_path.clone())),
        project_root.canonicalize().or_else(|_| Ok::<PathBuf, std::io::Error>(project_root.to_path_buf())),
    ) {
        if !resolved_current.starts_with(&resolved_root) {
            return serde_json::json!({"error": "invalid path"}).to_string();
        }
    }

    let baseline = std::fs::read_to_string(&baseline_path).unwrap_or_default();
    let current = if current_path.exists() {
        std::fs::read_to_string(&current_path).unwrap_or_default()
    } else {
        String::new()
    };

    let diff = crate::file_watcher::compute_unified_diff(&baseline, &current, file_path);
    let (lines_added, lines_removed) = {
        let text_diff = similar::TextDiff::from_lines(baseline.as_str(), current.as_str());
        let mut added = 0u32;
        let mut removed = 0u32;
        for change in text_diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => added += 1,
                similar::ChangeTag::Delete => removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }
        (added, removed)
    };

    serde_json::json!({
        "path": file_path,
        "diff": diff,
        "lines_added": lines_added,
        "lines_removed": lines_removed,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Per-round file snapshot history endpoints
// ---------------------------------------------------------------------------

/// Read the full POST body (honoring Content-Length). Returns the peeked
/// prefix if the headers already carried the entire payload; otherwise reads
/// the remainder from the stream.
async fn read_post_body(
    header_text: &str,
    stream: &mut tokio::net::TcpStream,
) -> String {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return peeked_body[..content_length].to_string();
    }
    let mut full = peeked_body.to_string();
    let remaining = content_length.saturating_sub(peeked_body.len());
    if remaining > 0 {
        let mut rest = vec![0u8; remaining];
        if stream.read_exact(&mut rest).await.is_ok() {
            full.push_str(&String::from_utf8_lossy(&rest));
        }
    }
    full
}

// ---------------------------------------------------------------------------
// File upload endpoints
// ---------------------------------------------------------------------------

/// Hard cap on individual uploaded file size. Prevents a rogue or mistaken
/// upload (e.g. someone dragging a multi-GB video file) from OOMing the
/// daemon or filling the session dir. Plumbed through the streaming reader
/// so we bail before reading the full body.
///
/// Picked to cover common real uploads (PDFs, CSVs, source archives,
/// annotated screenshots) without accepting arbitrary blobs. Can be made
/// configurable later via `[upload] max_size_mb` in intendant.toml.
const UPLOAD_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Stream the body of an HTTP request into a fresh tempfile, honouring
/// `Content-Length` and bailing out early if the body exceeds `max_bytes`.
///
/// Returns `(tempfile, size)` on success. Designed so the caller can then
/// commit the tempfile into the upload store via
/// [`crate::upload_store::commit_upload`], which atomically renames it
/// into place.
///
/// This is the binary counterpart to `read_post_body` — same peek-then-
/// stream pattern, but sinks to disk instead of a UTF-8 `String`.
async fn stream_body_to_tempfile(
    header_text: &str,
    stream: &mut tokio::net::TcpStream,
    max_bytes: usize,
) -> Result<(tempfile::NamedTempFile, usize), String> {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| "missing or invalid Content-Length".to_string())?;
    if content_length == 0 {
        return Err("empty body".to_string());
    }
    if content_length > max_bytes {
        return Err(format!(
            "body too large: {} bytes (cap is {})",
            content_length, max_bytes
        ));
    }

    let peeked_body = header_text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .as_bytes();
    let mut tmp = tempfile::NamedTempFile::new()
        .map_err(|e| format!("create tempfile: {e}"))?;

    // Write whatever body bytes we already have from the peek. These come
    // back through the same header_text split, so they're the leading
    // content_length bytes — truncate defensively in case the peek read
    // slightly more than the body.
    let peeked_n = peeked_body.len().min(content_length);
    tmp.write_all(&peeked_body[..peeked_n])
        .map_err(|e| format!("write tempfile: {e}"))?;
    let mut written = peeked_n;

    // Pull the rest from the socket in 64 KB chunks. The cap bails early;
    // the final total is asserted to equal Content-Length so we don't store
    // a truncated file.
    let mut buf = vec![0u8; 64 * 1024];
    while written < content_length {
        let want = (content_length - written).min(buf.len());
        match stream.read(&mut buf[..want]).await {
            Ok(0) => {
                return Err(format!(
                    "connection closed mid-upload at {} / {} bytes",
                    written, content_length
                ));
            }
            Ok(n) => {
                tmp.as_file_mut()
                    .write_all(&buf[..n])
                    .map_err(|e| format!("write tempfile: {e}"))?;
                written += n;
            }
            Err(e) => return Err(format!("socket read: {e}")),
        }
    }
    tmp.as_file_mut()
        .flush()
        .map_err(|e| format!("flush tempfile: {e}"))?;
    Ok((tmp, written))
}

/// Parse a query-string value by key out of a full `request_line`
/// (e.g. `POST /api/session/current/uploads?name=foo.pdf&destination=task HTTP/1.1`).
/// Returns the URL-decoded value, or `None` if the key isn't present.
fn query_param<'a>(request_line: &'a str, key: &str) -> Option<String> {
    let path_and_q = request_line.split_whitespace().nth(1)?;
    let query = path_and_q.splitn(2, '?').nth(1)?;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        if k == key {
            return Some(url_decode(v));
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` decoder: `%HH` → byte,
/// `+` → space. Good enough for filenames/destinations on the upload
/// path; we don't invite the full urlencoding crate just for this.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = &bytes[i + 1..i + 3];
                match std::str::from_utf8(h)
                    .ok()
                    .and_then(|hs| u8::from_str_radix(hs, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract the `Content-Type` request header value, or a generic default.
fn content_type_header(header_text: &str) -> String {
    header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-type:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Build an HTTP response for an upload endpoint error.
fn upload_error_response(status: &str, message: &str) -> String {
    let body = serde_json::json!({"error": message}).to_string();
    format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        body.len(),
        body
    )
}

/// Check whether it is safe to mutate the project tree (rollback/redo) right
/// now. Returns `Ok(())` if idle, or an `(status_code, body_json)` pair to
/// send back as-is.
fn ensure_idle(
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> Result<(), (&'static str, String)> {
    if let Some(state) = agent_state {
        let phase = state
            .lock()
            .map(|g| g.phase.clone())
            .unwrap_or_default();
        if !presence::is_agent_idle(&phase) {
            let body = serde_json::json!({
                "error": "agent is busy, stop the turn before rolling back",
                "phase": phase,
            })
            .to_string();
            return Err(("409 Conflict", body));
        }
    }
    Ok(())
}

/// GET /api/session/current/history — returns serialized `History` JSON.
async fn handle_history_get(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    let w = fw.lock().await;
    let body = serde_json::to_string(w.history())
        .unwrap_or_else(|_| "{}".to_string());
    ("200 OK", body)
}

/// POST /api/session/current/rollback — body:
/// ```json
/// { "round_id": N,
///   "revert_files": true,          // default true (backward-compat)
///   "revert_conversation": false   // default false
/// }
/// ```
///
/// Each boolean is independent. When both are false the endpoint is a
/// validation-only no-op (returns 400). Existing callers passing only
/// `round_id` get a file-only revert, matching prior behavior.
///
/// `revert_conversation` emits an `AppEvent::ConversationRollbackRequested`
/// on the shared bus. The active agent loop subscribes and either
/// truncates its native `Conversation` (native path), issues
/// `thread/rollback` (Codex), or shuts down and re-initializes
/// (session-reset for Claude Code / Gemini). A matching
/// `AppEvent::ConversationRolledBack` is emitted when the work
/// completes. The HTTP response does not wait for that completion —
/// the dashboard observes the event stream.
async fn handle_history_rollback(
    body_text: &str,
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
    bus: &EventBus,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    if let Err((status, body)) = ensure_idle(agent_state) {
        return (status, body);
    }
    let parsed: serde_json::Value = match serde_json::from_str(body_text) {
        Ok(v) => v,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("invalid body: {}", e)}).to_string(),
            );
        }
    };
    let round_id = match parsed.get("round_id").and_then(|v| v.as_u64()) {
        Some(id) => id,
        None => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "missing round_id"}).to_string(),
            );
        }
    };
    // Backward-compat: old callers pass only `round_id` and expect a
    // file-only revert. New callers supply both flags.
    let revert_files = parsed
        .get("revert_files")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let revert_conversation = parsed
        .get("revert_conversation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !revert_files && !revert_conversation {
        return (
            "400 Bad Request",
            serde_json::json!({
                "error": "at least one of revert_files / revert_conversation must be true"
            })
            .to_string(),
        );
    }

    // Resolve conversation-rollback parameters before we mutate any
    // state so a downstream failure doesn't leave files half-reverted
    // with no event emitted. Reading the history requires the same
    // mutex the rollback writes use, so we briefly acquire and release.
    let conv_params: Option<(Option<u32>, u32)> = if revert_conversation {
        let w = fw.lock().await;
        let hist = w.history();
        let target_idx = hist.rounds.iter().position(|r| r.id == round_id);
        let head_idx = hist
            .current_head_id
            .and_then(|hid| hist.rounds.iter().position(|r| r.id == hid));
        match (target_idx, head_idx) {
            (Some(t), Some(h)) => {
                // Compute turns to drop from the head turn-count sum
                // between (t, h]. This matches Codex's `turnsToRollback`
                // semantics: the number of turns we want to undo.
                let turns_to_drop: u32 = if t < h {
                    hist.rounds[t + 1..=h]
                        .iter()
                        .map(|r| r.turn_count.unwrap_or(0))
                        .sum()
                } else {
                    0
                };
                let target_msg_count = hist.rounds[t].native_message_count;
                Some((target_msg_count, turns_to_drop))
            }
            (Some(_), None) => {
                // No head — rolling back with no active position is a
                // pure file-state restore; nothing to drop from the
                // conversation side.
                Some((hist.rounds[target_idx.unwrap()].native_message_count, 0))
            }
            _ => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": format!(
                        "round {} not found in active history", round_id
                    )})
                    .to_string(),
                );
            }
        }
    } else {
        None
    };

    // File rollback (may fail for reasons unrelated to the conversation
    // side; bail out before emitting the conversation event so both
    // halves stay consistent from the user's perspective).
    let file_result_json = if revert_files {
        let mut w = fw.lock().await;
        match w.rollback(round_id) {
            Ok(res) => serde_json::json!({
                "to_round_id": res.to_round_id,
                "files_reverted": res.files_reverted,
            }),
            Err(e) => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": e.to_string()}).to_string(),
                );
            }
        }
    } else {
        serde_json::json!({ "to_round_id": round_id, "files_reverted": 0 })
    };

    // Dispatch the conversation-rollback event; the agent loop picks it
    // up and emits `ConversationRolledBack` when done.
    if let Some((target_msg_count, turns_to_drop)) = conv_params {
        bus.send(AppEvent::ConversationRollbackRequested {
            round_id,
            target_native_message_count: target_msg_count,
            turns_to_drop,
        });
    }

    (
        "200 OK",
        serde_json::json!({
            "to_round_id": file_result_json["to_round_id"],
            "files_reverted": file_result_json["files_reverted"],
            "revert_files": revert_files,
            "revert_conversation": revert_conversation,
        })
        .to_string(),
    )
}

/// POST /api/session/current/redo — no body. Advances `current_head_id`.
async fn handle_history_redo(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    if let Err((status, body)) = ensure_idle(agent_state) {
        return (status, body);
    }
    let mut w = fw.lock().await;
    match w.redo() {
        Ok(res) => (
            "200 OK",
            serde_json::json!({
                "to_round_id": res.to_round_id,
                "files_reverted": res.files_reverted,
            })
            .to_string(),
        ),
        Err(e) => (
            "400 Bad Request",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// POST /api/session/current/prune — drop abandoned branches and GC orphaned
/// content-addressed blobs.
async fn handle_history_prune(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    let mut w = fw.lock().await;
    match w.prune_abandoned() {
        Ok(res) => (
            "200 OK",
            serde_json::json!({
                "branches_removed": res.branches_removed,
                "bytes_freed": res.bytes_freed,
            })
            .to_string(),
        ),
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
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
    // External agent default (persisted to `[agent] default_backend`).
    // Values: "codex" | "claude-code" | "gemini" | None (internal agent).
    #[serde(default)]
    pub external_agent: Option<String>,
    // Codex runtime config (persisted to `[agent.codex]`). Mirrored here so
    // the Activity → Control sub-tab can load in one fetch.
    pub codex_sandbox: String,
    pub codex_approval_policy: String,
    #[serde(default)]
    pub codex_model: Option<String>,
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,
    #[serde(default)]
    pub codex_web_search: bool,
    #[serde(default)]
    pub codex_network_access: bool,
    #[serde(default)]
    pub codex_writable_roots: Vec<String>,
    // Gemini runtime config (persisted to `[agent.gemini_cli]`). Mirrors
    // the Codex fields above for the Activity → Control sub-tab.
    #[serde(default)]
    pub gemini_model: Option<String>,
    pub gemini_approval_mode: String,
    #[serde(default)]
    pub gemini_sandbox: bool,
    #[serde(default)]
    pub gemini_extensions: Vec<String>,
    #[serde(default)]
    pub gemini_allowed_mcp_servers: Vec<String>,
    #[serde(default)]
    pub gemini_include_directories: Vec<String>,
    #[serde(default)]
    pub gemini_debug: bool,
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
        external_agent: config.agent.default_backend.clone(),
        codex_sandbox: crate::project::normalize_sandbox_mode(&config.agent.codex.sandbox),
        codex_approval_policy: crate::project::normalize_approval_policy(
            &config.agent.codex.approval_policy,
        ),
        codex_model: config.agent.codex.model.clone(),
        codex_reasoning_effort: crate::project::normalize_reasoning_effort(
            config.agent.codex.reasoning_effort.as_deref(),
        ),
        codex_web_search: config.agent.codex.web_search,
        codex_network_access: config.agent.codex.network_access,
        codex_writable_roots: config.agent.codex.writable_roots.clone(),
        gemini_model: config.agent.gemini_cli.model.clone(),
        gemini_approval_mode: crate::project::normalize_gemini_approval_mode(
            &config.agent.gemini_cli.approval_mode,
        ),
        gemini_sandbox: config.agent.gemini_cli.sandbox,
        gemini_extensions: config.agent.gemini_cli.extensions.clone(),
        gemini_allowed_mcp_servers: config.agent.gemini_cli.allowed_mcp_servers.clone(),
        gemini_include_directories: config.agent.gemini_cli.include_directories.clone(),
        gemini_debug: config.agent.gemini_cli.debug,
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
    // Normalize empty strings to None so the TOML doesn't end up with
    // `default_backend = ""` — the loader treats "" as a valid override
    // and would try to resolve it to a backend.
    config.agent.default_backend = payload
        .external_agent
        .as_ref()
        .and_then(|s| if s.is_empty() { None } else { Some(s.clone()) });
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
    peer_registry: Option<crate::peer::PeerRegistry>,
    advertise_urls: Vec<String>,
    // Inbound bearer token enforcement. When `Some`, federation REST
    // endpoints (/api/peers*, /api/coordinator/*, /api/sessions)
    // require `Authorization: Bearer <token>` matching the configured
    // value; missing or wrong token returns 401. When `None`, no
    // application-layer auth is enforced — the operator's expected to
    // rely on transport security (mTLS proxy, tailnet, loopback).
    // Sourced from `[server.auth] bearer_token` in intendant.toml.
    //
    // /ws, /.well-known/agent-card.json, /config, the dashboard HTML,
    // and static assets are intentionally exempt in this slice — /ws
    // enforcement requires a parallel dashboard auth flow (browser
    // can't easily set headers on `WebSocket` opens) which lands in
    // slice 2d.
    inbound_bearer_token: Option<String>,
    // What to advertise in the local Agent Card's `auth` field —
    // tells connecting peers what wire-layer (transport) and
    // application-layer (bearer) auth they need to satisfy.
    // Built by `crate::main::build_local_advertised_auth` from
    // `[server.auth] advertised_transport` (`"none"` /
    // `"mutual-tls"` / `"pin-self-cert"`) and
    // `[server.auth] bearer_token`. The `pin-self-cert` path reads
    // the daemon's own `server.crt` from the LAN cert dir and
    // pre-fills the fingerprint so operators don't have to compute
    // it manually.
    //
    // Test call sites pass `AuthRequirements::none()` since they
    // don't exercise the advertise path; production call sites in
    // main.rs build the requirements from the project config.
    local_card_auth: crate::peer::AuthRequirements,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());

    // Build the local Agent Card from live runtime state so
    // `/.well-known/agent-card.json` can serve it. The transport URLs
    // come from [`resolve_advertise_urls`], which uses operator
    // overrides verbatim when provided and otherwise falls back to a
    // single auto-detected URL derived from the listener's bind
    // address. Multiple URLs let one daemon advertise itself reachable
    // via several paths (LAN IP, Tailscale, host port-forward, etc.)
    // — the connecting peer probes them in order.
    let advertise_urls =
        resolve_advertise_urls(listener.local_addr().ok(), &advertise_urls);
    let agent_card = build_local_agent_card(advertise_urls, local_card_auth);
    let agent_card_json =
        serde_json::to_string(&agent_card).unwrap_or_else(|_| "{}".to_string());

    // Pre-build ICE config for WebRTC display sessions from the gateway config.
    let ice_config = crate::display::IceConfig {
        ice_servers: config.ice_servers.clone(),
    };

    // Shared ICE-TCP peer registry + advertised TCP port.
    //
    // We multiplex ICE-TCP onto the HTTP listener port: the per-connection
    // accept handler (later in this function) peeks every accepted TCP
    // connection's first bytes to tell HTTP vs. WebSocket vs. STUN-framed
    // traffic apart. STUN traffic is read through one RFC 4571 frame and
    // handed to this registry, which demuxes to the matching peer by the
    // STUN USERNAME's local-ufrag half. The advertised TCP candidate port
    // is the HTTP port itself, so ICE-TCP flows through the exact same
    // tunnel/port-forward that already carries the dashboard — users
    // don't configure anything extra beyond what the dashboard already
    // requires.
    let http_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(0);
    let tcp_peer_registry = crate::display::webrtc::TcpPeerRegistry::new();
    let tcp_advertised_port: Option<u16> = if http_port != 0 {
        Some(http_port)
    } else {
        None
    };

    // Slice 3b: TCP relay registry for primary-as-media-relay. When
    // a federated WebRTC `Answer` flows from a peer back to the
    // browser, the translator (below) extracts the peer's ICE ufrag
    // from the SDP, resolves the peer's outbound TCP address, and
    // registers the mapping here. The accept loop (below) then
    // dispatches incoming STUN-framed TCP connections whose ufrag
    // matches an entry to the relay byte-forwarding path instead of
    // the local WebRtcPeer path — the primary opens a fresh TCP
    // connection to the peer and shuttles bytes between browser and
    // peer until either side closes. Browser ICE treats this as a
    // TCP candidate alongside the peer's direct candidate; direct
    // wins on reachable topologies, relay covers the browser-can-
    // only-reach-primary case (e.g. hypervisor-isolated VMs).
    let tcp_relay_registry = crate::display::webrtc::TcpRelayRegistry::new();

    // Primary's relay TCP URL, used to inject a relay candidate into
    // forwarded `Answer` SDPs. Derived from the agent card's first
    // IntendantWs transport — that's the URL the primary advertises
    // to peers, which on most deployments is also what browsers use
    // to reach the primary. Stored as a string so DNS resolution
    // happens lazily at per-Answer rewrite time rather than once at
    // startup (hostnames may not resolve at boot for Tailscale /
    // mDNS / etc).
    let relay_advertise_url: Option<String> = agent_card
        .transports
        .iter()
        .find_map(|t| match t {
            crate::peer::TransportSpec::IntendantWs { url } => Some(url.clone()),
            _ => None,
        });

    // Inject content-hash version into WASM/JS URLs for cache-busting.
    let v = asset_version_hash();
    let session_provider = config.provider.clone();
    let session_model = config.model.clone();
    let voice_debug = Arc::new(Mutex::new(VoiceDebugState::default()));
    let active_presence: Arc<Mutex<Option<ActivePresence>>> = Arc::new(Mutex::new(None));

    // Process-wide registry of standalone shell PTY sessions, keyed by
    // (host_id, terminal_id). Lives as long as the web gateway task and
    // is cloned into each per-connection handler so WS reconnects reattach
    // to existing shells. Keyed on host_id even though there's only one
    // host today so multi-host phase 1 can add siblings without refactor.
    let terminal_registry: Arc<crate::terminal::TerminalRegistry> = Arc::new(
        crate::terminal::TerminalRegistry::new(
            project_root.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))),
        ),
    );

    // Cache the latest usage_update JSON so late-connecting browsers get it
    // without sending ControlMsg (which would pollute the event log).
    let last_usage_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest live_usage_update JSON for late-connecting browsers.
    let last_live_usage_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest status event (has autonomy, session_id, task).
    let last_status_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest external_agent_changed event so a refreshed
    // browser learns the current value without having to re-fetch
    // settings. Without this the dashboard dropdown snaps back to
    // "None (internal agent)" on every page refresh even though the
    // daemon still has the value in memory.
    let last_external_agent_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest user_display_granted event. The authoritative
    // state lives in AutonomyState.user_display_granted on the server,
    // but the dashboard only learns about it via the broadcast; without
    // this cache a refreshed browser shows "off" regardless of whether
    // the user has actually granted access. Cleared on user_display_revoked
    // so a stale grant doesn't get replayed after the user revokes.
    let last_user_display_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache display_ready JSON per display_id for late-connecting browsers.
    // Using a HashMap so multiple concurrent display sessions are all replayed.
    let display_ready_cache: Arc<Mutex<HashMap<u32, String>>> = Arc::new(Mutex::new(HashMap::new()));
    {
        let usage_cache = last_usage_json.clone();
        let live_usage_cache = last_live_usage_json.clone();
        let status_cache = last_status_json.clone();
        let external_agent_cache = last_external_agent_json.clone();
        let user_display_cache = last_user_display_json.clone();
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
                        // Cache user_display_granted for replay on reconnect.
                        // Clear the cache on user_display_revoked so a refreshed
                        // browser after a revoke doesn't re-enable the badge.
                        if line.contains("\"event\":\"user_display_granted\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"user_display_revoked\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = None;
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
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"external_agent_changed\"") {
                            if let Ok(mut guard) = external_agent_cache.lock() {
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

    // Peer registry → dashboard push translator.
    //
    // When the registry is wired (the daemon was started with
    // federation enabled), subscribe to its [`RegistryEvent`] stream
    // and translate each event into the matching wire-format
    // [`OutboundEvent`] variant, broadcast over the same channel as
    // every other dashboard event. The browser's existing primary
    // WebSocket pipeline picks them up and updates peer rows in-place
    // without polling `GET /api/peers`.
    //
    // Lagged events are skipped on purpose: the dashboard's recovery
    // path is to re-fetch `/api/peers`, which always returns ground
    // truth. Closed receiver = registry was dropped, exit cleanly.
    if let Some(reg) = peer_registry.as_ref() {
        let mut reg_rx = reg.subscribe();
        let push_tx = broadcast_tx.clone();
        let reg_for_task = reg.clone();
        let relay_registry_for_task = Arc::clone(&tcp_relay_registry);
        let relay_url_for_task = relay_advertise_url.clone();
        let bus_for_task = bus.clone();
        tokio::spawn(async move {
            loop {
                match reg_rx.recv().await {
                    Ok(event) => {
                        let outbound = match event {
                            crate::peer::RegistryEvent::PeerAdded(snap) => {
                                crate::types::OutboundEvent::PeerAdded { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerRemoved(id) => {
                                crate::types::OutboundEvent::PeerRemoved {
                                    id: id.as_str().to_string(),
                                }
                            }
                            crate::peer::RegistryEvent::PeerStateChanged(snap) => {
                                crate::types::OutboundEvent::PeerStateChanged { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerEventForwarded {
                                peer,
                                event,
                            } => {
                                // Slice 3b: when a federated Answer
                                // comes back toward the browser, rewrite
                                // the SDP to inject a TCP candidate
                                // pointing at the primary's own relay
                                // address, and register the peer's ufrag
                                // in the relay registry so incoming
                                // browser TCP connections with that
                                // ufrag get forwarded to the peer. Other
                                // event variants pass through verbatim.
                                let rewritten_event = maybe_rewrite_federated_answer(
                                    &peer,
                                    event,
                                    &reg_for_task,
                                    &relay_registry_for_task,
                                    relay_url_for_task.as_deref(),
                                    &bus_for_task,
                                )
                                .await;
                                crate::types::OutboundEvent::PeerEventForwarded {
                                    peer_id: peer.as_str().to_string(),
                                    payload: rewritten_event,
                                }
                            }
                        };
                        crate::control::broadcast_event(&push_tx, &outbound);
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

        if let Some(p) = tcp_advertised_port {
            eprintln!("[web_gateway] ICE-TCP candidates advertise port {p}");
        }

        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();
            let agent_card_json = agent_card_json.clone();
            let peer_registry = peer_registry.clone();
            let ice_config = ice_config.clone();
            let tcp_peer_registry = Arc::clone(&tcp_peer_registry);
            let tcp_relay_registry = Arc::clone(&tcp_relay_registry);
            let tcp_advertised_port = tcp_advertised_port;
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
            let last_external_agent_json = last_external_agent_json.clone();
            let last_user_display_json = last_user_display_json.clone();
            let display_ready_cache = display_ready_cache.clone();
            let web_tui_tx = web_tui_tx.clone();
            let task_tx = task_tx.clone();
            let project_root = project_root.clone();
            let mcp_server = mcp_server.clone();
            let terminal_registry = terminal_registry.clone();
            let inbound_bearer_token = inbound_bearer_token.clone();

            tokio::spawn(async move {
                // Snapshot session state at connection time
                let session_snap = shared_session.read().await;
                let query_ctx = session_snap.query_ctx.clone();
                let frame_registry = session_snap.frame_registry.clone();
                let session_log = session_snap.session_log.clone();
                let recording_registry = session_snap.recording_registry.clone();
                let session_registry = session_snap.session_registry.clone();
                let snapshot_dir = session_snap.snapshot_dir.clone();
                let project_root_for_changes = session_snap.project_root_for_changes.clone();
                let file_watcher = session_snap.file_watcher.clone();
                drop(session_snap);
                // Peek at the first bytes to detect (in order):
                //  1. ICE-TCP STUN-framed traffic (RFC 4571 length prefix
                //     followed by a STUN message whose magic cookie
                //     0x2112A442 sits at payload offset 4 = peek offset 6)
                //  2. WebSocket upgrade (HTTP header containing
                //     "Upgrade: websocket")
                //  3. Plain HTTP (everything else)
                //
                // `peek()` does not consume the data, so both the
                // WebSocket handshake and the HTTP parser still get the
                // full request. Only the ICE-TCP branch actually reads
                // (and consumes) the first RFC 4571 frame, after which
                // the rest of the stream is handed to the WebRTC peer's
                // reader task.
                let mut buf = [0u8; 2048];
                let mut stream = stream;
                let n = match stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };

                // ICE-TCP detection: look for a STUN binding request
                // wrapped in an RFC 4571 2-byte BE length prefix. STUN
                // binding request type is 0x0001 (first payload byte < 2),
                // magic cookie is 0x2112A442 at payload offset 4, which
                // lives at peek offset 6..10 once we account for the
                // length prefix. A valid HTTP request never starts with
                // these bytes (method chars are ASCII >= 0x41).
                let looks_like_stun_tcp = n >= 22
                    && buf[2] < 2
                    && buf[6..10] == [0x21, 0x12, 0xA4, 0x42];
                if looks_like_stun_tcp {
                    // Consume the first RFC 4571 frame from the stream
                    // (peek leaves it in the kernel buffer; we have to
                    // read it through to hand a clean stream to the peer
                    // reader task).
                    let first_frame = match crate::display::webrtc::read_rfc4571_frame_pub(
                        &mut stream,
                    )
                    .await
                    {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!(
                                "[web_gateway] ICE-TCP first-frame read failed: {e}"
                            );
                            return;
                        }
                    };
                    let remote_addr = match stream.peer_addr() {
                        Ok(a) => a,
                        Err(_) => return,
                    };

                    // Slice 3b dispatch: parse the frame's ufrag once,
                    // then check the local `TcpPeerRegistry` first (for
                    // local WebRtcPeers the daemon owns) and fall
                    // through to the `TcpRelayRegistry` (federated
                    // peers the primary relays to). Unknown ufrag =
                    // close with a diagnostic log.
                    //
                    // Local first keeps the existing behavior
                    // unchanged for non-federated topologies;
                    // relay-as-fallback adds the federation relay
                    // path without touching the local fast path.
                    match crate::display::webrtc::parse_first_frame_ufrag(&first_frame) {
                        Some(ufrag) if tcp_peer_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_peer_registry
                                .route_accepted(stream, first_frame, remote_addr)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP local routing for {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) if tcp_relay_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_relay_registry
                                .route_accepted(stream, first_frame)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP relay routing for ufrag={ufrag} from {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: no route for ufrag {ufrag:?} from {remote_addr} \
                                 (neither local peer nor registered relay)"
                            );
                        }
                        None => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: first frame from {remote_addr} isn't a \
                                 STUN binding request with a parseable USERNAME"
                            );
                        }
                    }
                    return;
                }

                let header_text = String::from_utf8_lossy(&buf[..n]);
                let is_websocket = header_text
                    .lines()
                    .any(|l| l.to_lowercase().contains("upgrade: websocket"));

                // Parse the `Host:` header to learn what address the
                // browser thinks reaches us. We use this later as the IP
                // for ICE-TCP host candidates: Firefox refuses to pair
                // remote loopback candidates, so we need a non-loopback
                // address the browser can actually connect to. The only
                // one we know for sure the browser can reach is whatever
                // it just used to reach us for HTTP — which is exactly
                // what the Host header contains. If the user accessed
                // via a hostname (`localhost`, `myserver.local`) rather
                // than a literal IP, we get None here and skip the TCP
                // candidate entirely; those users can still use UDP if
                // their topology allows it.
                let browser_host_ip: Option<std::net::IpAddr> =
                    extract_host_header_ip(&header_text);

                if is_websocket {
                    // Bearer enforcement on /ws — dual-mode (Authorization
                    // header from daemons, ?token= query param from
                    // browsers). Reject with a plain HTTP 401 *before*
                    // the WebSocket handshake so the rejected client
                    // never sees a successful upgrade.
                    if let Err((status, body)) =
                        verify_bearer_for_ws(&header_text, inbound_bearer_token.as_deref())
                    {
                        use tokio::io::AsyncWriteExt;
                        let response = format!(
                            "HTTP/1.1 {status} Unauthorized\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             WWW-Authenticate: Bearer\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        return;
                    }
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

                    // Send cached external_agent_changed so the dropdown
                    // and status badge reflect the current value on a
                    // fresh browser connection.
                    if let Ok(guard) = last_external_agent_json.lock() {
                        if let Some(ref ea_json) = *guard {
                            let _ = direct_tx.send(ea_json.clone());
                        }
                    }

                    // Send cached user_display_granted so the "your display"
                    // status bar toggle reflects the current grant state on
                    // a refreshed browser. Cache is cleared on revoke so
                    // a revoked state simply results in nothing being sent
                    // (the dashboard's HTML default is "off").
                    if let Ok(guard) = last_user_display_json.lock() {
                        if let Some(ref ud_json) = *guard {
                            let _ = direct_tx.send(ud_json.clone());
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
                    // Each JSONL entry is converted to an OutboundEvent via
                    // session_log_entry_to_app_event → app_event_to_outbound
                    // so replay drives the same rendering path as live.
                    if let Some(ref ctx) = query_ctx {
                        let session_jsonl = ctx.log_dir.join("session.jsonl");
                        if let Ok(contents) = std::fs::read_to_string(&session_jsonl) {
                            let replay = serde_json::json!({
                                "t": "log_replay",
                                "entries": replay_jsonl_to_outbound_entries(&contents, &ctx.log_dir),
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
                    let terminal_registry_inbound = terminal_registry.clone();
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
                                        Some("annotation_attach") => {
                                            // User clicked "Attach" on an annotation/frame: register
                                            // the JPEG in the frame registry but DO NOT inject into
                                            // the agent context. The browser tracks this frame ID as
                                            // a pending attachment and submits it with the next task.
                                            //
                                            // Works regardless of presence/agent state — attachments
                                            // are independent of any running task.
                                            let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"].as_str().unwrap_or("annotation").to_string();
                                            let note = json["note"].as_str().unwrap_or("").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                                    let mut saved_path = String::new();
                                                    let mut registered = false;
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
                                                            Ok(path) => {
                                                                saved_path = path.display().to_string();
                                                                registered = true;
                                                            }
                                                            Err(e) => eprintln!("annotation_attach frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    let _ = direct_tx_inbound.send(serde_json::json!({
                                                        "t": "annotation_attached",
                                                        "frame_id": frame_id,
                                                        "stream": stream,
                                                        "path": saved_path,
                                                        "note": note,
                                                        "ok": registered,
                                                    }).to_string());
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} attached (pending)",
                                                            frame_id
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
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
                                                    let mut injected_to_queue = false;
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
                                                                        source: crate::event::InjectionSource::User,
                                                                        steer_id: None,
                                                                    });
                                                                    injected_to_queue = true;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    // Send path back to browser. Report whether the injection
                                                    // actually landed in the queue (not just whether the user
                                                    // pressed Send), so the UI doesn't lie when no presence is
                                                    // running.
                                                    let _ = direct_tx_inbound.send(serde_json::json!({
                                                        "t": "annotation_saved",
                                                        "frame_id": frame_id,
                                                        "path": saved_path,
                                                        "injected": injected_to_queue,
                                                    }).to_string());
                                                    let status_label = if inject {
                                                        if injected_to_queue {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved — no agent connected)"
                                                        }
                                                    } else {
                                                        ""
                                                    };
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} on {}{}",
                                                            frame_id, stream, status_label
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
                                                                    source: crate::event::InjectionSource::User,
                                                                    steer_id: None,
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
                                                    // Combine the Host-header IP with the
                                                    // port we want to advertise (HTTP port
                                                    // for Phase 3 multiplex, or standalone
                                                    // Phase 2 port) to form the single TCP
                                                    // candidate the peer will emit. None
                                                    // if either piece is missing (typically
                                                    // because the browser connected via
                                                    // hostname).
                                                    let tcp_advertised_addr: Option<std::net::SocketAddr> =
                                                        match (browser_host_ip, tcp_advertised_port) {
                                                            (Some(ip), Some(port)) => {
                                                                Some(std::net::SocketAddr::new(ip, port))
                                                            }
                                                            _ => None,
                                                        };
                                                    match session.handle_offer(
                                                        peer_id,
                                                        &sdp,
                                                        &ice_config,
                                                        Some(Arc::clone(&tcp_peer_registry)),
                                                        tcp_advertised_addr,
                                                        ice_tx,
                                                    ).await {
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
                                        Some("terminal_open") => {
                                            // {"t":"terminal_open","host_id":"local","terminal_id":"shell-0","cols":80,"rows":24}
                                            let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                                            let terminal_id = json["terminal_id"].as_str().unwrap_or("shell-0").to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey { host_id: host_id.clone(), terminal_id: terminal_id.clone() };

                                            match terminal_registry_inbound.open_or_attach(key.clone(), cols, rows).await {
                                                Ok(session) => {
                                                    // Spawn a forwarder task that drains the session's
                                                    // per-listener channel and sends base64-encoded
                                                    // output to this WS connection.
                                                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                                                    session.attach(tx);

                                                    let forwarder_tx = direct_tx_inbound.clone();
                                                    let fwd_host = host_id.clone();
                                                    let fwd_term = terminal_id.clone();
                                                    tokio::spawn(async move {
                                                        use base64::Engine as _;
                                                        while let Some(event) = rx.recv().await {
                                                            let msg = match event {
                                                                crate::terminal::TerminalEvent::Output(bytes) => {
                                                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                                                    serde_json::json!({
                                                                        "t": "terminal_output",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "data": b64,
                                                                    })
                                                                }
                                                                crate::terminal::TerminalEvent::Exited { status } => {
                                                                    serde_json::json!({
                                                                        "t": "terminal_exited",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "status": status,
                                                                    })
                                                                }
                                                            };
                                                            if forwarder_tx.send(msg.to_string()).is_err() {
                                                                break;
                                                            }
                                                        }
                                                    });

                                                    let ack = serde_json::json!({
                                                        "t": "terminal_opened",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                    });
                                                    let _ = direct_tx_inbound.send(ack.to_string());
                                                }
                                                Err(e) => {
                                                    let err = serde_json::json!({
                                                        "t": "terminal_error",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                        "error": e,
                                                    });
                                                    let _ = direct_tx_inbound.send(err.to_string());
                                                }
                                            }
                                        }
                                        Some("terminal_input") => {
                                            // {"t":"terminal_input","host_id":"local","terminal_id":"shell-0","data":"<base64>"}
                                            let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                                            let terminal_id = json["terminal_id"].as_str().unwrap_or("shell-0").to_string();
                                            let data_b64 = json["data"].as_str().unwrap_or("");
                                            use base64::Engine as _;
                                            if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                                let key = crate::terminal::TerminalKey { host_id, terminal_id };
                                                if let Some(session) = terminal_registry_inbound.get(&key).await {
                                                    session.write_input(&data);
                                                }
                                            }
                                        }
                                        Some("terminal_resize") => {
                                            // {"t":"terminal_resize","host_id":"local","terminal_id":"shell-0","cols":N,"rows":N}
                                            let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                                            let terminal_id = json["terminal_id"].as_str().unwrap_or("shell-0").to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey { host_id, terminal_id };
                                            if let Some(session) = terminal_registry_inbound.get(&key).await {
                                                session.resize(cols, rows);
                                            }
                                        }
                                        Some("terminal_close") => {
                                            // {"t":"terminal_close","host_id":"local","terminal_id":"shell-0"}
                                            let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                                            let terminal_id = json["terminal_id"].as_str().unwrap_or("shell-0").to_string();
                                            let key = crate::terminal::TerminalKey { host_id, terminal_id };
                                            terminal_registry_inbound.close(&key).await;
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
                                            // Fall through to ControlMsg parsing.
                                            // WebRtcSignal needs special handling because
                                            // it requires session_registry / direct_tx
                                            // access for the response leg; everything else
                                            // gets re-broadcast as AppEvent::ControlCommand
                                            // for the agent loop / TUI / MCP consumers.
                                            match serde_json::from_value::<ControlMsg>(json) {
                                                Ok(ControlMsg::WebRtcSignal { display_id, session_id, signal }) => {
                                                    handle_federated_webrtc_signal(
                                                        display_id,
                                                        session_id,
                                                        signal,
                                                        session_registry_inbound.as_ref(),
                                                        &ice_config,
                                                        Arc::clone(&tcp_peer_registry),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                    ).await;
                                                }
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

                    // Federation auth enforcement. Applied before any
                    // federation API branch in the dispatch chain
                    // below; non-federation paths (WASM, frames,
                    // dashboard HTML, /config, /.well-known, /ws,
                    // /static/*) sail through unauthenticated. See
                    // `is_federation_path` for the exact set and the
                    // `inbound_bearer_token` docs on `spawn_web_gateway`
                    // for the design rationale.
                    if is_federation_path(request_line) {
                        if let Err((status, body)) =
                            verify_bearer_token(&header_text, inbound_bearer_token.as_deref())
                        {
                            use tokio::io::AsyncWriteExt;
                            let reason = match status {
                                401 => "Unauthorized",
                                _ => "Error",
                            };
                            let response = format!(
                                "HTTP/1.1 {status} {reason}\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: no-cache\r\n\
                                 WWW-Authenticate: Bearer\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {body}",
                                body.len(),
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            return;
                        }
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
                    } else if request_line.starts_with("POST") && request_line.contains(" /api/session/current/uploads") {
                        // POST /api/session/current/uploads?name=<fn>&destination=task|workspace
                        //   Content-Type: <mime>
                        //   <raw bytes>
                        //
                        // Streams the body into a tempfile, commits it into
                        // the upload store (per-session `uploads/` or
                        // per-project `workspace_files/`), and broadcasts
                        // UploadReady so all connected browsers see it.
                        //
                        // Route sits in the `/api/session/current/*` family
                        // alongside `changes`, `history`, `rollback`, etc.
                        // That namespace is browser-session managed — not
                        // part of `is_federation_path`, so bearer-token auth
                        // doesn't apply. If a WAN-exposed deploy wants to
                        // protect uploads, gate the whole family at once.
                        use tokio::io::AsyncWriteExt;
                        let response = 'upload: {
                            let Some(ref slog) = session_log else {
                                break 'upload upload_error_response(
                                    "400 Bad Request",
                                    "no active session",
                                );
                            };
                            let Some(ref root) = project_root_for_changes else {
                                break 'upload upload_error_response(
                                    "400 Bad Request",
                                    "no project root",
                                );
                            };

                            let name = query_param(&request_line, "name")
                                .unwrap_or_else(|| "upload.bin".to_string());
                            let destination = query_param(&request_line, "destination")
                                .as_deref()
                                .and_then(crate::upload_store::UploadDestination::from_str)
                                .unwrap_or(crate::upload_store::UploadDestination::Task);
                            let mime = content_type_header(&header_text);

                            match stream_body_to_tempfile(
                                &header_text,
                                &mut stream,
                                UPLOAD_MAX_BYTES,
                            )
                            .await
                            {
                                Err(e) => {
                                    let status = if e.contains("too large") {
                                        "413 Payload Too Large"
                                    } else {
                                        "400 Bad Request"
                                    };
                                    break 'upload upload_error_response(status, &e);
                                }
                                Ok((tmp, size)) => {
                                    let (session_dir, session_id) = {
                                        match slog.lock() {
                                            Ok(l) => (
                                                l.dir().to_path_buf(),
                                                l.session_id().to_string(),
                                            ),
                                            Err(_) => {
                                                break 'upload upload_error_response(
                                                    "500 Internal Server Error",
                                                    "session log lock poisoned",
                                                );
                                            }
                                        }
                                    };
                                    match crate::upload_store::commit_upload(
                                        tmp,
                                        &name,
                                        &mime,
                                        size as u64,
                                        destination,
                                        &session_dir,
                                        &session_id,
                                        root,
                                    ) {
                                        Ok(descriptor) => {
                                            bus.send(crate::event::AppEvent::UploadReady {
                                                descriptor: descriptor.clone(),
                                            });
                                            let body = serde_json::to_string(&descriptor)
                                                .unwrap_or_else(|_| "{}".to_string());
                                            format!(
                                                "HTTP/1.1 200 OK\r\n\
                                                 Content-Type: application/json\r\n\
                                                 Content-Length: {}\r\n\
                                                 Cache-Control: no-cache\r\n\
                                                 Access-Control-Allow-Origin: *\r\n\
                                                 Connection: close\r\n\
                                                 \r\n\
                                                 {}",
                                                body.len(),
                                                body
                                            )
                                        }
                                        Err(e) => upload_error_response(
                                            "500 Internal Server Error",
                                            &format!("commit upload: {e}"),
                                        ),
                                    }
                                }
                            }
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET") && request_line.contains(" /api/session/current/uploads") {
                        // GET /api/session/current/uploads           — list uploads for the current session
                        // GET /api/session/current/uploads/<id>/raw  — stream bytes of one upload
                        use tokio::io::AsyncWriteExt;
                        let response = 'get_upload: {
                            let Some(ref slog) = session_log else {
                                break 'get_upload upload_error_response(
                                    "404 Not Found",
                                    "no active session",
                                );
                            };
                            let Some(ref root) = project_root_for_changes else {
                                break 'get_upload upload_error_response(
                                    "404 Not Found",
                                    "no project root",
                                );
                            };
                            let session_dir = match slog.lock() {
                                Ok(l) => l.dir().to_path_buf(),
                                Err(_) => {
                                    break 'get_upload upload_error_response(
                                        "500 Internal Server Error",
                                        "session log lock poisoned",
                                    );
                                }
                            };
                            // Path after /api/session/current/uploads
                            let path_and_q = request_line
                                .split_whitespace()
                                .nth(1)
                                .unwrap_or("");
                            let path = path_and_q.splitn(2, '?').next().unwrap_or("");
                            let suffix = path
                                .trim_start_matches("/api/session/current/uploads")
                                .trim_matches('/');
                            if suffix.is_empty() {
                                let uploads = crate::upload_store::list_uploads(&session_dir, root);
                                let body = serde_json::to_string(&uploads)
                                    .unwrap_or_else(|_| "[]".to_string());
                                format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Access-Control-Allow-Origin: *\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                )
                            } else if let Some(id) = suffix.strip_suffix("/raw") {
                                // GET raw bytes for one upload.
                                match crate::upload_store::find_upload(id, &session_dir, root) {
                                    None => upload_error_response(
                                        "404 Not Found",
                                        "upload not found",
                                    ),
                                    Some(d) => {
                                        match std::fs::read(&d.path) {
                                            Ok(bytes) => {
                                                let header = format!(
                                                    "HTTP/1.1 200 OK\r\n\
                                                     Content-Type: {}\r\n\
                                                     Content-Length: {}\r\n\
                                                     Content-Disposition: inline; filename=\"{}\"\r\n\
                                                     Cache-Control: no-cache\r\n\
                                                     Access-Control-Allow-Origin: *\r\n\
                                                     Connection: close\r\n\
                                                     \r\n",
                                                    d.mime,
                                                    bytes.len(),
                                                    d.name.replace('"', ""),
                                                );
                                                let _ = stream.write_all(header.as_bytes()).await;
                                                let _ = stream.write_all(&bytes).await;
                                                // Skip the trailing write_all below.
                                                break 'get_upload String::new();
                                            }
                                            Err(e) => upload_error_response(
                                                "500 Internal Server Error",
                                                &format!("read upload: {e}"),
                                            ),
                                        }
                                    }
                                }
                            } else {
                                upload_error_response(
                                    "404 Not Found",
                                    "unknown upload route",
                                )
                            }
                        };
                        if !response.is_empty() {
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.starts_with("DELETE")
                        && request_line.contains(" /api/session/current/uploads/")
                    {
                        // DELETE /api/session/current/uploads/<id> — remove the file + sidecar.
                        use tokio::io::AsyncWriteExt;
                        let response = 'del_upload: {
                            let Some(ref slog) = session_log else {
                                break 'del_upload upload_error_response(
                                    "404 Not Found",
                                    "no active session",
                                );
                            };
                            let Some(ref root) = project_root_for_changes else {
                                break 'del_upload upload_error_response(
                                    "404 Not Found",
                                    "no project root",
                                );
                            };
                            let session_dir = match slog.lock() {
                                Ok(l) => l.dir().to_path_buf(),
                                Err(_) => {
                                    break 'del_upload upload_error_response(
                                        "500 Internal Server Error",
                                        "session log lock poisoned",
                                    );
                                }
                            };
                            let path_and_q = request_line
                                .split_whitespace()
                                .nth(1)
                                .unwrap_or("");
                            let path = path_and_q.splitn(2, '?').next().unwrap_or("");
                            let id = path
                                .trim_start_matches("/api/session/current/uploads/")
                                .trim_matches('/');
                            if id.is_empty() {
                                break 'del_upload upload_error_response(
                                    "400 Bad Request",
                                    "missing upload id",
                                );
                            }
                            match crate::upload_store::delete_upload(id, &session_dir, root) {
                                Ok(_) => {
                                    bus.send(crate::event::AppEvent::UploadDeleted {
                                        id: id.to_string(),
                                    });
                                    let body = serde_json::json!({"ok": true}).to_string();
                                    format!(
                                        "HTTP/1.1 200 OK\r\n\
                                         Content-Type: application/json\r\n\
                                         Content-Length: {}\r\n\
                                         Cache-Control: no-cache\r\n\
                                         Access-Control-Allow-Origin: *\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    )
                                }
                                Err(e) => upload_error_response(
                                    "500 Internal Server Error",
                                    &format!("delete: {e}"),
                                ),
                            }
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET") && request_line.contains("/api/session/current/changes") {
                        // File change tracking endpoints:
                        //   GET /api/session/current/changes        — list all changed files
                        //   GET /api/session/current/changes/{path} — unified diff for one file
                        use tokio::io::AsyncWriteExt;
                        let body = handle_changes_request(
                            &request_line,
                            snapshot_dir.as_deref(),
                            project_root_for_changes.as_deref(),
                        );
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
                    } else if request_line.starts_with("GET") && request_line.contains("/api/session/current/history") {
                        // GET /api/session/current/history — serialized History.
                        use tokio::io::AsyncWriteExt;
                        let (status, body) = handle_history_get(file_watcher.as_ref()).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status, body.len(), body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains("/api/session/current/rollback") {
                        // POST /api/session/current/rollback body:
                        //   {"round_id": N,
                        //    "revert_files": bool (default true),
                        //    "revert_conversation": bool (default false)}
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let body_text = read_post_body(&header_text, &mut stream).await;
                        let agent_state = query_ctx
                            .as_ref()
                            .map(|ctx| ctx.agent_state.clone());
                        let (status, body) = handle_history_rollback(
                            &body_text,
                            file_watcher.as_ref(),
                            agent_state.as_ref(),
                            &bus,
                        ).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status, body.len(), body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains("/api/session/current/redo") {
                        // POST /api/session/current/redo — no body required.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let _ = read_post_body(&header_text, &mut stream).await;
                        let agent_state = query_ctx
                            .as_ref()
                            .map(|ctx| ctx.agent_state.clone());
                        let (status, body) = handle_history_redo(
                            file_watcher.as_ref(),
                            agent_state.as_ref(),
                        ).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status, body.len(), body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains("/api/session/current/prune") {
                        // POST /api/session/current/prune — no body required.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let _ = read_post_body(&header_text, &mut stream).await;
                        let (status, body) = handle_history_prune(file_watcher.as_ref()).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status, body.len(), body,
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
                        } else if rest_parts.len() >= 2 && rest_parts[1] == "report" {
                            // GET /api/session/{id}/report — download a zip of
                            // the current session's text artifacts for sharing
                            // with the dev. Pass id="current" to target the
                            // live daemon's own session via WebQueryCtx.
                            use tokio::io::AsyncWriteExt;
                            let session_id = rest_parts[0];
                            let resolved_dir: Option<PathBuf> = if session_id == "current" {
                                query_ctx.as_ref().map(|ctx| ctx.log_dir.clone())
                            } else {
                                resolve_session_dir(session_id)
                            };
                            match resolved_dir {
                                Some(dir) => match build_session_report_zip(&dir) {
                                    Ok(bytes) => {
                                        let fname = dir
                                            .file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "session".to_string());
                                        let header = format!(
                                            "HTTP/1.1 200 OK\r\n\
                                             Content-Type: application/zip\r\n\
                                             Content-Length: {}\r\n\
                                             Content-Disposition: attachment; filename=\"intendant-session-{}.zip\"\r\n\
                                             Cache-Control: no-cache\r\n\
                                             Connection: close\r\n\
                                             \r\n",
                                            bytes.len(),
                                            fname
                                        );
                                        let _ = stream.write_all(header.as_bytes()).await;
                                        let _ = stream.write_all(&bytes).await;
                                    }
                                    Err(e) => {
                                        let body = format!("Failed to build report: {}", e);
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
                                },
                                None => {
                                    let body = "Session not found";
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
                        let displays =
                            crate::display::enumerate_displays_with_sessions(&session_registry)
                                .await;
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
                    } else if request_line.contains(" /api/peers") {
                        // Peer registry endpoints. Dispatch:
                        //   GET    /api/peers                  → list
                        //   POST   /api/peers                  → add
                        //   DELETE /api/peers                  → remove
                        //   POST   /api/peers/{id}/message     → send message
                        //   POST   /api/peers/{id}/task        → delegate task
                        //   POST   /api/peers/{id}/approval    → resolve approval
                        //
                        // When no registry is wired in (test call sites
                        // that pass None), every request returns 503 so
                        // the dashboard can render "peers unavailable"
                        // instead of the empty list that a working-but-
                        // empty registry would produce.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};

                        // Extract subpath after `/api/peers`. The list/
                        // add/remove ops have an empty subpath; per-peer
                        // ops have `/{id}/{op}`. Extract the *path*
                        // token from the request line first (the second
                        // whitespace-separated word) — splitting on
                        // `/api/peers` directly would walk into the
                        // ` HTTP/1.1` suffix and mistake `HTTP` and `1.1`
                        // for path segments.
                        let path_token = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("");
                        // Split path from query string. `/api/peers/eligible
                        // ?capability=display` needs the query stripped before
                        // we extract subpath segments.
                        let (path, query_str) = match path_token.find('?') {
                            Some(i) => (&path_token[..i], &path_token[i + 1..]),
                            None => (path_token, ""),
                        };
                        let subpath = path
                            .strip_prefix("/api/peers")
                            .unwrap_or("")
                            .trim_start_matches('/');
                        let segments: Vec<&str> =
                            subpath.split('/').filter(|s| !s.is_empty()).collect();

                        let (status, body) = match peer_registry.as_ref() {
                            None => (
                                503,
                                serde_json::json!({
                                    "error": "peer registry not configured"
                                })
                                .to_string(),
                            ),
                            Some(registry)
                                if segments.is_empty()
                                    && request_line.starts_with("GET") =>
                            {
                                (200, peers_list_response_body(registry))
                            }
                            Some(registry)
                                if segments.is_empty()
                                    && (request_line.starts_with("POST")
                                        || request_line.starts_with("DELETE")) =>
                            {
                                let body_text = read_request_body(
                                    &mut stream,
                                    &header_text,
                                )
                                .await;
                                if request_line.starts_with("POST") {
                                    peers_add(registry, &body_text).await
                                } else {
                                    peers_remove(registry, &body_text).await
                                }
                            }
                            Some(registry)
                                if segments == ["eligible"]
                                    && request_line.starts_with("GET") =>
                            {
                                // GET /api/peers/eligible?capability=display
                                // — list peers that satisfy all listed
                                // capabilities. The `eligible` segment is
                                // a reserved sub-path on /api/peers; an
                                // actual peer with that bare id would be
                                // shadowed here, but PeerId values always
                                // carry a `<kind>:` prefix so that's not
                                // a real collision.
                                peers_eligible(registry, query_str)
                            }
                            Some(registry)
                                if segments.len() == 2
                                    && request_line.starts_with("POST") =>
                            {
                                let id = segments[0];
                                let op = segments[1];
                                let body_text = read_request_body(
                                    &mut stream,
                                    &header_text,
                                )
                                .await;
                                match op {
                                    "message" => {
                                        peers_send_message(registry, id, &body_text).await
                                    }
                                    "task" => {
                                        peers_delegate_task(registry, id, &body_text).await
                                    }
                                    "approval" => {
                                        peers_resolve_approval(registry, id, &body_text)
                                            .await
                                    }
                                    "webrtc" => {
                                        peers_webrtc_signal(registry, id, &body_text, &bus).await
                                    }
                                    other => (
                                        404,
                                        serde_json::json!({
                                            "error": format!(
                                                "unknown peer op: {other}"
                                            )
                                        })
                                        .to_string(),
                                    ),
                                }
                            }
                            Some(_) => (
                                405,
                                serde_json::json!({
                                    "error": "method not allowed"
                                })
                                .to_string(),
                            ),
                        };
                        let reason = match status {
                            200 => "OK",
                            400 => "Bad Request",
                            404 => "Not Found",
                            405 => "Method Not Allowed",
                            500 => "Internal Server Error",
                            502 => "Bad Gateway",
                            503 => "Service Unavailable",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n\
                             Access-Control-Allow-Headers: Content-Type\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains(" /api/coordinator/route") {
                        // POST /api/coordinator/route — capability-based
                        // task routing through the Coordinator primitive.
                        // Body shape: {"required_capabilities": ["display",
                        // ...], "task": {"instructions": "...", "context":
                        // ..., "client_correlation_id": "..."}}.
                        // Response: {"peer_id": "...", "task_id": "..."}
                        // on success, structured error otherwise.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let (status, body) = match peer_registry.as_ref() {
                            None => (
                                503,
                                serde_json::json!({
                                    "error": "peer registry not configured"
                                })
                                .to_string(),
                            ),
                            Some(_) if !request_line.starts_with("POST") => (
                                405,
                                serde_json::json!({
                                    "error": "method not allowed"
                                })
                                .to_string(),
                            ),
                            Some(registry) => {
                                let body_text =
                                    read_request_body(&mut stream, &header_text).await;
                                coordinator_route(registry, &body_text).await
                            }
                        };
                        let reason = match status {
                            200 => "OK",
                            400 => "Bad Request",
                            404 => "Not Found",
                            405 => "Method Not Allowed",
                            500 => "Internal Server Error",
                            502 => "Bad Gateway",
                            503 => "Service Unavailable",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Access-Control-Allow-Methods: POST, OPTIONS\r\n\
                             Access-Control-Allow-Headers: Content-Type\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/sessions") {
                        // Session listing endpoint. CORS `*` so the
                        // multi-host Stats tab can fetch sibling
                        // daemons' session lists to populate its "All
                        // Sessions" and "Disk Usage" cards per host.
                        let body = list_sessions();
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
                        } else if request_line.contains("/.well-known/agent-card.json") {
                            // Canonical peer identity + capability surface.
                            // Served alongside /config so the browser and
                            // federated peers can discover who this daemon
                            // is without parsing the voice-runtime config.
                            ("application/json", agent_card_json.clone(), "no-cache")
                        } else if request_line.contains("/config") {
                            ("application/json", config_json.clone(), "no-cache")
                        } else {
                            // Default: serve app.html (also matches /app for backward compat)
                            ("text/html; charset=utf-8", app_html.to_string(), "no-cache")
                        };

                        // CORS: allow the multi-host dashboard to
                        // `fetch()` /config and /.well-known/agent-card.json
                        // on this daemon from a page served by a sibling
                        // daemon (cross-origin). `*` works because our
                        // fetches don't send credentials.
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: {}\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
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
///
/// Returns voice/runtime fields only. Daemon identity (host label,
/// version, git sha) lives on the Agent Card at
/// `/.well-known/agent-card.json` and is assembled at gateway spawn
/// time via [`build_local_agent_card`].
pub fn build_config(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_config: crate::display::IceConfig,
) -> WebGatewayConfig {
    build_config_inner(
        live_provider,
        live_model,
        transcription_enabled,
        ice_config.ice_servers,
    )
}

// ---------------------------------------------------------------------------
// /api/peers helpers
// ---------------------------------------------------------------------------

/// Wrapper for the `GET /api/peers` JSON body.
///
/// Each entry is a [`crate::peer::PeerSnapshot`] — the same type the
/// registry's push events carry. One snapshot type means the dashboard
/// applies API entries and pushed deltas the same way; no parallel
/// schemas to drift apart.
#[derive(Serialize)]
struct PeerListResponse {
    peers: Vec<crate::peer::PeerSnapshot>,
}

#[derive(Deserialize)]
struct AddPeerRequest {
    card_url: String,
    /// Optional connecting-side override for the peer's transport
    /// URLs. When non-empty, the card's `transports` field is
    /// replaced with one `IntendantWs` entry per URL. Lets the
    /// operator route around topologies the advertising peer's card
    /// doesn't know about (port-forwards, proxies, named tunnels).
    /// `#[serde(default)]` so older clients without this field
    /// continue to work.
    #[serde(default)]
    via_urls: Vec<String>,
    /// Optional outbound bearer token sent to this peer (the
    /// `[[peer]] bearer_token` equivalent for dashboard-added
    /// peers). When set, sent on the agent-card fetch and the
    /// WebSocket upgrade. Required when the peer's card declares
    /// `auth.application = Some(Bearer)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer_token: Option<String>,
    /// Optional operator-supplied pinned cert fingerprints. When
    /// non-empty, REPLACES whatever the peer's card declares for
    /// `auth.transport` — eliminates the TOFU window when the
    /// operator got the fingerprint out-of-band. Same wire format
    /// as the card's: lowercase hex with optional `:` separators.
    #[serde(default)]
    pinned_fingerprints: Vec<String>,
    /// Explicit URL the **browser** uses to reach this peer's HTTP
    /// port for WebRTC ICE-TCP. When set, the dashboard uses this
    /// (not `d.ws_url`) as the `advertise_tcp_via_url` hint in the
    /// federated WebRTC offer. Decouples the browser-side URL from
    /// the via URL the primary uses for federation, which matters
    /// when the two network positions differ (primary-side localhost
    /// tunnel, browser on a different machine, etc.). `None` falls
    /// back to the slice 3a.2 behavior of using the primary's via URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser_tcp_via_url: Option<String>,
}

#[derive(Deserialize)]
struct RemovePeerRequest {
    peer_id: String,
}

/// Build the JSON body for `GET /api/peers`. Cheap — takes a
/// snapshot of the registry's handles and reads their current
/// watch-backed connection/status values. Handles are cloneable so
/// no lock is held across the serialization.
///
/// Each snapshot is built via [`crate::peer::PeerHandle::snapshot`], the
/// same constructor used by the registry's push event stream. The
/// dashboard applies an API entry and a pushed snapshot identically.
fn peers_list_response_body(registry: &crate::peer::PeerRegistry) -> String {
    let handles = registry.list();
    let peers: Vec<crate::peer::PeerSnapshot> = handles.iter().map(|h| h.snapshot()).collect();
    serde_json::to_string(&PeerListResponse { peers })
        .unwrap_or_else(|_| "{\"peers\":[]}".to_string())
}

/// Handle a `POST /api/peers` body: parse, call
/// `PeerRegistry::add_peer`, return `(status_code, body_json)`.
async fn peers_add(
    registry: &crate::peer::PeerRegistry,
    body_text: &str,
) -> (u16, String) {
    let req: AddPeerRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };
    match registry
        .add_peer_with_credentials(
            &req.card_url,
            req.via_urls,
            req.bearer_token,
            req.pinned_fingerprints,
            req.browser_tcp_via_url,
        )
        .await
    {
        Ok(peer_id) => (
            200,
            serde_json::json!({"peer_id": peer_id.as_str()}).to_string(),
        ),
        Err(e) => (
            502,
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// Handle a `DELETE /api/peers` body: parse, call
/// `PeerRegistry::remove_peer`, return `(status_code, body_json)`.
async fn peers_remove(
    registry: &crate::peer::PeerRegistry,
    body_text: &str,
) -> (u16, String) {
    let req: RemovePeerRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };
    let id = crate::peer::PeerId(req.peer_id);
    match registry.remove_peer(&id).await {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotFound(_)) => (
            404,
            serde_json::json!({"error": "peer not found"}).to_string(),
        ),
        Err(e) => (
            500,
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// Read the body of an HTTP request from `stream`, given the already-
/// peeked `header_text` (which may include a partial body in its
/// trailing portion after the `\r\n\r\n` delimiter). Returns the body
/// as an owned `String`.
///
/// Reads exactly `Content-Length` bytes total — the prefix already
/// in `header_text` plus any remainder still in the socket. Returns
/// an empty string when no `Content-Length` header is present.
///
/// Factored out of the original inline body-reading block in the
/// `/api/peers` handler so the per-peer outbound op handlers below
/// can share it without duplicating the peek-then-stream pattern.
async fn read_request_body(
    stream: &mut tokio::net::TcpStream,
    header_text: &str,
) -> String {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if content_length == 0 {
        return String::new();
    }
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return peeked_body[..content_length].to_string();
    }
    let remaining = content_length.saturating_sub(peeked_body.len());
    let mut full = peeked_body.to_string();
    let mut rest = vec![0u8; remaining];
    if stream.read_exact(&mut rest).await.is_ok() {
        full.push_str(&String::from_utf8_lossy(&rest));
    }
    full
}

// ---------------------------------------------------------------------------
// Per-peer outbound op handlers
// ---------------------------------------------------------------------------
//
// These three endpoints let the dashboard drive the read-write peer
// transport directly. Each maps a JSON body to the matching
// [`crate::peer::PeerHandle`] method:
//
//   POST /api/peers/{id}/message  →  PeerHandle::send_message
//   POST /api/peers/{id}/task     →  PeerHandle::delegate_task
//   POST /api/peers/{id}/approval →  PeerHandle::resolve_approval
//
// Error model (uniform across the three):
//
//   400  bad JSON / missing required field
//   404  peer not in registry
//   405  peer's transport doesn't support this op (UnsupportedCapability)
//   502  transport-level failure (NotConnected, Transport, Auth, …)
//   500  catch-all for unexpected errors
//
// Status codes pick a meaningful HTTP semantic per [`PeerError`] variant
// rather than collapsing everything to 502 — the dashboard renders a
// different message for "wrong peer kind" vs "peer is offline".

/// Shared body for `POST /api/peers/{id}/message`.
///
/// Two equivalent shapes accepted:
///
/// 1. Shorthand: `{"text": "hello"}` — implicit user role + Text content.
/// 2. Full:     `{"role": "user", "content": {"type": "text", "text": "hello"}, "session": null}`.
///
/// The `content` field, when present, wins over `text`. Either `text`
/// or `content` is required; everything else is optional.
#[derive(Deserialize)]
struct SendMessageRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    role: Option<crate::peer::MessageRole>,
    #[serde(default)]
    content: Option<crate::peer::MessageContent>,
    #[serde(default)]
    session: Option<String>,
}

impl SendMessageRequest {
    fn into_message(self) -> Result<crate::peer::PeerMessage, String> {
        let role = self.role.unwrap_or(crate::peer::MessageRole::User);
        let content = match (self.content, self.text) {
            (Some(c), _) => c,
            (None, Some(t)) => crate::peer::MessageContent::Text { text: t },
            (None, None) => {
                return Err("either 'text' or 'content' is required".to_string());
            }
        };
        Ok(crate::peer::PeerMessage {
            session: self.session,
            role,
            content,
        })
    }
}

#[derive(Deserialize)]
struct DelegateTaskRequest {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

#[derive(Deserialize)]
struct ResolveApprovalRequest {
    request_id: String,
    decision: crate::peer::ApprovalDecision,
}

/// Convert a [`crate::peer::PeerError`] into the matching HTTP status +
/// JSON error body. Used by all three per-peer op handlers.
fn peer_error_response(err: crate::peer::PeerError) -> (u16, String) {
    use crate::peer::PeerError;
    let status = match &err {
        PeerError::NotFound(_) => 404,
        PeerError::UnsupportedCapability(_) => 405,
        PeerError::NotConnected
        | PeerError::Transport(_)
        | PeerError::Auth(_)
        | PeerError::CardFetch(_)
        | PeerError::Rejected { .. } => 502,
        _ => 500,
    };
    (
        status,
        serde_json::json!({"error": err.to_string()}).to_string(),
    )
}

/// Look up a peer by id; return 404 + body when absent.
fn peer_handle_or_404(
    registry: &crate::peer::PeerRegistry,
    id: &str,
) -> Result<crate::peer::PeerHandle, (u16, String)> {
    let peer_id = crate::peer::PeerId(id.to_string());
    registry.get(&peer_id).ok_or_else(|| {
        (
            404,
            serde_json::json!({"error": format!("peer not found: {id}")}).to_string(),
        )
    })
}

/// JSON body shape for `POST /api/peers/{id}/webrtc`.
///
/// Single endpoint, signal-discriminated. The dashboard's per-peer
/// `RTCPeerConnection` glue posts every leg of the signaling exchange
/// (Offer, IceCandidate, Close) through this one path, scoped by
/// `display_id` + `session_id`. The peer responds asynchronously
/// via `OutboundEvent::WebRtcSignal` events that the registry
/// forwards to the browser through the existing
/// `OutboundEvent::PeerEventForwarded` channel.
#[derive(Deserialize)]
struct PeerWebRtcSignalRequest {
    display_id: u32,
    session_id: String,
    signal: crate::peer::WebRtcSignal,
}

/// Handle `POST /api/peers/{id}/webrtc`. Routes a WebRTC signaling
/// frame from the browser to the named peer over the federation
/// transport. Returns `200 {"ok": true}` on accepted dispatch, or
/// the standard 4xx/5xx envelope used by the other peer ops.
///
/// The peer's response (Answer, ICE candidates) flows back
/// asynchronously via the registry's per-peer event forwarder —
/// callers don't get the answer in this HTTP response, they
/// observe it on the dashboard's primary `/ws` as a
/// `PeerEventForwarded` whose payload is `PeerEvent::WebRtcSignal`.
async fn peers_webrtc_signal(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
    bus: &EventBus,
) -> (u16, String) {
    // Same source tag as the peer-side handler (see
    // `handle_federated_webrtc_signal`), so filtering the session
    // log on `source == "webrtc-peer"` catches the full signaling
    // conversation across both primary (outbound forward) and peer
    // (inbound handle) — the wire is the same signal, the logs say
    // so.
    const LOG_SOURCE: &str = "webrtc-peer";
    let req: PeerWebRtcSignalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("rejecting webrtc signal from browser — invalid body: {e}"),
                turn: None,
            });
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };
    let signal_kind = match &req.signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "forwarding {signal_kind} from browser to peer={id} display={} session={}",
            req.display_id, req.session_id
        ),
        turn: None,
    });
    let peer_id = crate::peer::PeerId(id.to_string());
    let handle = match registry.get(&peer_id) {
        Some(h) => h,
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} not in registry — dropping {signal_kind}"
                ),
                turn: None,
            });
            return (
                404,
                serde_json::json!({"error": "peer not found"}).to_string(),
            );
        }
    };
    let display_id = req.display_id;
    let session_id_str = req.session_id.clone();
    match handle
        .webrtc_signal(
            req.display_id,
            crate::peer::WebRtcSessionId(req.session_id),
            req.signal,
        )
        .await
    {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotConnected) => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} not connected — dropping {signal_kind} (display={display_id} session={session_id_str})"
                ),
                turn: None,
            });
            (
                502,
                serde_json::json!({"error": "peer is not connected"}).to_string(),
            )
        }
        Err(crate::peer::PeerError::UnsupportedCapability(_)) => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} transport lacks webrtc_signal — dropping {signal_kind}"
                ),
                turn: None,
            });
            (
                502,
                serde_json::json!({
                    "error": "peer's transport does not support WebRTC signaling"
                })
                .to_string(),
            )
        }
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                level: "error".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "webrtc_signal to peer {id} failed: {e}"
                ),
                turn: None,
            });
            (
                500,
                serde_json::json!({"error": e.to_string()}).to_string(),
            )
        }
    }
}

/// Slice 3b: rewrite an outgoing federated `WebRtcSignal::Answer` to
/// (a) register the peer's ICE ufrag in the relay registry and
/// (b) inject a TCP candidate pointing at the primary's own address
/// alongside the peer's direct candidate.
///
/// After the rewrite, a browser receiving this Answer has two TCP
/// candidates: the peer's direct TCP candidate (if the peer provided
/// one via `advertise_tcp_via_url`) and the primary's relay
/// candidate. Browser ICE tries both and uses whichever forms first.
/// Because the relay candidate is emitted with a lower priority
/// (see `inject_relay_tcp_candidate`), direct wins on reachable
/// topologies and relay is the fallback.
///
/// Non-Answer events pass through verbatim. Events with malformed
/// SDPs, missing ufrags, or a peer URL that can't be resolved fall
/// through without rewriting — the browser still sees the original
/// Answer, just without the relay candidate.
async fn maybe_rewrite_federated_answer(
    peer: &crate::peer::PeerId,
    event: crate::peer::PeerEvent,
    registry: &crate::peer::PeerRegistry,
    relay_registry: &Arc<crate::display::webrtc::TcpRelayRegistry>,
    relay_advertise_url: Option<&str>,
    bus: &EventBus,
) -> crate::peer::PeerEvent {
    const LOG_SOURCE: &str = "webrtc-peer";

    // Match only the specific variant that carries an Answer SDP; all
    // other event variants (Log, Usage, ActivityStarted, IceCandidate,
    // etc.) pass through unchanged.
    let (display_id, session_id, sdp) = match &event {
        crate::peer::PeerEvent::WebRtcSignal {
            display_id,
            session_id,
            signal: crate::peer::WebRtcSignal::Answer { sdp },
        } => (*display_id, session_id.clone(), sdp.clone()),
        _ => return event,
    };

    // Extract the peer's ICE ufrag from the Answer SDP. Without it we
    // can't key the relay registry, so we skip rewriting and let the
    // browser try whatever direct candidate the peer advertised.
    let ufrag = match crate::display::webrtc::parse_sdp_ice_ufrag(&sdp) {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     Answer SDP missing a=ice-ufrag attribute"
                ),
                turn: None,
            });
            return event;
        }
    };

    // Resolve the peer's outbound TCP address — where the primary
    // will dial when it sees a relay-destined TCP connection. Prefer
    // `browser_tcp_via_url` (operator's split-browser-side URL) then
    // fall back to `ws_url` (primary-side via URL). In the typical
    // co-located case the two are the same; in split topologies the
    // operator uses browser_tcp_via_url to point at where they'd
    // like the BROWSER to reach the peer. Here we're dialing FROM
    // the primary, but the primary typically shares the LAN position
    // of the operator's browser-reachable URL when one is set.
    let outbound_url = registry.get(peer).and_then(|h| {
        let snap = h.snapshot();
        snap.browser_tcp_via_url.or(snap.ws_url)
    });
    let outbound_url = match outbound_url {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     no outbound URL on the peer's snapshot (peer removed mid-Answer?)"
                ),
                turn: None,
            });
            return event;
        }
    };
    let outbound_addr = match resolve_url_to_socket_addr(&outbound_url).await {
        Some(addr) => addr,
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     outbound URL {outbound_url:?} didn't resolve to a SocketAddr"
                ),
                turn: None,
            });
            return event;
        }
    };
    relay_registry.register(ufrag.clone(), outbound_addr);

    // Resolve the primary's own relay URL into a SocketAddr we can
    // put in an SDP candidate line. When the primary has no
    // advertised URL we can work with (local_addr() was None at
    // spawn, headless mode, etc), skip injection and just forward
    // the Answer unchanged — the browser still has the peer's
    // direct candidate to try.
    let primary_relay_addr = match relay_advertise_url {
        Some(url) => match resolve_url_to_socket_addr(url).await {
            Some(addr) => addr,
            None => {
                bus.send(AppEvent::LogEntry {
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                         but can't inject relay candidate — primary's own URL {url:?} doesn't resolve"
                    ),
                    turn: None,
                });
                return event;
            }
        },
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                     but no primary relay URL configured — skipping candidate injection"
                ),
                turn: None,
            });
            return event;
        }
    };

    let rewritten_sdp =
        crate::display::webrtc::inject_relay_tcp_candidate(&sdp, primary_relay_addr);
    bus.send(AppEvent::LogEntry {
        level: "info".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "relay registered ufrag={ufrag} peer={peer} session={session_id} \
             primary_relay={primary_relay_addr} outbound={outbound_addr}"
        ),
        turn: None,
    });

    crate::peer::PeerEvent::WebRtcSignal {
        display_id,
        session_id,
        signal: crate::peer::WebRtcSignal::Answer { sdp: rewritten_sdp },
    }
}

/// Parse a WebSocket / HTTP URL and resolve it to a [`SocketAddr`].
///
/// Used to convert the browser's view of a peer's HTTP port (the
/// `advertise_tcp_via_url` hint in a federated
/// [`crate::peer::WebRtcSignal::Offer`]) into the concrete address
/// the peer advertises in its ICE-TCP candidate.
///
/// Accepts `ws://` / `wss://` / `http://` / `https://` schemes (all
/// produce the same authority shape). The host can be an IPv4
/// literal, a bracketed IPv6 literal, or a hostname — hostnames are
/// resolved via [`tokio::net::lookup_host`] and the first returned
/// address is used. The port must be explicit; there's no default-
/// port fallback, because we can't know what the peer's HTTP
/// listener bound to without being told.
///
/// Returns `None` on any parse or resolution failure. Callers treat
/// that as "no TCP candidate, UDP-only path" — the same behavior as
/// slice 3a's pre-3a.2 baseline.
async fn resolve_url_to_socket_addr(
    url: &str,
) -> Option<std::net::SocketAddr> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))?;
    // Strip any path / query that follows the authority. Authority
    // for an IPv6 literal is `[::1]:8766`, which contains neither
    // `/` nor `?` inside the brackets, so split-on-first is safe.
    let authority = rest.split(|c| c == '/' || c == '?').next()?;
    // Fast path for `ipv4:port` or `[ipv6]:port`: parse directly.
    if let Ok(addr) = authority.parse::<std::net::SocketAddr>() {
        return Some(addr);
    }
    // Hostname:port — needs DNS. `lookup_host` accepts `host:port`
    // and returns the resolved SocketAddrs in OS-chosen order; first
    // is the winner (matches what the kernel would pick for a
    // regular connect()).
    tokio::net::lookup_host(authority).await.ok()?.next()
}

/// Handle a federation-driven WebRTC signal arriving on this peer's
/// WebSocket inside a [`crate::event::ControlMsg::WebRtcSignal`].
///
/// Routes the signal to the matching `DisplaySession` method and
/// emits responses back over the connection's `direct_tx` as
/// [`crate::types::OutboundEvent::WebRtcSignal`] frames:
///
/// - `Offer` → `DisplaySession::handle_offer` → emit `Answer` + drain
///   the per-session ICE channel emitting `IceCandidate`s as they arrive.
/// - `IceCandidate` → `DisplaySession::add_ice_candidate`. No response.
/// - `Close` → `DisplaySession::remove_peer`. No response.
/// - `Answer` → protocol error (this side is the offer-receiver, not
///   the offer-sender). Logged and ignored.
/// - `Unknown` → forward-compat fallback. Ignored.
///
/// Slice 3a.2 threads the browser's view of the peer's HTTP port
/// through as the ICE-TCP candidate the peer advertises, multiplexed
/// onto its own `TcpPeerRegistry` (same mechanism as the local
/// browser↔daemon display path). When the Offer carries an
/// `advertise_tcp_via_url`, the peer advertises both its UDP host
/// candidates and a TCP candidate at the resolved address — which
/// enables federation WebRTC through any tunnel / port-forward /
/// Tailscale path the operator has already made reachable from the
/// browser. Without the hint (or when the URL can't be resolved),
/// the peer falls back to UDP host candidates only — the 3a baseline.
/// Slice 3b layers primary-as-media-relay on top for the browser-
/// cannot-reach-peer-at-all case.
///
/// `session_id` is round-tripped verbatim into the response so the
/// browser's per-(peer, session_id) `RTCPeerConnection` map can match
/// the answer/candidates back to the right pending session. The local
/// [`crate::display::PeerId`] used as the `WebRtcPeer` key is derived
/// by hashing `session_id` — same string hashes to the same u64, so
/// later `IceCandidate` / `Close` signals route to the same peer.
async fn handle_federated_webrtc_signal(
    display_id: u32,
    session_id: String,
    signal: crate::peer::WebRtcSignal,
    session_registry: Option<&Arc<tokio::sync::RwLock<crate::display::SessionRegistry>>>,
    ice_config: &crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    direct_tx: tokio::sync::mpsc::UnboundedSender<String>,
    bus: &EventBus,
) {
    // Short tag used as the `source` on every log line this handler
    // emits, so the operator can filter the session log to just the
    // federated-WebRTC conversation: `grep 'source":"webrtc-peer"'`.
    // Distinct from the local-display `display_offer` flow (which
    // emits via different codepaths) so logs are unambiguous even
    // when both are active.
    const LOG_SOURCE: &str = "webrtc-peer";

    // Structured signal-kind tag for log messages. The inner
    // `WebRtcSignal` variant name would also work but `Offer`/`Answer`
    // etc. are clearer than the enum's Debug rendering with fields.
    let signal_kind = match &signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "received {signal_kind} from connector (display={display_id} session={session_id})"
        ),
        turn: None,
    });

    let registry = match session_registry {
        Some(r) => r,
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dropping {signal_kind}: no session_registry (display={display_id} session={session_id})"
                ),
                turn: None,
            });
            return;
        }
    };
    let session = match registry.read().await.get(display_id) {
        Some(s) => s,
        None => {
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dropping {signal_kind}: unknown display {display_id} (session {session_id})"
                ),
                turn: None,
            });
            return;
        }
    };

    // Stable PeerId per session_id via DefaultHasher. Same string
    // hashes to the same u64, so subsequent IceCandidate and Close
    // signals route to the same WebRtcPeer in the session's peer map.
    let peer_id: crate::display::PeerId = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        session_id.hash(&mut h);
        h.finish()
    };

    match signal {
        crate::peer::WebRtcSignal::Offer {
            sdp,
            advertise_tcp_via_url,
        } => {
            // Resolve the browser-supplied URL hint to a SocketAddr.
            // Unreachable hostnames / malformed URLs / missing hint
            // all collapse to `None` → UDP-only host candidates, same
            // behavior as pre-3a.2. Wrapped in a single lookup so we
            // don't block handle_offer on DNS per-session.
            let tcp_advertised_addr = match advertise_tcp_via_url.as_deref() {
                Some(url) if !url.is_empty() => resolve_url_to_socket_addr(url).await,
                _ => None,
            };
            bus.send(AppEvent::LogEntry {
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "offer resolved advertise_tcp_via_url={:?} → tcp_candidate={:?}",
                    advertise_tcp_via_url.as_deref().unwrap_or(""),
                    tcp_advertised_addr
                ),
                turn: None,
            });
            // Loopback TCP candidates (127.0.0.1 / ::1) are silently
            // dropped by browsers as anti-rebinding mitigation (same
            // filter documented for the local path at
            // display/webrtc.rs:38-43; the federated path hits the
            // same trap when an operator configures a `localhost:NNNN`
            // tunnel on the primary side but the browser doesn't have
            // a matching loopback tunnel). No observable signaling
            // failure — ICE just silently never pairs. Emit a
            // prominent warn here so operators catch it at the first
            // Offer rather than debugging by inference through
            // "media never forms despite signaling completing."
            if let Some(addr) = tcp_advertised_addr {
                if addr.ip().is_loopback() {
                    bus.send(AppEvent::LogEntry {
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "advertise_tcp_via_url resolved to loopback ({}) — \
                             browsers silently drop remote loopback ICE \
                             candidates (anti-rebinding mitigation), so ICE-TCP \
                             will never pair. Configure the peer's \
                             `browser_tcp_via_url` (slice 3a.4) with a \
                             non-loopback address the browser's machine can \
                             reach (LAN IP, port-forward on a real NIC, \
                             Tailscale URL, etc.) or wait for slice 3b's \
                             primary-as-media-relay fallback.",
                            addr
                        ),
                        turn: None,
                    });
                }
            }
            let (ice_tx, mut ice_rx) =
                tokio::sync::mpsc::channel::<(crate::display::PeerId, String)>(64);
            let answer_result = session
                .handle_offer(
                    peer_id,
                    &sdp,
                    ice_config,
                    Some(tcp_peer_registry.clone()),
                    tcp_advertised_addr,
                    ice_tx,
                )
                .await;
            match answer_result {
                Ok(answer_sdp) => {
                    bus.send(AppEvent::LogEntry {
                        level: "info".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "offer handled, sending answer back to connector (display={display_id} session={session_id} answer_len={} bytes)",
                            answer_sdp.len()
                        ),
                        turn: None,
                    });
                    let answer = crate::types::OutboundEvent::WebRtcSignal {
                        display_id,
                        session_id: session_id.clone(),
                        signal: crate::peer::WebRtcSignal::Answer {
                            sdp: answer_sdp,
                        },
                    };
                    match serde_json::to_string(&answer) {
                        Ok(s) => {
                            if direct_tx.send(s).is_err() {
                                bus.send(AppEvent::LogEntry {
                                    level: "warn".to_string(),
                                    source: LOG_SOURCE.to_string(),
                                    content: format!(
                                        "failed to send answer to connector — direct_tx closed (display={display_id} session={session_id})"
                                    ),
                                    turn: None,
                                });
                            }
                        }
                        Err(e) => {
                            bus.send(AppEvent::LogEntry {
                                level: "error".to_string(),
                                source: LOG_SOURCE.to_string(),
                                content: format!(
                                    "failed to serialize answer (display={display_id} session={session_id}): {e}"
                                ),
                                turn: None,
                            });
                        }
                    }

                    // Drain the per-session ICE channel and forward
                    // server-side trickle candidates as separate
                    // WebRtcSignal frames. Task exits when the
                    // session removes the peer (channel closes).
                    let direct_tx_ice = direct_tx.clone();
                    let session_id_ice = session_id;
                    let bus_ice = bus.clone();
                    tokio::spawn(async move {
                        let mut count: u32 = 0;
                        while let Some((_pid, candidate_json)) = ice_rx.recv().await {
                            count = count.saturating_add(1);
                            let evt = crate::types::OutboundEvent::WebRtcSignal {
                                display_id,
                                session_id: session_id_ice.clone(),
                                signal: crate::peer::WebRtcSignal::IceCandidate {
                                    candidate_json,
                                },
                            };
                            if let Ok(s) = serde_json::to_string(&evt) {
                                if direct_tx_ice.send(s).is_err() {
                                    bus_ice.send(AppEvent::LogEntry {
                                        level: "debug".to_string(),
                                        source: LOG_SOURCE.to_string(),
                                        content: format!(
                                            "ice forwarder exiting — direct_tx closed (display={display_id} session={session_id_ice}) after {count} candidates"
                                        ),
                                        turn: None,
                                    });
                                    break;
                                }
                            }
                        }
                        bus_ice.send(AppEvent::LogEntry {
                            level: "debug".to_string(),
                            source: LOG_SOURCE.to_string(),
                            content: format!(
                                "ice forwarder finished — forwarded {count} candidates (display={display_id} session={session_id_ice})"
                            ),
                            turn: None,
                        });
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "handle_offer failed (display={display_id} session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::IceCandidate { candidate_json } => {
            match session.add_ice_candidate(peer_id, &candidate_json).await {
                Ok(()) => {
                    bus.send(AppEvent::LogEntry {
                        level: "debug".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "applied connector ICE candidate (display={display_id} session={session_id})"
                        ),
                        turn: None,
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "add_ice_candidate failed (display={display_id} session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::Answer { .. } => {
            // Protocol error: this side is the offer-receiver. Browsers
            // send Offers via the primary's federation transport;
            // peers reply with Answers via OutboundEvent::WebRtcSignal.
            // An incoming Answer here means a confused sender — log
            // and drop rather than silently mishandling.
            bus.send(AppEvent::LogEntry {
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "unexpected Answer received on peer side (display={display_id} session={session_id}) — ignoring"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Close => {
            session.remove_peer(peer_id).await;
            bus.send(AppEvent::LogEntry {
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "removed per-session WebRtcPeer on Close (display={display_id} session={session_id})"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Unknown => {
            // Forward-compat fallback for signal kinds added by newer
            // builds. Older daemons silently ignore — but log at
            // debug so the operator can see unknown signal arrivals
            // when they're hunting wire-format issues.
            bus.send(AppEvent::LogEntry {
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "ignoring unknown WebRtcSignal kind (display={display_id} session={session_id})"
                ),
                turn: None,
            });
        }
    }
}

/// Handle `POST /api/peers/{id}/message`.
async fn peers_send_message(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: SendMessageRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };
    let msg = match req.into_message() {
        Ok(m) => m,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": e}).to_string(),
            );
        }
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.send_message(msg).await {
        Ok(message_id) => (
            200,
            serde_json::json!({"message_id": message_id.0}).to_string(),
        ),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `POST /api/peers/{id}/task`.
async fn peers_delegate_task(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: DelegateTaskRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };
    let task = crate::peer::PeerTask {
        instructions: req.instructions,
        context: req.context,
        client_correlation_id: req.client_correlation_id,
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.delegate_task(task).await {
        Ok(task_id) => (
            200,
            serde_json::json!({"task_id": task_id.0}).to_string(),
        ),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `POST /api/peers/{id}/approval`.
async fn peers_resolve_approval(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: ResolveApprovalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.resolve_approval(&req.request_id, req.decision).await {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(e) => peer_error_response(e),
    }
}

// ---------------------------------------------------------------------------
// Coordinator endpoints — capability-based discovery + delegation
// ---------------------------------------------------------------------------

/// Parse `?capability=display&capability=custom:foo` into a typed
/// `Vec<Capability>` plus a list of unknown strings (for diagnostics).
/// Empty input returns `(vec![], vec![])` — empty-required-capabilities
/// matches every peer, which the handler rejects upstream.
fn parse_capability_query(query: &str) -> (Vec<crate::peer::Capability>, Vec<String>) {
    let mut caps = Vec::new();
    let mut unknown = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k != "capability" {
            continue;
        }
        match crate::peer::Capability::from_query_string(v) {
            Some(cap) => caps.push(cap),
            None => unknown.push(v.to_string()),
        }
    }
    (caps, unknown)
}

/// Handle `GET /api/peers/eligible?capability=...`. Returns the
/// connected peers whose Agent Card advertises every requested
/// capability. Each entry is a [`crate::peer::PeerSnapshot`] —
/// same shape as `/api/peers` so the dashboard can reuse rendering.
fn peers_eligible(
    registry: &crate::peer::PeerRegistry,
    query_str: &str,
) -> (u16, String) {
    let (caps, unknown) = parse_capability_query(query_str);
    if !unknown.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": format!(
                    "unrecognized capability values: {}",
                    unknown.join(", ")
                ),
                "hint": "use kebab-case kind names (display, computer-use, ...) or `custom:<name>`"
            })
            .to_string(),
        );
    }
    if caps.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": "at least one ?capability=... is required"
            })
            .to_string(),
        );
    }
    let coordinator = crate::peer::Coordinator::new(registry.clone());
    let peers: Vec<crate::peer::PeerSnapshot> = coordinator
        .eligible_peers(&caps)
        .iter()
        .map(|h| h.snapshot())
        .collect();
    let body = serde_json::json!({ "peers": peers }).to_string();
    (200, body)
}

/// JSON body shape for `POST /api/coordinator/route`.
#[derive(Deserialize)]
struct CoordinatorRouteRequest {
    /// Capabilities the executing peer must advertise. Each string is
    /// parsed via `Capability::from_query_string` for consistency with
    /// the eligible endpoint's URL query (kebab-case + `custom:<name>`).
    required_capabilities: Vec<String>,
    /// Wire-level task payload routed to the winning peer.
    task: CoordinatorRouteTask,
}

#[derive(Deserialize)]
struct CoordinatorRouteTask {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

/// Handle `POST /api/coordinator/route`. Routes the task to a
/// connected peer that satisfies all required capabilities,
/// returning the assigned task id on success or a structured error
/// on no-route / delegation failure.
async fn coordinator_route(
    registry: &crate::peer::PeerRegistry,
    body_text: &str,
) -> (u16, String) {
    let req: CoordinatorRouteRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")})
                    .to_string(),
            );
        }
    };

    // Translate the wire capability strings into typed Capability
    // values. Same parser as the eligible endpoint — keeps the URL
    // and JSON surfaces consistent.
    let mut caps = Vec::with_capacity(req.required_capabilities.len());
    let mut unknown = Vec::new();
    for s in &req.required_capabilities {
        match crate::peer::Capability::from_query_string(s) {
            Some(c) => caps.push(c),
            None => unknown.push(s.clone()),
        }
    }
    if !unknown.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": format!(
                    "unrecognized capability values: {}",
                    unknown.join(", ")
                ),
                "hint": "use kebab-case kind names (display, computer-use, ...) or `custom:<name>`"
            })
            .to_string(),
        );
    }
    if caps.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": "required_capabilities must not be empty"
            })
            .to_string(),
        );
    }

    let task = crate::peer::PeerTask {
        instructions: req.task.instructions,
        context: req.task.context,
        client_correlation_id: req.task.client_correlation_id,
    };
    let coordinator = crate::peer::Coordinator::new(registry.clone());
    let request = crate::peer::TaskRequest {
        required_capabilities: caps,
        task,
    };
    match coordinator.route_task(request).await {
        Ok(routed) => (
            200,
            serde_json::json!({
                "peer_id": routed.peer_id.as_str(),
                "task_id": routed.task_id.0,
            })
            .to_string(),
        ),
        Err(crate::peer::CoordinatorError::NoRoute {
            required,
            considered,
        }) => (
            404,
            serde_json::json!({
                "error": "no route",
                "required_capabilities": required
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>(),
                "considered": considered.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            })
            .to_string(),
        ),
        Err(crate::peer::CoordinatorError::DelegationFailed { peer, error }) => (
            502,
            serde_json::json!({
                "error": format!("delegation to {peer} failed: {error}"),
                "peer_id": peer.as_str(),
            })
            .to_string(),
        ),
    }
}

/// True for HTTP requests that hit the federation REST surface:
/// `/api/peers*`, `/api/coordinator/*`, and `/api/sessions`. These
/// are the endpoints the bearer-token enforcement layer protects
/// when `[server.auth] bearer_token` is set. Discovery
/// (`/.well-known/agent-card.json`), browser bootstrap (`/config`,
/// `/`, `/static/*`), and `/ws` are exempt — see
/// `spawn_web_gateway::inbound_bearer_token` docs for why.
fn is_federation_path(request_line: &str) -> bool {
    request_line.contains(" /api/peers")
        || request_line.contains(" /api/coordinator/")
        || request_line.contains(" /api/sessions")
}

/// Extract a token from the `?token=...` query parameter of an HTTP
/// request line. Used by the WebSocket upgrade auth path because the
/// browser cannot set arbitrary headers on `WebSocket` opens — the
/// dashboard appends `?token=...` to the /ws URL instead.
///
/// `request_line` is the first line of the HTTP request, e.g.
/// `"GET /ws?token=abc HTTP/1.1"`. Returns the extracted token if
/// present, `None` if there's no `?token=` parameter.
pub(crate) fn extract_token_query_param(request_line: &str) -> Option<String> {
    let path_and_query = request_line.split_whitespace().nth(1)?;
    let query = path_and_query.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            // No URL-decoding: bearer tokens are typically URL-safe
            // (hex / base64-url). If a token contains characters that
            // require encoding, the operator can either pick a
            // different token or send via Authorization header (which
            // doesn't have the URL-encoding constraint).
            return Some(value.to_string());
        }
    }
    None
}

/// Verify a WebSocket upgrade request carries the expected bearer
/// token. Browser WebSocket clients cannot natively set custom
/// headers on `WebSocket` opens, so this accepts the token in EITHER
/// an `Authorization: Bearer <token>` header (sent by
/// `IntendantWsTransport` from the daemon side) OR a `?token=...`
/// URL query parameter (sent by the browser dashboard). The dual
/// path is the standard pragmatic workaround for the browser
/// limitation.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token via either method. Returns `Err((401, body))`
/// otherwise — the caller writes a plain HTTP 401 response *before*
/// the WebSocket handshake and returns, so the rejected client never
/// sees a successful upgrade.
pub(crate) fn verify_bearer_for_ws(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };

    // Try the Authorization header first (cheaper and the daemon-to-
    // daemon path uses it). On miss, fall back to the URL query.
    if verify_bearer_token(header_text, Some(expected)).is_ok() {
        return Ok(());
    }

    let request_line = header_text.lines().next().unwrap_or("");
    if extract_token_query_param(request_line).as_deref() == Some(expected) {
        return Ok(());
    }

    Err((
        401,
        serde_json::json!({
            "error": "missing or invalid bearer token (Authorization header or ?token=)"
        })
        .to_string(),
    ))
}

/// Verify a federation HTTP request carries the expected bearer
/// token in the `Authorization` header. Header name lookup is
/// case-insensitive per the HTTP spec; the `Bearer` scheme prefix
/// match accepts either case.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token. Returns `Err((401, body_json))` otherwise —
/// the caller writes that response and returns.
pub(crate) fn verify_bearer_token(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };
    let auth_header = header_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("authorization") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let auth = match auth_header {
        Some(v) => v,
        None => {
            return Err((
                401,
                serde_json::json!({"error": "missing Authorization header"})
                    .to_string(),
            ));
        }
    };
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "));
    let token = match token {
        Some(t) => t.trim(),
        None => {
            return Err((
                401,
                serde_json::json!({
                    "error": "Authorization header must use Bearer scheme"
                })
                .to_string(),
            ));
        }
    };
    if token == expected {
        Ok(())
    } else {
        Err((
            401,
            serde_json::json!({"error": "invalid bearer token"}).to_string(),
        ))
    }
}

/// Resolve the list of WebSocket URLs to advertise in the Agent
/// Card for this daemon, in preference order.
///
/// **Additive auto-detection.** Mirrors WebRTC's host-candidate
/// gathering pattern: the daemon enumerates its own routable
/// interfaces via [`crate::lan::routable_local_addrs`] and emits one
/// URL per address by default, so the operator doesn't need to type
/// their own LAN IP into `--advertise-url`. The operator's overrides
/// (CLI `--advertise-url` or `[server.advertise]` in intendant.toml)
/// are *prepended* — they win on preference order, but the auto-
/// detected entries still ride along as fallbacks. The connecting
/// peer's `MultiTransport::connect` walks the merged list top-down
/// and picks the first that succeeds.
///
/// ## Bind-address rules
///
/// - **Specific bind** (e.g. `192.168.1.42:8765`): only that one IP
///   is auto-detected. The operator narrowed the listener for a
///   reason; we don't second-guess by also enumerating other
///   interfaces.
/// - **Wildcard bind** (`0.0.0.0` / `::`): every routable interface
///   becomes one URL. Loopback is excluded — advertising loopback to
///   remote peers is useless. If the operator wants to expose
///   loopback (e.g. for self-peering tests), they can pass it via
///   `--advertise-url`.
///
/// ## Fallbacks (in order, when auto-detection finds nothing)
///
/// 1. Resolved host label ([`crate::lan::resolve_host_label`]) —
///    works on a trusted LAN with mDNS, fragile elsewhere. Last-
///    ditch best-effort.
/// 2. `ws://localhost:0/ws` if there's no listener at all
///    (shouldn't happen in practice; the listener is always bound by
///    the time spawn is called). Card stays valid; URL won't work.
///
/// Dedup: exact-string match. If the operator's override happens to
/// match an auto-detected URL, only the operator's copy is kept.
pub(crate) fn resolve_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    overrides: &[String],
) -> Vec<String> {
    let port = local_addr.map(|a| a.port()).unwrap_or(0);

    // Auto-detect. Operator overrides come first; auto entries append.
    let auto = auto_detect_advertise_urls(local_addr, port);

    let mut out: Vec<String> = Vec::with_capacity(overrides.len() + auto.len());
    for url in overrides {
        if !out.contains(url) {
            out.push(url.clone());
        }
    }
    for url in auto {
        if !out.contains(&url) {
            out.push(url);
        }
    }

    if out.is_empty() {
        // No bind, no overrides, no interfaces. Card stays valid;
        // URL just won't work until the next daemon restart.
        out.push("ws://localhost:0/ws".to_string());
    }
    out
}

/// Build the auto-detected URL list from the listener bind address.
/// See [`resolve_advertise_urls`] for the full resolution rules.
fn auto_detect_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    port: u16,
) -> Vec<String> {
    use std::net::IpAddr;
    let Some(addr) = local_addr else { return Vec::new() };

    // Specific bind: that one IP wins, no enumeration.
    match addr.ip() {
        IpAddr::V4(v4) if !v4.is_unspecified() => {
            return vec![format_ws_url(&v4.to_string(), port)];
        }
        IpAddr::V6(v6) if !v6.is_unspecified() => {
            return vec![format_ws_url(&format!("[{v6}]"), port)];
        }
        _ => {}
    }

    // Wildcard bind: enumerate every non-loopback routable interface.
    // IPv4 entries sort before IPv6 — WebRTC ICE-TCP in WebKit/WKWebView
    // silently drops IPv6 ULA candidates (seen empirically against
    // fdc2::/8 addresses on macOS 15), so the *first* URL in the list
    // — which slice 3b's `maybe_rewrite_federated_answer` takes as the
    // relay candidate verbatim — needs to be the one browsers actually
    // dial. Within each address family we preserve `getifaddrs` order
    // (`stable_sort_by`), so a multi-NIC host that already had a
    // preferred primary interface keeps it.
    let mut ips = crate::lan::routable_local_addrs(false);
    ips.sort_by(|a, b| match (a, b) {
        (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
        (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut urls: Vec<String> = ips
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V6(v6) => format_ws_url(&format!("[{v6}]"), port),
            ip => format_ws_url(&ip.to_string(), port),
        })
        .collect();

    // No interfaces found (unusual — host with no networking?). Fall
    // back to the resolved host label so the card carries *something*
    // dialable on a trusted LAN with mDNS.
    if urls.is_empty() {
        urls.push(format_ws_url(&crate::lan::resolve_host_label(), port));
    }
    urls
}

fn format_ws_url(host: &str, port: u16) -> String {
    format!("ws://{host}:{port}/ws")
}

/// Assemble the [`crate::peer::AgentCard`] for this daemon from live
/// runtime state.
///
/// Called once per `spawn_web_gateway` invocation, right after the
/// config is serialized — the result is cached as `agent_card_json`
/// and cloned into each per-connection handler, matching the pattern
/// used for `/config`.
///
/// Capabilities:
/// - `ComputerUse`, `Knowledge`, `Display` are always-on subsystems
///   compiled into every build and always able to service a federation
///   request (for `Display`, that's `DisplaySession::handle_offer`
///   against whatever the local dashboard has activated — returns
///   "no such display" if nothing is active, which is the correct
///   semantics for a peer trying to view a display the operator
///   hasn't opened yet).
/// - `Voice` / `Phone` / `Recording` are gated on runtime configuration
///   that isn't plumbed through here yet. Those become additive as
///   each subsystem teaches itself to advertise, likely via dynamic
///   `PeerEvent::CapabilityEngaged` once slice 3a.2 lands.
///
/// `advertise_urls` is the preference-ordered list of WebSocket URLs
/// peers should try when dialing this daemon. Each becomes a
/// [`crate::peer::TransportSpec::IntendantWs`] entry in the card.
/// Built by [`resolve_advertise_urls`], which merges operator
/// overrides (`--advertise-url`, `[server.advertise]`) with auto-
/// detected fallback. The list is non-empty by construction.
///
/// `auth` is the [`crate::peer::AuthRequirements`] to advertise —
/// what connecting peers should send. Built by
/// `crate::main::build_local_advertised_auth` from
/// `[server.auth]` (advertised_transport + bearer_token) and the
/// LAN cert dir (for `pin-self-cert` fingerprint). Phase 1 of slice
/// 2c always passed `AuthRequirements::none()`; this signature
/// change lets the operator advertise mTLS / pinned-mTLS / bearer
/// in the card so connecting peers know what to send.
pub fn build_local_agent_card(
    advertise_urls: Vec<String>,
    auth: crate::peer::AuthRequirements,
) -> crate::peer::AgentCard {
    use crate::peer::{Capability, TransportSpec};
    let transports: Vec<TransportSpec> = advertise_urls
        .into_iter()
        .map(|url| TransportSpec::IntendantWs { url })
        .collect();
    crate::peer::AgentCard::local_intendant(
        crate::lan::resolve_host_label(),
        env!("CARGO_PKG_VERSION").to_string(),
        Some(env!("INTENDANT_GIT_SHA").to_string()),
        transports,
        vec![
            Capability::ComputerUse,
            Capability::Knowledge,
            Capability::Display,
        ],
        auth,
    )
}

fn build_config_inner(
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
            ..Default::default()
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
                ..Default::default()
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            ..Default::default()
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
            ..Default::default()
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

    /// A specific bind address is preserved verbatim in the
    /// advertised URL. The operator chose it; we trust them.
    #[test]
    fn advertise_url_preserves_specific_bind_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[]),
            vec!["ws://127.0.0.1:8765/ws".to_string()]
        );
        let lan_ip = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(lan_ip), &[]),
            vec!["ws://192.168.1.42:8765/ws".to_string()]
        );
    }

    /// Wildcard bind (0.0.0.0) gets replaced with one URL per routable
    /// interface (auto-detection), never the literal wildcard. This
    /// is the guard against the production case where main.rs binds
    /// to 0.0.0.0:8765 and an earlier implementation was handing out
    /// `ws://0.0.0.0:8765/ws` in the Agent Card — an unusable URL
    /// that the transport-url-is-the-listener-addr assumption let
    /// slip through localhost-only tests.
    ///
    /// The exact set of interfaces is environment-dependent so we
    /// can't pin specific addresses; we only assert that no entry is
    /// the wildcard literal and the port is preserved everywhere.
    #[test]
    fn advertise_url_replaces_ipv4_wildcard_with_interface_urls() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[]);
        assert!(
            !urls.is_empty(),
            "auto-detect should produce at least one URL"
        );
        for url in &urls {
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.starts_with("ws://"), "scheme preserved: {url}");
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
            let host = url
                .strip_prefix("ws://")
                .and_then(|rest| rest.strip_suffix(":8765/ws"))
                .expect("url has expected prefix/suffix");
            assert!(
                !host.is_empty(),
                "host must resolve to something non-empty: {url}"
            );
        }
    }

    /// Same guard for IPv6 wildcards (::), which have the same
    /// unreachability problem as 0.0.0.0. Auto-detected v6 entries
    /// are bracketed per RFC 3986; we don't pin which interfaces are
    /// found because that's environment-dependent.
    #[test]
    fn advertise_url_replaces_ipv6_wildcard_with_interface_urls() {
        use std::net::{Ipv6Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[]);
        assert!(
            !urls.is_empty(),
            "wildcard v6 bind should still produce some auto-detected URLs"
        );
        for url in &urls {
            assert!(
                !url.contains("[::]"),
                "ipv6 wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// IPv6 specific addresses are bracketed in the URL per RFC 3986
    /// so a literal address like `::1` doesn't collide with the
    /// `:port` separator.
    #[test]
    fn advertise_url_brackets_specific_ipv6_address() {
        use std::net::{Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(specific), &[]);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].contains("[::1]"),
            "ipv6 literal must be bracketed: {}",
            urls[0]
        );
    }

    // -----------------------------------------------------------------
    // resolve_url_to_socket_addr (slice 3a.2 — URL hint parsing)
    // -----------------------------------------------------------------

    /// Directly-parseable `ipv4:port` authorities are returned
    /// without any DNS round-trip.
    #[tokio::test]
    async fn resolve_url_parses_ipv4_literal_url() {
        let addr = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws")
            .await
            .expect("parses");
        assert_eq!(addr.to_string(), "127.0.0.1:8766");
    }

    /// Bracketed IPv6 literals round-trip through the parser; the
    /// `/ws` path suffix is stripped before the SocketAddr parse.
    #[tokio::test]
    async fn resolve_url_parses_ipv6_literal_url() {
        let addr = resolve_url_to_socket_addr("wss://[::1]:8443/ws")
            .await
            .expect("parses");
        assert_eq!(addr.port(), 8443);
        assert!(addr.is_ipv6(), "expected IPv6, got {addr}");
    }

    /// `http://` and `https://` are accepted alongside the WebSocket
    /// schemes so the same URL form works whether the operator types
    /// the dashboard URL or the /ws URL.
    #[tokio::test]
    async fn resolve_url_accepts_http_and_https_schemes() {
        let a = resolve_url_to_socket_addr("http://127.0.0.1:8000/")
            .await
            .expect("http parses");
        assert_eq!(a.port(), 8000);
        let b = resolve_url_to_socket_addr("https://127.0.0.1:8443")
            .await
            .expect("https parses");
        assert_eq!(b.port(), 8443);
    }

    /// Hostnames route through `tokio::net::lookup_host`. `localhost`
    /// is the one name we can rely on across every test environment.
    #[tokio::test]
    async fn resolve_url_resolves_localhost_via_dns() {
        let addr = resolve_url_to_socket_addr("ws://localhost:8766/ws")
            .await
            .expect("resolves");
        assert_eq!(addr.port(), 8766);
        assert!(
            addr.ip().is_loopback(),
            "localhost must resolve to a loopback address: {addr}"
        );
    }

    /// URLs with a path + query string strip cleanly: the authority
    /// is everything up to the first `/` or `?`.
    #[tokio::test]
    async fn resolve_url_strips_path_and_query() {
        let a = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws/path?foo=bar")
            .await
            .expect("parses");
        assert_eq!(a.to_string(), "127.0.0.1:8766");
    }

    /// Unknown schemes, missing ports, and unresolvable hostnames
    /// all return `None` — caller falls back to UDP-only path.
    #[tokio::test]
    async fn resolve_url_returns_none_on_malformed_inputs() {
        // Unknown scheme
        assert!(resolve_url_to_socket_addr("foo://127.0.0.1:8766").await.is_none());
        // Empty authority
        assert!(resolve_url_to_socket_addr("ws:///path").await.is_none());
        // No port (authority parses as IP but not SocketAddr; lookup_host
        // rejects a bare host with no port).
        assert!(resolve_url_to_socket_addr("ws://127.0.0.1/ws").await.is_none());
    }

    /// Operator overrides come first in the merged list (preference
    /// order), but auto-detected entries are appended as fallbacks.
    /// The connecting peer's `MultiTransport::connect` walks the list
    /// top-down and uses the first that succeeds, so overrides win on
    /// preference while auto entries provide redundancy.
    #[test]
    fn advertise_overrides_prepend_to_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Specific bind so we can assert exactly one auto-detected entry
        // (wildcard bind would enumerate every host interface — non-
        // deterministic in CI). Specific-bind also covers the
        // intentionally-narrowed-listener case.
        let bind = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let overrides = vec![
            "ws://192.168.1.42:8765/ws".to_string(),
            "wss://laptop.tail-abcd.ts.net:8443/ws".to_string(),
        ];
        let urls = resolve_advertise_urls(Some(bind), &overrides);
        // Overrides come first, auto-detected entry appended.
        assert_eq!(urls.len(), 3, "got: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
        assert_eq!(urls[1], "wss://laptop.tail-abcd.ts.net:8443/ws");
        assert_eq!(urls[2], "ws://127.0.0.1:8765/ws");
    }

    /// An empty overrides list relies entirely on auto-detection.
    /// With a specific bind the result is exactly that one URL.
    #[test]
    fn empty_overrides_use_only_auto_detected_url() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan), &[]);
        assert_eq!(urls, vec!["ws://192.168.1.42:8765/ws".to_string()]);
    }

    /// Dedup: an operator URL that happens to match an auto-detected
    /// entry is kept exactly once (in operator position, since
    /// overrides are processed first). Avoids advertising the same
    /// URL twice when the operator types out their LAN IP that the
    /// daemon would have auto-detected anyway.
    #[test]
    fn advertise_dedupes_overrides_matching_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let overrides = vec!["ws://192.168.1.42:8765/ws".to_string()];
        let urls = resolve_advertise_urls(Some(lan), &overrides);
        assert_eq!(urls.len(), 1, "duplicate suppressed: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    /// A wildcard bind enumerates every routable non-loopback
    /// interface. We can't pin exact addresses (CI hosts vary) but
    /// can assert: (a) at least one URL is produced, (b) loopback is
    /// excluded (advertising loopback to remote peers is useless),
    /// (c) the port matches the bind port.
    #[test]
    fn advertise_wildcard_bind_enumerates_interfaces_excluding_loopback() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[]);
        assert!(
            !urls.is_empty(),
            "expected at least one auto-detected URL, got: {urls:?}"
        );
        for url in &urls {
            assert!(
                !url.contains("127.0.0.1"),
                "loopback must not appear in auto-detected federation URLs: {url}"
            );
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in auto-detected URLs: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// When operator wants to override completely (e.g. for security
    /// reasons — only advertise the Tailscale URL even though the
    /// daemon binds wildcard), they bind to a specific interface
    /// instead of wildcard. Specific bind narrows auto-detection to
    /// just that interface, so combined with operator override the
    /// effective list is `[override..., that_one_interface]`.
    #[test]
    fn specific_bind_narrows_auto_detection_to_one_interface() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan_only = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan_only), &[]);
        assert_eq!(urls.len(), 1, "specific bind = exactly one auto entry");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
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
        let config = build_config(None, Some("gemini-2.5-flash-native-audio-preview-12-2025"), false, crate::display::IceConfig::default());
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(None, Some("gpt-4o-realtime-preview"), false, crate::display::IceConfig::default());
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(Some("openai"), None, false, crate::display::IceConfig::default());
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(None, None, false, crate::display::IceConfig::default());
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    #[test]
    fn test_scan_replay_status_extracts_provider_model_autonomy() {
        let contents = concat!(
            r#"{"ts":"10:00:00","event":"session_start","level":"info"}"#,
            "\n",
            r#"{"ts":"10:00:01","event":"info","level":"info","message":"Provider: openai"}"#,
            "\n",
            r#"{"ts":"10:00:02","event":"info","level":"info","message":"Model: gpt-5"}"#,
            "\n",
            r#"{"ts":"10:00:03","event":"info","level":"info","message":"Autonomy: High"}"#,
            "\n",
        );
        let (p, m, a) = scan_replay_status(contents);
        assert_eq!(p.as_deref(), Some("openai"));
        assert_eq!(m.as_deref(), Some("gpt-5"));
        assert_eq!(a.as_deref(), Some("High"));
    }

    #[test]
    fn test_scan_replay_status_reads_debug_level_entries() {
        // Newer sessions write Provider/Model/Autonomy as `l.debug(...)`
        // so the event_type is "debug", not "info".  scan_replay_status
        // must pick those up too.
        let contents = concat!(
            r#"{"ts":"10:00:00","event":"debug","level":"debug","message":"Provider: anthropic"}"#,
            "\n",
            r#"{"ts":"10:00:01","event":"debug","level":"debug","message":"Model: claude-sonnet-4-6"}"#,
            "\n",
            r#"{"ts":"10:00:02","event":"debug","level":"debug","message":"Autonomy: Medium"}"#,
            "\n",
        );
        let (p, m, a) = scan_replay_status(contents);
        assert_eq!(p.as_deref(), Some("anthropic"));
        assert_eq!(m.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(a.as_deref(), Some("Medium"));
    }

    #[test]
    fn test_replay_jsonl_produces_replay_start_marker_first() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("Provider: openai");
        log.info("Model: gpt-5");
        log.info("Autonomy: Medium");
        log.turn_start(1, 0.0, 100_000);
        log.auto_approved("exec: ls");
        log.round_complete(1, 3);
        drop(log);

        let contents =
            std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        // First entry is the replay_start marker.
        assert_eq!(
            entries[0].get("event").and_then(|v| v.as_str()),
            Some("replay_start")
        );
        assert_eq!(
            entries[0].get("provider").and_then(|v| v.as_str()),
            Some("openai")
        );

        // Each OutboundEvent entry has its historical `ts` injected.
        // Find the turn_started entry and verify it carries the original ts.
        let turn_started = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("turn_started"))
            .expect("turn_started should be present");
        assert!(
            turn_started.get("ts").is_some(),
            "ts should be injected into each outbound entry"
        );

        // auto_approved preview preserved.
        let auto_approved = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("auto_approved"))
            .expect("auto_approved should be present");
        assert_eq!(
            auto_approved.get("preview").and_then(|v| v.as_str()),
            Some("exec: ls")
        );

        // round_complete fields propagated.
        let round = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("round_complete"))
            .expect("round_complete should be present");
        assert_eq!(round.get("round").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            round.get("turns_in_round").and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    #[test]
    fn test_replay_jsonl_skips_internal_events() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.messages_input(r#"[{"role":"user","content":"hi"}]"#); // -> skip
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#); // -> skip
        drop(log);

        let contents =
            std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        // Entries are: [replay_start, turn_started].  messages_input,
        // agent_input, and session_start all return None.
        assert_eq!(entries.len(), 2, "unexpected entries: {:#?}", entries);
        assert_eq!(
            entries[0].get("event").and_then(|v| v.as_str()),
            Some("replay_start")
        );
        assert_eq!(
            entries[1].get("event").and_then(|v| v.as_str()),
            Some("turn_started")
        );
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());

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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx.clone(), config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
            external_agent: None,
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

    // ---- /api/peers endpoint tests ----

    /// Spawn a test gateway with the given peer registry option and
    /// return (port, gateway handle). Condensed helper to keep the
    /// /api/peers tests below compact.
    async fn spawn_test_gateway_with_registry(
        peer_registry: Option<crate::peer::PeerRegistry>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Fire a raw HTTP request and read the response. Small helper
    /// because the /api/peers tests all make a handful of these.
    async fn http_request(port: u16, request: &str) -> String {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Same as `spawn_test_gateway_with_registry` but also wires an
    /// inbound bearer token. Used by the federation auth tests.
    async fn spawn_test_gateway_with_auth(
        peer_registry: Option<crate::peer::PeerRegistry>,
        bearer_token: Option<String>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            bearer_token,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    // -----------------------------------------------------------------
    // verify_bearer_token + is_federation_path unit tests
    // -----------------------------------------------------------------

    #[test]
    fn verify_bearer_token_passes_when_no_token_configured() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_token(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_missing_header_when_required() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("missing Authorization"));
    }

    #[test]
    fn verify_bearer_token_rejects_wrong_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("invalid bearer"));
    }

    #[test]
    fn verify_bearer_token_accepts_correct_token() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_header_name_case_insensitive() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nauthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_scheme_case_insensitive() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_non_bearer_scheme() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Basic Zm9vOmJhcg==\r\n\r\n";
        let err = verify_bearer_token(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("Bearer scheme"));
    }

    #[test]
    fn is_federation_path_recognizes_federation_endpoints() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers HTTP/1.1"));
        assert!(is_federation_path(
            "DELETE /api/peers HTTP/1.1"
        ));
        assert!(is_federation_path("GET /api/peers/eligible HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers/intendant:foo/message HTTP/1.1"));
        assert!(is_federation_path(
            "POST /api/coordinator/route HTTP/1.1"
        ));
        assert!(is_federation_path("GET /api/sessions HTTP/1.1"));
    }

    #[test]
    fn is_federation_path_excludes_unauthenticated_endpoints() {
        // Discovery, dashboard bootstrap, and `/ws` must NOT be
        // mistaken for federation paths — they're intentionally
        // exempt from bearer enforcement.
        assert!(!is_federation_path(
            "GET /.well-known/agent-card.json HTTP/1.1"
        ));
        assert!(!is_federation_path("GET /config HTTP/1.1"));
        assert!(!is_federation_path("GET / HTTP/1.1"));
        assert!(!is_federation_path("GET /static/app.js HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /ws HTTP/1.1\r\nUpgrade: websocket"
        ));
        assert!(!is_federation_path("GET /api/settings HTTP/1.1"));
        assert!(!is_federation_path("POST /api/api-keys HTTP/1.1"));
    }

    // -----------------------------------------------------------------
    // End-to-end: federation REST auth enforcement
    // -----------------------------------------------------------------

    /// With `inbound_bearer_token` configured, a federation request
    /// without an Authorization header is rejected 401.
    #[tokio::test]
    async fn test_federation_endpoint_rejects_missing_bearer() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        // Request without auth — should 401, NOT pass through to the
        // 503-no-registry response that would happen otherwise.
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("missing Authorization"));
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate header signals the auth scheme"
        );
        handle.abort();
    }

    /// Wrong bearer token → 401 with "invalid bearer token".
    #[tokio::test]
    async fn test_federation_endpoint_rejects_wrong_bearer() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("invalid bearer"));
        handle.abort();
    }

    /// Correct bearer token → request flows through to the normal
    /// handler (which then returns 503 because no registry was
    /// configured — proves auth passed and dispatch ran).
    #[tokio::test]
    async fn test_federation_endpoint_accepts_correct_bearer() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-token\r\n\r\n",
        )
        .await;
        // Auth passed; handler returned its 503 (no registry).
        assert!(resp.contains("503"), "expected 503 (auth passed, registry missing), got: {resp}");
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /config is exempt — even when bearer is required for
    /// federation endpoints, the dashboard bootstrap continues to work
    /// without auth. This is how the dashboard remains usable on the
    /// loopback / trusted-network case where the operator has set a
    /// bearer for WAN federation.
    #[tokio::test]
    async fn test_config_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(port, "GET /config HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("200 OK"),
            "config should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    // -----------------------------------------------------------------
    // /ws bearer enforcement (slice 2d)
    // -----------------------------------------------------------------

    #[test]
    fn extract_token_query_param_finds_token() {
        assert_eq!(
            extract_token_query_param("GET /ws?token=abc HTTP/1.1"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_query_param_finds_token_among_others() {
        assert_eq!(
            extract_token_query_param("GET /ws?other=x&token=abc&more=y HTTP/1.1"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_query_param_returns_none_when_absent() {
        assert_eq!(extract_token_query_param("GET /ws HTTP/1.1"), None);
        assert_eq!(
            extract_token_query_param("GET /ws?other=x HTTP/1.1"),
            None
        );
    }

    #[test]
    fn extract_token_query_param_handles_no_request_line() {
        assert_eq!(extract_token_query_param(""), None);
    }

    #[test]
    fn verify_bearer_for_ws_passes_when_no_token_configured() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n";
        assert!(verify_bearer_for_ws(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_authorization_header() {
        let header =
            "GET /ws HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_token_query_param() {
        // The dashboard browser path: no Authorization header (browsers
        // can't easily set headers on WebSocket opens), token rides on
        // the URL.
        let header = "GET /ws?token=right HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_rejects_when_neither_present() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    #[test]
    fn verify_bearer_for_ws_rejects_wrong_query_token() {
        let header = "GET /ws?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    /// Header AND query both present — header wins (matches first).
    /// Mismatched header with matching query: header check fails, query
    /// check passes, overall accepted. Documents the fallback behavior.
    #[test]
    fn verify_bearer_for_ws_header_wrong_falls_back_to_query() {
        let header = "GET /ws?token=right HTTP/1.1\r\n\
                      Host: x\r\n\
                      Authorization: Bearer wrong\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    /// Real /ws upgrade through `spawn_test_gateway_with_auth`:
    /// connecting without a token gets a plain HTTP 401 *before* the
    /// WebSocket handshake completes — the dashboard sees a 401 page,
    /// not a successful upgrade then immediate close.
    #[tokio::test]
    async fn test_ws_upgrade_rejects_missing_bearer() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate signals scheme"
        );
        // Critically, the upgrade did NOT complete.
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must reject before WS handshake completes"
        );
        handle.abort();
    }

    /// /ws with a matching Authorization header completes the upgrade
    /// (101 Switching Protocols). This is the daemon-to-daemon path
    /// that IntendantWsTransport uses.
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_authorization_header() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Bearer ws-token\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /ws with `?token=` query parameter completes the upgrade. This
    /// is the dashboard-browser path (browsers can't set arbitrary
    /// headers on `WebSocket` opens, so the token rides on the URL).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_query_token() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws?token=ws-token HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /ws with no token still works when the gateway has no bearer
    /// configured (the common case for trusted-LAN deployments).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_when_no_bearer_configured() {
        let (port, handle) = spawn_test_gateway_with_auth(None, None).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /.well-known/agent-card.json is exempt — discovery must work
    /// before any auth handshake. Connecting peers fetch the card to
    /// see what auth they need to satisfy.
    #[tokio::test]
    async fn test_agent_card_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) =
            spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("200 OK"),
            "agent card should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// `GET /api/peers` returns 503 when the web gateway was spawned
    /// without a peer registry. This lets the dashboard distinguish
    /// "peers not configured" from "no peers yet" and render
    /// differently.
    #[tokio::test]
    async fn test_api_peers_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        assert!(resp.contains("peer registry not configured"));
        handle.abort();
    }

    /// `GET /api/peers` on a registry with no peers returns
    /// `{"peers":[]}`. Baseline for the list endpoint shape.
    #[tokio::test]
    async fn test_api_peers_list_empty_registry() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("200 OK"));
        // Split body from headers.
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(body.trim(), r#"{"peers":[]}"#);
        handle.abort();
    }

    /// End-to-end: spawn a "target" gateway (gateway A) and a
    /// "dashboard" gateway (gateway B) with a peer registry. POST
    /// A's card URL to B's /api/peers. Assert the peer is added,
    /// GET /api/peers shows it, DELETE removes it. This exercises
    /// the full path from HTTP request through PeerRegistry,
    /// IntendantWsTransport, the Agent Card fetch, WebSocket
    /// connect, and event drain.
    #[tokio::test]
    async fn test_api_peers_add_list_remove_end_to_end() {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) =
            spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (dash_port, dash_handle) =
            spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!(
            "http://127.0.0.1:{target_port}/.well-known/agent-card.json"
        );
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "add failed: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peer_id = parsed["peer_id"]
            .as_str()
            .expect("peer_id missing")
            .to_string();
        assert!(peer_id.starts_with("intendant:"));

        // GET /api/peers should now show the added peer.
        let list_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(list_resp.contains("200 OK"));
        let list_body = list_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let list: serde_json::Value = serde_json::from_str(list_body).unwrap();
        let peers_arr = list["peers"].as_array().unwrap();
        assert_eq!(peers_arr.len(), 1);
        assert_eq!(peers_arr[0]["id"].as_str().unwrap(), peer_id);
        // The "id" field should match the peer_id returned from POST.
        // The "version" should be the local build's version.
        assert_eq!(
            peers_arr[0]["version"].as_str().unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        // The dashboard panel rebuild relies on `ws_url` being
        // present so the browser can open a secondary WASM
        // connection without re-fetching the card. Guard against
        // the field being dropped or renamed.
        let ws_url = peers_arr[0]["ws_url"]
            .as_str()
            .expect("ws_url field must be present in the API response");
        assert!(
            ws_url.starts_with("ws://") && ws_url.ends_with("/ws"),
            "ws_url should be a native Intendant WebSocket URL: {ws_url}"
        );
        // The dashboard renders capability badges from this list,
        // so it must be present and contain the always-on phase 1
        // capabilities the test peer advertises.
        let caps = peers_arr[0]["capabilities"]
            .as_array()
            .expect("capabilities must be a JSON array");
        assert!(!caps.is_empty(), "expected at least one capability");

        // DELETE /api/peers with the peer_id.
        let del_body = serde_json::json!({"peer_id": peer_id}).to_string();
        let del_req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            del_body.len(),
            del_body
        );
        let del_resp = http_request(dash_port, &del_req).await;
        assert!(del_resp.contains("200 OK"), "delete failed: {del_resp}");

        // GET should now be empty.
        let empty_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let empty_body = empty_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(empty_body.trim(), r#"{"peers":[]}"#);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers` with an invalid body returns 400 with a
    /// diagnostic error message.
    #[tokio::test]
    async fn test_api_peers_post_invalid_body() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `DELETE /api/peers` for an unknown peer id returns 404.
    #[tokio::test]
    async fn test_api_peers_delete_unknown_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = r#"{"peer_id":"intendant:ghost"}"#;
        let req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Per-peer outbound op endpoints — `/api/peers/{id}/{op}`
    // -----------------------------------------------------------------

    /// Poll the registry until the peer transitions to
    /// `ConnectionState::Connected`, or `timeout` elapses. Returns
    /// whether the peer connected in time. Used by the routing tests
    /// below to avoid sending ops at a peer whose transport is still
    /// in handshake (which would bounce off as `NotConnected` → 502
    /// and obscure the actual code path under test).
    async fn wait_for_connected(
        registry: &crate::peer::PeerRegistry,
        peer_id: &crate::peer::PeerId,
        timeout: tokio::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Some(h) = registry.get(peer_id) {
                if h.is_connected() {
                    return true;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }
        false
    }

    /// Boilerplate: spawn target gateway A, register it as a peer on
    /// dashboard gateway B via HTTP, wait for the transport to connect,
    /// return everything the per-peer op tests need: the dashboard's
    /// port (where ops are POSTed) plus the peer id (the path
    /// parameter for every op endpoint) plus all four task handles to
    /// abort at end of test. Cuts ~30 lines of setup per test.
    async fn setup_peer_op_test() -> (
        u16,
        String,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) =
            spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let registry_for_wait = registry.clone();
        let (dash_port, dash_handle) =
            spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!(
            "http://127.0.0.1:{target_port}/.well-known/agent-card.json"
        );
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "register failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        let peer_id = parsed["peer_id"].as_str().unwrap().to_string();

        // Wait for the IntendantWsTransport to finish its handshake so
        // the op ack distinguishes "handler+routing works" from
        // "transport not ready yet".
        let pid = crate::peer::PeerId(peer_id.clone());
        assert!(
            wait_for_connected(
                &registry_for_wait,
                &pid,
                tokio::time::Duration::from_secs(3),
            )
            .await,
            "peer never reached Connected"
        );

        (dash_port, peer_id, target_handle, dash_handle)
    }

    /// `POST /api/peers/{id}/message` with a `{text}` shorthand body
    /// returns 200 + a `message_id`. Verifies the path-parameter
    /// routing, the JSON shorthand parsing, and the dispatch into
    /// `PeerHandle::send_message`. The wire-level encoding (this
    /// becomes a `ControlMsg::FollowUp` over the WebSocket) is covered
    /// by `peer::transport::intendant::tests::send_message_writes_followup_control_msg`.
    #[tokio::test]
    async fn test_api_peers_send_message_text_shorthand_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        let body = serde_json::json!({"text": "hello peer"}).to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["message_id"].as_str().is_some(),
            "expected message_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/message` with a full `{role, content,
    /// session}` body works the same. Verifies the full-control shape
    /// path through `SendMessageRequest::into_message` (where `content`
    /// wins over `text` when both are present).
    #[tokio::test]
    async fn test_api_peers_send_message_full_shape_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        let body = serde_json::json!({
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/task` with `{instructions}` returns 200 +
    /// `task_id`. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::delegate_task_writes_start_task_control_msg`.
    #[tokio::test]
    async fn test_api_peers_delegate_task_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        let body = serde_json::json!({
            "instructions": "do the thing",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/task HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "delegate_task failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["task_id"].as_str().is_some(),
            "expected task_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/approval` with `{request_id, decision}`
    /// returns 200. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::resolve_approval_maps_each_decision_to_its_control_msg`.
    #[tokio::test]
    async fn test_api_peers_resolve_approval_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        let body = serde_json::json!({
            "request_id": "42",
            "decision": "accept",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/approval HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "resolve_approval failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{unknown}/message` returns 404 with a
    /// diagnostic body. Doesn't require setup — exercises only the
    /// peer lookup path before any transport interaction.
    #[tokio::test]
    async fn test_api_peers_op_unknown_peer_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"text": "hi"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:ghost/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("intendant:ghost"),
            "404 body should mention the missing id: {resp_body}"
        );
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with malformed JSON returns 400.
    #[tokio::test]
    async fn test_api_peers_send_message_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with neither `text` nor
    /// `content` returns 400. Verifies the `into_message` validation
    /// rejects empty bodies before the peer lookup runs.
    #[tokio::test]
    async fn test_api_peers_send_message_requires_text_or_content() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"session": "scratch"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("text") && resp_body.contains("content"),
            "error body should mention the missing fields: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown sub-op (e.g. `/api/peers/{id}/bogus`) returns 404 with
    /// a diagnostic body. Guards the dispatch arm that distinguishes
    /// "supported op" from "unrecognized verb".
    #[tokio::test]
    async fn test_api_peers_unknown_op_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "{}";
        let req = format!(
            "POST /api/peers/intendant:any/bogus HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("bogus"),
            "404 body should name the unknown op: {resp_body}"
        );
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Coordinator endpoints — capability discovery + delegation
    // -----------------------------------------------------------------

    /// `GET /api/peers/eligible` returns 503 with no registry,
    /// matching the rest of /api/peers.
    #[tokio::test]
    async fn test_api_peers_eligible_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        handle.abort();
    }

    /// Missing `?capability=...` query param returns 400 with a
    /// hint that at least one is required.
    #[tokio::test]
    async fn test_api_peers_eligible_requires_capability_param() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("capability"),
            "400 body should mention capability: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown capability strings return 400 with the offending
    /// values surfaced (not silently dropped, which would let an
    /// /api/peers/eligible?capability=typo through and return all
    /// peers).
    #[tokio::test]
    async fn test_api_peers_eligible_rejects_unknown_capability() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display&capability=nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("nope"),
            "400 body should name the unknown capability: {resp_body}"
        );
        handle.abort();
    }

    /// With one connected peer that advertises both ComputerUse and
    /// Knowledge (the test fixture's defaults), `?capability=computer-use`
    /// returns the peer; `?capability=display` returns an empty list
    /// (the fixture doesn't advertise Display).
    #[tokio::test]
    async fn test_api_peers_eligible_returns_matching_peers() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        // Hits: the test peer's card advertises ComputerUse.
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=computer-use HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peers = parsed["peers"].as_array().expect("peers array");
        assert_eq!(peers.len(), 1, "expected one matching peer");
        assert_eq!(peers[0]["id"].as_str().unwrap(), peer_id);

        // Misses: the fixture doesn't advertise Voice (build_local_agent_card
        // advertises ComputerUse + Knowledge + Display; Voice / Phone /
        // Recording are gated on runtime configuration that isn't plumbed
        // through yet).
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=voice HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["peers"].as_array().unwrap().len(), 0);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/coordinator/route` with required_capabilities the
    /// connected peer satisfies returns 200 + peer_id + task_id.
    /// Wire encoding to ControlMsg::StartTask is covered by
    /// peer::transport::intendant::tests.
    #[tokio::test]
    async fn test_api_coordinator_route_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        let body = serde_json::json!({
            "required_capabilities": ["computer-use"],
            "task": {
                "instructions": "do the thing",
                "context": {"file": "src/main.rs"},
            },
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(
            parsed["peer_id"].as_str().expect("peer_id present"),
            peer_id
        );
        assert!(
            parsed["task_id"].as_str().is_some(),
            "task_id should be present in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Routing a capability no connected peer satisfies returns 404
    /// with the considered peer ids surfaced for diagnostics.
    #[tokio::test]
    async fn test_api_coordinator_route_no_match_returns_404() {
        let (dash_port, peer_id, target_handle, dash_handle) =
            setup_peer_op_test().await;

        // Voice is the "gated, not-advertised-by-default" capability
        // that the stock build_local_agent_card fixture doesn't claim
        // — so routing by it hits no-route and surfaces the considered
        // list. Display moved to always-on in the 3a.1 fix, so it can
        // no longer serve as the deliberately-unsatisfied capability.
        let body = serde_json::json!({
            "required_capabilities": ["voice"],
            "task": {"instructions": "needs voice"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), "no route");
        let considered = parsed["considered"].as_array().expect("considered array");
        assert!(
            considered.iter().any(|v| v.as_str() == Some(&peer_id)),
            "considered list should include the peer that didn't match"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Bad JSON body returns 400.
    #[tokio::test]
    async fn test_api_coordinator_route_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// Empty `required_capabilities` returns 400 — would otherwise
    /// match every peer and route to the first lexicographically,
    /// which is almost never what the caller meant.
    #[tokio::test]
    async fn test_api_coordinator_route_rejects_empty_capabilities() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({
            "required_capabilities": [],
            "task": {"instructions": "anything"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("required_capabilities"),
            "400 body should mention required_capabilities: {resp_body}"
        );
        handle.abort();
    }

    /// GET on the route endpoint returns 405 — only POST is allowed.
    #[tokio::test]
    async fn test_api_coordinator_route_get_returns_405() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("405"), "expected 405, got: {resp}");
        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_html() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
            ..Default::default()
        };
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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

    /// `/config` is scoped to voice/runtime config only after the
    /// AgentCard split. Identity fields (host_label, version, git_sha)
    /// moved to /.well-known/agent-card.json. This test enforces the
    /// boundary so a future code change can't reintroduce drift
    /// between the two by sneaking identity fields back into
    /// WebGatewayConfig.
    #[tokio::test]
    async fn test_config_endpoint_has_no_identity_fields() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None, None, None, None, None, None, Vec::new(), None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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

        // Extract the JSON body (after the header terminator).
        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let parsed: serde_json::Value =
            serde_json::from_str(body).expect("body is JSON");
        let obj = parsed.as_object().expect("body is an object");

        assert!(obj.contains_key("provider"), "should still have runtime fields");
        assert!(obj.contains_key("model"));
        assert!(
            !obj.contains_key("host_label"),
            "host_label must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("version"),
            "version must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("git_sha"),
            "git_sha must live on the agent card, not /config: {obj:?}"
        );

        handle.abort();
    }

    /// `/.well-known/agent-card.json` reflects live daemon state and
    /// deserializes into an [`crate::peer::AgentCard`] with the
    /// expected shape. This is the server-side guardrail the user
    /// asked for — if someone breaks the assembly in
    /// `build_local_agent_card`, the endpoint round-trip fails here
    /// before anyone hits it in the browser.
    #[tokio::test]
    async fn test_agent_card_endpoint_reflects_live_state() {
        use crate::peer::{AgentCard, AuthRequirements, Capability, TransportAuth, TransportSpec};

        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None, None, None, None, None, None, Vec::new(), None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(
                b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "agent card endpoint should return 200: {response_str}"
        );
        assert!(response_str.contains("application/json"));

        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let card: AgentCard =
            serde_json::from_str(body).expect("body deserializes as AgentCard");

        // Identity fields must be populated from live state.
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::PeerKind::Intendant),
            "local daemon must identify as Intendant kind: id = {:?}",
            card.id
        );
        assert!(
            card.id.as_str().starts_with("intendant:"),
            "PeerId must have intendant prefix: {}",
            card.id.as_str()
        );
        assert!(
            !card.label.is_empty(),
            "label must be resolved from lan::resolve_host_label"
        );
        assert_eq!(
            card.version,
            env!("CARGO_PKG_VERSION"),
            "version must come from CARGO_PKG_VERSION"
        );
        assert_eq!(
            card.git_sha.as_deref(),
            Some(env!("INTENDANT_GIT_SHA")),
            "git_sha must come from INTENDANT_GIT_SHA"
        );

        // Transports must advertise at least the native Intendant WS
        // transport, with a URL that points back at this listener.
        assert_eq!(card.transports.len(), 1, "expected one transport");
        let expected_url_prefix = format!("ws://127.0.0.1:{port}");
        match &card.transports[0] {
            TransportSpec::IntendantWs { url } => {
                assert!(
                    url.starts_with(&expected_url_prefix) && url.ends_with("/ws"),
                    "transport URL {url} should start with {expected_url_prefix} and end with /ws"
                );
            }
            other => panic!("expected IntendantWs transport, got {other:?}"),
        }

        // Phase 1 conservative capability set.
        assert!(
            card.capabilities.contains(&Capability::ComputerUse),
            "card should advertise ComputerUse capability: {:?}",
            card.capabilities
        );
        assert!(
            card.capabilities.contains(&Capability::Knowledge),
            "card should advertise Knowledge capability: {:?}",
            card.capabilities
        );

        // Auth defaults to None in phase 1 (trust the network layer).
        assert!(
            matches!(card.auth.transport, TransportAuth::None) && card.auth.application.is_none(),
            "expected AuthRequirements::none() in phase 1, got {:?}",
            card.auth
        );

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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
                spawn_web_gateway(listener, bus, broadcast_tx, config, ss, None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none())
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
                spawn_web_gateway(listener, bus, broadcast_tx, config, ss, None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none())
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
                spawn_web_gateway(listener, bus, broadcast_tx, config, ss, None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none())
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
        let handle = spawn_web_gateway(listener, bus, broadcast_tx, config, ActiveSessionState::empty(), None, None, None, None, None, None, Vec::new(), None, crate::peer::AuthRequirements::none());
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
