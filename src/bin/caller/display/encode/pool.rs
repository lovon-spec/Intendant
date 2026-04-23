//! Shared multi-codec, multi-layer encoder pool for one display.
//!
//! ## Why this exists
//!
//! The pre-pool design has one [`Encoder`](super::Encoder) per
//! [`DisplaySession`], with the codec locked to the first peer's offer
//! (see `display/mod.rs:396` `codec_mime: RwLock<&'static str>` and
//! `display/mod.rs:1044-1048` "First peer -- negotiate codec from SDP").
//! Every subsequent viewer must accept that locked codec or its WebRTC
//! offer fails outright with "peer does not support session codec
//! video/H264 with compatible profile" — exactly the symptom that bit
//! us in the multi-browser federation E2E session.
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
//! - **Precedent**: `str0m`'s own SFU example (`examples/chat.rs`) keeps
//!   one publisher [`Rtc`] and fans out to N subscriber [`Rtc`]s, with
//!   per-peer payload-type translation via [`Writer::match_params`]
//!   (str0m docs.rs).
//!
//! The right pattern is **shared encoder pool + per-peer forwarding**:
//! a small bank of encoders (typically 1-3) produces frames that all
//! peers consume; each peer's [`Rtc`] picks which codec/layer it can
//! decode and forwards just those frames. See
//! [`crate::display::forward`] for the per-peer forwarder side.
//!
//! ## Pool composition
//!
//! Each [`EncoderPool`] holds two kinds of encoders:
//!
//! - **Always-on** (constructed at pool creation): VP8 simulcast layers,
//!   typically three at full / half / quarter resolution. VP8 is the
//!   universal codec — Safari, Firefox, Chrome, Edge all decode it
//!   reliably and it has long history of working well for screen
//!   content. Always-on means VP8 frames are produced unconditionally
//!   so that any browser can subscribe instantly without waiting for
//!   encoder spin-up. The cost is small; one VP8 encoder at idle is
//!   ~5% of a core.
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
//! ## Relationship to str0m
//!
//! The pool produces [`Arc<EncodedFrame>`] payloads keyed by
//! [`SimulcastRid`] (str0m's `Rid` newtype). The per-peer forwarder
//! ([`crate::display::forward::PerPeerForwarder`]) takes those frames
//! and writes them via str0m's [`Writer::write_sample`], translating
//! the encoder's payload type to the peer's negotiated PT via
//! [`Writer::match_params`]. RID-tagged frames let str0m's simulcast
//! consumer track which layer is in flight per peer.
//!
//! str0m supports simulcast natively (per the str0m README's feature
//! matrix and `Rid` API in `str0m::media`). RID semantics: the
//! publisher emits frames with a per-layer RID (`f`/`h`/`q` for full,
//! half, quarter resolution by convention); the consumer-side str0m
//! filters by the active RID it has selected based on TWCC bandwidth
//! estimates.
//!
//! ## Lifecycle
//!
//! ```text
//!   pool.subscribe(peer_prefs) ─┐
//!         │                     │
//!         ▼                     ▼
//!   refcount[codec]++     Vec<EncoderSubscription> returned to peer
//!         │                     │
//!         ▼                     ▼
//!   if first peer +     forwarder reads from each subscription's
//!   not always-on:      broadcast::Receiver, picks frames matching
//!     spawn encoder     peer's chosen layer, writes to peer's str0m
//!                       Rtc with PT translation
//!         │
//!   ─── peer leaves ──→ pool.release(peer_prefs)
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
//! - Encoder spawning (the actual `tokio::task::spawn_blocking` + str0m
//!   wiring). Phase 3.
//! - Layer width/height selection logic. Phase 4.
//! - Bitrate-aware layer downgrade based on TWCC. Phase 4.
//! - Hardware encoder slot tracking (VAAPI session counter). Phase 3.
//!
//! This module currently establishes the type vocabulary and
//! orchestration contract; subsequent phases fill in the bodies.

use crate::display::EncodedFrame;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

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

/// Codec kinds the pool can produce. Closed enum because str0m only
/// supports a fixed set anyway, and adding a codec is a coordinated
/// change (new encoder backend + str0m PT registration + browser
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

    /// Whether this codec is in the always-on bank by default. Only
    /// VP8 is always-on (universal compatibility); everything else
    /// spins up on demand.
    pub fn is_always_on_default(&self) -> bool {
        matches!(self, Self::Vp8)
    }
}

impl fmt::Display for CodecKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Simulcast layer ID, RFC 8853. Newtype around String so we don't
/// confuse it with arbitrary identifiers. Maps to str0m's
/// `str0m::media::Rid` at the forwarding layer.
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

impl LayerSpec {
    /// Reference VP8 simulcast layout — three layers at full / half /
    /// quarter resolution from a source resolution. Bitrates roughly
    /// follow LiveKit's defaults (2.5 Mbps / 400 kbps / 125 kbps for
    /// 720p source).
    pub fn vp8_simulcast(source_w: u32, source_h: u32, framerate: u32) -> Vec<LayerSpec> {
        vec![
            LayerSpec {
                rid: SimulcastRid::full(),
                width: source_w,
                height: source_h,
                target_bitrate_kbps: 2500,
                framerate,
            },
            LayerSpec {
                rid: SimulcastRid::half(),
                width: source_w / 2,
                height: source_h / 2,
                target_bitrate_kbps: 400,
                framerate,
            },
            LayerSpec {
                rid: SimulcastRid::quarter(),
                width: source_w / 4,
                height: source_h / 4,
                target_bitrate_kbps: 125,
                framerate,
            },
        ]
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
///   size hints in str0m's media line)
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
struct OnDemandSlot {
    handle: EncoderHandle,
    refcount: usize,
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
    /// Always-on encoders (constructed at pool creation, never torn
    /// down). Today: a single VP8 layer at the source resolution.
    /// Phase 4 expands this into VP8 simulcast (multiple layers).
    always_on: Vec<EncoderHandle>,

    /// On-demand encoders, keyed by `(codec, rid)`. Spawned on first
    /// peer that needs them, torn down when the last peer leaves.
    on_demand: RwLock<HashMap<EncoderId, OnDemandSlot>>,

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

    /// Source resolution used for on-demand encoder spawns. Always-on
    /// layers carry their own width/height (may be downscaled simulcast
    /// layers), but on-demand encoders default to the source resolution
    /// and bitrate appropriate for their codec (from `LayerSpec::single`).
    source_width: u32,
    source_height: u32,
    framerate: u32,
}

impl EncoderPool {
    /// Construct a pool with the given always-on layer set, spawning
    /// one encoder thread per layer.
    ///
    /// * `source_width` / `source_height` — the capture resolution. Used
    ///   for on-demand encoder spawns (e.g. an H.264 encoder spun up
    ///   when the first H.264-preferring peer joins runs at the source
    ///   resolution, not at the simulcast layer size).
    /// * `framerate` — target capture rate; `duration_ms` is derived as
    ///   `1000 / framerate`.
    /// * `always_on_layers` — layers to spawn encoder threads for at
    ///   construction time. Phase 3a/3b default is a single VP8 layer
    ///   at source resolution; phase 4 replaces this with a VP8
    ///   simulcast stack.
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
        always_on_layers: Vec<LayerSpec>,
    ) -> Self {
        let duration_ms = if framerate > 0 { 1000 / framerate as u64 } else { 33 };
        let (i420_tx, _) = broadcast::channel::<I420Frame>(I420_BROADCAST_CAPACITY);

        let mut always_on = Vec::with_capacity(always_on_layers.len());
        for layer in always_on_layers {
            // Always-on bank is VP8 (universal codec, see module docs).
            let id = EncoderId::new(CodecKind::Vp8, layer.rid.clone());
            let handle = spawn_encoder_thread(id, layer, &i420_tx, duration_ms);
            always_on.push(handle);
        }

        Self {
            inner: Arc::new(EncoderPoolInner {
                always_on,
                on_demand: RwLock::new(HashMap::new()),
                keyframe_coalescer: KeyframeCoalescer::new(),
                i420_tx,
                duration_ms,
                source_width,
                source_height,
                framerate,
            }),
        }
    }

    /// Codecs this pool knows how to spawn an on-demand encoder for.
    /// Currently VP8 + H.264 (the two with wired backends).
    /// VP9 and AV1 will be added when their encoder crates are picked.
    fn on_demand_spawnable(codec: CodecKind) -> bool {
        matches!(codec, CodecKind::Vp8 | CodecKind::H264)
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
        // broadcast::send returns the receiver count on success, or
        // SendError if there are zero receivers (no encoders running).
        // Both are normal: the bridge keeps feeding regardless of
        // whether anyone is listening.
        self.inner
            .i420_tx
            .send(I420Frame { data, arrived })
            .unwrap_or(0)
    }

    /// Subscribe a peer to all encoders matching its codec preferences.
    /// Returns one [`EncoderSubscription`] per matching encoder.
    ///
    /// - **Always-on match first:** every always-on encoder whose codec
    ///   the peer supports gets a subscription, no refcount bookkeeping
    ///   (always-on encoders don't go away).
    /// - **On-demand spawn next:** for every codec in the peer's
    ///   preferences that (a) isn't already produced by an always-on
    ///   encoder and (b) the pool knows how to spawn
    ///   ([`Self::on_demand_spawnable`]), the pool ensures an on-demand
    ///   encoder is running and bumps its refcount. First peer to
    ///   subscribe spawns the encoder; subsequent peers share it.
    ///
    /// Peers preferring a codec the pool can't produce (e.g. VP9/AV1
    /// until backends land) get no subscription for that codec — the
    /// forwarder rejects with `NoCompatibleCodec` if the *whole* set
    /// is empty.
    ///
    /// Every returned [`EncoderSubscription`] increments some slot's
    /// refcount (or no-ops for always-on). Callers MUST call
    /// [`Self::release`] with the SAME prefs when the peer disconnects
    /// so on-demand encoders can be torn down.
    pub async fn subscribe(&self, prefs: &PeerCodecPreferences) -> Vec<EncoderSubscription> {
        let mut subs = Vec::new();
        let mut always_on_codecs: Vec<CodecKind> = Vec::new();

        // Always-on: no refcount, subscribe-only.
        for handle in &self.inner.always_on {
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

        // On-demand: for codecs the peer wants that aren't in
        // always_on, spawn + refcount.
        let mut on_demand = self.inner.on_demand.write().await;
        for &codec in &prefs.supported {
            if always_on_codecs.contains(&codec) {
                continue;
            }
            if !Self::on_demand_spawnable(codec) {
                continue;
            }
            // Single-layer on-demand (simulcast is phase 4, always-on only).
            let rid = SimulcastRid::full();
            let id = EncoderId::new(codec, rid);
            let source_width = self.inner.source_width;
            let source_height = self.inner.source_height;
            let framerate = self.inner.framerate;
            let i420_tx = &self.inner.i420_tx;
            let duration_ms = self.inner.duration_ms;
            let slot = on_demand.entry(id.clone()).or_insert_with(|| {
                let layer = LayerSpec::single(codec, source_width, source_height, framerate);
                let handle = spawn_encoder_thread(id.clone(), layer, i420_tx, duration_ms);
                OnDemandSlot { handle, refcount: 0 }
            });
            slot.refcount += 1;
            subs.push(EncoderSubscription {
                id: slot.handle.id.clone(),
                layer: slot.handle.layer.clone(),
                frames: slot.handle.subscribe(),
            });
        }

        subs
    }

    /// Drop a peer's references. Decrements refcount on on-demand
    /// slots; tears down encoders that hit refcount zero. Always-on
    /// slots ignore release (they live for the pool's lifetime).
    ///
    /// `prefs` must match what was passed to [`Self::subscribe`] so
    /// the pool decrements the right slots. Mismatched release is a
    /// bug — refcounts go negative at `saturating_sub` and the slot
    /// lives forever.
    pub async fn release(&self, prefs: &PeerCodecPreferences) {
        let always_on_codecs: Vec<CodecKind> = self
            .inner
            .always_on
            .iter()
            .map(|h| h.id.codec)
            .collect();

        let mut on_demand = self.inner.on_demand.write().await;
        for &codec in &prefs.supported {
            if always_on_codecs.contains(&codec) {
                continue;
            }
            if !Self::on_demand_spawnable(codec) {
                continue;
            }
            let rid = SimulcastRid::full();
            let id = EncoderId::new(codec, rid);
            let should_remove = if let Some(slot) = on_demand.get_mut(&id) {
                slot.refcount = slot.refcount.saturating_sub(1);
                if slot.refcount == 0 {
                    // Signal the encoder thread to exit. We also drop
                    // the handle (and thus its frames broadcast sender)
                    // by removing from the map — subscribers see Closed
                    // on their next recv.
                    slot.handle.shutdown.cancel();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if should_remove {
                on_demand.remove(&id);
            }
        }
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
    /// Called by the per-peer forwarder when str0m signals an inbound
    /// PLI/FIR for that peer.
    pub async fn request_keyframe(
        &self,
        codec: CodecKind,
        rid: Option<SimulcastRid>,
    ) -> bool {
        // Coalesce per (codec, rid). When rid is None we coalesce
        // against the full layer (callers using None typically mean
        // "any layer is fine, just give me a keyframe").
        let rid = rid.unwrap_or_else(SimulcastRid::full);
        if !self.inner.keyframe_coalescer.should_request(codec, &rid) {
            return false;
        }
        let id = EncoderId::new(codec, rid);
        // Always-on first.
        for handle in &self.inner.always_on {
            if handle.id == id {
                handle.force_keyframe.store(true, Ordering::SeqCst);
                return true;
            }
        }
        // On-demand.
        let on_demand = self.inner.on_demand.read().await;
        if let Some(slot) = on_demand.get(&id) {
            slot.handle.force_keyframe.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    /// Test-only access to the always-on handles. Lets tests verify
    /// pool composition without exposing internals to production code.
    #[cfg(test)]
    pub(crate) fn always_on(&self) -> &[EncoderHandle] {
        &self.inner.always_on
    }

    /// Test-only access to on-demand slot counts. Lets tests verify
    /// refcount + teardown semantics without exposing the map.
    #[cfg(test)]
    pub(crate) async fn on_demand_refcount(
        &self,
        codec: CodecKind,
        rid: SimulcastRid,
    ) -> Option<usize> {
        let id = EncoderId::new(codec, rid);
        let map = self.inner.on_demand.read().await;
        map.get(&id).map(|slot| slot.refcount)
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
        for handle in &self.always_on {
            handle.shutdown.cancel();
        }
        // try_read avoids an await in Drop. If the lock is contended
        // we skip explicit cancellation and rely on the i420_tx-drop
        // backstop — both paths converge on thread exit, just at
        // different latencies.
        if let Ok(slots) = self.on_demand.try_read() {
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
///    [`super::select_codec_for_mime`].
/// 2. Subscribes to the pool's I420 broadcast.
/// 3. In a `blocking_recv` loop: pulls the next I420 frame, swaps the
///    `force_keyframe` flag, calls `encoder.encode(...)`, and
///    broadcasts each produced packet (wrapped in
///    `Arc<EncodedFrame>`) to the per-encoder frames channel.
/// 4. Exits when `shutdown` is cancelled OR the I420 broadcast closes
///    (sender dropped at pool drop).
///
/// Encoder construction failures log + early-return — the thread exits
/// without producing frames; subscribers see a silent broadcast (no
/// `Lagged`, no `Closed` until pool drop) which surfaces as "the peer
/// negotiated a codec the system can't actually produce." Phase 3b's
/// on-demand spawn path will check the construction result before
/// returning the handle to subscribe(), so the failure is visible at
/// peer-add time rather than as silent black streams.
fn spawn_encoder_thread(
    id: EncoderId,
    layer: LayerSpec,
    i420_tx: &broadcast::Sender<I420Frame>,
    duration_ms: u64,
) -> EncoderHandle {
    let (frames_tx, _) =
        broadcast::channel::<Arc<EncodedFrame>>(ENCODER_FRAME_BROADCAST_CAPACITY);
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let shutdown = CancellationToken::new();

    let mut i420_rx = i420_tx.subscribe();
    let frames_tx_for_thread = frames_tx.clone();
    let force_kf_for_thread = Arc::clone(&force_keyframe);
    let shutdown_for_thread = shutdown.clone();
    let codec_mime = id.codec.mime();
    let width = layer.width;
    let height = layer.height;
    let bitrate_kbps = layer.target_bitrate_kbps;
    let id_for_log = id.clone();

    std::thread::spawn(move || {
        let mut encoder: Box<dyn super::Encoder> =
            match super::select_codec_for_mime(codec_mime, width, height, bitrate_kbps) {
                Ok((enc, _)) => enc,
                Err(e) => {
                    eprintln!(
                        "[encoder/pool] {} encoder construction failed for {}x{}: {}",
                        id_for_log, width, height, e
                    );
                    return;
                }
            };

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
                    // GOP cadence).
                    eprintln!(
                        "[encoder/pool] {} lagged by {} frames, skipping ahead",
                        id_for_log, n
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            let force_kf = force_kf_for_thread.swap(false, Ordering::SeqCst);

            match encoder.encode(&frame.data, duration_ms, force_kf) {
                Ok(packets) => {
                    for pkt in packets {
                        let ef = Arc::new(pkt.into_encoded_frame());
                        // Lossy broadcast: returns Err only if there
                        // are zero subscribers, which is fine.
                        let _ = frames_tx_for_thread.send(ef);
                    }
                }
                Err(e) => {
                    eprintln!("[encoder/pool] {} encode error: {}", id_for_log, e);
                }
            }
        }
    });

    EncoderHandle {
        id,
        layer,
        frames: frames_tx,
        force_keyframe,
        shutdown,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn codec_kind_mime_round_trip() {
        // mime() → CodecChoice expectation (existing constants from
        // super::). Guards against drift between the two enums.
        assert_eq!(CodecKind::Vp8.mime(), super::super::MIME_TYPE_VP8);
        assert_eq!(CodecKind::H264.mime(), super::super::MIME_TYPE_H264);
        assert_eq!(CodecKind::Vp9.mime(), "video/VP9");
        assert_eq!(CodecKind::Av1.mime(), "video/AV1");
    }

    #[test]
    fn codec_kind_from_mime_round_trips_every_kind() {
        for k in [CodecKind::Vp8, CodecKind::H264, CodecKind::Vp9, CodecKind::Av1] {
            assert_eq!(CodecKind::from_mime(k.mime()), Some(k));
        }
        assert_eq!(CodecKind::from_mime("video/HEVC"), None);
        assert_eq!(CodecKind::from_mime(""), None);
    }

    #[test]
    fn codec_kind_only_vp8_is_always_on_default() {
        assert!(CodecKind::Vp8.is_always_on_default());
        assert!(!CodecKind::H264.is_always_on_default());
        assert!(!CodecKind::Vp9.is_always_on_default());
        assert!(!CodecKind::Av1.is_always_on_default());
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
        // Order: full, half, quarter.
        assert_eq!(layers[0].rid, SimulcastRid::full());
        assert_eq!(layers[0].width, 1920);
        assert_eq!(layers[0].height, 1080);
        assert_eq!(layers[1].rid, SimulcastRid::half());
        assert_eq!(layers[1].width, 960);
        assert_eq!(layers[1].height, 540);
        assert_eq!(layers[2].rid, SimulcastRid::quarter());
        assert_eq!(layers[2].width, 480);
        assert_eq!(layers[2].height, 270);
        // Bitrate strictly descending — smaller layers are cheap.
        assert!(layers[0].target_bitrate_kbps > layers[1].target_bitrate_kbps);
        assert!(layers[1].target_bitrate_kbps > layers[2].target_bitrate_kbps);
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

    #[tokio::test]
    async fn pool_subscribes_to_always_on_codec() {
        // VP8 always-on. Peer supporting VP8 gets one subscription from
        // always_on (no on-demand spawn needed).
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

        let vp8_prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let subs = pool.subscribe(&vp8_prefs).await;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id.codec, CodecKind::Vp8);
    }

    #[tokio::test]
    async fn pool_rejects_codecs_with_no_backend() {
        // Peer advertises VP9/AV1 only — neither has a backend yet
        // (phase 4+). Always-on is VP8, no match; on-demand won't
        // spawn because on_demand_spawnable rejects them. Subscription
        // set is empty; forwarder rejects with NoCompatibleCodec.
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

        let unsupported_prefs =
            PeerCodecPreferences::new(vec![CodecKind::Vp9, CodecKind::Av1]);
        let subs = pool.subscribe(&unsupported_prefs).await;
        assert_eq!(subs.len(), 0);
    }

    #[tokio::test]
    async fn pool_request_keyframe_coalesces() {
        // Empty layer set — no encoders spawned, so request_keyframe
        // consistently returns false (coalescer admits the first, but
        // no matching encoder exists).
        let pool = EncoderPool::new(64, 64, 30, vec![]);
        let rid = SimulcastRid::full();
        // Coalescer admits first, but no encoder matches → false.
        assert!(!pool.request_keyframe(CodecKind::Vp8, Some(rid.clone())).await);
        // Second is coalesced at the coalescer layer regardless.
        assert!(!pool.request_keyframe(CodecKind::Vp8, Some(rid.clone())).await);
    }

    /// Keyframe request actually sets the encoder's atomic flag when
    /// an encoder matches. Exercises the full request_keyframe →
    /// coalescer → handle.force_keyframe path.
    #[tokio::test]
    async fn pool_request_keyframe_sets_encoder_flag() {
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

        // Initial state: flag is false.
        let handle = &pool.always_on()[0];
        assert!(!handle.force_keyframe.load(Ordering::SeqCst));

        // Fire keyframe request → flag goes true.
        let fired = pool
            .request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()))
            .await;
        assert!(fired, "request_keyframe must return true when encoder matches");
        assert!(handle.force_keyframe.load(Ordering::SeqCst));

        // Second request is coalesced (returns false) — flag stays
        // set because we haven't encoded yet (the encoder thread would
        // swap it back).
        let fired2 = pool
            .request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()))
            .await;
        assert!(!fired2);
        assert!(handle.force_keyframe.load(Ordering::SeqCst));
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

        let layer = LayerSpec::single(CodecKind::Vp8, W as u32, H as u32, 30);
        let pool = EncoderPool::new(W as u32, H as u32, 30, vec![layer]);

        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let mut subs = pool.subscribe(&prefs).await;
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
        assert!(!ef.data.is_empty(), "encoded frame payload must be non-empty");
    }

    /// Dropping the pool shuts down encoder threads. This is the
    /// regression guard for the "pool drop leaks encoder threads" class
    /// of bug — if we forget to cancel shutdown tokens or drop the
    /// i420_tx sender, encoder threads linger forever and cause the
    /// same class of X11 capture-thread-leak that phase 1 fixed for
    /// the capture side.
    #[tokio::test]
    async fn pool_drop_shuts_down_encoders() {
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let mut subs = pool.subscribe(&prefs).await;
        let mut rx = subs.remove(0).frames;

        // Drop the pool. The encoder thread must exit and the
        // per-encoder broadcast must close — the subscriber sees
        // `Closed` on its next recv.
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
        let pool = EncoderPool::new(64, 64, 30, vec![]); // no always-on
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Nothing on-demand yet.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            None
        );

        let subs = pool.subscribe(&prefs).await;
        assert_eq!(subs.len(), 1, "on-demand spawn must return one subscription");
        assert_eq!(subs[0].id.codec, CodecKind::Vp8);

        // Refcount = 1 after first subscribe.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            Some(1)
        );
    }

    /// Second peer subscribing to the same on-demand codec bumps
    /// refcount without spawning a new encoder.
    #[tokio::test]
    async fn on_demand_shares_encoder_across_peers() {
        let pool = EncoderPool::new(64, 64, 30, vec![]);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let subs_a = pool.subscribe(&prefs).await;
        let subs_b = pool.subscribe(&prefs).await;
        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_eq!(subs_a[0].id, subs_b[0].id, "same EncoderId across peers");

        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            Some(2)
        );
    }

    /// Release decrements refcount. When it hits zero, encoder is
    /// torn down — subsequent subscribe with same codec spawns fresh.
    #[tokio::test]
    async fn on_demand_releases_tear_down_at_refcount_zero() {
        let pool = EncoderPool::new(64, 64, 30, vec![]);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Two peers.
        let _subs_a = pool.subscribe(&prefs).await;
        let _subs_b = pool.subscribe(&prefs).await;
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            Some(2)
        );

        // Release one: refcount drops to 1, encoder stays.
        pool.release(&prefs).await;
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            Some(1)
        );

        // Release last: refcount hits 0, encoder torn down, slot
        // removed from the map.
        pool.release(&prefs).await;
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            None,
            "slot must be removed when refcount hits 0"
        );

        // A fresh subscribe spawns a new slot at refcount 1.
        let _subs_c = pool.subscribe(&prefs).await;
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            Some(1)
        );
    }

    /// Mixed always-on + on-demand: peer supporting both codecs gets
    /// subscriptions from each source. Always-on ignores refcount
    /// (still tracked only for on-demand).
    #[tokio::test]
    async fn pool_mixes_always_on_and_on_demand_subscriptions() {
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

        // Peer supporting both VP8 (always-on) and H.264 (on-demand
        // if backend available). We'd need an H.264 encoder to spawn,
        // which is platform-dependent. So instead test the routing
        // logic by asserting always-on VP8 works; skip real H.264
        // construction by noting the pool still returns a subscription
        // if on_demand_spawnable says yes. (The test doesn't send
        // frames, so encoder-construction failures would manifest as
        // silent broadcast close, not test failure.)
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let subs = pool.subscribe(&prefs).await;

        // VP8 from always-on + H.264 from on-demand = 2 subscriptions.
        assert_eq!(subs.len(), 2, "expected VP8 always-on + H.264 on-demand");
        let codecs: Vec<CodecKind> = subs.iter().map(|s| s.id.codec).collect();
        assert!(codecs.contains(&CodecKind::Vp8));
        assert!(codecs.contains(&CodecKind::H264));

        // On-demand H.264 refcount = 1 (always-on VP8 isn't tracked
        // in on_demand).
        assert_eq!(
            pool.on_demand_refcount(CodecKind::H264, SimulcastRid::full())
                .await,
            Some(1)
        );
        // VP8 is in always_on, not on_demand.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full())
                .await,
            None
        );
    }
}
