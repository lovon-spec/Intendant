use crate::error::CallerError;
use crate::live_audio_types::*;
use crate::pulse_audio::PulseAudioBridge;
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
pub struct AudioBridge {
    capture_handle: tokio::task::JoinHandle<()>,
    playback_handle: tokio::task::JoinHandle<()>,
}

impl AudioBridge {
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
    bridge: &PulseAudioBridge,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<AudioBridge, CallerError> {
    // Capture task: parec -> model
    let monitor_source = bridge.app_audio_monitor().to_string();
    let capture_write = session_write.clone();
    let capture_rate = sample_rate;
    let capture_provider = provider;

    let capture_handle = tokio::spawn(async move {
        let result = tokio::process::Command::new("parec")
            .args([
                &format!("--device={}", monitor_source),
                "--format=s16le",
                &format!("--rate={}", capture_rate),
                "--channels=1",
                "--raw",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("live_audio: parec spawn failed: {}", e);
                return;
            }
        };

        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };

        // Read in ~100ms chunks (sample_rate * 2 bytes/sample * 0.1s)
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

    // Playback task: model audio -> pacat
    let playback_sink = bridge.model_output_sink().to_string();
    let playback_rate = sample_rate;

    let playback_handle = tokio::spawn(async move {
        let result = tokio::process::Command::new("pacat")
            .args([
                "--playback",
                &format!("--device={}", playback_sink),
                "--format=s16le",
                &format!("--rate={}", playback_rate),
                "--channels=1",
                "--raw",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("live_audio: pacat spawn failed: {}", e);
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

    Ok(AudioBridge {
        capture_handle,
        playback_handle,
    })
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
pub async fn run_session(
    spec: &LiveAudioSpec,
    api_key: &str,
    bridge: &PulseAudioBridge,
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
    let audio_bridge = start_audio_bridge(
        session.ws_write.clone(),
        session.provider,
        session.sample_rate,
        bridge,
        audio_out_rx,
    )
    .await?;

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
                LiveAudioEvent::TurnComplete | LiveAudioEvent::Interrupted => {
                    // For TurnComplete after the model has spoken, we continue
                    // listening for more turns until timeout or disconnect
                }
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

    // Stop the audio bridge
    audio_bridge.stop();

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
}
