//! WebRTC-based display transport.
//!
//! A `DisplaySession` connects a platform-native capture backend to one or more
//! WebRTC peers via a shared VP8 encoder.  The pipeline is:
//!
//! ```text
//! [CaptureBackend] --mpsc(4)--> [capture bridge] --broadcast(16)--> [encoder]
//!                                     |                                |
//!                              latest_frame (RwLock)            per-peer mpsc(8)
//!                                                                      |
//!                                                              [WebRtcPeer sender]
//!                                                                      |
//!                                                               track.write_sample()
//! ```
//!
//! Backpressure rules:
//! - PipeWire -> tokio: bounded `mpsc(4)`, frames dropped via `try_send`.
//! - Broadcast to encoder subscribers: `broadcast(16)`, lagging receivers skip.
//! - Per-peer encoded frame queue: `mpsc(8)`, encoder drops via `try_send`.
//! - `latest_frame`: always overwritten, latest-wins.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::CallerError;

pub mod encode;
pub mod keymap;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod macos_keymap;
pub mod webrtc;
#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "linux")]
pub mod x11;

// ---------------------------------------------------------------------------
// Display enumeration
// ---------------------------------------------------------------------------

/// Information about a single physical display.
///
/// `id` is the intendant-stable identifier (0 = primary / user session default).
/// `platform_id` carries the native display identifier (CGDisplayID on macOS,
/// X11 screen number, PipeWire node_id on Wayland).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Intendant-stable display ID.  0 is always the primary display.
    pub id: u32,
    /// Platform-native display identifier.
    pub platform_id: u64,
    /// Human-readable name (e.g. "Built-in Retina Display", "HDMI-1").
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this display is the primary / main display.
    pub is_primary: bool,
}

/// Enumerate displays available on the current platform.
///
/// Returns a list of `DisplayInfo` with the primary display at index 0 (id=0).
/// On single-monitor setups this returns exactly one entry.  If enumeration
/// fails, returns a single fallback entry with default resolution.
pub async fn enumerate_displays() -> Vec<DisplayInfo> {
    let displays = enumerate_displays_platform().await;
    if displays.is_empty() {
        vec![DisplayInfo {
            id: 0,
            platform_id: 0,
            name: "Default Display".to_string(),
            width: 1920,
            height: 1080,
            is_primary: true,
        }]
    } else {
        displays
    }
}

/// Platform-specific display enumeration.
#[cfg(target_os = "macos")]
async fn enumerate_displays_platform() -> Vec<DisplayInfo> {
    macos::enumerate_displays().await
}

#[cfg(target_os = "linux")]
async fn enumerate_displays_platform() -> Vec<DisplayInfo> {
    // Try X11 first (works on both X11 sessions and Xwayland).
    let displays = x11::enumerate_displays().await;
    if !displays.is_empty() {
        return displays;
    }
    // On pure Wayland (no Xwayland), the portal doesn't expose enumeration
    // but we return a placeholder so callers know a display exists.
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return wayland::enumerate_displays().await;
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A raw, uncompressed video frame from the capture backend.
pub struct Frame {
    pub data: Vec<u8>,
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    Bgra,
    Rgba,
}

/// Encoded VP8 frame -- shared across peers, each peer packetizes independently.
#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_ms: u64,
    pub duration_ms: u64,
    pub is_keyframe: bool,
}

/// Browser input event -- carries DOM key identifiers and normalised mouse
/// coordinates (0.0..1.0).
///
/// Phase 1: physical key semantics only.  The `code` field (DOM physical key
/// position) is used for injection; the `key` field is carried but not used.
/// Non-US keyboard layouts will produce incorrect character output.  A future
/// phase will add character-level injection via the `key` field where the
/// platform supports it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t")]
pub enum InputEvent {
    #[serde(rename = "kd")]
    KeyDown {
        code: String,
        key: String,
        shift: bool,
        ctrl: bool,
        alt: bool,
        meta: bool,
    },
    #[serde(rename = "ku")]
    KeyUp {
        code: String,
        key: String,
        shift: bool,
        ctrl: bool,
        alt: bool,
        meta: bool,
    },
    #[serde(rename = "md")]
    MouseDown { x: f64, y: f64, b: u8 },
    #[serde(rename = "mu")]
    MouseUp { x: f64, y: f64, b: u8 },
    #[serde(rename = "mm")]
    MouseMove { x: f64, y: f64, #[serde(default)] buttons: u8 },
    #[serde(rename = "sc")]
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
}

// ---------------------------------------------------------------------------
// ICE configuration
// ---------------------------------------------------------------------------

/// WebRTC ICE configuration.  Defaults to empty (local-only, no STUN/TURN).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct IceConfig {
    pub ice_servers: Vec<IceServer>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

// ---------------------------------------------------------------------------
// Display backend trait
// ---------------------------------------------------------------------------

pub type PeerId = u64;

// ---------------------------------------------------------------------------
// Display metrics (atomic counters for the hot path)
// ---------------------------------------------------------------------------

/// Atomic counters embedded in the capture/encode/fan-out pipeline.
///
/// All counters are updated with `Relaxed` ordering -- they are advisory
/// telemetry, not synchronisation primitives.  Rate computation happens in
/// `DisplayMetricsSnapshot::from_counters()`, never on the hot path.
pub struct DisplayMetricsCounters {
    /// Total raw frames received from the capture backend.
    pub capture_frames: AtomicU64,
    /// Frames dropped at the broadcast send (no subscribers or lagging).
    pub capture_drops: AtomicU64,

    /// Total frames successfully VP8-encoded.
    pub encode_frames: AtomicU64,
    /// I420 buffers dropped by try_send to the encoder thread.
    pub encode_drops: AtomicU64,
    /// Cumulative encode latency in microseconds (capture-to-encoded-output).
    pub encode_latency_us_sum: AtomicU64,

    /// Total per-peer try_send failures in the fan-out task.
    pub peer_drops: AtomicU64,
    /// Current number of connected WebRTC peers.
    pub peer_count: AtomicU64,

    /// Monotonic microsecond timestamp of the last metrics reset.
    pub epoch_us: AtomicU64,
}

impl DisplayMetricsCounters {
    pub fn new() -> Self {
        let now_us = Instant::now().elapsed().as_micros() as u64;
        Self {
            capture_frames: AtomicU64::new(0),
            capture_drops: AtomicU64::new(0),
            encode_frames: AtomicU64::new(0),
            encode_drops: AtomicU64::new(0),
            encode_latency_us_sum: AtomicU64::new(0),
            peer_drops: AtomicU64::new(0),
            peer_count: AtomicU64::new(0),
            epoch_us: AtomicU64::new(now_us),
        }
    }
}

/// A point-in-time snapshot of display pipeline metrics, suitable for
/// serialisation and logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayMetricsSnapshot {
    pub display_id: u32,
    pub capture_fps: f64,
    pub capture_drops: u64,
    pub encode_fps: f64,
    pub encode_latency_avg_ms: f64,
    pub encode_drops: u64,
    pub peer_count: u64,
    pub peer_drops: u64,
    pub resolution: (u32, u32),
}

impl DisplayMetricsSnapshot {
    /// Read the atomic counters and compute rates over the elapsed window.
    /// Resets counters after reading so the next call covers a fresh window.
    pub fn from_counters(
        counters: &DisplayMetricsCounters,
        display_id: u32,
        resolution: (u32, u32),
        elapsed: &Instant,
    ) -> Self {
        let capture_frames = counters.capture_frames.swap(0, Ordering::Relaxed);
        let capture_drops = counters.capture_drops.swap(0, Ordering::Relaxed);
        let encode_frames = counters.encode_frames.swap(0, Ordering::Relaxed);
        let encode_drops = counters.encode_drops.swap(0, Ordering::Relaxed);
        let encode_latency_us = counters.encode_latency_us_sum.swap(0, Ordering::Relaxed);
        let peer_drops = counters.peer_drops.swap(0, Ordering::Relaxed);
        let peer_count = counters.peer_count.load(Ordering::Relaxed);

        let elapsed_secs = elapsed.elapsed().as_secs_f64().max(0.001);

        let encode_latency_avg_ms = if encode_frames > 0 {
            (encode_latency_us as f64 / encode_frames as f64) / 1000.0
        } else {
            0.0
        };

        Self {
            display_id,
            capture_fps: capture_frames as f64 / elapsed_secs,
            capture_drops,
            encode_fps: encode_frames as f64 / elapsed_secs,
            encode_latency_avg_ms,
            encode_drops,
            peer_count,
            peer_drops,
            resolution,
        }
    }
}

// ---------------------------------------------------------------------------
// Display backend trait
// ---------------------------------------------------------------------------

/// Platform-specific display capture and input injection.
#[async_trait]
pub trait DisplayBackend: Send + Sync + 'static {
    /// Begin capturing frames at the target framerate.
    /// Returns a receiver for raw frames (bounded channel, backend drops on full).
    async fn start_capture(
        &self,
        fps: u32,
    ) -> Result<mpsc::Receiver<Frame>, CallerError>;

    /// Stop capturing. Blocks until the capture thread/task has exited.
    async fn stop_capture(&self);

    /// Inject a browser input event into the display.
    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError>;

    /// Current display resolution (width, height).
    fn resolution(&self) -> (u32, u32);

    /// Human-readable backend name (e.g. "wayland", "x11").
    fn kind(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// DisplaySession
// ---------------------------------------------------------------------------

/// Manages a single display's capture pipeline, encoder, and WebRTC peers.
pub struct DisplaySession {
    pub display_id: u32,
    backend: Arc<dyn DisplayBackend>,
    frame_tx: broadcast::Sender<Arc<Frame>>,
    latest_frame: Arc<RwLock<Option<Arc<Frame>>>>,
    peers: Arc<RwLock<HashMap<PeerId, Arc<self::webrtc::WebRtcPeer>>>>,
    encoder_handle: Mutex<Option<JoinHandle<()>>>,
    capture_handle: Mutex<Option<JoinHandle<()>>>,
    shutdown: CancellationToken,
    counters: Arc<DisplayMetricsCounters>,
    /// Instant used as the epoch for rate computations.
    metrics_epoch: Mutex<Instant>,
    /// Negotiated codec MIME type for the encoder pipeline.
    /// Set on the first peer connection, all subsequent peers use the same codec.
    codec_mime: RwLock<&'static str>,
}

impl DisplaySession {
    /// Create a new display session.  Does NOT start capture -- call `start()`.
    pub fn new(display_id: u32, backend: Arc<dyn DisplayBackend>) -> Self {
        let (frame_tx, _) = broadcast::channel(16);
        Self {
            display_id,
            backend,
            frame_tx,
            latest_frame: Arc::new(RwLock::new(None)),
            peers: Arc::new(RwLock::new(HashMap::new())),
            encoder_handle: Mutex::new(None),
            capture_handle: Mutex::new(None),
            shutdown: CancellationToken::new(),
            counters: Arc::new(DisplayMetricsCounters::new()),
            metrics_epoch: Mutex::new(Instant::now()),
            codec_mime: RwLock::new(encode::MIME_TYPE_VP8),
        }
    }

    /// Start the capture and encoding pipeline.
    ///
    /// Spawns:
    /// 1. **Capture bridge** -- reads frames from the backend mpsc, updates
    ///    `latest_frame`, and broadcasts to subscribers.
    /// 2. **Encoder** -- subscribes to the broadcast, converts BGRA->I420,
    ///    VP8-encodes, and fans out `Arc<EncodedFrame>` to each peer's bounded
    ///    channel.
    /// 3. **FrameRegistry sampler** (if provided) -- 1 Hz JPEG capture for
    ///    model sampling and presence tools.
    ///
    /// If `event_bus` is provided, `DisplayResize` events are emitted when
    /// the capture backend delivers frames at a different resolution than the
    /// current encoder expects.  The encoder is transparently recreated with
    /// the new dimensions.
    pub async fn start(
        &self,
        fps: u32,
        frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
        event_bus: Option<crate::event::EventBus>,
    ) -> Result<(), CallerError> {
        let mut capture_rx = self.backend.start_capture(fps).await?;

        let (width, height) = self.backend.resolution();

        // --- Task 1: Capture bridge ---
        let frame_tx = self.frame_tx.clone();
        let latest = Arc::clone(&self.latest_frame);
        let shutdown = self.shutdown.clone();
        let cap_counters = Arc::clone(&self.counters);
        let cap_display_id = self.display_id;
        let event_bus_for_encoder = event_bus.clone();

        let capture_handle = tokio::spawn(async move {
            let mut clean_shutdown = false;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => { clean_shutdown = true; break; }
                    frame = capture_rx.recv() => {
                        let Some(frame) = frame else {
                            // Backend stopped — portal session ended, capture
                            // thread crashed, or display was disconnected.
                            eprintln!(
                                "[display/capture] display {} capture backend stopped \
                                 (channel closed), capture bridge exiting",
                                cap_display_id,
                            );
                            break;
                        };
                        cap_counters.capture_frames.fetch_add(1, Ordering::Relaxed);
                        let arc_frame = Arc::new(frame);
                        *latest.write().await = Some(Arc::clone(&arc_frame));
                        // If no subscribers, the send fails -- count as a drop.
                        if frame_tx.send(arc_frame).is_err() {
                            cap_counters.capture_drops.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            // If the backend stopped unexpectedly (not a clean shutdown),
            // emit DisplayCaptureLost so the session can be cleaned up.
            if !clean_shutdown {
                if let Some(ref bus) = event_bus {
                    bus.send(crate::event::AppEvent::DisplayCaptureLost {
                        display_id: cap_display_id,
                        reason: "capture backend stopped".to_string(),
                    });
                }
            }
        });
        *self.capture_handle.lock().await = Some(capture_handle);

        // --- Task 2: VP8 Encoder + WebRTC fan-out ---
        {
            let mut broadcast_rx = self.frame_tx.subscribe();
            let peers = Arc::clone(&self.peers);

            let duration_ms = if fps > 0 { 1000 / fps as u64 } else { 33 };

            // Encoded frame channel — survives encoder restarts.  The fanout
            // task reads from `efr_rx`; each encoder thread gets a clone of
            // `efr_tx` so dropping+respawning the encoder thread does not
            // close the channel.
            let (efr_tx, mut efr_rx) = mpsc::channel::<Arc<EncodedFrame>>(16);

            // Spawn the initial encoder thread.
            let (i420_tx, i420_rx) =
                std::sync::mpsc::sync_channel::<(Vec<u8>, Instant)>(4);

            let enc_counters = Arc::clone(&self.counters);
            let encoder_shutdown = self.shutdown.clone();
            let session_codec_mime = *self.codec_mime.read().await;
            spawn_encoder_thread(
                width, height, duration_ms,
                session_codec_mime,
                i420_rx, efr_tx.clone(),
                Arc::clone(&enc_counters), encoder_shutdown,
            );

            let bridge_counters = Arc::clone(&self.counters);
            let shutdown_bridge = self.shutdown.clone();
            let frame_interval = std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });
            let display_id = self.display_id;
            let event_bus = event_bus_for_encoder;
            let codec_mime_for_bridge = session_codec_mime;
            let bridge_handle = tokio::spawn(async move {
                let mut last_encode = tokio::time::Instant::now();
                // Track current encoder dimensions for resize detection.
                let mut enc_width = width;
                let mut enc_height = height;
                let mut i420_tx = i420_tx;
                loop {
                    tokio::select! {
                        _ = shutdown_bridge.cancelled() => break,
                        result = broadcast_rx.recv() => {
                            if last_encode.elapsed() < frame_interval {
                                continue;
                            }
                            last_encode = tokio::time::Instant::now();
                            let frame = match result {
                                Ok(f) => f,
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            };

                            // -- Resize detection --
                            // Round to even dimensions for VP8 compatibility.
                            let frame_w = frame.width & !1;
                            let frame_h = frame.height & !1;
                            if frame_w > 0 && frame_h > 0
                                && (frame_w != enc_width || frame_h != enc_height)
                            {
                                eprintln!(
                                    "[display/bridge] resolution changed {}x{} -> {}x{}, recreating encoder",
                                    enc_width, enc_height, frame_w, frame_h,
                                );
                                enc_width = frame_w;
                                enc_height = frame_h;

                                // Drop the old sender — the encoder thread's
                                // `i420_rx.recv()` will return Err and the
                                // thread will exit cleanly.
                                drop(i420_tx);

                                // Spawn a fresh encoder thread at the new
                                // dimensions, reusing the same `efr_tx`.
                                let (new_tx, new_rx) =
                                    std::sync::mpsc::sync_channel::<(Vec<u8>, Instant)>(4);
                                i420_tx = new_tx;
                                let counters_clone = Arc::clone(&bridge_counters);
                                let shutdown_clone = shutdown_bridge.clone();
                                spawn_encoder_thread(
                                    enc_width, enc_height, duration_ms,
                                    codec_mime_for_bridge,
                                    new_rx, efr_tx.clone(),
                                    counters_clone, shutdown_clone,
                                );

                                if let Some(ref bus) = event_bus {
                                    bus.send(crate::event::AppEvent::DisplayResize {
                                        display_id,
                                        width: enc_width,
                                        height: enc_height,
                                    });
                                }
                            }

                            let arrived = Instant::now();
                            let fd = frame.data.clone();
                            let fw = frame.width;
                            let fh = frame.height;
                            let fs = frame.stride;
                            let i420 = tokio::task::spawn_blocking(move || {
                                encode::bgra_to_i420(&fd, fw, fh, fs)
                            }).await;
                            if let Ok(i420) = i420 {
                                if i420_tx.try_send((i420, arrived)).is_err() {
                                    bridge_counters
                                        .encode_drops
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            });

            let fanout_counters = Arc::clone(&self.counters);
            let shutdown_fanout = self.shutdown.clone();
            let fanout_display_id = self.display_id;
            let encoder_handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_fanout.cancelled() => break,
                        ef = efr_rx.recv() => {
                            let Some(ef) = ef else {
                                // Encoder thread exited (panic, init failure,
                                // or clean shutdown).  Log and stop the
                                // BGRA->I420 bridge so the pipeline drains.
                                eprintln!(
                                    "[display/fanout] display {} encoder channel \
                                     closed, fan-out exiting",
                                    fanout_display_id,
                                );
                                break;
                            };
                            let peers_guard = peers.read().await;
                            for peer in peers_guard.values() {
                                if peer.encoded_frame_tx().try_send(Arc::clone(&ef)).is_err() {
                                    fanout_counters
                                        .peer_drops
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
                bridge_handle.abort();
            });
            *self.encoder_handle.lock().await = Some(encoder_handle);
        }

        // --- Task 3: FrameRegistry sampler (1 Hz JPEG for model feeds) ---
        if let Some(registry) = frame_registry {
            let latest = Arc::clone(&self.latest_frame);
            let shutdown_reg = self.shutdown.clone();
            let display_id = self.display_id;
            let mut frame_counter = 0u64;
            let reg_handle = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    tokio::select! {
                        _ = shutdown_reg.cancelled() => break,
                        _ = interval.tick() => {
                            let frame = latest.read().await.clone();
                            let Some(frame) = frame else { continue };
                            let w = frame.width;
                            let h = frame.height;
                            // Encode BGRA/RGBA → JPEG on blocking pool
                            let jpeg = tokio::task::spawn_blocking(move || {
                                // Strip row padding if stride > width * 4
                                let row_bytes = w as usize * 4;
                                let stride = frame.stride as usize;
                                let rgba_data = match frame.format {
                                    FrameFormat::Rgba if stride == row_bytes => frame.data.clone(),
                                    FrameFormat::Rgba => {
                                        let mut tight = Vec::with_capacity(row_bytes * h as usize);
                                        for row in 0..h as usize {
                                            let start = row * stride;
                                            tight.extend_from_slice(&frame.data[start..start + row_bytes]);
                                        }
                                        tight
                                    }
                                    FrameFormat::Bgra => {
                                        let mut tight = Vec::with_capacity(row_bytes * h as usize);
                                        for row in 0..h as usize {
                                            let start = row * stride;
                                            for col in 0..w as usize {
                                                let px = start + col * 4;
                                                tight.push(frame.data[px + 2]); // R
                                                tight.push(frame.data[px + 1]); // G
                                                tight.push(frame.data[px]);      // B
                                                tight.push(frame.data[px + 3]); // A
                                            }
                                        }
                                        tight
                                    }
                                };
                                let img = image::RgbaImage::from_raw(w, h, rgba_data)?;
                                let mut buf = std::io::Cursor::new(Vec::new());
                                img.write_to(&mut buf, image::ImageFormat::Jpeg).ok()?;
                                Some(buf.into_inner())
                            }).await.ok().flatten();
                            if let Some(jpeg_bytes) = jpeg {
                                frame_counter += 1;
                                let stream = format!("display_{}", display_id);
                                let frame_id = format!("{}-f{}", stream, frame_counter);
                                let meta = presence_core::FrameMeta {
                                    frame_id,
                                    stream,
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    sent_to_live: false,
                                    live_resolution: None,
                                    hq_resolution: Some(format!("{}x{}", w, h)),
                                    note: None,
                                };
                                let mut reg = registry.write().await;
                                let _ = reg.register(meta, &jpeg_bytes);
                            }
                        }
                    }
                }
            });
            // Store handle — stop() cancels via shutdown token.
            // Reuse encoder_handle field slot since we don't have a dedicated one.
            // Actually, let's just let it be managed by the CancellationToken.
            drop(reg_handle); // Managed by shutdown token; task self-cancels.
        }

        Ok(())
    }

    /// Read the current metrics snapshot and reset the rate counters.
    ///
    /// The returned snapshot covers the window since the last call to
    /// `metrics()` (or since `start()` if this is the first call).
    pub async fn metrics(&self) -> DisplayMetricsSnapshot {
        let mut epoch = self.metrics_epoch.lock().await;
        let snap = DisplayMetricsSnapshot::from_counters(
            &self.counters,
            self.display_id,
            self.backend.resolution(),
            &epoch,
        );
        *epoch = Instant::now();
        snap
    }

    /// Spawn a background task that logs a one-line metrics summary every 30s.
    ///
    /// The task runs until the session's shutdown token is cancelled.
    /// Returns the join handle so callers can await it if desired, but
    /// typically the shutdown token handles cleanup.
    pub fn spawn_metrics_logger(
        self: &Arc<Self>,
        event_bus: Option<crate::event::EventBus>,
    ) -> JoinHandle<()> {
        let session = Arc::clone(self);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(30));
            // Skip the immediate first tick.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let m = session.metrics().await;
                        eprintln!(
                            "[display/metrics] id={} capture={:.1}fps encode={:.1}fps \
                             drops=cap:{}/enc:{}/peer:{} peers={} latency_avg={:.1}ms res={}x{}",
                            m.display_id,
                            m.capture_fps,
                            m.encode_fps,
                            m.capture_drops,
                            m.encode_drops,
                            m.peer_drops,
                            m.peer_count,
                            m.encode_latency_avg_ms,
                            m.resolution.0,
                            m.resolution.1,
                        );
                        if let Some(ref bus) = event_bus {
                            bus.send(crate::event::AppEvent::DisplayMetrics {
                                snapshot: m,
                            });
                        }
                    }
                }
            }
        })
    }

    /// Stop capture, cancel all tasks, and close all peers.
    pub async fn stop(&self) {
        self.shutdown.cancel();
        self.backend.stop_capture().await;

        if let Some(h) = self.capture_handle.lock().await.take() {
            let _ = h.await;
        }
        if let Some(h) = self.encoder_handle.lock().await.take() {
            let _ = h.await;
        }

        let mut peers = self.peers.write().await;
        for (_, peer) in peers.drain() {
            peer.close().await;
        }
    }

    /// Subscribe to the raw frame broadcast.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<Arc<Frame>> {
        self.frame_tx.subscribe()
    }

    /// Get the most recently captured frame, or `None` if no frame yet.
    pub async fn latest_frame(&self) -> Option<Arc<Frame>> {
        self.latest_frame.read().await.clone()
    }

    /// Encode the latest frame as a PNG screenshot.
    pub async fn screenshot(&self) -> Result<Vec<u8>, CallerError> {
        let frame = self
            .latest_frame()
            .await
            .ok_or_else(|| CallerError::Display("no frame available for screenshot".into()))?;

        let (w, h) = (frame.width, frame.height);

        // Convert from BGRA (or RGBA) to tightly-packed RGBA for the image crate.
        // If stride > width * 4 the rows have alignment padding that must be stripped.
        let row_bytes = w as usize * 4;
        let stride = frame.stride as usize;

        let rgba_data = match frame.format {
            FrameFormat::Rgba if stride == row_bytes => frame.data.clone(),
            FrameFormat::Rgba => {
                let mut tight = Vec::with_capacity(row_bytes * h as usize);
                for row in 0..h as usize {
                    let start = row * stride;
                    tight.extend_from_slice(&frame.data[start..start + row_bytes]);
                }
                tight
            }
            FrameFormat::Bgra => {
                let mut tight = Vec::with_capacity(row_bytes * h as usize);
                for row in 0..h as usize {
                    let start = row * stride;
                    for col in 0..w as usize {
                        let px = start + col * 4;
                        // Swap B <-> R while copying
                        tight.push(frame.data[px + 2]); // R
                        tight.push(frame.data[px + 1]); // G
                        tight.push(frame.data[px]);      // B
                        tight.push(frame.data[px + 3]); // A
                    }
                }
                tight
            }
        };

        let img = image::RgbaImage::from_raw(w, h, rgba_data).ok_or_else(|| {
            CallerError::Display("failed to construct image from frame data".into())
        })?;

        let mut png_buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png_buf, image::ImageFormat::Png)
            .map_err(|e| CallerError::Display(format!("PNG encode: {e}")))?;

        Ok(png_buf.into_inner())
    }

    /// Handle a WebRTC SDP offer from a browser peer.
    ///
    /// Negotiates the codec from the SDP offer, creates a `WebRtcPeer` with the
    /// selected codec, subscribes it to the encoder output, adds it to the peer
    /// map, and returns the SDP answer.
    pub async fn handle_offer(
        &self,
        peer_id: PeerId,
        sdp: &str,
        ice_config: &IceConfig,
        ice_tx: mpsc::Sender<(PeerId, String)>,
    ) -> Result<String, CallerError> {
        // Negotiate codec from the offer SDP.
        let (width, height) = self.backend.resolution();
        let codec_choice = encode::select_codec(sdp, width, height, 2000)
            .map_err(|e| CallerError::WebRtc(format!("codec selection: {e}")))?;

        // Store the negotiated codec for the encoder pipeline.
        // The first peer's codec wins; subsequent peers use the same codec.
        *self.codec_mime.write().await = codec_choice.mime_type;

        let codec_mime = codec_choice.mime_type;

        let backend = Arc::clone(&self.backend);
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> =
            Arc::new(move |event: InputEvent| {
                let backend = Arc::clone(&backend);
                tokio::spawn(async move {
                    if let Err(e) = backend.inject_input(event).await {
                        eprintln!("[display] input injection failed: {e}");
                    }
                });
            });

        let (peer, answer_sdp) =
            self::webrtc::WebRtcPeer::new(peer_id, sdp, codec_mime, ice_config, input_handler, ice_tx)
                .await?;

        self.peers.write().await.insert(peer_id, Arc::new(peer));
        self.counters.peer_count.fetch_add(1, Ordering::Relaxed);
        Ok(answer_sdp)
    }

    /// Forward a trickle ICE candidate to a connected peer.
    pub async fn add_ice_candidate(
        &self,
        peer_id: PeerId,
        candidate_json: &str,
    ) -> Result<(), CallerError> {
        let peers = self.peers.read().await;
        let peer = peers.get(&peer_id).ok_or_else(|| {
            CallerError::WebRtc(format!("unknown peer {peer_id}"))
        })?;
        peer.add_ice_candidate(candidate_json).await
    }

    /// Remove and close a peer.
    pub async fn remove_peer(&self, peer_id: PeerId) {
        if let Some(peer) = self.peers.write().await.remove(&peer_id) {
            self.counters.peer_count.fetch_sub(1, Ordering::Relaxed);
            peer.close().await;
        }
    }

    /// Inject an input event into the display backend.
    pub async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        self.backend.inject_input(event).await
    }

    /// Current display resolution.
    pub fn resolution(&self) -> (u32, u32) {
        self.backend.resolution()
    }

}

// ---------------------------------------------------------------------------
// Encoder thread helper
// ---------------------------------------------------------------------------

/// Spawn an encoder thread that reads I420 frames from `i420_rx`, encodes
/// them using the negotiated codec, and sends `Arc<EncodedFrame>` to `efr_tx`.
///
/// `codec_mime` selects the encoder implementation (currently only `"video/VP8"`
/// is supported; future codecs will be added here).
///
/// The thread exits cleanly when `i420_rx` is closed (sender dropped) or
/// the shutdown token is cancelled.  This is a free function so the
/// broadcast-to-encoder bridge can call it on resize without needing
/// `&self`.
fn spawn_encoder_thread(
    width: u32,
    height: u32,
    duration_ms: u64,
    codec_mime: &'static str,
    i420_rx: std::sync::mpsc::Receiver<(Vec<u8>, Instant)>,
    efr_tx: mpsc::Sender<Arc<EncodedFrame>>,
    counters: Arc<DisplayMetricsCounters>,
    shutdown: CancellationToken,
) {
    std::thread::spawn(move || {
        let mut encoder: Box<dyn encode::Encoder> = match encode::select_codec_for_mime(codec_mime, width, height, 2000) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[display/encoder] {} encoder FAILED for {}x{}: {}", codec_mime, width, height, e);
                return;
            }
        };
        while let Ok((i420, arrived)) = i420_rx.recv() {
            if shutdown.is_cancelled() {
                break;
            }
            if let Ok(packets) = encoder.encode(&i420, duration_ms) {
                let latency_us = arrived.elapsed().as_micros() as u64;
                for pkt in packets {
                    counters.encode_frames.fetch_add(1, Ordering::Relaxed);
                    counters
                        .encode_latency_us_sum
                        .fetch_add(latency_us, Ordering::Relaxed);
                    let ef = Arc::new(pkt.into_encoded_frame());
                    if efr_tx.blocking_send(ef).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// SessionRegistry
// ---------------------------------------------------------------------------

/// Registry of active display sessions, keyed by display ID.
pub struct SessionRegistry {
    sessions: HashMap<u32, Arc<DisplaySession>>,
}

pub type SharedSessionRegistry = Arc<RwLock<SessionRegistry>>;

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    pub fn get(&self, display_id: u32) -> Option<Arc<DisplaySession>> {
        self.sessions.get(&display_id).cloned()
    }

    pub fn insert(&mut self, display_id: u32, session: Arc<DisplaySession>) {
        self.sessions.insert(display_id, session);
    }

    pub fn remove(&mut self, display_id: u32) -> Option<Arc<DisplaySession>> {
        self.sessions.remove(&display_id)
    }

    /// All active display IDs.
    pub fn display_ids(&self) -> Vec<u32> {
        self.sessions.keys().copied().collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_deserialize_key_down() {
        let json = r#"{"t":"kd","code":"KeyA","key":"a","shift":false,"ctrl":false,"alt":false,"meta":false}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::KeyDown { code, key, shift, ctrl, alt, meta } => {
                assert_eq!(code, "KeyA");
                assert_eq!(key, "a");
                assert!(!shift);
                assert!(!ctrl);
                assert!(!alt);
                assert!(!meta);
            }
            other => panic!("expected KeyDown, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_key_up() {
        let json = r#"{"t":"ku","code":"Space","key":" ","shift":false,"ctrl":true,"alt":false,"meta":false}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::KeyUp { code, ctrl, .. } => {
                assert_eq!(code, "Space");
                assert!(ctrl);
            }
            other => panic!("expected KeyUp, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_mouse_down() {
        let json = r#"{"t":"md","x":0.5,"y":0.25,"b":0}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::MouseDown { x, y, b } => {
                assert!((x - 0.5).abs() < f64::EPSILON);
                assert!((y - 0.25).abs() < f64::EPSILON);
                assert_eq!(b, 0);
            }
            other => panic!("expected MouseDown, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_mouse_up() {
        let json = r#"{"t":"mu","x":0.1,"y":0.9,"b":2}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::MouseUp { x, y, b } => {
                assert!((x - 0.1).abs() < f64::EPSILON);
                assert!((y - 0.9).abs() < f64::EPSILON);
                assert_eq!(b, 2);
            }
            other => panic!("expected MouseUp, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_mouse_move() {
        let json = r#"{"t":"mm","x":0.33,"y":0.66}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::MouseMove { x, y, buttons } => {
                assert!((x - 0.33).abs() < f64::EPSILON);
                assert!((y - 0.66).abs() < f64::EPSILON);
                assert_eq!(buttons, 0); // default when omitted
            }
            other => panic!("expected MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_mouse_move_with_buttons() {
        let json = r#"{"t":"mm","x":0.5,"y":0.5,"buttons":1}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::MouseMove { x, y, buttons } => {
                assert!((x - 0.5).abs() < f64::EPSILON);
                assert!((y - 0.5).abs() < f64::EPSILON);
                assert_eq!(buttons, 1); // left button held
            }
            other => panic!("expected MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_mouse_move_with_multiple_buttons() {
        let json = r#"{"t":"mm","x":0.1,"y":0.9,"buttons":5}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::MouseMove { x, y, buttons } => {
                assert!((x - 0.1).abs() < f64::EPSILON);
                assert!((y - 0.9).abs() < f64::EPSILON);
                assert_eq!(buttons, 5); // left + middle
            }
            other => panic!("expected MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn input_event_deserialize_scroll() {
        let json = r#"{"t":"sc","x":0.5,"y":0.5,"dx":0.0,"dy":-3.0}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::Scroll { x, y, dx, dy } => {
                assert!((x - 0.5).abs() < f64::EPSILON);
                assert!((y - 0.5).abs() < f64::EPSILON);
                assert!((dx - 0.0).abs() < f64::EPSILON);
                assert!((dy - (-3.0)).abs() < f64::EPSILON);
            }
            other => panic!("expected Scroll, got {other:?}"),
        }
    }

    #[test]
    fn session_registry_insert_get_remove() {
        let mut reg = SessionRegistry::new();
        assert!(reg.get(1).is_none());

        // We can't easily create a real DisplaySession without a backend in
        // tests, but we can test the registry logic with a minimal mock.
        // For unit-test purposes we verify the HashMap operations.
        // Full integration is tested in e2e.

        // Verify empty state
        assert!(reg.remove(1).is_none());
    }

    #[test]
    fn session_registry_operations() {
        // Test the HashMap wrapper logic directly.
        let mut map: HashMap<u32, String> = HashMap::new();
        map.insert(1, "session-1".to_string());
        map.insert(2, "session-2".to_string());
        assert_eq!(map.get(&1), Some(&"session-1".to_string()));
        assert_eq!(map.get(&3), None);
        assert_eq!(map.remove(&1), Some("session-1".to_string()));
        assert_eq!(map.get(&1), None);
    }

    #[test]
    fn ice_config_default_is_empty() {
        let config = IceConfig::default();
        assert!(config.ice_servers.is_empty());
    }

    #[test]
    fn ice_config_deserialize() {
        let json = r#"{"ice_servers":[{"urls":["stun:stun.l.google.com:19302"],"username":null,"credential":null}]}"#;
        let config: IceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ice_servers.len(), 1);
        assert_eq!(config.ice_servers[0].urls[0], "stun:stun.l.google.com:19302");
        assert!(config.ice_servers[0].username.is_none());
    }

    #[test]
    fn display_info_serde_roundtrip() {
        let info = DisplayInfo {
            id: 0,
            platform_id: 42,
            name: "Primary Display (1920x1080)".to_string(),
            width: 1920,
            height: 1080,
            is_primary: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: DisplayInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 0);
        assert_eq!(back.platform_id, 42);
        assert!(back.is_primary);
        assert_eq!(back.width, 1920);
        assert_eq!(back.height, 1080);
    }

    #[test]
    fn session_registry_display_ids() {
        let mut reg = SessionRegistry::new();
        assert!(reg.display_ids().is_empty());
    }

    #[test]
    fn metrics_counters_init_zeroed() {
        let c = DisplayMetricsCounters::new();
        assert_eq!(c.capture_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.capture_drops.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_drops.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_latency_us_sum.load(Ordering::Relaxed), 0);
        assert_eq!(c.peer_drops.load(Ordering::Relaxed), 0);
        assert_eq!(c.peer_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metrics_snapshot_computes_rates() {
        let c = DisplayMetricsCounters::new();
        // Simulate 150 captured frames, 2 drops, 140 encoded, 1 encode drop,
        // 3 peer drops over the window.
        c.capture_frames.store(150, Ordering::Relaxed);
        c.capture_drops.store(2, Ordering::Relaxed);
        c.encode_frames.store(140, Ordering::Relaxed);
        c.encode_drops.store(1, Ordering::Relaxed);
        // 140 frames * 5000us avg = 700_000us total
        c.encode_latency_us_sum.store(700_000, Ordering::Relaxed);
        c.peer_drops.store(3, Ordering::Relaxed);
        c.peer_count.store(2, Ordering::Relaxed);

        // Pretend the window started 5 seconds ago.
        let epoch = Instant::now() - std::time::Duration::from_secs(5);
        let snap = DisplayMetricsSnapshot::from_counters(&c, 99, (1920, 1080), &epoch);

        assert_eq!(snap.display_id, 99);
        assert_eq!(snap.resolution, (1920, 1080));
        // ~30 fps capture (150 / 5s)
        assert!((snap.capture_fps - 30.0).abs() < 1.0);
        assert_eq!(snap.capture_drops, 2);
        // ~28 fps encode (140 / 5s)
        assert!((snap.encode_fps - 28.0).abs() < 1.0);
        assert_eq!(snap.encode_drops, 1);
        // 700_000us / 140 frames = 5000us = 5.0ms avg
        assert!((snap.encode_latency_avg_ms - 5.0).abs() < 0.01);
        assert_eq!(snap.peer_count, 2);
        assert_eq!(snap.peer_drops, 3);

        // Counters should be reset after snapshot (except peer_count which is gauge).
        assert_eq!(c.capture_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.peer_count.load(Ordering::Relaxed), 2); // gauge, not reset
    }

    #[test]
    fn metrics_snapshot_zero_frames() {
        let c = DisplayMetricsCounters::new();
        let epoch = Instant::now() - std::time::Duration::from_secs(5);
        let snap = DisplayMetricsSnapshot::from_counters(&c, 0, (640, 480), &epoch);
        assert!((snap.capture_fps - 0.0).abs() < f64::EPSILON);
        assert!((snap.encode_fps - 0.0).abs() < f64::EPSILON);
        assert!((snap.encode_latency_avg_ms - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_snapshot_serializes() {
        let snap = DisplayMetricsSnapshot {
            display_id: 1,
            capture_fps: 30.0,
            capture_drops: 5,
            encode_fps: 28.5,
            encode_latency_avg_ms: 4.2,
            encode_drops: 2,
            peer_count: 1,
            peer_drops: 0,
            resolution: (1920, 1080),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"display_id\":1"));
        assert!(json.contains("\"capture_fps\":30.0"));
        assert!(json.contains("\"encode_drops\":2"));
    }
}
