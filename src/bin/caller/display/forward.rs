//! Per-peer forwarder: translates encoder output into one peer's
//! WebRTC RTP stream, with per-peer codec / simulcast-layer selection.
//!
//! ## Why this exists
//!
//! The peer pool (see [`crate::display::encode::pool`]) produces
//! encoded frames per `(codec, rid)`. Each WebRTC peer needs those
//! frames rewritten into its own RTP track with:
//!
//! - **Its own negotiated payload type (PT)**. Different peers can land
//!   on different PTs for the same codec depending on each peer's
//!   offer SDP — str0m acknowledges this explicitly in the
//!   [`Writer::match_params`] documentation: *"a certain codec
//!   configuration might not have the same payload type (PT) for two
//!   different peers."*
//! - **Its own SSRC + sequence numbers + RTP timestamps**, managed by
//!   str0m internally per [`Rtc`] instance.
//! - **Its own simulcast layer choice**, which may shift as TWCC
//!   bandwidth estimates change. Today this is static (`Rid::full`);
//!   phase 4 wires TWCC events into layer selection.
//!
//! ## Pattern: str0m's chat.rs SFU example
//!
//! The str0m crate ships an SFU example that's structurally identical
//! to what we need. Key elements:
//!
//! 1. **One [`Rtc`] per peer.** str0m does not support per-peer codec
//!    selection inside a single `Rtc`, so each peer gets its own.
//!    (str0m example: `Rtc::builder().build(Instant::now())`.)
//! 2. **Receive `Event::MediaData` from the publisher `Rtc`, enqueue
//!    onto a shared channel.** Our publisher is not an `Rtc` — it's
//!    the encoder pool producing `EncodedFrame` directly — but the
//!    channel abstraction is the same.
//! 3. **For each subscriber `Rtc`, translate PT via
//!    [`Writer::match_params`] and call [`Writer::write`] with the
//!    codec-specific sample.** This is the core of the forwarder loop.
//! 4. **For simulcast sources, the subscriber filters by RID.** The
//!    str0m example hard-codes to one RID; we pick per-peer based on
//!    TWCC bandwidth.
//!
//! ## Keyframe-first guard
//!
//! A peer that joins mid-stream MUST receive a keyframe before any
//! P-frame or the decoder produces garbage (browser shows black or
//! corruption until the next natural keyframe, often 2-5 seconds
//! later on static content).
//!
//! Every forwarder starts with `keyframe_seen: false`. Until set, it
//! drops non-keyframe frames and requests a keyframe from the pool
//! via [`crate::display::encode::pool::EncoderPool::request_keyframe`].
//! The pool's keyframe coalescer ensures N late-joiners produce one
//! keyframe, not N.
//!
//! Once the forwarder sees its first keyframe, it sets the flag and
//! forwards all subsequent frames (keyframe and P).
//!
//! ## Layer selection
//!
//! Simulcast lets one peer pick the layer it can sustain over its
//! link:
//!
//! - Full-resolution peer on LAN: RID `f`.
//! - Browser behind a 2 Mbps shared WiFi: RID `h` (or `q` under load).
//! - Browser on a mobile hotspot: RID `q`.
//!
//! The per-peer [`LayerSelector`] holds the currently-active RID and
//! accepts feedback from str0m's `Event::EgressBitrateEstimate` (phase
//! 4). In this design stub, selection is static (`RID_FULL` default).
//!
//! ## What this module is NOT doing yet
//!
//! - Spawning the forward loop task (phase 4).
//! - Wiring str0m's `Event::KeyframeRequest` → pool (phase 4).
//! - Wiring str0m's `Event::EgressBitrateEstimate` → layer selector
//!   (phase 4).
//! - Per-peer RTP timestamp anchoring beyond what str0m does internally
//!   (phase 4).
//!
//! This stub captures the types, the forwarder state machine, and the
//! per-peer contract the pool depends on. Phase 4 fills in the runtime.
//!
//! [`Rtc`]: https://docs.rs/str0m
//! [`Writer::match_params`]: https://docs.rs/str0m/latest/str0m/media/struct.Writer.html
//! [`Writer::write`]: https://docs.rs/str0m/latest/str0m/media/struct.Writer.html

use crate::display::encode::pool::{
    CodecKind, EncoderSubscription, PeerCodecPreferences, SimulcastRid,
};
use crate::display::EncodedFrame;
use crate::display::PeerId;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{broadcast, RwLock};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Forwarder-layer errors. These are in-process errors; wire-layer
/// WebRTC errors stay in [`crate::display::webrtc`].
#[derive(Debug)]
pub enum ForwarderError {
    /// Peer advertised codecs but the pool returned no subscriptions —
    /// the peer's codec set doesn't overlap with any encoder the pool
    /// is producing (or willing to spawn). Surfaces to the WebRTC
    /// handler as "offer rejected: no compatible codec."
    NoCompatibleCodec,
    /// str0m's [`Writer::match_params`] returned `None` — encoder PT
    /// doesn't have a peer-negotiated equivalent. Should be impossible
    /// if the pool subscription set matches the peer's negotiated
    /// codec set, so this represents a bug: fail loud.
    PayloadTypeTranslationFailed {
        codec: CodecKind,
        rid: SimulcastRid,
    },
    /// Subscriber channel lagged past recovery. str0m handles SFU-side
    /// losses with NACK + PLI, so the forwarder recovers naturally;
    /// this variant exists for logging / metrics not for failure
    /// semantics.
    SubscriptionLagged {
        codec: CodecKind,
        rid: SimulcastRid,
        skipped: u64,
    },
}

impl fmt::Display for ForwarderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCompatibleCodec => {
                write!(f, "peer's codec set does not overlap with pool output")
            }
            Self::PayloadTypeTranslationFailed { codec, rid } => write!(
                f,
                "str0m match_params returned None for {}:{} (pool/peer codec set mismatch)",
                codec, rid
            ),
            Self::SubscriptionLagged {
                codec,
                rid,
                skipped,
            } => write!(
                f,
                "forwarder skipped {} frames on {}:{} (slow subscriber, self-healing via NACK/PLI)",
                skipped, codec, rid
            ),
        }
    }
}

impl std::error::Error for ForwarderError {}

// ---------------------------------------------------------------------------
// Layer selector
// ---------------------------------------------------------------------------

/// Per-peer simulcast layer selection. Holds the currently-active RID;
/// the forwarder reads it on each frame to decide whether to forward
/// or drop.
///
/// Layer changes come from str0m's `Event::EgressBitrateEstimate` in
/// phase 4. For now: static default `RID_FULL`, updates via
/// [`LayerSelector::prefer`] which is called from the forwarder's
/// keyframe-request path (a new layer needs a fresh keyframe to be
/// decodable, so layer-switch and keyframe-request are paired).
pub struct LayerSelector {
    active: RwLock<SimulcastRid>,
}

impl LayerSelector {
    pub fn new() -> Self {
        Self {
            active: RwLock::new(SimulcastRid::full()),
        }
    }

    pub fn with_initial(rid: SimulcastRid) -> Self {
        Self {
            active: RwLock::new(rid),
        }
    }

    /// Currently-active RID. Cheap (read lock on a small value).
    pub async fn active(&self) -> SimulcastRid {
        self.active.read().await.clone()
    }

    /// Switch to a new layer. The caller should also request a
    /// keyframe — P-frames against the new layer's keyframe chain
    /// don't decode against the old layer's keyframe.
    pub async fn prefer(&self, rid: SimulcastRid) {
        *self.active.write().await = rid;
    }
}

impl Default for LayerSelector {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Why PerPeerForwarder lives inside the WebRtcPeer driver now
// ---------------------------------------------------------------------------
//
// An earlier design stub had a separate `PerPeerForwarder` type with a
// `run()` method that would loop over encoder subscriptions and write
// via `str0m::Writer::write_sample`. That design can't work: the
// `Rtc` instance is owned by the `WebRtcPeer` driver task
// (`display/webrtc.rs`), and a separate forwarder task has no path to
// call str0m's writer APIs. Moving the forwarder responsibilities
// into the driver avoids the cross-task-Rtc-access problem entirely:
// each peer's driver select!-loops over its pool subscriptions
// alongside its existing command/event arms, and the pt-caching +
// keyframe-gate logic from the stub merges into `DriverState`.
//
// What stays in this module:
//
// - [`LayerSelector`] — per-peer simulcast layer choice, wired to
//   TWCC bandwidth events in phase 4.
// - [`ForwarderError`] — shared vocabulary for the handful of
//   forwarder-layer failure modes (surfaced from the driver's write
//   path).
// - [`codec_preferences_from_offer`] — SDP-offer → `PeerCodecPreferences`
//   helper used by `handle_offer` before `pool.subscribe`.
//
// What was deleted: `PerPeerForwarder` struct, `PerPeerForwarderState`
// struct, the `should_forward` keyframe-gate helper (now lives in the
// driver's `keyframe_seen` check in `write_video_frame`), and the
// placeholder `run` method that couldn't call str0m.

// ---------------------------------------------------------------------------
// Helper: derive PeerCodecPreferences from an offer SDP
// ---------------------------------------------------------------------------

/// Build [`PeerCodecPreferences`] from a browser's offer SDP.
///
/// Uses the existing [`crate::display::encode::parse_offered_codecs`]
/// parser so we share vocabulary with the legacy codec-selection
/// path — there's one source of truth for "what did this SDP
/// advertise."
pub fn codec_preferences_from_offer(sdp: &str) -> PeerCodecPreferences {
    let offered = crate::display::encode::parse_offered_codecs(sdp);
    let mut supported = Vec::new();
    for name in offered {
        match name.as_str() {
            "VP8" => supported.push(CodecKind::Vp8),
            "H264" => supported.push(CodecKind::H264),
            "VP9" => supported.push(CodecKind::Vp9),
            "AV1" => supported.push(CodecKind::Av1),
            _ => {
                // Non-video or RTX / RED / ULPFEC — ignored; these
                // aren't codecs the encoder produces. The existing
                // parser returns these too.
            }
        }
    }
    PeerCodecPreferences::new(supported)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::encode::pool::{
        EncoderId, EncoderSubscription, LayerSpec, SimulcastRid,
    };
    use tokio::sync::broadcast;

    fn make_subscription(codec: CodecKind, rid: SimulcastRid) -> EncoderSubscription {
        let (_tx, rx) = broadcast::channel(16);
        EncoderSubscription {
            id: EncoderId::new(codec, rid.clone()),
            layer: LayerSpec::single(codec, 640, 480, 30),
            frames: rx,
        }
    }

    fn peer_id(n: u64) -> PeerId {
        n
    }

    // PerPeerForwarder tests were deleted with the type — the
    // keyframe-gate regression guard moves to the driver's write path
    // (`display/webrtc.rs::write_video_frame`), where `state.keyframe_seen`
    // now lives. Driver-side coverage is via e2e webrtc tests rather
    // than in-unit tests because the relevant path requires a live
    // `Rtc` instance.

    #[tokio::test]
    async fn layer_selector_starts_at_full() {
        let sel = LayerSelector::new();
        assert_eq!(sel.active().await, SimulcastRid::full());
    }

    #[tokio::test]
    async fn layer_selector_switches_on_prefer() {
        let sel = LayerSelector::new();
        sel.prefer(SimulcastRid::quarter()).await;
        assert_eq!(sel.active().await, SimulcastRid::quarter());
    }

    #[test]
    fn codec_preferences_from_offer_parses_known_codecs() {
        // Skeleton SDP carrying the codec rtpmap lines
        // parse_offered_codecs looks for.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=rtpmap:98 AV1/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(prefs.supports(CodecKind::Vp8));
        assert!(prefs.supports(CodecKind::H264));
        assert!(prefs.supports(CodecKind::Av1));
        assert!(!prefs.supports(CodecKind::Vp9));
    }

    #[test]
    fn codec_preferences_from_offer_ignores_non_codec_lines() {
        // RTX, ULPFEC, RED are not primary codecs; the pool doesn't
        // produce them directly and the forwarder shouldn't claim the
        // peer "supports" them as if they were decodable stand-alone.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 RTX/90000\r\n",
            "a=rtpmap:98 ulpfec/90000\r\n",
            "a=rtpmap:99 red/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert_eq!(prefs.supported, vec![CodecKind::Vp8]);
    }

    #[test]
    fn forwarder_error_display_includes_codec_id() {
        let e = ForwarderError::PayloadTypeTranslationFailed {
            codec: CodecKind::H264,
            rid: SimulcastRid::full(),
        };
        let s = format!("{}", e);
        assert!(s.contains("h264"));
        assert!(s.contains("f"));
    }
}
