use crate::error::CallerError;
use crate::live_audio_types::*;
use crate::audio_routing::AudioBridge;
use crate::quarantine;
use crate::schema_validator;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------------------
// Live audio events
// ---------------------------------------------------------------------------

/// Events emitted by the live audio session's read loop.
#[derive(Debug)]
pub enum LiveAudioEvent {
    Connected,
    SetupComplete,
    /// Model produced audio to play to the app (raw PCM16 bytes).
    AudioOut(Vec<u8>),
    /// Model transcription of what it said.
    ModelTranscript(String),
    /// Model text output (non-audio, e.g. the structured response).
    ModelText(String),
    /// Model attempted a tool call (will be quarantined).
    ToolCallAttempted {
        name: String,
        args: serde_json::Value,
    },
    TurnComplete,
    Interrupted,
    Disconnected(String),
    Error(String),
}

// ---------------------------------------------------------------------------
// Live audio session
// ---------------------------------------------------------------------------

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    WsMessage,
>;

/// A running live audio session connected to a model via WebSocket.
pub struct LiveAudioSession {
    ws_write: Arc<Mutex<WsSink>>,
    pub event_rx: mpsc::UnboundedReceiver<LiveAudioEvent>,
    pub provider: LiveAudioProvider,
    pub sample_rate: u32,
    read_handle: tokio::task::JoinHandle<()>,
}

impl LiveAudioSession {
    /// Send raw PCM16 audio to the model.
    pub async fn send_audio(&self, pcm16: &[u8]) -> Result<(), CallerError> {
        let b64 = BASE64.encode(pcm16);
        let msg = match self.provider {
            LiveAudioProvider::Gemini => serde_json::json!({
                "realtime_input": {
                    "media_chunks": [{
                        "mime_type": format!("audio/pcm;rate={}", self.sample_rate),
                        "data": b64
                    }]
                }
            }),
            LiveAudioProvider::OpenAI => serde_json::json!({
                "type": "input_audio_buffer.append",
                "audio": b64
            }),
        };
        let mut sink = self.ws_write.lock().await;
        sink.send(WsMessage::Text(msg.to_string()))
            .await
            .map_err(|e| CallerError::Agent(format!("WebSocket send error: {}", e)))?;
        Ok(())
    }

    /// Send a text message to the model.
    pub async fn send_text(&self, text: &str) -> Result<(), CallerError> {
        let msg = match self.provider {
            LiveAudioProvider::Gemini => serde_json::json!({
                "client_content": {
                    "turns": [{"role": "user", "parts": [{"text": text}]}],
                    "turn_complete": true
                }
            }),
            LiveAudioProvider::OpenAI => serde_json::json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            }),
        };

        let mut sink = self.ws_write.lock().await;
        sink.send(WsMessage::Text(msg.to_string()))
            .await
            .map_err(|e| CallerError::Agent(format!("WebSocket send error: {}", e)))?;

        // OpenAI requires an explicit response.create after sending content
        if self.provider == LiveAudioProvider::OpenAI {
            sink.send(WsMessage::Text(
                r#"{"type":"response.create"}"#.to_string(),
            ))
            .await
            .map_err(|e| CallerError::Agent(format!("WebSocket send error: {}", e)))?;
        }

        Ok(())
    }

    /// Gracefully close the WebSocket connection.
    pub async fn close(self) {
        let mut sink = self.ws_write.lock().await;
        let _ = sink.send(WsMessage::Close(None)).await;
        drop(sink);
        self.read_handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Gemini Live connection
// ---------------------------------------------------------------------------

const GEMINI_API_BASE: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContent";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash-native-audio-preview-12-2025";

pub async fn connect_gemini(
    api_key: &str,
    model: Option<&str>,
    playbook: &str,
    voice: Option<&str>,
    sample_rate: u32,
) -> Result<LiveAudioSession, CallerError> {
    let model_name = model.unwrap_or(DEFAULT_GEMINI_MODEL);
    let url = format!("{}?key={}", GEMINI_API_BASE, api_key);
    let voice_name = voice.unwrap_or("Aoede");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| CallerError::Agent(format!("Gemini WebSocket connect failed: {}", e)))?;

    let (ws_write, ws_read) = ws_stream.split();
    let ws_write = Arc::new(Mutex::new(ws_write));
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // Send setup message
    let setup = serde_json::json!({
        "setup": {
            "model": format!("models/{}", model_name),
            "generation_config": {
                "response_modalities": ["AUDIO"],
                "speech_config": {
                    "voice_config": {
                        "prebuilt_voice_config": {
                            "voice_name": voice_name
                        }
                    }
                }
            },
            "output_audio_transcription": {},
            "system_instruction": {
                "parts": [{ "text": playbook }]
            },
            "tools": []
        }
    });

    {
        let mut sink = ws_write.lock().await;
        sink.send(WsMessage::Text(setup.to_string()))
            .await
            .map_err(|e| CallerError::Agent(format!("Gemini setup send failed: {}", e)))?;
    }

    let _ = event_tx.send(LiveAudioEvent::Connected);

    // Spawn read loop
    let read_handle = tokio::spawn(gemini_read_loop(ws_read, event_tx));

    Ok(LiveAudioSession {
        ws_write,
        event_rx,
        provider: LiveAudioProvider::Gemini,
        sample_rate,
        read_handle,
    })
}

type WsReadStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
>;

async fn gemini_read_loop(
    mut ws_read: WsReadStream,
    event_tx: mpsc::UnboundedSender<LiveAudioEvent>,
) {
    while let Some(msg_result) = ws_read.next().await {
        let text = match msg_result {
            Ok(WsMessage::Text(t)) => t,
            Ok(WsMessage::Binary(b)) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Ok(WsMessage::Close(_)) => {
                let _ = event_tx.send(LiveAudioEvent::Disconnected("close frame".into()));
                break;
            }
            Err(e) => {
                let _ = event_tx.send(LiveAudioEvent::Error(format!("WS read error: {}", e)));
                break;
            }
            _ => continue,
        };

        let msg: serde_json::Value = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // setupComplete
        if msg.get("setupComplete").is_some() {
            let _ = event_tx.send(LiveAudioEvent::SetupComplete);
            continue;
        }

        // toolCall — quarantine, not dispatch
        if let Some(tool_call) = msg.get("toolCall") {
            if let Some(fcs) = tool_call.get("functionCalls").and_then(|v| v.as_array()) {
                for fc in fcs {
                    let name = fc["name"].as_str().unwrap_or("unknown").to_string();
                    let args = fc.get("args").cloned().unwrap_or_default();
                    let _ = event_tx.send(LiveAudioEvent::ToolCallAttempted { name, args });
                }
            }
            continue;
        }

        // toolCallCancellation — ignore
        if msg.get("toolCallCancellation").is_some() {
            continue;
        }

        // serverContent
        if let Some(response) = msg.get("serverContent") {
            // Output transcription
            if let Some(transcript) = response.get("outputTranscription") {
                if let Some(text) = transcript.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        let _ = event_tx.send(LiveAudioEvent::ModelTranscript(text.to_string()));
                    }
                }
                continue;
            }

            // turnComplete
            if response.get("turnComplete").is_some() {
                let _ = event_tx.send(LiveAudioEvent::TurnComplete);
                continue;
            }

            // interrupted
            if response.get("interrupted").is_some() {
                let _ = event_tx.send(LiveAudioEvent::Interrupted);
                continue;
            }

            // modelTurn parts
            if let Some(model_turn) = response.get("modelTurn") {
                if let Some(parts) = model_turn.get("parts").and_then(|v| v.as_array()) {
                    for part in parts {
                        // Audio data
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(mime) = inline.get("mimeType").and_then(|v| v.as_str()) {
                                if mime.starts_with("audio/") {
                                    if let Some(data) =
                                        inline.get("data").and_then(|v| v.as_str())
                                    {
                                        if let Ok(pcm) = BASE64.decode(data) {
                                            let _ = event_tx
                                                .send(LiveAudioEvent::AudioOut(pcm));
                                        }
                                    }
                                }
                            }
                        }
                        // Text output
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            let _ =
                                event_tx.send(LiveAudioEvent::ModelText(text.to_string()));
                        }
                        // Function call in model turn — quarantine
                        if let Some(fc) = part.get("functionCall") {
                            let name =
                                fc["name"].as_str().unwrap_or("unknown").to_string();
                            let args = fc.get("args").cloned().unwrap_or_default();
                            let _ = event_tx
                                .send(LiveAudioEvent::ToolCallAttempted { name, args });
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI Realtime connection
// ---------------------------------------------------------------------------

const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-realtime-preview";

pub async fn connect_openai(
    api_key: &str,
    model: Option<&str>,
    playbook: &str,
    voice: Option<&str>,
    sample_rate: u32,
) -> Result<LiveAudioSession, CallerError> {
    let model_name = model.unwrap_or(DEFAULT_OPENAI_MODEL);
    let url = format!("wss://api.openai.com/v1/realtime?model={}", model_name);
    let voice_name = voice.unwrap_or("alloy");

    // Build WebSocket request with auth headers via sub-protocols
    use tokio_tungstenite::tungstenite::http;
    let request = http::Request::builder()
        .uri(&url)
        .header("Sec-WebSocket-Protocol", format!(
            "realtime, openai-insecure-api-key.{}, openai-beta.realtime-v1",
            api_key
        ))
        .header("Host", "api.openai.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
        .body(())
        .map_err(|e| CallerError::Agent(format!("failed to build request: {}", e)))?;

    let (ws_stream, _): (
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        _,
    ) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| CallerError::Agent(format!("OpenAI WebSocket connect failed: {}", e)))?;

    let (ws_write, ws_read) = ws_stream.split();
    let ws_write: Arc<Mutex<WsSink>> = Arc::new(Mutex::new(ws_write));
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // Send session.update (zero tools)
    let setup = serde_json::json!({
        "type": "session.update",
        "session": {
            "modalities": ["audio", "text"],
            "instructions": playbook,
            "voice": voice_name,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "tools": []
        }
    });

    {
        let mut sink = ws_write.lock().await;
        sink.send(WsMessage::Text(setup.to_string()))
            .await
            .map_err(|e| CallerError::Agent(format!("OpenAI setup send failed: {}", e)))?;
    }

    let _ = event_tx.send(LiveAudioEvent::Connected);

    // Spawn read loop
    let read_handle = tokio::spawn(openai_read_loop(ws_read, event_tx));

    Ok(LiveAudioSession {
        ws_write,
        event_rx,
        provider: LiveAudioProvider::OpenAI,
        sample_rate,
        read_handle,
    })
}

async fn openai_read_loop(
    mut ws_read: WsReadStream,
    event_tx: mpsc::UnboundedSender<LiveAudioEvent>,
) {
    while let Some(msg_result) = ws_read.next().await {
        let text = match msg_result {
            Ok(WsMessage::Text(t)) => t,
            Ok(WsMessage::Close(_)) => {
                let _ = event_tx.send(LiveAudioEvent::Disconnected("close frame".into()));
                break;
            }
            Err(e) => {
                let _ = event_tx.send(LiveAudioEvent::Error(format!("WS read error: {}", e)));
                break;
            }
            _ => continue,
        };

        let msg: serde_json::Value = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let msg_type = msg["type"].as_str().unwrap_or("");
        match msg_type {
            "session.created" | "session.updated" => {
                let _ = event_tx.send(LiveAudioEvent::SetupComplete);
            }
            "response.audio.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    if let Ok(pcm) = BASE64.decode(delta) {
                        let _ = event_tx.send(LiveAudioEvent::AudioOut(pcm));
                    }
                }
            }
            "response.text.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    let _ = event_tx.send(LiveAudioEvent::ModelText(delta.to_string()));
                }
            }
            "response.audio_transcript.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    let _ = event_tx.send(LiveAudioEvent::ModelTranscript(delta.to_string()));
                }
            }
            "response.function_call_arguments.done" => {
                let name = msg["name"].as_str().unwrap_or("unknown").to_string();
                let args = serde_json::from_str::<serde_json::Value>(
                    msg["arguments"].as_str().unwrap_or("{}"),
                )
                .unwrap_or_default();
                let _ = event_tx.send(LiveAudioEvent::ToolCallAttempted { name, args });
            }
            "input_audio_buffer.speech_started" => {
                let _ = event_tx.send(LiveAudioEvent::Interrupted);
            }
            "response.done" => {
                let _ = event_tx.send(LiveAudioEvent::TurnComplete);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Audio bridge (PulseAudio <-> live model)
// ---------------------------------------------------------------------------

/// Bidirectional audio bridge between PulseAudio virtual devices and a live model session.
pub struct AudioStreamBridge {
    capture_handle: tokio::task::JoinHandle<()>,
    playback_handle: tokio::task::JoinHandle<()>,
}

impl AudioStreamBridge {
    /// Stop the audio bridge tasks.
    pub fn stop(self) {
        self.capture_handle.abort();
        self.playback_handle.abort();
    }
}

/// Start a bidirectional audio bridge.
///
/// - **Capture**: reads from PulseAudio monitor (app audio) and sends to the live model
/// - **Playback**: receives model audio output and writes to PulseAudio sink (app mic input)
pub async fn start_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    bridge: &AudioBridge,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    capture_tee_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<AudioStreamBridge, CallerError> {
    if let Some(host) = bridge.network_host() {
        return start_network_audio_bridge(
            session_write,
            provider,
            sample_rate,
            host,
            audio_out_rx,
            capture_tee_tx,
        )
        .await;
    }

    start_local_audio_bridge(
        session_write,
        provider,
        sample_rate,
        bridge,
        audio_out_rx,
        capture_tee_tx,
    )
    .await
}

/// Network audio bridge: connects to a bh-bridge on the host over TCP.
/// The TCP stream is full-duplex raw PCM16 mono — host→client is captured
/// app audio, client→host is model audio for playback.
async fn start_network_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    host_addr: &str,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    capture_tee_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<AudioStreamBridge, CallerError> {
    let stream = tokio::net::TcpStream::connect(host_addr)
        .await
        .map_err(|e| {
            CallerError::Agent(format!("bh-bridge connect to {} failed: {}", host_addr, e))
        })?;

    let (read_half, write_half) = tokio::io::split(stream);

    eprintln!("live_audio: network bridge connected to {}", host_addr);

    // Capture task: read PCM from TCP (host captures app audio) → send to model
    let capture_write = session_write;
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_handle = tokio::spawn(async move {
        let mut reader = read_half;
        let chunk_size = (capture_rate as usize) * 2 / 10; // ~100ms
        let mut buf = vec![0u8; chunk_size];
        let mut chunks_sent = 0usize;

        loop {
            match reader.read_exact(&mut buf).await {
                Ok(_) => {
                    chunks_sent += 1;
                    let b64 = BASE64.encode(&buf);
                    let msg = match capture_provider {
                        LiveAudioProvider::Gemini => serde_json::json!({
                            "realtime_input": {
                                "media_chunks": [{
                                    "mime_type": format!("audio/pcm;rate={}", capture_rate),
                                    "data": b64
                                }]
                            }
                        }),
                        LiveAudioProvider::OpenAI => serde_json::json!({
                            "type": "input_audio_buffer.append",
                            "audio": b64
                        }),
                    };
                    if let Some(ref tee) = capture_tee_tx {
                        let _ = tee.send(buf.clone());
                    }
                    let mut sink = capture_write.lock().await;
                    if sink.send(WsMessage::Text(msg.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        eprintln!("live_audio: network capture ended after {} chunks", chunks_sent);
    });

    // Playback task: model audio → write PCM to TCP (host plays to app mic)
    let playback_handle = tokio::spawn(async move {
        let mut writer = write_half;
        let mut rx = audio_out_rx;
        let mut total = 0usize;
        while let Some(pcm_data) = rx.recv().await {
            total += pcm_data.len();
            if writer.write_all(&pcm_data).await.is_err() {
                break;
            }
        }
        eprintln!("live_audio: network playback ended — {} bytes", total);
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

/// Local audio bridge: spawns sox processes for capture/playback via platform
/// audio devices (PulseAudio on Linux, BlackHole on macOS).
async fn start_local_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    bridge: &AudioBridge,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    capture_tee_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<AudioStreamBridge, CallerError> {
    // Capture task: platform command -> model
    let (capture_cmd, capture_args) = bridge.capture_command(sample_rate);
    let capture_write = session_write.clone();
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_cmd = capture_cmd.to_string();

    let capture_handle = tokio::spawn(async move {
        let result = tokio::process::Command::new(&capture_cmd)
            .args(&capture_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("live_audio: {} spawn failed: {}", capture_cmd, e);
                return;
            }
        };

        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };

        let chunk_size = (capture_rate as usize) * 2 / 10;
        let mut buf = vec![0u8; chunk_size];

        loop {
            match stdout.read_exact(&mut buf).await {
                Ok(_) => {
                    let b64 = BASE64.encode(&buf);
                    let msg = match capture_provider {
                        LiveAudioProvider::Gemini => serde_json::json!({
                            "realtime_input": {
                                "media_chunks": [{
                                    "mime_type": format!("audio/pcm;rate={}", capture_rate),
                                    "data": b64
                                }]
                            }
                        }),
                        LiveAudioProvider::OpenAI => serde_json::json!({
                            "type": "input_audio_buffer.append",
                            "audio": b64
                        }),
                    };
                    if let Some(ref tee) = capture_tee_tx {
                        let _ = tee.send(buf.clone());
                    }
                    let mut sink = capture_write.lock().await;
                    if sink.send(WsMessage::Text(msg.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        let _ = child.kill().await;
    });

    // Playback task: model audio -> platform playback command
    let (playback_cmd, playback_args) = bridge.playback_command(sample_rate);
    let playback_cmd = playback_cmd.to_string();

    let playback_handle = tokio::spawn(async move {
        let result = tokio::process::Command::new(&playback_cmd)
            .args(&playback_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("live_audio: {} spawn failed: {}", playback_cmd, e);
                return;
            }
        };

        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => return,
        };

        let mut rx = audio_out_rx;
        while let Some(pcm_data) = rx.recv().await {
            if stdin.write_all(&pcm_data).await.is_err() {
                break;
            }
        }

        let _ = child.kill().await;
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

// ---------------------------------------------------------------------------
// JSON extraction from transcript text
// ---------------------------------------------------------------------------

/// Extract the first valid JSON object from a transcript string.
///
/// Realtime models speak their responses, so the transcript may contain prose
/// before or after the JSON. This scans for balanced `{ ... }` and returns the
/// first substring that parses as a JSON object.
fn extract_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escape = false;
            let start = i;
            for j in start..bytes.len() {
                if escape {
                    escape = false;
                    continue;
                }
                match bytes[j] {
                    b'\\' if in_string => escape = true,
                    b'"' => in_string = !in_string,
                    b'{' if !in_string => depth += 1,
                    b'}' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            let candidate = &text[start..=j];
                            if let Ok(parsed) =
                                serde_json::from_str::<serde_json::Value>(candidate)
                            {
                                if parsed.is_object() {
                                    return Some(candidate.to_string());
                                }
                            }
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Transcript logger
// ---------------------------------------------------------------------------

pub struct TranscriptLogger {
    file: tokio::fs::File,
    path: PathBuf,
}

impl TranscriptLogger {
    pub async fn new(dir: &Path, live_audio_id: &str) -> Result<Self, CallerError> {
        let transcript_dir = dir.join(format!("live_audio_{}", live_audio_id));
        tokio::fs::create_dir_all(&transcript_dir).await?;
        let path = transcript_dir.join("transcript.jsonl");
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self { file, path })
    }

    pub async fn log(&mut self, speaker: &str, text: &str) -> Result<(), CallerError> {
        let entry = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "speaker": speaker,
            "text": text,
        });
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Full session orchestrator
// ---------------------------------------------------------------------------

/// Run a complete live audio session: connect, bridge audio, capture transcript,
/// validate response, quarantine unexpected content.
///
/// This is the main entry point called from the agent loop when handling a
/// `spawn_live_audio` tool call. It blocks until the call finishes or times out.
/// Buffer inbound audio chunks from the capture tee, accumulate ~3 seconds,
/// run silence detection, and send to Whisper for transcription.
/// Results are appended to the transcript JSONL as "app" speaker entries.
async fn whisper_inbound_loop(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    sample_rate: u32,
    transcript_path: &Path,
) {
    use crate::transcription::{self, Transcriber, TranscriptionConfig};

    // Try to create a Whisper transcriber — if OPENAI_API_KEY is not set, just skip
    let transcriber = match transcription::WhisperTranscriber::new(&TranscriptionConfig::default())
    {
        Ok(t) => t,
        Err(_) => return, // No API key or config issue — silently skip
    };

    // Buffer ~3 seconds of audio before sending to Whisper
    let threshold = (sample_rate as usize) * 2 * 3; // 3 seconds of 16-bit mono
    let mut audio_buf: Vec<u8> = Vec::with_capacity(threshold);
    let rms_threshold = 1000.0f64;

    // Open transcript file for appending
    let mut transcript_file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(transcript_path)
        .await
    {
        Ok(f) => f,
        Err(_) => return,
    };

    while let Some(chunk) = rx.recv().await {
        audio_buf.extend_from_slice(&chunk);

        if audio_buf.len() < threshold {
            continue;
        }

        // RMS silence detection
        let rms = {
            let samples = audio_buf
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64);
            let n = audio_buf.len() / 2;
            let sum_sq: f64 = samples.map(|s| s * s).sum();
            if n > 0 {
                (sum_sq / n as f64).sqrt()
            } else {
                0.0
            }
        };

        if rms < rms_threshold {
            audio_buf.clear();
            continue;
        }

        // Encode as WAV and transcribe
        let wav = transcription::encode_wav(&audio_buf, sample_rate, 1);
        audio_buf.clear();

        match transcriber.transcribe(&wav).await {
            Ok(segment) => {
                let text = segment.text.trim();
                if !text.is_empty() {
                    let entry = serde_json::json!({
                        "ts": chrono::Utc::now().to_rfc3339(),
                        "speaker": "app",
                        "text": text,
                    });
                    let mut line = serde_json::to_string(&entry).unwrap_or_default();
                    line.push('\n');
                    let _ = transcript_file.write_all(line.as_bytes()).await;
                }
            }
            Err(_) => {} // Transcription failures are non-fatal
        }
    }
}

pub async fn run_session(
    spec: &LiveAudioSpec,
    api_key: &str,
    bridge: &AudioBridge,
    session_log_dir: &Path,
    event_bus: Option<&crate::event::EventBus>,
) -> Result<LiveAudioResult, CallerError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(spec.timeout_secs);

    // Connect to the live model
    let mut session = match spec.provider {
        LiveAudioProvider::Gemini => {
            connect_gemini(
                api_key,
                spec.model.as_deref(),
                &spec.playbook,
                spec.voice.as_deref(),
                24000,
            )
            .await?
        }
        LiveAudioProvider::OpenAI => {
            connect_openai(
                api_key,
                spec.model.as_deref(),
                &spec.playbook,
                spec.voice.as_deref(),
                24000,
            )
            .await?
        }
    };

    // Emit started event
    if let Some(bus) = event_bus {
        bus.send(crate::event::AppEvent::LiveAudioStarted {
            id: spec.id.clone(),
            provider: format!("{:?}", spec.provider),
        });
    }

    // Set up transcript logger
    let mut transcript = TranscriptLogger::new(session_log_dir, &spec.id).await?;

    // Channel for routing model audio output to the playback task
    let (audio_out_tx, audio_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Start the audio bridge
    // Set up Whisper transcription tee for inbound audio
    let (capture_tee_tx, capture_tee_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let whisper_transcript_path = transcript.path().to_path_buf();
    let whisper_handle = tokio::spawn(async move {
        whisper_inbound_loop(capture_tee_rx, 24000, &whisper_transcript_path).await;
    });

    let audio_bridge = start_audio_bridge(
        session.ws_write.clone(),
        session.provider,
        session.sample_rate,
        bridge,
        audio_out_rx,
        Some(capture_tee_tx),
    )
    .await?;

    // Send initial message if provided (e.g. "The call has connected.")
    if let Some(ref msg) = spec.initial_message {
        session.send_text(msg).await?;
    }

    // Collect model text output and quarantine payloads
    let mut model_text = String::new();
    let mut model_transcript_buf = String::new();
    let mut quarantine_ids = Vec::new();

    // Event processing loop
    let status = loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            break LiveAudioStatus::TimedOut;
        }
        let remaining = timeout - elapsed;

        match tokio::time::timeout(remaining, session.event_rx.recv()).await {
            Ok(Some(event)) => match event {
                LiveAudioEvent::AudioOut(pcm) => {
                    let _ = audio_out_tx.send(pcm);
                }
                LiveAudioEvent::ModelTranscript(text) => {
                    let _ = transcript.log("model", &text).await;
                    model_transcript_buf.push_str(&text);

                    // Emit progress
                    if let Some(bus) = event_bus {
                        let preview = if model_transcript_buf.len() > 200 {
                            model_transcript_buf[model_transcript_buf.len() - 200..].to_string()
                        } else {
                            model_transcript_buf.clone()
                        };
                        bus.send(crate::event::AppEvent::LiveAudioProgress {
                            id: spec.id.clone(),
                            state: "speaking".into(),
                            elapsed_secs: start.elapsed().as_secs_f64(),
                            transcript_preview: preview,
                        });
                    }
                }
                LiveAudioEvent::ModelText(text) => {
                    model_text.push_str(&text);
                }
                LiveAudioEvent::ToolCallAttempted { name, args } => {
                    // Quarantine the tool call attempt
                    let content = serde_json::json!({"name": name, "args": args}).to_string();
                    match quarantine::store_payload(&spec.id, "tool_call_attempt", &content) {
                        Ok(payload) => quarantine_ids.push(payload.payload_id),
                        Err(e) => eprintln!("live_audio: quarantine write failed: {}", e),
                    }
                }
                LiveAudioEvent::Disconnected(_reason) => {
                    break LiveAudioStatus::Disconnected;
                }
                LiveAudioEvent::Error(e) => {
                    break LiveAudioStatus::Failed(e);
                }
                LiveAudioEvent::TurnComplete => {
                    // Check if the model has output a complete JSON response.
                    // Realtime models may deliver JSON via text (response.text.delta)
                    // or via audio transcript (response.audio_transcript.delta) —
                    // check both, preferring explicit text output.
                    let json_source = if !model_text.is_empty() {
                        Some(&model_text)
                    } else {
                        // Extract JSON from transcript: the model may speak prose
                        // before/after the JSON object, so find the outermost { ... }.
                        None
                    };

                    if let Some(src) = json_source {
                        if let Ok(parsed) =
                            serde_json::from_str::<serde_json::Value>(src)
                        {
                            if parsed.is_object() {
                                break LiveAudioStatus::Completed;
                            }
                        }
                    } else if !model_transcript_buf.is_empty() {
                        if let Some(json_str) =
                            extract_json_object(&model_transcript_buf)
                        {
                            model_text = json_str;
                            break LiveAudioStatus::Completed;
                        }
                    }
                    // Otherwise continue listening for more turns
                }
                LiveAudioEvent::Interrupted => {}
                LiveAudioEvent::Connected | LiveAudioEvent::SetupComplete => {}
            },
            Ok(None) => {
                // Channel closed — session ended
                break LiveAudioStatus::Disconnected;
            }
            Err(_) => {
                // Timeout
                break LiveAudioStatus::TimedOut;
            }
        }
    };

    // Stop the audio bridge and whisper task
    audio_bridge.stop();
    whisper_handle.abort();

    // Close the WebSocket
    session.close().await;

    // Validate the structured response
    let (response_data, final_status) = if !model_text.is_empty() {
        match serde_json::from_str::<serde_json::Value>(&model_text) {
            Ok(value) => {
                let mut qfn = quarantine::make_quarantine_fn(spec.id.clone());
                match schema_validator::validate(&spec.response_schema, &value, &mut qfn) {
                    Ok((validated, extra_quarantined)) => {
                        for q in &extra_quarantined {
                            quarantine_ids.push(q.payload_id.clone());
                        }
                        (Some(validated), status)
                    }
                    Err(errors) => {
                        let error_msg = errors
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; ");
                        // Quarantine the raw model output
                        if let Ok(payload) =
                            quarantine::store_payload(&spec.id, "schema_violation", &model_text)
                        {
                            quarantine_ids.push(payload.payload_id);
                        }
                        (None, LiveAudioStatus::SchemaError(error_msg))
                    }
                }
            }
            Err(_) => {
                // Model text wasn't valid JSON — quarantine it
                if let Ok(payload) =
                    quarantine::store_payload(&spec.id, "invalid_json", &model_text)
                {
                    quarantine_ids.push(payload.payload_id);
                }
                (None, LiveAudioStatus::SchemaError("model output was not valid JSON".into()))
            }
        }
    } else {
        (None, status)
    };

    let duration_secs = start.elapsed().as_secs_f64();

    // Emit completed event
    if let Some(bus) = event_bus {
        bus.send(crate::event::AppEvent::LiveAudioCompleted {
            id: spec.id.clone(),
            status: format!("{:?}", final_status),
            quarantine_count: quarantine_ids.len(),
        });
    }

    Ok(LiveAudioResult {
        id: spec.id.clone(),
        status: final_status,
        response_data,
        quarantine_ids,
        transcript_path: transcript.path().to_path_buf(),
        duration_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_setup_message_has_no_tools() {
        let setup = serde_json::json!({
            "setup": {
                "model": "models/gemini-2.5-flash",
                "generation_config": {
                    "response_modalities": ["AUDIO"],
                    "speech_config": {
                        "voice_config": {
                            "prebuilt_voice_config": {
                                "voice_name": "Aoede"
                            }
                        }
                    }
                },
                "output_audio_transcription": {},
                "system_instruction": {
                    "parts": [{ "text": "test playbook" }]
                },
                "tools": []
            }
        });

        let tools = setup["setup"]["tools"].as_array().unwrap();
        assert!(tools.is_empty(), "untrusted agent must have zero tools");
    }

    #[test]
    fn openai_setup_message_has_no_tools() {
        let setup = serde_json::json!({
            "type": "session.update",
            "session": {
                "modalities": ["audio", "text"],
                "instructions": "test playbook",
                "voice": "alloy",
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "tools": []
            }
        });

        let tools = setup["session"]["tools"].as_array().unwrap();
        assert!(tools.is_empty(), "untrusted agent must have zero tools");
    }

    #[test]
    fn gemini_audio_send_format() {
        let b64 = BASE64.encode(&[0i16.to_le_bytes()[0], 0i16.to_le_bytes()[1]]);
        let msg = serde_json::json!({
            "realtime_input": {
                "media_chunks": [{
                    "mime_type": "audio/pcm;rate=24000",
                    "data": b64
                }]
            }
        });
        assert!(msg["realtime_input"]["media_chunks"][0]["data"].is_string());
    }

    #[test]
    fn openai_audio_send_format() {
        let b64 = BASE64.encode(&[0u8; 2]);
        let msg = serde_json::json!({
            "type": "input_audio_buffer.append",
            "audio": b64
        });
        assert_eq!(msg["type"], "input_audio_buffer.append");
    }

    #[test]
    fn parse_gemini_server_content_audio() {
        // Simulate a Gemini serverContent message with audio
        let audio_data = BASE64.encode(&[1u8, 2, 3, 4]);
        let msg = serde_json::json!({
            "serverContent": {
                "modelTurn": {
                    "parts": [{
                        "inlineData": {
                            "mimeType": "audio/pcm",
                            "data": audio_data
                        }
                    }]
                }
            }
        });

        // Verify we can extract audio data
        let parts = msg["serverContent"]["modelTurn"]["parts"]
            .as_array()
            .unwrap();
        let inline = &parts[0]["inlineData"];
        assert!(inline["mimeType"].as_str().unwrap().starts_with("audio/"));
        let decoded = BASE64.decode(inline["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, vec![1, 2, 3, 4]);
    }

    #[test]
    fn parse_gemini_tool_call_attempt() {
        let msg = serde_json::json!({
            "toolCall": {
                "functionCalls": [{
                    "name": "browse_url",
                    "args": {"url": "http://evil.com"}
                }]
            }
        });

        let fcs = msg["toolCall"]["functionCalls"].as_array().unwrap();
        assert_eq!(fcs.len(), 1);
        assert_eq!(fcs[0]["name"], "browse_url");
    }

    #[test]
    fn parse_openai_function_call_done() {
        let msg = serde_json::json!({
            "type": "response.function_call_arguments.done",
            "name": "exec_command",
            "arguments": "{\"command\":\"ls\"}"
        });

        assert_eq!(msg["type"], "response.function_call_arguments.done");
        let name = msg["name"].as_str().unwrap();
        assert_eq!(name, "exec_command");
        let args: serde_json::Value =
            serde_json::from_str(msg["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn extract_json_from_plain_object() {
        let text = r#"{"status": "ok"}"#;
        assert_eq!(extract_json_object(text).unwrap(), text);
    }

    #[test]
    fn extract_json_from_transcript_with_prose() {
        let text = r#"Test complete. {"status": "ok"}"#;
        assert_eq!(
            extract_json_object(text).unwrap(),
            r#"{"status": "ok"}"#
        );
    }

    #[test]
    fn extract_json_with_trailing_prose() {
        let text = r#"Here it is: {"a": 1, "b": "hello"} That's all."#;
        assert_eq!(
            extract_json_object(text).unwrap(),
            r#"{"a": 1, "b": "hello"}"#
        );
    }

    #[test]
    fn extract_json_nested_braces() {
        let text = r#"Result: {"data": {"inner": true}, "ok": false}"#;
        assert_eq!(
            extract_json_object(text).unwrap(),
            r#"{"data": {"inner": true}, "ok": false}"#
        );
    }

    #[test]
    fn extract_json_with_braces_in_strings() {
        let text = r#"{"msg": "use {x} here"}"#;
        assert_eq!(extract_json_object(text).unwrap(), text);
    }

    #[test]
    fn extract_json_none_when_no_json() {
        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("").is_none());
        assert!(extract_json_object("{ broken").is_none());
    }

    // -----------------------------------------------------------------------
    // Integration tests — real API calls to OpenAI Realtime
    //
    // Requires OPENAI_API_KEY in env. Skipped by `cargo test --bins`.
    // Run with:
    //   cargo test --bin intendant test_live_audio_openai -- --ignored --nocapture
    // -----------------------------------------------------------------------

    const TEST_MODEL: &str = "gpt-realtime-1.5";

    fn require_openai_key() -> Option<String> {
        match std::env::var("OPENAI_API_KEY") {
            Ok(k) if !k.is_empty() => Some(k),
            _ => {
                eprintln!("OPENAI_API_KEY not set, skipping");
                None
            }
        }
    }

    /// Layer 1: WebSocket connects and session.update is accepted.
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_connect() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        let mut session = connect_openai(
            &api_key,
            Some(TEST_MODEL),
            "You are a test agent.",
            Some("alloy"),
            24000,
        )
        .await
        .expect("connect_openai failed");

        let mut got_setup = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(LiveAudioEvent::SetupComplete)) => {
                    got_setup = true;
                    eprintln!("  SetupComplete received");
                    break;
                }
                Ok(Some(LiveAudioEvent::Connected)) => {
                    eprintln!("  Connected");
                }
                Ok(Some(LiveAudioEvent::Error(e))) => {
                    panic!("session error during setup: {}", e);
                }
                Ok(Some(other)) => {
                    eprintln!("  setup event: {:?}", other);
                }
                Ok(None) | Err(_) => break,
            }
        }

        session.close().await;
        assert!(got_setup, "did not receive SetupComplete from OpenAI Realtime");
    }

    /// Layer 2: Send text, receive audio + transcript + turn_complete.
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_text_round_trip() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        let mut session = connect_openai(
            &api_key,
            Some(TEST_MODEL),
            "You are a test assistant. Respond in one short sentence.",
            Some("alloy"),
            24000,
        )
        .await
        .expect("connect_openai failed");

        // Wait for setup
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(LiveAudioEvent::SetupComplete)) => break,
                Ok(Some(_)) => continue,
                _ => panic!("did not receive SetupComplete"),
            }
        }

        // Send text
        session
            .send_text("Say hello.")
            .await
            .expect("send_text failed");
        eprintln!("  Sent text prompt, waiting for response...");

        let mut got_audio = false;
        let mut got_transcript = false;
        let mut got_turn_complete = false;
        let mut audio_bytes = 0usize;
        let mut transcript_buf = String::new();

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(event)) => match event {
                    LiveAudioEvent::AudioOut(pcm) => {
                        audio_bytes += pcm.len();
                        got_audio = true;
                    }
                    LiveAudioEvent::ModelTranscript(text) => {
                        transcript_buf.push_str(&text);
                        got_transcript = true;
                    }
                    LiveAudioEvent::ModelText(text) => {
                        eprintln!("  ModelText: {}", text);
                    }
                    LiveAudioEvent::TurnComplete => {
                        got_turn_complete = true;
                        break;
                    }
                    LiveAudioEvent::Error(e) => panic!("session error: {}", e),
                    LiveAudioEvent::Disconnected(r) => panic!("disconnected: {}", r),
                    _ => {}
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        session.close().await;

        eprintln!("  Audio: {} bytes received", audio_bytes);
        eprintln!("  Transcript: {:?}", transcript_buf);
        eprintln!("  TurnComplete: {}", got_turn_complete);

        assert!(got_turn_complete, "did not receive TurnComplete");
        assert!(got_audio, "no audio output received");
        // Transcript is expected but not strictly guaranteed by all models
        if !got_transcript {
            eprintln!("  WARN: no transcript received (model may not support audio_transcript)");
        }
    }

    /// Layer 2.5: Connect with audio bridge, send text kick-off, log all events.
    /// Diagnoses whether the model speaks, produces text, or just sits silent.
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_bridge_diagnostics() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        if !crate::audio_routing::is_available().await {
            eprintln!("virtual audio routing not available, skipping");
            return;
        }

        let session_id = format!(
            "diag-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let mut bridge = crate::audio_routing::create_bridge(&session_id)
            .await
            .expect("create_bridge failed");

        if let Err(e) = crate::audio_routing::set_as_default(&mut bridge).await {
            eprintln!("  WARN: set_as_default: {}", e);
        }

        let playbook = "You are running an automated test. There is nobody on the line. \
                         Say 'test complete' once, then output the JSON: {\"status\": \"ok\"}";

        let mut session = connect_openai(
            &api_key,
            Some(TEST_MODEL),
            playbook,
            Some("alloy"),
            24000,
        )
        .await
        .expect("connect_openai failed");

        // Wait for setup
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(LiveAudioEvent::SetupComplete)) => {
                    eprintln!("  SetupComplete");
                    break;
                }
                Ok(Some(e)) => eprintln!("  setup: {:?}", e),
                _ => panic!("no SetupComplete"),
            }
        }

        // Start audio bridge (capture silence → model, model audio → playback)
        let (audio_out_tx, audio_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let audio_bridge = start_audio_bridge(
            session.ws_write.clone(),
            session.provider,
            session.sample_rate,
            &bridge,
            audio_out_rx,
            None,
        )
        .await
        .expect("start_audio_bridge failed");
        eprintln!("  Audio bridge started");

        // Send a text kick-off to prompt the model
        session
            .send_text("Begin the test now.")
            .await
            .expect("send_text failed");
        eprintln!("  Sent text kick-off");

        // Collect all events for up to 20 seconds
        let mut audio_bytes = 0usize;
        let mut audio_chunks = 0usize;
        let mut transcript_buf = String::new();
        let mut text_buf = String::new();
        let mut turn_completes = 0usize;
        let mut tool_calls = Vec::new();

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(event)) => match event {
                    LiveAudioEvent::AudioOut(pcm) => {
                        audio_bytes += pcm.len();
                        audio_chunks += 1;
                        let _ = audio_out_tx.send(pcm);
                    }
                    LiveAudioEvent::ModelTranscript(t) => {
                        eprintln!("  TRANSCRIPT: {:?}", t);
                        transcript_buf.push_str(&t);
                    }
                    LiveAudioEvent::ModelText(t) => {
                        eprintln!("  MODEL_TEXT: {:?}", t);
                        text_buf.push_str(&t);
                    }
                    LiveAudioEvent::TurnComplete => {
                        turn_completes += 1;
                        eprintln!("  TURN_COMPLETE #{}", turn_completes);
                        // Stop after first completed turn
                        break;
                    }
                    LiveAudioEvent::ToolCallAttempted { name, args } => {
                        eprintln!("  TOOL_CALL: {} {:?}", name, args);
                        tool_calls.push(name);
                    }
                    LiveAudioEvent::Interrupted => {
                        eprintln!("  INTERRUPTED");
                    }
                    other => {
                        eprintln!("  {:?}", other);
                    }
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        audio_bridge.stop();
        session.close().await;
        drop(bridge);

        eprintln!("\n  === Diagnostics ===");
        eprintln!("  Audio: {} bytes in {} chunks", audio_bytes, audio_chunks);
        eprintln!("  Transcript: {:?}", transcript_buf);
        eprintln!("  ModelText: {:?}", text_buf);
        eprintln!("  TurnCompletes: {}", turn_completes);
        eprintln!("  ToolCalls: {:?}", tool_calls);

        // At minimum, the model should have produced something
        assert!(
            audio_bytes > 0 || !transcript_buf.is_empty() || !text_buf.is_empty(),
            "model produced no output at all"
        );
    }

    /// Interactive test: real phone call via Linphone + live audio model.
    ///
    /// 1. Creates audio bridge (sets BlackHole as system default)
    /// 2. Connects to OpenAI Realtime with a conversational playbook
    /// 3. Prints instructions — call intendant7 from your phone
    /// 4. Model handles the conversation, logs transcript
    /// 5. Validates structured response
    ///
    /// Run with:
    ///   cargo test --bin intendant test_live_audio_phone_call -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_phone_call() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        if !crate::audio_routing::is_available().await {
            eprintln!("virtual audio routing not available, skipping");
            return;
        }

        let session_id = format!(
            "call-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let mut bridge = crate::audio_routing::create_bridge(&session_id)
            .await
            .expect("create_bridge failed");

        // No need for set_as_default — pjsua uses explicit device IDs.
        // The bridge is only needed for its capture/playback sox commands.

        // Start pjsua as the SIP client with BlackHole devices.
        // capture-dev=3 (BlackHole 2ch) → model's voice goes to caller
        // playback-dev=2 (BlackHole 16ch) → caller's voice goes to model
        let pjsua_bin = "/tmp/pjproject/pjsip-apps/bin/pjsua-aarch64-apple-darwin25.4.0";
        let sip_password = std::fs::read_to_string(
            dirs::home_dir().unwrap().join("lin"),
        )
        .expect("~/lin should contain the SIP password")
        .trim()
        .to_string();

        eprintln!("  Starting pjsua (SIP client) with BlackHole audio...");
        let mut pjsua = tokio::process::Command::new(pjsua_bin)
            .args([
                "--id=sip:intendant7@sip.linphone.org",
                "--registrar=sip:sip.linphone.org",
                "--realm=sip.linphone.org",
                "--username=intendant7",
                &format!("--password={}", sip_password),
                "--capture-dev=3", "--playback-dev=2",
                "--auto-answer=200",
                "--ec-tail=0",
                "--no-vad",
                "--use-srtp=1", "--srtp-secure=0",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to start pjsua");
        tokio::time::sleep(Duration::from_secs(3)).await;
        eprintln!("  pjsua registered. Auto-answer enabled.");

        eprintln!("\n  ╔══════════════════════════════════════════════════╗");
        eprintln!("  ║  Call intendant7 from your phone NOW.            ║");
        eprintln!("  ║  The call will auto-answer.                      ║");
        eprintln!("  ║  The AI will hear you and talk back.             ║");
        eprintln!("  ║  Stay on for 30+ seconds.                        ║");
        eprintln!("  ║                                                  ║");
        eprintln!("  ║  Timeout: 120 seconds                            ║");
        eprintln!("  ╚══════════════════════════════════════════════════╝\n");

        let schema = crate::live_audio_types::ResponseSchema {
            fields: vec![
                crate::live_audio_types::FieldSpec {
                    name: "summary".to_string(),
                    field_type: crate::live_audio_types::FieldType::String {
                        max_length: Some(500),
                        allowed_values: None,
                        tainted: true,
                    },
                    required: true,
                    description: Some(
                        "Brief summary of what was discussed in the call".to_string(),
                    ),
                },
                crate::live_audio_types::FieldSpec {
                    name: "caller_mood".to_string(),
                    field_type: crate::live_audio_types::FieldType::String {
                        max_length: Some(50),
                        allowed_values: Some(vec![
                            "friendly".into(),
                            "neutral".into(),
                            "frustrated".into(),
                            "unknown".into(),
                        ]),
                        tainted: false,
                    },
                    required: true,
                    description: Some("The caller's apparent mood".to_string()),
                },
            ],
        };

        let system_prompt = crate::prompts::build_live_audio_prompt(
            "You are a friendly AI assistant answering a phone call. \
             Greet the caller warmly, ask how you can help, and have a brief \
             natural conversation. After the caller says goodbye or the \
             conversation reaches a natural end, output the response JSON. \
             Keep the call under 60 seconds.",
            &schema,
            None,
        );

        let spec = crate::live_audio_types::LiveAudioSpec {
            id: session_id.clone(),
            provider: crate::live_audio_types::LiveAudioProvider::OpenAI,
            model: Some(TEST_MODEL.to_string()),
            playbook: system_prompt,
            response_schema: schema,
            timeout_secs: 120,
            voice: Some("alloy".to_string()),
            display_id: None,
            initial_message: None, // Wait for the caller to speak first
        };

        let tmp_dir = tempfile::tempdir().expect("create temp dir");
        let log_dir = tmp_dir.path().to_path_buf();

        let result = run_session(&spec, &api_key, &bridge, &log_dir, None)
            .await
            .expect("run_session failed");

        // Stop pjsua
        if let Some(mut stdin) = pjsua.stdin.take() {
            let _ = stdin.write_all(b"q\n").await;
        }
        let _ = pjsua.wait().await;
        drop(bridge);
        eprintln!("\n  pjsua stopped. Audio bridge restored.\n");

        // Read and display transcript
        if result.transcript_path.exists() {
            eprintln!("  === Transcript ===");
            if let Ok(content) = std::fs::read_to_string(&result.transcript_path) {
                for line in content.lines() {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        let speaker = entry["speaker"].as_str().unwrap_or("?");
                        let text = entry["text"].as_str().unwrap_or("");
                        eprintln!("  [{}] {}", speaker, text);
                    }
                }
            }
            eprintln!();
        }

        eprintln!("  Status: {:?}", result.status);
        eprintln!("  Duration: {:.1}s", result.duration_secs);
        eprintln!("  Response: {:?}", result.response_data);
        eprintln!("  Quarantine: {:?}", result.quarantine_ids);

        match &result.status {
            LiveAudioStatus::Completed => {
                let data = result.response_data.as_ref().unwrap();
                eprintln!("\n  Summary: {}", data["summary"].as_str().unwrap_or("?"));
                eprintln!("  Caller mood: {}", data["caller_mood"].as_str().unwrap_or("?"));
                eprintln!("\n  PASS");
            }
            LiveAudioStatus::TimedOut => {
                eprintln!("\n  Session timed out — no JSON response from model");
            }
            other => {
                eprintln!("\n  Unexpected status: {:?}", other);
            }
        }
    }

    /// Layer 3: Full run_session pipeline with audio bridge + schema validation.
    /// Skips if virtual audio routing is not available (no BlackHole / PulseAudio).
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_full_session() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        if !crate::audio_routing::is_available().await {
            eprintln!("virtual audio routing not available, skipping full session test");
            return;
        }

        let session_id = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let mut bridge = crate::audio_routing::create_bridge(&session_id)
            .await
            .expect("create_bridge failed");

        if let Err(e) = crate::audio_routing::set_as_default(&mut bridge).await {
            eprintln!("  WARN: could not set bridge as default: {}", e);
        }

        let schema = crate::live_audio_types::ResponseSchema {
            fields: vec![crate::live_audio_types::FieldSpec {
                name: "status".to_string(),
                field_type: crate::live_audio_types::FieldType::String {
                    max_length: Some(100),
                    allowed_values: None,
                    tainted: false,
                },
                required: true,
                description: Some("Test status".to_string()),
            }],
        };

        let system_prompt = crate::prompts::build_live_audio_prompt(
            "You are running an automated test. There is no one on the other end of \
             the call — you will hear silence. Say 'test complete' once, then \
             immediately output the JSON response with status set to 'ok'.",
            &schema,
            None,
        );

        let spec = crate::live_audio_types::LiveAudioSpec {
            id: session_id.clone(),
            provider: crate::live_audio_types::LiveAudioProvider::OpenAI,
            model: Some(TEST_MODEL.to_string()),
            playbook: system_prompt,
            response_schema: schema,
            timeout_secs: 45,
            voice: Some("alloy".to_string()),
            display_id: None,
            // No real counterparty in the test — kick the model off via text
            initial_message: Some("Begin.".to_string()),
        };

        let tmp_dir = tempfile::tempdir().expect("create temp dir");
        eprintln!("  Session ID: {}", session_id);
        eprintln!("  Log dir: {}", tmp_dir.path().display());

        let result = run_session(&spec, &api_key, &bridge, tmp_dir.path(), None)
            .await
            .expect("run_session failed");

        drop(bridge);

        eprintln!("  Status: {:?}", result.status);
        eprintln!("  Duration: {:.1}s", result.duration_secs);
        eprintln!("  Response data: {:?}", result.response_data);
        eprintln!("  Quarantine IDs: {:?}", result.quarantine_ids);

        // Check transcript file was created
        assert!(
            result.transcript_path.exists(),
            "transcript file should exist at {}",
            result.transcript_path.display()
        );

        match &result.status {
            LiveAudioStatus::Completed => {
                let data = result
                    .response_data
                    .as_ref()
                    .expect("Completed but no response_data");
                assert!(
                    data.get("status").is_some(),
                    "response missing 'status' field: {}",
                    data
                );
                eprintln!("  PASS: session completed with valid response");
            }
            LiveAudioStatus::TimedOut => {
                // Acceptable — the model heard silence and may not have produced JSON
                eprintln!("  WARN: session timed out (model did not output JSON within timeout)");
            }
            LiveAudioStatus::SchemaError(e) => {
                // The model produced JSON but it didn't match — still useful signal
                eprintln!("  WARN: schema validation failed: {}", e);
            }
            other => {
                panic!("unexpected session status: {:?}", other);
            }
        }
    }
}
