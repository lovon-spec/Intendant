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

pub mod clipboard;
pub mod encode;
pub mod forward;
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
///
/// Note: on Wayland, platform enumeration cannot know the true capture
/// resolution before a portal session is opened — it returns a placeholder.
/// Callers that have access to the live session registry should prefer
/// [`enumerate_displays_with_sessions`], which patches each entry with the
/// session's actual stream resolution.
pub async fn enumerate_displays() -> Vec<DisplayInfo> {
    let displays = enumerate_displays_platform().await;
    if displays.is_empty() {
        // macOS ScreenCaptureKit may not be ready on first call (TCC prompt,
        // cold start). Retry once after a brief delay before falling back.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let retry = enumerate_displays_platform().await;
        if retry.is_empty() {
            vec![DisplayInfo {
                id: 0,
                platform_id: 0,
                name: "Default Display".to_string(),
                width: 1920,
                height: 1080,
                is_primary: true,
            }]
        } else {
            retry
        }
    } else {
        displays
    }
}

/// Enumerate displays and overlay each entry with the live resolution from
/// its active capture session, when one exists.
///
/// This exists because [`enumerate_displays`] cannot see the session registry
/// and therefore cannot report the true resolution on Wayland, where the
/// portal grants a stream at whatever size it likes (often a downscale of the
/// compositor resolution). Agents calling `list_displays` need the true
/// capture size so their click coordinates match the screenshot they receive.
pub async fn enumerate_displays_with_sessions(
    registry: &Option<SharedSessionRegistry>,
) -> Vec<DisplayInfo> {
    let mut displays = enumerate_displays().await;
    if let Some(reg) = registry.as_ref() {
        let reg = reg.read().await;
        for d in &mut displays {
            if let Some(session) = reg.get(d.id) {
                let (w, h) = session.resolution();
                if w > 0 && h > 0 {
                    d.width = w;
                    d.height = h;
                }
            }
        }
    }
    displays
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

/// Encoded video frame -- shared across peers, each peer packetizes independently.
///
/// Carries a [`encode::PayloadSpec`] so the per-peer WebRTC driver can
/// resolve the peer-negotiated RTP payload type via `str0m::Writer::match_params`
/// and cache the result. H.264 frames in particular need the full spec
/// (profile-level-id + packetization-mode) because str0m discriminates
/// parameter sets, not just codec names.
#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_ms: u64,
    pub duration_ms: u64,
    pub is_keyframe: bool,
    /// Codec + fmtp identity of this frame. Set by the encoder at
    /// construction time; propagated unchanged through the pipeline.
    pub payload_spec: encode::PayloadSpec,
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
    ///
    /// `Arc<AtomicU64>` (not bare `AtomicU64`) so the pool-mode
    /// per-peer intake task at `webrtc.rs::pool_frame_intake` can
    /// share the same counter via `Arc::clone(&self.counters.peer_drops)`.
    /// Until 3c.4 deletes the legacy fan-out, both paths feed this
    /// counter so `DisplayMetricsSnapshot.peer_drops` continues to
    /// reflect total drops across pre-pool and pool peers.
    pub peer_drops: Arc<AtomicU64>,
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
            peer_drops: Arc::new(AtomicU64::new(0)),
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
    /// Serializes codec selection + encoder startup on the first `handle_offer()`.
    /// Guards a bool: `false` = encoder not yet started, `true` = running.
    /// All concurrent first-offer callers block on this mutex so only one
    /// task performs codec negotiation and starts the encoder pipeline.
    encoder_init_lock: Mutex<bool>,
    /// Capture FPS stored from `start()` for deferred encoder startup.
    fps: Mutex<u32>,
    /// EventBus stored from `start()` for deferred encoder startup.
    encoder_event_bus: Mutex<Option<crate::event::EventBus>>,
    /// Clipboard monitor for bidirectional clipboard sync.
    clipboard_monitor: Arc<clipboard::ClipboardMonitor>,
    /// Handle for the clipboard forwarding task (remote -> browser).
    clipboard_handle: Mutex<Option<JoinHandle<()>>>,
    /// Channel used by `handle_offer` to wake the capture/encode bridge when
    /// a new peer attaches.  The bridge responds by forcing a keyframe on
    /// the next encoded frame (so a peer joining on an idle desktop gets a
    /// decodable reference within ~1 tick instead of waiting for the next
    /// GOP boundary).  `Some` after `start_encoder_pipeline()` has run.
    keyframe_tx: Mutex<Option<mpsc::UnboundedSender<()>>>,
    /// Shared multi-codec encoder pool. Constructed lazily on the first
    /// `start()` call once the backend's resolution and fps are known;
    /// ownership is `Arc` so subsequent integration code can hand it to
    /// per-peer forwarders without lifetime juggling.
    ///
    /// Phase 3c.1 only establishes the lifetime — the pool spawns its
    /// always-on VP8 encoder, but nothing pushes frames into it and no
    /// peer subscribes. The idle encoder thread is ~free (it blocks in
    /// `blocking_recv` until the bridge starts publishing I420 frames in
    /// 3c.2). The existing single-encoder pipeline at
    /// `start_encoder_pipeline` / `codec_mime` / `encoder_init_lock`
    /// remains the live path until 3c.4 flips the default and deletes
    /// the old fanout.
    ///
    /// `std::sync::OnceLock` (stable since 1.70) is the right shape:
    /// one-time init with shared read access afterward, synchronous so
    /// the blocking portion of `start()` can construct it inline, no
    /// tokio dependency. Concurrent `start()` callers (not expected but
    /// cheap to tolerate) converge on a single pool.
    pool: std::sync::OnceLock<Arc<encode::pool::EncoderPool>>,
}

/// Parse the truthiness of an `INTENDANT_DISPLAY_POOL` env value
/// (or any equivalent on/off flag). `Some("1")`, `Some("true")` (any
/// case), `Some("TRUE")` → `true`. Everything else (`None`, empty
/// string, `"0"`, `"false"`, garbage) → `false`.
///
/// Extracted from [`pool_mode_enabled`] so the parsing rules can be
/// unit-tested without touching `std::env::var` (env mutation in
/// parallel tests is racy across the cargo-test process).
fn parse_pool_flag(value: Option<&str>) -> bool {
    match value {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        None => false,
    }
}

/// Whether `INTENDANT_DISPLAY_POOL=1` (or `=true`, case-insensitive)
/// routes new offers through the encoder-pool path
/// (`DisplaySession::handle_offer_pool_mode`). Default OFF — the
/// legacy single-encoder fan-out is the live path until 3c.4 flips
/// the default and 3c.5 deletes the legacy code.
///
/// Read per-call rather than at startup so an operator can flip the
/// flag mid-session via `kill -HUP`-and-restart pattern (or, more
/// usefully, set/unset between offers in a debug session). Concurrent
/// peers can be on different paths; the [`webrtc::WebRtcPeer::is_pool_mode`]
/// marker keeps the legacy fan-out from duplicating frames into pool
/// peers.
fn pool_mode_enabled() -> bool {
    parse_pool_flag(std::env::var("INTENDANT_DISPLAY_POOL").as_deref().ok())
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
            encoder_init_lock: Mutex::new(false),
            fps: Mutex::new(30),
            encoder_event_bus: Mutex::new(None),
            clipboard_monitor: Arc::new(clipboard::ClipboardMonitor::new()),
            clipboard_handle: Mutex::new(None),
            keyframe_tx: Mutex::new(None),
            pool: std::sync::OnceLock::new(),
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

        // Store fps and event_bus for deferred encoder startup.
        // The encoder pipeline is started on the first handle_offer() call
        // so we know which codec the peer negotiated.
        *self.fps.lock().await = fps;
        *self.encoder_event_bus.lock().await = event_bus_for_encoder;

        // Phase 3c.1: construct the shared encoder pool with a single
        // always-on VP8 layer at the source resolution. The pool spawns
        // its encoder thread immediately; that thread blocks in
        // `blocking_recv` until the bridge task starts publishing I420
        // frames in 3c.2. Idle cost is negligible. No peer subscribes to
        // this pool until 3c.3 wires `handle_offer` through
        // `pool.subscribe(...)`; the pre-pool pipeline
        // (`start_encoder_pipeline`, `codec_mime`, `encoder_init_lock`)
        // stays the live path until 3c.4.
        //
        // `get_or_init` swallows concurrent initializations cheaply; in
        // practice `start()` is called at most once per session.
        let _ = self.pool.get_or_init(|| {
            Arc::new(encode::pool::EncoderPool::new(
                width,
                height,
                fps,
                vec![encode::pool::LayerSpec::single(
                    encode::pool::CodecKind::Vp8,
                    width,
                    height,
                    fps,
                )],
            ))
        });

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
        self.clipboard_monitor.stop();
        self.backend.stop_capture().await;

        if let Some(h) = self.capture_handle.lock().await.take() {
            let _ = h.await;
        }
        if let Some(h) = self.encoder_handle.lock().await.take() {
            let _ = h.await;
        }
        if let Some(h) = self.clipboard_handle.lock().await.take() {
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

    /// Start the encoder pipeline with the given codec.
    ///
    /// Called exactly once from `handle_offer()` under `encoder_init_lock`.
    /// The caller is responsible for guarding against double-start.
    async fn start_encoder_pipeline(&self, codec_mime: &'static str) {
        let fps = *self.fps.lock().await;
        let event_bus = self.encoder_event_bus.lock().await.clone();

        let (width, height) = self.backend.resolution();
        let mut broadcast_rx = self.frame_tx.subscribe();
        let peers = Arc::clone(&self.peers);

        let duration_ms = if fps > 0 { 1000 / fps as u64 } else { 33 };

        // Encoded frame channel -- survives encoder restarts.  The fanout
        // task reads from `efr_rx`; each encoder thread gets a clone of
        // `efr_tx` so dropping+respawning the encoder thread does not
        // close the channel.
        let (efr_tx, mut efr_rx) = mpsc::channel::<Arc<EncodedFrame>>(16);

        // Spawn the initial encoder thread.
        // Channel payload: (i420 buffer, arrival time, force_keyframe_flag).
        let (i420_tx, i420_rx) =
            std::sync::mpsc::sync_channel::<(Vec<u8>, Instant, bool)>(4);

        let enc_counters = Arc::clone(&self.counters);
        let encoder_shutdown = self.shutdown.clone();
        spawn_encoder_thread(
            width, height, duration_ms,
            codec_mime,
            i420_rx, efr_tx.clone(),
            Arc::clone(&enc_counters), encoder_shutdown,
        );

        let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
        *self.keyframe_tx.lock().await = Some(kf_tx);

        let bridge_counters = Arc::clone(&self.counters);
        let shutdown_bridge = self.shutdown.clone();
        let frame_interval = std::time::Duration::from_millis(
            if fps > 0 { 1000 / fps as u64 } else { 33 },
        );
        let display_id = self.display_id;
        let codec_mime_for_bridge = codec_mime;
        // Phase 3c.2: capture the pool Arc (if populated by `start()`) so
        // the bridge can dual-feed I420 frames into it. No peer subscribes
        // to the pool until 3c.3 wires `handle_offer`; this feed is
        // transitional and exists so the pool's always-on encoder sees
        // real frames rather than sitting idle. Cloned here (outside the
        // spawned task) so the task closure owns an `Option<Arc>` and
        // doesn't need to touch `self.pool` at runtime.
        //
        // Deliberately unconditional — if the pool is None (shouldn't
        // happen post-start, but belt-and-braces), the dual-feed is a
        // no-op. Old path stays the only one that affects peer
        // observability until 3c.3/3c.4.
        let pool_for_bridge: Option<Arc<encode::pool::EncoderPool>> =
            self.pool.get().map(Arc::clone);
        let bridge_handle = tokio::spawn(async move {
            // Track current encoder dimensions for resize detection.
            let mut enc_width = width;
            let mut enc_height = height;
            let mut i420_tx = i420_tx;
            // Most recently captured (and BGRA->I420 converted) frame.
            // `generation` is bumped each time the capture branch replaces
            // the buffer; the tick branch compares against `last_sent_gen`
            // to tell new content from a repeat.
            let mut latest_i420: Option<(Vec<u8>, Instant)> = None;
            let mut generation: u64 = 0;
            let mut last_sent_gen: Option<u64> = None;
            // Wall-clock time of the most recent send to the encoder.
            // Used to drive the idle heartbeat.
            let mut last_send_at = Instant::now();
            // Force-keyframe window opened by `handle_offer` when a new
            // peer attaches.  During the window every tick forwards to
            // the encoder regardless of dirty state; the very first
            // forwarded frame carries `force_keyframe=true`.  Sized to
            // comfortably exceed one H.264 GOP at the configured feed
            // rate so the fallback path (ffmpeg-pipe, which ignores the
            // force flag) still lands a natural keyframe inside it.
            let mut burst_until: Option<Instant> = None;
            let mut burst_first = false;
            // Heartbeat: when the buffer hasn't changed and we aren't in
            // a burst, send one repeat every IDLE_HEARTBEAT so the
            // encoder's internal timebase and RTP cadence keep flowing
            // without re-encoding the same static frame 30 times/second.
            const IDLE_HEARTBEAT: std::time::Duration =
                std::time::Duration::from_secs(1);
            const PEER_JOIN_BURST: std::time::Duration =
                std::time::Duration::from_millis(1500);

            let mut tick = tokio::time::interval(frame_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown_bridge.cancelled() => break,
                    _ = kf_rx.recv() => {
                        // Peer attached (or a PLI is being simulated).
                        // Open/refresh the burst window.  The next tick
                        // will forward the latest frame with
                        // force_keyframe=true.
                        burst_until = Some(Instant::now() + PEER_JOIN_BURST);
                        burst_first = true;
                    }
                    result = broadcast_rx.recv() => {
                        let frame = match result {
                            Ok(f) => f,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        };

                        // -- Resize detection --
                        // Round to even dimensions for codec compatibility.
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

                            // Drop the old sender -- the encoder thread's
                            // `i420_rx.recv()` will return Err and the
                            // thread will exit cleanly.
                            drop(i420_tx);
                            // Drop stale buffer: dimensions changed.
                            latest_i420 = None;
                            last_sent_gen = None;

                            // Spawn a fresh encoder thread at the new
                            // dimensions, reusing the same `efr_tx`.
                            let (new_tx, new_rx) =
                                std::sync::mpsc::sync_channel::<(Vec<u8>, Instant, bool)>(4);
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

                            // Phase 3c.3a: replace the pool's encoders
                            // at the new dimensions in the same beat
                            // the old path respawns its encoder. After
                            // this call the pool's dimensions, its
                            // always_on handles, and (eventually) its
                            // on_demand slots are all coherent with
                            // the new capture size, and the
                            // dimensions-gate from 3c.2a becomes an
                            // invariant assert rather than a runtime
                            // skip.
                            //
                            // Subscribers (3c.3b onward) see their
                            // broadcast::Receiver<Arc<EncodedFrame>>
                            // close on the next recv and must
                            // re-subscribe via `pool.subscribe`. The
                            // forwarder learns to do that when 3c.3b
                            // lands peer routing; until then, no
                            // subscribers exist and the swap is
                            // pool-internal only.
                            if let Some(ref pool) = pool_for_bridge {
                                pool.on_resize(enc_width, enc_height);
                            }
                        }

                        // Convert the new BGRA capture to I420 and stash
                        // it as the latest buffer.  The tick branch will
                        // pick it up on the next interval (or sooner if
                        // we're in a burst).  We deliberately keep the
                        // conversion out of the send path so the broadcast
                        // receiver can drain quickly; libvpx/libx264 see
                        // frames at the tick rate, decoupled from capture
                        // jitter.
                        let arrived = Instant::now();
                        let frame_arc = Arc::clone(&frame);
                        let i420 = tokio::task::spawn_blocking(move || {
                            encode::bgra_to_i420(
                                &frame_arc.data,
                                frame_arc.width,
                                frame_arc.height,
                                frame_arc.stride,
                            )
                        }).await;
                        if let Ok(i420) = i420 {
                            generation = generation.wrapping_add(1);
                            latest_i420 = Some((i420, arrived));
                        }
                    }
                    _ = tick.tick() => {
                        let Some((ref i420, arrived)) = latest_i420 else {
                            // No capture yet -- nothing to forward.  The
                            // encoder is happy to sit idle.
                            continue;
                        };

                        let in_burst = burst_until
                            .map(|t| Instant::now() < t)
                            .unwrap_or(false);
                        if !in_burst {
                            burst_until = None;
                        }

                        let changed = last_sent_gen != Some(generation);
                        let heartbeat_due = last_send_at.elapsed() >= IDLE_HEARTBEAT;

                        if !(changed || heartbeat_due || in_burst) {
                            // Nothing new and nothing overdue -- skip the
                            // tick.  This is the main CPU win: a fully
                            // idle desktop stops clocking the encoder
                            // at 30fps.
                            continue;
                        }

                        let force_kf = in_burst && burst_first;
                        if force_kf {
                            burst_first = false;
                        }

                        let old_path_ok = i420_tx
                            .try_send((i420.clone(), arrived, force_kf))
                            .is_ok();
                        if !old_path_ok {
                            bridge_counters
                                .encode_drops
                                .fetch_add(1, Ordering::Relaxed);
                        }

                        // Phase 3c.2: dual-feed the same gated I420 frame
                        // into the pool's broadcast so the pool's
                        // always-on encoder (and, in 3c.3+, on-demand
                        // encoders) see the same tick rhythm, heartbeat
                        // cadence, and burst window the old path does.
                        // Pool has its own broadcast capacity and its
                        // own dropping semantics, so the two paths don't
                        // serialize on each other. `push_i420_frame`
                        // returns the current subscriber count — a
                        // healthy pool post-`start()` has ≥1 always-on
                        // encoder subscribed, but we don't react to the
                        // return value here (the pool's own lag/drop
                        // handling covers encoder stalls).
                        //
                        // Done unconditionally (not gated on
                        // `old_path_ok`) because the pool has independent
                        // capacity; rate-limiting it to the old path's
                        // success would tie the new path's throughput
                        // to the old one's failure mode, which is the
                        // opposite of what dual-feed is for.
                        //
                        // TODO 3c.4: when the old single-encoder path
                        // is deleted, switch to `Arc<Vec<u8>>` end-to-end
                        // so this doesn't double-clone.
                        if let Some(ref pool) = pool_for_bridge {
                            // 3c.3a established the invariant that the
                            // resize branch above calls
                            // `pool.on_resize(...)` in the same beat it
                            // updates `enc_width`/`enc_height`, so by
                            // the time we get here the pool's
                            // dimensions always match the bridge's.
                            // `debug_assert` catches a future regression
                            // (someone adds a resize path that bypasses
                            // pool.on_resize) without costing anything
                            // in release builds.
                            debug_assert_eq!(
                                pool.dimensions(),
                                (enc_width, enc_height),
                                "pool.on_resize must be called in the bridge's \
                                 resize branch before any push at the new \
                                 dimensions"
                            );
                            pool.push_i420_frame(
                                Arc::new(i420.clone()),
                                arrived,
                            );
                        }

                        if old_path_ok {
                            last_sent_gen = Some(generation);
                            last_send_at = Instant::now();
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
                            eprintln!(
                                "[display/fanout] display {} encoder channel \
                                 closed, fan-out exiting",
                                fanout_display_id,
                            );
                            break;
                        };
                        let peers_guard = peers.read().await;
                        for peer in peers_guard.values() {
                            // Phase 3c.3b.3: skip pool-mode peers.
                            // They receive frames via their per-peer
                            // `pool_frame_intake` task; forwarding
                            // here would produce duplicate RTP
                            // samples on the peer's encoded-frame
                            // mpsc and corrupt decode (codec PTs
                            // wouldn't match either, since the pool
                            // peer's str0m enabled codec set was
                            // negotiated separately from the legacy
                            // session codec). The marker is a bool
                            // on `WebRtcPeer` (no lock acquisition
                            // on this hot path).
                            if peer.is_pool_mode() {
                                continue;
                            }
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

    /// Handle a WebRTC SDP offer from a browser peer.
    ///
    /// On the first call, selects the best available codec and starts the
    /// encoder pipeline.  Subsequent peers reuse the running encoder's codec
    /// without re-negotiating -- all peers share one encoder, so the codec
    /// is locked once it starts.
    ///
    /// The `encoder_init_lock` mutex serializes codec selection and encoder
    /// startup so that concurrent first-offer calls cannot race on codec
    /// negotiation.  Only the first caller performs negotiation and starts
    /// the encoder; all others wait and then use the established codec.
    ///
    /// Creates a `WebRtcPeer`, subscribes it to the encoder output, adds it to
    /// the peer map, starts clipboard monitoring, and returns the SDP answer.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_offer(
        &self,
        peer_id: PeerId,
        sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<self::webrtc::TcpPeerRegistry>>,
        tcp_advertised_addr: Option<std::net::SocketAddr>,
        ice_tx: mpsc::Sender<(PeerId, String)>,
    ) -> Result<String, CallerError> {
        // Phase 3c.3b.3: env-gated route to the pool-mode path.
        // `INTENDANT_DISPLAY_POOL=1` (or `=true`, case-insensitive)
        // routes the offer through the encoder pool +
        // `WebRtcPeer::new_pool_mode`. Default OFF — the legacy
        // single-encoder fan-out below remains the live path until
        // 3c.4 flips the default and 3c.5 deletes the legacy code.
        // Read per-call so an operator can enable the flag without
        // restarting; concurrent peers can be on different paths
        // (the `WebRtcPeer.pool_mode` marker keeps the fan-out from
        // duplicating frames into pool peers).
        if pool_mode_enabled() {
            return self
                .handle_offer_pool_mode(
                    peer_id,
                    sdp,
                    ice_config,
                    tcp_peer_registry,
                    tcp_advertised_addr,
                    ice_tx,
                )
                .await;
        }

        // Serialize codec selection + encoder startup.
        let codec_mime = {
            let mut init = self.encoder_init_lock.lock().await;
            if *init {
                // Encoder already running -- verify the new peer supports
                // the locked codec (name + fmtp profile for H264).
                let locked_mime = *self.codec_mime.read().await;
                let locked_name = locked_mime.split('/').last().unwrap_or("");
                if locked_name.eq_ignore_ascii_case("H264") {
                    // For H264, verify fmtp compatibility (profile-level-id
                    // and packetization-mode), not just codec name.
                    if !encode::has_compatible_h264_offer(sdp) {
                        let browser_codecs = encode::parse_offered_codecs(sdp);
                        return Err(CallerError::WebRtc(format!(
                            "peer does not support session codec {} with compatible profile (offered: {:?})",
                            locked_mime, browser_codecs,
                        )));
                    }
                } else {
                    // Non-H264 codecs: name-only check.
                    let browser_codecs = encode::parse_offered_codecs(sdp);
                    if !browser_codecs
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(locked_name))
                    {
                        return Err(CallerError::WebRtc(format!(
                            "peer does not support session codec {} (offered: {:?})",
                            locked_mime, browser_codecs,
                        )));
                    }
                }
                locked_mime
            } else {
                // First peer -- negotiate codec from SDP and start encoder.
                let (width, height) = self.backend.resolution();
                let (_encoder, codec_choice) = encode::select_codec(sdp, width, height, 2000);
                let mime = codec_choice.mime();
                *self.codec_mime.write().await = mime;
                self.start_encoder_pipeline(mime).await;
                *init = true;
                mime
            }
        };

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

        // Clipboard handler: browser -> remote clipboard.
        let clipboard_monitor = Arc::clone(&self.clipboard_monitor);
        let clipboard_handler: Arc<dyn Fn(clipboard::ClipboardContent) + Send + Sync> =
            Arc::new(move |content: clipboard::ClipboardContent| {
                let monitor = Arc::clone(&clipboard_monitor);
                tokio::spawn(async move {
                    match content {
                        clipboard::ClipboardContent::Text(text) => {
                            if let Err(e) = monitor.set_text(&text).await {
                                eprintln!("[display/clipboard] set_text failed: {e}");
                            }
                        }
                        clipboard::ClipboardContent::Image { mime, data } => {
                            if let Err(e) = monitor.set_image(&mime, &data).await {
                                eprintln!("[display/clipboard] set_image failed: {e}");
                            }
                        }
                    }
                });
            });

        // Translate the session-level `codec_mime` (today: singleton because
        // the encoder pool isn't yet wired) into the per-peer codec set
        // WebRtcPeer::new now expects. Phase 3 will replace this singleton
        // with the actual pool's currently-running codec set, at which point
        // a peer offering multiple codecs gets its Rtc configured to
        // negotiate the best overlap rather than pre-locked to the first
        // peer's choice.
        let codec_kind = encode::pool::CodecKind::from_mime(codec_mime)
            .ok_or_else(|| CallerError::WebRtc(format!(
                "unsupported session codec mime: {codec_mime}"
            )))?;
        let codec_set = [codec_kind];

        let (peer, answer_sdp) = self::webrtc::WebRtcPeer::new(
            peer_id,
            sdp,
            &codec_set,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            input_handler,
            clipboard_handler,
            ice_tx,
        )
        .await?;

        let peer = Arc::new(peer);
        // `HashMap::insert` silently drops a displaced value, which strands
        // the previous peer's driver task and — worse — lets the gauge
        // increment below double-count it. Every `peer_count` drift I saw
        // during federation smoke-tests (1 → 5 → 6 across a single
        // debugging session, even as clients closed) was one of these
        // replacements. Toggling the display off/on "fixed" it by wiping
        // the whole HashMap on session teardown; doing it properly means
        // closing the replaced peer and skipping the fetch_add so the
        // counter stays a true gauge.
        //
        // Replacement happens legitimately on every repeat offer for the
        // same `peer_id` — browser renegotiation, federated signaling that
        // reuses the primary's WS peer_id across sessions, a Retry-After
        // driven reconnect, etc. — so this isn't a corner case.
        let replaced = {
            let mut guard = self.peers.write().await;
            guard.insert(peer_id, Arc::clone(&peer))
        };
        match replaced {
            Some(old) => {
                old.close().await;
            }
            None => {
                self.counters.peer_count.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Start clipboard monitoring (remote -> browser) if not already running.
        self.ensure_clipboard_forwarding().await;

        // Wake the bridge so the next forwarded frame is a forced keyframe.
        // Without this, a peer joining during an idle desktop would wait up
        // to one GOP interval (and, for VP8 on static content, potentially
        // much longer) for a decodable reference.
        if let Some(tx) = self.keyframe_tx.lock().await.as_ref() {
            let _ = tx.send(());
        }

        Ok(answer_sdp)
    }

    /// Pool-mode counterpart to [`Self::handle_offer`], gated by
    /// `INTENDANT_DISPLAY_POOL=1` (3c.3b.3 → 3c.4 cutover).
    ///
    /// Differences from the legacy path:
    ///   - No `encoder_init_lock` dance: codec selection is per-peer
    ///     via str0m's `enable_*()` calls in `WebRtcPeer::new_pool_mode`,
    ///     driven by the codecs the pool's initial subscribe actually
    ///     returned. No first-peer codec lock.
    ///   - No `keyframe_tx` wake: pool peers don't share the legacy
    ///     bridge's keyframe channel. Per-peer keyframe-on-join is
    ///     wired via the encoder pool's `request_keyframe_*` API in
    ///     3c.4 (today the encoder's intrinsic GOP cadence still
    ///     produces a keyframe within ~one GOP boundary; the
    ///     PLI-driven explicit request lands with the simulcast
    ///     work).
    ///   - No `pool_leases` tracking on `DisplaySession`:
    ///     `WebRtcPeer::new_pool_mode` hands the lease to the
    ///     per-peer `pool_frame_intake` task, which owns it for the
    ///     peer's lifetime. `Self::remove_peer` calls
    ///     `peer.close()`, which fires the shutdown token, which the
    ///     intake task observes — its select-arm drops the lease via
    ///     `drop(current_lease.take())` (RAII releases on-demand
    ///     refcounts under the existing generation gate). The
    ///     primer's "DisplaySession stores HashMap<PeerId, PoolLease>"
    ///     suggestion was written before the 3c.3b.2 design
    ///     finalized intake-owns-lease; tracking the lease in two
    ///     places would either duplicate ownership (won't compile)
    ///     or race the intake's resubscribe path on early release.
    ///
    /// Common with legacy: input/clipboard handler closures, peer
    /// insertion + replaced-peer close + counter, ensure_clipboard_forwarding.
    /// 3c.4 deletes the legacy path and folds the common boilerplate
    /// into a single fn; until then the duplication is intentional.
    async fn handle_offer_pool_mode(
        &self,
        peer_id: PeerId,
        sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<self::webrtc::TcpPeerRegistry>>,
        tcp_advertised_addr: Option<std::net::SocketAddr>,
        ice_tx: mpsc::Sender<(PeerId, String)>,
    ) -> Result<String, CallerError> {
        let pool = self.pool.get().ok_or_else(|| {
            CallerError::WebRtc(
                "pool-mode handle_offer: encoder pool not initialized — \
                 caller must invoke DisplaySession::start() before serving \
                 offers"
                    .to_string(),
            )
        })?;

        // Ensure the capture-to-I420 bridge is running. The bridge's
        // `pool.push_i420_frame` call is the only path that delivers
        // captured frames into the pool's always-on encoder; without
        // it, this peer subscribes successfully but the encoder never
        // produces output → connected WebRTC peer with a black stream.
        //
        // The bridge today lives inside `start_encoder_pipeline`, which
        // also starts the legacy single-encoder thread + fan-out. In a
        // pool-only deployment those run with no consumers (the
        // fan-out's `is_pool_mode` filter at `display/mod.rs:1126`
        // skips pool peers, and there are no legacy peers). That's a
        // bounded CPU waste — bounded because 3c.4 deletes the legacy
        // path entirely. We accept it during the cutover to keep the
        // BGRA→I420 conversion single-pass: a separate pool-only
        // bridge would double the conversion cost in mixed-mode
        // deployments (legacy + pool peers on the same session, which
        // is the common cutover scenario), and would require a
        // second `pool.push_i420_frame` path that races the legacy
        // bridge's existing dual-feed and produces double-encoded
        // frames at the pool's broadcast — same shape as the
        // multi-sub fan-out bug from 3c.3b.2a.
        //
        // VP8 is the always-on codec the pool spawned at `start()`;
        // matching the legacy encoder's codec to it keeps both
        // encoders aligned on the same session-level codec mime so
        // that a future legacy peer arriving in mixed mode finds a
        // codec it can pair with via the existing
        // `encoder_init_lock` validation path.
        self.ensure_legacy_bridge_running_for_pool().await;

        // Build the peer's codec preferences from its SDP offer. The
        // strict `forward::codec_preferences_from_offer` filters
        // H.264 by str0m's exact match rules (profile / packetization
        // / level), so an offer that advertises an incompatible H.264
        // variant is correctly excluded here rather than producing a
        // black stream after match_params drops the spec downstream.
        let prefs = forward::codec_preferences_from_offer(sdp);
        if prefs.is_empty() {
            return Err(CallerError::WebRtc(format!(
                "pool-mode: peer offer has no compatible codecs (offered: {:?})",
                encode::parse_offered_codecs(sdp),
            )));
        }

        let (subs, lease) = pool.subscribe(&prefs).map_err(|e| {
            CallerError::WebRtc(format!(
                "pool-mode subscribe failed for peer prefs {:?}: {:?}",
                prefs.supported, e,
            ))
        })?;

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

        let clipboard_monitor = Arc::clone(&self.clipboard_monitor);
        let clipboard_handler: Arc<dyn Fn(clipboard::ClipboardContent) + Send + Sync> =
            Arc::new(move |content: clipboard::ClipboardContent| {
                let monitor = Arc::clone(&clipboard_monitor);
                tokio::spawn(async move {
                    match content {
                        clipboard::ClipboardContent::Text(text) => {
                            if let Err(e) = monitor.set_text(&text).await {
                                eprintln!("[display/clipboard] set_text failed: {e}");
                            }
                        }
                        clipboard::ClipboardContent::Image { mime, data } => {
                            if let Err(e) = monitor.set_image(&mime, &data).await {
                                eprintln!("[display/clipboard] set_image failed: {e}");
                            }
                        }
                    }
                });
            });

        // Share the legacy fan-out's `peer_drops` counter so the
        // pool path's drops show up alongside legacy drops in
        // `DisplayMetricsSnapshot.peer_drops`. Cheap clone (Arc).
        let drops_counter = Arc::clone(&self.counters.peer_drops);

        let (peer, answer_sdp) = self::webrtc::WebRtcPeer::new_pool_mode(
            peer_id,
            sdp,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            input_handler,
            clipboard_handler,
            ice_tx,
            Arc::clone(pool),
            subs,
            lease,
            prefs,
            drops_counter,
        )
        .await?;

        let peer = Arc::new(peer);
        // Same replaced-peer handling as the legacy path: close the
        // displaced peer (if any) so its driver task tears down,
        // skip the gauge bump on replacement so peer_count stays
        // accurate. See the comment block in `handle_offer` (legacy)
        // for the federation-smoke-test backstory.
        let replaced = {
            let mut guard = self.peers.write().await;
            guard.insert(peer_id, Arc::clone(&peer))
        };
        match replaced {
            Some(old) => {
                old.close().await;
            }
            None => {
                self.counters.peer_count.fetch_add(1, Ordering::Relaxed);
            }
        }

        self.ensure_clipboard_forwarding().await;

        Ok(answer_sdp)
    }

    /// Idempotently ensure the legacy capture-to-I420 bridge is
    /// running, with VP8 as the legacy encoder's codec. The bridge's
    /// `pool.push_i420_frame` call is the only path that delivers
    /// captured frames into the pool's always-on encoder; this hook
    /// closes the pool-mode-only black-stream regression flagged in
    /// the 3c.3b.3 review (pool peer subscribed but encoder never
    /// produced output because the bridge had never been started).
    ///
    /// Side effects on first call:
    ///   - `start_encoder_pipeline(VP8)` runs (spawns the bridge,
    ///     legacy encoder thread, fan-out task).
    ///   - `codec_mime` set to VP8.
    ///   - `encoder_init_lock` flipped to `true`.
    ///
    /// Subsequent calls (whether from another pool offer OR the legacy
    /// `handle_offer` path) are no-ops: the lock guard returns early.
    /// In a mixed-mode session where a legacy peer connects after the
    /// bridge was started by a pool peer, the legacy peer's
    /// `encoder_init_lock` check (`handle_offer` line ~1190) finds
    /// init=true and validates its offer's codec against the locked
    /// VP8 — exactly the contract the existing legacy first-peer
    /// path produces.
    ///
    /// 3c.4 will delete `start_encoder_pipeline` (and the encoder /
    /// fan-out it spawns) entirely; the bridge will then live as a
    /// pool-only feed wired in `start()`. This helper is the
    /// transitional shim that keeps pool-mode correctness without
    /// duplicating the bridge.
    async fn ensure_legacy_bridge_running_for_pool(&self) {
        let mut init = self.encoder_init_lock.lock().await;
        if !*init {
            self.start_encoder_pipeline(encode::MIME_TYPE_VP8).await;
            *self.codec_mime.write().await = encode::MIME_TYPE_VP8;
            *init = true;
        }
    }

    /// Ensure a clipboard forwarding task is running.
    ///
    /// Starts watching the system clipboard and forwards changes to all
    /// connected peers via their clipboard data channel.
    async fn ensure_clipboard_forwarding(&self) {
        let mut handle = self.clipboard_handle.lock().await;
        if handle.is_some() {
            return; // already running
        }

        let mut rx = self.clipboard_monitor.start_watching();
        let peers = Arc::clone(&self.peers);
        let shutdown = self.shutdown.clone();

        *handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    content = rx.recv() => {
                        let Some(content) = content else { break };
                        let peers_guard = peers.read().await;
                        for peer in peers_guard.values() {
                            if let Err(e) = peer.send_clipboard(&content).await {
                                eprintln!("[display/clipboard] send to peer failed: {e}");
                            }
                        }
                    }
                }
            }
        }));
    }

    /// Forward a trickle ICE candidate to a connected peer.
    ///
    /// If the peer has already been removed (its `remove_peer` raced ahead
    /// of a late-arrival trickle candidate), this returns `Ok(())` instead
    /// of an error. The candidate is dropped silently because the browser's
    /// own RTCPeerConnection has already transitioned away from this peer
    /// — the candidate has nowhere to go regardless, and raising it to an
    /// error spams the federation log with "unknown peer N" during every
    /// normal disconnect. Before this change, a browser closing a display
    /// panel would reliably emit N of these errors as its last trickled
    /// ICE candidates arrived after `remove_peer` ran.
    pub async fn add_ice_candidate(
        &self,
        peer_id: PeerId,
        candidate_json: &str,
    ) -> Result<(), CallerError> {
        let peers = self.peers.read().await;
        let Some(peer) = peers.get(&peer_id) else {
            return Ok(());
        };
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
    i420_rx: std::sync::mpsc::Receiver<(Vec<u8>, Instant, bool)>,
    efr_tx: mpsc::Sender<Arc<EncodedFrame>>,
    counters: Arc<DisplayMetricsCounters>,
    shutdown: CancellationToken,
) {
    std::thread::spawn(move || {
        let mut encoder: Box<dyn encode::Encoder> = match encode::select_codec_for_mime(codec_mime, width, height, 2000) {
            Ok((enc, _choice)) => enc,
            Err(e) => {
                eprintln!("[display/encoder] {} encoder FAILED for {}x{}: {}", codec_mime, width, height, e);
                return;
            }
        };

        // Watchdog: detect silent encoders that accept input but produce
        // nothing on the wire (e.g. h264_vaapi on hosts where virtio-gpu
        // video acceleration is broken — vaInitialize "succeeds" but no
        // NALs ever come out of stdout). After this many consecutive input
        // frames with no encoded packet, swap to a known-good fallback.
        // 30 frames ≈ 1s at 30fps, well above the normal 1-2 frame
        // pipeline depth for any healthy encoder.
        const WATCHDOG_THRESHOLD: u64 = 30;
        let mut frames_since_last_output: u64 = 0;
        let mut watchdog_swap_done = false;

        while let Ok((i420, arrived, force_keyframe)) = i420_rx.recv() {
            if shutdown.is_cancelled() {
                break;
            }

            let produced = match encoder.encode(&i420, duration_ms, force_keyframe) {
                Ok(packets) => {
                    let n = packets.len();
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
                    n
                }
                Err(e) => {
                    eprintln!("[display/encoder] encode error: {}", e);
                    0
                }
            };

            if produced > 0 {
                frames_since_last_output = 0;
            } else {
                frames_since_last_output += 1;
                if !watchdog_swap_done && frames_since_last_output >= WATCHDOG_THRESHOLD {
                    watchdog_swap_done = true;
                    eprintln!(
                        "[display/encoder] watchdog: {} consecutive input frames produced no output",
                        frames_since_last_output,
                    );
                    if let Some(new_enc) = try_h264_fallback(codec_mime, width, height) {
                        eprintln!(
                            "[display/encoder] watchdog: swapped encoder to libx264 fallback",
                        );
                        encoder = new_enc;
                        frames_since_last_output = 0;
                    } else {
                        eprintln!(
                            "[display/encoder] watchdog: no fallback available, encoder stays",
                        );
                    }
                }
            }
        }
    });
}

/// Attempt a watchdog-triggered swap to the libx264 software H.264 encoder.
///
/// Returns `Some(encoder)` only when:
/// - the codec is H.264 (no fallback strategy for VP8 — libvpx doesn't
///   exhibit this silent-failure pattern), and
/// - VA-API hasn't already been banned (if it has, the current encoder
///   is already libx264 and there's nothing better to swap to), and
/// - the new encoder spawns cleanly.
///
/// On non-Linux targets there's no VA-API path to ban, so this is a no-op.
#[cfg(target_os = "linux")]
fn try_h264_fallback(
    codec_mime: &'static str,
    width: u32,
    height: u32,
) -> Option<Box<dyn encode::Encoder>> {
    if codec_mime != encode::MIME_TYPE_H264 {
        return None;
    }
    if encode::h264_linux::is_vaapi_banned() {
        return None;
    }
    encode::h264_linux::ban_vaapi();
    match encode::select_codec_for_mime(codec_mime, width, height, 2000) {
        Ok((enc, _)) => Some(enc),
        Err(e) => {
            eprintln!(
                "[display/encoder] watchdog: libx264 fallback creation failed: {}",
                e,
            );
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn try_h264_fallback(
    _codec_mime: &'static str,
    _width: u32,
    _height: u32,
) -> Option<Box<dyn encode::Encoder>> {
    None
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

    // -----------------------------------------------------------------------
    // Phase 3c.3b.3: env-flag parsing
    // -----------------------------------------------------------------------

    /// `1` → enabled. The canonical "set" value, matching the
    /// primer's `INTENDANT_DISPLAY_POOL=1` recipe.
    #[test]
    fn parse_pool_flag_one_is_true() {
        assert!(parse_pool_flag(Some("1")));
    }

    /// `true`, `True`, `TRUE`, `tRuE` → enabled. Case-insensitive
    /// because env-var typing is finicky and we'd rather "obviously
    /// truthy" works than make operators remember an exact spelling.
    #[test]
    fn parse_pool_flag_true_is_case_insensitive() {
        for v in ["true", "True", "TRUE", "tRuE", "TRue"] {
            assert!(
                parse_pool_flag(Some(v)),
                "value {:?} must enable pool mode",
                v
            );
        }
    }

    /// Unset → disabled. The default is OFF until 3c.4 flips it.
    #[test]
    fn parse_pool_flag_none_is_false() {
        assert!(!parse_pool_flag(None));
    }

    /// Anything else → disabled. Empty string, `0`, `false`, garbage.
    /// We don't accept `yes`/`on` to stay narrow — `1` and `true`
    /// are the documented recipe; anything else might be an accident.
    #[test]
    fn parse_pool_flag_other_values_are_false() {
        for v in ["", "0", "false", "False", "FALSE", "yes", "on", " 1", "1 ", "no", "garbage"] {
            assert!(
                !parse_pool_flag(Some(v)),
                "value {:?} must NOT enable pool mode (only `1` / `true` do)",
                v
            );
        }
    }

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

    // -----------------------------------------------------------------------
    // Phase 3c.1: pool lifetime on DisplaySession
    // -----------------------------------------------------------------------

    /// Minimal in-process `DisplayBackend` for session-level lifecycle
    /// tests. Returns a dropped receiver from `start_capture` — the
    /// capture bridge sees channel-closed immediately and exits cleanly,
    /// which is enough to exercise the `start()` path without spinning
    /// up a real capture source.
    struct StubBackend {
        width: u32,
        height: u32,
    }

    #[async_trait]
    impl DisplayBackend for StubBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {}
        async fn inject_input(&self, _event: InputEvent) -> Result<(), CallerError> {
            Ok(())
        }
        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }
        fn kind(&self) -> &'static str {
            "stub"
        }
    }

    /// Before `start()` runs, the pool is uninitialized. `get()` returning
    /// `None` is the contract other phases rely on to know whether the
    /// session is hot (bridge running, pool ready) or still cold.
    #[test]
    fn display_session_pool_uninitialized_before_start() {
        let backend = Arc::new(StubBackend { width: 640, height: 480 });
        let session = DisplaySession::new(0, backend);
        assert!(
            session.pool.get().is_none(),
            "pool must be uninitialized until start() runs"
        );
    }

    /// `start()` constructs the pool exactly once, with one always-on
    /// VP8 layer at the capture resolution. This test pins the 3c.1
    /// contract:
    ///   1. The pool is populated after `start()`.
    ///   2. It has exactly one always-on layer (the VP8 layer we asked
    ///      for), not zero (construction failure would panic in
    ///      `EncoderPool::new`) and not many (simulcast lands in phase 4).
    /// Neither the bridge nor any peer is wired to the pool yet, so this
    /// test confirms lifetime only — not frame flow.
    #[tokio::test]
    async fn display_session_start_initializes_pool_with_one_vp8_layer() {
        let backend = Arc::new(StubBackend { width: 640, height: 480 });
        let session = DisplaySession::new(0, backend);
        session.start(30, None, None).await.expect("start() must succeed");
        let pool = session
            .pool
            .get()
            .expect("pool must be initialized after start()");
        assert_eq!(
            pool.always_on().len(),
            1,
            "exactly one always-on VP8 layer spawned"
        );
    }

    /// Phase 3c.2 relies on the pool's always-on VP8 encoder being an
    /// i420-broadcast subscriber the moment `start()` completes — the
    /// bridge's dual-feed `push_i420_frame` call must deliver to at
    /// least one receiver, otherwise the new path is a silent no-op
    /// and 3c.3's peer subscription wiring will observe a never-
    /// decoding stream. This test locks that precondition by pushing
    /// a synthetic I420 frame through the pool directly (no bridge
    /// involvement) and asserting the subscriber count is non-zero.
    ///
    /// If this test ever fires, `EncoderPool::new` is spawning its
    /// always-on encoders without subscribing them to `i420_tx` — a
    /// silent-black-screen regression that phase 4 (simulcast) would
    /// amplify.
    #[tokio::test]
    async fn pool_always_on_encoder_subscribed_to_i420_after_start() {
        let backend = Arc::new(StubBackend { width: 640, height: 480 });
        let session = DisplaySession::new(0, backend);
        session.start(30, None, None).await.expect("start() must succeed");
        let pool = session.pool.get().expect("pool initialized after start()");

        // 640x480 I420 frame is width*height*3/2 bytes (Y plane full,
        // U+V half each). Contents don't matter — we're checking the
        // broadcast topology, not encoder output.
        let i420 = Arc::new(vec![0u8; 640 * 480 * 3 / 2]);
        let subscriber_count = pool.push_i420_frame(i420, Instant::now());
        assert!(
            subscriber_count >= 1,
            "pool.push_i420_frame must deliver to ≥1 subscriber (the \
             always-on VP8 encoder); got {subscriber_count}. If this is \
             0, EncoderPool::new is not wiring up always-on encoders \
             to the i420 broadcast."
        );
    }

    // -----------------------------------------------------------------------
    // Phase 3c.3b.3a: pool-mode bridge-startup regression
    // -----------------------------------------------------------------------

    /// **3c.3b.3 review finding regression test.**
    ///
    /// The 3c.3b.3 wire-in initially returned from `handle_offer`
    /// straight into `handle_offer_pool_mode` BEFORE
    /// `start_encoder_pipeline` could run. The bridge that calls
    /// `pool.push_i420_frame` only spawns inside that pipeline, so a
    /// pool-mode-only deployment connected its WebRTC peer
    /// successfully but the always-on encoder never received I420
    /// frames → black stream.
    ///
    /// `ensure_legacy_bridge_running_for_pool` is the shim that
    /// closes the hole. This test pins the contract: a single call
    /// on a freshly-`start()`-ed session triggers the legacy
    /// pipeline (which includes the bridge, which is the only
    /// caller of `pool.push_i420_frame`).
    #[tokio::test]
    async fn ensure_legacy_bridge_running_starts_pipeline_on_first_call() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");

        // Pre-condition: legacy pipeline not yet started; the bridge
        // that feeds the pool's always-on encoder is therefore not
        // running. (start() initializes the pool but does NOT spawn
        // the bridge — the bridge lives inside start_encoder_pipeline.)
        assert!(
            !*session.encoder_init_lock.lock().await,
            "encoder_init_lock starts false (no peer/bridge yet)"
        );

        session.ensure_legacy_bridge_running_for_pool().await;

        assert!(
            *session.encoder_init_lock.lock().await,
            "shim must set encoder_init_lock=true so subsequent calls \
             from either path skip a duplicate pipeline start"
        );
        assert_eq!(
            *session.codec_mime.read().await,
            encode::MIME_TYPE_VP8,
            "shim must lock the legacy session codec to VP8 — matching \
             the always-on codec the pool spawned in start() — so a \
             mixed-mode legacy peer arriving later finds VP8 and the \
             validation path in handle_offer succeeds"
        );
    }

    /// Calling the shim a second time (e.g. by a second pool-mode
    /// offer, or by both pool and legacy offers in the same session)
    /// is a no-op: the lock guard returns early. The bridge is not
    /// re-spawned; codec_mime stays VP8.
    #[tokio::test]
    async fn ensure_legacy_bridge_running_is_idempotent() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");

        session.ensure_legacy_bridge_running_for_pool().await;
        session.ensure_legacy_bridge_running_for_pool().await;
        session.ensure_legacy_bridge_running_for_pool().await;

        assert!(
            *session.encoder_init_lock.lock().await,
            "after multiple calls the lock stays true"
        );
        assert_eq!(
            *session.codec_mime.read().await,
            encode::MIME_TYPE_VP8,
            "codec_mime must NOT change on repeat calls — that would \
             break the encoder_init_lock validation contract for any \
             legacy peer that has already negotiated against VP8"
        );
    }
}
