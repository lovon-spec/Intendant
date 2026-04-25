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
    /// Pool-path counterpart to [`Self::keyframe_tx`]. Wakes the
    /// pool-feed bridge to open a peer-join burst window when a new
    /// pool peer attaches. Distinct from `keyframe_tx` because the
    /// pool-feed bridge has its own select loop and own state. `Some`
    /// after [`Self::ensure_pool_feed_bridge_started`] has run.
    ///
    /// **Why a burst is needed even though the pool also sets
    /// `force_keyframe`:** [`crate::display::encode::pool::EncoderPool::request_keyframe_all`]
    /// sets a per-encoder atomic flag that VP8 and macOS H.264 honor
    /// on their next encode. Linux H.264 (ffmpeg-pipe) explicitly
    /// ignores the flag (see `h264_linux.rs::encode`'s `_force_keyframe`
    /// underscore) — there's no per-frame "emit IDR now" path on a
    /// long-running rawvideo pipe. Compensation is the same as the
    /// legacy bridge: clock the encoder at tick rate for ~one GOP
    /// boundary so its `-g 30` natural cadence emits a keyframe
    /// inside the burst window. Without this, an idle-desktop pool
    /// peer-join on Linux H.264 stays black for many seconds.
    pool_feed_keyframe_tx: Mutex<Option<mpsc::UnboundedSender<()>>>,
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
    /// Handle for the pool-only capture-to-I420 bridge task, or
    /// `None` when no pool peer has connected yet (and never set in
    /// legacy-only sessions — there the legacy bridge dual-feeds the
    /// pool itself).
    ///
    /// Spawned by [`Self::ensure_pool_feed_bridge_started`] on the
    /// first pool-mode offer. Owns its own BGRA→I420 conversion and
    /// `pool.push_i420_frame` loop, so it doesn't touch
    /// `encoder_init_lock` / `codec_mime` / the legacy encoder
    /// pipeline. That's the 3c.3b.3a fix for the codec-lock-in
    /// regression: a pool first-offer no longer locks the session
    /// codec to VP8 and reject a later legacy H.264-only peer.
    ///
    /// Coordination with the legacy `start_encoder_pipeline`'s
    /// dual-feed: both paths take `encoder_init_lock` (legacy as part
    /// of its first-peer dance, pool inside `ensure_pool_feed_bridge_started`)
    /// before consulting this handle, so the "pool gets fed by exactly
    /// one bridge" invariant holds across concurrent first-offers.
    /// Whichever path takes the init lock first wins the right to
    /// own the pool feed; the loser observes the winner's state and
    /// skips its own attempt.
    ///
    /// 3c.4 deletes the legacy pipeline; the pool-feed bridge becomes
    /// the only bridge and gets spawned unconditionally from `start()`.
    /// At that point this field becomes vestigial and goes away
    /// alongside the legacy code.
    pool_feed_bridge_handle: Mutex<Option<JoinHandle<()>>>,
}

/// Parse the truthiness of an `INTENDANT_DISPLAY_POOL` env value
/// (or any equivalent on/off flag).
///
/// **3c.4a flipped the default.** Pool is now the default path; the
/// flag is interpreted as opt-OUT, narrow. Only `Some("0")` and
/// `Some("false")` (case-insensitive) explicitly disable pool mode
/// and route through the legacy single-encoder fan-out. `None`,
/// empty string, and any other value (including `"1"`, `"true"`,
/// garbage) all enable pool mode.
///
/// The narrow opt-out matches the previous narrow opt-in: there are
/// only two documented "pool off" values, mirroring the previous
/// two "pool on" values. Operators who set `INTENDANT_DISPLAY_POOL=1`
/// for the pre-flip rollout continue to get pool mode (no behavior
/// change for them), and the flag remains an emergency rollback path
/// until 3c.5 deletes both the env flag and the legacy code.
///
/// Extracted from [`pool_mode_enabled`] so the parsing rules can be
/// unit-tested without touching `std::env::var` (env mutation in
/// parallel tests is racy across the cargo-test process).
fn parse_pool_flag(value: Option<&str>) -> bool {
    match value {
        Some(v) if v == "0" || v.eq_ignore_ascii_case("false") => false,
        _ => true,
    }
}

/// Whether the encoder-pool path
/// (`DisplaySession::handle_offer_pool_mode`) is active. **Default
/// ON** as of 3c.4a; opt out with `INTENDANT_DISPLAY_POOL=0` (or
/// `=false`, case-insensitive) for emergency rollback to the legacy
/// single-encoder fan-out. 3c.5 removes the env flag and deletes
/// the legacy code entirely.
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
            pool_feed_keyframe_tx: Mutex::new(None),
            pool: std::sync::OnceLock::new(),
            pool_feed_bridge_handle: Mutex::new(None),
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
                // 3c.3b.4h: pool encoders feed the same metrics
                // counters as the legacy bridge so DisplayMetricsSnapshot
                // continues to reflect total throughput. After 3c.4
                // deletes the legacy bridge, pool is the sole producer.
                Some(Arc::clone(&self.counters)),
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
        // 3c.3b.3c-followup: the pool-feed bridge observes
        // `shutdown.cancel()` above and exits on its own, but for
        // deterministic shutdown ordering (and parity with the
        // other tasks DisplaySession owns) we take the handle and
        // await its completion here too.
        if let Some(h) = self.pool_feed_bridge_handle.lock().await.take() {
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
        // **Phase 3c.3b.3a coordination:** if `ensure_pool_feed_bridge_started`
        // already spawned a pool-only bridge for an earlier pool-mode
        // offer, that bridge owns the pool feed; the legacy bridge
        // here MUST NOT also push to the pool, or each captured frame
        // would be I420-converted and pushed twice → duplicate frames
        // on the pool's broadcast → encoded duplicates on every pool
        // peer's RTP stream → corrupted decode (same shape as the
        // 3c.3b.2a multi-sub fan-out bug). The Option short-circuits
        // both `pool.push_i420_frame` and `pool.on_resize` calls below
        // — pool-feed bridge handles both for the pool side when it
        // owns the feed. The check is one mutex acquisition before
        // the bridge task spawn (not in the hot per-frame path).
        let pool_for_bridge: Option<Arc<encode::pool::EncoderPool>> = {
            let pool_feed_running = self
                .pool_feed_bridge_handle
                .lock()
                .await
                .is_some();
            if pool_feed_running {
                None
            } else {
                self.pool.get().map(Arc::clone)
            }
        };
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
    ///     bridge's keyframe channel. Peer-join keyframe is wired
    ///     in two parts at the tail of this function:
    ///     * [`crate::display::encode::pool::EncoderPool::request_keyframe_all`]
    ///       (3c.3b.4a): fires a coalesced PLI-equivalent across
    ///       every active encoder. Honored by VP8 and macOS H.264.
    ///     * Burst signal via [`Self::signal_peer_join_burst`]
    ///       (3c.3b.4b/.4c): wakes whichever bridge owns the pool
    ///       feed (pool-feed bridge in pool-only sessions, legacy
    ///       bridge in mixed sessions where a legacy peer attached
    ///       first) to clock the encoder at tick rate for ~1.5s.
    ///       Required for codecs that ignore `force_keyframe` on a
    ///       long-running pipe (Linux ffmpeg H.264) — without the
    ///       burst, an idle-desktop pool peer-join on Linux H.264
    ///       stays black for many seconds waiting on the heartbeat-
    ///       paced 1 push/sec cadence.
    ///     The PLI-driven per-peer explicit request from str0m's
    ///     inbound RTCP lands with the simulcast work.
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

        // Bridge spawn deferred until AFTER prefs validation +
        // pool.subscribe + new_pool_mode all succeed, per the 3c.3b.3a
        // review's low-priority finding: an invalid pool offer (no
        // overlapping codecs, encoder backend exhausted, etc.) MUST
        // NOT leave a bridge running with no peer ever attached. By
        // gating on success here, every spawned bridge has at least
        // one peer about to be inserted into the registry.
        self.ensure_pool_feed_bridge_started().await;

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

        // 3c.3b.4a: force a keyframe on every active pool encoder so
        // the new peer's first encoded frame is a decodable I-frame
        // rather than a P-frame referencing a keyframe the peer never
        // received. Coalesced per (codec, rid) — N peers joining in
        // the same beat produce one keyframe per encoder, not N
        // (mediasoup PLI-storm guard). Mirrors the legacy
        // `keyframe_tx.send(())` at the tail of `handle_offer`.
        // Placed AFTER all peer setup so the peer's pool subscription
        // is in place when the keyframe lands in the encoder broadcast.
        pool.request_keyframe_all();

        // 3c.3b.4c: open the peer-join burst window via whichever
        // bridge currently owns the pool feed. In a pool-only session
        // that's the pool-feed bridge (keyed on `pool_feed_keyframe_tx`,
        // installed by `ensure_pool_feed_bridge_started` above); in a
        // mixed session where a legacy peer attached first, the legacy
        // bridge (keyed on `keyframe_tx`) owns the feed and
        // `ensure_pool_feed_bridge_started` early-returned without
        // installing the pool-feed sender. See
        // [`Self::signal_peer_join_burst`] for the dispatch contract.
        self.signal_peer_join_burst().await;

        Ok(answer_sdp)
    }

    /// Open the peer-join burst window on the bridge currently
    /// **feeding the pool**.
    ///
    /// **Pool feed ownership invariant.** The pool feed has exactly
    /// one owner at any time, established by the 3c.3b.3b
    /// coordination: `encoder_init_lock` serializes the decision,
    /// and whichever bridge wins the lock first owns pool feed
    /// ownership for the session lifetime. Specifically:
    ///   - Pool-first session: `ensure_pool_feed_bridge_started`
    ///     installs `pool_feed_keyframe_tx` and spawns the pool-feed
    ///     bridge. If a legacy peer arrives LATER,
    ///     `start_encoder_pipeline` runs with `pool_for_bridge=None` —
    ///     its `keyframe_tx` is installed but its bridge does NOT
    ///     feed the pool. Both channels are `Some` in this state.
    ///   - Legacy-first session: `start_encoder_pipeline` installs
    ///     `keyframe_tx` and the legacy bridge feeds the pool via
    ///     `pool_for_bridge=Some`. `ensure_pool_feed_bridge_started`
    ///     early-returns when `*init == true`, so
    ///     `pool_feed_keyframe_tx` stays `None`.
    /// In every state, `pool_feed_keyframe_tx.is_some()` is the
    /// single source of truth for "pool-feed bridge owns the pool
    /// feed" — see [`Self::pool_feed_keyframe_tx`] field doc and
    /// [`Self::ensure_pool_feed_bridge_started`].
    ///
    /// **Dispatch.** Try the pool-feed bridge first; fall back to
    /// the legacy bridge only if pool-feed isn't installed. Exactly
    /// one channel is signaled per call. Sending to BOTH (the
    /// 3c.3b.4c-pre version) was correct but over-woke the legacy
    /// bridge in pool-first → legacy-later sessions where the legacy
    /// bridge runs but doesn't feed the pool — wasted work and an
    /// obscured invariant.
    ///
    /// **Why this exists.** 3c.3b.4b only signaled
    /// `pool_feed_keyframe_tx`, which leaves legacy-first mixed
    /// sessions black: pool-feed bridge never starts →
    /// `pool_feed_keyframe_tx` stays `None` → 4b's signal no-ops →
    /// later pool peer's Linux H.264 forwarder waits seconds on the
    /// 1 push/sec heartbeat cadence (the ffmpeg-pipe encoder
    /// ignores `force_keyframe`).
    async fn signal_peer_join_burst(&self) {
        // Pool-feed bridge owns the pool feed iff its keyframe
        // channel is installed. Try it first.
        if let Some(tx) = self.pool_feed_keyframe_tx.lock().await.as_ref() {
            let _ = tx.send(());
            return;
        }
        // Pool-feed bridge isn't installed → legacy bridge owns the
        // pool feed via `pool_for_bridge=Some`. Wake its burst.
        if let Some(tx) = self.keyframe_tx.lock().await.as_ref() {
            let _ = tx.send(());
        }
    }

    /// Idempotently ensure the **pool-only** capture-to-I420 bridge is
    /// running. This bridge's `pool.push_i420_frame` call is what
    /// keeps the pool's always-on (and on-demand) encoders fed in
    /// pool-mode-only sessions. Without it, a pool peer subscribes
    /// successfully but the encoder never produces output → black
    /// stream (the 3c.3b.3 high-priority finding).
    ///
    /// Distinct from the legacy bridge inside [`Self::start_encoder_pipeline`]
    /// in three deliberate ways:
    ///   1. **Doesn't touch `encoder_init_lock` or `codec_mime`** —
    ///      so a pool first-offer no longer locks the session codec
    ///      to VP8 and rejects a later legacy H.264-only peer
    ///      (3c.3b.3a finding 1).
    ///   2. **Doesn't spawn a legacy encoder thread or fan-out task**
    ///      — so a pool-only deployment doesn't pay the CPU cost of
    ///      a legacy VP8 encoder running with no consumers.
    ///   3. **Pool-private peer-join burst channel.** As of 3c.3b.4b
    ///      the bridge listens on `pool_feed_keyframe_tx` and opens
    ///      a 1.5s burst window when signaled (mirrors legacy
    ///      `keyframe_tx`). Required because `force_keyframe` on a
    ///      long-running ffmpeg-pipe encoder (Linux H.264) is a
    ///      no-op, so `EncoderPool::request_keyframe_all` alone
    ///      can't reach those encoders — the burst clocks the
    ///      encoder at tick rate so its `-g 30` natural cadence
    ///      lands a keyframe inside the window.
    ///
    /// What it DOES match from the legacy bridge (3c.3b.3b
    /// follow-up): the **tick + heartbeat** pattern. The bridge
    /// caches the latest I420 buffer and forwards it on every tick
    /// when the buffer changed OR the idle heartbeat is due. Without
    /// this, a damage-driven capture backend (Wayland especially)
    /// that emits nothing while the desktop is idle would leave the
    /// pool's encoder starved of input — a peer joining mid-idle
    /// would see a black stream until the next desktop damage event,
    /// regressing the existing legacy behavior. The heartbeat
    /// re-pushes the latest frame once per second so the encoder's
    /// GOP cadence keeps producing decodable references.
    ///
    /// What it ALSO matches: the legacy bridge's `AppEvent::DisplayResize`
    /// emission on dimension change. Without this, presence / MCP /
    /// outbound listeners would never learn about display size
    /// changes in pool-only sessions (the legacy bridge would either
    /// be absent or have `pool_for_bridge=None` and skip its own
    /// emission too).
    ///
    /// Coordination with the legacy bridge: both paths take
    /// `encoder_init_lock` BEFORE consulting `pool_feed_bridge_handle`.
    /// Whichever first-offer path acquires the init lock first wins
    /// the right to feed the pool:
    ///   - If legacy ran first: `init=true` here → skip (legacy bridge
    ///     dual-feeds the pool via the existing 3c.2 path).
    ///   - If pool ran first: `init=false` here → spawn the pool-feed
    ///     bridge. A subsequent legacy offer will see
    ///     `pool_feed_bridge_handle.is_some()` in
    ///     `start_encoder_pipeline` and skip its own dual-feed.
    /// Either way the pool's broadcast gets exactly one feed, and
    /// no double-encoding bug.
    async fn ensure_pool_feed_bridge_started(&self) {
        // Order: encoder_init_lock first, then pool_feed_bridge_handle.
        // Same nesting in `start_encoder_pipeline`'s consultation, so
        // concurrent first-offers can't deadlock and can't both end up
        // owning the pool feed.
        let init = self.encoder_init_lock.lock().await;
        let mut handle = self.pool_feed_bridge_handle.lock().await;
        if handle.is_some() {
            // Already running from a previous pool offer.
            return;
        }
        if *init {
            // Legacy bridge is running and dual-feeds the pool. We'd
            // double-feed if we also spawned. Skip.
            return;
        }
        let Some(pool) = self.pool.get().map(Arc::clone) else {
            // Pool not initialized — caller must invoke `start()` first.
            // `handle_offer_pool_mode` already errors if the pool is
            // missing; if we somehow reach here without it, silently
            // skip rather than panic. Diagnostic eprintln so the bug
            // is visible in logs without crashing the session.
            eprintln!(
                "[display/pool-feed] pool not initialized — \
                 ensure_pool_feed_bridge_started is a no-op; \
                 DisplaySession::start() must run before any offer"
            );
            return;
        };
        let mut broadcast_rx = self.frame_tx.subscribe();
        let (initial_w, initial_h) = self.backend.resolution();
        let shutdown = self.shutdown.clone();
        let display_id = self.display_id;
        let event_bus = self.encoder_event_bus.lock().await.clone();
        let fps = *self.fps.lock().await;
        let frame_interval = std::time::Duration::from_millis(
            if fps > 0 { 1000 / fps as u64 } else { 33 },
        );
        // Snapshot the most recent BGRA the capture has already
        // produced so the bridge can seed its `latest_i420` cache
        // before entering the select loop. Without this, a pool
        // peer arriving AFTER the capture has produced one frame
        // and gone idle (typical Wayland damage-driven behavior:
        // initial rendering then no events) leaves the bridge with
        // `latest_i420 = None` and the heartbeat has nothing to
        // re-push — the encoder starves until the next desktop
        // damage. The 3c.3b.3c heartbeat test pushed BGRA AFTER
        // bridge spawn, which doesn't exercise this idle-on-arrival
        // path. Cloning the Arc<RwLock> outside the spawned task
        // so the closure owns its handle.
        let latest_frame = Arc::clone(&self.latest_frame);
        // 3c.3b.4b: peer-join keyframe burst channel. `handle_offer_pool_mode`
        // sends `()` after every successful peer attach; the bridge opens a
        // ~1.5s burst window during which every tick forwards latest_i420
        // regardless of dirty state. Required for codecs that ignore
        // `force_keyframe` on a long-running pipe (Linux ffmpeg H.264) —
        // pool.request_keyframe_all alone won't reach those encoders, and
        // the heartbeat-only path (1 push/sec) means an idle-desktop
        // peer-join can wait many seconds for a natural GOP boundary.
        // Mirrors the legacy bridge's `kf_rx`/burst at mod.rs:984-998.
        let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
        *self.pool_feed_keyframe_tx.lock().await = Some(kf_tx);
        let task = tokio::spawn(async move {
            // Heartbeat cadence. Mirrors the legacy bridge's value;
            // 1 second strikes the balance between "encoder stays
            // healthy on idle" and "encoder doesn't burn CPU on
            // identical-frame re-encodes." Smaller would re-encode
            // more often; larger would let GOP boundaries drift past
            // a peer-join window on truly static desktops.
            const IDLE_HEARTBEAT: std::time::Duration =
                std::time::Duration::from_secs(1);
            // Peer-join burst window. Sized to comfortably exceed
            // one Linux ffmpeg H.264 GOP at 30fps (`-g 30` → ~1s) so
            // the natural keyframe lands inside the window even
            // though `force_keyframe` is ignored on the rawvideo pipe.
            // Same constant the legacy bridge uses (mod.rs:984).
            const PEER_JOIN_BURST: std::time::Duration =
                std::time::Duration::from_millis(1500);

            let mut enc_w = initial_w & !1;
            let mut enc_h = initial_h & !1;
            // `Arc` so `tick.tick()` re-pushes can clone cheaply
            // instead of the `Vec<u8>` clone the simpler
            // pre-3c.3b.3b-followup version did.
            let mut latest_i420: Option<(Arc<Vec<u8>>, Instant)> = None;
            // `generation` bumps on every replacement; `last_sent_gen`
            // is what we last forwarded. Mismatch = "buffer changed
            // since last send" (i.e. real damage on a damage-driven
            // backend, or any frame on a poll backend).
            let mut generation: u64 = 0;
            let mut last_sent_gen: Option<u64> = None;
            let mut last_send_at = Instant::now();
            // 3c.3b.4b: peer-join burst window. `None` outside a
            // burst; `Some(deadline)` while clocking the encoder
            // through to a natural keyframe regardless of dirty
            // state. Opened by `kf_rx.recv()` (signaled by
            // `handle_offer_pool_mode`'s tail), expires after
            // `PEER_JOIN_BURST` past the deadline.
            let mut burst_until: Option<Instant> = None;

            // 3c.3b.3c-followup seed: convert the most recent BGRA
            // the capture has already produced (if any) so the first
            // tick has something to push even if no new BGRA arrives
            // before the heartbeat. Closes the "first pool peer
            // arrives after the capture went idle" black-screen path.
            // Bumps `generation`; the very first tick sees
            // `last_sent_gen=None != Some(1)` → forwards immediately,
            // not waiting for IDLE_HEARTBEAT.
            //
            // enc_w/enc_h are also seeded from the snapshotted
            // frame's actual dimensions. Without that, the first
            // BGRA arriving on the broadcast at a different size
            // than `backend.resolution()` would emit a spurious
            // resize event ("64x64 -> actual" when the frame was
            // never at 64x64 to begin with).
            if let Some(frame) = latest_frame.read().await.clone() {
                let frame_w = frame.width & !1;
                let frame_h = frame.height & !1;
                if frame_w > 0 && frame_h > 0 {
                    enc_w = frame_w;
                    enc_h = frame_h;
                    // 3c.3b.3e: if the snapshotted frame's dims differ
                    // from the pool's (display resized between pool
                    // construction at `backend.resolution()` and the
                    // first pool offer, or `backend.resolution()`
                    // returned pre-resize dims), reshape the pool
                    // BEFORE the first tick pushes the seeded I420 at
                    // these dims. Without this, the pool's encoders
                    // stay configured at the original dimensions while
                    // receiving new-dim I420 — silent black-screen
                    // class. Mirrors the in-loop resize branch below
                    // (on_resize + DisplayResize event in one beat),
                    // emitting via `event_bus` so presence / MCP /
                    // outbound listeners learn about the seed-time
                    // dimension change.
                    if (frame_w, frame_h) != pool.dimensions() {
                        eprintln!(
                            "[display/pool-feed] display {} seed \
                             resolution {:?} -> {}x{}",
                            display_id, pool.dimensions(), frame_w, frame_h,
                        );
                        pool.on_resize(frame_w, frame_h);
                        if let Some(ref bus) = event_bus {
                            bus.send(crate::event::AppEvent::DisplayResize {
                                display_id,
                                width: frame_w,
                                height: frame_h,
                            });
                        }
                    }
                    let frame_arc = Arc::clone(&frame);
                    let arrived = Instant::now();
                    let i420_result = tokio::task::spawn_blocking(move || {
                        encode::bgra_to_i420(
                            &frame_arc.data,
                            frame_arc.width,
                            frame_arc.height,
                            frame_arc.stride,
                        )
                    })
                    .await;
                    if let Ok(i420) = i420_result {
                        generation = generation.wrapping_add(1);
                        latest_i420 = Some((Arc::new(i420), arrived));
                    }
                }
            }

            let mut tick = tokio::time::interval(frame_interval);
            tick.set_missed_tick_behavior(
                tokio::time::MissedTickBehavior::Skip,
            );

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    maybe_kf = kf_rx.recv() => {
                        // `Some(())` = peer-join signal. `None` =
                        // sender dropped (DisplaySession dropped); the
                        // shutdown.cancelled() arm will fire on the
                        // next select pass and break the loop. Either
                        // way, opening (or refreshing) the burst
                        // window is the right action only on Some.
                        if maybe_kf.is_some() {
                            burst_until =
                                Some(Instant::now() + PEER_JOIN_BURST);
                        }
                    }
                    result = broadcast_rx.recv() => {
                        let frame = match result {
                            Ok(f) => f,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        };

                        let frame_w = frame.width & !1;
                        let frame_h = frame.height & !1;
                        if frame_w == 0 || frame_h == 0 {
                            continue;
                        }

                        // Resize handling. The legacy bridge calls
                        // `pool.on_resize` AND emits
                        // `AppEvent::DisplayResize` from its dual-feed
                        // path (gated on `pool_for_bridge.is_some()`),
                        // which is None when this pool-feed bridge
                        // runs — so we own BOTH for the pool side
                        // when we're the active feed. Without the
                        // event emit here, presence / MCP / outbound
                        // listeners wouldn't learn about display size
                        // changes in a pool-first session.
                        if frame_w != enc_w || frame_h != enc_h {
                            eprintln!(
                                "[display/pool-feed] display {} resolution \
                                 {}x{} -> {}x{}",
                                display_id, enc_w, enc_h, frame_w, frame_h,
                            );
                            pool.on_resize(frame_w, frame_h);
                            if let Some(ref bus) = event_bus {
                                bus.send(crate::event::AppEvent::DisplayResize {
                                    display_id,
                                    width: frame_w,
                                    height: frame_h,
                                });
                            }
                            enc_w = frame_w;
                            enc_h = frame_h;
                            // Drop stale buffer: it's at the old
                            // dimensions; pool's encoders have already
                            // been respawned by `pool.on_resize` and
                            // would either reject or mis-encode an
                            // old-dim frame.
                            latest_i420 = None;
                            last_sent_gen = None;
                        }

                        let frame_arc = Arc::clone(&frame);
                        let arrived = Instant::now();
                        let i420_result = tokio::task::spawn_blocking(move || {
                            encode::bgra_to_i420(
                                &frame_arc.data,
                                frame_arc.width,
                                frame_arc.height,
                                frame_arc.stride,
                            )
                        })
                        .await;
                        if let Ok(i420) = i420_result {
                            generation = generation.wrapping_add(1);
                            latest_i420 = Some((Arc::new(i420), arrived));
                        }
                    }
                    _ = tick.tick() => {
                        // Forward latest_i420 if anything's worth
                        // forwarding. "Worth forwarding" = a fresher
                        // buffer than last sent, OR the heartbeat is
                        // due (kicks the encoder so a peer joining a
                        // damage-idle desktop sees output within ~one
                        // GOP rather than waiting for the next desktop
                        // event), OR a peer-join burst is active (3c.3b.4b
                        // — clocks the encoder at tick rate so codecs
                        // that ignore `force_keyframe` on a long-running
                        // pipe (Linux ffmpeg H.264) hit a natural
                        // keyframe inside the burst window rather than
                        // waiting many seconds on heartbeat-only).
                        let Some((ref i420, arrived)) = latest_i420 else {
                            continue;
                        };

                        let changed = last_sent_gen != Some(generation);
                        let heartbeat_due =
                            last_send_at.elapsed() >= IDLE_HEARTBEAT;
                        let in_burst = burst_until
                            .map_or(false, |u| Instant::now() < u);
                        if !(changed || heartbeat_due || in_burst) {
                            continue;
                        }

                        pool.push_i420_frame(Arc::clone(i420), arrived);
                        last_sent_gen = Some(generation);
                        last_send_at = Instant::now();
                    }
                }
            }
        });
        *handle = Some(task);
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
/// - the new encoder spawns cleanly.
///
/// **3c.3b.4g:** the previous version ALSO early-returned when
/// `is_vaapi_banned()` was already true, on the assumption that the
/// current encoder must already be libx264. That assumption fails
/// for encoders constructed BEFORE a sibling watchdog set the ban
/// (multi-H.264-pool-slot and mixed pool/legacy sessions both reach
/// this state): the second watchdog would see the ban, return None,
/// and leave a pre-ban VAAPI encoder stranded on the broken path
/// forever. Fix: drop the `is_vaapi_banned` early-return, treat
/// `ban_vaapi()` as the idempotent no-op it is, always attempt
/// construction. At worst an already-libx264 encoder respawns
/// libx264 once (a one-time waste; the watchdog latches and won't
/// fire again); at best a pre-ban VAAPI encoder gets the libx264
/// it would otherwise miss. Mirrors the same fix in
/// [`crate::display::encode::pool::try_h264_fallback_for_layer`] —
/// the two helpers will collapse to one when 3c.4 deletes this
/// legacy path.
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
    // Idempotent — see VAAPI_BANNED in encode/h264_linux.rs (one-way
    // AtomicBool that's never cleared). Calling when already banned
    // is a no-op store.
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

    /// `1` → enabled. The canonical "set" value, kept compatible
    /// with the pre-3c.4a `INTENDANT_DISPLAY_POOL=1` recipe so
    /// operators who set it during the rollout continue to get pool
    /// mode (no behavior change).
    #[test]
    fn parse_pool_flag_one_is_true() {
        assert!(parse_pool_flag(Some("1")));
    }

    /// `true`, `True`, `TRUE`, `tRuE` → enabled. Same back-compat
    /// rationale as `parse_pool_flag_one_is_true`.
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

    /// **3c.4a flip.** Unset → enabled (was: disabled). Pool is
    /// now the default path; operators don't have to set anything
    /// to opt in.
    #[test]
    fn parse_pool_flag_none_is_true_after_flip() {
        assert!(
            parse_pool_flag(None),
            "post-3c.4a default is pool ON when INTENDANT_DISPLAY_POOL is unset",
        );
    }

    /// **3c.4a flip.** Only `0` and `false` (case-insensitive)
    /// explicitly opt OUT to legacy. Narrow opt-out mirroring the
    /// previous narrow opt-in (`1` / `true`). Empty string, `yes`,
    /// `no`, garbage all stay enabled — the operator either
    /// affirmatively typed an opt-out value or they get the default.
    #[test]
    fn parse_pool_flag_zero_and_false_disable() {
        for v in ["0", "false", "False", "FALSE", "fAlSe"] {
            assert!(
                !parse_pool_flag(Some(v)),
                "value {:?} must DISABLE pool mode (legacy opt-out)",
                v
            );
        }
    }

    /// **3c.4a flip.** Anything that isn't a documented opt-out
    /// keeps pool ON. Includes obvious "off-ish" garbage like
    /// "no" and "off" that we deliberately do NOT recognize as
    /// opt-out — only `0` / `false` qualify (mirrors the pre-flip
    /// narrowness around `1` / `true` for opt-in).
    #[test]
    fn parse_pool_flag_other_values_stay_enabled_after_flip() {
        for v in ["", "yes", "on", " 0", "0 ", "no", "off", "garbage"] {
            assert!(
                parse_pool_flag(Some(v)),
                "value {:?} must enable pool mode (only `0` / `false` opt out)",
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
    // Phase 3c.3b.3a: pool-feed bridge — does NOT lock legacy codec
    // -----------------------------------------------------------------------

    /// **3c.3b.3a finding 1 regression test.** The first iteration of
    /// the pool-mode bridge-startup hook piggybacked on
    /// `start_encoder_pipeline(VP8)` and set `encoder_init_lock=true`
    /// + `codec_mime=VP8` as a side effect. That weakened the
    /// coexistence story: a pool first-offer would lock the session
    /// to VP8 and reject a later legacy H.264-only peer that the
    /// original first-peer codec negotiation would have accepted.
    ///
    /// The fix decouples "bridge needs to run for pool feed" from
    /// "legacy codec is decided." The pool-feed bridge is a separate
    /// task that does BGRA→I420→`pool.push_i420_frame` without
    /// touching `encoder_init_lock` or `codec_mime`. This test pins
    /// the contract.
    #[tokio::test]
    async fn ensure_pool_feed_bridge_started_does_not_lock_legacy_codec() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");

        // Pre-condition baseline: legacy pipeline never started.
        assert!(!*session.encoder_init_lock.lock().await);
        let codec_before = *session.codec_mime.read().await;

        session.ensure_pool_feed_bridge_started().await;

        // Post-condition: bridge handle is set (pool feed is running)
        // BUT the legacy codec/init state is untouched. A legacy
        // H.264-only peer arriving next runs through handle_offer's
        // first-peer codec-negotiation path normally.
        assert!(
            session.pool_feed_bridge_handle.lock().await.is_some(),
            "pool feed bridge must be spawned"
        );
        assert!(
            !*session.encoder_init_lock.lock().await,
            "encoder_init_lock MUST stay false — pool path does not \
             decide a session codec, only ensures the pool feed is \
             running. A later legacy peer must be free to negotiate \
             its own codec via select_codec."
        );
        assert_eq!(
            *session.codec_mime.read().await,
            codec_before,
            "codec_mime MUST stay at its default — pool path does not \
             touch the legacy session codec. Changing it would break \
             a later legacy peer's codec validation."
        );

        // Cleanup: cancel shutdown so the bridge task exits.
        session.shutdown.cancel();
    }

    /// Calling the bridge starter a second time is a no-op: the
    /// handle guard returns early. The first task keeps running; we
    /// don't spawn duplicates.
    #[tokio::test]
    async fn ensure_pool_feed_bridge_started_is_idempotent() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");

        session.ensure_pool_feed_bridge_started().await;
        // Capture the handle's task id (via Debug repr) so a
        // second-spawn regression would surface as a different id.
        let first_handle_dbg = format!(
            "{:?}",
            session
                .pool_feed_bridge_handle
                .lock()
                .await
                .as_ref()
                .map(|h| h.id())
        );

        session.ensure_pool_feed_bridge_started().await;
        session.ensure_pool_feed_bridge_started().await;

        let after_handle_dbg = format!(
            "{:?}",
            session
                .pool_feed_bridge_handle
                .lock()
                .await
                .as_ref()
                .map(|h| h.id())
        );
        assert_eq!(
            first_handle_dbg, after_handle_dbg,
            "subsequent calls must NOT replace the existing bridge task"
        );

        session.shutdown.cancel();
    }

    /// When the legacy bridge is already running (legacy first-peer
    /// arrived before any pool peer), the pool-feed bridge MUST NOT
    /// spawn — the legacy bridge's existing 3c.2 dual-feed already
    /// covers the pool. Spawning a second bridge would double-feed
    /// the pool's I420 broadcast → duplicate encoded frames on every
    /// pool peer → corrupted decode (3c.3b.2a multi-sub fan-out
    /// shape).
    ///
    /// We simulate "legacy bridge already running" by setting
    /// `encoder_init_lock=true` directly. The `start_encoder_pipeline`
    /// call sequence isn't necessary for this contract — the gate
    /// the test exercises is purely the init-lock check inside
    /// `ensure_pool_feed_bridge_started`.
    #[tokio::test]
    async fn ensure_pool_feed_bridge_started_skips_when_legacy_running() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");

        // Pretend the legacy first-peer path has already run.
        *session.encoder_init_lock.lock().await = true;

        session.ensure_pool_feed_bridge_started().await;

        assert!(
            session.pool_feed_bridge_handle.lock().await.is_none(),
            "with encoder_init_lock=true, the pool-feed bridge must \
             NOT spawn — legacy bridge owns the pool feed via dual-feed"
        );

        session.shutdown.cancel();
    }

    /// Build a synthetic BGRA frame for tests. Contents are
    /// uninteresting (all zeros) — we only care that the bridge
    /// converts and pushes them.
    fn make_test_bgra(width: u32, height: u32) -> Frame {
        Frame {
            data: vec![0u8; (width * height * 4) as usize],
            format: FrameFormat::Bgra,
            width,
            height,
            stride: width * 4,
            timestamp: Instant::now(),
        }
    }

    /// **3c.3b.3b finding 2 regression test.** Pool-only sessions
    /// must emit `AppEvent::DisplayResize` on dimension changes.
    /// Pre-fix the pool-feed bridge called `pool.on_resize` but
    /// dropped the event-bus emission entirely — presence / MCP /
    /// outbound listeners learned about resizes only when the
    /// legacy bridge was running.
    #[tokio::test]
    async fn pool_feed_bridge_emits_display_resize_on_dimension_change() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(7, backend);
        let bus = crate::event::EventBus::new();
        let mut bus_rx = bus.subscribe();
        session
            .start(30, None, Some(bus))
            .await
            .expect("start must succeed");
        session.ensure_pool_feed_bridge_started().await;

        // Push the FIRST frame at the initial size; this seeds the
        // bridge's enc_w/enc_h tracking but does NOT cross the
        // resize threshold (initial==initial).
        let _ = session
            .frame_tx
            .send(Arc::new(make_test_bgra(64, 64)));

        // Push a frame at a NEW size. This must trigger both
        // `pool.on_resize` (covered by the existing pool tests) AND
        // the AppEvent::DisplayResize emission (the regression
        // surface this test pins).
        let _ = session
            .frame_tx
            .send(Arc::new(make_test_bgra(128, 96)));

        // Drain events looking for our DisplayResize. Other events
        // (capture-side, etc.) might land first; bounded loop with
        // a generous timeout to give the spawn_blocking BGRA→I420
        // conversion + tick time to fire.
        let saw_resize = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match bus_rx.recv().await {
                        Ok(crate::event::AppEvent::DisplayResize {
                            display_id: 7,
                            width: 128,
                            height: 96,
                        }) => return true,
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            },
        )
        .await
        .unwrap_or(false);

        assert!(
            saw_resize,
            "pool-feed bridge MUST emit AppEvent::DisplayResize on \
             dimension change — without it, presence/MCP/outbound \
             listeners never learn about resolution changes in \
             pool-only sessions"
        );

        session.shutdown.cancel();
    }

    /// **3c.3b.3b finding 1 regression test.** On damage-driven
    /// capture backends (Wayland especially), no BGRA frames flow
    /// while the desktop is idle. The legacy bridge handled this by
    /// caching `latest_i420` and re-pushing it on the IDLE_HEARTBEAT
    /// cadence (~1 Hz) so the encoder kept producing output. The
    /// initial pool-feed bridge dropped that pattern: it pushed only
    /// when a fresh BGRA frame arrived. A pool peer joining mid-idle
    /// would see no encoded frames.
    ///
    /// This test pins the heartbeat: push exactly one BGRA, wait
    /// past the IDLE_HEARTBEAT window, count how many encoded
    /// frames arrive at a pool subscriber. Without heartbeat we'd
    /// see ≤1 (the initial encode); with heartbeat we see ≥2 (initial
    /// + at least one heartbeat re-push).
    #[tokio::test]
    async fn pool_feed_bridge_heartbeats_on_idle_capture() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start must succeed");
        session.ensure_pool_feed_bridge_started().await;

        // Subscribe to the pool's encoder before pushing the BGRA so
        // we don't miss the first frame.
        let pool = session
            .pool
            .get()
            .expect("pool initialized after start");
        let prefs = encode::pool::PeerCodecPreferences::new(vec![
            encode::pool::CodecKind::Vp8,
        ]);
        let (subs, _lease) = pool
            .subscribe(&prefs)
            .expect("VP8 always-on subscribe");
        let mut frame_rx = subs
            .into_iter()
            .next()
            .expect("at least one subscription")
            .frames;

        // Push EXACTLY ONE BGRA frame. After this, no more capture
        // activity — simulating a fully idle damage-driven backend.
        let _ = session
            .frame_tx
            .send(Arc::new(make_test_bgra(64, 64)));

        // Count encoded frames over a window > IDLE_HEARTBEAT (1s)
        // + buffer for VP8 encoder startup. The bridge ticks at
        // 33ms (fps=30); after the first send, ticks observe
        // unchanged buffer + heartbeat-not-due → skip. After
        // ~30 ticks (~1s), heartbeat is due → re-push → encoder
        // produces another frame.
        let mut count: u32 = 0;
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1800),
            async {
                while let Ok(frame) = frame_rx.recv().await {
                    let _ = frame;
                    count += 1;
                    if count >= 2 {
                        return;
                    }
                }
            },
        )
        .await;

        assert!(
            count >= 2,
            "heartbeat must re-push latest_i420 to the pool encoder; \
             got {count} frames in 1.8s. Pre-fix behavior would deliver \
             ≤1 (initial encode then encoder starves on idle BGRA \
             stream — the regression on damage-driven backends)."
        );

        session.shutdown.cancel();
    }

    /// **3c.3b.3c follow-up finding 1.** First-pool-peer-joins-mid-idle
    /// black screen: if the capture has already produced a frame
    /// into `latest_frame` before the first pool offer, then goes
    /// idle (typical Wayland damage-driven shape: initial render,
    /// no further events), the bridge spawned by
    /// `ensure_pool_feed_bridge_started` would historically miss
    /// that frame and the heartbeat would have nothing to re-push.
    /// The fix seeds the bridge's `latest_i420` from
    /// `self.latest_frame` at startup. This test pins it: write a
    /// BGRA into `latest_frame` BEFORE starting the bridge, never
    /// push anything via `frame_tx`, and assert encoded frames flow
    /// to a pool subscriber.
    #[tokio::test]
    async fn pool_feed_bridge_seeds_latest_i420_from_capture_snapshot() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start must succeed");

        // Pre-seed latest_frame as if the capture had produced one
        // frame and gone idle. With StubBackend's closed channel,
        // the capture task exits before populating latest_frame
        // organically — this write simulates the "capture was here,
        // then went silent" state that triggers the bug.
        *session.latest_frame.write().await =
            Some(Arc::new(make_test_bgra(64, 64)));

        // Subscribe to the pool's encoder BEFORE spawning the
        // bridge so we don't miss the first encoded output.
        let pool = session
            .pool
            .get()
            .expect("pool initialized after start");
        let prefs = encode::pool::PeerCodecPreferences::new(vec![
            encode::pool::CodecKind::Vp8,
        ]);
        let (subs, _lease) = pool
            .subscribe(&prefs)
            .expect("VP8 always-on subscribe");
        let mut frame_rx = subs
            .into_iter()
            .next()
            .expect("at least one subscription")
            .frames;

        // Spawn the bridge. Critically: do NOT push any BGRA via
        // `frame_tx` — we're testing the seed-from-snapshot path,
        // not the broadcast-recv path. Pre-fix the bridge would have
        // `latest_i420 = None` and the encoder would never see input.
        session.ensure_pool_feed_bridge_started().await;

        // Within one tick (~33ms at fps=30) the seeded buffer should
        // be forwarded. Generous timeout for VP8 encoder warmup
        // (cold-start can take a few hundred ms before the first
        // packet emerges). Pre-fix would never produce ANY frame.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            frame_rx.recv(),
        )
        .await;

        assert!(
            result.is_ok() && result.as_ref().unwrap().is_ok(),
            "bridge must seed latest_i420 from capture snapshot and \
             forward it on the first tick — got timeout or recv \
             error within 2s. Pre-fix: pool encoder never sees \
             input → no frames ever emitted."
        );

        session.shutdown.cancel();
    }

    /// **3c.3b.3d follow-up finding (3c.3b.3e).** Seed-path pool
    /// dimension desync. The 3c.3b.3d seed branch updates `enc_w`/
    /// `enc_h` from the snapshotted frame's actual dimensions but
    /// did NOT call `pool.on_resize` if the pool was constructed at
    /// a different size — display resized between pool construction
    /// (at `backend.resolution()`) and the first pool offer, OR
    /// `backend.resolution()` returned pre-resize dims, etc. The
    /// seed-then-tick sequence then pushed the new-dim I420 into a
    /// pool whose encoders were configured for the old dims. Pre-
    /// 3c.3b.3e: silent black-screen / encoder reject. The
    /// `pool_feed_bridge_seeds_latest_i420_from_capture_snapshot`
    /// regression test uses 64x64 backend AND 64x64 cached frame so
    /// it does NOT cover this mismatch — this test does. Asserts
    /// BOTH the pool resize AND the `DisplayResize` event parity
    /// (mirroring the in-loop resize branch's two-effect contract).
    #[tokio::test]
    async fn pool_feed_bridge_seed_resizes_pool_when_snapshot_dims_differ() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(11, backend);
        let bus = crate::event::EventBus::new();
        let mut bus_rx = bus.subscribe();
        session
            .start(30, None, Some(bus))
            .await
            .expect("start must succeed");

        // Pool is constructed inside `start()` at backend.resolution()
        // → 64x64 here. Pin the precondition so a future change to
        // the pool constructor's default dims fires this assert
        // instead of silently invalidating the test premise.
        let pool = session
            .pool
            .get()
            .expect("pool initialized after start");
        assert_eq!(
            pool.dimensions(),
            (64, 64),
            "pool dims must start at backend.resolution()",
        );

        // Pre-seed `latest_frame` at 128x96 — different from pool's
        // 64x64. Models the racy case: capture produced a frame at
        // the new size while the pool was already constructed at the
        // old size (display resized between pool construction and
        // first pool offer).
        *session.latest_frame.write().await =
            Some(Arc::new(make_test_bgra(128, 96)));

        // Spawn the pool-feed bridge. The seed branch must observe
        // the dimension mismatch and reshape the pool BEFORE the
        // first tick would push an old-dim-incompatible buffer.
        session.ensure_pool_feed_bridge_started().await;

        // Wait for the seed path to fire and reshape. Bounded — the
        // seed runs synchronously inside the spawned task before the
        // first `tick.tick()` await, but we need to give the task
        // time to be scheduled, grab the latest_frame read lock, and
        // complete the spawn_blocking BGRA→I420 conversion. Drain
        // events looking for our DisplayResize; other events
        // (capture-side, etc.) might land first.
        let saw_resize = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match bus_rx.recv().await {
                        Ok(crate::event::AppEvent::DisplayResize {
                            display_id: 11,
                            width: 128,
                            height: 96,
                        }) => return true,
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            },
        )
        .await
        .unwrap_or(false);

        assert!(
            saw_resize,
            "seed branch must emit AppEvent::DisplayResize when \
             snapshot dims differ from pool dims; got timeout. \
             Pre-3c.3b.3e: enc_w/enc_h get updated locally but pool \
             and listeners never learn — black-screen class bug.",
        );

        // Pool must have been reshaped. If pool.dimensions() still
        // reports (64, 64) here, encoders are at 64x64 while the
        // first tick pushed 128x96 I420 — mis-encode / reject.
        assert_eq!(
            pool.dimensions(),
            (128, 96),
            "seed branch must call pool.on_resize when snapshot \
             dims differ from pool dims; pool stayed at {:?}. \
             Pre-3c.3b.3e: pool encoders configured at old dims \
             receive new-dim I420 from the first tick — encoder \
             mis-encodes or rejects.",
            pool.dimensions(),
        );

        session.shutdown.cancel();
    }

    /// **3c.3b.4b regression test.** After a peer-join signal on
    /// `pool_feed_keyframe_tx`, the pool-feed bridge must push
    /// `latest_i420` at tick rate for the burst window — even when
    /// the buffer hasn't changed and the heartbeat hasn't elapsed.
    /// Without this, codecs that ignore `force_keyframe` on a
    /// long-running pipe (Linux ffmpeg H.264) wait many seconds for
    /// a natural keyframe on idle desktops because the heartbeat-
    /// only path delivers ~1 frame/sec — `pool.request_keyframe_all`
    /// alone won't reach those encoders. Counts encoded frames
    /// received by a pool subscriber over the burst window; without
    /// burst the count would be ≤2 (initial encode + at most one
    /// heartbeat), with burst it should be at tick rate (~30fps).
    #[tokio::test]
    async fn pool_feed_bridge_burst_clocks_encoder_at_tick_rate() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start must succeed");

        // Pre-seed `latest_frame` so the bridge has something to
        // push from tick 1 (the seed branch converts and primes
        // `latest_i420`). Avoids racing on the broadcast-recv path
        // and isolates the test to "what happens when burst fires
        // on a static buffer."
        *session.latest_frame.write().await =
            Some(Arc::new(make_test_bgra(64, 64)));

        let pool = session
            .pool
            .get()
            .expect("pool initialized after start");
        let prefs = encode::pool::PeerCodecPreferences::new(vec![
            encode::pool::CodecKind::Vp8,
        ]);
        let (subs, _lease) = pool
            .subscribe(&prefs)
            .expect("VP8 always-on subscribe");
        let mut frame_rx = subs
            .into_iter()
            .next()
            .expect("at least one subscription")
            .frames;

        session.ensure_pool_feed_bridge_started().await;

        // Drain the seeded frame's initial encode before measuring;
        // we want to count what the BURST produces, not the cold-
        // start encode that happens regardless.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            frame_rx.recv(),
        )
        .await;

        // Signal peer-join burst.
        session
            .pool_feed_keyframe_tx
            .lock()
            .await
            .as_ref()
            .expect("kf_tx installed by ensure_pool_feed_bridge_started")
            .send(())
            .expect("kf channel must be open");

        // Count frames over 1.2s — burst window is 1.5s, so we're
        // measuring inside the active window. Tick rate is 30fps so
        // we expect close to 36 frames; threshold at 10 gives 3–4x
        // slack for environmental jitter (VP8 encoder warm-up,
        // scheduler pauses, broadcast lag). Pre-3c.3b.4b: heartbeat
        // only would deliver 1 frame in 1.2s (the IDLE_HEARTBEAT
        // re-push at the 1s boundary).
        let start = Instant::now();
        let mut count = 0u32;
        while start.elapsed() < std::time::Duration::from_millis(1200) {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                frame_rx.recv(),
            )
            .await
            {
                Ok(Ok(_)) => count += 1,
                _ => break,
            }
        }

        assert!(
            count >= 10,
            "burst window must clock encoder at tick rate; got \
             {count} frames in 1.2s. Pre-3c.3b.4b: heartbeat-only \
             would deliver ≤2 — Linux ffmpeg H.264 stays black on \
             idle peer-join.",
        );

        session.shutdown.cancel();
    }

    /// **3c.3b.4c regression test (legacy-first mixed session).**
    /// When a legacy peer attached first, the legacy bridge owns
    /// the pool feed and `ensure_pool_feed_bridge_started` early-
    /// returns without installing `pool_feed_keyframe_tx`. A later
    /// pool peer's burst signal must reach the LEGACY bridge's
    /// `keyframe_tx`, or Linux H.264 stays black on the pool peer.
    /// `signal_peer_join_burst` dispatches to the pool-feed channel
    /// first and falls back to the legacy channel only when
    /// `pool_feed_keyframe_tx` is `None` (3c.3b.4d). Asserting the
    /// legacy channel receives in this state confirms the fallback
    /// path is wired.
    #[tokio::test]
    async fn signal_peer_join_burst_wakes_legacy_bridge_in_mixed_session() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);

        // Simulate "legacy bridge owns pool feed":
        //   - install legacy `keyframe_tx` (as `start_encoder_pipeline`
        //     would have done after taking `encoder_init_lock`)
        //   - leave `pool_feed_keyframe_tx` as `None` (mirrors the
        //     `ensure_pool_feed_bridge_started` early-return when
        //     `*init == true`).
        let (legacy_tx, mut legacy_rx) = mpsc::unbounded_channel::<()>();
        *session.keyframe_tx.lock().await = Some(legacy_tx);
        assert!(
            session.pool_feed_keyframe_tx.lock().await.is_none(),
            "test premise: pool_feed_keyframe_tx must be None to \
             model the mixed-session path",
        );

        session.signal_peer_join_burst().await;

        let recv = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            legacy_rx.recv(),
        )
        .await;
        assert!(
            matches!(recv, Ok(Some(()))),
            "legacy keyframe_tx must receive burst signal in mixed \
             session; got {recv:?}. Pre-3c.3b.4c: signal only went \
             to pool_feed_keyframe_tx (which is None here) → no-op \
             → Linux H.264 pool peer stays black.",
        );
    }

    /// **3c.3b.4d regression test (pool-first → legacy-later
    /// session).** Both keyframe channels are `Some` in this state
    /// (pool-feed bridge owns the pool feed via the install-first
    /// invariant; the legacy bridge runs but with `pool_for_bridge=
    /// None` per 3c.3b.3b coordination). `signal_peer_join_burst`
    /// MUST dispatch to the pool-feed channel only — over-waking
    /// the legacy bridge is wasted work and obscures the
    /// "pool_feed_keyframe_tx.is_some() ⇔ pool-feed bridge owns the
    /// pool feed" invariant.
    ///
    /// 3c.3b.4c-pre version sent to BOTH channels, which was correct
    /// (the legacy bridge with `pool_for_bridge=None` doesn't feed
    /// the pool, so the over-wake had no effect on the pool feed)
    /// but wasted work and made the invariant easy to misread. This
    /// test would have passed against both implementations; it
    /// exists to PIN the tightened semantics so a future "send to
    /// both for safety" regression fires here.
    #[tokio::test]
    async fn signal_peer_join_burst_dispatches_to_pool_feed_when_both_installed(
    ) {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);

        let (pool_tx, mut pool_rx) = mpsc::unbounded_channel::<()>();
        let (legacy_tx, mut legacy_rx) = mpsc::unbounded_channel::<()>();
        *session.pool_feed_keyframe_tx.lock().await = Some(pool_tx);
        *session.keyframe_tx.lock().await = Some(legacy_tx);

        session.signal_peer_join_burst().await;

        // Pool-feed channel MUST receive (it owns the feed).
        let pool_recv = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            pool_rx.recv(),
        )
        .await;
        assert!(
            matches!(pool_recv, Ok(Some(()))),
            "pool_feed_keyframe_tx must receive when both channels \
             are installed; got {pool_recv:?}",
        );

        // Legacy channel must NOT receive — it's not feeding the
        // pool in this state. Bounded short timeout: if the signal
        // were going to land it would be near-instantaneous.
        let legacy_recv = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            legacy_rx.recv(),
        )
        .await;
        assert!(
            legacy_recv.is_err(),
            "keyframe_tx must NOT receive when pool-feed bridge \
             owns the pool feed; got {legacy_recv:?}. Sending to \
             both over-wakes the legacy bridge — wasted work and \
             muddies the dispatch invariant.",
        );
    }

    /// **3c.3b.4c regression test (pool-only session).** Symmetric
    /// to the mixed-session test above: in a pool-only session the
    /// pool-feed bridge owns the feed and `pool_feed_keyframe_tx` is
    /// installed. Legacy `keyframe_tx` is `None`. `signal_peer_join_burst`
    /// must hit the pool-feed channel. Pins the existing 3c.3b.4b
    /// path stays correct after the dual-channel dispatch refactor.
    #[tokio::test]
    async fn signal_peer_join_burst_wakes_pool_feed_bridge_in_pool_only_session(
    ) {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);

        let (pool_tx, mut pool_rx) = mpsc::unbounded_channel::<()>();
        *session.pool_feed_keyframe_tx.lock().await = Some(pool_tx);
        assert!(
            session.keyframe_tx.lock().await.is_none(),
            "test premise: keyframe_tx must be None to model the \
             pool-only path",
        );

        session.signal_peer_join_burst().await;

        let recv = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            pool_rx.recv(),
        )
        .await;
        assert!(
            matches!(recv, Ok(Some(()))),
            "pool_feed_keyframe_tx must receive burst signal in \
             pool-only session; got {recv:?}",
        );
    }

    /// **3c.3b.3c follow-up finding 2.** `DisplaySession::stop()`
    /// must take + await the pool-feed bridge handle alongside
    /// the other owned tasks (capture/encoder/clipboard) for
    /// deterministic shutdown. The pool-feed task observes
    /// `shutdown.cancel()` on its own, but leaving the JoinHandle
    /// in the slot breaks the cleanup pattern and makes "is this
    /// session fully stopped" non-observable.
    #[tokio::test]
    async fn stop_takes_and_awaits_pool_feed_bridge_handle() {
        let backend = Arc::new(StubBackend { width: 64, height: 64 });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start must succeed");
        session.ensure_pool_feed_bridge_started().await;

        // Pre-condition: bridge spawned, handle is Some.
        assert!(
            session.pool_feed_bridge_handle.lock().await.is_some(),
            "pool feed bridge must be spawned before stop"
        );

        session.stop().await;

        assert!(
            session.pool_feed_bridge_handle.lock().await.is_none(),
            "stop() must take + await the pool-feed bridge handle, \
             leaving the slot empty (parity with capture/encoder/\
             clipboard cleanup at display/mod.rs:792-800)"
        );
    }
}
