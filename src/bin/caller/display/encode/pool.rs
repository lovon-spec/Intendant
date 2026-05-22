//! Shared multi-codec, multi-layer encoder pool for one display.
//!
//! ## Why this exists
//!
//! The pre-pool design used one [`Encoder`](super::Encoder) per
//! [`DisplaySession`] with the codec locked to the first peer's
//! offer (a single `codec_mime: RwLock<&'static str>` set in
//! `handle_offer`'s "first peer" branch and never reset). Every
//! subsequent viewer had to accept that locked codec or its WebRTC
//! offer failed outright with "peer does not support session codec
//! video/H264 with compatible profile" — the failure mode that bit
//! us in the multi-browser federation E2E session. The pre-pool
//! path was deleted entirely in 3c.4b; the description here is
//! retained to motivate the design.
//!
//! ## Why per-peer encoder is the wrong answer
//!
//! The reflexive fix — "just give every peer its own encoder" — is what
//! transcoding gateways do. It is **not** what production SFUs do for
//! the broadcast/many-viewers shape:
//!
//! - **CPU**: N× encoding cost. Industry simulcast (LiveKit reference
//!   numbers) costs ~1.7× one encode for three layers, because the
//!   small layers are nearly free. At 30 viewers, per-peer = 30 encodes
//!   vs simulcast = 3.
//! - **Hardware**: VAAPI ~4-8 concurrent, NVENC ~8-12, VideoToolbox
//!   ~3-4 reliably. Per-peer hits the wall at viewer ~5-8 and silently
//!   degrades to libx264 software fallback.
//! - **Precedent**: production SFUs keep one publisher-side encode bank and
//!   fan out to N subscriber-side transports, with per-peer packetization and
//!   codec negotiation at the edge.
//!
//! The right pattern is **shared encoder pool + per-peer forwarding**:
//! a small bank of encoders (typically 1-3) produces frames that all
//! peers consume; each peer's RTC driver picks which codec/layer it can
//! decode and forwards just those frames. The per-peer forwarding
//! logic lives inside the `WebRtcPeer` driver task (in
//! `display/webrtc.rs`), not in a separate module — the driver owns
//! the RTC peer connection and is the only caller that can write RTP.
//!
//! ## Pool composition
//!
//! Each [`EncoderPool`] holds two kinds of encoders:
//!
//! - **Always-on** (constructed at pool creation): the platform
//!   [`BASELINE_CODEC`]. On macOS/Linux that's VP8 layers from
//!   `LayerSpec::vp8_simulcast` (up to three at full / half / quarter).
//!   VP8 is the universal codec — Safari, Firefox, Chrome, Edge all
//!   decode it reliably and it has a long history of working well for
//!   screen content. On Windows the VP8/libvpx backend is gated off
//!   (Tier-0 deferral), so the baseline is instead a single full-resolution
//!   H.264 layer via the Media Foundation software encoder — also
//!   universally decodable by WebRTC browsers. The layers exist as a
//!   *capability*; which ones
//!   actually emit frames is governed by the demand-bound (#48) and
//!   capacity policies. By default:
//!     - a local DisplaySlot viewer (single-RID, post-#58) demands `f`
//!       only — only the full layer emits;
//!     - a federated viewer (single-encoding floor pick, post-#48)
//!       demands `q` only — only the quarter layer emits;
//!     - an opt-in multi-RID viewer (offer carries
//!       `a=simulcast:recv f;h;q`) demands all three — the experimental
//!       adaptive-bandwidth path that fans out f / h / q simultaneously.
//!   "Always-on" thus means the encoder *threads* are spawned eagerly
//!   so any browser can subscribe instantly without waiting for spin-up;
//!   it does NOT mean every layer is producing frames. Idle cost per
//!   spawned-but-paused VP8 encoder is negligible (~5 % of a core for
//!   the active layer; paused threads block in `blocking_recv`).
//! - **On-demand** (spawned when first matching peer joins, torn down
//!   when last leaves): H.264, AV1, VP9. These exist for browsers that
//!   prefer or only support a non-VP8 codec — Safari shipped H.264 long
//!   before any other browser engine, Chrome/Firefox now ship AV1, etc.
//!   On-demand encoders are refcounted by viewer count; the slot is
//!   released when the last peer using it disconnects.
//!
//! Adding a codec is additive: spawn a new on-demand slot, peers that
//! prefer it pick it up, peers that don't are unaffected.
//!
//! ## Relationship to the WebRTC driver
//!
//! The pool produces [`Arc<EncodedFrame>`] payloads keyed by
//! [`SimulcastRid`]. The per-peer forwarding
//! lives inside each peer's `WebRtcPeer` driver task
//! (`display/webrtc.rs`), which owns the peer's RTC connection and therefore
//! the only path that can write RTP. Each frame carries a
//! [`crate::display::encode::PayloadSpec`]; the driver checks it against the
//! negotiated sender codec before packetizing. An earlier design sketch had a
//! separate `PerPeerForwarder` task doing this work, but a separate task can't
//! reach the driver's RTC state; merging the forwarder into the driver
//! sidesteps the problem.
//!
//! RID semantics: the publisher emits frames with a per-layer RID (`f`/`h`/`q`
//! for full, half, quarter resolution by convention). Phase 4 currently selects
//! one active subscription per peer; TWCC-driven dynamic RID switching is a
//! later layer-selection step.
//!
//! ## Lifecycle
//!
//! ```text
//!   pool.subscribe(peer_prefs) ─┐
//!         │                     │
//!         ▼                     ▼
//!   refcount[codec]++     (Vec<EncoderSubscription>, PoolLease)
//!         │                     │
//!         ▼                     ▼
//!   if first peer +     forwarder reads from each subscription's
//!   not always-on:      broadcast::Receiver, picks frames matching
//!     sync construct    peer's chosen layer, packetizes into the
//!     + spawn           peer's RTC sender
//!         │
//!   ─── peer leaves / handle_offer fails ──→ PoolLease::drop
//!         │
//!         ▼
//!   refcount[codec]--
//!         │
//!         ▼
//!   if refcount == 0 +
//!   not always-on:
//!     tear down encoder
//! ```
//!
//! Release is tied to the [`PoolLease`] handle's `Drop`, not a separate
//! `release(prefs)` call: `Drop` can't `await`, so the pool's
//! `on_demand` map is a `std::sync::Mutex` and release is synchronous
//! over the exact `EncoderId`s the subscribe call bumped. Any code
//! path that drops the lease — peer disconnect, offer failure,
//! explicit `lease.release()` — releases deterministically.
//!
//! ## PLI coalescing
//!
//! Without coalescing, N viewers each requesting a keyframe (PLI) at
//! roughly the same time fires N keyframe requests at the encoder.
//! mediasoup's docs explicitly call this out as a 2-3× bandwidth
//! amplifier on the publisher side. [`KeyframeCoalescer`] dedupes
//! requests per `(codec, rid)` within a small window
//! ([`KEYFRAME_COALESCE_WINDOW`]).
//!
//! ## Out of scope for this stub
//!
//! - Encoder spawning (the actual `tokio::task::spawn_blocking` wiring).
//!   Phase 3.
//! - Layer width/height selection logic. Phase 4.
//! - Bitrate-aware layer downgrade based on TWCC. Phase 4.
//! - Hardware encoder slot tracking (VAAPI session counter). Phase 3.
//!
//! This module currently establishes the type vocabulary and
//! orchestration contract; subsequent phases fill in the bodies.

use crate::display::{visual_marker, EncodedFrame};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum attempts [`EncoderPool::subscribe`] will make before
/// giving up on a stale-epoch race. Two attempts is enough: the
/// first races with `on_resize`, the second has fresh dimensions
/// (and a microsecond-scale construct window before the next
/// possible on_resize). A pathological case where every attempt
/// races would mean resize traffic at sub-millisecond cadence,
/// which is itself a bug worth surfacing.
const MAX_SUBSCRIBE_ATTEMPTS: usize = 2;

/// Outcome of one [`EncoderPool::subscribe_once`] attempt. The outer
/// `subscribe` loop continues only on [`Self::StaleEpochRetry`].
enum SubscribeAttemptOutcome {
    /// Attempt produced a final result — either a successful
    /// subscription or a definitive NoCompatibleCodec that doesn't
    /// stem from a resize race. Outer subscribe returns this verbatim.
    Done(Result<(Vec<EncoderSubscription>, PoolLease), SubscribeError>),
    /// Attempt detected `source_gen` advanced during off-lock
    /// construction AND the would-be result is empty (only on-demand
    /// codecs requested, all stale). Outer subscribe retries with
    /// fresh dimensions.
    StaleEpochRetry,
}

/// Conventional simulcast RID for the highest-quality layer (full
/// resolution). Matches LiveKit / mediasoup convention.
pub const RID_FULL: &str = "f";

/// Conventional simulcast RID for the medium layer (typically half
/// resolution).
pub const RID_HALF: &str = "h";

/// Conventional simulcast RID for the lowest layer (typically quarter
/// resolution).
pub const RID_QUARTER: &str = "q";

/// PLI/FIR coalesce window. Within this duration, multiple keyframe
/// requests for the same `(codec, rid)` collapse into one request to
/// the encoder. 50 ms is short enough that perceived recovery latency
/// is unchanged for any single viewer, and long enough to absorb the
/// spike when N viewers hit the wire at once.
pub const KEYFRAME_COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// Bounded capacity for each encoder's outbound `EncodedFrame`
/// broadcast. Lossy by design — slow subscribers drop frames at this
/// queue rather than backpressuring the encoder, which would degrade
/// every other viewer.
pub const ENCODER_FRAME_BROADCAST_CAPACITY: usize = 16;

/// Bounded capacity for the pool's inbound I420 broadcast. Sized to match
/// the existing bridge → encoder sync_channel that this replaces (4
/// frames at 30fps ≈ 130ms of buffering, enough to absorb a brief
/// scheduler hiccup without wedging the bridge). Lossy: a slow encoder
/// thread sees `RecvError::Lagged` and skips ahead rather than
/// backpressuring the bridge.
pub const I420_BROADCAST_CAPACITY: usize = 4;

// ---------------------------------------------------------------------------
// Codec identity
// ---------------------------------------------------------------------------

/// The always-on / baseline codec for this platform — the codec the pool
/// guarantees is producing frames the instant any peer subscribes, and the one
/// `EncoderPool::new` / `on_resize` spawn for every always-on layer.
///
/// VP8 everywhere it's available (universal browser support, no licensing
/// complications). On Windows the VP8/libvpx backend is gated off (Tier-0
/// deferral — see `vp8.rs` / `Cargo.toml`), so the baseline is **H.264** via
/// the Media Foundation software encoder ([`super::h264_windows`]); H.264 is
/// universally decodable by WebRTC browsers too, so it is a sound baseline.
/// This keeps the Windows streaming path supplied with a working always-on
/// encoder while leaving the macOS/Linux VP8 baseline unchanged.
#[cfg(not(target_os = "windows"))]
pub const BASELINE_CODEC: CodecKind = CodecKind::Vp8;
/// See the non-Windows definition above; Windows has no VP8 backend so the
/// baseline is H.264.
#[cfg(target_os = "windows")]
pub const BASELINE_CODEC: CodecKind = CodecKind::H264;

/// Codec kinds the pool can produce. Closed enum because adding a codec is a
/// coordinated change (new encoder backend + RTC codec registration + browser
/// compat survey).
///
/// Distinct from [`super::CodecChoice`] (which is the existing
/// "what did we pick for this session" enum). Pool-level identity
/// includes codecs we plan to support but haven't wired backends for
/// yet (Av1, Vp9), so these are kept separate to avoid leaking
/// pool-internal vocabulary into the older single-encoder API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecKind {
    Vp8,
    H264,
    Vp9,
    Av1,
}

impl CodecKind {
    /// Wire / SDP MIME type for this codec, e.g. `"video/VP8"`.
    pub fn mime(&self) -> &'static str {
        match self {
            Self::Vp8 => super::MIME_TYPE_VP8,
            Self::H264 => super::MIME_TYPE_H264,
            Self::Vp9 => "video/VP9",
            Self::Av1 => "video/AV1",
        }
    }

    /// Inverse of [`Self::mime`]. Returns `None` for unrecognised wire
    /// strings — callers that need to fail loud on unknown codecs must
    /// handle the `None` case explicitly rather than matching on the
    /// MIME string themselves (keeps the codec vocabulary in one place).
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            m if m == super::MIME_TYPE_VP8 => Some(Self::Vp8),
            m if m == super::MIME_TYPE_H264 => Some(Self::H264),
            "video/VP9" => Some(Self::Vp9),
            "video/AV1" => Some(Self::Av1),
            _ => None,
        }
    }

    /// Short string for logs. Distinct from `mime()` because logs read
    /// better with `vp8` / `h264` than `video/VP8` / `video/H264`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vp8 => "vp8",
            Self::H264 => "h264",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }

    /// Whether this codec is in the always-on bank by default. The
    /// [`BASELINE_CODEC`] is always-on (VP8 on macOS/Linux for universal
    /// compatibility; H.264 on Windows where VP8 is unavailable); everything
    /// else spins up on demand.
    pub fn is_always_on_default(&self) -> bool {
        *self == BASELINE_CODEC
    }
}

impl fmt::Display for CodecKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Simulcast layer ID, RFC 8853. Newtype around String so we don't
/// confuse it with arbitrary identifiers. Maps to RTP RID at the
/// forwarding layer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimulcastRid(pub String);

impl SimulcastRid {
    pub fn new(rid: impl Into<String>) -> Self {
        Self(rid.into())
    }

    /// `RID_FULL` — convention for the top simulcast layer.
    pub fn full() -> Self {
        Self(RID_FULL.to_string())
    }

    /// `RID_HALF` — convention for the middle simulcast layer.
    pub fn half() -> Self {
        Self(RID_HALF.to_string())
    }

    /// `RID_QUARTER` — convention for the bottom simulcast layer.
    pub fn quarter() -> Self {
        Self(RID_QUARTER.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a token as a known simulcast RID. Recognizes the three
    /// canonical names ([`RID_FULL`] / [`RID_HALF`] / [`RID_QUARTER`])
    /// and returns `Some` for them; returns `None` for any other token.
    ///
    /// Forward-compat: callers parsing offerer-supplied RID lists
    /// (notably `parse_offer_simulcast_recv_rids` in
    /// [`crate::display::webrtc`]) `filter_map` through this so unknown
    /// future RID names silently drop while known ones pass through.
    /// Strict variants that need to surface unknowns can match on the
    /// raw `&str` directly before calling this.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s {
            RID_FULL => Some(Self::full()),
            RID_HALF => Some(Self::half()),
            RID_QUARTER => Some(Self::quarter()),
            _ => None,
        }
    }
}

impl fmt::Display for SimulcastRid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Encoder spec & layer
// ---------------------------------------------------------------------------

/// Resolution + bitrate spec for one simulcast layer. A non-simulcast
/// codec is represented as a single layer (typically [`SimulcastRid::full`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerSpec {
    pub rid: SimulcastRid,
    pub width: u32,
    pub height: u32,
    pub target_bitrate_kbps: u32,
    pub framerate: u32,
}

/// Minimum encoder dimension. Layers smaller than this are dropped
/// from the simulcast set rather than included — VP8 requires
/// even dims at minimum 16x16 (libvpx; smaller sizes work in some
/// builds but aren't portable), and at quarter-of-source the
/// quarter layer hits this floor for source widths/heights below
/// ~64 px (rare but possible during a live resize transient).
///
/// Set to 16 to match the lowest libvpx contract dim. A layer
/// dropped here means the simulcast set returns 1 or 2 layers
/// instead of 3 — peers that subscribed to the dropped RID see
/// a normal `RecvError::Closed` and resubscribe via the
/// pool-frame-intake reconnect path.
pub const MIN_LAYER_DIM: u32 = 16;

/// Normalize a `(width, height)` pair to the constraints both
/// VP8 encoder construction and [`super::downscale_i420`] require:
///
///   1. Round to nearest even (`& !1`).
///   2. Reject if either dim falls below [`MIN_LAYER_DIM`].
///
/// Returns `Some((even_w, even_h))` for valid layer dims,
/// `None` for dims that should drop the layer entirely.
///
/// **Used by [`LayerSpec::vp8_simulcast`]** to filter the layers
/// it returns. Since 4a-fix-#3 the resize path doesn't rescale
/// old layers — it re-invokes the pool's stored
/// [`LayerFactory`] with the new source dims, so the same
/// `vp8_simulcast` call (and its `normalize_layer_dims` filter)
/// runs on resize too. That guarantees a 64×64 → 60×48 resize
/// drops the quarter layer the same way a fresh
/// `vp8_simulcast(60, 48, ...)` would, AND the next 60×48 →
/// 64×64 resize restores the dropped quarter (because the
/// factory regenerates from the canonical layout, not from the
/// previous epoch's surviving handles).
fn normalize_layer_dims(w: u32, h: u32) -> Option<(u32, u32)> {
    let w = w & !1;
    let h = h & !1;
    if w < MIN_LAYER_DIM || h < MIN_LAYER_DIM {
        None
    } else {
        Some((w, h))
    }
}

impl LayerSpec {
    /// Reference VP8 simulcast layout — up to three layers at full /
    /// half / quarter resolution from a source resolution. Bitrates
    /// roughly follow LiveKit's defaults (2.5 Mbps / 400 kbps /
    /// 125 kbps for 720p source).
    ///
    /// Each layer's dimensions are rounded down to the nearest even
    /// number — VP8 requires even dims (per [`vp8::Vp8Encoder::new`]
    /// and the same constraint enforced by [`super::downscale_i420`]),
    /// and naked integer division produces odd dims for common
    /// display sizes. 1366×768 is the canonical case: full 1366×768
    /// (already even), half 683×384 (683 odd → encoder reject), quarter
    /// 341×192 (341 odd → encoder reject). With the round-down those
    /// become 682×384 and 340×192 — losing one column on each odd
    /// layer, which is invisible at the encode-then-display stage.
    ///
    /// Layers below [`MIN_LAYER_DIM`] are dropped from the returned
    /// vec — at small source dims (e.g. 60×48) the quarter would
    /// be 14×10, below libvpx's portable minimum. Returning 1 or 2
    /// layers instead of 3 is the safe degradation; the caller still
    /// gets at least the full layer for any source ≥ MIN_LAYER_DIM.
    /// If the source itself is below MIN_LAYER_DIM in either dim,
    /// returns an empty vec — at that point the display pipeline
    /// can't encode at all and the caller should fail loud at pool
    /// construction.
    pub fn vp8_simulcast(source_w: u32, source_h: u32, framerate: u32) -> Vec<LayerSpec> {
        let mut out = Vec::with_capacity(3);
        for (rid, divisor, target_bitrate_kbps) in [
            (SimulcastRid::full(), 1, 2500),
            (SimulcastRid::half(), 2, 400),
            (SimulcastRid::quarter(), 4, 125),
        ] {
            let Some((w, h)) = normalize_layer_dims(source_w / divisor, source_h / divisor) else {
                continue;
            };
            out.push(LayerSpec {
                rid,
                width: w,
                height: h,
                target_bitrate_kbps,
                framerate,
            });
        }
        out
    }

    /// Single-layer spec for codecs we don't simulcast (H.264 today —
    /// libx264 + ffmpeg's broken-pipe model makes parallel encoders
    /// fragile). Single full-resolution stream, no RID-based switching.
    pub fn single(codec: CodecKind, width: u32, height: u32, framerate: u32) -> LayerSpec {
        let bitrate = match codec {
            CodecKind::H264 | CodecKind::Vp9 | CodecKind::Av1 => 2500,
            CodecKind::Vp8 => 2500,
        };
        LayerSpec {
            rid: SimulcastRid::full(),
            width,
            height,
            target_bitrate_kbps: bitrate,
            framerate,
        }
    }
}

/// Identity of one encoder instance the pool can spawn. The pool keys
/// its slots on `(codec, rid)` so simulcast layers of the same codec
/// are independently spawnable / addressable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EncoderId {
    pub codec: CodecKind,
    pub rid: SimulcastRid,
}

impl EncoderId {
    pub fn new(codec: CodecKind, rid: SimulcastRid) -> Self {
        Self { codec, rid }
    }
}

impl fmt::Display for EncoderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.codec, self.rid)
    }
}

// ---------------------------------------------------------------------------
// Encoder handle (one running encoder)
// ---------------------------------------------------------------------------

/// Handle to one running encoder inside the pool.
///
/// Holding a clone of `frames` does **not** keep the encoder alive — the
/// encoder thread holds its own clone of the underlying state and exits
/// only when (a) the pool's I420 input broadcast closes (last sender
/// drops, typically at pool drop), or (b) the pool fires its
/// [`shutdown`](Self::shutdown) cancellation token (per-encoder, used
/// by on-demand teardown so other encoders keep running). Both paths
/// are cooperative; the thread checks `shutdown.is_cancelled()` between
/// frames so a cancellation lands within at most one `blocking_recv`
/// wakeup (~one frame interval).
#[derive(Clone)]
pub struct EncoderHandle {
    pub id: EncoderId,
    pub layer: LayerSpec,
    /// Broadcast of encoded frames produced by this encoder. Each
    /// peer's forwarder calls `frames.subscribe()` once when it joins.
    /// The broadcast is lossy (slow subscribers see `Lagged` and skip)
    /// — intentional, because backpressuring the encoder degrades
    /// every other peer using this layer.
    pub frames: broadcast::Sender<Arc<EncodedFrame>>,
    /// Per-encoder force-keyframe flag. [`EncoderPool::request_keyframe`]
    /// stores `true` here; the encoder thread `swap`s it back to false
    /// when consumed on the next frame and passes the bool to
    /// [`crate::display::encode::Encoder::encode`]. AtomicBool keeps the
    /// signaling lock-free between the async pool API and the std::thread
    /// encoder loop.
    pub force_keyframe: Arc<AtomicBool>,
    /// **Phase 4d.0**: per-encoder pause flag. When set, the encoder
    /// thread drains its `i420_rx` broadcast subscription as usual
    /// (so the channel doesn't lag) but skips the downscale + encode
    /// + broadcast step entirely. Used by the layer-selection policy
    /// (4d.2) to throttle layers no peer is consuming under current
    /// bandwidth conditions, without tearing down the encoder slot
    /// itself — resume is just a flag flip and the next captured frame
    /// gets encoded.
    ///
    /// Behavior preserved across pause:
    /// - `force_keyframe`: NOT consumed while paused, so a keyframe
    ///   request that arrives during pause is honored on the first
    ///   frame after resume — exactly the right thing for "viewer just
    ///   subscribed to this layer, give them a fresh keyframe."
    /// - Watchdog: not advanced while paused (otherwise a long pause
    ///   would trip the silent-output threshold and trigger an
    ///   unnecessary H.264 fallback on resume).
    /// - Metrics: encoder doesn't count `encode_frames` /
    ///   `encode_freshness_us_sum` while paused (no work was done).
    ///   `encode_drops` from broadcast lag still counts (lag reflects
    ///   subscriber slowness, not pause state).
    pub paused: Arc<AtomicBool>,
    /// Per-encoder shutdown signal. Cancelled by [`EncoderPool`] on
    /// release/drop. Encoder thread checks between frames and breaks
    /// cleanly on next iter. Distinct from "i420 broadcast closed" so
    /// individual on-demand encoders can be torn down without dropping
    /// the shared input channel.
    pub shutdown: CancellationToken,
}

impl EncoderHandle {
    /// Subscribe a new consumer (peer forwarder) to this encoder's
    /// frame stream. Subscriber starts receiving from the next emitted
    /// frame; previously emitted frames are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<EncodedFrame>> {
        self.frames.subscribe()
    }
}

// ---------------------------------------------------------------------------
// I420 input frame
// ---------------------------------------------------------------------------

/// One I420-converted capture frame, fed into the pool's input broadcast
/// by the bridge. `data` is `Arc`-wrapped so multiple encoder threads
/// each get a cheap clone (the bytes themselves aren't copied per
/// subscriber).
#[derive(Clone, Debug)]
pub struct I420Frame {
    pub data: Arc<Vec<u8>>,
    pub arrived: Instant,
    pub visual_marker_value: Option<u32>,
}

// ---------------------------------------------------------------------------
// Subscription returned to peer forwarders
// ---------------------------------------------------------------------------

/// Subscription package handed back to one peer's forwarder by
/// [`EncoderPool::subscribe`]. Carries everything the forwarder needs
/// to consume one encoder's output:
///
/// - the [`EncoderId`] (so the forwarder knows which codec/layer this is)
/// - the [`LayerSpec`] (resolution / bitrate / framerate, useful for
///   layer-selection policy)
/// - the live broadcast receiver
///
/// A peer that supports multiple codecs receives multiple
/// `EncoderSubscription`s — one per codec the peer can decode. The
/// forwarder picks which to actually consume based on the peer's
/// negotiated codec set; unconsumed subscriptions are dropped at peer
/// teardown which decrements the encoder's refcount via
/// [`EncoderPool::release`].
pub struct EncoderSubscription {
    pub id: EncoderId,
    pub layer: LayerSpec,
    pub frames: broadcast::Receiver<Arc<EncodedFrame>>,
}

// ---------------------------------------------------------------------------
// Peer codec preferences (input to subscribe)
// ---------------------------------------------------------------------------

/// What a peer can decode. The forwarder builds this from the peer's
/// SDP offer using [`super::parse_offered_codecs`] (existing function).
///
/// Order matters only as a preference hint for the forwarder when
/// multiple codecs would work; the pool subscribes the peer to **all**
/// codecs it supports and lets the forwarder choose at frame time
/// (cheap; subscribing is just a `broadcast::Receiver` per codec).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerCodecPreferences {
    pub supported: Vec<CodecKind>,
}

impl PeerCodecPreferences {
    pub fn new(supported: Vec<CodecKind>) -> Self {
        Self { supported }
    }

    pub fn supports(&self, codec: CodecKind) -> bool {
        self.supported.contains(&codec)
    }

    pub fn is_empty(&self) -> bool {
        self.supported.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Refcount slot for on-demand encoders
// ---------------------------------------------------------------------------

/// One on-demand encoder slot in the pool. Refcounted so the encoder
/// is torn down when the last peer using it leaves.
///
/// Always-on encoders use a different code path: they're never released
/// and never tracked by refcount (an always-on slot at refcount 0 is
/// still alive, intentionally).
///
/// `generation` is a monotonically-increasing per-slot-instance token
/// allocated from the pool-level [`EncoderPoolInner::slot_gen_counter`]
/// every time a new slot is inserted for a given `EncoderId`. Leases
/// record the generation at subscribe time; release only decrements
/// the refcount when the current slot's generation matches the
/// recorded one. This prevents a stale lease from
/// [`Self::on_resize`]-torn-down incarnation A from decrementing the
/// refcount of a subsequently-subscribed incarnation B that happens
/// to share the same `EncoderId` — the scenario where the forwarder
/// detects Closed, re-subscribes, and THEN drops its old lease last.
struct OnDemandSlot {
    handle: EncoderHandle,
    refcount: usize,
    generation: u64,
}

// ---------------------------------------------------------------------------
// Subscribe error
// ---------------------------------------------------------------------------

/// Subscribe failure modes. Kept minimal because the pool itself has
/// exactly one way to say "nothing I can offer this peer" today —
/// every returned codec has a working encoder backend at the moment of
/// the call. Hardware-exhaustion (VAAPI session limit hit) would land
/// as a distinct variant when that tracking exists.
#[derive(Debug)]
pub enum SubscribeError {
    /// The peer's codec preferences produced zero subscriptions:
    /// either no overlap with the pool's codec set, or every on-demand
    /// codec the peer wanted failed encoder construction at this
    /// moment. Forwarder should reject the WebRTC offer with
    /// "no compatible codec".
    NoCompatibleCodec,
}

impl fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCompatibleCodec => {
                write!(f, "no pool codec overlaps the peer's preferences")
            }
        }
    }
}

impl std::error::Error for SubscribeError {}

// ---------------------------------------------------------------------------
// PoolLease — RAII release handle
// ---------------------------------------------------------------------------

/// RAII handle tying a peer's pool subscriptions to the pool's
/// on-demand refcounts. Release happens on [`Drop`] (or explicit
/// [`Self::release`]) — whichever fires first.
///
/// Drop is synchronous: decrements the refcount on each acquired
/// `EncoderId` under the pool's `std::sync::Mutex`, and if a slot hits
/// zero, cancels its `shutdown` token and removes it from the map.
/// This works from any context (async, sync, during shutdown, outside
/// a runtime) because there is no `.await` path.
///
/// Always-on encoders are not in `on_demand_ids` and are never released
/// (they live for the pool's lifetime), so dropping a lease that only
/// holds always-on subscriptions is a no-op for the refcount bookkeeping.
///
/// Construction is private: [`EncoderPool::subscribe`] is the only
/// place a `PoolLease` comes from, which guarantees `on_demand_ids`
/// matches what the pool actually bumped.
pub struct PoolLease {
    pool: Arc<EncoderPoolInner>,
    /// Exact on-demand `(EncoderId, generation)` pairs this lease
    /// refcounts. Only contains entries that `subscribe` successfully
    /// incremented (construction failures never land here). The
    /// `generation` is the slot's instance-unique token at subscribe
    /// time, used by [`Self::release_impl`] to guard against the
    /// stale-lease-on-replaced-slot scenario — see
    /// [`OnDemandSlot::generation`] for the full contract.
    on_demand_refs: Vec<(EncoderId, u64)>,
    /// Set on explicit release so `Drop` is a no-op. Atomic because
    /// `Drop` takes `&mut self` but we want `release(self)` to consume
    /// while also being robust against accidental double-release.
    released: AtomicBool,
}

impl PoolLease {
    /// Explicitly release now rather than waiting for Drop. Consumes
    /// the lease. Calling again is impossible (moved), and the Drop
    /// that fires on the moved-out lease is a no-op because `released`
    /// is already set.
    pub fn release(mut self) {
        self.release_impl();
    }

    /// Returns the number of on-demand encoders this lease is holding
    /// open. Useful for diagnostics and for tests that verify
    /// refcount semantics. Always-on encoders aren't counted.
    pub fn on_demand_count(&self) -> usize {
        self.on_demand_refs.len()
    }

    /// Release the lease's claim on a subset of its on-demand
    /// encoders without releasing the entire lease. The remaining
    /// claims continue to be the lease's responsibility on full
    /// release ([`Self::release`] or `Drop`).
    ///
    /// Used by the per-peer pool intake at
    /// `webrtc.rs::pool_frame_intake` after
    /// `active_codec_from_subscriptions` picks the active codec out
    /// of a multi-codec subscription set: the inactive codecs'
    /// subscriptions are partitioned off and their on-demand claims
    /// released so encoders we won't consume don't stay refcounted
    /// into perpetuity (encoding into a broadcast channel with no
    /// receivers — the wasted-CPU regression caught in the 3c.3b.2a
    /// review). Active-codec subscriptions stay in the lease and feed
    /// the multi-forwarder fan-out (phase 4c).
    ///
    /// IDs in `ids` that don't appear in `on_demand_refs` are
    /// silently skipped. This is the always-on case: the intake
    /// passes "every inactive subscription's id" without
    /// distinguishing always-on from on-demand, and always-on slots
    /// have no refcount entry so passing their ids is a no-op.
    ///
    /// The generation gate from [`Self::release_impl`] applies:
    /// stale claims against replaced slots (post-`on_resize`) are
    /// skipped without decrementing the replacement slot's refcount.
    ///
    /// Idempotent against double-release: if [`Self::release`] or
    /// `Drop` already ran, this is a no-op (the `released` flag
    /// short-circuits the entire path).
    pub fn release_on_demand_subset(&mut self, ids: &[EncoderId]) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        if ids.is_empty() {
            return;
        }
        // Partition the lease's claims: the ones we're releasing
        // now, and the ones we keep for full-release later.
        let (to_release, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.on_demand_refs)
            .into_iter()
            .partition(|(id, _gen)| ids.contains(id));
        self.on_demand_refs = keep;

        if to_release.is_empty() {
            return;
        }

        let mut guard = self.pool.on_demand.lock().unwrap();
        for (id, recorded_gen) in &to_release {
            if let Some(slot) = guard.get_mut(id) {
                // Generation gate: stale claim against a replaced
                // slot must not decrement the replacement. See
                // `release_impl` for the full contract.
                if slot.generation != *recorded_gen {
                    continue;
                }
                slot.refcount = slot.refcount.saturating_sub(1);
                if slot.refcount == 0 {
                    slot.handle.shutdown.cancel();
                    guard.remove(id);
                }
            }
            // Slot not in map: already torn down by another lease's
            // release, or by on_resize. No work for us.
        }
    }

    fn release_impl(&mut self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut guard = self.pool.on_demand.lock().unwrap();
        for (id, recorded_gen) in &self.on_demand_refs {
            if let Some(slot) = guard.get_mut(id) {
                // Generation gate: only decrement when the slot still
                // has the same incarnation this lease subscribed
                // against. If `on_resize` (or any future replace-in-
                // place path) dropped the old slot and a new one was
                // installed under the same `EncoderId`, its
                // generation differs — this lease's claim was against
                // the OLD slot and must not decrement the NEW one.
                // The new slot's refcount is owned by whichever
                // forwarder subscribed against it post-replace.
                if slot.generation != *recorded_gen {
                    continue;
                }
                slot.refcount = slot.refcount.saturating_sub(1);
                if slot.refcount == 0 {
                    // Signal the encoder thread to exit. Dropping the
                    // handle below closes its frames broadcast, which
                    // subscribers see on next recv; the encoder thread
                    // itself exits on CancellationToken observation
                    // OR on i420_rx Closed (whichever fires first).
                    slot.handle.shutdown.cancel();
                    guard.remove(id);
                }
            }
            // Slot not in map: either already torn down by another
            // lease's release, or dropped entirely by on_resize.
            // Either way, our claim is moot — skip.
        }
    }
}

impl Drop for PoolLease {
    fn drop(&mut self) {
        self.release_impl();
    }
}

// ---------------------------------------------------------------------------
// Keyframe coalescer
// ---------------------------------------------------------------------------

/// Dedupes keyframe (PLI/FIR) requests within a short window per
/// `(codec, rid)`. Without this, N viewers all PLI-ing simultaneously
/// produces N keyframe requests at the encoder, which mediasoup's docs
/// explicitly call out as a 2-3× bandwidth amplifier.
///
/// API: callers ask `should_request(...)` before forwarding a PLI to
/// the encoder. If the answer is `true`, fire the request and the
/// coalescer records the time. If `false`, drop the PLI silently —
/// another peer already requested a keyframe in this window and the
/// encoder will produce one shortly.
pub struct KeyframeCoalescer {
    last_request: std::sync::Mutex<HashMap<(CodecKind, SimulcastRid), Instant>>,
    window: Duration,
}

impl KeyframeCoalescer {
    pub fn new() -> Self {
        Self::with_window(KEYFRAME_COALESCE_WINDOW)
    }

    pub fn with_window(window: Duration) -> Self {
        Self {
            last_request: std::sync::Mutex::new(HashMap::new()),
            window,
        }
    }

    /// Returns `true` if the caller should fire a keyframe request to
    /// the encoder, `false` if a request was already fired for this
    /// `(codec, rid)` within the coalesce window.
    ///
    /// Internally records the request time on `true` so subsequent
    /// callers within the window see `false`.
    pub fn should_request(&self, codec: CodecKind, rid: &SimulcastRid) -> bool {
        let now = Instant::now();
        let key = (codec, rid.clone());
        let mut guard = self.last_request.lock().unwrap();
        match guard.get(&key) {
            Some(&prev) if now.duration_since(prev) < self.window => false,
            _ => {
                guard.insert(key, now);
                true
            }
        }
    }
}

impl Default for KeyframeCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// EncoderPool
// ---------------------------------------------------------------------------

/// The orchestrator. One pool per [`crate::display::DisplaySession`].
///
/// Phase 3a (this commit) implements the always-on encoder spawn path —
/// `new` actually spawns encoder threads for each always-on layer, the
/// bridge feeds I420 frames via [`Self::push_i420_frame`], and
/// [`Self::request_keyframe`] propagates through the coalescer to the
/// matching encoder thread's atomic flag.
///
/// Phase 3b will fill in on-demand encoder spawn (refcount-driven, one
/// per non-always-on codec subscriber) so a peer offering only H.264
/// triggers an H.264 encoder spawn; the slot is torn down when the
/// last H.264 peer disconnects.
///
/// Phase 3c wires this pool into [`crate::display::DisplaySession`],
/// replacing the single-encoder code path in `start_encoder_pipeline`.
///
/// The pool is `Clone` (Arc-backed) — one reference goes to the bridge
/// (feeds I420), one to each peer's forwarder (subscribes / releases),
/// and one to the WebRTC PLI handler (`request_keyframe`).
#[derive(Clone)]
pub struct EncoderPool {
    inner: Arc<EncoderPoolInner>,
}

struct EncoderPoolInner {
    /// Always-on encoders (constructed at pool creation, torn down and
    /// respawned atomically on resize). Today: a single VP8 layer at
    /// the source resolution. Phase 4 expands this into VP8 simulcast
    /// (multiple layers).
    ///
    /// Behind `StdRwLock` because [`EncoderPool::on_resize`] mutates
    /// the vec (swapping every handle for a fresh one at new
    /// dimensions) while readers — `subscribe`, `request_keyframe`,
    /// `Drop` — iterate it. Reads are frequent and short; writes (only
    /// `on_resize`) are rare. std's `RwLock` is fine; we don't need
    /// parking_lot's extra features and consistency with the `StdMutex`
    /// already used for `on_demand` is easier to reason about.
    always_on: StdRwLock<Vec<EncoderHandle>>,

    /// On-demand encoders, keyed by `(codec, rid)`. Spawned on first
    /// peer that needs them, torn down when the last peer leaves.
    ///
    /// Uses `std::sync::Mutex` rather than `tokio::sync::RwLock` so
    /// `PoolLease::Drop` can release synchronously — tokio's async
    /// locks are off-limits from `Drop` because it can't `await`, and
    /// spawning a release task from `Drop` is fragile during shutdown
    /// or outside a runtime. Critical sections here are short
    /// (decrement + zero-check + cancel) so a blocking `.lock()` is
    /// acceptable even from async callers.
    on_demand: StdMutex<HashMap<EncoderId, OnDemandSlot>>,

    /// Coalesces PLI/FIR across viewers per `(codec, rid)`.
    keyframe_coalescer: KeyframeCoalescer,

    /// Shared I420 input broadcast. The bridge sends one frame per
    /// tick; every running encoder subscribes once at spawn and reads
    /// via blocking_recv from its dedicated thread.
    i420_tx: broadcast::Sender<I420Frame>,

    /// Frame duration in milliseconds (1000 / fps), passed into each
    /// encoder's `encode()` call. Stored on the pool because every
    /// on-demand spawn needs it.
    duration_ms: u64,

    /// Source resolution + epoch under one lock so callers always see
    /// a consistent (width, height, gen) triple. Replaces an earlier
    /// design that stored these in separate atomics, which permitted
    /// torn reads where `subscribe` could capture an epoch from
    /// before a resize but read dimensions from after, or vice versa
    /// — the gen check then either false-positived (cancelled valid
    /// encoders) or, theoretically, missed a stale install. The
    /// `RwLock<SourceState>` makes the snapshot operation atomic by
    /// virtue of the read lock.
    ///
    /// Performance: `dimensions()` is called by the bridge's
    /// `debug_assert!` in non-release builds; in release builds it
    /// has only test/diagnostic callers. `on_resize` (the only
    /// writer) is rare. Read-lock contention is therefore negligible
    /// in practice.
    source_state: StdRwLock<SourceState>,

    /// Capture framerate in Hz. Immutable for the pool's lifetime —
    /// resize only changes spatial dimensions, not framerate. If we
    /// ever need to support framerate change at runtime, this can
    /// move into `SourceState`.
    framerate: u32,

    /// Monotonically-increasing counter that allocates a unique
    /// `generation` token for every `OnDemandSlot` inserted into the
    /// `on_demand` map. Leases record the slot's generation at
    /// subscribe time so release can distinguish the slot they
    /// subscribed against from a later incarnation that happens to
    /// reuse the same `EncoderId`. Pool-level (not per-id) because
    /// unique-across-all-slots is sufficient; the bookkeeping cost of
    /// a per-id counter isn't worth the extra state.
    slot_gen_counter: AtomicU64,

    /// Shared metrics counters. Every encoder thread holds an
    /// `Arc::clone` of this and bumps `encode_frames` +
    /// `encode_freshness_us_sum` per encoded packet, plus
    /// `encode_drops` on broadcast lag. The pool is the sole
    /// producer of these counters since 3c.4b deleted the legacy
    /// fan-out, so [`crate::display::DisplayMetricsSnapshot`]
    /// reflects total session throughput.
    counters: Arc<crate::display::DisplayMetricsCounters>,

    /// Factory closure invoked at construction *and on every resize*
    /// to derive the canonical always-on layer set for the current
    /// source dimensions. Storing the factory rather than the
    /// resulting `Vec<LayerSpec>` is what makes resize idempotent
    /// across the round-trip: a 64×64 → 60×48 resize that drops
    /// the quarter layer is automatically restored on the next
    /// 60×48 → 64×64 resize, because the factory is called fresh
    /// at each new dim and `vp8_simulcast(64, 64)` returns all
    /// three layers again. Symmetrically, this avoids the
    /// rounding drift that accumulates when each resize derives
    /// from the previous epoch's already-rounded dims (e.g.
    /// 1366×768 half = 682; resize to 1920×1080 via repeated
    /// rescaling would yield 958, not the canonical 960).
    layer_factory: LayerFactory,
}

/// Function that produces the always-on layer set for a given
/// source `(width, height)`. Called by [`EncoderPool::new`] at
/// construction and by [`EncoderPool::on_resize`] after every
/// real-dim change, so the layer set is **always** the canonical
/// layout for the current source dims — no derived-from-old-dims
/// drift, no permanently-dropped layers after a shrink-then-grow
/// cycle.
///
/// Common factories:
///   - `|w, h| LayerSpec::vp8_simulcast(w, h, fps)` — production
///     simulcast layout, 3 layers normalized to even dims and
///     dropped below `MIN_LAYER_DIM`.
///   - `|w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, fps)]` —
///     single full-source VP8 layer (used by tests that exercise
///     the on-demand path with a minimal always-on set).
///   - `|_, _| vec![]` — no always-on, on-demand-only flows.
pub type LayerFactory = Box<dyn Fn(u32, u32) -> Vec<LayerSpec> + Send + Sync>;

/// Atomic snapshot of source dimensions + resize epoch. Stored
/// behind a `RwLock` inside [`EncoderPoolInner`] so any caller that
/// needs both the dimensions and the epoch (notably
/// [`EncoderPool::subscribe_once`], whose pass-3 stale check
/// compares the captured epoch against the current epoch) gets a
/// consistent view rather than two separate atomic reads that
/// could straddle a concurrent `on_resize`.
#[derive(Clone, Copy, Debug)]
struct SourceState {
    width: u32,
    height: u32,
    /// Bumped on every real-dim `on_resize`. Same-dim no-ops do not
    /// advance the epoch (so racing subscribes aren't penalized for
    /// a resize that changed nothing).
    gen: u64,
}

/// Policy for an always-on / baseline encoder that fails to construct at
/// pool startup ([`EncoderPool::new`]).
///
/// **macOS / Linux — fail loud (panic).** The baseline there is VP8
/// (libvpx), the universally-available fallback the pool guarantees is
/// producing frames the instant any peer subscribes. libvpx has no host
/// dependency that can be absent, so a construction failure means the
/// build is fundamentally broken — better to fail loud at startup than
/// serve a silent never-decoding stream. Keeping the panic here also means
/// a real regression in the VP8 path can't be masked by this softening.
#[cfg(not(target_os = "windows"))]
fn baseline_encoder_construction_failed(id: &EncoderId, err: &str) -> ! {
    panic!(
        "always-on encoder {} construction failed at pool startup: {} — \
         always-on codecs must always be constructable; a VP8 libvpx \
         failure at startup is unrecoverable",
        id, err,
    )
}

/// Windows variant — degrade, do not panic.
///
/// The baseline on Windows is the Media Foundation H.264 software encoder
/// (VP8/libvpx is gated off). Unlike libvpx, that MFT can be genuinely
/// unavailable or reject a configuration on a given host (e.g. the
/// `Server-Media-Foundation` optional feature missing, no registered H.264
/// encoder MFT, or an output media type the MFT won't initialize). When
/// the pool is constructed eagerly at `--web` daemon startup
/// (`auto_activate_windows_user_display` → `DisplaySession::start`), a
/// panic here takes down the entire dashboard daemon, not just the display
/// stream. So on Windows we log and continue with an empty (or partial)
/// always-on bank: `subscribe` simply yields no baseline subscription, the
/// Video tab reports no active stream, and every other surface (Activity,
/// Stats, Terminal, Sessions, Settings) stays up. This is the
/// degrade-gracefully contract the rest of the pool already honors for
/// on-demand construction failures.
#[cfg(target_os = "windows")]
fn baseline_encoder_construction_failed(id: &EncoderId, err: &str) {
    eprintln!(
        "[encoder/pool] WARN: always-on baseline encoder {} failed to \
         construct at pool startup: {} — display will not stream on this \
         host (the dashboard stays up); check that the Media Foundation \
         H.264 encoder is available",
        id, err,
    );
}

impl EncoderPool {
    /// Construct a pool. The `layer_factory` closure is invoked
    /// **immediately** with `(source_width, source_height)` to
    /// produce the initial always-on layer set, and it is stored
    /// on the pool so that [`Self::on_resize`] can re-invoke it
    /// with the new dims to derive the canonical layout for the
    /// new source size — see [`LayerFactory`] for why this matters.
    ///
    /// * `source_width` / `source_height` — the capture resolution.
    ///   Used for on-demand encoder spawns (e.g. an H.264 encoder
    ///   spun up when the first H.264-preferring peer joins runs
    ///   at the source resolution, not at the simulcast layer size).
    /// * `framerate` — target capture rate; `duration_ms` is derived
    ///   as `1000 / framerate`.
    /// * `layer_factory` — produces the always-on layer set for any
    ///   given source dims. Production uses
    ///   `|w, h| LayerSpec::vp8_simulcast(w, h, fps)`; tests typically
    ///   use `|_, _| vec![]` (on-demand only) or
    ///   `move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)]`
    ///   (single full-source layer that tracks resize).
    ///
    /// Always-on codec is always VP8 — it has the broadest browser
    /// support and no codec-licensing complications, so it's the safe
    /// default the pool guarantees is producing frames the instant any
    /// peer subscribes. H.264 is spawned on-demand by
    /// [`Self::subscribe`] when a peer needs it; VP9 / AV1 are not yet
    /// wired to a backend (phase 4+).
    ///
    /// Returns a pool with all always-on encoder threads already
    /// running. The pool's I420 broadcast is empty until the caller
    /// starts feeding frames via [`Self::push_i420_frame`].
    pub fn new(
        source_width: u32,
        source_height: u32,
        framerate: u32,
        layer_factory: impl Fn(u32, u32) -> Vec<LayerSpec> + Send + Sync + 'static,
        counters: Option<Arc<crate::display::DisplayMetricsCounters>>,
    ) -> Self {
        // Every pool encoder thread bumps these counters on each
        // encoded packet (encode_frames, encode_freshness_us_sum) and
        // on broadcast lag (encode_drops). DisplayMetricsSnapshot
        // reflects the pool's total throughput. Tests that don't
        // care about metrics pass `None`; production passes
        // `Some(Arc::clone(&self.counters))` from DisplaySession::start.
        let counters =
            counters.unwrap_or_else(|| Arc::new(crate::display::DisplayMetricsCounters::new()));
        let duration_ms = if framerate > 0 {
            1000 / framerate as u64
        } else {
            33
        };
        let (i420_tx, _) = broadcast::channel::<I420Frame>(I420_BROADCAST_CAPACITY);

        let layer_factory: LayerFactory = Box::new(layer_factory);
        let initial_layers = (layer_factory)(source_width, source_height);
        let mut always_on = Vec::with_capacity(initial_layers.len());
        for layer in initial_layers {
            // Always-on bank is the platform [`BASELINE_CODEC`] (VP8 on
            // macOS/Linux, H.264 on Windows — see module docs).
            let id = EncoderId::new(BASELINE_CODEC, layer.rid.clone());
            match try_spawn_encoder_thread(
                id.clone(),
                layer,
                source_width,
                source_height,
                &i420_tx,
                duration_ms,
                &counters,
            ) {
                Ok(handle) => always_on.push(handle),
                Err(e) => baseline_encoder_construction_failed(&id, &e),
            }
        }

        Self {
            inner: Arc::new(EncoderPoolInner {
                always_on: StdRwLock::new(always_on),
                on_demand: StdMutex::new(HashMap::new()),
                keyframe_coalescer: KeyframeCoalescer::new(),
                i420_tx,
                duration_ms,
                source_state: StdRwLock::new(SourceState {
                    width: source_width,
                    height: source_height,
                    gen: 0,
                }),
                framerate,
                slot_gen_counter: AtomicU64::new(0),
                counters,
                layer_factory,
            }),
        }
    }

    /// Codecs this pool knows how to spawn an on-demand encoder for.
    /// VP8 + H.264 are the codecs with wired backends; VP9 and AV1 will be
    /// added when their encoder crates are picked.
    ///
    /// On Windows the VP8/libvpx backend is gated off (its `new()` always
    /// `Err`s), so VP8 is excluded — attempting it would just fail
    /// construction and get logged+skipped. H.264 (Media Foundation) is the
    /// only spawnable codec there, and it's already the always-on baseline.
    #[cfg(not(target_os = "windows"))]
    fn on_demand_spawnable(codec: CodecKind) -> bool {
        matches!(codec, CodecKind::Vp8 | CodecKind::H264)
    }

    /// Windows variant — see the non-Windows definition. VP8 has no backend.
    #[cfg(target_os = "windows")]
    fn on_demand_spawnable(codec: CodecKind) -> bool {
        matches!(codec, CodecKind::H264)
    }

    /// Source (capture) dimensions the pool was constructed with.
    ///
    /// Used by the bridge's dual-feed path to guard against pushing
    /// I420 frames of mismatched size after a resolution change — the
    /// pool's encoders are locked to these dimensions until a future
    /// `on_resize` method (phase 3c.3) tears them down and respawns
    /// them at new dimensions. Until that exists, a push at the wrong
    /// size would deliver a buffer the encoder can't interpret.
    ///
    /// Returns `(source_width, source_height)` — the values most
    /// recently set by either [`Self::new`] or [`Self::on_resize`].
    /// Reads the dimensions and the resize epoch atomically; if you
    /// need the epoch as well (the bridge's debug_assert doesn't,
    /// but [`Self::subscribe_once`]'s race check does), call
    /// [`Self::snapshot_source`] instead so a single read returns
    /// both.
    pub fn dimensions(&self) -> (u32, u32) {
        let s = self.snapshot_source();
        (s.width, s.height)
    }

    /// Atomic (width, height, epoch) snapshot under the source-state
    /// read lock. Used by [`Self::subscribe_once`] to capture the
    /// dimensions used for on-demand encoder construction AND the
    /// epoch they correspond to in a single critical section, so the
    /// pass-3 stale check is comparing apples to apples.
    fn snapshot_source(&self) -> SourceState {
        *self.inner.source_state.read().unwrap()
    }

    /// Replace every encoder in the pool with a fresh one at new source
    /// dimensions.
    ///
    /// Called by the capture bridge when the backend reports a
    /// resolution change (X11 xrandr, window mode switch, hot-plug).
    /// Each existing encoder handle has its shutdown cancelled and a
    /// new handle is spawned at the layer-proportional rescaled size,
    /// keeping the same codec/rid identity. The pool's dimension
    /// atomics advance to the new values BEFORE the handle swap so
    /// concurrent `dimensions()` readers (including the bridge's
    /// push_i420_frame gate) observe a consistent (new dimensions +
    /// new handles) pair.
    ///
    /// Post-conditions:
    /// - `self.dimensions() == (new_width, new_height)`.
    /// - `self.source_gen` has been bumped; in-flight subscribes that
    ///   snapshotted it earlier will detect the race and cancel their
    ///   orphan encoders rather than installing them at stale
    ///   dimensions.
    /// - Every always-on handle is fresh, at a layer size proportional
    ///   to the old layer's ratio of the old source dimensions
    ///   (simulcast-safe).
    /// - Every on-demand slot has been **dropped** (map cleared, each
    ///   old handle's `shutdown` cancelled). On-demand slots are NOT
    ///   respawned in place — doing so would produce a generation-vs-
    ///   refcount mismatch (the transferred refcount would be owned
    ///   by no live lease). Forwarders detecting `RecvError::Closed`
    ///   re-subscribe via [`Self::subscribe`], which spawns a fresh
    ///   slot at the new dimensions with refcount=1 and a fresh
    ///   generation.
    /// - Every old handle's `shutdown` is cancelled; its encoder
    ///   thread exits on next `blocking_recv` wakeup (one frame
    ///   interval at most) and, per the post-recv shutdown check,
    ///   does not emit a stale frame on its way out.
    ///
    /// Subscriber impact: any existing
    /// `broadcast::Receiver<Arc<EncodedFrame>>` obtained from one of
    /// the swapped always-on handles or a dropped on-demand slot
    /// observes `RecvError::Closed` on its next recv. Peer forwarders
    /// (phase 3c.3b onward) must handle Closed by tearing the peer
    /// down or re-subscribing via [`Self::subscribe`].
    ///
    /// No-op when `(new_width, new_height) == self.dimensions()` —
    /// avoids encoder churn when the capture backend emits a
    /// same-dimensions re-announcement (common on xrandr
    /// notifications that only changed refresh rate, etc.). In that
    /// case `source_gen` is NOT bumped, so in-flight subscribes keep
    /// their in-construction encoders and install normally.
    ///
    /// # Panics
    ///
    /// On macOS / Linux, panics if a new always-on encoder fails to
    /// construct — a VP8 baseline failure at any lifecycle point (startup
    /// or resize) is unrecoverable by contract (see [`Self::new`] and
    /// [`baseline_encoder_construction_failed`]). On Windows the same
    /// failure is logged and the layer is dropped instead, so a resize the
    /// Media Foundation H.264 MFT can't honor never crashes the daemon.
    pub fn on_resize(&self, new_width: u32, new_height: u32) {
        // Atomic update of dimensions + epoch under the source-state
        // write lock. Holding the lock across both the dim and gen
        // updates means any concurrent reader either sees the OLD
        // (width, height, gen) triple or the NEW one — never a
        // tearing combination. This replaces the earlier design of
        // three separate atomics, where a subscribe could capture an
        // epoch from one side of the resize and dimensions from the
        // other, producing false-positive cancellations or (in
        // pathological orderings) installs at stale dimensions.
        //
        // Same-dim early return: read the current state under the
        // read lock; if dims unchanged, drop the read lock and
        // return without acquiring the write lock — avoids epoch
        // bumps that would penalize racing subscribes for nothing.
        let (old_width, old_height) = {
            let s = self.inner.source_state.read().unwrap();
            (s.width, s.height)
        };
        if (old_width, old_height) == (new_width, new_height) {
            return;
        }

        // Take the write lock, advance the source state. Bumped epoch
        // is the authoritative "resize has happened" signal —
        // concurrent readers that observe a bumped epoch are
        // guaranteed to see the new dimensions in the same snapshot.
        {
            let mut s = self.inner.source_state.write().unwrap();
            s.width = new_width;
            s.height = new_height;
            s.gen = s.gen.saturating_add(1);
        }

        // Swap always-on handles. Hold the write lock across
        // try_spawn_encoder_thread (synchronous codec probe,
        // potentially a subprocess spawn for ffmpeg-based backends).
        // This serializes resize against subscribe / request_keyframe
        // / drop — which is the right tradeoff: resize is rare and
        // expensive; a brief read-side block during a resize is
        // correct (callers see either all-old or all-new state).
        {
            let mut always_on = self.inner.always_on.write().unwrap();
            let old_handles: Vec<EncoderHandle> = std::mem::take(&mut *always_on);
            for handle in &old_handles {
                handle.shutdown.cancel();
            }
            // Drop every old handle. We don't iterate them as the
            // source of new layers — instead we re-invoke the
            // factory with new dims to get the canonical layout
            // for the new source. This is what makes resize
            // idempotent across round-trip (e.g. 64×64 → 60×48 →
            // 64×64 restores any layers dropped at 60×48) and
            // drift-free (e.g. 1366×768 → 1920×1080 produces
            // 960×540 half, not 958×540 derived from the rounded
            // 682×384 intermediate).
            drop(old_handles);

            let new_layers = (self.inner.layer_factory)(new_width, new_height);
            for layer in new_layers {
                let id = EncoderId::new(BASELINE_CODEC, layer.rid.clone());
                match try_spawn_encoder_thread(
                    id.clone(),
                    layer,
                    new_width,
                    new_height,
                    &self.inner.i420_tx,
                    self.inner.duration_ms,
                    &self.inner.counters,
                ) {
                    Ok(new_handle) => always_on.push(new_handle),
                    // Same per-platform policy as `EncoderPool::new`:
                    // panic on macOS/Linux (VP8 always reconstructs),
                    // log + degrade on Windows (a resize to a resolution
                    // the MF H.264 MFT rejects must not crash the daemon —
                    // the bank is left without this layer until the next
                    // resize regenerates it).
                    Err(e) => baseline_encoder_construction_failed(&id, &e),
                }
            }
        }

        // Drop on-demand slots entirely. We do NOT respawn them in
        // place because that would create a lifetime mismatch: the
        // new slot would inherit the same `EncoderId` but have
        // (correctly) a new generation, so any existing lease for
        // the old slot would not match and the refcount transferred
        // from the old slot would be orphaned — the new slot would
        // sit at non-zero refcount nobody owns, leaking a live
        // encoder thread that no forwarder ever claimed.
        //
        // Dropping the slots instead matches the subscribe path's
        // natural recovery: a forwarder whose Receiver sees Closed
        // calls `subscribe` again, which re-spawns the encoder at
        // the current source dimensions with refcount=1 and a fresh
        // generation. Old leases that haven't dropped yet find
        // nothing in the map on release and skip — the generation
        // check in `release_impl` handles the case where a newer
        // subscribe reinstalled before an older lease dropped.
        //
        // Failure mode: if no forwarder re-subscribes (e.g. the
        // peer's RTCPeerConnection closed concurrently with
        // on_resize), the codec simply isn't served until someone
        // asks for it — which is what we want; zombie slots at
        // refcount=0 would be a CPU leak.
        {
            let mut on_demand = self.inner.on_demand.lock().unwrap();
            let old_slots: HashMap<EncoderId, OnDemandSlot> = std::mem::take(&mut *on_demand);
            for (_id, old_slot) in old_slots {
                old_slot.handle.shutdown.cancel();
            }
        }
    }

    /// Push one I420 frame into the pool. Bridge calls this for every
    /// I420 frame produced from a fresh BGRA capture (and during
    /// idle-heartbeat ticks).
    ///
    /// Lossy: returns the count of currently-subscribed encoders that
    /// will receive the frame, but if any individual encoder's
    /// broadcast receiver lags, that encoder skips and self-recovers
    /// at next frame. Does not backpressure the bridge — by design,
    /// because backpressure here would stall every other encoder for
    /// one slow one.
    pub fn push_i420_frame(&self, data: Arc<Vec<u8>>, arrived: Instant) -> usize {
        self.push_i420_frame_with_visual_marker(data, arrived, None)
    }

    /// Push one I420 frame with an optional diagnostic visual-marker value.
    ///
    /// The bridge stamps the source I420 frame before broadcasting to the pool.
    /// Downscaled layers would otherwise shrink that marker, so encoder
    /// threads re-stamp the same value after per-layer downscale when this is
    /// `Some`.
    pub fn push_i420_frame_with_visual_marker(
        &self,
        data: Arc<Vec<u8>>,
        arrived: Instant,
        visual_marker_value: Option<u32>,
    ) -> usize {
        // broadcast::send returns the receiver count on success, or
        // SendError if there are zero receivers (no encoders running).
        // Both are normal: the bridge keeps feeding regardless of
        // whether anyone is listening.
        self.inner
            .i420_tx
            .send(I420Frame {
                data,
                arrived,
                visual_marker_value,
            })
            .unwrap_or(0)
    }

    /// Subscribe a peer to the encoders in the pool that can serve its
    /// codec preferences. Returns one [`EncoderSubscription`] per
    /// codec that the peer supports AND the pool can produce right
    /// now, paired with a [`PoolLease`] that holds the refcounts for
    /// any on-demand encoders this subscribe bumped.
    ///
    /// Synchronous: uses a `std::sync::Mutex` internally so the call
    /// doesn't `await` and `PoolLease::drop` can release without a
    /// runtime. Safe to call from async contexts — the critical
    /// section is brief (map insert + encoder-thread spawn on the
    /// std::thread side, no I/O).
    ///
    /// Resize-race retry: subscribe runs `subscribe_once` in a loop
    /// of up to [`MAX_SUBSCRIBE_ATTEMPTS`] attempts. When the inner
    /// attempt detects an `on_resize` raced its off-lock construction
    /// AND would otherwise return [`SubscribeError::NoCompatibleCodec`]
    /// (i.e. all returnable codecs were stale), we retry once with a
    /// fresh source-epoch snapshot — turning a microsecond-window
    /// race into a transparent recovery rather than an offer
    /// rejection. After all attempts hit stale-epoch, we return
    /// `NoCompatibleCodec`; in practice two consecutive races inside
    /// the same call require resize traffic at sub-millisecond
    /// cadence, which would itself be a higher-order bug worth
    /// failing loud on.
    ///
    /// Failure-filtering contract:
    /// - For **always-on** codecs whose layer matches a peer-preferred
    ///   codec: always returns a subscription (always-on encoders are
    ///   constructed at pool creation and known-working).
    /// - For **on-demand** codecs the peer prefers that the pool
    ///   [`Self::on_demand_spawnable`] supports: if the slot is already
    ///   running, bump its refcount and subscribe. If not, synchronously
    ///   construct an encoder via [`super::select_codec_for_mime`];
    ///   on `Ok`, spawn the driver thread and return a subscription;
    ///   on `Err`, log and **skip this codec** (no half-alive slot,
    ///   no ghost subscription).
    /// - Codecs the peer prefers that the pool cannot produce (no
    ///   always-on match AND not spawnable) are silently skipped.
    ///
    /// If the filtered result set is empty, returns
    /// [`SubscribeError::NoCompatibleCodec`] — the caller (WebRTC
    /// offer handler) should reject the offer rather than build a
    /// peer that would see a silent never-decoding stream.
    pub fn subscribe(
        &self,
        prefs: &PeerCodecPreferences,
    ) -> Result<(Vec<EncoderSubscription>, PoolLease), SubscribeError> {
        for attempt in 0..MAX_SUBSCRIBE_ATTEMPTS {
            match self.subscribe_once(prefs) {
                SubscribeAttemptOutcome::Done(result) => return result,
                SubscribeAttemptOutcome::StaleEpochRetry => {
                    eprintln!(
                        "[encoder/pool] subscribe: stale-epoch detected on \
                         attempt {} — retrying with fresh source dimensions",
                        attempt + 1,
                    );
                    continue;
                }
            }
        }
        // Every attempt hit a resize race. This means on_resize is
        // firing faster than subscribe's pass-2 construct can
        // complete — should be impossible under normal operation
        // (resize is rare; pass 2 is microseconds). Return the
        // standard NoCompatibleCodec; caller (offer handler) treats
        // it as transient and retries on next offer.
        Err(SubscribeError::NoCompatibleCodec)
    }

    fn subscribe_once(&self, prefs: &PeerCodecPreferences) -> SubscribeAttemptOutcome {
        let mut subs = Vec::new();
        let mut always_on_codecs: Vec<CodecKind> = Vec::new();

        // Atomic snapshot of (width, height, epoch). Pass 1 builds
        // on-demand `LayerSpec::single` from `snapshot.{width, height}`
        // so the encoder we construct in pass 2 corresponds exactly
        // to `snapshot.gen`; pass 3 then compares the current epoch
        // against `snapshot.gen` under the on-demand lock to detect
        // a `on_resize` that fired during construction. Single
        // critical section under the source-state read lock means
        // the snapshot can't tear (an earlier design captured the
        // epoch separately from the dimensions and was vulnerable
        // to torn reads where the gen and dimensions came from
        // opposite sides of a resize).
        //
        // Always-on subs aren't affected because their handles live
        // in `self.inner.always_on`, which `on_resize` swaps
        // atomically under its own write lock — their subscribe()
        // receivers observe Closed via the normal broadcast path if
        // a resize happened before they're consumed.
        let source_at_start = self.snapshot_source();

        // Always-on: no refcount, subscribe-only. On macOS/Linux these
        // are guaranteed to be producing frames (EncoderPool::new panics
        // on a VP8 baseline construction failure); on Windows the bank may
        // be empty if the Media Foundation H.264 baseline failed to
        // construct (logged + degraded), in which case this loop simply
        // yields no baseline subscription. Read lock is held only for the
        // duration of this iteration; on_resize acquires the write lock to
        // swap handles.
        {
            let always_on = self.inner.always_on.read().unwrap();
            for handle in always_on.iter() {
                if prefs.supports(handle.id.codec) {
                    subs.push(EncoderSubscription {
                        id: handle.id.clone(),
                        layer: handle.layer.clone(),
                        frames: handle.subscribe(),
                    });
                }
                if !always_on_codecs.contains(&handle.id.codec) {
                    always_on_codecs.push(handle.id.codec);
                }
            }
        }

        // On-demand: for codecs the peer wants that aren't in
        // always_on, spawn + refcount. The mutex is held ONLY for the
        // existence check and the install step — construction
        // (select_codec_for_mime, which for H.264 on Linux runs
        // `ffmpeg -version` + `Command::spawn`, see h264_linux.rs) runs
        // off-lock so it doesn't block other subscribe / release /
        // request_keyframe callers on an async tokio worker. Race
        // handling for concurrent subscribes is below.
        //
        // Each entry records the slot's `generation` so release can
        // tell "this lease holds the slot I inserted" apart from
        // "this lease holds a predecessor that was already replaced."
        let mut on_demand_refs: Vec<(EncoderId, u64)> = Vec::new();

        // Pass 1 (lock held, fast path only): for every codec already
        // running, bump refcount and emit the subscription. Collect
        // the codecs that still need construction into a worklist.
        let mut to_construct: Vec<(CodecKind, EncoderId, LayerSpec)> = Vec::new();
        {
            let mut on_demand = self.inner.on_demand.lock().unwrap();
            for &codec in &prefs.supported {
                if always_on_codecs.contains(&codec) {
                    continue;
                }
                if !Self::on_demand_spawnable(codec) {
                    continue;
                }
                let rid = SimulcastRid::full();
                let id = EncoderId::new(codec, rid);
                if let Some(slot) = on_demand.get_mut(&id) {
                    slot.refcount += 1;
                    on_demand_refs.push((id.clone(), slot.generation));
                    subs.push(EncoderSubscription {
                        id: slot.handle.id.clone(),
                        layer: slot.handle.layer.clone(),
                        frames: slot.handle.subscribe(),
                    });
                } else {
                    // Use the dimensions from the snapshot captured
                    // at function entry — same gen, same dims,
                    // checked together by pass 3.
                    let layer = LayerSpec::single(
                        codec,
                        source_at_start.width,
                        source_at_start.height,
                        self.inner.framerate,
                    );
                    to_construct.push((codec, id, layer));
                }
            }
        } // lock released here

        // Pass 2 (lock released): construct each needed encoder. This
        // is the slow/blocking work (subprocess spawn, codec init, VAAPI
        // probe). Two concurrent subscribe calls asking for the same
        // codec may both land here — pass 3 deduplicates via the race
        // check.
        let mut constructed: Vec<(EncoderId, EncoderHandle, LayerSpec)> = Vec::new();
        for (_codec, id, layer) in to_construct {
            match try_spawn_encoder_thread(
                id.clone(),
                layer.clone(),
                // On-demand encoders are always at source dim
                // (LayerSpec::single uses snapshot dims), so
                // needs_downscale is false for them — passing
                // source dims here is a no-op the encoder thread
                // never exercises. Threading them anyway keeps
                // the API uniform with the always-on case.
                source_at_start.width,
                source_at_start.height,
                &self.inner.i420_tx,
                self.inner.duration_ms,
                &self.inner.counters,
            ) {
                Ok(handle) => constructed.push((id, handle, layer)),
                Err(e) => {
                    eprintln!(
                        "[encoder/pool] on-demand {} construction failed, \
                         excluding from subscription: {}",
                        id, e,
                    );
                    // fall through — this codec is skipped, peer falls
                    // back to whatever else it supports (if anything).
                }
            }
        }

        // Pass 3 (lock held): install constructed encoders, handling
        // two races:
        //   (a) another subscribe installed the same slot while our
        //       construction was off-lock (install-race, existing),
        //   (b) on_resize fired while our construction was off-lock
        //       (resize-race, new in 3c.3a3): any constructed
        //       encoder we have was built at pre-resize dimensions
        //       and would receive post-resize I420 if installed.
        if !constructed.is_empty() {
            let mut on_demand = self.inner.on_demand.lock().unwrap();
            // Check for the resize race (b) under the lock — the
            // bumped epoch combined with on_resize's on-demand
            // teardown means if source_gen advanced since pass 1,
            // every entry in `constructed` is stale. Cancel them
            // all; the subscribe result returns whatever pass 1's
            // fast-path + always-on slots supplied, or
            // NoCompatibleCodec if that set is empty. The caller
            // (WebRTC offer handler / forwarder) treats this as a
            // transient subscribe failure and retries on the next
            // offer/reconnect — same semantics as any other
            // encoder construction failure.
            let stale_epoch = self.snapshot_source().gen != source_at_start.gen;
            if stale_epoch {
                for (id, handle, _layer) in &constructed {
                    eprintln!(
                        "[encoder/pool] subscribe: cancelling stale-dimensions \
                         encoder for {id:?} — on_resize fired during construction"
                    );
                    handle.shutdown.cancel();
                }
                // Always retry on stale, regardless of whether
                // pass 1's always-on / fast-path slots already
                // populated `subs`. Returning a partial result here
                // would silently drop the codec the peer might have
                // negotiated their SDP around — without 3c.3b.2's
                // narrowed-negotiation contract (WebRtcPeer's enabled
                // codec set derived from RETURNED subscriptions, not
                // from the original peer prefs), the peer could pick
                // an SDP codec we won't actually serve and see a
                // black stream.
                //
                // Drop any always-on / fast-path subs we'd built so
                // their broadcast Receivers don't leak (each one
                // holds an entry in the encoder's broadcast Sender's
                // subscriber list; on retry we'll get fresh ones).
                // The `on_demand_refs` accumulated for this attempt
                // are also dropped — they were claims against slots
                // that may already be torn down by on_resize, so the
                // gen-check in PoolLease::release_impl would skip
                // them anyway, but the explicit drop here is
                // clearer.
                drop(on_demand);
                drop(subs);
                drop(on_demand_refs);
                return SubscribeAttemptOutcome::StaleEpochRetry;
            }
            for (id, handle, _layer) in constructed {
                match on_demand.get_mut(&id) {
                    Some(existing) => {
                        // Race loss: another subscribe installed this
                        // slot first. Bump their refcount, cancel our
                        // orphan encoder. Brief CPU waste on the
                        // orphan until the cancellation token observes
                        // and the thread exits; refcount stays
                        // consistent.
                        existing.refcount += 1;
                        on_demand_refs.push((id.clone(), existing.generation));
                        subs.push(EncoderSubscription {
                            id: existing.handle.id.clone(),
                            layer: existing.handle.layer.clone(),
                            frames: existing.handle.subscribe(),
                        });
                        handle.shutdown.cancel();
                    }
                    None => {
                        // Race win: no slot yet. Install ours with a
                        // fresh generation from the pool-level
                        // counter.
                        let generation = self.inner.slot_gen_counter.fetch_add(1, Ordering::SeqCst);
                        let slot = OnDemandSlot {
                            handle: handle.clone(),
                            refcount: 1,
                            generation,
                        };
                        on_demand.insert(id.clone(), slot);
                        on_demand_refs.push((id.clone(), generation));
                        subs.push(EncoderSubscription {
                            id: handle.id.clone(),
                            layer: handle.layer.clone(),
                            frames: handle.subscribe(),
                        });
                    }
                }
            }
        }

        if subs.is_empty() {
            return SubscribeAttemptOutcome::Done(Err(SubscribeError::NoCompatibleCodec));
        }

        SubscribeAttemptOutcome::Done(Ok((
            subs,
            PoolLease {
                pool: Arc::clone(&self.inner),
                on_demand_refs,
                released: AtomicBool::new(false),
            },
        )))
    }

    /// Request a keyframe from one encoder (or all layers of one codec
    /// if `rid` is `None`). Coalesced — multiple callers within
    /// [`KEYFRAME_COALESCE_WINDOW`] result in one request.
    ///
    /// Returns `true` if the request was forwarded to the encoder
    /// (i.e. coalescer admitted it AND a matching encoder exists),
    /// `false` if it was deduped against a recent request OR if no
    /// encoder matched the `(codec, rid)` lookup.
    ///
    /// Called by the per-peer forwarder when inbound RTCP requests a
    /// keyframe for that peer.
    pub fn request_keyframe(&self, codec: CodecKind, rid: Option<SimulcastRid>) -> bool {
        // Coalesce per (codec, rid). When rid is None we coalesce
        // against the full layer (callers using None typically mean
        // "any layer is fine, just give me a keyframe").
        let rid = rid.unwrap_or_else(SimulcastRid::full);
        if !self.inner.keyframe_coalescer.should_request(codec, &rid) {
            return false;
        }
        let id = EncoderId::new(codec, rid);
        // Always-on first. Read-only iteration; on_resize's writer
        // waits until this read guard drops before swapping handles.
        {
            let always_on = self.inner.always_on.read().unwrap();
            for handle in always_on.iter() {
                if handle.id == id {
                    handle.force_keyframe.store(true, Ordering::SeqCst);
                    return true;
                }
            }
        }
        // On-demand.
        let on_demand = self.inner.on_demand.lock().unwrap();
        if let Some(slot) = on_demand.get(&id) {
            slot.handle.force_keyframe.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    /// Request a keyframe from EVERY active encoder in the pool —
    /// always-on layers + on-demand encoders that currently have at
    /// least one subscriber (`refcount > 0`). Each individual request
    /// goes through [`Self::request_keyframe`] so it's coalesced per
    /// `(codec, rid)` against the same window: a second peer joining
    /// in the same beat as the first does NOT produce a second
    /// keyframe per encoder. PLI-storm safe.
    ///
    /// Returns the count of admitted requests (i.e. how many encoders
    /// will produce a forced keyframe on their next encode). Useful
    /// for tests; production callers can ignore.
    ///
    /// **Call site: peer-join.** Called by
    /// [`crate::display::DisplaySession::handle_offer_pool_mode`]
    /// after the new peer's pool subscription is in place. Without
    /// this, a peer joining during an idle desktop would wait up to
    /// one GOP boundary (and for VP8 on static content, potentially
    /// much longer) for a natural keyframe before its decoder could
    /// produce a visible image. Mirrors the legacy path's
    /// `keyframe_tx.send(())` from b241cf5.
    ///
    /// On-demand encoders with `refcount == 0` are skipped: they
    /// have no consumer that would benefit, and an on-demand spawn
    /// emits a cold-start keyframe naturally on first encode.
    pub fn request_keyframe_all(&self) -> usize {
        let always_on_ids = self.always_on_ids();
        let on_demand_ids: Vec<EncoderId> = {
            let on_demand = self.inner.on_demand.lock().unwrap();
            on_demand
                .iter()
                .filter(|(_, slot)| slot.refcount > 0)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut count = 0usize;
        for id in always_on_ids.into_iter().chain(on_demand_ids) {
            if self.request_keyframe(id.codec, Some(id.rid)) {
                count += 1;
            }
        }
        count
    }

    /// Snapshot the current always-on encoder IDs.
    ///
    /// Returns a `Vec` (not a borrow) so callers don't hold the
    /// `always_on` read lock across their loop body — important for
    /// the 4d.2 aggregator, whose action closure calls back into
    /// [`Self::pause_layer`] / [`Self::resume_layer`] (which take the
    /// same lock).
    ///
    /// Reflects the **current** pool state at call time, including
    /// any post-[`Self::on_resize`] layer-set changes. The aggregator
    /// queries this on every action rather than snapshotting at
    /// session start, so a session that begins with a small layer set
    /// (e.g. only `full` because the source dims filter out
    /// `half`/`quarter` via `vp8_simulcast`'s `normalize_layer_dims`)
    /// and is then resized larger while idle still pauses / resumes
    /// the newly-spawned layers correctly.
    ///
    /// On-demand encoders are NOT included — they're lifecycle-tied
    /// to peer presence already (refcounted by
    /// [`Self::release_on_demand_subset`]) and don't need pause/resume
    /// orchestration. Use [`Self::on_demand_count`] for those.
    pub fn always_on_ids(&self) -> Vec<EncoderId> {
        self.inner
            .always_on
            .read()
            .unwrap()
            .iter()
            .map(|h| h.id.clone())
            .collect()
    }

    /// **Phase 4d.0**: pause one encoder slot, identified by
    /// `(codec, rid)`. The slot's encoder thread keeps running and
    /// keeps draining its `i420_rx` broadcast subscription (so the
    /// channel doesn't lag), but skips the downscale + encode +
    /// broadcast step entirely. Resume via [`Self::resume_layer`].
    ///
    /// Returns `true` if a matching slot was found and its pause flag
    /// flipped to `true`; `false` if no slot exists for `(codec, rid)`.
    /// Idempotent: pausing an already-paused slot returns `true` and
    /// is a no-op for the encoder thread.
    ///
    /// Searches always-on slots first, then on-demand. Mirrors the
    /// lookup pattern in [`Self::request_keyframe`] so a future code
    /// path that does both (e.g. a layer-selection policy
    /// pause-then-resume) sees consistent semantics.
    ///
    /// Used by the layer-selection policy: 4d.2's zero-peer
    /// aggregator pauses all always-on simulcast layers after a
    /// debounce window at zero peers (CPU saver during idle); 4d.3
    /// will add receiver-feedback-driven pause for individual
    /// over-budget layers when a peer's link health degrades.
    /// Direct callers in production should be the aggregator;
    /// peer-side code never calls this directly.
    pub fn pause_layer(&self, codec: CodecKind, rid: SimulcastRid) -> bool {
        let id = EncoderId::new(codec, rid);
        {
            let always_on = self.inner.always_on.read().unwrap();
            for handle in always_on.iter() {
                if handle.id == id {
                    handle.paused.store(true, Ordering::SeqCst);
                    return true;
                }
            }
        }
        let on_demand = self.inner.on_demand.lock().unwrap();
        if let Some(slot) = on_demand.get(&id) {
            slot.handle.paused.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    /// **Phase 4d.0**: resume an encoder slot previously paused via
    /// [`Self::pause_layer`]. Returns `true` if a matching slot was
    /// found; `false` if no slot exists for `(codec, rid)`.
    /// Idempotent: resuming an already-active slot is a no-op.
    ///
    /// Resume is fast — just an atomic flag flip — so the next
    /// captured frame after the flip is encoded normally. Within
    /// one capture interval (~33ms at 30fps) the resumed layer is
    /// producing again.
    ///
    /// **Forces a keyframe on the paused → active transition.**
    /// Without this, the first post-resume frame is a P-frame
    /// referencing decoder state from BEFORE the pause — and the
    /// decoder either:
    ///   - has stale state from before the pause (timed out, dropped
    ///     reference frames during the gap → corruption / black until
    ///     the next natural keyframe), or
    ///   - is brand-new (a viewer subscribed to this layer DURING
    ///     the pause, expecting the resumed stream to be
    ///     immediately decodable → black until the next keyframe).
    ///
    /// The transition detection uses `swap(false, SeqCst)`: when the
    /// previous value was `true`, this is the paused → active edge
    /// and we set `force_keyframe = true`. Repeated resume calls on
    /// an already-active slot see swap-returns-false and skip the
    /// keyframe force — preserves idempotency without re-firing
    /// keyframes on every resume call.
    ///
    /// (A `force_keyframe` set externally during the pause window
    /// also survives, because the encoder thread's swap on
    /// `force_keyframe` only runs after the pause check and was
    /// skipped while paused. So the first post-resume encode picks
    /// up either the resume-edge force or the externally-requested
    /// force — both produce a keyframe.)
    pub fn resume_layer(&self, codec: CodecKind, rid: SimulcastRid) -> bool {
        let id = EncoderId::new(codec, rid);
        {
            let always_on = self.inner.always_on.read().unwrap();
            for handle in always_on.iter() {
                if handle.id == id {
                    let was_paused = handle.paused.swap(false, Ordering::SeqCst);
                    if was_paused {
                        handle.force_keyframe.store(true, Ordering::SeqCst);
                    }
                    return true;
                }
            }
        }
        let on_demand = self.inner.on_demand.lock().unwrap();
        if let Some(slot) = on_demand.get(&id) {
            let was_paused = slot.handle.paused.swap(false, Ordering::SeqCst);
            if was_paused {
                slot.handle.force_keyframe.store(true, Ordering::SeqCst);
            }
            return true;
        }
        false
    }

    /// **Phase 4d.0**: query the pause state of an encoder slot.
    /// Returns `Some(true)` if paused, `Some(false)` if active,
    /// `None` if no slot exists for `(codec, rid)`.
    ///
    /// Caller-visible distinction between "paused" and "no slot" lets
    /// the aggregator (4d.2) tell apart "I asked for a layer that
    /// doesn't exist" (bug — should never reach here in production)
    /// from "the layer is paused" (expected steady state under
    /// bandwidth-constrained conditions).
    pub fn is_layer_paused(&self, codec: CodecKind, rid: SimulcastRid) -> Option<bool> {
        let id = EncoderId::new(codec, rid);
        {
            let always_on = self.inner.always_on.read().unwrap();
            for handle in always_on.iter() {
                if handle.id == id {
                    return Some(handle.paused.load(Ordering::SeqCst));
                }
            }
        }
        let on_demand = self.inner.on_demand.lock().unwrap();
        if let Some(slot) = on_demand.get(&id) {
            return Some(slot.handle.paused.load(Ordering::SeqCst));
        }
        None
    }

    /// Test-only access to the always-on handles. Lets tests verify
    /// pool composition without exposing internals to production code.
    ///
    /// Returns a read guard; callers hold it for the duration of the
    /// slice's use. `RwLock` backing means on_resize waits for all
    /// test guards to drop before swapping handles — fine for tests
    /// (short critical sections) and matches production's reader
    /// pattern.
    #[cfg(test)]
    pub(crate) fn always_on(&self) -> std::sync::RwLockReadGuard<'_, Vec<EncoderHandle>> {
        self.inner.always_on.read().unwrap()
    }

    /// Test-only access to on-demand slot counts. Lets tests verify
    /// refcount + teardown semantics without exposing the map.
    #[cfg(test)]
    pub(crate) fn on_demand_refcount(&self, codec: CodecKind, rid: SimulcastRid) -> Option<usize> {
        let id = EncoderId::new(codec, rid);
        let map = self.inner.on_demand.lock().unwrap();
        map.get(&id).map(|slot| slot.refcount)
    }

    /// Test-only access to the source-generation epoch. Lets tests
    /// assert that `on_resize` actually bumped it — the contract the
    /// subscribe-race fix depends on. Not exposed in production
    /// because no hot-path caller needs it; the race check lives
    /// inside subscribe itself.
    #[cfg(test)]
    pub(crate) fn source_gen(&self) -> u64 {
        self.snapshot_source().gen
    }
}

impl Drop for EncoderPoolInner {
    fn drop(&mut self) {
        // Cancel encoder shutdowns explicitly so threads exit on the
        // next iteration even if they're blocked in blocking_recv —
        // CancellationToken::cancel wakes any future await but for the
        // std::thread blocking case the second signal (i420_tx drop
        // closing the channel) is what actually wakes them. Both run:
        // Cancel sets the flag for the loop's per-iter check, then
        // dropping the broadcast sender below closes the channel and
        // the recv returns Err(Closed) immediately.
        if let Ok(always_on) = self.always_on.read() {
            for handle in always_on.iter() {
                handle.shutdown.cancel();
            }
        }
        // try_lock avoids blocking Drop if the mutex is contended (e.g.
        // a subscribe or release racing pool teardown). If we can't
        // acquire cleanly we skip explicit cancellation and rely on
        // the i420_tx-drop backstop — both paths converge on thread
        // exit, just at different latencies.
        if let Ok(slots) = self.on_demand.try_lock() {
            for slot in slots.values() {
                slot.handle.shutdown.cancel();
            }
        }
        // i420_tx (the one Sender) drops when this struct's fields go
        // out of scope after this method returns. That closes the
        // broadcast and unblocks every encoder thread's blocking_recv.
    }
}

// ---------------------------------------------------------------------------
// Encoder thread spawn
// ---------------------------------------------------------------------------

/// Spawn one encoder thread for the given layer, returning its
/// [`EncoderHandle`]. The thread:
///
/// 1. Constructs the codec's encoder backend via
///    [`super::select_codec_for_mime`] — **on the encoder thread
///    itself** (see below).
/// 2. Subscribes to the pool's I420 broadcast.
/// 3. In a `blocking_recv` loop: pulls the next I420 frame, swaps the
///    `force_keyframe` flag, calls `encoder.encode(...)`, and
///    broadcasts each produced packet (wrapped in
///    `Arc<EncodedFrame>`) to the per-encoder frames channel.
/// 4. Exits when `shutdown` is cancelled OR the I420 broadcast closes
///    (sender dropped at pool drop).
///
/// **Construct-on-the-driver-thread.** The encoder is built inside the
/// spawned thread and then *used and dropped* on that same thread for
/// its entire life. This is load-bearing for the Windows Media
/// Foundation backend ([`super::h264_windows`]), whose `new()` calls
/// `CoInitializeEx` + `MFStartup` and whose `Drop` calls `MFShutdown` +
/// `CoUninitialize` — COM init/teardown is **per-thread**, so the same
/// thread that initializes COM must be the one that touches and releases
/// the COM objects. The other backends (VP8/libvpx, VideoToolbox,
/// ffmpeg) have no per-thread state and are unaffected; their
/// construction code is unchanged — only the thread on which
/// `select_codec_for_mime` runs has moved.
///
/// **No ghost handles.** Construction can still fail (a host without a
/// usable H.264 MFT, libvpx ABI mismatch, …) and the contract is that a
/// failed construct must *not* publish an [`EncoderHandle`] — callers
/// (the on-demand subscribe path) rely on the error to exclude the codec
/// from the subscription set rather than hand back a subscription to an
/// encoder that will never emit a frame. To keep that contract while
/// moving construction onto the thread, the thread reports the
/// construction outcome back over a one-shot startup channel and this
/// function blocks until it arrives: on `Err` we return the error (the
/// thread has already exited, nothing was published); on `Ok` we return
/// the `EncoderHandle`. This replaces the original design where the
/// caller constructed synchronously and moved the boxed encoder into the
/// thread — which worked for libvpx/VideoToolbox/ffmpeg but constructed
/// the Windows MF encoder on a Tokio worker only to use and drop it on
/// the encoder thread, a latent cross-thread-COM hazard.
fn try_spawn_encoder_thread(
    id: EncoderId,
    layer: LayerSpec,
    source_w: u32,
    source_h: u32,
    i420_tx: &broadcast::Sender<I420Frame>,
    duration_ms: u64,
    counters: &Arc<crate::display::DisplayMetricsCounters>,
) -> Result<EncoderHandle, String> {
    // The construction parameters captured for the driver thread. The
    // thread runs `select_codec_for_mime` so any per-thread codec state
    // (Windows COM/MF) is initialized on the thread that will use it.
    let mime = id.codec.mime();
    let (cw, ch, cbr) = (layer.width, layer.height, layer.target_bitrate_kbps);
    let construct = move || super::select_codec_for_mime(mime, cw, ch, cbr).map(|(enc, _)| enc);
    spawn_encoder_thread_with(
        id,
        layer,
        source_w,
        source_h,
        construct,
        i420_tx,
        duration_ms,
        counters,
    )
}

/// 3c.3b.4f: per-encoder silent-output watchdog.
///
/// Detects encoders that accept input but produce no encoded output
/// (the canonical failure mode is `h264_vaapi` on hosts where
/// virtio-gpu video acceleration is half-broken: `vaInitialize`
/// "succeeds" but no NAL units ever come out of stdout). After
/// [`ENCODER_SILENT_FRAMES_THRESHOLD`] consecutive silent encodes
/// the watchdog asks the caller to attempt a fallback once; after
/// the swap (or after the swap-attempt fails) the watchdog stops
/// firing for the lifetime of this encoder thread.
///
/// Catches `h264_vaapi` silent-failure (vaInitialize succeeds, no
/// NALs ever emitted) — without this the stream would stay black
/// indefinitely on hosts where VAAPI claims to work but doesn't.
struct WatchdogState {
    frames_since_last_output: u64,
    swap_done: bool,
}

/// 30 frames ≈ 1s at 30fps, well above the normal 1–2 frame
/// pipeline depth for any healthy encoder.
const ENCODER_SILENT_FRAMES_THRESHOLD: u64 = 30;

impl WatchdogState {
    fn new() -> Self {
        Self {
            frames_since_last_output: 0,
            swap_done: false,
        }
    }

    /// Record the result of one `encoder.encode` call. `produced` is
    /// the number of encoded packets emitted (zero on silent-success
    /// AND on encode-error — both are "no output reached the wire").
    /// Returns `true` if the caller should attempt a fallback swap
    /// AFTER this call. The watchdog never returns `true` more than
    /// once per encoder lifetime — a failed fallback swap doesn't
    /// re-arm.
    fn record(&mut self, produced: usize) -> bool {
        if produced > 0 {
            self.frames_since_last_output = 0;
            return false;
        }
        if self.swap_done {
            return false;
        }
        self.frames_since_last_output += 1;
        if self.frames_since_last_output >= ENCODER_SILENT_FRAMES_THRESHOLD {
            self.swap_done = true;
            self.frames_since_last_output = 0;
            return true;
        }
        false
    }
}

/// 3c.3b.4f: pool-path counterpart to `mod.rs::try_h264_fallback`.
///
/// Invariants:
///   - only fires for H.264 (no fallback for VP8 — libvpx doesn't
///     exhibit the silent-failure pattern)
///   - the new encoder constructs cleanly
///
/// **3c.3b.4g:** the previous version ALSO early-returned when
/// `is_vaapi_banned()` was already true, on the assumption "we're
/// already on libx264 so there's nothing to swap to." That
/// assumption holds for an encoder constructed AFTER a ban, but
/// fails for encoders constructed BEFORE a sibling watchdog set
/// the ban: the second watchdog would see the ban, return None,
/// and leave a pre-ban VAAPI encoder stranded on the broken path
/// forever. Multi-H.264-pool-slot and mixed pool/legacy sessions
/// can both reach this state. Fix: drop the early-return, treat
/// `ban_vaapi()` as the idempotent no-op it is, and always attempt
/// construction. At worst an already-libx264 encoder respawns
/// libx264 once (a one-time waste; the watchdog latches and won't
/// fire again on this thread); at best a pre-ban VAAPI encoder
/// gets the libx264 it would otherwise miss.
///
/// Layer-aware: takes the existing [`LayerSpec`] so the replacement
/// encoder is configured for the same width / height / bitrate /
/// framerate as the original. The legacy mod.rs version takes raw
/// `(width, height, bitrate=2000)` because legacy has only one
/// shared encoder; pool has one per layer, each with its own spec.
///
/// On non-Linux targets there's no VA-API path to ban, so this is
/// a no-op — same as the legacy `#[cfg(not(target_os = "linux"))]`
/// arm.
#[cfg(target_os = "linux")]
fn try_h264_fallback_for_layer(
    codec: CodecKind,
    layer: &LayerSpec,
) -> Option<Box<dyn super::Encoder>> {
    if codec != CodecKind::H264 {
        return None;
    }
    // Idempotent — see VAAPI_BANNED in h264_linux.rs (one-way
    // AtomicBool that's never cleared). Calling when already
    // banned is a no-op store.
    super::h264_linux::ban_vaapi();
    match super::select_codec_for_mime(
        codec.mime(),
        layer.width,
        layer.height,
        layer.target_bitrate_kbps,
    ) {
        Ok((enc, _)) => Some(enc),
        Err(e) => {
            eprintln!(
                "[encoder/pool] watchdog: libx264 fallback creation failed: {}",
                e,
            );
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn try_h264_fallback_for_layer(
    _codec: CodecKind,
    _layer: &LayerSpec,
) -> Option<Box<dyn super::Encoder>> {
    None
}

/// Spawn the encoder driver thread, constructing the [`super::Encoder`]
/// **inside that thread** via the `construct` closure.
///
/// Blocks until the thread reports its construction outcome over a
/// one-shot startup channel: returns `Err` (and publishes no handle) if
/// `construct` failed, or the running [`EncoderHandle`] once the encoder
/// is built. Constructing on the thread that will use and drop the
/// encoder is what makes the Windows MF backend's per-thread COM
/// init/teardown correct; see [`try_spawn_encoder_thread`].
fn spawn_encoder_thread_with(
    id: EncoderId,
    layer: LayerSpec,
    source_w: u32,
    source_h: u32,
    construct: impl FnOnce() -> Result<Box<dyn super::Encoder>, String> + Send + 'static,
    i420_tx: &broadcast::Sender<I420Frame>,
    duration_ms: u64,
    counters: &Arc<crate::display::DisplayMetricsCounters>,
) -> Result<EncoderHandle, String> {
    let (frames_tx, _) = broadcast::channel::<Arc<EncodedFrame>>(ENCODER_FRAME_BROADCAST_CAPACITY);
    let force_keyframe = Arc::new(AtomicBool::new(false));
    // Phase 4d.0: paused defaults to false. Layer-selection policy
    // flips this via [`EncoderPool::pause_layer`] /
    // [`EncoderPool::resume_layer`]: 4d.2 pauses all simulcast
    // layers after a debounce at zero peers (CPU saver during
    // idle); 4d.3 will pause individual over-budget layers when
    // receiver feedback (RTCP RR fraction_lost et al.) shows a
    // peer's link can't sustain them.
    let paused = Arc::new(AtomicBool::new(false));
    let shutdown = CancellationToken::new();

    let mut i420_rx = i420_tx.subscribe();
    let frames_tx_for_thread = frames_tx.clone();
    let force_kf_for_thread = Arc::clone(&force_keyframe);
    let paused_for_thread = Arc::clone(&paused);
    let shutdown_for_thread = shutdown.clone();
    let id_for_log = id.clone();
    // 3c.3b.4f: watchdog needs the codec + layer to attempt a
    // fallback if the encoder goes silent. CodecKind is Copy;
    // LayerSpec clones cheaply (no Arc, no Vec — just primitives
    // + a SimulcastRid String).
    let codec_for_thread = id.codec;
    let layer_for_thread = layer.clone();
    // Phase 4a: per-layer downscale. The bridge pushes I420 at the
    // source dims; this encoder is constructed for `layer.width` ×
    // `layer.height`. When they differ (simulcast: half/quarter
    // layers), each frame must be downscaled before encode or the
    // encoder will reject (size mismatch) or mis-encode.
    let needs_downscale = (layer.width, layer.height) != (source_w, source_h);
    let downscale_src_w = source_w;
    let downscale_src_h = source_h;
    let downscale_dst_w = layer.width;
    let downscale_dst_h = layer.height;
    // 3c.3b.4h: per-encoder metrics. Bumped per encoded packet
    // (encode_frames + encode_freshness_us_sum) and on broadcast lag
    // (encode_drops). Counter is shared with DisplaySession via Arc.
    let counters_for_thread = Arc::clone(counters);

    // One-shot startup channel: the thread constructs the encoder and
    // reports `Ok(())` / `Err(reason)` back here before entering its
    // loop. Sized 1 — exactly one message is ever sent. This lets us
    // construct on the encoder thread (correct for Windows per-thread
    // COM/MF) yet still propagate a construction failure to the caller
    // synchronously, so no handle is published for an encoder that
    // could not be built.
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    std::thread::spawn(move || {
        // Construct on THIS thread so any per-thread codec state
        // (Windows COM apartment + Media Foundation) is initialized,
        // used, and torn down all on one thread. On failure, report the
        // error and exit without ever touching the i420 broadcast.
        let mut encoder = match construct() {
            Ok(enc) => {
                // Report success first; if the receiver is already gone
                // (caller dropped), there's nothing to drive, so exit.
                if startup_tx.send(Ok(())).is_err() {
                    return;
                }
                enc
            }
            Err(e) => {
                let _ = startup_tx.send(Err(e));
                return;
            }
        };
        // Drop the startup sender now that the outcome is delivered; the
        // encoder's lifetime is owned entirely by this thread from here.
        drop(startup_tx);
        let mut watchdog = WatchdogState::new();
        // Windows black-frame diagnostic: count encode calls so the
        // hop-by-hop avg-byte logging below self-limits to the first few
        // frames (then stays off the hot path).
        #[cfg(target_os = "windows")]
        let mut diag_frame_count: u64 = 0;

        loop {
            if shutdown_for_thread.is_cancelled() {
                break;
            }
            let frame = match i420_rx.blocking_recv() {
                Ok(f) => f,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Slow encoder fell behind by `n` frames; skip
                    // ahead. Codec keyframe machinery will recover
                    // (next force_keyframe or the encoder's natural
                    // GOP cadence). 3c.3b.4h: count the skipped
                    // frames as encode_drops so the metric reflects
                    // backpressure pressure even when the encoder
                    // itself isn't logging individual drops.
                    // 3c.3b.4i: gate on receiver_count > 0 — an
                    // encoder with zero consumers (e.g. always-on
                    // VP8 during a legacy-only session, or unused
                    // always-on VP8 in an H.264-only pool session)
                    // is producing into a void; counting its lag
                    // would inflate `encode_drops` against work no
                    // peer is waiting for.
                    if frames_tx_for_thread.receiver_count() > 0 {
                        counters_for_thread
                            .encode_drops
                            .fetch_add(n, Ordering::Relaxed);
                    }
                    eprintln!(
                        "[encoder/pool] {} lagged by {} frames, skipping ahead",
                        id_for_log, n
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            // Re-check shutdown after waking from blocking_recv.
            // Between the top-of-loop check and this point, another
            // task may have cancelled our shutdown — typically
            // `EncoderPool::on_resize` for an old-generation handle,
            // or `PoolLease::release_impl` for an on-demand slot
            // whose refcount hit zero. Without this second check the
            // thread would run encode on `frame`, which for the
            // on_resize case is already post-resize data at new
            // dimensions that this encoder (configured for old
            // dimensions) would misinterpret — feeding a stale frame
            // to any still-live subscriber before finally exiting on
            // the next top-of-loop check.
            //
            // Dropping the frame and exiting here is cheaper than
            // restructuring the `blocking_recv` into a tokio select
            // on shutdown (which would require the encoder loop to
            // become async), and matches the semantics every other
            // shutdown-aware Rust loop uses: "if shutdown fires, do
            // not produce another unit of output."
            if shutdown_for_thread.is_cancelled() {
                break;
            }

            // Phase 4d.0: pause check. Done AFTER the shutdown
            // re-check (so a paused encoder that's also shutting
            // down exits cleanly) but BEFORE the force_keyframe
            // swap (so a keyframe request that arrives during pause
            // is preserved across pause→resume — the resume's first
            // encode honors it). Watchdog is also skipped while
            // paused: a long pause should not trip the silent-
            // output threshold and trigger an unnecessary H.264
            // fallback when we resume.
            //
            // Frame is consumed (we already `blocking_recv`'d above)
            // and dropped — we don't try to keep it around for
            // resume because i420 frames are pushed at capture rate
            // (typically 30fps), so the next post-resume blocking_recv
            // returns a fresh frame in ≤33ms. Buffering would just
            // surface a stale frame.
            if paused_for_thread.load(Ordering::SeqCst) {
                continue;
            }

            // No peer is currently reading this encoder's output. Keep draining
            // the shared I420 broadcast so the receiver stays current, but skip
            // all expensive per-layer work (downscale + codec encode). The next
            // subscribed frame will still honor any pending force-keyframe flag
            // because we intentionally check demand before swapping it below.
            if frames_tx_for_thread.receiver_count() == 0 {
                continue;
            }

            let force_kf = force_kf_for_thread.swap(false, Ordering::SeqCst);

            // Phase 4a: per-layer downscale. The bridge pushes I420
            // at source dims; for simulcast layers (half/quarter) the
            // encoder is sized differently and would mis-encode or
            // reject without resizing first. The `needs_downscale`
            // check is computed once at thread spawn, so the hot path
            // for the source-dim layer (always-on full + every
            // on-demand on-source-dim layer) pays nothing.
            let mut scaled_buf;
            let mut stamped_buf;
            let i420_for_encode: &[u8] = if needs_downscale {
                scaled_buf = super::downscale_i420(
                    &frame.data,
                    downscale_src_w,
                    downscale_src_h,
                    downscale_dst_w,
                    downscale_dst_h,
                );
                if let Some(value) = frame.visual_marker_value {
                    let y_len = (downscale_dst_w as usize) * (downscale_dst_h as usize);
                    if let Some(y) = scaled_buf.get_mut(0..y_len) {
                        visual_marker::stamp_y_plane(
                            y,
                            downscale_dst_w as usize,
                            downscale_dst_h as usize,
                            value,
                        );
                    }
                }
                &scaled_buf
            } else if let Some(value) = frame.visual_marker_value {
                stamped_buf = frame.data.as_ref().clone();
                let y_len = (downscale_dst_w as usize) * (downscale_dst_h as usize);
                if let Some(y) = stamped_buf.get_mut(0..y_len) {
                    visual_marker::stamp_y_plane(
                        y,
                        downscale_dst_w as usize,
                        downscale_dst_h as usize,
                        value,
                    );
                }
                &stamped_buf
            } else {
                &frame.data
            };

            // Windows black-frame diagnostic (hop B): log the average byte of
            // the exact I420 slice handed to the encoder for the first few
            // frames. This is the buffer the codec actually sees — if the
            // bridge logged a bright I420 (hop A) but this reads ~0, the frame
            // was lost/zeroed in the pool broadcast or the
            // downscale/visual-marker selection above; if both are bright, the
            // black is inside the encoder (hop C, in h264_windows.rs).
            #[cfg(target_os = "windows")]
            {
                if diag_frame_count < 5 {
                    eprintln!(
                        "[encoder/pool] {} encode-input frame #{} i420 avg={} \
                         (len={}, downscale={})",
                        id_for_log,
                        diag_frame_count + 1,
                        super::sampled_avg_byte(i420_for_encode),
                        i420_for_encode.len(),
                        needs_downscale,
                    );
                }
                diag_frame_count += 1;
            }

            let produced = match encoder.encode(i420_for_encode, duration_ms, force_kf) {
                Ok(packets) => {
                    let n = packets.len();
                    // 3c.3b.4h: latency from capture-arrival to
                    // encoded-packet-emission. Mirrors the legacy
                    // mod.rs::start_encoder_pipeline arithmetic
                    // (one freshness value computed per encode call,
                    // summed in once per packet — multi-packet
                    // outputs accumulate the same value per packet,
                    // matching legacy semantics so average rates
                    // compose cleanly across codecs.
                    // A zero-consumer encoder never reaches this
                    // block; it drains I420 and skips the encode
                    // earlier in the loop.
                    let freshness_us = frame.arrived.elapsed().as_micros() as u64;
                    for pkt in packets {
                        counters_for_thread
                            .encode_frames
                            .fetch_add(1, Ordering::Relaxed);
                        counters_for_thread
                            .encode_freshness_us_sum
                            .fetch_add(freshness_us, Ordering::Relaxed);
                        let ef = Arc::new(pkt.into_encoded_frame());
                        // Lossy broadcast: returns Err only if there
                        // are zero subscribers, which is fine.
                        let _ = frames_tx_for_thread.send(ef);
                    }
                    n
                }
                Err(e) => {
                    eprintln!("[encoder/pool] {} encode error: {}", id_for_log, e);
                    0
                }
            };

            // Silent-output watchdog. After 30 consecutive input
            // frames produced no output, attempt a one-shot fallback
            // swap (Linux H.264 → libx264). Prevents h264_vaapi
            // silent-failure (vaInitialize succeeds, no NALs ever
            // emitted) from black-screening the stream forever.
            if watchdog.record(produced) {
                eprintln!(
                    "[encoder/pool] {} watchdog: {} consecutive input \
                     frames produced no output",
                    id_for_log, ENCODER_SILENT_FRAMES_THRESHOLD,
                );
                if let Some(new_enc) =
                    try_h264_fallback_for_layer(codec_for_thread, &layer_for_thread)
                {
                    eprintln!(
                        "[encoder/pool] {} watchdog: swapped encoder to libx264 fallback",
                        id_for_log,
                    );
                    encoder = new_enc;
                } else {
                    eprintln!(
                        "[encoder/pool] {} watchdog: no fallback available, encoder stays",
                        id_for_log,
                    );
                }
            }
        }
    });

    // Block until the thread reports its construction outcome. A
    // `RecvError` here means the thread panicked or exited before
    // sending — treat that as a construction failure rather than
    // publishing a handle to a dead thread.
    match startup_rx.recv() {
        Ok(Ok(())) => Ok(EncoderHandle {
            id,
            layer,
            frames: frames_tx,
            force_keyframe,
            paused,
            shutdown,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "encoder {id} thread exited before reporting construction outcome"
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Codec-agnostic, allocation-free pool logic tests that run on **every**
/// platform (no `EncoderPool::new`, so no encoder backend is constructed).
/// Kept separate from [`tests`] so the Windows target — where the heavier
/// pool-construction tests are gated off (see that module's note) — still
/// verifies codec identity, the platform baseline, and the pure helper math.
#[cfg(test)]
mod logic_tests {
    use super::*;

    #[test]
    fn codec_kind_mime_round_trip() {
        assert_eq!(CodecKind::Vp8.mime(), super::super::MIME_TYPE_VP8);
        assert_eq!(CodecKind::H264.mime(), super::super::MIME_TYPE_H264);
        assert_eq!(CodecKind::Vp9.mime(), "video/VP9");
        assert_eq!(CodecKind::Av1.mime(), "video/AV1");
    }

    #[test]
    fn codec_kind_from_mime_round_trips_every_kind() {
        for k in [
            CodecKind::Vp8,
            CodecKind::H264,
            CodecKind::Vp9,
            CodecKind::Av1,
        ] {
            assert_eq!(CodecKind::from_mime(k.mime()), Some(k));
        }
        assert_eq!(CodecKind::from_mime(""), None);
    }

    #[test]
    fn codec_kind_only_baseline_is_always_on_default() {
        // Exactly the platform BASELINE_CODEC is always-on (VP8 on
        // macOS/Linux, H.264 on Windows where VP8 is gated off); every other
        // codec spins up on demand. This is the load-bearing cross-platform
        // assertion for the Windows H.264-baseline wiring.
        for k in [
            CodecKind::Vp8,
            CodecKind::H264,
            CodecKind::Vp9,
            CodecKind::Av1,
        ] {
            assert_eq!(
                k.is_always_on_default(),
                k == BASELINE_CODEC,
                "{k:?} always-on should equal (k == BASELINE_CODEC)"
            );
        }
        assert!(BASELINE_CODEC.is_always_on_default());
    }

    #[test]
    fn baseline_codec_is_h264_on_windows_vp8_elsewhere() {
        #[cfg(target_os = "windows")]
        assert_eq!(BASELINE_CODEC, CodecKind::H264);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(BASELINE_CODEC, CodecKind::Vp8);
    }

    #[test]
    fn simulcast_rid_constants_match_constructors() {
        assert_eq!(SimulcastRid::full().as_str(), RID_FULL);
        assert_eq!(SimulcastRid::half().as_str(), RID_HALF);
        assert_eq!(SimulcastRid::quarter().as_str(), RID_QUARTER);
    }

    #[test]
    fn vp8_simulcast_layout_is_three_descending_layers() {
        let layers = LayerSpec::vp8_simulcast(1920, 1080, 30);
        assert_eq!(layers.len(), 3);
        // Order: full, half, quarter, with exact even-rounded dims.
        assert_eq!(layers[0].rid, SimulcastRid::full());
        assert_eq!((layers[0].width, layers[0].height), (1920, 1080));
        assert_eq!(layers[1].rid, SimulcastRid::half());
        assert_eq!((layers[1].width, layers[1].height), (960, 540));
        assert_eq!(layers[2].rid, SimulcastRid::quarter());
        assert_eq!((layers[2].width, layers[2].height), (480, 270));
        // Bitrate strictly descending — smaller layers are cheap.
        assert!(layers[0].target_bitrate_kbps > layers[1].target_bitrate_kbps);
        assert!(layers[1].target_bitrate_kbps > layers[2].target_bitrate_kbps);
    }
}

// These pool-orchestration tests are gated off Windows. They were written
// around VP8 as the always-on baseline: they construct pools with
// `LayerSpec::vp8_simulcast` factories at small synthetic dimensions (64×64
// and below, down to 16×16 quarter layers) — sizes VP8/libvpx accepts but the
// Windows Media Foundation H.264 encoder MFT rejects at `SetOutputType`
// (`MF_E_INVALIDMEDIATYPE`; the MS H.264 encoder enforces a larger minimum
// frame size). On Windows the baseline codec is H.264 (`BASELINE_CODEC`), so
// every such `EncoderPool::new` would try to spawn an H.264 encoder at those
// dims and panic on the always-on construction-failure contract.
//
// The orchestration semantics these tests cover (refcounted on-demand slots,
// PoolLease drop ordering, resize epoch races, pause/resume, keyframe
// coalescing) are codec-agnostic and fully exercised on macOS/Linux where VP8
// is the baseline. The Windows-specific pool behavior — H.264 as the always-on
// baseline — is covered by [`logic_tests`] (which run everywhere) plus
// `h264_windows`'s own encoder tests (which construct the MF encoder and
// encode a real frame). Rather than rewrite 47 VP8-shaped construction sites
// to H.264-compatible dimensions (a large, risky change to proven test code),
// the heavyweight module is gated; see the task's pool-integration scope note.
#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::thread::sleep;

    // NOTE: codec-identity / baseline / simulcast-layout / RID-constant tests
    // live in [`super::logic_tests`] (compiled on every platform). This module
    // is gated off Windows and holds the pool-construction tests; see the
    // module-level comment above for why.

    /// **Phase 4a follow-up regression test (review finding).** Common
    /// non-power-of-2 display widths produce odd half/quarter dims
    /// from naked integer division. 1366×768 is the canonical case
    /// (typical laptop screens, "HD-ready" TVs, many VM consoles):
    /// - half  = 1366/2, 768/2  = 683 (odd!), 384
    /// - quarter = 1366/4, 768/4 = 341 (odd!), 192
    ///
    /// Pre-fix: VP8 encoder construction rejects odd dims (vp8.rs)
    /// AND `downscale_i420` debug-asserts even-dims-only — so once
    /// 4b switches `DisplaySession::start` to `vp8_simulcast`,
    /// `EncoderPool::new` would panic on always-on construction
    /// for any 1366-wide source.
    ///
    /// Fix: each layer's dims are rounded down to even. 1366
    /// becomes 1366 (already even), 683 → 682, 341 → 340. Costs
    /// at most 1 column/row of source pixels per odd layer; the
    /// loss is invisible at the encode-then-display stage.
    #[test]
    fn vp8_simulcast_normalizes_odd_layer_dims_for_1366x768() {
        let layers = LayerSpec::vp8_simulcast(1366, 768, 30);
        assert_eq!(layers.len(), 3, "all three layers above MIN_LAYER_DIM");
        // Full: source already even, untouched.
        assert_eq!((layers[0].width, layers[0].height), (1366, 768));
        // Half: 1366/2 = 683 → 682 (round down to even). 768/2 = 384 unchanged.
        assert_eq!((layers[1].width, layers[1].height), (682, 384));
        // Quarter: 1366/4 = 341 → 340 (round down). 768/4 = 192 unchanged.
        assert_eq!((layers[2].width, layers[2].height), (340, 192));
        // All four dims even — the property that satisfies VP8 +
        // downscale_i420 contracts.
        for l in &layers {
            assert_eq!(l.width % 2, 0, "{l:?} width must be even");
            assert_eq!(l.height % 2, 0, "{l:?} height must be even");
        }
    }

    /// Source already even on both axes — round-down should be a
    /// no-op. Pins that we don't accidentally subtract pixels from
    /// already-clean dims.
    #[test]
    fn vp8_simulcast_preserves_already_even_dims() {
        let layers = LayerSpec::vp8_simulcast(1280, 720, 30);
        assert_eq!(
            [
                (layers[0].width, layers[0].height),
                (layers[1].width, layers[1].height),
                (layers[2].width, layers[2].height),
            ],
            [(1280, 720), (640, 360), (320, 180)],
            "even source dims must pass through divisions cleanly",
        );
    }

    /// Source so small that quarter / half drop below MIN_LAYER_DIM
    /// must produce a shorter list rather than an unencodable layer.
    /// E.g. source 60×48: quarter would be 14×10, both below 16.
    /// Half is 30×24 — also below MIN_LAYER_DIM=16 on the height
    /// rounding? No: 24 ≥ 16 and 30 ≥ 16, so half survives.
    /// Quarter (14×10) drops.
    #[test]
    fn vp8_simulcast_drops_layers_below_min_dim() {
        // Source 60x48: quarter = 14x10 (both <16) → drop.
        // Half = 30x24 (both ≥16) → keep.
        // Full = 60x48 → keep.
        let layers = LayerSpec::vp8_simulcast(60, 48, 30);
        assert_eq!(layers.len(), 2, "quarter should be dropped");
        assert_eq!(layers[0].rid, SimulcastRid::full());
        assert_eq!(layers[1].rid, SimulcastRid::half());
        for l in &layers {
            assert!(
                l.width >= MIN_LAYER_DIM && l.height >= MIN_LAYER_DIM,
                "{l:?} must respect MIN_LAYER_DIM",
            );
        }

        // Source so small even half drops: 30x24. Half = 14x12 → drop.
        let layers = LayerSpec::vp8_simulcast(30, 24, 30);
        assert_eq!(layers.len(), 1, "only full survives");
        assert_eq!(layers[0].rid, SimulcastRid::full());

        // Source below MIN_LAYER_DIM on either axis: empty.
        // 14x14 (both below 16) → empty.
        assert!(LayerSpec::vp8_simulcast(14, 14, 30).is_empty());
        // Asymmetric: 32x10 → height < MIN_LAYER_DIM → empty.
        assert!(LayerSpec::vp8_simulcast(32, 10, 30).is_empty());
    }

    #[test]
    fn encoder_id_display_is_codec_colon_rid() {
        let id = EncoderId::new(CodecKind::Vp8, SimulcastRid::half());
        assert_eq!(format!("{}", id), "vp8:h");
    }

    #[test]
    fn peer_prefs_supports_lookup() {
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        assert!(prefs.supports(CodecKind::Vp8));
        assert!(prefs.supports(CodecKind::H264));
        assert!(!prefs.supports(CodecKind::Av1));
        assert!(!prefs.is_empty());

        let empty = PeerCodecPreferences::default();
        assert!(empty.is_empty());
        assert!(!empty.supports(CodecKind::Vp8));
    }

    #[test]
    fn keyframe_coalescer_dedupes_within_window() {
        let coalescer = KeyframeCoalescer::with_window(Duration::from_millis(100));
        let rid = SimulcastRid::full();

        // First request fires.
        assert!(coalescer.should_request(CodecKind::Vp8, &rid));
        // Second, immediate, dedupes.
        assert!(!coalescer.should_request(CodecKind::Vp8, &rid));
        // Different RID is independent.
        assert!(coalescer.should_request(CodecKind::Vp8, &SimulcastRid::half()));
        // Different codec is independent.
        assert!(coalescer.should_request(CodecKind::H264, &rid));
    }

    #[test]
    fn keyframe_coalescer_admits_after_window() {
        let coalescer = KeyframeCoalescer::with_window(Duration::from_millis(20));
        let rid = SimulcastRid::full();

        assert!(coalescer.should_request(CodecKind::Vp8, &rid));
        sleep(Duration::from_millis(40));
        // Window has elapsed — next request fires.
        assert!(coalescer.should_request(CodecKind::Vp8, &rid));
    }

    // -----------------------------------------------------------------------
    // Phase 3c.3a: on_resize
    // -----------------------------------------------------------------------

    /// Calling `on_resize` with the same dimensions the pool was built
    /// at is a no-op: same handles, same subscribe behavior. The
    /// capture backend emits same-dimension re-announcements on xrandr
    /// notifications that only changed refresh rate; we don't want
    /// encoder churn there.
    #[tokio::test]
    async fn on_resize_with_same_dimensions_is_noop() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Snapshot the single always-on handle's identity *before*
        // resize. If resize respawns anything on a same-dim call, the
        // Arc<AtomicBool> for force_keyframe would be a fresh
        // allocation and pointer inequality would fire.
        let before_id = {
            let always_on = pool.always_on();
            always_on[0].id.clone()
        };
        let before_force_kf_ptr = {
            let always_on = pool.always_on();
            Arc::as_ptr(&always_on[0].force_keyframe) as usize
        };

        pool.on_resize(64, 64);

        assert_eq!(pool.dimensions(), (64, 64));
        let after_id = {
            let always_on = pool.always_on();
            always_on[0].id.clone()
        };
        let after_force_kf_ptr = {
            let always_on = pool.always_on();
            Arc::as_ptr(&always_on[0].force_keyframe) as usize
        };
        assert_eq!(before_id, after_id);
        assert_eq!(
            before_force_kf_ptr, after_force_kf_ptr,
            "same-dim on_resize must leave handle identity untouched"
        );
    }

    /// `on_resize` to different dimensions advances the pool's
    /// atomic dimensions, keeps the always-on handle count the same,
    /// and replaces the handle with a freshly-spawned one (so
    /// existing subscribers see Closed on next recv and the new
    /// handle carries the new layer dimensions). This is the
    /// contract the bridge depends on.
    #[tokio::test]
    async fn on_resize_to_new_dimensions_respawns_always_on() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Grab a subscription against the old handle. After resize
        // it should return Closed on its next recv (the old handle's
        // broadcast::Sender drops when the vec is overwritten).
        let mut old_frames_rx = {
            let always_on = pool.always_on();
            always_on[0].frames.subscribe()
        };
        let old_layer = {
            let always_on = pool.always_on();
            always_on[0].layer.clone()
        };

        pool.on_resize(128, 96);

        assert_eq!(pool.dimensions(), (128, 96));
        let (new_handle_count, new_layer) = {
            let always_on = pool.always_on();
            (always_on.len(), always_on[0].layer.clone())
        };
        assert_eq!(new_handle_count, 1, "resize must preserve handle count");
        // Layer rescales proportionally. For the single-layer case
        // old_layer was (64, 64), source was (64, 64), new source is
        // (128, 96) → new layer is (128, 96).
        assert_eq!(new_layer.width, 128);
        assert_eq!(new_layer.height, 96);
        assert_eq!(new_layer.rid, old_layer.rid, "rid preserved across resize");

        // Old subscription must terminate. The mechanics:
        // - on_resize cancels the old handle's shutdown token, but the
        //   encoder thread only checks shutdown at the top of its loop
        //   — so we need a frame push to wake it from `blocking_recv`.
        // - The wake-push causes BOTH old and new encoder threads to
        //   wake and process the frame. Both may emit a frame to their
        //   respective `frames_tx` broadcasts before the next loop iter
        //   checks shutdown. The old thread then sees shutdown
        //   cancelled at the top of the loop, exits, drops its
        //   `frames_tx_for_thread` clone. Combined with the earlier
        //   drop of `handle.frames` in on_resize, both senders are
        //   gone, the broadcast channel closes, and subscribers
        //   receive `Closed` on their next recv.
        // - So the subscriber may see one final frame (a VP8 encode of
        //   the wake-push I420) followed by Closed, OR Closed
        //   directly if timing happens to catch the thread mid-exit.
        //   Both orderings are valid; the contract is "terminates
        //   eventually."
        let wake_frame = Arc::new(vec![0u8; 128 * 96 * 3 / 2]);
        pool.push_i420_frame(wake_frame, Instant::now());

        // Drain until we see Closed or hit the timeout. Up to 3
        // iterations is plenty — one for the wake-push's trailing
        // encode, one or two for Lagged slots from the broadcast's
        // internal recycling.
        let mut closed = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        for _ in 0..3 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(result) = tokio::time::timeout(remaining, old_frames_rx.recv()).await else {
                break;
            };
            match result {
                Err(broadcast::error::RecvError::Closed) => {
                    closed = true;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Ok(_) => continue, // final trailing frame; loop again
            }
        }
        assert!(
            closed,
            "old subscription must see Closed within 2s after on_resize + wake-push; \
             either the old handle's frames-Sender clone didn't drop, or the old \
             encoder thread didn't exit after shutdown-cancellation."
        );
    }

    /// `on_resize` preserves the pool's invariant that pushing an
    /// I420 frame at the new dimensions reaches the always-on
    /// encoder. This is the direct precondition the bridge's
    /// post-resize push relies on.
    #[tokio::test]
    async fn on_resize_leaves_pool_ready_for_push_at_new_dimensions() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        pool.on_resize(128, 96);

        // New I420 buffer sized for 128x96. If on_resize left a stale
        // subscriber count (e.g. old handle's subscription is gone
        // but new handle didn't subscribe), push would return 0 and
        // the always-on encoder would see no frames.
        let i420 = Arc::new(vec![0u8; 128 * 96 * 3 / 2]);
        let subscriber_count = pool.push_i420_frame(i420, Instant::now());
        assert!(
            subscriber_count >= 1,
            "post-resize always-on must be i420-subscribed; got {subscriber_count}"
        );
    }

    /// `on_resize` with proportional layer rescale. A simulcast-
    /// layered pool (half-resolution always-on layer) resized from
    /// 1000x500 → 2000x1000 should see that layer scaled to 1000x500
    /// (still half of source). Locks the rescale formula for the
    /// phase 4 simulcast case even though phase 3c.3a doesn't
    /// exercise it directly.
    #[tokio::test]
    async fn on_resize_rescales_layers_proportionally() {
        // Half-of-source layer factory. The factory rule (RID +
        // bitrate constant, dims = source/2) is what `vp8_simulcast`
        // does for its half layer; defining it inline here pins the
        // contract that on_resize re-derives layer dims from the
        // factory at the new source dims, not from the previous
        // epoch's already-rounded dims.
        let pool = EncoderPool::new(
            1000,
            500,
            30,
            |w, h| {
                vec![LayerSpec {
                    rid: SimulcastRid::half(),
                    width: (w / 2) & !1,
                    height: (h / 2) & !1,
                    target_bitrate_kbps: 400,
                    framerate: 30,
                }]
            },
            None,
        );

        pool.on_resize(2000, 1000);

        let scaled = {
            let always_on = pool.always_on();
            always_on[0].layer.clone()
        };
        assert_eq!(scaled.width, 1000); // was 500 (half of 1000), now half of 2000
        assert_eq!(scaled.height, 500); // was 250 (half of 500), now half of 1000
        assert_eq!(scaled.rid, SimulcastRid::half());
        assert_eq!(
            scaled.target_bitrate_kbps, 400,
            "bitrate preserved across rescale; TWCC adjusts at runtime"
        );
    }

    /// **Phase 4a follow-up regression test (review finding).** The
    /// resize path must enforce the same `MIN_LAYER_DIM` floor as
    /// initial construction (`vp8_simulcast`) — otherwise an
    /// undersized layer survives a resize-down and tries to keep
    /// encoding at unsupported dims.
    ///
    /// 64×64 → 60×48 is the canonical case:
    /// - Pool starts with `vp8_simulcast(64, 64)` → full 64×64,
    ///   half 32×32, quarter 16×16. All survive.
    /// - Resize to 60×48: full → 60×48 (ok), half → 30×24 (ok),
    ///   quarter → 14×12 (width 14 < MIN_LAYER_DIM=16) → drop.
    ///
    /// Pre-fix: the resize path rescaled old layers in place
    /// (`rescale_layer_spec`) and only rounded to even, so
    /// `on_resize` blindly respawned a 14×12 quarter encoder.
    /// VP8 either rejected it (panic via `unwrap_or_else`) or
    /// silently mis-encoded — both broken. Post-fix: `on_resize`
    /// re-invokes the pool's `LayerFactory` with the new dims,
    /// so the same `vp8_simulcast` filter that drops sub-MIN
    /// layers at construction also drops them at resize.
    ///
    /// Companion regression tests:
    /// `on_resize_grow_back_restores_dropped_simulcast_layers`
    /// (shrink-then-grow restores) and
    /// `on_resize_avoids_rounding_drift_via_factory_regen`
    /// (drift-free resize through odd intermediate dims).
    #[tokio::test]
    async fn on_resize_drops_simulcast_layers_below_min_dim() {
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        // Precondition: all 3 simulcast layers spawned cleanly.
        {
            let always_on = pool.always_on();
            assert_eq!(
                always_on.len(),
                3,
                "vp8_simulcast at 64x64 must yield all 3 layers"
            );
        }

        // Resize to dims where quarter would round to 14×12.
        pool.on_resize(60, 48);

        let always_on = pool.always_on();
        let rids: Vec<SimulcastRid> = always_on.iter().map(|h| h.layer.rid.clone()).collect();
        assert_eq!(
            always_on.len(),
            2,
            "quarter layer must be dropped on resize below MIN_LAYER_DIM \
             (got rids={rids:?})"
        );
        assert!(
            rids.contains(&SimulcastRid::full()),
            "full layer must survive — got rids={rids:?}"
        );
        assert!(
            rids.contains(&SimulcastRid::half()),
            "half layer must survive at 30×24 — got rids={rids:?}"
        );
        assert!(
            !rids.contains(&SimulcastRid::quarter()),
            "quarter layer must be dropped (would have been 14×12) — \
             got rids={rids:?}"
        );

        // Survivors must have valid even dims at or above MIN_LAYER_DIM
        // — the same contract `vp8_simulcast` enforces on a fresh pool.
        for h in always_on.iter() {
            assert_eq!(h.layer.width % 2, 0, "{:?} width must be even", h.id);
            assert_eq!(h.layer.height % 2, 0, "{:?} height must be even", h.id);
            assert!(
                h.layer.width >= MIN_LAYER_DIM,
                "{:?} width {} below MIN_LAYER_DIM",
                h.id,
                h.layer.width,
            );
            assert!(
                h.layer.height >= MIN_LAYER_DIM,
                "{:?} height {} below MIN_LAYER_DIM",
                h.id,
                h.layer.height,
            );
        }

        // The surviving set must equal what a fresh-pool
        // `vp8_simulcast(60, 48, 30)` would return — single source
        // of truth for "valid layer dims at this source size,"
        // checked by both the resize and initial-construction paths
        // through `normalize_layer_dims`.
        let fresh = LayerSpec::vp8_simulcast(60, 48, 30);
        let fresh_rids: Vec<SimulcastRid> = fresh.iter().map(|l| l.rid.clone()).collect();
        let mut sorted_rids = rids.clone();
        sorted_rids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut sorted_fresh = fresh_rids.clone();
        sorted_fresh.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            sorted_rids, sorted_fresh,
            "after-resize RIDs must match fresh-pool RIDs at the same \
             source dims (resize: {rids:?}, fresh: {fresh_rids:?})"
        );
    }

    /// **Phase 4a follow-up regression test #2 (review finding).**
    /// Layers dropped on a resize-down must come back on the next
    /// resize-up if the new dims support them.
    ///
    /// Pre-fix: `on_resize` derived new layers by iterating the
    /// surviving old `always_on` Vec. Once quarter was dropped at
    /// 60×48, it was no longer in the iteration source, so a
    /// subsequent 60×48 → 64×64 resize produced only full+half —
    /// quarter never came back even though `vp8_simulcast(64, 64)`
    /// would include it for a fresh pool. Post-fix: `on_resize`
    /// re-invokes the pool's `LayerFactory` with the new dims,
    /// which always returns the canonical layout for those dims.
    ///
    /// 64×64 → 60×48 → 64×64 round-trip:
    ///   start: full(64×64), half(32×32), quarter(16×16)
    ///   after  60×48: full(60×48), half(30×24)              [quarter dropped]
    ///   after  64×64: full(64×64), half(32×32), quarter(16×16)  [restored]
    #[tokio::test]
    async fn on_resize_grow_back_restores_dropped_simulcast_layers() {
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        // Start: 3 layers.
        assert_eq!(pool.always_on().len(), 3, "initial pool has 3 layers");

        // Shrink: quarter dropped.
        pool.on_resize(60, 48);
        assert_eq!(
            pool.always_on().len(),
            2,
            "after 64×64 → 60×48: quarter (would be 14×12) drops"
        );

        // Grow back: quarter must return.
        pool.on_resize(64, 64);
        let always_on = pool.always_on();
        assert_eq!(
            always_on.len(),
            3,
            "after 60×48 → 64×64 round-trip: quarter must be restored \
             (got {} layers, rids={:?})",
            always_on.len(),
            always_on
                .iter()
                .map(|h| h.layer.rid.as_str())
                .collect::<Vec<_>>(),
        );
        let rids: Vec<&str> = always_on.iter().map(|h| h.layer.rid.as_str()).collect();
        assert!(rids.contains(&RID_FULL), "full present");
        assert!(rids.contains(&RID_HALF), "half present");
        assert!(
            rids.contains(&RID_QUARTER),
            "quarter restored after the round-trip — got rids={rids:?}"
        );
    }

    /// **Phase 4a follow-up regression test #3 (review finding).**
    /// Resize through an odd-width intermediate must not
    /// accumulate rounding drift. The canonical
    /// `vp8_simulcast(target_w, target_h)` layout is the truth at
    /// every step — never derived from the previous epoch's
    /// already-rounded dims.
    ///
    /// Pre-fix: `rescale_layer_spec` computed new dims as
    /// `old_layer_w * new_src_w / old_src_w`. So on a 1366×768
    /// pool, half started at 682×384 (rounded down from 683).
    /// Resizing to 1920×1080 then computed
    /// `682 * 1920 / 1366 = 958` (even, but NOT 960). After enough
    /// resizes, the layer dims drift away from any clean fraction
    /// of the source.
    ///
    /// Post-fix: factory regenerates from canonical layout.
    /// 1366×768 → 1920×1080 produces half = 960×540, matching
    /// `vp8_simulcast(1920, 1080).half`.
    #[tokio::test]
    async fn on_resize_avoids_rounding_drift_via_factory_regen() {
        let pool = EncoderPool::new(
            1366,
            768,
            30,
            |w, h| LayerSpec::vp8_simulcast(w, h, 30),
            None,
        );

        // Sanity: at 1366×768 the half layer is 682×384 (683 rounded
        // down to even). This is the dim that the pre-fix rescaling
        // would propagate forward.
        let half = pool
            .always_on()
            .iter()
            .find(|h| h.layer.rid == SimulcastRid::half())
            .map(|h| h.layer.clone())
            .expect("half layer at 1366×768");
        assert_eq!(
            (half.width, half.height),
            (682, 384),
            "1366/2 rounds down to 682",
        );

        pool.on_resize(1920, 1080);

        // Post-resize half must match what `vp8_simulcast(1920, 1080)`
        // would return for a fresh pool — 960×540, NOT 958×540
        // (which is what the pre-fix `682 * 1920 / 1366` would yield).
        let half_after = pool
            .always_on()
            .iter()
            .find(|h| h.layer.rid == SimulcastRid::half())
            .map(|h| h.layer.clone())
            .expect("half layer after resize to 1920×1080");
        assert_eq!(
            (half_after.width, half_after.height),
            (960, 540),
            "post-resize half must equal vp8_simulcast(1920, 1080).half — \
             pre-fix `rescale_layer_spec` would yield 958×540 from drift",
        );

        // Cross-check: post-resize layer set is identical (RID + dims)
        // to a fresh `vp8_simulcast(1920, 1080)`. Any drift in any
        // layer would surface here.
        let actual: Vec<(String, u32, u32)> = pool
            .always_on()
            .iter()
            .map(|h| {
                (
                    h.layer.rid.as_str().to_string(),
                    h.layer.width,
                    h.layer.height,
                )
            })
            .collect();
        let mut actual_sorted = actual.clone();
        actual_sorted.sort();
        let expected: Vec<(String, u32, u32)> = LayerSpec::vp8_simulcast(1920, 1080, 30)
            .iter()
            .map(|l| (l.rid.as_str().to_string(), l.width, l.height))
            .collect();
        let mut expected_sorted = expected.clone();
        expected_sorted.sort();
        assert_eq!(
            actual_sorted, expected_sorted,
            "post-resize layer set must match fresh vp8_simulcast — \
             actual: {actual:?}, expected: {expected:?}",
        );
    }

    /// Finding 1 fix: after on_resize cancels an encoder's shutdown
    /// token, the next wake-push must not produce an encoded frame.
    /// The thread sees shutdown at the top of the loop OR re-checks
    /// after `blocking_recv` returns; either way it exits before
    /// calling `encoder.encode` on post-resize data its encoder
    /// (configured for old dimensions) would misinterpret.
    ///
    /// Exercises the fix directly: cancel shutdown on an encoder
    /// handle, push a wake-frame, verify the subscriber sees Closed
    /// rather than Ok(frame). The pre-fix bug would emit one frame
    /// before exiting.
    #[tokio::test]
    async fn encoder_thread_exits_on_shutdown_without_emitting_frame() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Subscribe to the always-on encoder's frames.
        let mut frames_rx = {
            let always_on = pool.always_on();
            always_on[0].frames.subscribe()
        };

        // Cancel the thread's shutdown token. From the pool API this
        // isn't how shutdown normally fires (it's driven by on_resize
        // or lease drop), but the contract we're testing is "a
        // cancelled-shutdown encoder must not produce another encoded
        // frame," which is the same regardless of what fired the
        // cancellation.
        {
            let always_on = pool.always_on();
            always_on[0].shutdown.cancel();
        }

        // Push a wake-frame so the thread wakes from blocking_recv.
        let frame = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        pool.push_i420_frame(frame, Instant::now());

        // Wait long enough for the thread to wake, observe the
        // cancelled shutdown, and exit without encoding. 200ms is
        // orders of magnitude above one frame interval at 30fps
        // (~33ms) — if the encoder were going to emit a stale
        // frame, it would have by now.
        //
        // NB: we don't wait for `RecvError::Closed` because the
        // handle's `frames` Sender is still alive in
        // `pool.always_on` (only the thread's Sender clone drops on
        // exit). The test's contract is "no frame was emitted," not
        // "channel closed" — in on_resize the channel does close
        // because the handle itself is dropped, but that's a
        // separate path already covered by
        // `on_resize_to_new_dimensions_respawns_always_on`.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        match frames_rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {
                // Expected: thread exited before encoding.
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                // Also acceptable if somehow the handle dropped; the
                // contract (no stale frame) still holds.
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                panic!(
                    "unexpected Lagged — encoder should not have produced \
                     enough frames to lag a fresh subscriber"
                )
            }
            Ok(_) => panic!(
                "encoder emitted a frame after shutdown — the post-blocking_recv \
                 shutdown check is missing or reordered, allowing one stale \
                 encode pass through"
            ),
        }
    }

    /// Finding 2 fix (a): on_resize drops on-demand slots entirely.
    /// Earlier drafts respawned them in place (preserving refcount),
    /// which created a lifetime mismatch where existing leases held
    /// stale generations against the new slot. Post-fix, on_resize
    /// simply tears slots down; forwarders re-subscribe on Closed
    /// and get fresh slots.
    #[tokio::test]
    async fn on_resize_drops_on_demand_slots_rather_than_respawning() {
        // Construct with empty always_on so VP8 falls to on-demand,
        // giving us a VP8 slot to observe.
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (_subs, _lease) = pool.subscribe(&prefs).expect("on-demand VP8 spawn");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1),
            "on-demand slot present pre-resize"
        );

        pool.on_resize(128, 96);

        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None,
            "on_resize must tear down on-demand slots; the forwarder's \
             Closed-driven re-subscribe is what spawns a fresh slot \
             (phase 3c.3b)"
        );
    }

    /// Finding 2 fix (b): a stale lease from a pre-resize subscribe
    /// MUST NOT decrement the refcount of a post-resize re-subscribed
    /// slot that happens to share the same EncoderId. The generation
    /// token on each slot and in each lease's on_demand_refs is what
    /// makes this safe.
    ///
    /// Scenario:
    ///   T0: subscribe → slot gen=0, lease A refs gen=0, refcount=1
    ///   T1: on_resize → slot gen=0 dropped
    ///   T2: subscribe → slot gen=1, lease B refs gen=1, refcount=1
    ///   T3: drop lease A — must NOT decrement slot gen=1's refcount
    ///   T4: drop lease B — tears slot down
    #[tokio::test]
    async fn stale_lease_does_not_decrement_replacement_slot() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let (_subs_a, lease_a) = pool.subscribe(&prefs).expect("first subscribe");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1),
            "slot created at refcount=1"
        );

        pool.on_resize(128, 96);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None,
            "slot dropped by resize"
        );

        let (_subs_b, lease_b) = pool.subscribe(&prefs).expect("re-subscribe");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1),
            "re-subscribe spawns fresh slot at refcount=1 with new generation"
        );

        // Drop the stale lease. If the generation gate is missing or
        // broken, this would decrement slot B's refcount to 0 and
        // tear it down — the exact bug. With the gate, release_impl
        // sees slot B's gen != lease A's recorded gen and skips.
        drop(lease_a);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1),
            "stale lease from the pre-resize slot must NOT decrement \
             the post-resize slot's refcount; gen mismatch should skip"
        );

        // Legitimate drop: lease B was against slot B, genes match,
        // refcount → 0, slot torn down.
        drop(lease_b);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None,
            "legitimate lease drop tears slot down when refcount hits 0"
        );
    }

    /// Finding 1 (3c.3b.2-pre): the source-state snapshot
    /// (width, height, gen) returned by `dimensions()` and the
    /// internal `snapshot_source` is atomic — a caller that reads
    /// the dims and the epoch at the same moment cannot get a
    /// torn pair where one came from before a concurrent
    /// `on_resize` and the other from after. Locks the
    /// `RwLock<SourceState>` substitution that replaced the
    /// earlier three-atomic design.
    #[tokio::test]
    async fn source_state_snapshot_is_atomic() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Initial snapshot: matches construction.
        let s0 = pool.snapshot_source();
        assert_eq!((s0.width, s0.height), (64, 64));
        assert_eq!(s0.gen, 0);
        assert_eq!(pool.dimensions(), (s0.width, s0.height));

        pool.on_resize(800, 600);

        // Post-resize snapshot: dims advanced, gen advanced —
        // both observed in the same read-lock-protected critical
        // section so no torn read is possible. The earlier
        // three-atomic design could observe (new, new, old) or
        // (old, new, new) under contention; this test pins the
        // single-atomic-snapshot contract.
        let s1 = pool.snapshot_source();
        assert_eq!((s1.width, s1.height), (800, 600));
        assert!(s1.gen > s0.gen);
        // dimensions() reads through the same snapshot helper, so
        // it must agree.
        assert_eq!(pool.dimensions(), (s1.width, s1.height));
    }

    /// Finding 2 (3c.3b.2-pre): on stale-epoch detection, subscribe
    /// retries rather than returning a partial result. Without a
    /// deterministic test hook for the race, this test captures the
    /// guarantee that even when always-on/fast-path codecs are
    /// available, a stale-detected attempt does not silently drop
    /// the on-demand codec and return only the always-on subset.
    /// Indirect verification: the subscribe success path post-resize
    /// includes the on-demand codec at the new dimensions (already
    /// covered by `subscribe_after_resize_uses_new_dimensions`); the
    /// `MAX_SUBSCRIBE_ATTEMPTS` constant ensures retries are bounded
    /// (this test asserts the cap is sane).
    #[tokio::test]
    async fn subscribe_retry_cap_is_bounded() {
        // The retry loop is bounded so a pathological "every attempt
        // races" doesn't spin forever. Two attempts is the documented
        // ceiling — a third attempt would mean three consecutive
        // sub-millisecond resizes during a single subscribe, which
        // is itself a bug worth surfacing.
        assert!(
            MAX_SUBSCRIBE_ATTEMPTS >= 1,
            "must allow at least one attempt"
        );
        assert!(
            MAX_SUBSCRIBE_ATTEMPTS <= 4,
            "more than a few retries indicates either a livelock \
             tolerance the production system shouldn't hide, or \
             unrealistic resize traffic — keep the cap tight"
        );
    }

    /// Finding 1 (3c.3a3): subscribe's pass 2 runs off-lock; an
    /// on_resize that fires during construction must be detected in
    /// pass 3 so a stale-dimensions encoder isn't installed as the
    /// slot for its EncoderId. The mechanism is the `source_gen`
    /// epoch: on_resize bumps it, subscribe captures it at start,
    /// pass 3 compares. This test pins the on_resize half of the
    /// contract — the epoch actually advances on a real-dim change
    /// and stays flat on a same-dim no-op.
    #[tokio::test]
    async fn on_resize_bumps_source_gen_epoch() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );
        let before = pool.source_gen();

        // Same-dim: no-op, epoch unchanged.
        pool.on_resize(64, 64);
        assert_eq!(
            pool.source_gen(),
            before,
            "same-dim on_resize must not bump source_gen; subscribe \
             races against an epoch that didn't actually move would \
             cancel valid encoders unnecessarily"
        );

        // Real change: epoch must advance at least by one. Monotonic
        // is sufficient; no need to assert exact value since test
        // ordering vs other methods that might bump it is fragile.
        pool.on_resize(128, 96);
        assert!(
            pool.source_gen() > before,
            "on_resize must bump source_gen when dims actually change"
        );

        // Another real change: advances again.
        let mid = pool.source_gen();
        pool.on_resize(320, 240);
        assert!(
            pool.source_gen() > mid,
            "on_resize must bump source_gen on every real-dim change"
        );
    }

    /// Finding 2 (3c.3a3): a subscribe that completes entirely after
    /// an on_resize uses the new dimensions (captured fresh when
    /// subscribe runs), so its constructed encoder is at the new
    /// dimensions and passes pass 3's epoch check. The scenario that
    /// would trip the race — subscribe's pass 2 overlapping with
    /// on_resize — is hard to produce deterministically in a unit
    /// test (pass 2 is a few-microsecond critical section for VP8).
    /// This test pins the no-race happy path as a regression guard:
    /// subscribe after resize must produce a working slot at the
    /// post-resize dimensions.
    #[tokio::test]
    async fn subscribe_after_resize_uses_new_dimensions() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        pool.on_resize(128, 96);

        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (subs, _lease) = pool
            .subscribe(&prefs)
            .expect("subscribe must succeed post-resize");

        assert_eq!(subs.len(), 1);
        // The on-demand VP8 encoder was constructed after on_resize,
        // so LayerSpec::single picked up the new atomics. Verify.
        assert_eq!(subs[0].layer.width, 128);
        assert_eq!(subs[0].layer.height, 96);
    }

    #[tokio::test]
    async fn pool_dimensions_reflects_construction() {
        // The bridge uses `pool.dimensions()` to gate push_i420_frame
        // when the capture resolution changes out from under the pool's
        // startup-dimension-locked encoders. This test pins that the
        // accessor returns the same (width, height) pair that was
        // passed to `EncoderPool::new` — any drift between construction
        // and readback would silently bypass the bridge's safety gate
        // and feed mis-sized I420 into the encoder.
        let pool = EncoderPool::new(
            1280,
            720,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );
        assert_eq!(pool.dimensions(), (1280, 720));
    }

    #[tokio::test]
    async fn pool_subscribes_to_always_on_codec() {
        // VP8 always-on. Peer supporting VP8 gets one subscription from
        // always_on (no on-demand spawn needed).
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        let vp8_prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (subs, _lease) = pool.subscribe(&vp8_prefs).expect("subscribe must succeed");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id.codec, CodecKind::Vp8);
    }

    #[tokio::test]
    async fn pool_rejects_codecs_with_no_backend() {
        // Peer advertises VP9/AV1 only — neither has a backend yet
        // (phase 4+). Always-on is VP8, no match; on-demand won't
        // spawn because on_demand_spawnable rejects them. Subscription
        // set is empty → subscribe returns NoCompatibleCodec.
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        let unsupported_prefs = PeerCodecPreferences::new(vec![CodecKind::Vp9, CodecKind::Av1]);
        let result = pool.subscribe(&unsupported_prefs);
        assert!(
            matches!(result, Err(SubscribeError::NoCompatibleCodec)),
            "expected NoCompatibleCodec when no peer codec overlaps the pool"
        );
    }

    #[tokio::test]
    async fn pool_request_keyframe_coalesces() {
        // Empty layer set — no encoders spawned, so request_keyframe
        // consistently returns false (coalescer admits the first, but
        // no matching encoder exists).
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let rid = SimulcastRid::full();
        // Coalescer admits first, but no encoder matches → false.
        assert!(!pool.request_keyframe(CodecKind::Vp8, Some(rid.clone())));
        // Second is coalesced at the coalescer layer regardless.
        assert!(!pool.request_keyframe(CodecKind::Vp8, Some(rid.clone())));
    }

    /// Keyframe request actually sets the encoder's atomic flag when
    /// an encoder matches. Exercises the full request_keyframe →
    /// coalescer → handle.force_keyframe path.
    #[tokio::test]
    async fn pool_request_keyframe_sets_encoder_flag() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Initial state: flag is false.
        let handle = &pool.always_on()[0];
        assert!(!handle.force_keyframe.load(Ordering::SeqCst));

        // Fire keyframe request → flag goes true.
        let fired = pool.request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()));
        assert!(
            fired,
            "request_keyframe must return true when encoder matches"
        );
        assert!(handle.force_keyframe.load(Ordering::SeqCst));

        // Second request is coalesced (returns false) — flag stays
        // set because we haven't encoded yet (the encoder thread would
        // swap it back).
        let fired2 = pool.request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()));
        assert!(!fired2);
        assert!(handle.force_keyframe.load(Ordering::SeqCst));
    }

    /// `request_keyframe_all` sets the force-keyframe flag on every
    /// active encoder (always-on + on-demand with refcount > 0), and
    /// coalesces against the per-`(codec, rid)` window so a second
    /// immediate call admits zero requests. Pins the peer-join
    /// keyframe-on-join contract used by
    /// `DisplaySession::handle_offer_pool_mode`. Without this, late-
    /// joining pool peers wait up to one GOP boundary on idle
    /// desktops (the b241cf5 black-screen-on-idle class).
    #[tokio::test]
    async fn pool_request_keyframe_all_fires_all_active_encoders() {
        // Three always-on layers (full / half / quarter) — exercises
        // multi-encoder iteration, not just a single layer.
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        // Initial state: flags clear on every always-on encoder.
        {
            let always_on = pool.always_on();
            assert_eq!(always_on.len(), 3, "expected three always-on layers");
            for handle in always_on.iter() {
                assert!(
                    !handle.force_keyframe.load(Ordering::SeqCst),
                    "encoder {:?} flag must start clear",
                    handle.id,
                );
            }
        }

        let fired = pool.request_keyframe_all();
        assert_eq!(
            fired, 3,
            "request_keyframe_all must admit one request per active \
             encoder; got {fired}, expected 3 (three always-on layers)",
        );

        // All flags set after the call.
        {
            let always_on = pool.always_on();
            for handle in always_on.iter() {
                assert!(
                    handle.force_keyframe.load(Ordering::SeqCst),
                    "encoder {:?} flag must be set after \
                     request_keyframe_all",
                    handle.id,
                );
            }
        }

        // Second immediate call: coalesced. Encoders haven't run yet
        // (no I420 push) so flags stay set.
        let fired2 = pool.request_keyframe_all();
        assert_eq!(
            fired2, 0,
            "second immediate request_keyframe_all must coalesce \
             per-(codec,rid) window; got {fired2}",
        );
        {
            let always_on = pool.always_on();
            for handle in always_on.iter() {
                assert!(handle.force_keyframe.load(Ordering::SeqCst));
            }
        }
    }

    /// **Phase 4d.2 follow-up: `always_on_ids` enumeration.** Pins
    /// the contract the aggregator wiring at
    /// `DisplaySession::start` relies on: a fresh pool's
    /// `always_on_ids()` returns one `EncoderId` per always-on
    /// layer the factory produced, with `(codec, rid)` matching
    /// what the spec advertised. Aggregator queries this on every
    /// pause/resume action so post-resize layer-set changes are
    /// reflected without a separate refresh path.
    #[tokio::test]
    async fn pool_always_on_ids_returns_one_id_per_layer() {
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        let ids = pool.always_on_ids();
        assert_eq!(
            ids.len(),
            3,
            "vp8_simulcast at 64x64@30 should produce three layers \
             (full / half / quarter); always_on_ids returned {ids:?}",
        );
        for id in &ids {
            assert_eq!(
                id.codec,
                CodecKind::Vp8,
                "every always-on id must be VP8 (the simulcast \
                 codec); got {id:?}",
            );
        }
        // Must match what `always_on()` (the internal accessor) sees.
        let internal_ids: Vec<EncoderId> = pool.always_on().iter().map(|h| h.id.clone()).collect();
        assert_eq!(
            ids, internal_ids,
            "always_on_ids must mirror the internal handle set \
             exactly (same order, same EncoderIds)",
        );
    }

    /// **3c.3b.4f WatchdogState contract.** Pinning the four
    /// behaviors the encoder thread relies on:
    ///   1. produced > 0 resets the silent-frame counter
    ///   2. exactly `ENCODER_SILENT_FRAMES_THRESHOLD` consecutive
    ///      silent frames trigger ONE swap-attempt request
    ///   3. after the swap-attempt is recorded, the watchdog never
    ///      fires again — even on continued silence (the fallback
    ///      either succeeded or there's nothing better to swap to)
    ///   4. interleaved silent/produced frames never accumulate
    ///      across non-silent frames
    #[test]
    fn watchdog_state_contract() {
        let mut w = WatchdogState::new();

        // Behavior 1: produced > 0 keeps the counter at zero.
        for _ in 0..50 {
            assert!(!w.record(1));
        }

        // Behavior 2: silent frames accumulate; below threshold returns
        // false; AT threshold returns true exactly once.
        for i in 1..ENCODER_SILENT_FRAMES_THRESHOLD {
            assert!(
                !w.record(0),
                "silent frame {i} below threshold must not fire watchdog",
            );
        }
        assert!(
            w.record(0),
            "{}th silent frame must fire watchdog (swap request)",
            ENCODER_SILENT_FRAMES_THRESHOLD,
        );

        // Behavior 3: post-swap latch — never fires again, even on
        // continued silence well past the threshold.
        for _ in 0..(ENCODER_SILENT_FRAMES_THRESHOLD * 3) {
            assert!(!w.record(0), "post-swap watchdog must never fire again",);
        }
        // Even produced > 0 followed by silence stays latched.
        assert!(!w.record(2));
        for _ in 0..(ENCODER_SILENT_FRAMES_THRESHOLD * 2) {
            assert!(!w.record(0));
        }
    }

    /// **3c.3b.4f WatchdogState reset behavior.** A produced frame
    /// in the middle of an accumulating silent run resets the counter
    /// to zero. Pre-fix bug class: any "stuck once accumulated"
    /// implementation would fire prematurely on a partially-silent
    /// pattern that's actually healthy (e.g. occasional skipped
    /// frames in a normal encoder).
    #[test]
    fn watchdog_state_resets_counter_on_produced() {
        let mut w = WatchdogState::new();

        // Accumulate just below threshold.
        for _ in 0..(ENCODER_SILENT_FRAMES_THRESHOLD - 1) {
            assert!(!w.record(0));
        }
        // Single produced frame resets.
        assert!(!w.record(1));
        // After reset we can again accumulate up to (but not past)
        // threshold without firing.
        for _ in 0..(ENCODER_SILENT_FRAMES_THRESHOLD - 1) {
            assert!(!w.record(0));
        }
        // The threshold-th silent frame after reset is what fires.
        assert!(w.record(0));
    }

    /// End-to-end: push synthetic I420 frames through the pool and
    /// verify encoded frames come out via `subscribe`. This is the
    /// regression guard that phase 3a's encoder spawn actually works —
    /// not just that the types line up.
    #[tokio::test]
    async fn pool_produces_encoded_frames_from_synthetic_i420() {
        // Small frame: 64x64 black (Y=0, U=128, V=128). I420 size =
        // W*H + 2*(W/2)*(H/2) = W*H*3/2.
        const W: usize = 64;
        const H: usize = 64;
        let i420_size = W * H * 3 / 2;
        let mut frame_data = vec![0u8; i420_size];
        // U and V planes are chroma — 128 is neutral (achromatic).
        for byte in &mut frame_data[W * H..] {
            *byte = 128;
        }
        let frame_arc = Arc::new(frame_data);

        let pool = EncoderPool::new(
            W as u32,
            H as u32,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (mut subs, _lease) = pool.subscribe(&prefs).expect("subscribe must succeed");
        assert_eq!(subs.len(), 1);
        let mut rx = subs.remove(0).frames;

        // Give the encoder thread a moment to finish construction
        // (blocking_recv subscribes are cheap but the thread needs to
        // reach its first recv before push_i420_frame is observed).
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Push a handful of frames. VP8 should emit a keyframe packet
        // on (or shortly after) the first frame, then P-frames on
        // subsequent ones.
        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&frame_arc), Instant::now());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Expect at least one encoded frame within 2 seconds.
        let ef = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("encoded frame should arrive within 2s")
            .expect("broadcast should not be closed while pool is alive");
        assert!(
            !ef.data.is_empty(),
            "encoded frame payload must be non-empty"
        );
    }

    /// **Phase 4a regression test.** A pool with a half-resolution
    /// always-on layer (e.g. `vp8_simulcast`'s middle layer at
    /// 32x32 against a 64x64 source) must downscale incoming
    /// source-dim I420 to the layer's dim before encoding —
    /// otherwise the encoder configured for 32x32 would either
    /// reject a 64x64 buffer or mis-encode it. Pre-fix: subscribing
    /// to the half/quarter layer with a source-dim push silently
    /// produced no decodable frames (encoder either errored on
    /// every encode or emitted garbage). This test pins the
    /// downscale path end-to-end: encoded frames flow from a
    /// half-dim subscriber when fed source-dim I420.
    #[tokio::test]
    async fn pool_downscales_source_i420_for_half_resolution_layer() {
        const SRC_W: u32 = 64;
        const SRC_H: u32 = 64;
        const HALF_W: u32 = 32;
        const HALF_H: u32 = 32;

        // Build a constant-Y I420 at source dims so the encoder
        // has stable input. (Random-pattern input would change the
        // encoder's keyframe cadence and complicate timing.)
        let i420_size = (SRC_W * SRC_H * 3 / 2) as usize;
        let mut frame_data = vec![0u8; i420_size];
        for byte in &mut frame_data[(SRC_W * SRC_H) as usize..] {
            *byte = 128;
        }
        let frame_arc = Arc::new(frame_data);

        // Pool with one always-on VP8 layer at HALF_W × HALF_H,
        // even though the source is SRC_W × SRC_H. This is the
        // shape the simulcast path uses — multiple layers, each
        // smaller than the source.
        // Half-of-source layer factory. The pool gets a single
        // always-on layer at HALF_W × HALF_H derived from the
        // construction (SRC_W, SRC_H) call.
        let pool = EncoderPool::new(
            SRC_W,
            SRC_H,
            30,
            |_w, _h| {
                vec![LayerSpec {
                    rid: SimulcastRid::half(),
                    width: HALF_W,
                    height: HALF_H,
                    target_bitrate_kbps: 400,
                    framerate: 30,
                }]
            },
            None,
        );

        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (mut subs, _lease) = pool.subscribe(&prefs).expect("subscribe");
        assert_eq!(subs.len(), 1, "should get one always-on VP8 sub");
        let mut rx = subs.remove(0).frames;

        tokio::time::sleep(Duration::from_millis(50)).await;

        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&frame_arc), Instant::now());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Expect encoded packets within 2s. Pre-fix the encoder
        // would reject the size-mismatched buffer on every push
        // and never produce output.
        let ef = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("encoded frame from half-layer must arrive within 2s")
            .expect("broadcast must stay open");
        assert!(
            !ef.data.is_empty(),
            "encoded frame from half-layer must be non-empty",
        );
    }

    /// **3c.3b.4h regression test.** When `EncoderPool::new` is
    /// passed an explicit metrics counters Arc, the pool's encoder
    /// thread bumps `encode_frames` and `encode_freshness_us_sum` per
    /// emitted packet. Without this wiring the dashboard's
    /// fps/latency metrics would read zero (pool is the sole
    /// producer since 3c.4b). Pins the wiring end-to-end: explicit
    /// counters Arc → pool → encoder thread → counter increments
    /// observable from the test.
    #[tokio::test]
    async fn pool_encoder_thread_increments_metrics_counters() {
        const W: usize = 64;
        const H: usize = 64;
        let i420_size = W * H * 3 / 2;
        let mut frame_data = vec![0u8; i420_size];
        for byte in &mut frame_data[W * H..] {
            *byte = 128;
        }
        let frame_arc = Arc::new(frame_data);

        // Construct pool with an EXPLICIT counters Arc (production
        // path) and hold a clone so the test can read post-encode.
        let counters = Arc::new(crate::display::DisplayMetricsCounters::new());
        let pool = EncoderPool::new(
            W as u32,
            H as u32,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            Some(Arc::clone(&counters)),
        );

        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (mut subs, _lease) = pool.subscribe(&prefs).expect("subscribe");
        let mut rx = subs.remove(0).frames;

        // Wait for encoder thread construction.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Push a handful of frames so the encoder produces packets.
        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&frame_arc), Instant::now());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Drain at least one encoded packet so we know the encoder
        // ran (and therefore had a chance to bump counters).
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("encoded frame within 2s")
            .expect("broadcast not closed");

        // Give the encoder a moment to process additional pushes
        // (the first packet only proves one encode happened; we want
        // the cumulative counter > 0 across multiple).
        tokio::time::sleep(Duration::from_millis(100)).await;

        let frames_encoded = counters.encode_frames.load(Ordering::SeqCst);
        let freshness_sum = counters.encode_freshness_us_sum.load(Ordering::SeqCst);

        assert!(
            frames_encoded > 0,
            "encode_frames must be incremented per encoded packet; got \
             {frames_encoded}. Without this, dashboard fps reads zero \
             (pool is the sole producer since 3c.4b).",
        );
        assert!(
            freshness_sum > 0,
            "encode_freshness_us_sum must accumulate from frame.arrived → \
             encoded packet emission; got {freshness_sum}",
        );
    }

    /// **3c.3b.4i regression test.** Pool encoder must NOT bump
    /// `encode_frames` / `encode_freshness_us_sum` / `encode_drops`
    /// when its `frames_tx` has zero subscribers. The production
    /// scenario this protects: an H.264-only session leaves the
    /// always-on VP8 encoder alive (it always spawns) even though
    /// no peer ever subscribes to it — the only consumers are on
    /// the on-demand H.264 slot. Without the gate, VP8's
    /// unsubscribed packets would be counted alongside H.264's
    /// real packets and inflate dashboard fps. This test pins the
    /// gate: construct a pool, push frames WITHOUT subscribing,
    /// assert counters stay zero. Then subscribe, push more,
    /// assert they start incrementing.
    #[tokio::test]
    async fn pool_encoder_does_not_count_metrics_without_subscribers() {
        const W: usize = 64;
        const H: usize = 64;
        let mut frame_data = vec![0u8; W * H * 3 / 2];
        for byte in &mut frame_data[W * H..] {
            *byte = 128;
        }
        let frame_arc = Arc::new(frame_data);

        let counters = Arc::new(crate::display::DisplayMetricsCounters::new());
        let pool = EncoderPool::new(
            W as u32,
            H as u32,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            Some(Arc::clone(&counters)),
        );

        // Phase 1: NO subscribe. Encoder thread is alive and
        // consuming I420 (always-on), but its frames_tx has zero
        // receivers — the gate must skip metric increments.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for _ in 0..10 {
            pool.push_i420_frame(Arc::clone(&frame_arc), Instant::now());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Wait for the encoder to finish processing the pushed frames.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let frames_no_sub = counters.encode_frames.load(Ordering::SeqCst);
        let freshness_no_sub = counters.encode_freshness_us_sum.load(Ordering::SeqCst);
        assert_eq!(
            frames_no_sub, 0,
            "encode_frames must NOT increment when encoder has zero \
             subscribers; got {frames_no_sub}. Pre-3c.3b.4i: legacy-\
             only sessions saw doubled metrics because pool always-\
             on VP8 counted alongside the legacy encoder.",
        );
        assert_eq!(
            freshness_no_sub, 0,
            "encode_freshness_us_sum must NOT accumulate when encoder \
             has zero subscribers; got {freshness_no_sub}",
        );

        // Phase 2: subscribe and push more. Counters MUST start
        // incrementing now — confirms the gate's positive case
        // still works (regression guard against an over-eager fix
        // that gates everything off).
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (mut subs, _lease) = pool.subscribe(&prefs).expect("subscribe");
        let _rx = subs.remove(0).frames;

        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&frame_arc), Instant::now());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;

        let frames_with_sub = counters.encode_frames.load(Ordering::SeqCst);
        assert!(
            frames_with_sub > 0,
            "encode_frames MUST increment after a subscriber attaches; \
             got {frames_with_sub}. Gate must only skip when there are \
             actually zero consumers.",
        );
    }

    /// Dropping the pool shuts down encoder threads. This is the
    /// regression guard for the "pool drop leaks encoder threads" class
    /// of bug — if we forget to cancel shutdown tokens or drop the
    /// i420_tx sender, encoder threads linger forever and cause the
    /// same class of X11 capture-thread-leak that phase 1 fixed for
    /// the capture side.
    #[tokio::test]
    async fn pool_drop_shuts_down_encoders() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (mut subs, lease) = pool.subscribe(&prefs).expect("subscribe must succeed");
        let mut rx = subs.remove(0).frames;

        // Drop lease first (peer disconnect) then pool (session
        // teardown). The lease holds an `Arc<EncoderPoolInner>`, so
        // `drop(pool)` alone wouldn't reach `EncoderPoolInner::drop`
        // while the lease was alive — which matches production:
        // the session can't fully tear down until every peer's lease
        // has been released.
        drop(lease);
        drop(pool);

        // The thread may still be mid-blocking_recv when drop fires.
        // CancellationToken::cancel + i420_tx-drop together guarantee
        // it exits, but we give it a generous window for the thread
        // scheduler to run.
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        match result {
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                // Expected: encoder exited, sender dropped.
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // Encoder produced output before exiting; try again.
                let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("second recv should not time out");
                assert!(
                    matches!(second, Err(broadcast::error::RecvError::Closed)),
                    "after Lagged, next recv should be Closed"
                );
            }
            Ok(Ok(_frame)) => {
                // Frame arrived before close — try again for close.
                let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("second recv should not time out");
                assert!(
                    matches!(second, Err(broadcast::error::RecvError::Closed)),
                    "after frame, next recv should be Closed"
                );
            }
            Err(_) => panic!("encoder thread did not exit within 2s of pool drop"),
        }
    }

    /// On-demand spawn works: empty always_on, peer asks for VP8 →
    /// pool spawns a VP8 on-demand encoder, refcount = 1. Uses VP8
    /// so the test doesn't depend on the platform's H.264 backend.
    #[tokio::test]
    async fn on_demand_spawns_on_first_peer() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None); // no always-on
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Nothing on-demand yet.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None
        );

        let (subs, _lease) = pool.subscribe(&prefs).expect("subscribe must succeed");
        assert_eq!(
            subs.len(),
            1,
            "on-demand spawn must return one subscription"
        );
        assert_eq!(subs[0].id.codec, CodecKind::Vp8);

        // Refcount = 1 after first subscribe.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1)
        );
    }

    /// Second peer subscribing to the same on-demand codec bumps
    /// refcount without spawning a new encoder.
    #[tokio::test]
    async fn on_demand_shares_encoder_across_peers() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let (subs_a, _lease_a) = pool.subscribe(&prefs).expect("subscribe a");
        let (subs_b, _lease_b) = pool.subscribe(&prefs).expect("subscribe b");
        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_eq!(subs_a[0].id, subs_b[0].id, "same EncoderId across peers");

        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(2)
        );
    }

    /// Lease drop decrements refcount. When it hits zero, encoder is
    /// torn down — subsequent subscribe with same codec spawns fresh.
    /// Validates the RAII release path that replaced the explicit
    /// `release(&prefs)` call.
    #[tokio::test]
    async fn on_demand_releases_tear_down_at_refcount_zero() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Two peers.
        let (_subs_a, lease_a) = pool.subscribe(&prefs).expect("subscribe a");
        let (_subs_b, lease_b) = pool.subscribe(&prefs).expect("subscribe b");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(2)
        );

        // Drop one lease: refcount drops to 1, encoder stays.
        drop(lease_a);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1)
        );

        // Drop last lease: refcount hits 0, encoder torn down, slot
        // removed from the map.
        drop(lease_b);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None,
            "slot must be removed when refcount hits 0"
        );

        // A fresh subscribe spawns a new slot at refcount 1.
        let (_subs_c, _lease_c) = pool.subscribe(&prefs).expect("subscribe c");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1)
        );
    }

    /// **3c.3b.2b finding 2 fix.** `release_on_demand_subset` releases
    /// only the specified IDs while keeping the lease alive for the
    /// rest. After it runs, the lease's full release (Drop) handles
    /// the remaining claims; the partially-released slots' refcount
    /// is unaffected by the eventual full release.
    ///
    /// Setup pins the per-id semantics by:
    ///   1. Two leases on the same VP8 on-demand slot → refcount=2.
    ///   2. Lease A partial-releases the VP8 id → its claim removed
    ///      from on_demand_refs, slot refcount → 1. Lease B's claim
    ///      still alive.
    ///   3. Lease A's Drop is now a no-op for VP8 (removed from
    ///      on_demand_refs above).
    ///   4. Lease B's Drop releases its claim → refcount → 0 → slot
    ///      torn down.
    #[tokio::test]
    async fn release_on_demand_subset_decrements_only_specified_ids() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let (_subs_a, mut lease_a) = pool.subscribe(&prefs).expect("subscribe a");
        let (_subs_b, lease_b) = pool.subscribe(&prefs).expect("subscribe b");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(2),
            "two leases against the same on-demand slot → refcount 2"
        );
        assert_eq!(lease_a.on_demand_count(), 1);

        // Lease A partial-releases its VP8 claim. Refcount drops to
        // 1 (Lease B still holds), slot stays alive.
        let vp8_full = EncoderId::new(CodecKind::Vp8, SimulcastRid::full());
        lease_a.release_on_demand_subset(&[vp8_full.clone()]);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1),
            "partial release decrements specified slot's refcount"
        );
        assert_eq!(
            lease_a.on_demand_count(),
            0,
            "partial release removes the claim from on_demand_refs"
        );

        // Drop Lease A entirely. Its full-release iterates an empty
        // on_demand_refs (we already partial-released VP8) → no-op
        // for VP8 → refcount stays at 1.
        drop(lease_a);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1),
            "Lease A drop must NOT double-release the slot it already \
             released via release_on_demand_subset"
        );

        // Drop Lease B → refcount → 0 → torn down.
        drop(lease_b);
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None,
            "Lease B drop releases the last claim; slot torn down"
        );
    }

    /// IDs not in the lease's on_demand_refs are silently skipped.
    /// Two scenarios:
    ///   - Always-on slot id (never refcounted) → no-op.
    ///   - Codec the lease never claimed → no-op.
    /// The intake passes "every inactive subscription's id" without
    /// distinguishing always-on from on-demand, relying on this
    /// silent-skip behaviour to keep the call site simple.
    #[tokio::test]
    async fn release_on_demand_subset_silently_skips_unknown_ids() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );
        // VP8 is always-on; no on-demand claim. Subscribe still
        // returns the always-on sub but lease has empty on_demand_refs.
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (_subs, mut lease) = pool.subscribe(&prefs).expect("subscribe always-on VP8");
        assert_eq!(lease.on_demand_count(), 0);

        // Pass an always-on id and a never-claimed id; both should
        // be silently skipped (no-op, no panic).
        let vp8_full = EncoderId::new(CodecKind::Vp8, SimulcastRid::full());
        let h264_full = EncoderId::new(CodecKind::H264, SimulcastRid::full());
        lease.release_on_demand_subset(&[vp8_full, h264_full]);
        assert_eq!(lease.on_demand_count(), 0);
    }

    /// Empty `ids` slice is a no-op fast-path (skips even the lock
    /// acquisition). Pinning so a future "always partition" refactor
    /// doesn't accidentally make a hot path slower.
    #[tokio::test]
    async fn release_on_demand_subset_empty_ids_is_noop() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (_subs, mut lease) = pool.subscribe(&prefs).expect("subscribe on-demand VP8");
        assert_eq!(lease.on_demand_count(), 1);

        lease.release_on_demand_subset(&[]);
        assert_eq!(lease.on_demand_count(), 1, "empty ids must not touch refs");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1)
        );
    }

    /// Finding 3 in the 3c.0a review: construction runs off-lock.
    /// Two subscribe calls racing for the same on-demand codec must
    /// both succeed, end up sharing one slot, with refcount = 2 and
    /// no deadlock. Spawns two concurrent subscribes via tokio tasks;
    /// joins them and asserts final state.
    ///
    /// Race coverage is also asymmetric timing: one subscribe gets
    /// slightly ahead in its construction, other subscribe races
    /// behind. Both paths of the pass-3 dedup (race win / race loss)
    /// are exercised over many runs even though any single run hits
    /// only one ordering.
    #[tokio::test]
    async fn subscribe_race_for_same_on_demand_codec() {
        let pool = Arc::new(EncoderPool::new(64, 64, 30, |_, _| vec![], None));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let pool_a = Arc::clone(&pool);
        let prefs_a = prefs.clone();
        let pool_b = Arc::clone(&pool);
        let prefs_b = prefs.clone();

        let task_a = tokio::task::spawn_blocking(move || pool_a.subscribe(&prefs_a));
        let task_b = tokio::task::spawn_blocking(move || pool_b.subscribe(&prefs_b));

        let (result_a, result_b) = tokio::join!(task_a, task_b);
        let (subs_a, _lease_a) = result_a.expect("task a join").expect("subscribe a");
        let (subs_b, _lease_b) = result_b.expect("task b join").expect("subscribe b");

        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_eq!(
            subs_a[0].id, subs_b[0].id,
            "concurrent subscribes must end up sharing one slot"
        );
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(2),
            "refcount must be exactly 2 — both subscribers counted \
             (never 1 from a missed install, never 3+ from a double-install)"
        );
    }

    /// Explicit `PoolLease::release` is equivalent to dropping the
    /// lease — consumes the lease, fires the sync release, subsequent
    /// implicit Drop is a no-op. Guards against double-release ever
    /// over-decrementing the refcount.
    #[tokio::test]
    async fn on_demand_lease_explicit_release() {
        let pool = EncoderPool::new(64, 64, 30, |_, _| vec![], None);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (_subs, lease) = pool.subscribe(&prefs).expect("subscribe");
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            Some(1)
        );
        lease.release();
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None
        );
    }

    /// Mixed always-on + on-demand: peer supporting both codecs gets
    /// subscriptions from each source. Always-on ignores refcount
    /// (still tracked only for on-demand).
    #[tokio::test]
    async fn pool_mixes_always_on_and_on_demand_subscriptions() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Peer supporting both VP8 (always-on) and H.264 (on-demand).
        // H.264 on-demand spawn synchronously calls select_codec_for_mime;
        // on hosts without a working H.264 backend this returns Err and
        // the codec is skipped (only VP8 subscription survives). On
        // hosts where it works, we get both subscriptions.
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let (subs, _lease) = pool.subscribe(&prefs).expect("subscribe must succeed");

        // VP8 from always-on is guaranteed. H.264 on-demand is
        // best-effort — depends on platform backend availability.
        let codecs: Vec<CodecKind> = subs.iter().map(|s| s.id.codec).collect();
        assert!(
            codecs.contains(&CodecKind::Vp8),
            "VP8 always-on must be present"
        );

        // VP8 is in always_on, not on_demand — refcount tracking only
        // applies to on-demand slots.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None
        );

        // If H.264 backend worked, we should see a 1-refcount H.264
        // slot. If not, refcount is None. Assert the condition is
        // consistent with what came back in the subscription set.
        let h264_in_subs = codecs.contains(&CodecKind::H264);
        let h264_refcount = pool.on_demand_refcount(CodecKind::H264, SimulcastRid::full());
        assert_eq!(
            h264_in_subs,
            h264_refcount.is_some(),
            "H.264 subscription presence must agree with refcount presence"
        );
    }

    /// **#71 defensive coverage**: subscribe with H.264 in prefs,
    /// drop the lease, assert the H.264 on-demand slot is torn down.
    /// Guards against the encoder-pool stale-lifecycle scenario
    /// observed during the #67 federated H.264 attempt: the original
    /// symptom is no longer reachable via the federated path (#67's
    /// VP8 codec preference pin in `PeerDisplayConnection.connect()`
    /// blocks H.264 negotiation), but the local DisplaySlot path on
    /// macOS still negotiates H.264 by default (#58), so this test
    /// codifies the lifecycle invariant for any H.264 demand source
    /// that lands in the future.
    ///
    /// Cross-platform: skips the full lifecycle assertion when the
    /// host's H.264 backend is unavailable (e.g. CI without ffmpeg /
    /// openh264 / VideoToolbox). When the backend works, asserts
    /// `Some(1)` after subscribe and `None` after lease drop.
    #[tokio::test]
    async fn h264_on_demand_releases_at_refcount_zero() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let (subs, lease) = pool.subscribe(&prefs).expect("subscribe must succeed");

        let h264_in_subs = subs.iter().any(|s| s.id.codec == CodecKind::H264);
        if !h264_in_subs {
            // Backend unavailable on this host; skip the lifecycle
            // assertion. The mixed-codec subscribe test
            // (`pool_mixes_always_on_and_on_demand_subscriptions`)
            // already covers the "skipped silently" contract.
            return;
        }

        assert_eq!(
            pool.on_demand_refcount(CodecKind::H264, SimulcastRid::full()),
            Some(1),
            "H.264 on-demand slot must show refcount 1 immediately after subscribe"
        );

        drop(lease);

        assert_eq!(
            pool.on_demand_refcount(CodecKind::H264, SimulcastRid::full()),
            None,
            "H.264 slot must be removed when refcount hits 0; if this fires, the \
             encoder will keep running with no consumer (the #71 stale-encoder \
             symptom — see commit message of the fix that lands here)"
        );
    }

    // -------------------------------------------------------------------
    // Phase 4d.0: pause_layer / resume_layer / is_layer_paused
    // -------------------------------------------------------------------

    /// Default state: every encoder slot starts with `paused == false`,
    /// and `is_layer_paused` reflects that. Pins the contract that
    /// pool.subscribe wires up an active encoder, NOT a paused one
    /// (the layer-selection policy in 4d.2 explicitly pauses; nothing
    /// implicitly paused at construction).
    #[tokio::test]
    async fn pool_layer_paused_defaults_false() {
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        for rid in [
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ] {
            assert_eq!(
                pool.is_layer_paused(CodecKind::Vp8, rid.clone()),
                Some(false),
                "VP8 simulcast layer {} must start un-paused",
                rid.as_str(),
            );
        }
    }

    /// `pause_layer` flips the slot's atomic flag; `resume_layer`
    /// flips it back. Each return `true` for known slots; both are
    /// idempotent (pause-then-pause, resume-then-resume — the second
    /// call is a no-op for the encoder thread but the API still
    /// returns true since the slot exists).
    #[tokio::test]
    async fn pool_pause_resume_layer_toggles_flag() {
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        // Pause half — full and quarter stay active.
        let paused = pool.pause_layer(CodecKind::Vp8, SimulcastRid::half());
        assert!(paused, "pause_layer must return true for known slot");
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::half()),
            Some(true)
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::full()),
            Some(false),
            "pausing one layer must not affect siblings"
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::quarter()),
            Some(false)
        );

        // Idempotent pause: second call returns true, state unchanged.
        let paused_again = pool.pause_layer(CodecKind::Vp8, SimulcastRid::half());
        assert!(
            paused_again,
            "pause_layer is idempotent on already-paused slot"
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::half()),
            Some(true)
        );

        // Resume.
        let resumed = pool.resume_layer(CodecKind::Vp8, SimulcastRid::half());
        assert!(resumed, "resume_layer must return true for known slot");
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::half()),
            Some(false)
        );

        // Idempotent resume.
        let resumed_again = pool.resume_layer(CodecKind::Vp8, SimulcastRid::half());
        assert!(
            resumed_again,
            "resume_layer is idempotent on already-active slot"
        );
    }

    /// Unknown `(codec, rid)` lookups return `false` from
    /// pause/resume and `None` from is_layer_paused — distinct from
    /// "paused" so the aggregator (4d.2) can distinguish "I asked
    /// for a layer that doesn't exist" (bug) from "the layer is
    /// paused" (expected steady state).
    #[tokio::test]
    async fn pool_pause_resume_unknown_layer_is_noop() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // No H.264 always-on; no H.264 on-demand subscribed yet.
        assert_eq!(
            pool.is_layer_paused(CodecKind::H264, SimulcastRid::full()),
            None,
            "is_layer_paused returns None for unknown (codec, rid)"
        );
        assert!(
            !pool.pause_layer(CodecKind::H264, SimulcastRid::full()),
            "pause_layer returns false for unknown (codec, rid)"
        );
        assert!(
            !pool.resume_layer(CodecKind::H264, SimulcastRid::full()),
            "resume_layer returns false for unknown (codec, rid)"
        );

        // VP8 quarter layer not in the single-layer pool either.
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::quarter()),
            None
        );
    }

    /// `force_keyframe` requests survive across pause windows: a
    /// keyframe requested while paused is preserved in the flag (the
    /// encoder thread never reaches the swap while paused), so the
    /// first encoded frame after resume IS a keyframe. This is what
    /// makes the layer-selection policy's "viewer subscribed to a
    /// resumed layer" path immediately decodable.
    #[tokio::test]
    async fn pool_force_keyframe_survives_pause_window() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        pool.pause_layer(CodecKind::Vp8, SimulcastRid::full());
        let fired = pool.request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()));
        assert!(fired, "request_keyframe still admits while paused");

        // The flag is set on the encoder handle. The encoder thread
        // is paused, so it never reaches the `force_keyframe.swap`
        // call — the flag stays set.
        let handle_paused = &pool.always_on()[0];
        assert!(
            handle_paused.force_keyframe.load(Ordering::SeqCst),
            "force_keyframe flag must be set on the handle while paused"
        );

        // Resume — the next encode (next captured frame) will swap
        // the flag and produce a keyframe. We can't drive a real
        // encoder here without a bridge feeding I420, but the flag
        // surviving across pause is the contract the encoder loop
        // depends on.
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        let handle_resumed = &pool.always_on()[0];
        assert!(
            handle_resumed.force_keyframe.load(Ordering::SeqCst),
            "force_keyframe flag must STILL be set after resume — \
             the encoder thread will swap+consume on its next encode"
        );
    }

    /// **Review fix**: `resume_layer` MUST force a keyframe on the
    /// paused → active transition. Without this, the first post-
    /// resume frame is a P-frame referencing pre-pause state — stale
    /// for subscribers that lost reference frames during the pause,
    /// missing entirely for subscribers that joined during the pause.
    ///
    /// Pin the contract: pause clears nothing on the force_keyframe
    /// flag; resume from paused sets it.
    #[tokio::test]
    async fn pool_resume_layer_from_paused_sets_force_keyframe() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Initial state: not paused, force_keyframe clear. Pre-condition
        // for the test (otherwise we'd be measuring noise).
        let handle = &pool.always_on()[0];
        assert!(!handle.force_keyframe.load(Ordering::SeqCst));
        assert!(!handle.paused.load(Ordering::SeqCst));

        // Pause does NOT touch force_keyframe.
        pool.pause_layer(CodecKind::Vp8, SimulcastRid::full());
        assert!(
            !handle.force_keyframe.load(Ordering::SeqCst),
            "pause_layer must not touch force_keyframe"
        );

        // Resume from paused → force_keyframe set.
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        assert!(
            handle.force_keyframe.load(Ordering::SeqCst),
            "resume_layer from paused MUST set force_keyframe so the \
             first post-resume encode is decodable for any subscriber \
             whose decoder state went stale during the pause"
        );
    }

    /// Idempotent resume on an already-active slot must NOT newly
    /// force a keyframe — re-firing on every resume call would burn
    /// peak-bandwidth keyframes for nothing whenever the aggregator
    /// (4d.2) recomputes layer state and "resumes" something that
    /// was never paused.
    ///
    /// Uses `swap(false, ..)` to detect the transition: when the
    /// previous paused value was already false, no force fires.
    #[tokio::test]
    async fn pool_resume_layer_on_already_active_does_not_force_keyframe() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        let handle = &pool.always_on()[0];
        assert!(!handle.force_keyframe.load(Ordering::SeqCst));
        assert!(!handle.paused.load(Ordering::SeqCst));

        // First resume on already-active slot: no transition, no force.
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        assert!(
            !handle.force_keyframe.load(Ordering::SeqCst),
            "resume_layer on already-active slot must NOT force a keyframe"
        );

        // Repeated resume calls also no-op the keyframe force.
        for _ in 0..5 {
            pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        }
        assert!(
            !handle.force_keyframe.load(Ordering::SeqCst),
            "repeated resume_layer on already-active slot must NOT \
             accumulate keyframe forces"
        );

        // Sanity: the only way force_keyframe gets set without an
        // explicit request_keyframe is via paused → active edge.
        pool.pause_layer(CodecKind::Vp8, SimulcastRid::full());
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        assert!(
            handle.force_keyframe.load(Ordering::SeqCst),
            "paused → active edge must set force_keyframe — sanity \
             check that the swap-based detection does fire on transitions"
        );
    }

    /// Pause/resume targets the right layer in a multi-layer pool.
    /// Pause full, leave half + quarter active; verify each one's
    /// state independently. Pins per-(codec, rid) routing so a
    /// future refactor that switches the lookup data structure can't
    /// accidentally collapse layer state.
    #[tokio::test]
    async fn pool_pause_resume_targets_correct_layer_in_simulcast() {
        let pool = EncoderPool::new(64, 64, 30, |w, h| LayerSpec::vp8_simulcast(w, h, 30), None);

        // Pause full only.
        pool.pause_layer(CodecKind::Vp8, SimulcastRid::full());
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::full()),
            Some(true)
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::half()),
            Some(false)
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::quarter()),
            Some(false)
        );

        // Pause quarter while full is paused; verify both paused +
        // half still active.
        pool.pause_layer(CodecKind::Vp8, SimulcastRid::quarter());
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::full()),
            Some(true)
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::half()),
            Some(false)
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::quarter()),
            Some(true)
        );

        // Resume full only — quarter stays paused.
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::full()),
            Some(false)
        );
        assert_eq!(
            pool.is_layer_paused(CodecKind::Vp8, SimulcastRid::quarter()),
            Some(true)
        );
    }

    /// **End-to-end behavioral test**: paused layer's encoder thread
    /// consumes I420 frames (so the broadcast doesn't lag) but
    /// produces NO encoded output. Resume restores production.
    ///
    /// Drives a real encoder via `push_i420_frame` (no mocking) and
    /// observes the encoded-frames broadcast directly. This is the
    /// contract the layer-selection policy (4d.2) actually relies on
    /// — pausing must save real encoder CPU, not just flip a flag.
    ///
    /// The 200ms quiet-window assertion gives the encoder thread
    /// plenty of time to wake from blocking_recv, see the pause flag,
    /// and skip the encode. A real encode takes single-digit ms; if
    /// the pause check were broken, frames would arrive within ~10ms
    /// and the test would fail well within the window.
    #[tokio::test]
    async fn pool_paused_encoder_produces_no_frames_resume_restores() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        // Subscribe directly to the always-on full layer's broadcast.
        // We hold this receiver to observe encoded output.
        let mut frames_rx = {
            let always_on = pool.always_on();
            always_on[0].subscribe()
        };

        // Pause BEFORE pushing any frames. Push several I420 frames
        // and verify nothing arrives on the broadcast within 200ms.
        pool.pause_layer(CodecKind::Vp8, SimulcastRid::full());

        let i420 = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&i420), Instant::now());
        }

        let quiet_window = std::time::Duration::from_millis(200);
        match tokio::time::timeout(quiet_window, frames_rx.recv()).await {
            Err(_timeout) => {
                // Expected: no frames arrived during pause window.
            }
            Ok(Ok(_frame)) => {
                panic!(
                    "paused encoder produced an encoded frame within {}ms; \
                     the pause flag check in the encoder loop is broken \
                     (the encode + broadcast happened despite paused=true)",
                    quiet_window.as_millis(),
                );
            }
            Ok(Err(e)) => {
                panic!("broadcast error during pause window: {e:?}");
            }
        }

        // Resume — push a fresh frame and verify a frame arrives.
        // The first post-resume encode should also be a keyframe
        // (the encoder's natural cold-start behavior plus our
        // implicit "first encode after a quiet period" treatment).
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());
        pool.push_i420_frame(Arc::clone(&i420), Instant::now());

        let active_window = std::time::Duration::from_secs(2);
        match tokio::time::timeout(active_window, frames_rx.recv()).await {
            Ok(Ok(_frame)) => {
                // Got it — resume restored encoding.
            }
            Ok(Err(e)) => {
                panic!("broadcast error after resume: {e:?}");
            }
            Err(_timeout) => {
                panic!(
                    "resumed encoder produced no frame within {}s — \
                     the resume path is broken",
                    active_window.as_secs(),
                );
            }
        }
    }

    /// **Review fix end-to-end test**: the first post-resume encoded
    /// frame must be a keyframe even after the encoder has already
    /// produced a P-frame in this session (i.e., not just the natural
    /// cold-start keyframe). Without resume forcing a keyframe,
    /// subscribers whose decoder state went stale during the pause
    /// would have garbage / black until the next natural GOP keyframe
    /// (~30 frames at kf_max_dist=30, i.e., ~1s on idle desktops or
    /// indefinitely on fully static content).
    ///
    /// Test sequence:
    ///   1. Push frame, drain — naturally a cold-start keyframe.
    ///   2. Push frame, drain — naturally a P-frame (no force).
    ///   3. Pause, resume.
    ///   4. Push frame, drain — assert IS a keyframe.
    ///
    /// Step 2 is the load-bearing one: it ensures we've moved past
    /// the encoder's natural cold-start keyframe so step 4's keyframe
    /// can only have come from the resume_layer force.
    #[tokio::test]
    async fn pool_resume_after_prior_p_frame_produces_keyframe() {
        let pool = EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        );

        let mut frames_rx = {
            let always_on = pool.always_on();
            always_on[0].subscribe()
        };

        // Helper: push frames + drain encoded output; return the
        // sequence of `is_keyframe` flags collected. Drains greedily
        // up to a short deadline to absorb any per-frame multi-packet
        // splits or queued output.
        async fn push_and_drain(
            pool: &EncoderPool,
            rx: &mut broadcast::Receiver<Arc<EncodedFrame>>,
            i420: &Arc<Vec<u8>>,
        ) -> Vec<bool> {
            pool.push_i420_frame(Arc::clone(i420), Instant::now());
            let deadline = std::time::Duration::from_secs(2);
            let mut got: Vec<bool> = Vec::new();
            // First frame: wait up to 2s.
            match tokio::time::timeout(deadline, rx.recv()).await {
                Ok(Ok(frame)) => got.push(frame.is_keyframe),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => return got,
                Err(_) => return got,
            }
            // Drain any additional frames immediately available without
            // blocking — handles multi-packet outputs from one encode
            // (rare for VP8 small frames but defensive).
            while let Ok(Ok(frame)) =
                tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await
            {
                got.push(frame.is_keyframe);
            }
            got
        }

        let i420 = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);

        // 1. Cold-start frame — naturally a keyframe.
        let frames1 = push_and_drain(&pool, &mut frames_rx, &i420).await;
        assert!(
            !frames1.is_empty(),
            "cold-start push must produce at least one frame"
        );
        assert!(
            frames1[0],
            "cold-start frame must be a keyframe (encoder's natural \
             behavior on first encode)"
        );

        // 2. Second frame — naturally a P-frame, no force_keyframe set.
        // VP8 with identical content ([0u8; …]) emits a tiny P-frame
        // referencing the cold-start keyframe (verified by the
        // existing vp8.rs tests).
        let frames2 = push_and_drain(&pool, &mut frames_rx, &i420).await;
        assert!(
            !frames2.is_empty(),
            "second push must produce at least one frame"
        );
        assert!(
            !frames2[0],
            "second frame must be a P-frame (no force_keyframe set; \
             encoder cadence has not yet hit kf_max_dist) — got \
             keyframe, which means the test setup is producing \
             keyframes for the wrong reason and the post-resume \
             keyframe assertion below would be ambiguous"
        );

        // 3. Pause then resume — the resume call should set
        // force_keyframe on the encoder handle. The encoder thread
        // will swap+consume the flag on the next push.
        pool.pause_layer(CodecKind::Vp8, SimulcastRid::full());
        pool.resume_layer(CodecKind::Vp8, SimulcastRid::full());

        // 4. Push a frame — assert the result is a keyframe. The
        // encoder is configured with kf_max_dist=30 and only 2
        // frames have been encoded, so the natural-cadence keyframe
        // is ~28 frames away. The ONLY way this frame is a keyframe
        // is if resume_layer set force_keyframe.
        let frames3 = push_and_drain(&pool, &mut frames_rx, &i420).await;
        assert!(
            !frames3.is_empty(),
            "post-resume push must produce at least one frame"
        );
        assert!(
            frames3[0],
            "post-resume frame MUST be a keyframe — natural cadence \
             is ~28 frames away, so a non-keyframe here means \
             resume_layer failed to set force_keyframe on the \
             paused → active transition. Subscribers whose decoder \
             state went stale during the pause would render garbage \
             until the next natural keyframe."
        );
    }
}
