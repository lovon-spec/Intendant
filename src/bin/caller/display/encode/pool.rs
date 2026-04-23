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
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};

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
/// encoder runs as long as the pool keeps the matching slot. Cloning
/// the broadcast sender is the standard tokio pattern for handing out
/// per-subscriber `Receiver`s without giving subscribers shutdown
/// authority.
///
/// Phase 3 fills in the actual spawning logic; today this is the type
/// shape consumers of the pool rely on.
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
/// **This stub establishes the type vocabulary and lifecycle contract.**
/// The actual encoder spawning, broadcast wiring, and shutdown logic
/// land in phase 3 of the redesign; the methods below have signatures
/// only.
///
/// The pool is intentionally `Arc`-shareable — one pool reference goes
/// to the bridge task (which feeds I420 frames into all encoders), one
/// to each peer's forwarder (which calls `subscribe`/`release`), and
/// one to the WebRTC PLI handler (which calls `request_keyframe`).
#[derive(Clone)]
pub struct EncoderPool {
    inner: Arc<EncoderPoolInner>,
}

struct EncoderPoolInner {
    /// Always-on encoders (constructed at pool creation, never torn
    /// down). Today: VP8 simulcast layers. The pool builder decides
    /// the layout; the pool itself doesn't change it at runtime.
    always_on: Vec<EncoderHandle>,

    /// On-demand encoders, keyed by `(codec, rid)`. Spawned on first
    /// peer that needs them, torn down when the last peer leaves.
    on_demand: RwLock<HashMap<EncoderId, OnDemandSlot>>,

    /// Coalesces PLI/FIR across viewers per `(codec, rid)`.
    keyframe_coalescer: KeyframeCoalescer,
}

impl EncoderPool {
    /// Construct a pool with a fixed always-on encoder bank. Typically
    /// `LayerSpec::vp8_simulcast(width, height, fps)` mapped to handles
    /// by phase-3 spawn logic.
    ///
    /// **Stub:** in this design pass, `always_on` is accepted but the
    /// handles' encoder threads are not yet spawned. Phase 3 wires in
    /// `tokio::task::spawn_blocking` + the existing `Vp8Encoder` /
    /// `H264*Encoder` backends.
    pub fn new(always_on: Vec<EncoderHandle>) -> Self {
        Self {
            inner: Arc::new(EncoderPoolInner {
                always_on,
                on_demand: RwLock::new(HashMap::new()),
                keyframe_coalescer: KeyframeCoalescer::new(),
            }),
        }
    }

    /// Subscribe a peer to all encoders matching its codec preferences.
    /// Spawns on-demand encoders if this is the first peer that needs
    /// them. Returns one [`EncoderSubscription`] per matching encoder
    /// (multiple if simulcast — one per layer).
    ///
    /// **Stub:** returns subscriptions for already-existing always-on
    /// encoders. On-demand spawn logic is phase 3.
    pub async fn subscribe(
        &self,
        prefs: &PeerCodecPreferences,
    ) -> Vec<EncoderSubscription> {
        let mut subs = Vec::new();
        for handle in &self.inner.always_on {
            if prefs.supports(handle.id.codec) {
                subs.push(EncoderSubscription {
                    id: handle.id.clone(),
                    layer: handle.layer.clone(),
                    frames: handle.subscribe(),
                });
            }
        }
        // On-demand: phase 3 wires this. Today: only always-on slots
        // are returned; peers preferring on-demand-only codecs get an
        // empty subscription set (forwarder will treat as "no compatible
        // codec" — same as today's failure mode but isolated to that
        // peer instead of locking the whole session).
        subs
    }

    /// Drop a peer's references. Decrements refcount on on-demand
    /// slots; tears down encoders that hit refcount zero. Always-on
    /// slots ignore release (they live for the pool's lifetime).
    ///
    /// **Stub:** no-op today (no on-demand encoders to release). Phase 3
    /// adds the refcount-and-shutdown logic.
    pub async fn release(&self, _prefs: &PeerCodecPreferences) {
        // Phase 3.
    }

    /// Request a keyframe from one encoder (or all layers of one codec
    /// if `rid` is `None`). Coalesced — multiple callers within
    /// [`KEYFRAME_COALESCE_WINDOW`] result in one request.
    ///
    /// Called by the per-peer forwarder when str0m signals an inbound
    /// PLI/FIR for that peer.
    ///
    /// **Stub:** runs the coalescer correctly but does not yet forward
    /// the request to the encoder backend. Phase 3 wires the channel
    /// from coalescer → encoder thread.
    pub async fn request_keyframe(
        &self,
        codec: CodecKind,
        rid: Option<SimulcastRid>,
    ) -> bool {
        // Coalesce per (codec, rid). When rid is None we coalesce
        // against the full layer (callers using None typically mean
        // "any layer is fine, just give me a keyframe").
        let rid = rid.unwrap_or_else(SimulcastRid::full);
        self.inner.keyframe_coalescer.should_request(codec, &rid)
        // Phase 3: if the above returned true, send to encoder's
        // keyframe_tx channel.
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
    async fn pool_subscribes_only_to_supported_codecs() {
        // Build a pool with one always-on VP8 layer.
        let (tx, _) = broadcast::channel(ENCODER_FRAME_BROADCAST_CAPACITY);
        let handle = EncoderHandle {
            id: EncoderId::new(CodecKind::Vp8, SimulcastRid::full()),
            layer: LayerSpec::single(CodecKind::Vp8, 1920, 1080, 30),
            frames: tx,
        };
        let pool = EncoderPool::new(vec![handle]);

        // Peer that supports VP8 gets one subscription.
        let vp8_prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let subs = pool.subscribe(&vp8_prefs).await;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id.codec, CodecKind::Vp8);

        // Peer that only supports H.264 gets zero (no on-demand spawn
        // in this stub; phase 3 will spawn one and return it).
        let h264_prefs = PeerCodecPreferences::new(vec![CodecKind::H264]);
        let subs = pool.subscribe(&h264_prefs).await;
        assert_eq!(subs.len(), 0);
    }

    #[tokio::test]
    async fn pool_request_keyframe_coalesces() {
        let pool = EncoderPool::new(vec![]);
        let rid = SimulcastRid::full();
        // First fires.
        assert!(pool.request_keyframe(CodecKind::Vp8, Some(rid.clone())).await);
        // Immediate second is coalesced.
        assert!(!pool.request_keyframe(CodecKind::Vp8, Some(rid.clone())).await);
        // None-rid coalesces against full (the convention used by the impl).
        assert!(!pool.request_keyframe(CodecKind::Vp8, None).await);
    }
}
