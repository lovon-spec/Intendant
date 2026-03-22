use crate::presence::{self, AgentStateSnapshot};
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::types::LogLevel;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

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
                "session": {
                    "type": "realtime",
                    "model": model,
                    "modalities": ["audio", "text"],
                    "voice": "alloy",
                },
                "expires_after": {
                    "anchor": "created_at",
                    "seconds": 600,
                }
            });
            let resp = reqwest::Client::new()
                .post("https://api.openai.com/v1/realtime/client_secrets")
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
            let token = data["value"]
                .as_str()
                .ok_or("No 'value' in OpenAI response")?;
            let expires_at = data["expires_at"].as_i64().unwrap_or(0);
            Ok(serde_json::json!({ "token": token, "expires_at": expires_at }).to_string())
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
}

impl Default for WebGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled: false,
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
fn replay_session_log(contents: &str) -> Vec<serde_json::Value> {
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
            "agent_input" => {
                ("agent", format!("Commands: {}", message), "detail")
            }
            "agent_output" => {
                // Parse runtime JSON to extract stdout_tail for display.
                let mut parts = Vec::new();
                for json_line in message.lines() {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_line) {
                        if parsed.get("type").and_then(|v| v.as_str()) == Some("result") {
                            if let Some(data_str) = parsed.get("data").and_then(|v| v.as_str()) {
                                if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str) {
                                    if let Some(stdout) = data.get("stdout_tail").and_then(|v| v.as_str()) {
                                        let trimmed = stdout.trim();
                                        if !trimmed.is_empty() {
                                            parts.push(trimmed.to_string());
                                        }
                                    }
                                    if let Some(stderr) = data.get("stderr_tail").and_then(|v| v.as_str()) {
                                        let trimmed = stderr.trim();
                                        if !trimmed.is_empty() {
                                            parts.push(format!("[stderr] {}", trimmed));
                                        }
                                    }
                                    // Show non-zero exit codes
                                    if let Some(code) = data.get("exit_code").and_then(|v| v.as_i64()) {
                                        if code != 0 {
                                            parts.push(format!("exit code: {}", code));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if parts.is_empty() { continue; }
                ("agent", parts.join("\n"), "agent")
            }

            // ── Voice / presence lifecycle ──
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
                ("live", format!("[You] {}", text), "info")
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

/// Bidirectional proxy: WebSocket (noVNC) ↔ TCP (x11vnc RFB).
///
/// noVNC sends/receives VNC protocol data as WebSocket binary frames.
/// This function accepts the WebSocket handshake, connects to the local
/// x11vnc TCP port, and forwards data in both directions until either
/// side disconnects.
async fn handle_vnc_proxy(stream: tokio::net::TcpStream, vnc_port: u32) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let ws_stream = match tokio_tungstenite::accept_hdr_async(
        stream,
        VncWsCallback,
    )
    .await
    {
        Ok(ws) => ws,
        Err(_) => return,
    };

    let tcp = match tokio::net::TcpStream::connect(format!("127.0.0.1:{}", vnc_port)).await {
        Ok(s) => s,
        Err(_) => return,
    };

    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // WS → TCP: binary frames from noVNC to x11vnc
    let ws_to_tcp = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    if tcp_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // TCP → WS: VNC data from x11vnc to noVNC as binary frames
    let tcp_to_ws = tokio::spawn(async move {
        let mut buf = [0u8; 16384];
        loop {
            let n = match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if ws_tx
                .send(Message::Binary(buf[..n].to_vec().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // When either direction closes, abort both
    tokio::select! {
        _ = ws_to_tcp => {}
        _ = tcp_to_ws => {}
    }
}

/// WebSocket handshake callback that adds the `Sec-WebSocket-Protocol: binary`
/// header when the client requests it. noVNC requires this sub-protocol.
struct VncWsCallback;

impl tokio_tungstenite::tungstenite::handshake::server::Callback for VncWsCallback {
    fn on_request(
        self,
        request: &tokio_tungstenite::tungstenite::http::Request<()>,
        mut response: tokio_tungstenite::tungstenite::http::Response<()>,
    ) -> Result<
        tokio_tungstenite::tungstenite::http::Response<()>,
        tokio_tungstenite::tungstenite::http::Response<Option<String>>,
    > {
        // noVNC sends Sec-WebSocket-Protocol: binary
        if let Some(proto) = request.headers().get("sec-websocket-protocol") {
            if let Ok(s) = proto.to_str() {
                if s.contains("binary") {
                    response.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        "binary".parse().unwrap(),
                    );
                }
            }
        }
        Ok(response)
    }
}

pub fn spawn_web_gateway(
    port: u16,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    query_ctx: Option<WebQueryCtx>,
    transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
    frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());

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
    // Cache the latest VNC port from display_ready events for the /vnc proxy.
    let last_vnc_port: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    // Cache the last display_ready JSON for late-connecting browsers.
    let last_display_ready_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let usage_cache = last_usage_json.clone();
        let live_usage_cache = last_live_usage_json.clone();
        let status_cache = last_status_json.clone();
        let vnc_cache = last_vnc_port.clone();
        let display_cache = last_display_ready_json.clone();
        let mut usage_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match usage_rx.recv().await {
                    Ok(line) => {
                        // Cache VNC port and full event from display_ready events
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                if let Some(port) = parsed.get("vnc_port").and_then(|v| v.as_u64()) {
                                    if let Ok(mut guard) = vnc_cache.lock() {
                                        *guard = Some(port as u32);
                                    }
                                }
                            }
                            if let Ok(mut guard) = display_cache.lock() {
                                *guard = Some(line.clone());
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
        let addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Live gateway bind failed on {}: {}", addr, e);
                return;
            }
        };

        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();
            let query_ctx = query_ctx.clone();
            let voice_debug = voice_debug.clone();
            let session_provider = session_provider.clone();
            let session_model = session_model.clone();
            let app_html = app_html.clone();
            let transcriber = transcriber.clone();
            let active_presence = active_presence.clone();
            let last_usage_json = last_usage_json.clone();
            let last_live_usage_json = last_live_usage_json.clone();
            let last_status_json = last_status_json.clone();
            let last_vnc_port = last_vnc_port.clone();
            let last_display_ready_json = last_display_ready_json.clone();
            let web_tui_tx = web_tui_tx.clone();
            let frame_registry = frame_registry.clone();
            let session_log = session_log.clone();

            tokio::spawn(async move {
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
                    // Detect /vnc path for VNC proxy before accepting
                    let request_line = header_text.lines().next().unwrap_or("");
                    let is_vnc_proxy = request_line.contains("/vnc");

                    if is_vnc_proxy {
                        // VNC WebSocket-to-TCP proxy
                        let vnc_port = if let Some(port_str) = request_line.split("port=").nth(1) {
                            port_str.split_whitespace().next()
                                .and_then(|s| s.split('&').next())
                                .and_then(|s| s.parse::<u32>().ok())
                        } else {
                            None
                        }.or_else(|| last_vnc_port.lock().ok().and_then(|g| *g));

                        let Some(port) = vnc_port else { return };

                        handle_vnc_proxy(stream, port).await;
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

                    // Send cached display_ready so late-connecting browsers
                    // auto-create display slots.
                    if let Ok(guard) = last_display_ready_json.lock() {
                        if let Some(ref display_json) = *guard {
                            let _ = direct_tx.send(display_json.clone());
                        }
                    }

                    // Replay session log so late-connecting browsers see
                    // historical events (not just real-time from now on).
                    if let Some(ref ctx) = query_ctx {
                        let session_jsonl = ctx.log_dir.join("session.jsonl");
                        if let Ok(contents) = std::fs::read_to_string(&session_jsonl) {
                            let replay = serde_json::json!({
                                "t": "log_replay",
                                "entries": replay_session_log(&contents),
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
                    let session_log_inbound = session_log.clone();
                    let inbound = tokio::spawn(async move {
                        // Track whether this connection has an active presence model,
                        // so we can auto-send PresenceDisconnected if the WebSocket drops
                        // without a clean presence_disconnect message (e.g. tab close
                        // before beforeunload fires, network failure).
                        let mut is_presence_connected = false;
                        // Whether this connection is the active voice owner
                        let mut is_active = false;

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
                                                slot.is_none()
                                            };

                                            if becomes_active {
                                                // First-connect wins: grant active status
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
                                            // (passive browsers don't pause server-side presence)
                                            if becomes_active {
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

                                            // Tell old active to disconnect voice
                                            if let Some(ref old) = *slot {
                                                if old.connection_id != connection_id_inbound {
                                                    let force_msg = serde_json::json!({
                                                        "t": "force_disconnect_voice",
                                                        "reason": "handover",
                                                    });
                                                    let _ = old.direct_tx.send(force_msg.to_string());
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

                                            // Send active_granted to this connection
                                            let granted_msg = serde_json::json!({
                                                "t": "active_granted",
                                                "is_active": true,
                                                "handover_context": handover_context,
                                                "conversation_context": conversation_ctx,
                                            });
                                            let _ = direct_tx_inbound.send(granted_msg.to_string());

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
                                            if let Some(ref registry) = frame_registry_inbound {
                                                let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                                                let stream = json["stream"].as_str().unwrap_or("cam0").to_string();
                                                if let Some(data_b64) = json["data"].as_str() {
                                                    use base64::Engine;
                                                    if let Ok(jpeg_bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream,
                                                            timestamp: chrono::Utc::now().to_rfc3339(),
                                                            sent_to_live: true,
                                                            live_resolution: Some("768x768".to_string()),
                                                            hq_resolution: None, // Could be extracted from image headers
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) = reg.register(meta, &jpeg_bytes) {
                                                            eprintln!("frame registry write failed: {}", e);
                                                        }
                                                    }
                                                }
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

                                            let query_result = if let Some((ctrl, msg)) = presence::action_to_control_msg(&action) {
                                                // Action tools: dispatch via EventBus
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
                             Cache-Control: public, max-age=31536000, immutable\r\n\
                             Connection: close\r\n\
                             \r\n",
                            wasm_data.len()
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(wasm_data).await;
                    } else if request_line.contains("/frames/") {
                        // Serve HQ frame images from the frame registry.
                        // URL format: /frames/<frame_id>
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
                    } else if request_line.starts_with("POST") && request_line.contains("/session") {
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
                    } else {
                        let (content_type, body, cache) = if request_line.contains("/wasm-web/presence_web.js") {
                            ("application/javascript", WASM_WEB_JS.to_string(), "public, max-age=31536000, immutable")
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
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
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
        }
    } else {
        let mut cfg = WebGatewayConfig::default();
        cfg.transcription_enabled = transcription_enabled;
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
        assert!(APP_HTML.contains("tab-usage"));
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
        let config = build_config(None, Some("gemini-2.5-flash-native-audio-preview-12-2025"), false);
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(None, Some("gpt-4o-realtime-preview"), false);
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(Some("openai"), None, false);
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(None, None, false);
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(0, bus, broadcast_tx, config, None, None, None, None, None);

        // Give it a moment to bind
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx.clone(), config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled: false,
        };
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

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
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, query_ctx, None, None, None, None);
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
        drop(listener);

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
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, query_ctx, None, None, None, None);
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
        drop(listener);

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
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, query_ctx, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None, None, None, None, None);
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
}
