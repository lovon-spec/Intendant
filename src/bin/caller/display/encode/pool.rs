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
//! peers consume; each peer's `Rtc` picks which codec/layer it can
//! decode and forwards just those frames. The per-peer forwarding
//! logic lives inside the `WebRtcPeer` driver task (in
//! `display/webrtc.rs`), not in a separate module — the driver owns
//! the `Rtc` and is the only caller that can reach str0m's
//! `Writer::write_sample`.
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
//! [`SimulcastRid`] (str0m's `Rid` newtype). The per-peer forwarding
//! lives inside each peer's `WebRtcPeer` driver task
//! (`display/webrtc.rs`), which owns the peer's `Rtc` and therefore
//! the only path to str0m's `Writer::write_sample`. Each frame
//! carries a [`crate::display::encode::PayloadSpec`]; the driver
//! resolves it to the peer's negotiated payload type via
//! `Writer::match_params` on first hit and caches the result. An
//! earlier design sketch had a separate `PerPeerForwarder` task doing
//! this work, but a separate task can't reach the driver's `Rtc`;
//! merging the forwarder into the driver sidesteps the problem.
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
//!   refcount[codec]++     (Vec<EncoderSubscription>, PoolLease)
//!         │                     │
//!         ▼                     ▼
//!   if first peer +     forwarder reads from each subscription's
//!   not always-on:      broadcast::Receiver, picks frames matching
//!     sync construct    peer's chosen layer, writes to peer's str0m
//!     + spawn           Rtc with PT translation
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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

    /// Source resolution used for on-demand encoder spawns. Always-on
    /// layers carry their own width/height (may be downscaled simulcast
    /// layers), but on-demand encoders default to the source resolution
    /// and bitrate appropriate for their codec (from `LayerSpec::single`).
    ///
    /// Atomic because [`EncoderPool::on_resize`] updates these when the
    /// capture backend reports a new resolution; `dimensions()` readers
    /// must see the updated value without taking a lock. Stores use
    /// `Ordering::SeqCst` to match the ordering model the `always_on`
    /// write lock provides — on_resize writes dimensions then swaps
    /// handles, readers see them in that order.
    source_width: AtomicU32,
    source_height: AtomicU32,
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

    /// Epoch counter incremented by [`EncoderPool::on_resize`] whenever
    /// source dimensions actually change. `EncoderPool::subscribe`
    /// snapshots this at pass 1 (when it reads `source_width`/
    /// `source_height` to build a `LayerSpec::single` for an on-demand
    /// codec), runs encoder construction OFF-lock in pass 2, then
    /// compares under the on-demand lock in pass 3 before installing
    /// the constructed encoder. If the epoch has moved, the encoder
    /// is at stale dimensions and must be cancelled rather than
    /// installed — otherwise a peer subscribing during a resize
    /// could latch an old-dimensions encoder into the pool, which
    /// would then receive post-resize I420 frames and produce
    /// misinterpreted output.
    ///
    /// Bumped inside `on_resize` only after the same-dim early-return
    /// has been cleared, so readers that observe a bumped epoch can
    /// trust the source dimensions have actually changed.
    source_gen: AtomicU64,
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
            // Use the failable constructor here and PANIC on failure —
            // always-on is the universally-available fallback path; if
            // even VP8 won't construct, there is no recovery and the
            // display pipeline is fundamentally broken. Better to fail
            // loud at pool construction than produce a silent
            // never-decoding stream.
            let id = EncoderId::new(CodecKind::Vp8, layer.rid.clone());
            let handle = try_spawn_encoder_thread(id.clone(), layer, &i420_tx, duration_ms)
                .unwrap_or_else(|e| {
                    panic!(
                        "always-on encoder {} construction failed at pool startup: {} — \
                         always-on codecs must always be constructable; a VP8 libvpx \
                         failure at startup is unrecoverable",
                        id, e,
                    )
                });
            always_on.push(handle);
        }

        Self {
            inner: Arc::new(EncoderPoolInner {
                always_on: StdRwLock::new(always_on),
                on_demand: StdMutex::new(HashMap::new()),
                keyframe_coalescer: KeyframeCoalescer::new(),
                i420_tx,
                duration_ms,
                source_width: AtomicU32::new(source_width),
                source_height: AtomicU32::new(source_height),
                framerate,
                slot_gen_counter: AtomicU64::new(0),
                source_gen: AtomicU64::new(0),
            }),
        }
    }

    /// Codecs this pool knows how to spawn an on-demand encoder for.
    /// Currently VP8 + H.264 (the two with wired backends).
    /// VP9 and AV1 will be added when their encoder crates are picked.
    fn on_demand_spawnable(codec: CodecKind) -> bool {
        matches!(codec, CodecKind::Vp8 | CodecKind::H264)
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
    pub fn dimensions(&self) -> (u32, u32) {
        (
            self.inner.source_width.load(Ordering::SeqCst),
            self.inner.source_height.load(Ordering::SeqCst),
        )
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
    /// down or re-subscribing via [`Self::subscribe`]. Matches the
    /// str0m chat.rs "re-subscribe per publisher epoch" pattern.
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
    /// Panics if a new always-on encoder fails to construct: an
    /// always-on construction failure at any lifecycle point —
    /// startup or resize — is unrecoverable by contract (see
    /// [`Self::new`]).
    pub fn on_resize(&self, new_width: u32, new_height: u32) {
        let old_width = self.inner.source_width.load(Ordering::SeqCst);
        let old_height = self.inner.source_height.load(Ordering::SeqCst);
        if (old_width, old_height) == (new_width, new_height) {
            return;
        }

        // Advance dimension atomics first so any concurrent reader —
        // the bridge's push_i420_frame dimension gate, a subscribe
        // that consults source_width/height for an on-demand
        // LayerSpec::single default — sees the new size before it
        // sees the new handles. SeqCst matches the write lock's
        // ordering so observers see (dims, handles) atomically on
        // release.
        self.inner.source_width.store(new_width, Ordering::SeqCst);
        self.inner.source_height.store(new_height, Ordering::SeqCst);
        // Bump the source_gen epoch. Any subscribe that captured
        // source_gen before this store AND is still off-lock
        // constructing its on-demand encoder will detect the
        // mismatch in pass 3 and cancel its stale-dimensions
        // encoder instead of installing it. Bumped between the
        // dimension stores and the handle swaps so the epoch
        // transition is the authoritative "resize has happened"
        // signal — concurrent readers that observe a bumped epoch
        // are guaranteed to see both the new dimensions and the
        // (about-to-be) new handles.
        self.inner.source_gen.fetch_add(1, Ordering::SeqCst);

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
            for old_handle in old_handles {
                let rescaled = rescale_layer_spec(
                    &old_handle.layer,
                    old_width,
                    old_height,
                    new_width,
                    new_height,
                );
                let new_handle = try_spawn_encoder_thread(
                    old_handle.id.clone(),
                    rescaled,
                    &self.inner.i420_tx,
                    self.inner.duration_ms,
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "on_resize: always-on respawn failed for {:?}: {} — \
                         always-on construction failure is unrecoverable by \
                         contract (see EncoderPool::new)",
                        old_handle.id, e,
                    )
                });
                always_on.push(new_handle);
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
            let old_slots: HashMap<EncoderId, OnDemandSlot> =
                std::mem::take(&mut *on_demand);
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
        // broadcast::send returns the receiver count on success, or
        // SendError if there are zero receivers (no encoders running).
        // Both are normal: the bridge keeps feeding regardless of
        // whether anyone is listening.
        self.inner
            .i420_tx
            .send(I420Frame { data, arrived })
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

    fn subscribe_once(
        &self,
        prefs: &PeerCodecPreferences,
    ) -> SubscribeAttemptOutcome {
        let mut subs = Vec::new();
        let mut always_on_codecs: Vec<CodecKind> = Vec::new();

        // Snapshot the source epoch at the start of the subscribe so
        // pass 3 can detect a race with `on_resize`. Any on-demand
        // encoder we construct in pass 2 uses dimensions we read here
        // (via pass 1's source_width/height reads); if `on_resize`
        // fires between pass 2 and pass 3, the epoch changes and the
        // encoder we built is at stale dimensions. Pass 3 checks
        // under the on-demand lock and cancels stale constructs
        // instead of installing them. Always-on subs aren't affected
        // because their handles live in `self.inner.always_on`,
        // which `on_resize` swaps atomically — their subscribe()
        // receivers observe Closed via the normal broadcast path if
        // a resize happened before they're consumed.
        let source_gen_at_start =
            self.inner.source_gen.load(Ordering::SeqCst);

        // Always-on: no refcount, subscribe-only. These are guaranteed
        // to be producing frames — EncoderPool::new panics on
        // always-on construction failure. Read lock is held only for
        // the duration of this iteration; on_resize acquires the write
        // lock to swap handles.
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
                    let layer = LayerSpec::single(
                        codec,
                        self.inner.source_width.load(Ordering::SeqCst),
                        self.inner.source_height.load(Ordering::SeqCst),
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
                &self.inner.i420_tx,
                self.inner.duration_ms,
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
            let stale_epoch = self.inner.source_gen.load(Ordering::SeqCst)
                != source_gen_at_start;
            if stale_epoch {
                for (id, handle, _layer) in &constructed {
                    eprintln!(
                        "[encoder/pool] subscribe: cancelling stale-dimensions \
                         encoder for {id:?} — on_resize fired during construction"
                    );
                    handle.shutdown.cancel();
                }
                // Don't install. Drop the lock before deciding:
                // - subs non-empty: we have always-on / fast-path
                //   codecs to serve; return partial result. Caller
                //   gets a working subscription, just not at the
                //   full set of requested codecs. This is identical
                //   to a peer that asked for codecs we don't
                //   support — same SDP semantics, no need to retry.
                // - subs empty: ALL the codecs the peer wanted were
                //   stale. Returning NoCompatibleCodec here would
                //   reject an offer that would succeed on retry.
                //   Signal the outer `subscribe` to retry instead.
                drop(on_demand);
                if subs.is_empty() {
                    return SubscribeAttemptOutcome::StaleEpochRetry;
                }
                return SubscribeAttemptOutcome::Done(Ok((
                    subs,
                    PoolLease {
                        pool: Arc::clone(&self.inner),
                        on_demand_refs,
                        released: AtomicBool::new(false),
                    },
                )));
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
                        let generation =
                            self.inner.slot_gen_counter.fetch_add(1, Ordering::SeqCst);
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
    /// Called by the per-peer forwarder when str0m signals an inbound
    /// PLI/FIR for that peer.
    pub fn request_keyframe(
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
    pub(crate) fn on_demand_refcount(
        &self,
        codec: CodecKind,
        rid: SimulcastRid,
    ) -> Option<usize> {
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
        self.inner.source_gen.load(Ordering::SeqCst)
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
///    [`super::select_codec_for_mime`].
/// 2. Subscribes to the pool's I420 broadcast.
/// 3. In a `blocking_recv` loop: pulls the next I420 frame, swaps the
///    `force_keyframe` flag, calls `encoder.encode(...)`, and
///    broadcasts each produced packet (wrapped in
///    `Arc<EncodedFrame>`) to the per-encoder frames channel.
/// 4. Exits when `shutdown` is cancelled OR the I420 broadcast closes
///    (sender dropped at pool drop).
///
/// Synchronously probes the encoder backend via
/// [`super::select_codec_for_mime`] and spawns the driver thread only
/// if construction succeeded. Returns the `EncoderHandle` on `Ok`,
/// propagates the construction error on `Err` — callers (the pool's
/// on-demand subscribe path) use the error to skip the codec rather
/// than return a ghost subscription.
///
/// Replaces the earlier in-thread construction that logged failures
/// and silently exited after the handle had already been published to
/// subscribers. That behavior surfaced as "the peer negotiated a
/// codec the system can't actually produce" — which was one of the
/// root causes the encoder-pool redesign exists to fix.
/// Rescale a `LayerSpec` proportionally from source dimensions
/// `(old_src_w, old_src_h)` to `(new_src_w, new_src_h)`. Preserves
/// the layer's ratio to its source — a full-resolution layer stays
/// at the new source dimensions, a half-resolution simulcast layer
/// stays at half of the new source, etc.
///
/// Widths and heights are rounded down to even (VP8/H.264 both
/// require even dimensions). `rid`, `target_bitrate_kbps`, and
/// `framerate` are preserved unchanged — RID identifies the layer
/// across resize, and bitrate adaptation is TWCC's responsibility
/// (future phase).
///
/// For the single-VP8-layer baseline case (phase 3c.3a), this
/// simplifies to "the new source dimensions," because the always-on
/// layer's dimensions match the source. The general form exists for
/// simulcast (phase 4), where always_on will have multiple layers at
/// different ratios.
fn rescale_layer_spec(
    spec: &LayerSpec,
    old_src_w: u32,
    old_src_h: u32,
    new_src_w: u32,
    new_src_h: u32,
) -> LayerSpec {
    // Defensive — divide-by-zero would be a bug elsewhere (pool is
    // constructed from real capture dimensions which are always > 0),
    // but emit a sensible spec rather than panicking.
    if old_src_w == 0 || old_src_h == 0 {
        return LayerSpec {
            rid: spec.rid.clone(),
            width: new_src_w & !1,
            height: new_src_h & !1,
            target_bitrate_kbps: spec.target_bitrate_kbps,
            framerate: spec.framerate,
        };
    }
    let w = (spec.width as u64 * new_src_w as u64 / old_src_w as u64) as u32;
    let h = (spec.height as u64 * new_src_h as u64 / old_src_h as u64) as u32;
    LayerSpec {
        rid: spec.rid.clone(),
        width: w & !1,
        height: h & !1,
        target_bitrate_kbps: spec.target_bitrate_kbps,
        framerate: spec.framerate,
    }
}

fn try_spawn_encoder_thread(
    id: EncoderId,
    layer: LayerSpec,
    i420_tx: &broadcast::Sender<I420Frame>,
    duration_ms: u64,
) -> Result<EncoderHandle, String> {
    // Synchronous construction probe. If this fails, we never spawn
    // the thread, never publish a handle, and the caller knows to
    // exclude this codec from the subscription set.
    let (encoder, _) = super::select_codec_for_mime(
        id.codec.mime(),
        layer.width,
        layer.height,
        layer.target_bitrate_kbps,
    )?;
    Ok(spawn_encoder_thread_with(id, layer, encoder, i420_tx, duration_ms))
}

/// Spawn the encoder driver thread with a pre-constructed [`super::Encoder`].
/// Returns immediately; the thread runs until `shutdown.cancel()` or the
/// i420 broadcast closes.
fn spawn_encoder_thread_with(
    id: EncoderId,
    layer: LayerSpec,
    encoder: Box<dyn super::Encoder>,
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
    let id_for_log = id.clone();

    std::thread::spawn(move || {
        let mut encoder = encoder;

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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

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
        assert_eq!(
            new_layer.rid, old_layer.rid,
            "rid preserved across resize"
        );

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
            let Ok(result) =
                tokio::time::timeout(remaining, old_frames_rx.recv()).await
            else {
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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

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
        let half_layer = LayerSpec {
            rid: SimulcastRid::half(),
            width: 500,
            height: 250,
            target_bitrate_kbps: 400,
            framerate: 30,
        };
        // Use vp8_simulcast-style: half-res layer on a 1000x500 source.
        let pool = EncoderPool::new(1000, 500, 30, vec![half_layer]);

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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (_subs, _lease) = pool
            .subscribe(&prefs)
            .expect("on-demand VP8 spawn");
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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
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
            vec![LayerSpec::single(CodecKind::Vp8, 64, 64, 30)],
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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(1280, 720, 30, vec![layer]);
        assert_eq!(pool.dimensions(), (1280, 720));
    }

    #[tokio::test]
    async fn pool_subscribes_to_always_on_codec() {
        // VP8 always-on. Peer supporting VP8 gets one subscription from
        // always_on (no on-demand spawn needed).
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

        let unsupported_prefs =
            PeerCodecPreferences::new(vec![CodecKind::Vp9, CodecKind::Av1]);
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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

        // Initial state: flag is false.
        let handle = &pool.always_on()[0];
        assert!(!handle.force_keyframe.load(Ordering::SeqCst));

        // Fire keyframe request → flag goes true.
        let fired = pool.request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()));
        assert!(fired, "request_keyframe must return true when encoder matches");
        assert!(handle.force_keyframe.load(Ordering::SeqCst));

        // Second request is coalesced (returns false) — flag stays
        // set because we haven't encoded yet (the encoder thread would
        // swap it back).
        let fired2 = pool.request_keyframe(CodecKind::Vp8, Some(SimulcastRid::full()));
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
        let pool = EncoderPool::new(64, 64, 30, vec![]); // no always-on
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Nothing on-demand yet.
        assert_eq!(
            pool.on_demand_refcount(CodecKind::Vp8, SimulcastRid::full()),
            None
        );

        let (subs, _lease) = pool.subscribe(&prefs).expect("subscribe must succeed");
        assert_eq!(subs.len(), 1, "on-demand spawn must return one subscription");
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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
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
        let pool = Arc::new(EncoderPool::new(64, 64, 30, vec![]));
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
        let pool = EncoderPool::new(64, 64, 30, vec![]);
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
        let layer = LayerSpec::single(CodecKind::Vp8, 64, 64, 30);
        let pool = EncoderPool::new(64, 64, 30, vec![layer]);

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
        assert!(codecs.contains(&CodecKind::Vp8), "VP8 always-on must be present");

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
        let h264_refcount =
            pool.on_demand_refcount(CodecKind::H264, SimulcastRid::full());
        assert_eq!(
            h264_in_subs,
            h264_refcount.is_some(),
            "H.264 subscription presence must agree with refcount presence"
        );
    }
}
