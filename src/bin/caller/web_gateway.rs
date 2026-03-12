use crate::presence::{self, AgentStateSnapshot};
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::types::LogLevel;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

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

const WEB_HTML: &str = include_str!("../../../static/live.html");
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
}

impl Default for WebGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
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
pub fn spawn_web_gateway(
    port: u16,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    query_ctx: Option<WebQueryCtx>,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());
    let session_provider = config.provider.clone();
    let session_model = config.model.clone();
    let voice_debug = Arc::new(Mutex::new(VoiceDebugState::default()));

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
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (mut ws_tx, mut ws_rx) = ws_stream.split();
                    let mut outbound_rx = broadcast_tx.subscribe();

                    // Direct response channel: tool_response and state_snapshot
                    // messages for this specific connection (not broadcast).
                    let (direct_tx, mut direct_rx) =
                        tokio::sync::mpsc::unbounded_channel::<String>();

                    // Send bootstrap state snapshot on connect
                    if let Some(ref ctx) = query_ctx {
                        let state = ctx.agent_state.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let bootstrap = serde_json::json!({
                            "t": "state_snapshot",
                            "state": state,
                        });
                        let _ = direct_tx.send(bootstrap.to_string());
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
                    let inbound = tokio::spawn(async move {
                        // Track whether this connection has an active presence model,
                        // so we can auto-send PresenceDisconnected if the WebSocket drops
                        // without a clean presence_disconnect message (e.g. tab close
                        // before beforeunload fires, network failure).
                        let mut is_presence_connected = false;

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
                                            if let Some(key_event) = crate::tui::web::parse_web_key(&json) {
                                                bus_inbound.send(AppEvent::Key(key_event));
                                            }
                                        }
                                        Some("resize") => {
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            bus_inbound.send(AppEvent::Resize(cols, rows));
                                        }
                                        // Legacy: keep accepting live_connected/live_disconnected
                                        // for older clients, mapping to the new protocol
                                        Some("live_connected") => {
                                            is_presence_connected = true;
                                            voice_debug_inbound.lock().unwrap_or_else(|e| e.into_inner()).connected = true;
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
                                            bus_inbound.send(AppEvent::PresenceConnected {
                                                server_session_id: None,
                                                last_event_seq: 0,
                                                live_provider: Some(msg_provider),
                                                live_model: Some(msg_model),
                                            });
                                        }
                                        Some("live_disconnected") => {
                                            is_presence_connected = false;
                                            voice_debug_inbound.lock().unwrap_or_else(|e| e.into_inner()).connected = false;
                                            bus_inbound.send(AppEvent::PresenceDisconnected);
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

                                            // Send welcome with replay window if presence session is available
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
                                                    session.set_connected(true);
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
                                                    });
                                                    let _ = direct_tx_inbound.send(welcome_msg.to_string());
                                                }
                                            }

                                            bus_inbound.send(AppEvent::PresenceConnected {
                                                server_session_id,
                                                last_event_seq,
                                                live_provider: Some(msg_provider),
                                                live_model: Some(msg_model),
                                            });
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
                                            bus_inbound.send(AppEvent::PresenceDisconnected);
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
                                            bus_inbound.send(AppEvent::VoiceLog {
                                                text,
                                                seq,
                                                tool_context,
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

                                            bus_inbound.send(AppEvent::PresenceCheckpointReceived {
                                                summary,
                                                last_event_seq,
                                            });
                                        }
                                        Some("voice_diagnostic") => {
                                            let kind = json["kind"].as_str().unwrap_or("unknown").to_string();
                                            let detail = json["detail"].as_str().unwrap_or("").to_string();
                                            bus_inbound.send(AppEvent::VoiceDiagnostic {
                                                kind,
                                                detail,
                                            });
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

                                            let result = if let Some((ctrl, msg)) = presence::action_to_control_msg(&action) {
                                                // Action tools: dispatch via EventBus
                                                bus_inbound.send(AppEvent::ControlCommand(ctrl));
                                                msg
                                            } else {
                                                match action {
                                                    presence::PresenceAction::TextResult(text) => text,
                                                    presence::PresenceAction::NeedsIO { tool_name, args: io_args } => {
                                                        if let Some(ref ctx) = query_ctx_inbound {
                                                            if let Some(result) = presence::handle_tool_query(
                                                                &ctx.agent_state,
                                                                &ctx.project_root,
                                                                &ctx.log_dir,
                                                                &ctx.knowledge_path,
                                                                &tool_name,
                                                                &io_args,
                                                            ).await {
                                                                result
                                                            } else {
                                                                format!("Unknown tool: {}", tool)
                                                            }
                                                        } else {
                                                            "Presence query context not available".to_string()
                                                        }
                                                    }
                                                    _ => unreachable!(),
                                                }
                                            };

                                            // Log the tool response at Debug level
                                            let result_preview = if result.len() > 200 {
                                                format!("{}...", &result[..200])
                                            } else {
                                                result.clone()
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[tool_response] {} → {}", tool, result_preview),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let response = serde_json::json!({
                                                "t": "tool_response",
                                                "id": req_id,
                                                "result": result,
                                            });
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        _ => {
                                            // Fall through to ControlMsg parsing
                                            if let Ok(ctrl) = serde_json::from_value::<ControlMsg>(json) {
                                                bus_inbound.send(AppEvent::ControlCommand(ctrl));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // WebSocket closed — auto-resume server presence if this
                        // client had an active presence model (covers tab close without
                        // beforeunload, network drops, etc.)
                        if is_presence_connected {
                            bus_inbound.send(AppEvent::PresenceDisconnected);
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
                             Cache-Control: public, max-age=86400\r\n\
                             Connection: close\r\n\
                             \r\n",
                            wasm_data.len()
                        );
                        let _ = stream.try_write(header.as_bytes());
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(wasm_data).await;
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
                        let debug_json = serde_json::json!({
                            "agent_state": state,
                            "voice": vd,
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
                        let (content_type, body) = if request_line.contains("/wasm-web/presence_web.js") {
                            ("application/javascript", WASM_WEB_JS)
                        } else if request_line.contains("/audio-processor.js") {
                            ("application/javascript", AUDIO_PROCESSOR_JS)
                        } else if request_line.contains("/config") {
                            ("application/json", config_json.as_str())
                        } else {
                            ("text/html; charset=utf-8", WEB_HTML)
                        };

                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: {}\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            content_type,
                            body.len(),
                            body
                        );
                        let _ = stream.try_write(response.as_bytes());
                    }
                    // Give the client time to receive before dropping.
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            });
        }
    })
}

/// Build a `WebGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
pub fn build_config(live_provider: Option<&str>, live_model: Option<&str>) -> WebGatewayConfig {
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
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
        };
    }

    // Fall back to env var detection
    if std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("GEMINI_API_KEY").is_err() {
        WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
        }
    } else {
        WebGatewayConfig::default()
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
    fn test_live_html_embedded() {
        assert!(!WEB_HTML.is_empty());
        assert!(WEB_HTML.contains("<!DOCTYPE html>"));
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
        let config = build_config(None, Some("gemini-2.5-flash-native-audio-preview-12-2025"));
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(None, Some("gpt-4o-realtime-preview"));
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(Some("openai"), None);
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(None, None);
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(0, bus, broadcast_tx, config, None);

        // Give it a moment to bind
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_websocket_echo() {
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        // Bind to port 0 for a random free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a Status control message
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Verify the EventBus receives the ControlCommand
        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");

        match event {
            AppEvent::ControlCommand(ControlMsg::Status) => {}
            _ => panic!("expected ControlCommand(Status), got {:?}", event),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_broadcast_to_websocket() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx.clone(), config, None);
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
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
        };
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
    async fn test_legacy_live_connected_maps_to_presence() {
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Legacy live_connected should map to PresenceConnected
        ws.send(Message::Text(r#"{"t":"live_connected"}"#.into()))
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
                assert!(server_session_id.is_none());
                assert_eq!(last_event_seq, 0);
            }
            _ => panic!("expected PresenceConnected"),
        }

        // Legacy live_disconnected
        ws.send(Message::Text(r#"{"t":"live_disconnected"}"#.into()))
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
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
        let (bus, _rx) = EventBus::new();
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
        });

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, query_ctx);
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
        let (bus, _rx) = EventBus::new();
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
        });

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, query_ctx);
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
        let (bus, mut rx) = EventBus::new();
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
        });

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, query_ctx);
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
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a resize (not live_connected), then drop
        ws.send(Message::Text(r#"{"t":"resize","cols":80,"rows":24}"#.into()))
            .await
            .unwrap();

        // Drain the Resize event
        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");
        assert!(matches!(event, AppEvent::Resize(80, 24)));

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
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(port, bus, broadcast_tx, config, None);
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
}
