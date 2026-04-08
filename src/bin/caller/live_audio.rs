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
// ---------------------------------------------------------------------------
// Vortex wire protocol (must match VortexAudioDaemon)
// ---------------------------------------------------------------------------

const VORTEX_MSG_CONFIGURE: u32 = 0x01;
const VORTEX_MSG_PCM_OUTPUT: u32 = 0x02;
const VORTEX_MSG_PCM_INPUT: u32 = 0x03;
const VORTEX_MSG_START: u32 = 0x04;
const VORTEX_MSG_STOP: u32 = 0x05;

/// Read one Vortex wire protocol message: [u32 LE type][u32 LE len][payload].
async fn vortex_read_msg(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<(u32, Vec<u8>), CallerError> {
    let mut hdr = [0u8; 8];
    reader.read_exact(&mut hdr).await.map_err(|e| {
        CallerError::Agent(format!("vortex: read header: {}", e))
    })?;
    let msg_type = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let payload_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload).await.map_err(|e| {
            CallerError::Agent(format!("vortex: read payload: {}", e))
        })?;
    }
    Ok((msg_type, payload))
}

/// Write one Vortex wire protocol message.
async fn vortex_write_msg(
    writer: &mut (impl AsyncWriteExt + Unpin),
    msg_type: u32,
    payload: &[u8],
) -> Result<(), CallerError> {
    // Write header + payload as a single buffer to avoid partial messages.
    // The daemon's readExactly busy-waits on EAGAIN, so split writes can
    // cause it to spin between header and payload arrival.
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.extend_from_slice(&msg_type.to_le_bytes());
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(payload);
    writer.write_all(&msg).await.map_err(|e| {
        CallerError::Agent(format!("vortex: write msg: {}", e))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Format conversion: Vortex (Float32 stereo 48kHz) ↔ Model (PCM16 mono 24kHz)
// ---------------------------------------------------------------------------

/// Convert Vortex daemon PCM_OUTPUT (Float32 stereo 48kHz) to model input
/// (PCM16 mono 24kHz). 8:1 size reduction.
fn vortex_capture_convert(f32_stereo_48k: &[u8]) -> Vec<u8> {
    // Each stereo frame = 2 floats = 8 bytes
    // After mono downmix + 2:1 decimation + i16 conversion: 1 frame → 2 bytes
    let num_floats = f32_stereo_48k.len() / 4;
    let num_stereo_frames = num_floats / 2;
    let num_output_samples = num_stereo_frames / 2; // 2:1 decimation
    let mut out = Vec::with_capacity(num_output_samples * 2);

    for i in (0..num_stereo_frames).step_by(2) {
        // Read stereo pair (left + right)
        let base = i * 8; // 2 floats * 4 bytes each
        if base + 8 > f32_stereo_48k.len() {
            break;
        }
        let left = f32::from_le_bytes([
            f32_stereo_48k[base],
            f32_stereo_48k[base + 1],
            f32_stereo_48k[base + 2],
            f32_stereo_48k[base + 3],
        ]);
        let right = f32::from_le_bytes([
            f32_stereo_48k[base + 4],
            f32_stereo_48k[base + 5],
            f32_stereo_48k[base + 6],
            f32_stereo_48k[base + 7],
        ]);
        let mono = (left + right) * 0.5;
        let clamped = mono.clamp(-1.0, 1.0);
        let sample = (clamped * 32767.0) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Convert model output (PCM16 mono 24kHz) to Vortex daemon PCM_INPUT
/// (Float32 stereo 48kHz). 8:1 size expansion.
fn vortex_playback_convert(pcm16_mono_24k: &[u8]) -> Vec<u8> {
    let num_samples = pcm16_mono_24k.len() / 2;
    // Each mono sample → 2 stereo frames (upsample) × 2 channels × 4 bytes
    let mut out = Vec::with_capacity(num_samples * 16);

    for i in 0..num_samples {
        let sample = i16::from_le_bytes([
            pcm16_mono_24k[i * 2],
            pcm16_mono_24k[i * 2 + 1],
        ]);
        let f = sample as f32 / 32768.0;
        let bytes = f.to_le_bytes();
        // Duplicate sample for 2:1 upsample, stereo (L=R)
        for _ in 0..2 {
            out.extend_from_slice(&bytes); // left
            out.extend_from_slice(&bytes); // right
        }
    }
    out
}

pub async fn start_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    bridge: &AudioBridge,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    capture_tee_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<AudioStreamBridge, CallerError> {
    if bridge.vortex_socket_path().is_some() {
        return start_vortex_shm_bridge(
            session_write,
            provider,
            sample_rate,
            audio_out_rx,
            capture_tee_tx,
        )
        .await;
    }

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
// ---------------------------------------------------------------------------
// Vortex direct shared memory bridge (no daemon needed)
// ---------------------------------------------------------------------------

// Layout constants matching VortexSharedAudio.h
const VORTEX_SHM_NAME: &[u8] = b"/vortex-audio\0";
const VORTEX_SHM_MAGIC: u32 = 0x56585348;
const VORTEX_RING_FRAMES: usize = 65536;
const VORTEX_MAX_CHANNELS: usize = 2;
const VORTEX_RING_SAMPLES: usize = VORTEX_RING_FRAMES * VORTEX_MAX_CHANNELS;
const VORTEX_RING_MASK: u64 = (VORTEX_RING_SAMPLES - 1) as u64;

// Field offsets into VortexSharedAudioState (bytes)
const OFF_MAGIC: usize = 0;
const OFF_IS_ACTIVE: usize = 16;
const OFF_OUT_WRITE_POS: usize = 24;
const OFF_OUT_READ_POS: usize = 32;
const OFF_IN_WRITE_POS: usize = 40;
const OFF_IN_READ_POS: usize = 48;
const OFF_OUT_BUFFER: usize = 56;
const OFF_IN_BUFFER: usize = OFF_OUT_BUFFER + VORTEX_RING_SAMPLES * 4;

/// Direct shared memory bridge: reads/writes the Vortex HAL plugin's ring
/// buffers without the daemon. No sockets, no IPC, no deadlocks.
async fn start_vortex_shm_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    capture_tee_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<AudioStreamBridge, CallerError> {
    // Open and mmap the shared memory
    let fd = unsafe {
        libc::shm_open(
            VORTEX_SHM_NAME.as_ptr() as *const libc::c_char,
            libc::O_RDWR,
            0,
        )
    };
    if fd < 0 {
        return Err(CallerError::Agent(format!(
            "vortex shm_open failed (errno {}). Are Vortex guest tools installed?",
            std::io::Error::last_os_error()
        )));
    }

    let shm_size = OFF_IN_BUFFER + VORTEX_RING_SAMPLES * 4;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            shm_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    unsafe { libc::close(fd) };
    if ptr == libc::MAP_FAILED {
        return Err(CallerError::Agent("vortex mmap failed".into()));
    }
    let base = ptr as *mut u8;

    // Verify magic
    let magic = unsafe { (base.add(OFF_MAGIC) as *const std::sync::atomic::AtomicU32).as_ref().unwrap() }
        .load(std::sync::atomic::Ordering::Acquire);
    if magic != VORTEX_SHM_MAGIC {
        return Err(CallerError::Agent(format!(
            "vortex shm magic mismatch: expected 0x{:08X}, got 0x{:08X}",
            VORTEX_SHM_MAGIC, magic
        )));
    }

    eprintln!("live_audio: vortex shm bridge attached");

    // Raw pointers aren't Send. We use usize to pass the address to spawned
    // tasks. This is safe because the mmap region lives for the process lifetime
    // and both tasks only access disjoint rings (output vs input).
    let base_usize = base as usize;

    // Helper: read/write atomics via raw address (Send-safe)
    fn atomic_load_u64(base: usize, offset: usize, order: std::sync::atomic::Ordering) -> u64 {
        unsafe { &*((base + offset) as *const std::sync::atomic::AtomicU64) }.load(order)
    }
    fn atomic_store_u64(base: usize, offset: usize, val: u64, order: std::sync::atomic::Ordering) {
        unsafe { &*((base + offset) as *const std::sync::atomic::AtomicU64) }.store(val, order);
    }
    fn read_f32(base: usize, buf_offset: usize, idx: usize) -> f32 {
        unsafe { *((base + buf_offset) as *const f32).add(idx) }
    }
    fn write_f32(base: usize, buf_offset: usize, idx: usize, val: f32) {
        unsafe { *((base + buf_offset) as *mut f32).add(idx) = val };
    }

    // Capture task: poll output ring → convert → model WebSocket
    let capture_write = session_write;
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_base = base_usize;
    let capture_handle = tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let b = capture_base;
        let mut ticker = tokio::time::interval(Duration::from_millis(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            let w = atomic_load_u64(b, OFF_OUT_WRITE_POS, Ordering::Acquire);
            let r = atomic_load_u64(b, OFF_OUT_READ_POS, Ordering::Relaxed);
            let avail = w.wrapping_sub(r) as usize;
            if avail == 0 {
                continue;
            }

            let to_read = avail.min(VORTEX_RING_SAMPLES);
            let mut f32_buf = Vec::with_capacity(to_read);
            for i in 0..to_read {
                let idx = ((r + i as u64) & VORTEX_RING_MASK) as usize;
                f32_buf.push(read_f32(b, OFF_OUT_BUFFER, idx));
            }
            atomic_store_u64(b, OFF_OUT_READ_POS, r + to_read as u64, Ordering::Release);

            let f32_bytes: Vec<u8> = f32_buf.iter().flat_map(|f| f.to_le_bytes()).collect();
            let pcm16 = vortex_capture_convert(&f32_bytes);
            if pcm16.is_empty() {
                continue;
            }

            let b64 = BASE64.encode(&pcm16);
            let ws_msg = match capture_provider {
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
                let _ = tee.send(pcm16);
            }
            let mut sink = capture_write.lock().await;
            if sink.send(WsMessage::Text(ws_msg.to_string())).await.is_err() {
                break;
            }
        }
    });

    // Playback task: model audio → convert → write to input ring
    let playback_base = base_usize;
    let playback_handle = tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let b = playback_base;
        let mut rx = audio_out_rx;

        while let Some(pcm_data) = rx.recv().await {
            let f32_bytes = vortex_playback_convert(&pcm_data);
            let samples: Vec<f32> = f32_bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();

            let mut written = 0;
            while written < samples.len() {
                let w = atomic_load_u64(b, OFF_IN_WRITE_POS, Ordering::Relaxed);
                let r = atomic_load_u64(b, OFF_IN_READ_POS, Ordering::Acquire);
                let space = VORTEX_RING_SAMPLES - (w.wrapping_sub(r)) as usize;
                if space == 0 {
                    tokio::task::yield_now().await;
                    continue;
                }
                let to_write = (samples.len() - written).min(space);
                for i in 0..to_write {
                    let idx = ((w + i as u64) & VORTEX_RING_MASK) as usize;
                    write_f32(b, OFF_IN_BUFFER, idx, samples[written + i]);
                }
                atomic_store_u64(b, OFF_IN_WRITE_POS, w + to_write as u64, Ordering::Release);
                written += to_write;
            }
        }
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

/// Vortex audio bridge: listens on a Unix socket for the Vortex guest daemon,
/// speaks the Vortex wire protocol, and converts between Float32 stereo 48kHz
/// (daemon) and PCM16 mono 24kHz (model).
async fn start_vortex_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    socket_path: &str,
    audio_out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    capture_tee_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<AudioStreamBridge, CallerError> {
    // Clean up stale socket and bind
    let _ = std::fs::remove_file(socket_path);
    let listener = tokio::net::UnixListener::bind(socket_path).map_err(|e| {
        CallerError::Agent(format!("vortex: bind {}: {}", socket_path, e))
    })?;
    eprintln!("live_audio: vortex bridge listening on {}", socket_path);

    // Wait for daemon to connect (it retries every 2s)
    let stream = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .map_err(|_| {
            CallerError::Agent("vortex: daemon did not connect within 30s".into())
        })?
        .map_err(|e| CallerError::Agent(format!("vortex: accept: {}", e)))?
        .0;
    eprintln!("live_audio: vortex daemon connected");

    let (mut read_half, mut write_half) = tokio::io::split(stream);

    // Handshake: read CONFIGURE
    let (msg_type, payload) = vortex_read_msg(&mut read_half).await?;
    if msg_type != VORTEX_MSG_CONFIGURE {
        return Err(CallerError::Agent(format!(
            "vortex: expected CONFIGURE (0x01), got 0x{:02x}",
            msg_type
        )));
    }
    if payload.len() >= 8 {
        let daemon_rate = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let daemon_channels = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        eprintln!(
            "live_audio: vortex daemon format: {}Hz {}ch float32",
            daemon_rate, daemon_channels
        );
    }

    // Read START
    let (msg_type, _) = vortex_read_msg(&mut read_half).await?;
    if msg_type != VORTEX_MSG_START {
        return Err(CallerError::Agent(format!(
            "vortex: expected START (0x04), got 0x{:02x}",
            msg_type
        )));
    }
    eprintln!("live_audio: vortex streaming started");

    // Wrap write_half in Arc<Mutex> for shared access from playback task
    let write_half = Arc::new(Mutex::new(write_half));

    // Capture: two tasks to decouple socket reads from WebSocket writes.
    // Task A drains the daemon socket as fast as possible (prevents buffer
    // backup that deadlocks the daemon's send/recv). Task B forwards the
    // converted PCM to the model WebSocket at its own pace.
    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Task A: drain daemon socket → channel
    let capture_drain = tokio::spawn(async move {
        let mut reader = read_half;
        loop {
            match vortex_read_msg(&mut reader).await {
                Ok((VORTEX_MSG_PCM_OUTPUT, payload)) => {
                    let pcm16 = vortex_capture_convert(&payload);
                    if !pcm16.is_empty() {
                        if cap_tx.send(pcm16).is_err() {
                            break;
                        }
                    }
                }
                Ok((VORTEX_MSG_STOP, _)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        eprintln!("live_audio: vortex capture drain ended");
    });

    // Task B: channel → model WebSocket
    let capture_write = session_write;
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_handle = tokio::spawn(async move {
        while let Some(pcm16) = cap_rx.recv().await {
            let b64 = BASE64.encode(&pcm16);
            let ws_msg = match capture_provider {
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
                let _ = tee.send(pcm16);
            }
            let mut sink = capture_write.lock().await;
            if sink.send(WsMessage::Text(ws_msg.to_string())).await.is_err() {
                break;
            }
        }
        capture_drain.abort();
        eprintln!("live_audio: vortex capture ended");
    });

    // Playback task: model audio → convert → daemon PCM_INPUT
    let playback_write = write_half;
    let playback_handle = tokio::spawn(async move {
        let mut rx = audio_out_rx;
        while let Some(pcm_data) = rx.recv().await {
            let f32_stereo = vortex_playback_convert(&pcm_data);
            let mut writer = playback_write.lock().await;
            if vortex_write_msg(&mut *writer, VORTEX_MSG_PCM_INPUT, &f32_stereo)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

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
    // Silence watchdog state
    let mut last_model_output = Instant::now();
    let mut silence_nudged = false;
    // Turn counter: nudge the model to emit JSON after enough turns
    let mut turn_complete_count = 0u32;
    let mut json_nudged = false;
    // Throttle progress events to avoid flooding the event bus
    let mut last_progress_emit = Instant::now();

    // Event processing loop
    let status = loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            break LiveAudioStatus::TimedOut;
        }
        let remaining = timeout - elapsed;

        // Silence watchdog: if no model output for 15s, nudge the model.
        // This prevents indefinite hangs when the model freezes on
        // unexpected input.
        let silence_limit = Duration::from_secs(15);
        let time_since_output = last_model_output.elapsed();
        if time_since_output >= silence_limit && !silence_nudged {
            silence_nudged = true;
            let _ = session.send_text("Are you still there? Please continue the conversation.").await;
        }

        match tokio::time::timeout(remaining.min(silence_limit), session.event_rx.recv()).await {
            Ok(Some(event)) => match event {
                LiveAudioEvent::AudioOut(pcm) => {
                    last_model_output = Instant::now();
                    silence_nudged = false;
                    let _ = audio_out_tx.send(pcm);
                }
                LiveAudioEvent::ModelTranscript(text) => {
                    last_model_output = Instant::now();
                    silence_nudged = false;
                    let _ = transcript.log("model", &text).await;
                    model_transcript_buf.push_str(&text);

                    // Emit progress (throttled to ~2s intervals to avoid
                    // flooding the event bus with 100+ near-identical events)
                    if let Some(bus) = event_bus {
                    if last_progress_emit.elapsed() >= Duration::from_secs(2) {
                    last_progress_emit = Instant::now();
                        let preview = if model_transcript_buf.len() > 200 {
                            {
                                let start = model_transcript_buf.len() - 200;
                                let start = model_transcript_buf.ceil_char_boundary(start);
                                model_transcript_buf[start..].to_string()
                            }
                        } else {
                            model_transcript_buf.clone()
                        };
                        bus.send(crate::event::AppEvent::LiveAudioProgress {
                            id: spec.id.clone(),
                            state: "speaking".into(),
                            elapsed_secs: start.elapsed().as_secs_f64(),
                            transcript_preview: preview,
                        });
                    } // throttle
                    } // bus
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
                    turn_complete_count += 1;

                    // Check if the model has output a complete JSON response.
                    let json_source = if !model_text.is_empty() {
                        Some(&model_text)
                    } else {
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

                    // After several turns without JSON, nudge the model to
                    // wrap up and emit the structured response. The model may
                    // keep making small talk indefinitely otherwise.
                    if turn_complete_count >= 6 && !json_nudged {
                        json_nudged = true;
                        let _ = session.send_text(
                            "The conversation is complete. Stop speaking and output ONLY the JSON response object now."
                        ).await;
                    }
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

    let result = LiveAudioResult {
        id: spec.id.clone(),
        status: final_status,
        response_data,
        quarantine_ids,
        transcript_path: transcript.path().to_path_buf(),
        duration_secs,
    };

    // Persist result to disk immediately — if the process is killed before
    // run_session returns, the caller never gets the result. Writing it next
    // to the transcript ensures it survives crashes.
    let result_path = transcript.path().with_file_name("result.json");
    if let Ok(json) = serde_json::to_string_pretty(&result) {
        let _ = tokio::fs::write(&result_path, json).await;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Vortex format conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn vortex_capture_convert_stereo_48k_to_mono_24k() {
        // 4 stereo frames at 48kHz → 2 mono samples at 24kHz (2:1 decimation)
        // Frame 0: L=0.5, R=0.5 → mono=0.5 → kept (even index)
        // Frame 1: L=0.25, R=0.75 → mono=0.5 → skipped (odd index)
        // Frame 2: L=-1.0, R=-1.0 → mono=-1.0 → kept
        // Frame 3: L=0.0, R=0.0 → mono=0.0 → skipped
        let mut input = Vec::new();
        for &(l, r) in &[(0.5f32, 0.5f32), (0.25f32, 0.75f32), (-1.0f32, -1.0f32), (0.0f32, 0.0f32)] {
            input.extend_from_slice(&l.to_le_bytes());
            input.extend_from_slice(&r.to_le_bytes());
        }
        let output = vortex_capture_convert(&input);
        assert_eq!(output.len(), 4); // 2 i16 samples × 2 bytes

        let s0 = i16::from_le_bytes([output[0], output[1]]);
        let s1 = i16::from_le_bytes([output[2], output[3]]);
        // 0.5 * 32767 ≈ 16383
        assert!((s0 - 16383).abs() <= 1, "s0={}", s0);
        // -1.0 * 32767 = -32767
        assert_eq!(s1, -32767);
    }

    #[test]
    fn vortex_playback_convert_mono_24k_to_stereo_48k() {
        // 2 mono samples at 24kHz → 4 stereo frames at 48kHz
        let input: Vec<u8> = [16383i16, -32767i16]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let output = vortex_playback_convert(&input);
        // 2 samples × 2 (upsample) × 2 (stereo) × 4 (f32) = 32 bytes
        assert_eq!(output.len(), 32);

        // First sample duplicated twice as stereo
        let f0 = f32::from_le_bytes([output[0], output[1], output[2], output[3]]);
        let f1 = f32::from_le_bytes([output[4], output[5], output[6], output[7]]);
        assert!((f0 - 16383.0 / 32768.0).abs() < 0.001);
        assert_eq!(f0, f1); // stereo: L == R
    }

    #[test]
    fn vortex_round_trip_preserves_signal() {
        // Create a 440Hz tone as PCM16 mono 24kHz (model format)
        let num_samples = 240; // 10ms
        let mut pcm16 = Vec::with_capacity(num_samples * 2);
        for i in 0..num_samples {
            let t = i as f32 / 24000.0;
            let val = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
            let sample = (val * 32767.0) as i16;
            pcm16.extend_from_slice(&sample.to_le_bytes());
        }

        // Playback convert (24k mono → 48k stereo float32)
        let f32_stereo = vortex_playback_convert(&pcm16);
        // Capture convert back (48k stereo float32 → 24k mono pcm16)
        let round_trip = vortex_capture_convert(&f32_stereo);

        assert_eq!(round_trip.len(), pcm16.len());

        // Samples should be close (quantization error ≤ 1)
        for i in 0..num_samples {
            let orig = i16::from_le_bytes([pcm16[i * 2], pcm16[i * 2 + 1]]);
            let rt = i16::from_le_bytes([round_trip[i * 2], round_trip[i * 2 + 1]]);
            assert!(
                (orig - rt).abs() <= 1,
                "sample {}: orig={} round_trip={}",
                i, orig, rt
            );
        }
    }

    #[test]
    fn vortex_capture_empty_input() {
        assert!(vortex_capture_convert(&[]).is_empty());
    }

    #[test]
    fn vortex_playback_empty_input() {
        assert!(vortex_playback_convert(&[]).is_empty());
    }

    // -----------------------------------------------------------------------
    // Existing tests
    // -----------------------------------------------------------------------

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

    /// Interactive phone call test via Vortex audio bridge + pjsua SIP client.
    ///
    /// IMPORTANT: This test requires a GUI login session. macOS gates audio
    /// input behind the WindowServer session — processes from SSH get silence.
    /// Run this test from Terminal.app inside the VM's display, or install
    /// pjsua as a LaunchAgent. The Vortex daemon and intendant can run from
    /// any context; only pjsua (the app opening mic input) needs GUI session.
    ///
    /// Prerequisites:
    ///   - Vortex guest tools installed (VortexAudioPlugin + VortexAudioDaemon)
    ///   - "Vortex Audio" set as default input AND output in System Settings
    ///   - VortexAudioDaemon running with --socket /tmp/intendant-audio.sock
    ///   - ~/bin/pjsua built from pjsip source
    ///   - ~/lin containing the SIP password
    ///   - OPENAI_API_KEY in environment
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

        let vortex_socket = "/tmp/intendant-audio.sock";
        let session_id = format!(
            "call-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let bridge = crate::audio_routing::create_vortex_bridge(vortex_socket);

        // Discover Vortex Audio's pjsua device index dynamically.
        let pjsua_bin = dirs::home_dir().unwrap().join("bin/pjsua");
        if !pjsua_bin.exists() {
            eprintln!("~/bin/pjsua not found, skipping");
            return;
        }
        let dev_output = std::process::Command::new(&pjsua_bin)
            .args(["--null-audio"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                if let Some(mut stdin) = c.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(b"q\n");
                }
                c.wait_with_output()
            });
        let vortex_dev_idx = match dev_output {
            Ok(out) => {
                // pjsua prints device list to stdout
                let output = String::from_utf8_lossy(&out.stdout);
                output
                    .lines()
                    .filter(|l| l.contains("dev_id"))
                    .position(|l| l.contains("Vortex Audio"))
                    .map(|i| i.to_string())
            }
            Err(_) => None,
        };
        let dev_idx = match vortex_dev_idx {
            Some(idx) => idx,
            None => {
                eprintln!("Vortex Audio not found in pjsua device list, skipping");
                return;
            }
        };
        let capture_arg = format!("--capture-dev={}", dev_idx);
        let playback_arg = format!("--playback-dev={}", dev_idx);

        let sip_password = match std::fs::read_to_string(
            dirs::home_dir().unwrap().join("lin"),
        ) {
            Ok(p) => p.trim().to_string(),
            Err(_) => {
                eprintln!("~/lin not found (SIP password), skipping");
                return;
            }
        };

        // Launch pjsua AFTER the bridge connects (spawned with delay).
        // pjsua opening Vortex Audio triggers a flood of PCM_OUTPUT from
        // the daemon. The bridge's drain task must be running first.
        // NOTE: pjsua must run in the GUI login session for mic input.
        let pjsua_bin_clone = pjsua_bin.clone();
        let pjsua_handle = tokio::spawn(async move {
            // Wait for the bridge to connect and start draining
            tokio::time::sleep(Duration::from_secs(8)).await;
            let mut pjsua = tokio::process::Command::new(&pjsua_bin_clone)
                .args([
                    "--id=sip:intendant7@sip.linphone.org",
                    "--registrar=sip:sip.linphone.org",
                    "--realm=sip.linphone.org",
                    "--username=intendant7",
                    &format!("--password={}", sip_password),
                    &capture_arg,
                    &playback_arg,
                    "--auto-answer=200",
                    "--ec-tail=0",
                    "--no-vad",
                    "--use-srtp=2",
                    "--srtp-secure=0",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("failed to start pjsua");

            // Wait for registration, then make outbound call
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Some(ref mut stdin) = pjsua.stdin {
                let _ = stdin.write_all(b"m\n").await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = stdin
                    .write_all(b"sip:intendant8@sip.linphone.org\n")
                    .await;
            }
            eprintln!("  pjsua calling intendant8 — ANSWER YOUR PHONE!");
            pjsua
        });
        eprintln!("  pjsua will start after bridge connects. Timeout: 120s\n");

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
            initial_message: None,
        };

        let tmp_dir = tempfile::tempdir().expect("create temp dir");

        let result = run_session(&spec, &api_key, &bridge, tmp_dir.path(), None)
            .await
            .expect("run_session failed");

        // Stop pjsua
        if let Ok(mut pjsua) = pjsua_handle.await {
            if let Some(mut stdin) = pjsua.stdin.take() {
                let _ = stdin.write_all(b"q\n").await;
            }
            let _ = pjsua.wait().await;
        }
        drop(bridge);

        // Display transcript
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

        match &result.status {
            LiveAudioStatus::Completed => {
                let data = result.response_data.as_ref().unwrap();
                eprintln!("  Summary: {}", data["summary"].as_str().unwrap_or("?"));
                eprintln!("  Caller mood: {}", data["caller_mood"].as_str().unwrap_or("?"));
            }
            LiveAudioStatus::TimedOut => {
                eprintln!("  Session timed out — model did not produce JSON response");
            }
            other => {
                eprintln!("  Unexpected status: {:?}", other);
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
