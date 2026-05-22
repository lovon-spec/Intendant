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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::CallerError;

pub mod aggregator;
pub mod capture;
pub mod clipboard;
pub mod encode;
pub mod forward;
pub mod keymap;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod macos_keymap;
pub mod tile;
pub mod twcc_tap;
pub mod visual_marker;
#[cfg(target_os = "linux")]
pub mod wayland;
pub mod webrtc;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub mod windows_keymap;
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

/// Enumerate Windows displays via DXGI output enumeration (the same DXGI
/// objects the Desktop Duplication capture backend uses). Returns an empty
/// `Vec` on failure; [`enumerate_displays`] then supplies a fallback entry.
#[cfg(target_os = "windows")]
async fn enumerate_displays_platform() -> Vec<DisplayInfo> {
    windows::enumerate_displays().await
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
/// verify the frame matches the peer-negotiated RTP codec before packetizing.
/// H.264 frames in particular need the full spec (profile-level-id +
/// packetization-mode) because browser negotiation discriminates parameter
/// sets, not just codec names.
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
    MouseMove {
        x: f64,
        y: f64,
        #[serde(default)]
        buttons: u8,
    },
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
    /// Cumulative time from frame arrival in the encoder queue to encoded
    /// output, in microseconds. **NOT** a measure of encoder processing time
    /// alone — includes the wait the frame did in the encoder's input queue
    /// before it was picked up. On an idle Wayland desktop where the only
    /// thing that fires the encoder is the 30-second periodic snapshot,
    /// this can climb to 30+ seconds even though the actual encode pass
    /// took milliseconds. Reported by `DisplayMetricsSnapshot` as
    /// `encode_freshness_avg_ms`; the metric tells you how stale the
    /// emitted frames are at output time, not how slow the encoder is.
    pub encode_freshness_us_sum: AtomicU64,

    /// Total per-peer try_send failures in the fan-out task.
    ///
    /// `Arc<AtomicU64>` (not bare `AtomicU64`) so the per-peer
    /// `pool_frame_intake` task at `webrtc.rs` can share the same
    /// counter via `Arc::clone(&self.counters.peer_drops)`.
    pub peer_drops: Arc<AtomicU64>,
    /// Current number of connected WebRTC peers.
    pub peer_count: AtomicU64,

    /// Tile-stream damage samples processed in the current metrics window.
    pub tile_damage_samples: AtomicU64,
    /// Total dirty rects reported by the damage source in the current window.
    pub tile_dirty_rects: AtomicU64,
    /// Total dirty tiles selected in the current window.
    pub tile_dirty_tiles: AtomicU64,
    /// Sum of dirty fractions in parts-per-million for averaging.
    pub tile_dirty_fraction_ppm_sum: AtomicU64,
    /// Dirty tile updates skipped by the source cadence cap.
    pub tile_delta_cadence_skips: AtomicU64,
    /// Tile records packed into delta frames.
    pub tile_delta_records: AtomicU64,
    /// Tile delta wire frames generated.
    pub tile_delta_frames: AtomicU64,
    /// Tile delta wire bytes generated.
    pub tile_delta_bytes: AtomicU64,
    /// Tile records packed into snapshot frames.
    pub tile_snapshot_records: AtomicU64,
    /// Tile snapshot wire frames generated.
    pub tile_snapshot_frames: AtomicU64,
    /// Tile snapshot wire bytes generated.
    pub tile_snapshot_bytes: AtomicU64,

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
            encode_freshness_us_sum: AtomicU64::new(0),
            peer_drops: Arc::new(AtomicU64::new(0)),
            peer_count: AtomicU64::new(0),
            tile_damage_samples: AtomicU64::new(0),
            tile_dirty_rects: AtomicU64::new(0),
            tile_dirty_tiles: AtomicU64::new(0),
            tile_dirty_fraction_ppm_sum: AtomicU64::new(0),
            tile_delta_cadence_skips: AtomicU64::new(0),
            tile_delta_records: AtomicU64::new(0),
            tile_delta_frames: AtomicU64::new(0),
            tile_delta_bytes: AtomicU64::new(0),
            tile_snapshot_records: AtomicU64::new(0),
            tile_snapshot_frames: AtomicU64::new(0),
            tile_snapshot_bytes: AtomicU64::new(0),
            epoch_us: AtomicU64::new(now_us),
        }
    }

    fn record_tile_damage_sample(&self, rect_count: usize, tile_count: usize, dirty_fraction: f32) {
        self.tile_damage_samples.fetch_add(1, Ordering::Relaxed);
        self.tile_dirty_rects
            .fetch_add(rect_count as u64, Ordering::Relaxed);
        self.tile_dirty_tiles
            .fetch_add(tile_count as u64, Ordering::Relaxed);
        let ppm = (dirty_fraction.clamp(0.0, 1.0) * 1_000_000.0).round() as u64;
        self.tile_dirty_fraction_ppm_sum
            .fetch_add(ppm, Ordering::Relaxed);
    }

    fn record_tile_delta_cadence_skip(&self) {
        self.tile_delta_cadence_skips
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_tile_delta_source(&self, records: usize, frames: usize, bytes: usize) {
        self.tile_delta_records
            .fetch_add(records as u64, Ordering::Relaxed);
        self.tile_delta_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        self.tile_delta_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn record_tile_snapshot_source(&self, records: usize, frames: usize, bytes: usize) {
        self.tile_snapshot_records
            .fetch_add(records as u64, Ordering::Relaxed);
        self.tile_snapshot_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        self.tile_snapshot_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
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
    pub encode_freshness_avg_ms: f64,
    pub encode_drops: u64,
    pub peer_count: u64,
    pub peer_drops: u64,
    pub resolution: (u32, u32),
    pub tile_damage_samples: u64,
    pub tile_dirty_rects: u64,
    pub tile_dirty_tiles: u64,
    pub tile_dirty_fraction_avg: f64,
    pub tile_delta_cadence_skips: u64,
    pub tile_delta_records: u64,
    pub tile_delta_fps: f64,
    pub tile_delta_kbps: f64,
    pub tile_snapshot_records: u64,
    pub tile_snapshot_frames: u64,
    pub tile_snapshot_kbps: f64,
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
        let encode_freshness_us = counters.encode_freshness_us_sum.swap(0, Ordering::Relaxed);
        let peer_drops = counters.peer_drops.swap(0, Ordering::Relaxed);
        let peer_count = counters.peer_count.load(Ordering::Relaxed);
        let tile_damage_samples = counters.tile_damage_samples.swap(0, Ordering::Relaxed);
        let tile_dirty_rects = counters.tile_dirty_rects.swap(0, Ordering::Relaxed);
        let tile_dirty_tiles = counters.tile_dirty_tiles.swap(0, Ordering::Relaxed);
        let tile_dirty_fraction_ppm_sum = counters
            .tile_dirty_fraction_ppm_sum
            .swap(0, Ordering::Relaxed);
        let tile_delta_cadence_skips = counters.tile_delta_cadence_skips.swap(0, Ordering::Relaxed);
        let tile_delta_records = counters.tile_delta_records.swap(0, Ordering::Relaxed);
        let tile_delta_frames = counters.tile_delta_frames.swap(0, Ordering::Relaxed);
        let tile_delta_bytes = counters.tile_delta_bytes.swap(0, Ordering::Relaxed);
        let tile_snapshot_records = counters.tile_snapshot_records.swap(0, Ordering::Relaxed);
        let tile_snapshot_frames = counters.tile_snapshot_frames.swap(0, Ordering::Relaxed);
        let tile_snapshot_bytes = counters.tile_snapshot_bytes.swap(0, Ordering::Relaxed);

        let elapsed_secs = elapsed.elapsed().as_secs_f64().max(0.001);

        let encode_freshness_avg_ms = if encode_frames > 0 {
            (encode_freshness_us as f64 / encode_frames as f64) / 1000.0
        } else {
            0.0
        };
        let tile_dirty_fraction_avg = if tile_damage_samples > 0 {
            (tile_dirty_fraction_ppm_sum as f64 / tile_damage_samples as f64) / 1_000_000.0
        } else {
            0.0
        };

        Self {
            display_id,
            capture_fps: capture_frames as f64 / elapsed_secs,
            capture_drops,
            encode_fps: encode_frames as f64 / elapsed_secs,
            encode_freshness_avg_ms,
            encode_drops,
            peer_count,
            peer_drops,
            resolution,
            tile_damage_samples,
            tile_dirty_rects,
            tile_dirty_tiles,
            tile_dirty_fraction_avg,
            tile_delta_cadence_skips,
            tile_delta_records,
            tile_delta_fps: tile_delta_frames as f64 / elapsed_secs,
            tile_delta_kbps: (tile_delta_bytes as f64 * 8.0) / elapsed_secs / 1000.0,
            tile_snapshot_records,
            tile_snapshot_frames,
            tile_snapshot_kbps: (tile_snapshot_bytes as f64 * 8.0) / elapsed_secs / 1000.0,
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
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError>;

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
    capture_handle: Mutex<Option<JoinHandle<()>>>,
    shutdown: CancellationToken,
    counters: Arc<DisplayMetricsCounters>,
    /// Instant used as the epoch for rate computations.
    metrics_epoch: Mutex<Instant>,
    /// Clipboard monitor for bidirectional clipboard sync.
    clipboard_monitor: Arc<clipboard::ClipboardMonitor>,
    /// Handle for the clipboard forwarding task (remote -> browser).
    clipboard_handle: Mutex<Option<JoinHandle<()>>>,
    /// Wakes the pool-feed bridge to open a peer-join burst window
    /// when a new pool peer attaches. `Some` after
    /// [`Self::spawn_pool_feed_bridge`] has run (eagerly from
    /// [`Self::start`] since 3c.4d).
    ///
    /// **Why a burst is needed even though the pool also sets
    /// `force_keyframe`:** [`crate::display::encode::pool::EncoderPool::request_keyframe_all`]
    /// sets a per-encoder atomic flag that VP8 and macOS H.264 honor
    /// on their next encode. Linux H.264 (ffmpeg-pipe) explicitly
    /// ignores the flag (see `h264_linux.rs::encode`'s `_force_keyframe`
    /// underscore) — there's no per-frame "emit IDR now" path on a
    /// long-running rawvideo pipe. Compensation: clock the encoder
    /// at tick rate for ~one GOP boundary so its `-g 30` natural
    /// cadence emits a keyframe inside the burst window. Without
    /// this, an idle-desktop pool peer-join on Linux H.264 stays
    /// black for many seconds.
    pool_feed_keyframe_tx: Mutex<Option<mpsc::UnboundedSender<()>>>,
    /// Shared multi-codec encoder pool. Constructed lazily on the first
    /// `start()` call once the backend's resolution and fps are known;
    /// ownership is `Arc` so subsequent integration code can hand it to
    /// per-peer forwarders without lifetime juggling.
    ///
    /// `std::sync::OnceLock` (stable since 1.70) is the right shape:
    /// one-time init with shared read access afterward, synchronous so
    /// the blocking portion of `start()` can construct it inline, no
    /// tokio dependency. Concurrent `start()` callers (not expected but
    /// cheap to tolerate) converge on a single pool.
    pool: std::sync::OnceLock<Arc<encode::pool::EncoderPool>>,
    /// Handle for the capture-to-I420 bridge task. `Some` for the
    /// lifetime of a started session: spawned eagerly by
    /// [`Self::spawn_pool_feed_bridge`] during [`Self::start`]
    /// (since 3c.4d, was lazy on first offer before that). Owns the
    /// BGRA→I420 conversion and `pool.push_i420_frame` loop. Drained
    /// on `stop()` for deterministic teardown ordering.
    pool_feed_bridge_handle: Mutex<Option<JoinHandle<()>>>,
    /// D-3c: federated peers that opted into dirty-region tile
    /// streaming. Local DisplaySlot peers do not create tile data
    /// channels yet, so subscribers are registered explicitly by the
    /// federated offer path instead of inferred from `peers`.
    tile_subscribers: Arc<RwLock<HashSet<PeerId>>>,
    /// D-3c: capture-damage-to-tile bridge task. Sends initial
    /// snapshots and XDamage-driven tile updates to `tile_subscribers`
    /// over WebRTC data channels while leaving the VP8 video track alive
    /// as the current fallback.
    tile_stream_handle: Mutex<Option<JoinHandle<()>>>,
    /// D-4d: bounded replay buffer for recently sent tile deltas.
    /// Browser gap reports can be satisfied from this buffer when the
    /// complete missing sequence range is still present; otherwise
    /// recovery falls back to a fresh snapshot.
    tile_replay: Arc<RwLock<tile::recovery::TileUpdateReplayBuffer>>,
    tile_epoch: Arc<AtomicU32>,
    tile_snapshot_id: Arc<AtomicU32>,
    /// **Phase 0 visual-freshness diagnostic marker.** When `true`, the
    /// pool-feed bridge stamps a 32-bit binary timestamp into the top-left
    /// 128×64 px of each I420 Y plane before forwarding it to the encoder
    /// pool. The browser-side sampler in `PeerDisplayConnection` reads
    /// the marker per video frame to measure visual-freshness (effective
    /// fps, freeze intervals, transition gap p50/p95/max) without relying
    /// on getStats counters that proved misleading on task #81.
    ///
    /// **Off by default.** Toggled at runtime via
    /// `ControlMsg::SetDiagnosticsVisualMarker { display_id, enabled }`.
    /// Visible to ALL viewers of this display when on (the marker is
    /// stamped pre-encoder so it lands in every encoded layer);
    /// acceptable for an opt-in diagnostic flag. See task #80's standing
    /// quality bar (visual freshness, not packet counters) and task #83
    /// (Phase 0 scaffold) for context. The encoded marker value is the
    /// lower 32 bits of `(arrived - session_epoch).as_millis()`, so each
    /// frame carries its capture-time timestamp without a side channel.
    diagnostics_visual_marker: Arc<AtomicBool>,
    /// Reference instant for the diagnostic marker's 32-bit timestamp.
    /// Set once in [`Self::new`]; the bridge computes
    /// `arrived.duration_since(session_epoch).as_millis() as u32` per
    /// frame so the wrap horizon is ~49.7 days from session start
    /// (effectively irrelevant for any realistic smoke run).
    session_epoch: Instant,
    /// **Phase 4d.3b** layer-policy coordinator handle. `Some` for
    /// the lifetime of a started session: spawned eagerly by
    /// [`Self::start`] after the pool-feed bridge, awaited in
    /// [`Self::stop`] after the bridge is drained.
    ///
    /// The coordinator is the single owner of `pool.pause_layer` /
    /// `pool.resume_layer` decisions for this display. It composes
    /// three per-policy votes by intersection — pause wins; resume
    /// requires every active policy to agree:
    ///
    /// - **Presence policy** (zero-peer debounce): votes empty
    ///   when `presence_state` is Idle, full current rid set
    ///   otherwise. Owns the pause-after-5s-at-zero-peers and
    ///   resume-on-first-peer behaviour the original 4d.2
    ///   `spawn_zero_peer_aggregator` provided.
    /// - **Aggregate-TWCC policy** (per-peer cascaded loss):
    ///   votes floor + cascade-projected upper layers. Active on
    ///   the rtc 0.9 / WKWebView stack via
    ///   [`crate::display::twcc_tap`].
    /// - **Per-RID RR policy** (per-(peer, RID) `fraction_lost`):
    ///   votes floor + per-RID Wanted-state-projection. Currently
    ///   inert on the rtc 0.9 stack (RR stats never populate),
    ///   stays warm so future stacks that surface fresh RR
    ///   activate it without code changes.
    ///
    /// See [`crate::display::aggregator::spawn_layer_policy_coordinator`]
    /// for the composition machinery and
    /// [`crate::display::aggregator::compose_effective_wanted`]
    /// for the intersection rule. `None` before `start()` / after
    /// `stop()`.
    layer_policy_handle: Mutex<Option<JoinHandle<()>>>,
}

/// Convert one BGRA frame to I420 for the pool-feed bridge.
///
/// Phase 0 visual-freshness stamps are applied later, at pool send/encoder
/// time, so heartbeat re-sends of a static desktop still carry a fresh marker
/// timestamp and downscaled layers get a marker in their final output
/// resolution.
fn convert_for_pool_feed(bgra: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let i420 = encode::bgra_to_i420(bgra, width, height, stride);

    // Windows black-frame diagnostic (hop A of the capture → encoder chain):
    // log the average byte of the BGRA going INTO bgra_to_i420 and the I420
    // coming OUT, for the first few converted frames. The capture thread logs
    // the same BGRA buffer as `avg_byte` (~189 on a live desktop); if the
    // value here differs, the buffer changed between capture and the bridge.
    // If BGRA-in is bright but I420-out is ~0, the conversion (or its
    // dimensions/stride) is the offending hop.
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static BRIDGE_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
        let n = BRIDGE_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 5 {
            eprintln!(
                "[display/pool-feed] frame #{} convert {}x{} stride={} \
                 bgra avg={} -> i420 avg={} (i420 len={})",
                n + 1,
                width,
                height,
                stride,
                encode::sampled_avg_byte(bgra),
                encode::sampled_avg_byte(&i420),
                i420.len(),
            );
        }
    }

    i420
}

const TILE_STREAM_TILE_SIZE_PX: u16 = 64;
const TILE_DELTA_TARGET_FPS: u32 = 15;

fn tile_delta_min_interval() -> Duration {
    Duration::from_millis(1_000 / TILE_DELTA_TARGET_FPS as u64)
}

fn should_emit_tile_delta(
    now: Instant,
    last_sent_at: Option<Instant>,
    min_interval: Duration,
) -> bool {
    match last_sent_at {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= min_interval,
    }
}

fn tile_snapshot_period(mode: tile::policy::TileMode) -> Duration {
    match mode {
        tile::policy::TileMode::Tiles => Duration::from_secs(30),
        tile::policy::TileMode::Video => Duration::from_secs(60),
    }
}

fn tile_pixel_format(format: FrameFormat) -> tile::encode::TilePixelFormat {
    match format {
        FrameFormat::Bgra => tile::encode::TilePixelFormat::Bgra,
        FrameFormat::Rgba => tile::encode::TilePixelFormat::Rgba,
    }
}

fn tile_grid_for_frame(frame: &Frame) -> Option<tile::grid::TileGrid> {
    tile::grid::TileGrid::new(frame.width, frame.height, TILE_STREAM_TILE_SIZE_PX)
}

fn all_tile_ids(grid: &tile::grid::TileGrid) -> Vec<tile::grid::TileId> {
    let mut out = Vec::with_capacity(grid.total_tiles());
    for y in 0..grid.height_tiles {
        for x in 0..grid.width_tiles {
            out.push(tile::grid::TileId::new(x, y));
        }
    }
    out
}

fn encode_tile_records(
    frame: &Frame,
    tiles: Vec<tile::grid::TileId>,
    visual_marker_value: Option<u32>,
) -> Result<Vec<tile::transport::TileRecord>, tile::encode::TileEncodeError> {
    let src = tile::encode::TileSource {
        data: &frame.data,
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
        format: tile_pixel_format(frame.format),
        tile_size_px: TILE_STREAM_TILE_SIZE_PX,
    };
    tiles
        .into_iter()
        .map(|tile| {
            let mut raw = tile::encode::raw_bgra_tile(&src, tile)?;
            if let Some(value) = visual_marker_value {
                stamp_visual_marker_bgra_tile(&mut raw, tile, src.tile_size_px, value);
            }
            tile::encode::encode_raw_bgra_payload(tile, raw, src.tile_size_px)
        })
        .collect()
}

fn current_visual_marker_value(marker_flag: &AtomicBool, session_epoch: Instant) -> Option<u32> {
    if marker_flag.load(Ordering::Relaxed) {
        Some(
            Instant::now()
                .saturating_duration_since(session_epoch)
                .as_millis() as u32,
        )
    } else {
        None
    }
}

fn stamp_visual_marker_bgra_tile(
    raw_bgra: &mut [u8],
    tile: tile::grid::TileId,
    tile_size_px: u16,
    value: u32,
) {
    let tile_size = tile_size_px as usize;
    if tile_size == 0 || raw_bgra.len() < tile_size * tile_size * 4 {
        return;
    }

    let tile_x0 = tile.x as usize * tile_size;
    let tile_y0 = tile.y as usize * tile_size;
    let tile_x1 = tile_x0 + tile_size;
    let tile_y1 = tile_y0 + tile_size;

    if tile_x0 >= visual_marker::MARKER_W || tile_y0 >= visual_marker::MARKER_H {
        return;
    }

    for row in 0..visual_marker::ROWS {
        for col in 0..visual_marker::COLS {
            let bit_idx = row * visual_marker::COLS + col;
            let bit = (value >> bit_idx) & 1;
            let luma = if bit == 1 {
                visual_marker::LUMA_HIGH
            } else {
                visual_marker::LUMA_LOW
            };
            let marker_x0 = col * visual_marker::TILE_PX;
            let marker_y0 = row * visual_marker::TILE_PX;
            let marker_x1 = marker_x0 + visual_marker::TILE_PX;
            let marker_y1 = marker_y0 + visual_marker::TILE_PX;

            let x0 = marker_x0.max(tile_x0);
            let y0 = marker_y0.max(tile_y0);
            let x1 = marker_x1.min(tile_x1);
            let y1 = marker_y1.min(tile_y1);
            if x0 >= x1 || y0 >= y1 {
                continue;
            }

            for y in y0..y1 {
                let local_y = y - tile_y0;
                for x in x0..x1 {
                    let local_x = x - tile_x0;
                    let idx = (local_y * tile_size + local_x) * 4;
                    if idx + 3 < raw_bgra.len() {
                        raw_bgra[idx] = luma;
                        raw_bgra[idx + 1] = luma;
                        raw_bgra[idx + 2] = luma;
                        raw_bgra[idx + 3] = 255;
                    }
                }
            }
        }
    }
}

fn should_try_xdamage_for_tile_stream(backend_kind: &str) -> bool {
    backend_kind == "x11"
}

fn make_damage_backend(
    width: u32,
    height: u32,
    backend_kind: &'static str,
) -> Box<dyn capture::damage::DamageBackend> {
    #[cfg(not(target_os = "linux"))]
    let _ = backend_kind;

    #[cfg(target_os = "linux")]
    {
        if should_try_xdamage_for_tile_stream(backend_kind) {
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
            match capture::x11_damage::X11DamageBackend::new(&display) {
                Ok(backend) => {
                    eprintln!("[display/tile] XDamage backend enabled on DISPLAY={display}");
                    return Box::new(backend);
                }
                Err(e) => {
                    eprintln!(
                        "[display/tile] XDamage unavailable on DISPLAY={display}: {e}; \
                         tile stream will use frame-diff fallback"
                    );
                }
            }
        } else {
            eprintln!("[display/tile] backend={backend_kind} uses frame-diff tile damage fallback");
        }
    }

    Box::new(capture::damage::NullDamageBackend::new(width, height))
}

async fn send_tile_snapshot_to_peer(
    peer: Arc<webrtc::WebRtcPeer>,
    frame: Arc<Frame>,
    epoch: u32,
    snapshot_id: u32,
    visual_marker_value: Option<u32>,
    counters: Arc<DisplayMetricsCounters>,
) {
    let Some(grid) = tile_grid_for_frame(&frame) else {
        return;
    };
    let encode_result = tokio::task::spawn_blocking({
        let frame = Arc::clone(&frame);
        move || {
            let records = encode_tile_records(&frame, all_tile_ids(&grid), visual_marker_value)?;
            Ok::<_, tile::encode::TileEncodeError>((grid, records))
        }
    })
    .await;

    let Ok(Ok((grid, records))) = encode_result else {
        eprintln!("[display/tile] snapshot tile encode failed");
        return;
    };

    let record_count = records.len();
    let frames = match tile::transport::pack_snapshot_chunks(
        epoch,
        snapshot_id,
        grid.width_tiles,
        grid.height_tiles,
        grid.tile_size_px,
        records,
    ) {
        Ok(frames) => frames,
        Err(e) => {
            eprintln!("[display/tile] snapshot pack failed: {e}");
            return;
        }
    };

    let frame_count = frames.len();
    let mut byte_count = 0usize;
    for frame in frames {
        match tile::transport::encode_frame(&frame) {
            Ok(bytes) => {
                byte_count = byte_count.saturating_add(bytes.len());
                if let Err(e) = peer.send_tile_snapshot_frame(bytes).await {
                    eprintln!("[display/tile] send snapshot failed: {e}");
                }
            }
            Err(e) => eprintln!("[display/tile] snapshot wire encode failed: {e}"),
        }
    }
    counters.record_tile_snapshot_source(record_count, frame_count, byte_count);
}

async fn send_tile_control_to_peers(
    peers: &[Arc<webrtc::WebRtcPeer>],
    frame: tile::transport::TileFrame,
    context: &str,
) {
    match tile::transport::encode_frame(&frame) {
        Ok(bytes) => {
            for peer in peers {
                if let Err(e) = peer.send_tile_control_frame(bytes.clone()).await {
                    eprintln!("[display/tile] {context} send failed: {e}");
                }
            }
        }
        Err(e) => eprintln!("[display/tile] {context} encode failed: {e}"),
    }
}

async fn send_latest_tile_snapshot_to_peer_id(
    peers: Arc<RwLock<HashMap<PeerId, Arc<webrtc::WebRtcPeer>>>>,
    latest_frame: Arc<RwLock<Option<Arc<Frame>>>>,
    tile_epoch: Arc<AtomicU32>,
    tile_snapshot_id: Arc<AtomicU32>,
    marker_flag: Arc<AtomicBool>,
    counters: Arc<DisplayMetricsCounters>,
    session_epoch: Instant,
    peer_id: PeerId,
    context: &'static str,
) {
    let peer = peers.read().await.get(&peer_id).cloned();
    let Some(peer) = peer else {
        return;
    };
    let frame = latest_frame.read().await.clone();
    let Some(frame) = frame else {
        eprintln!("[display/tile] {context}: no latest frame available for snapshot");
        return;
    };
    let epoch = tile_epoch.load(Ordering::Relaxed);
    let snapshot_id = tile_snapshot_id.fetch_add(1, Ordering::Relaxed);
    let visual_marker_value = current_visual_marker_value(&marker_flag, session_epoch);
    send_tile_snapshot_to_peer(
        peer,
        frame,
        epoch,
        snapshot_id,
        visual_marker_value,
        counters,
    )
    .await;
}

async fn tile_subscriber_peer_handles(
    peers: &Arc<RwLock<HashMap<PeerId, Arc<webrtc::WebRtcPeer>>>>,
    subscribers: &Arc<RwLock<HashSet<PeerId>>>,
) -> Vec<Arc<webrtc::WebRtcPeer>> {
    let ids: Vec<PeerId> = subscribers.read().await.iter().copied().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let peers = peers.read().await;
    ids.into_iter()
        .filter_map(|id| peers.get(&id).cloned())
        .collect()
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
            capture_handle: Mutex::new(None),
            shutdown: CancellationToken::new(),
            counters: Arc::new(DisplayMetricsCounters::new()),
            metrics_epoch: Mutex::new(Instant::now()),
            clipboard_monitor: Arc::new(clipboard::ClipboardMonitor::new()),
            clipboard_handle: Mutex::new(None),
            pool_feed_keyframe_tx: Mutex::new(None),
            pool: std::sync::OnceLock::new(),
            pool_feed_bridge_handle: Mutex::new(None),
            tile_subscribers: Arc::new(RwLock::new(HashSet::new())),
            tile_stream_handle: Mutex::new(None),
            tile_replay: Arc::new(RwLock::new(tile::recovery::TileUpdateReplayBuffer::new())),
            tile_epoch: Arc::new(AtomicU32::new(1)),
            tile_snapshot_id: Arc::new(AtomicU32::new(1)),
            diagnostics_visual_marker: Arc::new(AtomicBool::new(false)),
            session_epoch: Instant::now(),
            layer_policy_handle: Mutex::new(None),
        }
    }

    /// Toggle the Phase 0 visual-freshness diagnostic marker on or off.
    /// Idempotent. Effective on the next encoder-tick after the store
    /// (Relaxed ordering is sufficient — this is a diagnostic flag, no
    /// happens-before requirement against the marker's pixel writes).
    ///
    /// Visible to ALL viewers of this display when on, since the marker
    /// is stamped pre-encoder. Operators enable it for a smoke run and
    /// disable it after collecting the NDJSON transcript.
    pub fn set_diagnostics_visual_marker(&self, enabled: bool) {
        self.diagnostics_visual_marker
            .store(enabled, Ordering::Relaxed);
    }

    /// Read the current state of the diagnostic marker flag. Used by the
    /// runtime toggle handler for idempotency reporting.
    pub fn diagnostics_visual_marker_enabled(&self) -> bool {
        self.diagnostics_visual_marker.load(Ordering::Relaxed)
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

        // Source resolution is resolved AFTER `start_capture` because
        // some backends (Wayland portal) revise dims during capture
        // negotiation. Normalize to even dims as the single point of
        // enforcement for the bridge + pool + encoder chain — VP8
        // encoder construction and `downscale_i420` both require even
        // dims, and most backends already apply `& !1` (X11Backend::new
        // does), but defending here means the contract holds even if a
        // future backend forgets.
        let (raw_width, raw_height) = self.backend.resolution();
        let width = raw_width & !1;
        let height = raw_height & !1;

        // Phase 4b validation guard. If `vp8_simulcast` would produce
        // no encodable layers at these dims, fail loud BEFORE spawning
        // the capture bridge and AFTER cleanly tearing down the
        // backend's capture state. `EncoderPool::new` accepts an empty
        // always-on set deliberately (many unit tests pass
        // `|_, _| vec![]` for on-demand-only flows), but production
        // always-on must be guaranteed — half-initializing a pool that
        // emits no media is the silent-black-screen class.
        //
        // This check runs before the `tokio::spawn` below — leaving a
        // backend capture thread running after `start()` reports
        // failure is the leak class to avoid (X11's capture thread, in
        // particular, ignores send-on-dropped-rx and only exits on
        // explicit `stop_capture`).
        // On Windows the always-on baseline is a single H.264 layer (VP8 is
        // gated off — see `encode::pool::BASELINE_CODEC`), so the guard checks
        // that the single full-resolution layer clears MIN_LAYER_DIM rather
        // than the VP8 simulcast set.
        #[cfg(not(target_os = "windows"))]
        let baseline_empty = encode::pool::LayerSpec::vp8_simulcast(width, height, fps).is_empty();
        #[cfg(target_os = "windows")]
        let baseline_empty =
            width < encode::pool::MIN_LAYER_DIM || height < encode::pool::MIN_LAYER_DIM;
        if baseline_empty {
            self.backend.stop_capture().await;
            return Err(CallerError::Display(format!(
                "source too small for the always-on encoder baseline: \
                 {raw_width}x{raw_height} (normalized to {width}x{height}, \
                 below MIN_LAYER_DIM={})",
                encode::pool::MIN_LAYER_DIM,
            )));
        }

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

        // Construct the shared encoder pool with a VP8 simulcast-capable
        // layer factory (full / half / quarter, dropping any below
        // MIN_LAYER_DIM). The pool spawns one VP8 encoder thread per
        // surviving layer immediately; each thread blocks in
        // `blocking_recv` until the pool-feed bridge below starts
        // publishing I420 frames. Idle cost is negligible per layer.
        //
        // Whether each layer actually emits frames is governed by the
        // demand-bound (#48) and capacity policies (4d.3b). Default
        // demand is shaped by the connecting peer:
        //   - local DisplaySlot single-RID viewer (post-#58 default):
        //     demands `f` only → upper layers stay paused;
        //   - federated single-encoding/no-recv-simulcast viewer
        //     (post-#48 floor pick): demands `q` only → upper layers
        //     stay paused;
        //   - opt-in multi-RID viewer (offer carries
        //     `a=simulcast:recv f;h;q`): demands `{f,h,q}` → all
        //     layers emit. This is the experimental adaptive-bandwidth
        //     path, not the standard configuration.
        //
        // `get_or_init` swallows concurrent initializations cheaply;
        // in practice `start()` is called at most once per session.
        // Always-on layer factory. The factory receives the (possibly
        // resized) source dims and re-derives the layout from them — so a
        // runtime resize regenerates the layer set at the new dims rather
        // than rescaling (and accumulating rounding drift on) the previous
        // epoch's handles. This is the contract from 4a-fix-#3: every
        // construction site (initial spawn AND on_resize) goes through the
        // factory's `normalize_layer_dims` filter.
        //
        // macOS/Linux: VP8 simulcast (up to full / half / quarter). The
        // multi-RID end-to-end machinery (answer SDP carrying
        // `a=simulcast:send f;h;q` + per-rid lines, multi-forwarder intake,
        // TWCC-driven per-layer pick) lights up only when a peer offers the
        // matching `a=simulcast:recv f;h;q` hint; the default DisplaySlot and
        // federated paths leave it dormant.
        //
        // Windows: a single full-resolution H.264 layer (`BASELINE_CODEC` is
        // H.264 there because VP8/libvpx is gated off). H.264 isn't
        // simulcast in this pool — matching the existing `LayerSpec::single`
        // rationale — so the always-on bank is one layer the Media
        // Foundation encoder serves.
        #[cfg(not(target_os = "windows"))]
        let layer_factory =
            move |w: u32, h: u32| encode::pool::LayerSpec::vp8_simulcast(w, h, fps);
        #[cfg(target_os = "windows")]
        let layer_factory = move |w: u32, h: u32| {
            vec![encode::pool::LayerSpec::single(
                encode::pool::CodecKind::H264,
                w,
                h,
                fps,
            )]
        };

        let pool_arc = Arc::clone(self.pool.get_or_init(|| {
            Arc::new(encode::pool::EncoderPool::new(
                width,
                height,
                fps,
                layer_factory,
                // Pool encoders feed the same metrics counters as the
                // capture bridge so DisplayMetricsSnapshot continues to
                // reflect total throughput. Pool is the sole producer
                // since 3c.4b deleted the legacy fan-out.
                Some(Arc::clone(&self.counters)),
            ))
        }));

        // 3c.4d: spawn the pool-feed bridge eagerly. The bridge owns
        // BGRA→I420 conversion and `pool.push_i420_frame`; it pumps
        // every always-on encoder for the lifetime of the session,
        // independent of whether any peer is connected. Replaces the
        // previous lazy spawn from `handle_offer_pool_mode` (then
        // `ensure_pool_feed_bridge_started`) — see
        // [`Self::spawn_pool_feed_bridge`] for the rationale.
        self.spawn_pool_feed_bridge(Arc::clone(&pool_arc), fps, event_bus_for_encoder)
            .await;
        self.spawn_tile_stream_bridge(fps).await;

        // 4d.3b: spawn the single layer-policy coordinator. One task
        // owns `pool.pause_layer` / `pool.resume_layer` decisions
        // for this display. Three policies vote (presence,
        // aggregate-TWCC, per-RID-RR) and the coordinator composes
        // by intersection — pause wins; resume requires every
        // active policy to agree. Replaces the previous design that
        // ran each policy as an independent task writing to the
        // pool, which produced opposite actions when one policy had
        // signal and another didn't (the per-RID "no signal →
        // default Wanted" semantic would resume layers the TWCC
        // policy had paused).
        //
        // Three closures, all capturing `Arc<EncoderPool>` and
        // querying it fresh each tick — `pool.on_resize` regenerates
        // always-on layer handles, so snapshots taken at spawn time
        // would go stale.
        //
        //   - `get_current_rids` returns the pool's full VP8
        //     simulcast layer set in spec order (descending
        //     bitrate; last entry is floor). Coordinator derives
        //     the floor + upper-layer split internally.
        //   - `is_layer_paused` queries actual pool state for one
        //     RID — diffing against actual (rather than against an
        //     internal `last_applied`) handles the resize-
        //     regenerates-active case correctly.
        //   - `on_action` routes `CapacityAction::PauseLayer` /
        //     `ResumeLayer` to `pool.pause_layer` /
        //     `pool.resume_layer` with `CodecKind::Vp8`.
        //
        // The 5 s zero-peer pause debounce, the 5 s drop /
        // 1 s restore debounces for both TWCC and RR, and the
        // hysteresis between drop (0.05) and recovery (0.02)
        // thresholds all live inside the coordinator and are
        // configured via `CapacityPolicyConfig::default()`.
        let pool_for_rids = Arc::clone(&pool_arc);
        let get_current_rids: Box<dyn Fn() -> Vec<encode::pool::SimulcastRid> + Send + Sync> =
            Box::new(move || {
                pool_for_rids
                    .always_on_ids()
                    .into_iter()
                    .filter(|id| id.codec == encode::pool::CodecKind::Vp8)
                    .map(|id| id.rid)
                    .collect()
            });
        let pool_for_query = Arc::clone(&pool_arc);
        let is_layer_paused: Box<
            dyn Fn(&encode::pool::SimulcastRid) -> Option<bool> + Send + Sync,
        > = Box::new(move |rid| {
            pool_for_query.is_layer_paused(encode::pool::CodecKind::Vp8, rid.clone())
        });
        let pool_for_action = Arc::clone(&pool_arc);
        let on_action: Box<dyn Fn(aggregator::CapacityAction) + Send + Sync> =
            Box::new(move |action| {
                // Operational observability: one line per layer-policy
                // action. Volume is bounded by the cascade (max 4
                // events per drop+recover cycle: pause top, pause mid,
                // resume mid, resume top) plus presence transitions.
                // Format mirrors the action variant for grep-ability:
                // `[layer-policy] PauseLayer(<rid>)` / `ResumeLayer(<rid>)`.
                match &action {
                    aggregator::CapacityAction::PauseLayer(rid) => {
                        eprintln!("[layer-policy] PauseLayer({rid:?})");
                    }
                    aggregator::CapacityAction::ResumeLayer(rid) => {
                        eprintln!("[layer-policy] ResumeLayer({rid:?})");
                    }
                }
                match action {
                    aggregator::CapacityAction::PauseLayer(rid) => {
                        pool_for_action.pause_layer(encode::pool::CodecKind::Vp8, rid);
                    }
                    aggregator::CapacityAction::ResumeLayer(rid) => {
                        pool_for_action.resume_layer(encode::pool::CodecKind::Vp8, rid);
                    }
                }
            });
        let layer_policy_task = aggregator::spawn_layer_policy_coordinator(
            Arc::clone(&self.peers),
            get_current_rids,
            is_layer_paused,
            on_action,
            aggregator::CapacityPolicyConfig::default(),
            self.shutdown.clone(),
        );
        *self.layer_policy_handle.lock().await = Some(layer_policy_task);

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
            // Managed by shutdown token; task self-cancels on stop().
            drop(reg_handle);
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
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            // Skip the immediate first tick.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let m = session.metrics().await;
                        eprintln!(
                            "[display/metrics] id={} capture={:.1}fps encode={:.1}fps \
                             drops=cap:{}/enc:{}/peer:{} peers={} freshness_avg={:.1}ms res={}x{} \
                             tile=dirty:{}r/{}t/{:.3} delta={:.1}fps/{:.1}kbps/{}rec/skips:{} \
                             snap={}f/{:.1}kbps/{}rec",
                            m.display_id,
                            m.capture_fps,
                            m.encode_fps,
                            m.capture_drops,
                            m.encode_drops,
                            m.peer_drops,
                            m.peer_count,
                            m.encode_freshness_avg_ms,
                            m.resolution.0,
                            m.resolution.1,
                            m.tile_dirty_rects,
                            m.tile_dirty_tiles,
                            m.tile_dirty_fraction_avg,
                            m.tile_delta_fps,
                            m.tile_delta_kbps,
                            m.tile_delta_records,
                            m.tile_delta_cadence_skips,
                            m.tile_snapshot_frames,
                            m.tile_snapshot_kbps,
                            m.tile_snapshot_records,
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
        if let Some(h) = self.tile_stream_handle.lock().await.take() {
            let _ = h.await;
        }
        // 4d.3b: layer-policy coordinator observes the same
        // `shutdown` token and exits its tick loop on its own. Take
        // + await for deterministic teardown ordering (parity with
        // the bridge and capture handles above) and so a follow-up
        // `start()` doesn't leak the previous session's task.
        if let Some(h) = self.layer_policy_handle.lock().await.take() {
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

    /// D-3c: register a federated WebRtcPeer for tile streaming and
    /// queue/send an initial snapshot when a captured frame is already
    /// available. Local DisplaySlot peers are not registered in D-3.
    pub async fn register_tile_subscriber(&self, peer_id: PeerId) {
        self.tile_subscribers.write().await.insert(peer_id);
        let Some(peer) = self.get_peer(peer_id).await else {
            return;
        };
        let Some(frame) = self.latest_frame().await else {
            return;
        };
        let epoch = self.tile_epoch.load(Ordering::Relaxed);
        let snapshot_id = self.tile_snapshot_id.fetch_add(1, Ordering::Relaxed);
        let visual_marker_value =
            current_visual_marker_value(&self.diagnostics_visual_marker, self.session_epoch);
        send_tile_snapshot_to_peer(
            peer,
            frame,
            epoch,
            snapshot_id,
            visual_marker_value,
            Arc::clone(&self.counters),
        )
        .await;
    }

    /// D-3c: unregister a tile subscriber. Safe to call even when the
    /// peer was never registered.
    pub async fn unregister_tile_subscriber(&self, peer_id: PeerId) {
        self.tile_subscribers.write().await.remove(&peer_id);
    }

    fn build_tile_control_handler(&self, peer_id: PeerId) -> self::webrtc::TileControlHandler {
        let peers = Arc::clone(&self.peers);
        let subscribers = Arc::clone(&self.tile_subscribers);
        let latest_frame = Arc::clone(&self.latest_frame);
        let tile_epoch = Arc::clone(&self.tile_epoch);
        let tile_snapshot_id = Arc::clone(&self.tile_snapshot_id);
        let tile_replay = Arc::clone(&self.tile_replay);
        let marker_flag = Arc::clone(&self.diagnostics_visual_marker);
        let counters = Arc::clone(&self.counters);
        let session_epoch = self.session_epoch;

        Arc::new(move |msg| {
            let peers = Arc::clone(&peers);
            let subscribers = Arc::clone(&subscribers);
            let latest_frame = Arc::clone(&latest_frame);
            let tile_epoch = Arc::clone(&tile_epoch);
            let tile_snapshot_id = Arc::clone(&tile_snapshot_id);
            let tile_replay = Arc::clone(&tile_replay);
            let marker_flag = Arc::clone(&marker_flag);
            let counters = Arc::clone(&counters);

            tokio::spawn(async move {
                match msg {
                    self::webrtc::TileControlMessage::Subscribe { .. } => {
                        subscribers.write().await.insert(peer_id);
                        send_latest_tile_snapshot_to_peer_id(
                            peers,
                            latest_frame,
                            tile_epoch,
                            tile_snapshot_id,
                            Arc::clone(&marker_flag),
                            Arc::clone(&counters),
                            session_epoch,
                            peer_id,
                            "subscribe",
                        )
                        .await;
                    }
                    self::webrtc::TileControlMessage::SnapshotRequest { .. } => {
                        send_latest_tile_snapshot_to_peer_id(
                            peers,
                            latest_frame,
                            tile_epoch,
                            tile_snapshot_id,
                            Arc::clone(&marker_flag),
                            Arc::clone(&counters),
                            session_epoch,
                            peer_id,
                            "snapshot-request",
                        )
                        .await;
                    }
                    self::webrtc::TileControlMessage::GapReport {
                        epoch,
                        last_seen_seq,
                        expected_seq,
                    } => {
                        let decision = {
                            let mut replay = tile_replay.write().await;
                            replay.replay_gap(epoch, last_seen_seq, expected_seq, Instant::now())
                        };
                        match decision {
                            tile::recovery::ReplayDecision::Frames(frames) => {
                                let peer = peers.read().await.get(&peer_id).cloned();
                                let Some(peer) = peer else {
                                    return;
                                };
                                for frame in frames {
                                    if let Err(e) = peer.send_tile_delta_frame(frame.bytes).await {
                                        eprintln!("[display/tile] gap replay send failed: {e}");
                                    }
                                }
                            }
                            tile::recovery::ReplayDecision::SnapshotRequired => {
                                send_latest_tile_snapshot_to_peer_id(
                                    peers,
                                    latest_frame,
                                    tile_epoch,
                                    tile_snapshot_id,
                                    Arc::clone(&marker_flag),
                                    Arc::clone(&counters),
                                    session_epoch,
                                    peer_id,
                                    "gap-recovery",
                                )
                                .await;
                            }
                            tile::recovery::ReplayDecision::NoGap => {}
                        }
                    }
                }
            });
        })
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
                        tight.push(frame.data[px]); // B
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
    /// Per-peer codec selection via the encoder pool: each peer's
    /// codec set is negotiated from its own SDP rather than locked
    /// to a session-wide codec, so concurrent peers can use different
    /// codecs without interfering. See [`Self::handle_offer_pool_mode`]
    /// for the implementation; this method is a thin wrapper retained
    /// for the public API surface.
    ///
    /// Creates a `WebRtcPeer`, subscribes it to the pool encoder
    /// output for its negotiated codec, inserts it into the peer
    /// map (closing any displaced peer with the same id), starts
    /// clipboard monitoring, fires a peer-join keyframe + burst,
    /// and returns the SDP answer.
    #[allow(clippy::too_many_arguments)]
    /// `input_authorized` is the per-peer gate the data-channel input
    /// handler consults before forwarding events to [`DisplayBackend::inject_input`].
    /// `display/mod.rs` deliberately does NOT know how authority is
    /// stored — the closure is the entire boundary. Phase 5a.1.
    ///
    /// Callers that don't gate input (test harnesses, federated paths
    /// that explicitly opt out of authority enforcement) pass
    /// `Arc::new(|| true)` or `Arc::new(|| false)`. The local `/ws`
    /// display-offer path in `web_gateway` builds a closure capturing
    /// `(display_id, this_connection_id, authority_map)`; the federated
    /// path passes deny-by-default until federation authority lands.
    pub async fn handle_offer(
        &self,
        peer_id: PeerId,
        sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<self::webrtc::TcpPeerRegistry>>,
        tcp_advertised_addr: Option<std::net::SocketAddr>,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        input_authorized: Arc<dyn Fn() -> bool + Send + Sync>,
        authority_handler: self::webrtc::AuthorityChannelHandler,
    ) -> Result<String, CallerError> {
        self.handle_offer_pool_mode(
            peer_id,
            sdp,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            ice_tx,
            input_authorized,
            authority_handler,
        )
        .await
    }

    /// The body of [`Self::handle_offer`]. Kept separate so the
    /// public method stays a thin wrapper that callers can audit at
    /// a glance; 3c.5 may inline this once the env-flag and the last
    /// stale references are gone.
    ///
    /// Pipeline:
    ///   - Codec selection is per-peer in [`webrtc::WebRtcPeer::new`],
    ///     driven by the active codec selected from the pool's initial
    ///     subscribe result. No first-peer codec lock.
    ///   - Peer-join keyframe is wired in two parts at the tail:
    ///     * [`crate::display::encode::pool::EncoderPool::request_keyframe_all`]
    ///       (3c.3b.4a) fires a coalesced PLI-equivalent across
    ///       every active encoder. Honored by VP8 and macOS H.264.
    ///     * Burst signal via [`Self::signal_peer_join_burst`]
    ///       (3c.3b.4b) wakes the pool-feed bridge to clock the
    ///       encoder at tick rate for ~1.5s. Required for codecs
    ///       that ignore `force_keyframe` on a long-running pipe
    ///       (Linux ffmpeg H.264) — without the burst, an idle-
    ///       desktop pool peer-join on Linux H.264 stays black for
    ///       many seconds waiting on the heartbeat-paced 1 push/sec
    ///       cadence.
    ///     The PLI-driven per-peer explicit request from inbound RTCP
    ///     lands with the simulcast work.
    ///   - No `pool_leases` tracking on `DisplaySession`:
    ///     [`webrtc::WebRtcPeer::new`] hands the lease to
    ///     the per-peer `pool_frame_intake` task, which owns it for
    ///     the peer's lifetime. [`Self::remove_peer`] calls
    ///     `peer.close()`, the shutdown token fires, and the intake
    ///     task drops the lease via `drop(current_lease.take())`
    ///     (RAII releases on-demand refcounts under the existing
    ///     generation gate).
    async fn handle_offer_pool_mode(
        &self,
        peer_id: PeerId,
        sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<self::webrtc::TcpPeerRegistry>>,
        tcp_advertised_addr: Option<std::net::SocketAddr>,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        input_authorized: Arc<dyn Fn() -> bool + Send + Sync>,
        authority_handler: self::webrtc::AuthorityChannelHandler,
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
        // H.264 by the exact profile / packetization / level that our
        // encoder can produce, so an incompatible H.264 variant is
        // excluded here rather than producing a black stream after the
        // driver drops mismatched frames downstream.
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

        let input_handler =
            gated_input_handler(Arc::clone(&self.backend), input_authorized.clone());

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
        let tile_control_handler = self.build_tile_control_handler(peer_id);

        // Per-peer forwarder drops feed the session's `peer_drops`
        // counter so `DisplayMetricsSnapshot.peer_drops` reflects
        // total drops across all peers. Cheap clone (Arc).
        let drops_counter = Arc::clone(&self.counters.peer_drops);

        let (peer, answer_sdp) = self::webrtc::WebRtcPeer::new(
            peer_id,
            sdp,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            Arc::clone(pool),
            subs,
            lease,
            prefs,
            drops_counter,
        )
        .await?;

        // 3c.4d: pool-feed bridge is spawned eagerly in `start()`,
        // not here. Earlier this call gated bridge spawn on prefs
        // validation + `pool.subscribe` + `WebRtcPeer::new` all
        // succeeding (3c.3b.3a finding) — but the bridge has no
        // dependency on peer presence, and gating it on first-offer
        // success added complexity for no benefit. The bridge runs
        // for the lifetime of the session.

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

        // Open the peer-join burst window on the pool-feed bridge
        // (always running since `start()`, see 3c.4d). The burst is
        // required for codecs that ignore `force_keyframe` on a
        // long-running pipe (Linux ffmpeg H.264) — without it
        // `pool.request_keyframe_all` above can't reach those
        // encoders. See [`Self::signal_peer_join_burst`].
        self.signal_peer_join_burst().await;

        Ok(answer_sdp)
    }

    /// Open the peer-join burst window on the pool-feed bridge.
    ///
    /// Required for codecs that ignore `force_keyframe` on a
    /// long-running pipe (Linux ffmpeg H.264) — `request_keyframe_all`
    /// alone won't reach those encoders, so the burst clocks the
    /// encoder at tick rate so its `-g 30` natural cadence emits a
    /// keyframe inside the window.
    ///
    /// Since 3c.4d's eager bridge spawn from `start()`, the pool-feed
    /// keyframe channel is installed unconditionally for the lifetime
    /// of a started session — the `if let Some` is defense-in-depth
    /// for a session whose `start()` hasn't run yet.
    async fn signal_peer_join_burst(&self) {
        if let Some(tx) = self.pool_feed_keyframe_tx.lock().await.as_ref() {
            let _ = tx.send(());
        }
    }

    async fn spawn_tile_stream_bridge(&self, _fps: u32) {
        if self.tile_stream_handle.lock().await.is_some() {
            return;
        }

        let mut broadcast_rx = self.frame_tx.subscribe();
        let peers = Arc::clone(&self.peers);
        let subscribers = Arc::clone(&self.tile_subscribers);
        let shutdown = self.shutdown.clone();
        let tile_replay = Arc::clone(&self.tile_replay);
        let tile_epoch = Arc::clone(&self.tile_epoch);
        let tile_snapshot_id = Arc::clone(&self.tile_snapshot_id);
        let display_id = self.display_id;
        let marker_flag = Arc::clone(&self.diagnostics_visual_marker);
        let counters = Arc::clone(&self.counters);
        let session_epoch = self.session_epoch;
        let (initial_w, initial_h) = self.backend.resolution();
        let backend_kind = self.backend.kind();

        let task = tokio::spawn(async move {
            let mut damage = make_damage_backend(initial_w, initial_h, backend_kind);
            let mut frame_diff =
                capture::frame_diff::FrameDiffDamageTracker::new(TILE_STREAM_TILE_SIZE_PX);
            let mut grid: Option<tile::grid::TileGrid> = None;
            let mut synthetic_dirty = tile::synthetic_dirty::SyntheticDirtySources::new()
                .with_marker((0, 0), visual_marker::MARKER_W as u32);
            let mut last_cursor: Option<(i32, i32)> = None;
            let mut tile_policy = tile::policy::TilePolicy::new(Instant::now());
            let mut tile_mode = tile::policy::TileMode::Tiles;
            let mut next_snapshot_at = Instant::now() + tile_snapshot_period(tile_mode);
            let mut last_delta_sent_at: Option<Instant> = None;
            let mut seq: u32 = 1;

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    result = broadcast_rx.recv() => {
                        let frame = match result {
                            Ok(frame) => frame,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        };

                        let Some(next_grid) = tile_grid_for_frame(&frame) else {
                            continue;
                        };

                        let peers_now = tile_subscriber_peer_handles(&peers, &subscribers).await;
                        if peers_now.is_empty() {
                            grid = Some(next_grid);
                            tile_policy = tile::policy::TilePolicy::new(Instant::now());
                            tile_mode = tile::policy::TileMode::Tiles;
                            next_snapshot_at = Instant::now() + tile_snapshot_period(tile_mode);
                            last_delta_sent_at = None;
                            continue;
                        }

                        let visual_marker_value =
                            current_visual_marker_value(&marker_flag, session_epoch);

                        let resized = grid.map_or(true, |g| g != next_grid);
                        if resized {
                            let epoch = tile_epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                            seq = 1;
                            grid = Some(next_grid);
                            tile_policy = tile::policy::TilePolicy::new(Instant::now());
                            tile_mode = tile::policy::TileMode::Tiles;
                            last_delta_sent_at = None;
                            synthetic_dirty.reset_cursor();
                            last_cursor = None;
                            let resize = tile::transport::TileFrame::Resize {
                                new_epoch: epoch,
                                grid_w_tiles: next_grid.width_tiles,
                                grid_h_tiles: next_grid.height_tiles,
                                tile_size_px: next_grid.tile_size_px,
                            };
                            send_tile_control_to_peers(&peers_now, resize, "resize").await;
                            let snapshot_id = tile_snapshot_id.fetch_add(1, Ordering::Relaxed);
                            for peer in peers_now {
                                send_tile_snapshot_to_peer(
                                    peer,
                                    Arc::clone(&frame),
                                    epoch,
                                    snapshot_id,
                                    visual_marker_value,
                                    Arc::clone(&counters),
                                ).await;
                            }
                            next_snapshot_at = Instant::now() + tile_snapshot_period(tile_mode);
                            continue;
                        }

                        if Instant::now() >= next_snapshot_at {
                            let epoch = tile_epoch.load(Ordering::Relaxed);
                            let snapshot_id = tile_snapshot_id.fetch_add(1, Ordering::Relaxed);
                            for peer in peers_now {
                                send_tile_snapshot_to_peer(
                                    peer,
                                    Arc::clone(&frame),
                                    epoch,
                                    snapshot_id,
                                    visual_marker_value,
                                    Arc::clone(&counters),
                                ).await;
                            }
                            next_snapshot_at = Instant::now() + tile_snapshot_period(tile_mode);
                            continue;
                        }

                        let cursor_pos = damage.cursor_position();
                        let cursor_changed = cursor_pos.is_some() && cursor_pos != last_cursor;
                        if cursor_changed {
                            last_cursor = cursor_pos;
                        }

                        let mut rects = match damage.capability() {
                            capture::damage::DamageCapability::OsLevel => {
                                match damage.poll_damage() {
                                    Ok(rects) => rects,
                                    Err(e) => {
                                        eprintln!(
                                            "[display/tile] display {display_id} damage poll failed: {e}"
                                        );
                                        Vec::new()
                                    }
                                }
                            }
                            capture::damage::DamageCapability::FrameDiff
                            | capture::damage::DamageCapability::None => {
                                match frame_diff.diff_frame(&frame) {
                                    Ok(rects) => rects,
                                    Err(e) => {
                                        eprintln!(
                                            "[display/tile] display {display_id} frame-diff failed: {e}"
                                        );
                                        Vec::new()
                                    }
                                }
                            }
                        };

                        let policy_dirty = next_grid.dirty_tiles(&rects);
                        let dirty_fraction = next_grid.dirty_fraction(policy_dirty.len());
                        let next_mode = tile_policy.evaluate(dirty_fraction, Instant::now());
                        if next_mode != tile_mode {
                            let epoch = tile_epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                            seq = 1;
                            tile_mode = next_mode;
                            last_delta_sent_at = None;
                            next_snapshot_at = Instant::now() + tile_snapshot_period(tile_mode);
                            match tile_mode {
                                tile::policy::TileMode::Video => {
                                    eprintln!(
                                        "[display/tile] display {display_id} fallback_to_video \
                                         dirty_fraction={dirty_fraction:.3}"
                                    );
                                    let fallback = tile::transport::TileFrame::FallbackToVideo {
                                        new_epoch: epoch,
                                    };
                                    send_tile_control_to_peers(
                                        &peers_now,
                                        fallback,
                                        "fallback-to-video",
                                    ).await;
                                }
                                tile::policy::TileMode::Tiles => {
                                    eprintln!(
                                        "[display/tile] display {display_id} fallback_to_tile \
                                         dirty_fraction={dirty_fraction:.3}"
                                    );
                                    let fallback = tile::transport::TileFrame::FallbackToTile {
                                        new_epoch: epoch,
                                    };
                                    send_tile_control_to_peers(
                                        &peers_now,
                                        fallback,
                                        "fallback-to-tile",
                                    ).await;
                                    let snapshot_id =
                                        tile_snapshot_id.fetch_add(1, Ordering::Relaxed);
                                    for peer in peers_now {
                                        send_tile_snapshot_to_peer(
                                            peer,
                                            Arc::clone(&frame),
                                            epoch,
                                            snapshot_id,
                                            visual_marker_value,
                                            Arc::clone(&counters),
                                        ).await;
                                    }
                                }
                            }
                            continue;
                        }

                        if tile_mode == tile::policy::TileMode::Video {
                            continue;
                        }

                        synthetic_dirty.set_marker_enabled(visual_marker_value.is_some());
                        rects.extend(
                            synthetic_dirty.collect(cursor_pos, visual_marker_value.is_some()),
                        );

                        if cursor_changed {
                            if let Some((x_px, y_px)) = cursor_pos {
                                let cursor = tile::transport::TileFrame::CursorState {
                                    epoch: tile_epoch.load(Ordering::Relaxed),
                                    seq,
                                    x_px,
                                    y_px,
                                    visible: true,
                                };
                                match tile::transport::encode_frame(&cursor) {
                                    Ok(bytes) => {
                                        let peers_now =
                                            tile_subscriber_peer_handles(&peers, &subscribers).await;
                                        for peer in peers_now {
                                            if let Err(e) =
                                                peer.send_tile_control_frame(bytes.clone()).await
                                            {
                                                eprintln!("[display/tile] cursor send failed: {e}");
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("[display/tile] cursor encode failed: {e}");
                                    }
                                }
                            }
                        }

                        if rects.is_empty() {
                            continue;
                        }

                        let dirty: Vec<_> = next_grid.dirty_tiles(&rects).into_iter().collect();
                        if dirty.is_empty() {
                            continue;
                        }
                        counters.record_tile_damage_sample(
                            rects.len(),
                            dirty.len(),
                            next_grid.dirty_fraction(dirty.len()),
                        );

                        let now = Instant::now();
                        if !should_emit_tile_delta(
                            now,
                            last_delta_sent_at,
                            tile_delta_min_interval(),
                        ) {
                            counters.record_tile_delta_cadence_skip();
                            continue;
                        }
                        last_delta_sent_at = Some(now);

                        let epoch = tile_epoch.load(Ordering::Relaxed);
                        let encode_result = tokio::task::spawn_blocking({
                            let frame = Arc::clone(&frame);
                            move || encode_tile_records(&frame, dirty, visual_marker_value)
                        }).await;

                        let Ok(Ok(records)) = encode_result else {
                            eprintln!("[display/tile] update tile encode failed");
                            continue;
                        };
                        let record_count = records.len();

                        let frames = match tile::transport::pack_tile_updates(epoch, seq, records) {
                            Ok(frames) => frames,
                            Err(e) => {
                                eprintln!("[display/tile] update pack failed: {e}");
                                continue;
                            }
                        };
                        seq = seq.wrapping_add(frames.len() as u32);

                        let mut encoded = Vec::with_capacity(frames.len());
                        let frame_count = frames.len();
                        let mut byte_count = 0usize;
                        for frame in frames {
                            let frame_seq = match &frame {
                                tile::transport::TileFrame::TileUpdate { seq, .. } => *seq,
                                _ => continue,
                            };
                            match tile::transport::encode_frame(&frame) {
                                Ok(bytes) => {
                                    byte_count = byte_count.saturating_add(bytes.len());
                                    encoded.push((frame_seq, bytes));
                                }
                                Err(e) => eprintln!("[display/tile] update wire encode failed: {e}"),
                            }
                        }
                        counters.record_tile_delta_source(record_count, frame_count, byte_count);
                        {
                            let mut replay = tile_replay.write().await;
                            let now = Instant::now();
                            for (frame_seq, bytes) in &encoded {
                                replay.push(epoch, *frame_seq, bytes.clone(), now);
                            }
                        }

                        let peers_now = tile_subscriber_peer_handles(&peers, &subscribers).await;
                        for peer in peers_now {
                            for (_, bytes) in &encoded {
                                if let Err(e) = peer.send_tile_delta_frame(bytes.clone()).await {
                                    eprintln!("[display/tile] delta send failed: {e}");
                                }
                            }
                        }
                    }
                }
            }
        });

        *self.tile_stream_handle.lock().await = Some(task);
    }

    /// Spawn the pool-feed bridge — the BGRA→I420 conversion task that
    /// pumps the encoder pool. Called eagerly from [`Self::start`]
    /// after the pool is initialized so the bridge runs unconditionally
    /// for the lifetime of the session, even before any peer connects.
    ///
    /// **Why eager (3c.4d):** the bridge used to spawn lazily on the
    /// first offer (`ensure_pool_feed_bridge_started`, idempotent), but
    /// nothing about the bridge actually depends on a peer being
    /// present — it just feeds the pool. Lazy spawn coupled the bridge
    /// lifecycle to `handle_offer_pool_mode`, requiring an
    /// idempotency check, a `pool.get()` defensive read, and reaching
    /// into `Mutex<Option<_>>` storage for `fps` and `event_bus` that
    /// `start` had set moments earlier. Eager spawn from `start`
    /// removes all of that machinery and gives the bridge the same
    /// lifetime as the capture loop.
    ///
    /// **Why the bridge exists** (unchanged from earlier rationale):
    /// pool-mode peers subscribe to the encoder broadcast directly;
    /// without something pumping I420 frames into the pool the
    /// encoders sit idle and peers see a black stream (3c.3b.3
    /// high-priority finding).
    ///
    /// **Tick + heartbeat pattern.** The bridge caches the latest I420
    /// buffer and forwards it on every tick when the buffer changed OR
    /// the idle heartbeat is due. Required because a damage-driven
    /// capture backend (Wayland especially) emits nothing while the
    /// desktop is idle — a peer joining mid-idle would see black
    /// until the next desktop damage event without it. The heartbeat
    /// re-pushes the latest frame once per second so the encoder's
    /// GOP cadence keeps producing decodable references.
    ///
    /// **Peer-join burst channel.** The bridge listens on
    /// `pool_feed_keyframe_tx` and opens a 1.5s burst window when
    /// signaled. Required because `force_keyframe` on a long-running
    /// ffmpeg-pipe encoder (Linux H.264) is a no-op, so
    /// [`EncoderPool::request_keyframe_all`](crate::display::encode::pool::EncoderPool::request_keyframe_all)
    /// alone can't reach those encoders — the burst clocks the
    /// encoder at tick rate so its `-g 30` natural cadence lands a
    /// keyframe inside the window.
    ///
    /// **`AppEvent::DisplayResize` emission.** The bridge emits the
    /// resize event when the capture backend hands over a frame at a
    /// new resolution. Required so presence / MCP / outbound
    /// listeners learn about display size changes (no other code path
    /// emits these in pool-only sessions).
    ///
    /// **Single-spawn contract.** `start` is the sole caller; this
    /// function does not check whether the bridge is already running.
    /// Calling it twice would spawn duplicate bridges — both feeding
    /// the same pool, both pushing identical I420 frames every tick.
    /// `start` runs at most once per session, so the contract holds
    /// by construction.
    async fn spawn_pool_feed_bridge(
        &self,
        pool: Arc<encode::pool::EncoderPool>,
        fps: u32,
        event_bus: Option<crate::event::EventBus>,
    ) {
        let mut broadcast_rx = self.frame_tx.subscribe();
        let (initial_w, initial_h) = self.backend.resolution();
        let shutdown = self.shutdown.clone();
        let display_id = self.display_id;
        // Phase 0 visual-freshness diagnostic plumbing. Cloned out of
        // self here so the spawned async block (and its inner
        // spawn_blocking closures) can read the flag without
        // re-borrowing self after move. The flag is checked once per
        // converted frame; when off, the cost is a single Relaxed
        // atomic load. When on, the marker stamp adds ~512 byte writes
        // into the Y plane (32 tiles × 16 px wide), dominated by the
        // existing Y-plane construction cost from `bgra_to_i420`.
        let marker_flag = Arc::clone(&self.diagnostics_visual_marker);
        let session_epoch = self.session_epoch;
        let frame_interval =
            std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });
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
        let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
        *self.pool_feed_keyframe_tx.lock().await = Some(kf_tx);
        let task = tokio::spawn(async move {
            // Heartbeat cadence. 1 second strikes the balance
            // between "encoder stays healthy on idle" and "encoder
            // doesn't burn CPU on identical-frame re-encodes."
            // Smaller would re-encode more often; larger would let
            // GOP boundaries drift past a peer-join window on
            // truly static desktops.
            const IDLE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(1);
            // Peer-join burst window. Sized to comfortably exceed
            // one Linux ffmpeg H.264 GOP at 30fps (`-g 30` → ~1s) so
            // the natural keyframe lands inside the window even
            // though `force_keyframe` is ignored on the rawvideo pipe.
            const PEER_JOIN_BURST: std::time::Duration = std::time::Duration::from_millis(1500);

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
                            display_id,
                            pool.dimensions(),
                            frame_w,
                            frame_h,
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
                    // Pass NORMALIZED dims (frame_w / frame_h, computed
                    // above with `& !1`) instead of raw frame.width /
                    // frame.height. Odd raw dims would produce odd-dim
                    // I420 from `bgra_to_i420`'s ceil-chroma sizing,
                    // which then hits the layer-encode `downscale_i420`
                    // path with an unencodable source layout (downscale
                    // requires even source AND dest). The pool was
                    // constructed at the same normalized dims, so
                    // passing odd here would also be a bridge↔pool
                    // dimension desync (silent black-screen class).
                    // Cropping the rightmost column / bottom row at
                    // this stage is invisible at display.
                    let i420_result = tokio::task::spawn_blocking({
                        move || {
                            convert_for_pool_feed(
                                &frame_arc.data,
                                frame_w,
                                frame_h,
                                frame_arc.stride,
                            )
                        }
                    })
                    .await;
                    if let Ok(i420) = i420_result {
                        generation = generation.wrapping_add(1);
                        latest_i420 = Some((Arc::new(i420), arrived));
                    }
                }
            }

            let mut tick = tokio::time::interval(frame_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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

                        // Resize handling. The bridge owns BOTH
                        // `pool.on_resize` AND the
                        // `AppEvent::DisplayResize` emission. Without
                        // the event emit here, presence / MCP /
                        // outbound listeners wouldn't learn about
                        // display size changes (the bridge is the
                        // sole feed since 3c.4b).
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
                        // Same normalized-dims rationale as the seed
                        // path above: pass `frame_w` / `frame_h`
                        // (rounded to even with `& !1` at the top of
                        // this branch) so I420 dims match what
                        // `downscale_i420` and the pool's encoders
                        // expect.
                        let i420_result = tokio::task::spawn_blocking({
                            move || convert_for_pool_feed(
                                &frame_arc.data,
                                frame_w,
                                frame_h,
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

                        let visual_marker_value =
                            if marker_flag.load(Ordering::Relaxed) {
                                // Lower 32 bits of millis since session start.
                                // Wrap horizon is ~49.7 days — irrelevant for
                                // any realistic smoke run; the browser sampler
                                // treats it as a monotonic per-frame token, not
                                // a wall-clock value.
                                Some(
                                    Instant::now()
                                        .saturating_duration_since(session_epoch)
                                        .as_millis()
                                        as u32,
                                )
                            } else {
                                None
                            };
                        pool.push_i420_frame_with_visual_marker(
                            Arc::clone(i420),
                            arrived,
                            visual_marker_value,
                        );
                        last_sent_gen = Some(generation);
                        last_send_at = Instant::now();
                    }
                }
            }
        });
        *self.pool_feed_bridge_handle.lock().await = Some(task);
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
        self.unregister_tile_subscriber(peer_id).await;
        if let Some(peer) = self.peers.write().await.remove(&peer_id) {
            self.counters.peer_count.fetch_sub(1, Ordering::Relaxed);
            peer.close().await;
        }
    }

    /// F-1.3b3: fetch a clone of the per-peer
    /// [`self::webrtc::WebRtcPeer`] handle for gateway-side wiring
    /// after [`Self::handle_offer`]. Returns `None` if the peer has
    /// been removed since the offer was processed (e.g., a fast
    /// `WebRtcSignal::Close` raced the post-offer registration).
    ///
    /// Used by the federated authority subscriber registration to
    /// bind the per-peer authority data-channel push target. The
    /// `Arc` clone is cheap; callers should not hold the returned
    /// handle across awaits in latency-sensitive paths because it
    /// keeps the peer alive past `remove_peer`.
    pub async fn get_peer(&self, peer_id: PeerId) -> Option<Arc<self::webrtc::WebRtcPeer>> {
        self.peers.read().await.get(&peer_id).cloned()
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
// SessionRegistry
// ---------------------------------------------------------------------------

/// Registry of active display sessions, keyed by display ID.
pub struct SessionRegistry {
    sessions: HashMap<u32, Arc<DisplaySession>>,
    diagnostics_visual_marker_defaults: HashMap<u32, bool>,
}

pub type SharedSessionRegistry = Arc<RwLock<SessionRegistry>>;

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            diagnostics_visual_marker_defaults: HashMap::new(),
        }
    }

    pub fn get(&self, display_id: u32) -> Option<Arc<DisplaySession>> {
        self.sessions.get(&display_id).cloned()
    }

    pub fn insert(&mut self, display_id: u32, session: Arc<DisplaySession>) {
        if let Some(enabled) = self
            .diagnostics_visual_marker_defaults
            .get(&display_id)
            .copied()
        {
            session.set_diagnostics_visual_marker(enabled);
        }
        self.sessions.insert(display_id, session);
    }

    pub fn remove(&mut self, display_id: u32) -> Option<Arc<DisplaySession>> {
        self.sessions.remove(&display_id)
    }

    /// All active display IDs.
    pub fn display_ids(&self) -> Vec<u32> {
        self.sessions.keys().copied().collect()
    }

    /// Set the Phase 0 visual-freshness marker for an active display, or
    /// remember the desired state for the next session created for that
    /// display. The smoke harness intentionally arms the marker before
    /// opening the federated display; applying the pending default here
    /// makes that ordering reliable.
    ///
    /// Returns `true` when an active session was updated immediately.
    pub fn set_diagnostics_visual_marker(&mut self, display_id: u32, enabled: bool) -> bool {
        self.diagnostics_visual_marker_defaults
            .insert(display_id, enabled);
        if let Some(session) = self.sessions.get(&display_id) {
            session.set_diagnostics_visual_marker(enabled);
            true
        } else {
            false
        }
    }
}

/// Build the per-peer data-channel input handler used by
/// [`DisplaySession::handle_offer_pool_mode`].  The handler:
///
/// 1. consults the `input_authorized` closure (Phase 5a.1 gate) and
///    silently drops the event if it returns `false`;
/// 2. otherwise spawns the existing async `inject_input` dispatch onto
///    the tokio runtime.
///
/// Extracted so the gate logic is unit-testable without standing up a
/// full `DisplaySession` + `WebRtcPeer` + offer.  Keeping it free-
/// standing rather than a method on `DisplaySession` means tests can
/// build it from any backend stub in the test module without going
/// through the offer plumbing.
///
/// The closure is `Arc<dyn Fn(InputEvent) + Send + Sync>`, sync-callable
/// from rtc's data-channel receive context (which is sync); the async
/// injection is `tokio::spawn`-ed because `DisplayBackend::inject_input`
/// is async.  This shape predates phase 5a.1 — the gate just sits in
/// front of it.
pub(crate) fn gated_input_handler(
    backend: Arc<dyn DisplayBackend>,
    input_authorized: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Arc<dyn Fn(InputEvent) + Send + Sync> {
    Arc::new(move |event: InputEvent| {
        // Phase 5a.1 authority gate: silent drop when the per-peer
        // closure says no, matching the `/ws display_input` gate
        // convention (no per-message denial feedback; the browser
        // already learned it's not the holder via the
        // `display_input_authority_state` notification).  Unclaimed
        // and "this peer holds it" both resolve to `true` — see the
        // closure builder in `web_gateway::spawn_web_gateway`.
        if !input_authorized() {
            return;
        }
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            if let Err(e) = backend.inject_input(event).await {
                eprintln!("[display] input injection failed: {e}");
            }
        });
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn tile_damage_uses_xdamage_only_for_x11_backend() {
        assert!(should_try_xdamage_for_tile_stream("x11"));
        assert!(!should_try_xdamage_for_tile_stream("wayland"));
        assert!(!should_try_xdamage_for_tile_stream("macos"));
        assert!(!should_try_xdamage_for_tile_stream("stub"));
    }

    #[test]
    fn input_event_deserialize_key_down() {
        let json = r#"{"t":"kd","code":"KeyA","key":"a","shift":false,"ctrl":false,"alt":false,"meta":false}"#;
        let evt: InputEvent = serde_json::from_str(json).unwrap();
        match evt {
            InputEvent::KeyDown {
                code,
                key,
                shift,
                ctrl,
                alt,
                meta,
            } => {
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
        assert_eq!(
            config.ice_servers[0].urls[0],
            "stun:stun.l.google.com:19302"
        );
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
        let reg = SessionRegistry::new();
        assert!(reg.display_ids().is_empty());
    }

    #[test]
    fn metrics_counters_init_zeroed() {
        let c = DisplayMetricsCounters::new();
        assert_eq!(c.capture_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.capture_drops.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_drops.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_freshness_us_sum.load(Ordering::Relaxed), 0);
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
        c.encode_freshness_us_sum.store(700_000, Ordering::Relaxed);
        c.peer_drops.store(3, Ordering::Relaxed);
        c.peer_count.store(2, Ordering::Relaxed);
        c.record_tile_damage_sample(2, 5, 0.25);
        c.record_tile_damage_sample(4, 7, 0.50);
        c.record_tile_delta_cadence_skip();
        c.record_tile_delta_source(12, 3, 15_000);
        c.record_tile_snapshot_source(20, 4, 30_000);

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
        assert!((snap.encode_freshness_avg_ms - 5.0).abs() < 0.01);
        assert_eq!(snap.peer_count, 2);
        assert_eq!(snap.peer_drops, 3);
        assert_eq!(snap.tile_damage_samples, 2);
        assert_eq!(snap.tile_dirty_rects, 6);
        assert_eq!(snap.tile_dirty_tiles, 12);
        assert!((snap.tile_dirty_fraction_avg - 0.375).abs() < 0.001);
        assert_eq!(snap.tile_delta_cadence_skips, 1);
        assert_eq!(snap.tile_delta_records, 12);
        assert!((snap.tile_delta_fps - 0.6).abs() < 0.1);
        assert!((snap.tile_delta_kbps - 24.0).abs() < 1.0);
        assert_eq!(snap.tile_snapshot_records, 20);
        assert_eq!(snap.tile_snapshot_frames, 4);
        assert!((snap.tile_snapshot_kbps - 48.0).abs() < 1.0);

        // Counters should be reset after snapshot (except peer_count which is gauge).
        assert_eq!(c.capture_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.encode_frames.load(Ordering::Relaxed), 0);
        assert_eq!(c.tile_delta_records.load(Ordering::Relaxed), 0);
        assert_eq!(c.tile_snapshot_records.load(Ordering::Relaxed), 0);
        assert_eq!(c.peer_count.load(Ordering::Relaxed), 2); // gauge, not reset
    }

    #[test]
    fn metrics_snapshot_zero_frames() {
        let c = DisplayMetricsCounters::new();
        let epoch = Instant::now() - std::time::Duration::from_secs(5);
        let snap = DisplayMetricsSnapshot::from_counters(&c, 0, (640, 480), &epoch);
        assert!((snap.capture_fps - 0.0).abs() < f64::EPSILON);
        assert!((snap.encode_fps - 0.0).abs() < f64::EPSILON);
        assert!((snap.encode_freshness_avg_ms - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_snapshot_serializes() {
        let snap = DisplayMetricsSnapshot {
            display_id: 1,
            capture_fps: 30.0,
            capture_drops: 5,
            encode_fps: 28.5,
            encode_freshness_avg_ms: 4.2,
            encode_drops: 2,
            peer_count: 1,
            peer_drops: 0,
            resolution: (1920, 1080),
            tile_damage_samples: 3,
            tile_dirty_rects: 4,
            tile_dirty_tiles: 5,
            tile_dirty_fraction_avg: 0.25,
            tile_delta_cadence_skips: 6,
            tile_delta_records: 7,
            tile_delta_fps: 8.0,
            tile_delta_kbps: 9.0,
            tile_snapshot_records: 10,
            tile_snapshot_frames: 11,
            tile_snapshot_kbps: 12.0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"display_id\":1"));
        assert!(json.contains("\"capture_fps\":30.0"));
        assert!(json.contains("\"encode_drops\":2"));
        assert!(json.contains("\"tile_delta_records\":7"));
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
        async fn start_capture(&self, _fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
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

    #[test]
    fn tile_bgra_visual_marker_stamp_writes_expected_marker_pixels() {
        let tile_size = TILE_STREAM_TILE_SIZE_PX as usize;
        let sample = |buf: &[u8], x: usize, y: usize| -> [u8; 4] {
            let i = (y * tile_size + x) * 4;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        let bit_center = |tile: tile::grid::TileId, bit_idx: usize| -> Option<(usize, usize)> {
            let col = bit_idx % visual_marker::COLS;
            let row = bit_idx / visual_marker::COLS;
            let global_x = col * visual_marker::TILE_PX + (visual_marker::TILE_PX / 2);
            let global_y = row * visual_marker::TILE_PX + (visual_marker::TILE_PX / 2);
            let tile_x0 = tile.x as usize * tile_size;
            let tile_y0 = tile.y as usize * tile_size;
            if global_x < tile_x0
                || global_x >= tile_x0 + tile_size
                || global_y < tile_y0
                || global_y >= tile_y0 + tile_size
            {
                return None;
            }
            Some((global_x - tile_x0, global_y - tile_y0))
        };

        let mut tile0 = vec![0u8; tile_size * tile_size * 4];
        let value = 1u32 << 0;
        let tile0_id = tile::grid::TileId::new(0, 0);
        stamp_visual_marker_bgra_tile(&mut tile0, tile0_id, TILE_STREAM_TILE_SIZE_PX, value);
        let (bit0_x, bit0_y) = bit_center(tile0_id, 0).expect("bit 0 in tile 0");
        assert_eq!(
            sample(&tile0, bit0_x, bit0_y),
            [
                visual_marker::LUMA_HIGH,
                visual_marker::LUMA_HIGH,
                visual_marker::LUMA_HIGH,
                255,
            ],
            "bit 0 center should stamp high luma in tile 0"
        );
        let (bit1_x, bit1_y) = bit_center(tile0_id, 1).expect("bit 1 in tile 0");
        assert_eq!(
            sample(&tile0, bit1_x, bit1_y),
            [
                visual_marker::LUMA_LOW,
                visual_marker::LUMA_LOW,
                visual_marker::LUMA_LOW,
                255,
            ],
            "bit 1 center should stamp low luma in tile 0"
        );
        let mut tile1 = vec![0u8; tile_size * tile_size * 4];
        let tile1_id = tile::grid::TileId::new(1, 0);
        let tile1_bits: Vec<_> = (0..visual_marker::COLS * visual_marker::ROWS)
            .filter(|bit| bit_center(tile1_id, *bit).is_some())
            .collect();
        assert!(tile1_bits.len() >= 2);
        let first_tile1_bit = tile1_bits[0];
        let second_tile1_bit = tile1_bits[1];
        stamp_visual_marker_bgra_tile(
            &mut tile1,
            tile1_id,
            TILE_STREAM_TILE_SIZE_PX,
            (1u32 << first_tile1_bit) | (1u32 << second_tile1_bit),
        );
        let (first_x, first_y) =
            bit_center(tile1_id, first_tile1_bit).expect("first tile 1 bit in tile 1");
        assert_eq!(
            sample(&tile1, first_x, first_y),
            [
                visual_marker::LUMA_HIGH,
                visual_marker::LUMA_HIGH,
                visual_marker::LUMA_HIGH,
                255,
            ],
            "first covered marker bit should stamp high luma in tile 1"
        );
        let (second_x, second_y) =
            bit_center(tile1_id, second_tile1_bit).expect("second tile 1 bit in tile 1");
        assert_eq!(
            sample(&tile1, second_x, second_y),
            [
                visual_marker::LUMA_HIGH,
                visual_marker::LUMA_HIGH,
                visual_marker::LUMA_HIGH,
                255,
            ],
            "second covered marker bit should stamp high luma in tile 1"
        );

        let mut tile2 = vec![0u8; tile_size * tile_size * 4];
        stamp_visual_marker_bgra_tile(
            &mut tile2,
            tile::grid::TileId::new(2, 0),
            TILE_STREAM_TILE_SIZE_PX,
            u32::MAX,
        );
        assert!(
            tile2.iter().all(|&b| b == 0),
            "tile outside marker bounds should remain untouched"
        );
    }

    #[test]
    fn tile_delta_cadence_caps_to_target_interval() {
        let now = Instant::now();
        let min = Duration::from_millis(66);

        assert!(should_emit_tile_delta(now, None, min));
        assert!(!should_emit_tile_delta(
            now + Duration::from_millis(65),
            Some(now),
            min
        ));
        assert!(should_emit_tile_delta(
            now + Duration::from_millis(66),
            Some(now),
            min
        ));
    }

    #[test]
    fn session_registry_applies_pending_visual_marker_default_on_insert() {
        let mut reg = SessionRegistry::new();
        assert!(
            !reg.set_diagnostics_visual_marker(7, true),
            "setting marker before display exists should be recorded as pending"
        );

        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = Arc::new(DisplaySession::new(7, backend));
        assert!(
            !session.diagnostics_visual_marker_enabled(),
            "new sessions start with marker disabled before registry insertion"
        );

        reg.insert(7, Arc::clone(&session));
        assert!(
            session.diagnostics_visual_marker_enabled(),
            "registry insertion should apply the pending marker default"
        );

        assert!(
            reg.set_diagnostics_visual_marker(7, false),
            "setting marker after display exists should update active session"
        );
        assert!(
            !session.diagnostics_visual_marker_enabled(),
            "active session should reflect updated marker default"
        );
    }

    /// `DisplayBackend` that records whether `stop_capture` was called.
    ///
    /// `StubBackend`'s `start_capture` returns an immediately-dropped
    /// sender, so the capture bridge exits cleanly on its own and the
    /// fail-loud test can't tell whether `start()` cleaned up the
    /// backend or just leaked the in-flight capture state. Real
    /// backends (X11Backend in particular) spawn a `std::thread` that
    /// only exits on explicit `stop_capture` — phase 4b's fail-loud
    /// guard MUST call it before returning Err, otherwise the thread
    /// runs forever after the session reports failure.
    ///
    /// Used by `display_session_start_fails_loud_on_source_too_small_for_vp8`
    /// to assert the cleanup contract directly.
    struct CleanupTrackingBackend {
        width: u32,
        height: u32,
        stop_capture_called: AtomicBool,
    }

    #[async_trait]
    impl DisplayBackend for CleanupTrackingBackend {
        async fn start_capture(&self, _fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {
            self.stop_capture_called.store(true, Ordering::SeqCst);
        }
        async fn inject_input(&self, _event: InputEvent) -> Result<(), CallerError> {
            Ok(())
        }
        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }
        fn kind(&self) -> &'static str {
            "cleanup-tracking"
        }
    }

    /// **#48 test helper**: register a fake peer in `session.peers`
    /// whose negotiated `active_rids` is the full VP8 simulcast set
    /// (`f`, `h`, `q`). Tests that exercise the pool-feed bridge →
    /// encoder → consumer pipeline directly need this so the
    /// per-tick layer-policy doesn't observe `current_peers.is_empty()`,
    /// compute `demanded = empty`, and pause every encoder before the
    /// test's first frame can flow.
    ///
    /// Pre-#48 the layer-policy's per-tick output during a no-peers
    /// window stayed at `current_rids` (presence_active=true via
    /// PAUSE_DEBOUNCE), so these tests implicitly relied on a 5s
    /// window where all encoders ran. With #48 the demanded-bound
    /// fires immediately on `current_peers.is_empty()` — correct
    /// production behavior, but breaks tests that don't model peers.
    ///
    /// Insert the peer BEFORE `session.start()` so the layer-policy's
    /// first tick (immediate after spawn) sees the peer.
    ///
    /// Only used by the VP8-simulcast bridge tests, which are gated off
    /// Windows; gate the helper too so it isn't dead code there.
    #[cfg(not(target_os = "windows"))]
    async fn register_test_peer_demanding_all_layers(session: &DisplaySession) {
        use crate::display::encode::pool::SimulcastRid;
        use crate::display::webrtc::WebRtcPeer;
        let peer = Arc::new(WebRtcPeer::new_for_test(
            1u64,
            vec![
                SimulcastRid::full(),
                SimulcastRid::half(),
                SimulcastRid::quarter(),
            ],
        ));
        session.peers.write().await.insert(1u64, peer);
    }

    /// Before `start()` runs, the pool is uninitialized. `get()` returning
    /// `None` is the contract other phases rely on to know whether the
    /// session is hot (bridge running, pool ready) or still cold.
    #[test]
    fn display_session_pool_uninitialized_before_start() {
        let backend = Arc::new(StubBackend {
            width: 640,
            height: 480,
        });
        let session = DisplaySession::new(0, backend);
        assert!(
            session.pool.get().is_none(),
            "pool must be uninitialized until start() runs"
        );
    }

    /// **Phase 4b**: `start()` constructs the pool with the full VP8
    /// simulcast layer set (full / half / quarter). Pre-4b the factory
    /// returned a single layer; post-4b it returns the canonical
    /// `LayerSpec::vp8_simulcast(w, h, fps)` layout. Each surviving
    /// layer spawns its own encoder thread immediately.
    ///
    /// This test pins:
    ///   1. The pool is populated after `start()`.
    ///   2. For a typical capture dim (640×480, ≥ MIN_LAYER_DIM on
    ///      every divisor), all three layers spawn — full, half,
    ///      quarter — at the expected dims and RIDs.
    ///   3. Layer ordering matches `vp8_simulcast`'s canonical order
    ///      (full → half → quarter), so the receive-side picker can
    ///      rely on the highest-quality layer being index 0.
    ///
    /// Neither the bridge nor any peer is wired to the pool yet, so
    /// this test confirms construction lifetime only — not frame flow.
    /// (See `pool_always_on_layers_all_subscribe_to_i420_after_start`
    /// for the end-to-end frame-flow test.)
    ///
    /// VP8-specific (gated off Windows): the layer set, codec identity,
    /// and three-layer ordering are the VP8-simulcast baseline. On
    /// Windows the baseline is a single H.264 layer — see
    /// `display_session_start_initializes_pool_with_single_h264_layer`.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn display_session_start_initializes_pool_with_vp8_simulcast() {
        let backend = Arc::new(StubBackend {
            width: 640,
            height: 480,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");
        let pool = session
            .pool
            .get()
            .expect("pool must be initialized after start()");
        let always_on = pool.always_on();
        assert_eq!(
            always_on.len(),
            3,
            "VP8 simulcast spawns three always-on layers for a 640×480 source"
        );
        // Order is full / half / quarter — matches `vp8_simulcast`'s
        // canonical layout. Pinning order so receive-side layer picking
        // can index by position.
        assert_eq!(
            always_on[0].id.codec,
            encode::pool::CodecKind::Vp8,
            "all simulcast layers are VP8"
        );
        assert_eq!(
            always_on[0].id.rid,
            encode::pool::SimulcastRid::full(),
            "layer 0 is the full-resolution RID"
        );
        assert_eq!(
            (always_on[0].layer.width, always_on[0].layer.height),
            (640, 480)
        );
        assert_eq!(
            always_on[1].id.rid,
            encode::pool::SimulcastRid::half(),
            "layer 1 is the half-resolution RID"
        );
        assert_eq!(
            (always_on[1].layer.width, always_on[1].layer.height),
            (320, 240)
        );
        assert_eq!(
            always_on[2].id.rid,
            encode::pool::SimulcastRid::quarter(),
            "layer 2 is the quarter-resolution RID"
        );
        assert_eq!(
            (always_on[2].layer.width, always_on[2].layer.height),
            (160, 120)
        );
    }

    /// Windows counterpart to
    /// `display_session_start_initializes_pool_with_vp8_simulcast`. On
    /// Windows the always-on baseline is a **single full-resolution
    /// H.264 layer** (VP8/libvpx is gated off — see
    /// `encode::pool::BASELINE_CODEC`), not VP8 simulcast. A 640×480
    /// source (well above MIN_LAYER_DIM, comfortably within the MS H.264
    /// software MFT's accepted range) yields exactly one always-on
    /// encoder at the full RID and full source dims.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn display_session_start_initializes_pool_with_single_h264_layer() {
        let backend = Arc::new(StubBackend {
            width: 640,
            height: 480,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");
        let pool = session
            .pool
            .get()
            .expect("pool must be initialized after start()");
        let always_on = pool.always_on();
        assert_eq!(
            always_on.len(),
            1,
            "Windows baseline spawns a single H.264 always-on layer for a 640×480 source"
        );
        assert_eq!(
            always_on[0].id.codec,
            encode::pool::CodecKind::H264,
            "the Windows always-on baseline codec is H.264"
        );
        assert_eq!(
            always_on[0].id.rid,
            encode::pool::SimulcastRid::full(),
            "the single H.264 layer uses the full-resolution RID"
        );
        assert_eq!(
            (always_on[0].layer.width, always_on[0].layer.height),
            (640, 480),
            "the single layer covers the full source resolution"
        );
    }

    /// **Phase 4b** (post-review fix): `start()` must reject source
    /// dims for which `vp8_simulcast` produces no encodable layers AND
    /// must clean up the backend's capture state before returning
    /// Err. The pool constructor itself accepts an empty always-on
    /// set (many unit tests rely on this for on-demand-only flows),
    /// but production always-on must be guaranteed — an empty result
    /// means the session has no media path and would silently produce
    /// no frames.
    ///
    /// The cleanup half of the contract was added in response to
    /// review: pre-fix the validation ran AFTER the capture bridge
    /// had already been spawned, leaving the backend's capture
    /// thread running after `start()` reported failure (X11Backend's
    /// thread ignores send-on-dropped-rx and only exits on
    /// `stop_capture`). `CleanupTrackingBackend` records whether
    /// `stop_capture` was called so this test can assert the cleanup
    /// directly — `StubBackend`'s no-op `stop_capture` would let a
    /// regression here pass silently.
    ///
    /// Source dims < MIN_LAYER_DIM (16) on either axis trigger the
    /// fail-loud path: `vp8_simulcast`'s `normalize_layer_dims`
    /// filter drops every layer the divisor produces, including the
    /// full layer.
    #[tokio::test]
    async fn display_session_start_fails_loud_and_cleans_up_on_source_too_small() {
        // 14×14 — both dims below MIN_LAYER_DIM=16, every layer
        // (full / half / quarter) drops, vp8_simulcast returns empty.
        let backend = Arc::new(CleanupTrackingBackend {
            width: 14,
            height: 14,
            stop_capture_called: AtomicBool::new(false),
        });
        let session = DisplaySession::new(0, backend.clone());
        let err = session
            .start(30, None, None)
            .await
            .expect_err("start() must fail for source too small for VP8 simulcast");
        match err {
            CallerError::Display(msg) => {
                assert!(
                    msg.contains("too small") && msg.contains("14x14"),
                    "error must name the raw source dims; got: {msg}"
                );
                assert!(
                    msg.contains("MIN_LAYER_DIM"),
                    "error must mention MIN_LAYER_DIM for diagnostic clarity; got: {msg}"
                );
            }
            other => panic!("expected CallerError::Display, got {other:?}"),
        }
        // Pool must NOT have been constructed — fail-loud means no
        // half-initialized state is left behind for the caller to
        // accidentally use.
        assert!(
            session.pool.get().is_none(),
            "pool must remain uninitialized when start() fails"
        );
        // Backend's capture state must have been torn down — leaving a
        // backend thread running after start() reports failure is the
        // leak class the validation reorder was meant to prevent.
        assert!(
            backend.stop_capture_called.load(Ordering::SeqCst),
            "fail-loud must call backend.stop_capture() to undo the \
             start_capture() call from earlier in start(); without \
             this, real backends (X11Backend) leak their capture \
             thread."
        );
    }

    /// **Phase 4b** (post-review fix): odd source dimensions must be
    /// normalized to even before reaching the pool / bridge / encoder
    /// chain. VP8 encoder construction (vp8.rs) and `downscale_i420`
    /// (encode/mod.rs) both require even dims, but `vp8_simulcast`'s
    /// `normalize_layer_dims` filter applies `& !1` only to the LAYER
    /// dims it returns — not to the source dim those layers were
    /// derived from. Without explicit normalization in
    /// `DisplaySession::start`, an odd source like 17×480 would land
    /// in the pool as `dimensions() == (17, 480)` (raw) but the layer
    /// factory would emit a 16×480 full layer. The bridge would then
    /// pass odd-raw-dim BGRA buffers into `bgra_to_i420`, producing
    /// odd-dim I420, which `downscale_i420` rejects (debug-asserts
    /// even dims) — silent black-screen class.
    ///
    /// Pin: a 17×480 source produces a pool with even `dimensions()`
    /// (16×480 after normalization) and exactly one surviving
    /// always-on simulcast layer (the full layer at 16×480, since
    /// half/quarter at 8/4 width drop below MIN_LAYER_DIM).
    ///
    /// VP8-specific (gated off Windows): the "only the full layer
    /// survives" outcome is a property of `vp8_simulcast`'s per-layer
    /// MIN_LAYER_DIM filter, which the Windows single-H.264-layer
    /// factory doesn't have. The 16-px-wide H.264 case would also stress
    /// the MS MFT's minimum frame size, which is orthogonal to the
    /// odd-dim-normalization behavior under test. The source-dim
    /// even-normalization itself is platform-agnostic and already
    /// exercised by the pool-level dimension tests.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn display_session_start_normalizes_odd_source_dims_in_pool() {
        // 17×480 — width is odd, so `& !1` rounds to 16. Half would
        // be 8×240 (width below MIN_LAYER_DIM → drop). Quarter would
        // be 4×120 (drop). So only the full layer at 16×480 survives.
        let backend = Arc::new(StubBackend {
            width: 17,
            height: 480,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed for 17×480 (normalizes to 16×480)");
        let pool = session
            .pool
            .get()
            .expect("pool must be initialized after start()");
        assert_eq!(
            pool.dimensions(),
            (16, 480),
            "pool dims must be normalized (raw 17×480 → even 16×480), \
             NOT raw — bridge passes these dims through to bgra_to_i420 \
             and downscale_i420, both of which require even."
        );
        let always_on = pool.always_on();
        assert_eq!(
            always_on.len(),
            1,
            "only the full simulcast layer survives at 16×480 (half=8×240 \
             and quarter=4×120 both have a dim below MIN_LAYER_DIM=16)"
        );
        assert_eq!(
            (always_on[0].layer.width, always_on[0].layer.height),
            (16, 480),
            "surviving full layer matches the normalized source dims"
        );
    }

    /// **Phase 4b**: every always-on simulcast layer must be subscribed
    /// to the pool's I420 broadcast the moment `start()` returns.
    /// Pre-4b only the single full layer was spawned, so the existing
    /// `pool_always_on_encoder_subscribed_to_i420_after_start` test
    /// asserted `subscriber_count >= 1`. Post-4b we have three layers,
    /// and any one of them being unsubscribed is a silent-black-screen
    /// regression for the peers that pick that RID. Tightening the
    /// assertion to require exactly the always-on count.
    ///
    /// VP8-specific (gated off Windows): only the hardcoded
    /// three-layer precondition is VP8-simulcast. The underlying
    /// invariant — `push_i420_frame` reaches *every* always-on encoder
    /// — is platform-agnostic and asserted on Windows by
    /// `pool_single_h264_layer_subscribes_to_i420_after_start`.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_always_on_layers_all_subscribe_to_i420_after_start() {
        let backend = Arc::new(StubBackend {
            width: 640,
            height: 480,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");
        let pool = session.pool.get().expect("pool initialized after start()");
        let expected_count = pool.always_on().len();
        assert_eq!(
            expected_count, 3,
            "precondition: 640×480 source spawns 3 simulcast layers"
        );

        // 640x480 I420 frame — what the bridge will push during
        // production operation. Each always-on encoder needs its
        // own subscription receiver; the encoder thread does the
        // per-layer downscale (4a) before encode, so source-dim
        // I420 is the correct shape to push.
        let i420 = Arc::new(vec![0u8; 640 * 480 * 3 / 2]);
        let subscriber_count = pool.push_i420_frame(i420, Instant::now());
        assert_eq!(
            subscriber_count, expected_count,
            "pool.push_i420_frame must deliver to every always-on \
             encoder ({expected_count} subscribers expected); got \
             {subscriber_count}. If this is < {expected_count}, one \
             or more simulcast layers is not wired to the i420 \
             broadcast — silent-black-screen regression for any peer \
             that picks the missing RID."
        );
    }

    /// Windows counterpart to
    /// `pool_always_on_layers_all_subscribe_to_i420_after_start`. The
    /// single H.264 always-on layer must be subscribed to the pool's
    /// I420 broadcast the moment `start()` returns, so the bridge's
    /// first `push_i420_frame` reaches it. Same "every always-on encoder
    /// is wired" invariant, single-layer shape: exactly one subscriber.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn pool_single_h264_layer_subscribes_to_i420_after_start() {
        let backend = Arc::new(StubBackend {
            width: 640,
            height: 480,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");
        let pool = session.pool.get().expect("pool initialized after start()");
        let expected_count = pool.always_on().len();
        assert_eq!(
            expected_count, 1,
            "precondition: 640×480 source spawns one H.264 always-on layer on Windows"
        );

        let i420 = Arc::new(vec![0u8; 640 * 480 * 3 / 2]);
        let subscriber_count = pool.push_i420_frame(i420, Instant::now());
        assert_eq!(
            subscriber_count, expected_count,
            "pool.push_i420_frame must deliver to the single always-on \
             H.264 encoder ({expected_count} subscriber expected); got \
             {subscriber_count}. If 0, the always-on layer is not wired \
             to the i420 broadcast — silent-black-screen regression."
        );
    }

    /// **Phase 4b**: real-dim resize through 1366×768 must regenerate
    /// the simulcast layer set via the pool's stored factory, NOT by
    /// rescaling the previous epoch's handles. The pool-level
    /// `vp8_simulcast_normalizes_odd_layer_dims_for_1366x768` test
    /// already pins the layer arithmetic; this DisplaySession-level
    /// smoke test confirms the production `start()` path wires the
    /// factory through correctly so a typical-laptop resize
    /// (e.g. 1280×720 → 1366×768 → 1280×720) ends up at canonical
    /// dims at every step.
    ///
    /// Pre-4b this would have been a no-op test (single layer at
    /// source dim, which trivially regenerates). Post-4b each resize
    /// re-derives 3 layers from canonical inputs — a regression that
    /// stored-then-rescaled the previous layout would land here as
    /// "half/quarter dims drift by one pixel after a series of
    /// odd-dim resizes."
    ///
    /// VP8-specific (gated off Windows): the half/quarter drift this
    /// pins only exists for VP8 simulcast. The Windows factory produces
    /// a single full-res H.264 layer with no derived dims to drift, and
    /// the factory-regenerates-on-resize contract itself is already
    /// covered platform-agnostically by the `pool::tests` `on_resize`
    /// suite.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_resize_through_1366x768_yields_canonical_layout_at_every_step() {
        let backend = Arc::new(StubBackend {
            width: 1280,
            height: 720,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");
        let pool = session.pool.get().expect("pool initialized after start()");

        // Step 1: starting dims 1280×720 — even on every divisor,
        // canonical layout is the trivial case.
        let layers = pool.always_on();
        assert_eq!(
            [
                (layers[0].layer.width, layers[0].layer.height),
                (layers[1].layer.width, layers[1].layer.height),
                (layers[2].layer.width, layers[2].layer.height)
            ],
            [(1280, 720), (640, 360), (320, 180)],
            "1280×720 source produces canonical even-dim simulcast"
        );
        drop(layers); // release read guard before mutating via on_resize

        // Step 2: resize to 1366×768 — half=683 (odd → 682), quarter=341 (odd → 340).
        // Pre-4a-fix-#3 a rescale-from-handles path could have produced
        // drift here; post-fix the factory re-derives from canonical
        // 1366×768 inputs.
        pool.on_resize(1366, 768);
        let layers = pool.always_on();
        assert_eq!(
            [
                (layers[0].layer.width, layers[0].layer.height),
                (layers[1].layer.width, layers[1].layer.height),
                (layers[2].layer.width, layers[2].layer.height)
            ],
            [(1366, 768), (682, 384), (340, 192)],
            "1366×768 resize must re-derive layout from canonical inputs \
             (half=682×384 NOT drift-derived 682-or-some-other-value)"
        );
        drop(layers);

        // Step 3: resize back to 1280×720 — must restore the trivial
        // even-dim layout, NOT the drift-accumulated dims that would
        // result from rescaling the 1366×768 layout. This is the
        // 4a-fix-#3 contract: factory regeneration eliminates the
        // round-trip drift bug class.
        pool.on_resize(1280, 720);
        let layers = pool.always_on();
        assert_eq!(
            [
                (layers[0].layer.width, layers[0].layer.height),
                (layers[1].layer.width, layers[1].layer.height),
                (layers[2].layer.width, layers[2].layer.height)
            ],
            [(1280, 720), (640, 360), (320, 180)],
            "resize back to 1280×720 must restore canonical layout, not \
             accumulate drift from the intermediate 1366×768 epoch"
        );
    }

    // (Phase 3c.2's `pool_always_on_encoder_subscribed_to_i420_after_start`
    //  was superseded by phase 4b's `pool_always_on_layers_all_subscribe
    //  _to_i420_after_start` above, which tightens the assertion from
    //  `>= 1` to exact-count and so catches partial-simulcast-subscribe
    //  regressions the old test couldn't.)

    // -----------------------------------------------------------------------
    // Phase 3c.3b.3a: pool-feed bridge — does NOT lock legacy codec
    // -----------------------------------------------------------------------

    /// 3c.4d: `start()` spawns the pool-feed bridge eagerly, before
    /// any offer is served. Pre-3c.4d the bridge was lazy and only
    /// appeared after the first `handle_offer_pool_mode` call —
    /// regression here would mean a peer subscribing before the
    /// first offer never sees pool encoder output (the bridge would
    /// be missing, encoders would sit idle, peer would see black).
    /// Pin both the eager spawn AND the keyframe channel install,
    /// since the burst signal at the tail of `handle_offer_pool_mode`
    /// silently drops on a missing channel — a regression there would
    /// not fail any other test.
    #[tokio::test]
    async fn pool_feed_bridge_spawned_eagerly_in_start() {
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start() must succeed");

        // Bridge handle must be Some — start() owns the spawn; no
        // offer has been served.
        assert!(
            session.pool_feed_bridge_handle.lock().await.is_some(),
            "start() must spawn pool-feed bridge eagerly (3c.4d)"
        );

        // Keyframe channel must be installed — peer-join burst
        // signaling depends on it.
        assert!(
            session.pool_feed_keyframe_tx.lock().await.is_some(),
            "start() must install pool_feed_keyframe_tx so the \
             peer-join burst signal at the tail of \
             handle_offer_pool_mode reaches the bridge (3c.4d)"
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
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(7, backend);
        let bus = crate::event::EventBus::new();
        let mut bus_rx = bus.subscribe();
        session
            .start(30, None, Some(bus))
            .await
            .expect("start must succeed");

        // Push the FIRST frame at the initial size; this seeds the
        // bridge's enc_w/enc_h tracking but does NOT cross the
        // resize threshold (initial==initial).
        let _ = session.frame_tx.send(Arc::new(make_test_bgra(64, 64)));

        // Push a frame at a NEW size. This must trigger both
        // `pool.on_resize` (covered by the existing pool tests) AND
        // the AppEvent::DisplayResize emission (the regression
        // surface this test pins).
        let _ = session.frame_tx.send(Arc::new(make_test_bgra(128, 96)));

        // Drain events looking for our DisplayResize. Other events
        // (capture-side, etc.) might land first; bounded loop with
        // a generous timeout to give the spawn_blocking BGRA→I420
        // conversion + tick time to fire.
        let saw_resize = tokio::time::timeout(std::time::Duration::from_secs(2), async {
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
        })
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
    ///
    /// VP8-specific (gated off Windows): subscribes to the pool with a
    /// VP8 preference and counts VP8-encoded frames at 64×64. Windows
    /// has no VP8 backend (the subscribe would fail), and the MS H.264
    /// MFT's minimum frame size makes a 64×64 H.264 rewrite unreliable.
    /// The heartbeat re-push mechanism itself is codec-agnostic bridge
    /// logic, fully exercised here on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_feed_bridge_heartbeats_on_idle_capture() {
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(0, backend);
        register_test_peer_demanding_all_layers(&session).await;
        session
            .start(30, None, None)
            .await
            .expect("start must succeed");

        // Subscribe to the pool's encoder before pushing the BGRA so
        // we don't miss the first frame.
        let pool = session.pool.get().expect("pool initialized after start");
        let prefs = encode::pool::PeerCodecPreferences::new(vec![encode::pool::CodecKind::Vp8]);
        let (subs, _lease) = pool.subscribe(&prefs).expect("VP8 always-on subscribe");
        let mut frame_rx = subs
            .into_iter()
            .next()
            .expect("at least one subscription")
            .frames;

        // Push EXACTLY ONE BGRA frame. After this, no more capture
        // activity — simulating a fully idle damage-driven backend.
        let _ = session.frame_tx.send(Arc::new(make_test_bgra(64, 64)));

        // Count encoded frames over a window > IDLE_HEARTBEAT (1s)
        // + buffer for VP8 encoder startup. The bridge ticks at
        // 33ms (fps=30); after the first send, ticks observe
        // unchanged buffer + heartbeat-not-due → skip. After
        // ~30 ticks (~1s), heartbeat is due → re-push → encoder
        // produces another frame.
        let mut count: u32 = 0;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(1800), async {
            while let Ok(frame) = frame_rx.recv().await {
                let _ = frame;
                count += 1;
                if count >= 2 {
                    return;
                }
            }
        })
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
    /// black screen: if `latest_frame` is populated by a prior
    /// capture cycle before the bridge runs its first tick — and
    /// then the desktop goes idle (typical Wayland damage-driven
    /// shape: initial render, no further events) — the bridge must
    /// snapshot `latest_frame` so the heartbeat has something to
    /// re-push. Pre-fix: snapshot was missing, latest_i420 stayed
    /// None, encoder starved → black.
    ///
    /// This test pins the seed code path: pre-seed `latest_frame`
    /// **before `start()`** so the bridge's seed snapshot
    /// reliably finds the frame. (With eager spawn from 3c.4d, the
    /// bridge task is queued during `start()`; writing
    /// `latest_frame` after `start()` would race the snapshot's
    /// read lock against the test's write lock — seed sometimes
    /// fires, sometimes doesn't.) Then never push anything via
    /// `frame_tx` and assert encoded frames flow to a pool
    /// subscriber, proving the seed branch alone fed the encoder.
    ///
    /// VP8-specific (gated off Windows): subscribes with a VP8
    /// preference and asserts VP8-encoded frame flow at 64×64. Windows
    /// has no VP8 backend; the seed-snapshot bridge logic it exercises
    /// is codec-agnostic and covered here on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_feed_bridge_seeds_latest_i420_from_capture_snapshot() {
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(0, backend);
        register_test_peer_demanding_all_layers(&session).await;

        // Pre-seed BEFORE start() so the bridge's eager-spawn
        // snapshot reads it on first scheduling. With StubBackend's
        // closed channel the capture task exits without writing
        // latest_frame organically, so the only frame the bridge
        // can ever see is this pre-seed.
        *session.latest_frame.write().await = Some(Arc::new(make_test_bgra(64, 64)));

        session
            .start(30, None, None)
            .await
            .expect("start must succeed");

        // Subscribe to the pool's encoder. The bridge has been
        // queued by start() but may not have run yet — that's fine,
        // its snapshot will read the pre-seeded latest_frame
        // whenever it gets scheduled.
        let pool = session.pool.get().expect("pool initialized after start");
        let prefs = encode::pool::PeerCodecPreferences::new(vec![encode::pool::CodecKind::Vp8]);
        let (subs, _lease) = pool.subscribe(&prefs).expect("VP8 always-on subscribe");
        let mut frame_rx = subs
            .into_iter()
            .next()
            .expect("at least one subscription")
            .frames;

        // Within one tick (~33ms at fps=30) the seeded buffer should
        // be forwarded. Generous timeout for VP8 encoder warmup
        // (cold-start can take a few hundred ms before the first
        // packet emerges). Pre-fix would never produce ANY frame.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), frame_rx.recv()).await;

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
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(11, backend);
        let bus = crate::event::EventBus::new();
        let mut bus_rx = bus.subscribe();

        // Pre-seed `latest_frame` at 128x96 BEFORE `start()` so the
        // bridge's eager-spawn snapshot reliably reads it. Models the
        // racy case: capture produced a frame at the new size while
        // the pool was already constructed at the old size (display
        // resized between pool construction and bridge spawn). The
        // pre-write order matters with eager spawn — writing after
        // start() would race the bridge task's read lock against the
        // test's write lock and produce flaky on_resize firing.
        *session.latest_frame.write().await = Some(Arc::new(make_test_bgra(128, 96)));

        session
            .start(30, None, Some(bus))
            .await
            .expect("start must succeed");

        // Pool is constructed inside `start()` at backend.resolution()
        // → 64x64 here. Pin the precondition so a future change to
        // the pool constructor's default dims fires this assert
        // instead of silently invalidating the test premise. (The
        // bridge's seed branch races against this assertion only on
        // outcome — the assertion checks the pre-resize state from
        // pool init, not what the bridge has done yet.)
        let pool = session.pool.get().expect("pool initialized after start");
        // Note: this dimension check is racy with the bridge's seed
        // branch — if the bridge already ran and called on_resize,
        // pool.dimensions() will already be (128, 96). That's fine:
        // the assertion below catches both the pre- and post-seed
        // states, and the resize-event drain is the real test.
        let initial_dims = pool.dimensions();
        assert!(
            initial_dims == (64, 64) || initial_dims == (128, 96),
            "pool dims must be either pre-resize (64,64) or \
             post-seed (128,96); got {initial_dims:?}",
        );

        // Wait for the seed path to fire and reshape. Bounded — the
        // seed runs synchronously inside the spawned task before the
        // first `tick.tick()` await, but we need to give the task
        // time to be scheduled, grab the latest_frame read lock, and
        // complete the spawn_blocking BGRA→I420 conversion. Drain
        // events looking for our DisplayResize; other events
        // (capture-side, etc.) might land first.
        let saw_resize = tokio::time::timeout(std::time::Duration::from_secs(2), async {
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
        })
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
    ///
    /// VP8-specific (gated off Windows): subscribes with a VP8
    /// preference and counts VP8-encoded frames over the burst window at
    /// 64×64. Windows has no VP8 backend; the burst-clocking bridge
    /// logic is codec-agnostic and exercised here on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_feed_bridge_burst_clocks_encoder_at_tick_rate() {
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(0, backend);
        register_test_peer_demanding_all_layers(&session).await;

        // Pre-seed `latest_frame` BEFORE `start()` so the bridge's
        // eager-spawn snapshot reliably reads it. The seed branch
        // converts and primes `latest_i420` — without this, the
        // burst-rate assertion below would be measuring the bridge
        // pushing nothing (latest_i420 = None). Pre-write order
        // matters with eager spawn (3c.4d): writing after start()
        // races the bridge's snapshot read against the test's
        // write lock.
        *session.latest_frame.write().await = Some(Arc::new(make_test_bgra(64, 64)));

        session
            .start(30, None, None)
            .await
            .expect("start must succeed");

        let pool = session.pool.get().expect("pool initialized after start");
        let prefs = encode::pool::PeerCodecPreferences::new(vec![encode::pool::CodecKind::Vp8]);
        let (subs, _lease) = pool.subscribe(&prefs).expect("VP8 always-on subscribe");
        let mut frame_rx = subs
            .into_iter()
            .next()
            .expect("at least one subscription")
            .frames;

        // Drain the seeded frame's initial encode before measuring;
        // we want to count what the BURST produces, not the cold-
        // start encode that happens regardless.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), frame_rx.recv()).await;

        // Signal peer-join burst.
        session
            .pool_feed_keyframe_tx
            .lock()
            .await
            .as_ref()
            .expect("kf_tx installed by spawn_pool_feed_bridge during start()")
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
            match tokio::time::timeout(std::time::Duration::from_millis(200), frame_rx.recv()).await
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

    /// `signal_peer_join_burst` must send `()` on
    /// `pool_feed_keyframe_tx` whenever the channel is installed.
    /// Pins the contract that `handle_offer_pool_mode`'s tail relies
    /// on for the burst window — without the send, codecs that
    /// ignore `force_keyframe` on a long-running pipe (Linux ffmpeg
    /// H.264) sit on a P-frame stream past `request_keyframe_all`
    /// for many seconds on idle desktops.
    #[tokio::test]
    async fn signal_peer_join_burst_wakes_pool_feed_bridge_in_pool_only_session() {
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(0, backend);

        let (pool_tx, mut pool_rx) = mpsc::unbounded_channel::<()>();
        *session.pool_feed_keyframe_tx.lock().await = Some(pool_tx);

        session.signal_peer_join_burst().await;

        let recv =
            tokio::time::timeout(std::time::Duration::from_millis(100), pool_rx.recv()).await;
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
        let backend = Arc::new(StubBackend {
            width: 64,
            height: 64,
        });
        let session = DisplaySession::new(0, backend);
        session
            .start(30, None, None)
            .await
            .expect("start must succeed");

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

    // ---- Phase 5a.1 gated-input-handler tests ------------------------
    //
    // Per the slice spec: in display/mod.rs, test ONLY closure plumbing
    // (true → backend gets called; false → backend doesn't).  Holder /
    // non-holder authority semantics belong in `web_gateway` tests
    // because that's where the closure is built from the authority map.

    /// Backend stub that records `inject_input` invocations.  Differs
    /// from `StubBackend` in that it counts; otherwise behaves the same
    /// (no real capture, no real injection).
    struct InjectCountingBackend {
        width: u32,
        height: u32,
        inject_count: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait]
    impl DisplayBackend for InjectCountingBackend {
        async fn start_capture(&self, _fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {}
        async fn inject_input(&self, _event: InputEvent) -> Result<(), CallerError> {
            self.inject_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }
        fn kind(&self) -> &'static str {
            "inject-counting"
        }
    }

    /// Closure returns `true` → events reach `inject_input`.  This is
    /// the unclaimed-or-this-peer-holds-it case from the per-`/ws`
    /// closure builder; both resolve to `true` there.
    #[tokio::test]
    async fn gated_input_handler_passes_event_when_authorized() {
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let backend: Arc<dyn DisplayBackend> = Arc::new(InjectCountingBackend {
            width: 64,
            height: 64,
            inject_count: Arc::clone(&counter),
        });
        let input_authorized: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);
        let handler = super::gated_input_handler(Arc::clone(&backend), input_authorized);

        handler(InputEvent::MouseMove {
            x: 0.5,
            y: 0.5,
            buttons: 0,
        });

        // The handler `tokio::spawn`s the async injection — give the
        // runtime a chance to run the spawned task before asserting.
        // Yield twice to defeat the single-poll-then-check race that a
        // single yield_now occasionally exhibits under heavy multi-test
        // contention; if the spawned future has been polled to
        // completion and pumped through any pending wakers, the counter
        // increment is visible.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "authorized event should reach the backend"
        );
    }

    /// Closure returns `false` → events are silently dropped, matching
    /// the `/ws display_input` gate convention (no per-message denial
    /// feedback).  This is the "another connection holds the slot"
    /// case, plus the federated deny-by-default until federation
    /// authority lands as its own slice.
    #[tokio::test]
    async fn gated_input_handler_drops_event_when_unauthorized() {
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let backend: Arc<dyn DisplayBackend> = Arc::new(InjectCountingBackend {
            width: 64,
            height: 64,
            inject_count: Arc::clone(&counter),
        });
        let input_authorized: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| false);
        let handler = super::gated_input_handler(Arc::clone(&backend), input_authorized);

        handler(InputEvent::MouseMove {
            x: 0.5,
            y: 0.5,
            buttons: 0,
        });
        handler(InputEvent::KeyDown {
            code: "KeyA".into(),
            key: "a".into(),
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
        });

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "unauthorized events must not reach the backend at all"
        );
    }

    /// The closure can change its decision over time — required for
    /// the live grant/release flow, where the same `WebRtcPeer` lives
    /// across multiple authority transitions.  Verifies the gate reads
    /// fresh state on each event, not a captured snapshot.
    #[tokio::test]
    async fn gated_input_handler_re_evaluates_authorization_per_event() {
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let backend: Arc<dyn DisplayBackend> = Arc::new(InjectCountingBackend {
            width: 64,
            height: 64,
            inject_count: Arc::clone(&counter),
        });
        let allow = Arc::new(AtomicBool::new(true));
        let allow_for_closure = Arc::clone(&allow);
        let input_authorized: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || allow_for_closure.load(std::sync::atomic::Ordering::SeqCst));
        let handler = super::gated_input_handler(Arc::clone(&backend), input_authorized);

        handler(InputEvent::MouseMove {
            x: 0.0,
            y: 0.0,
            buttons: 0,
        });
        allow.store(false, std::sync::atomic::Ordering::SeqCst);
        handler(InputEvent::MouseMove {
            x: 0.1,
            y: 0.1,
            buttons: 0,
        });
        allow.store(true, std::sync::atomic::Ordering::SeqCst);
        handler(InputEvent::MouseMove {
            x: 0.2,
            y: 0.2,
            buttons: 0,
        });

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "gate must check authorization on every event, not at construction time"
        );
    }
}
