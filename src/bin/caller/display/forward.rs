//! Per-peer forwarder: translates encoder output into one peer's
//! WebRTC RTP stream, with per-peer codec / simulcast-layer selection.
//!
//! Implementation note: the codebase uses the `rtc` crate (rtc-rs,
//! version 0.9). API names below refer to rtc-rs unless explicitly
//! noted as external prior art.
//!
//! ## Why this exists
//!
//! The peer pool (see [`crate::display::encode::pool`]) produces
//! encoded frames per `(codec, rid)`. Each WebRTC peer needs those
//! frames rewritten into its own RTP track with:
//!
//! - **Its own negotiated payload type (PT)**. Different peers can
//!   land on different PTs for the same codec depending on each peer's
//!   offer SDP, so a frame produced by the encoder pool with one PT
//!   must be rewritten to whatever PT each subscriber peer negotiated
//!   for the same codec.
//! - **Its own SSRC + sequence numbers + RTP timestamps**, managed by
//!   rtc-rs per `RTCPeerConnection`.
//! - **Its own simulcast layer choice**, which may shift as
//!   bandwidth-estimate feedback (TWCC) changes. Today this is static
//!   (`Rid::full`); phase 4 wires TWCC events into layer selection.
//!
//! ## Pattern: SFU forwarding (modeled on str0m's chat.rs example)
//!
//! str0m's chat.rs SFU example is the canonical reference for this
//! pattern (str0m is a separate Rust WebRTC crate from rtc-rs but
//! the SFU shape is the same). Key elements, expressed in our
//! rtc-rs vocabulary:
//!
//! 1. **One `RTCPeerConnection` per peer.** rtc-rs's
//!    `RTCPeerConnection` negotiates a single codec set per instance,
//!    so each peer gets its own — there is no per-peer-codec
//!    selection inside one connection.
//! 2. **Receive encoded frames from the publisher, enqueue onto a
//!    shared channel.** Our publisher is the encoder pool producing
//!    `EncodedFrame` directly (not another `RTCPeerConnection`),
//!    but the fan-out channel is the same shape.
//! 3. **For each subscriber `RTCPeerConnection`, translate the
//!    encoder's PT to the peer-negotiated PT and write the codec-
//!    specific sample.** This is the core of the forwarder loop.
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
//! accepts feedback from the WebRTC bandwidth-estimate signal (TWCC,
//! phase 4). In this design stub, selection is static (`RID_FULL`
//! default).
//!
//! ## What this module is NOT doing yet
//!
//! - Spawning the forward loop task (phase 4).
//! - Wiring keyframe-request feedback (PLI / FIR) → pool (phase 4).
//! - Wiring bandwidth-estimate feedback → layer selector (phase 4).
//! - Per-peer RTP timestamp anchoring beyond what rtc-rs does
//!   internally (phase 4).
//!
//! This stub captures the types, the forwarder state machine, and the
//! per-peer contract the pool depends on. Phase 4 fills in the runtime.

use crate::display::encode::pool::{
    CodecKind, EncoderSubscription, PeerCodecPreferences, SimulcastRid,
};
use crate::display::EncodedFrame;
use crate::display::PeerId;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    /// PT translation between encoder PT and peer-negotiated PT
    /// returned `None` — the encoder's payload type doesn't have a
    /// peer-negotiated equivalent. Should be impossible if the pool
    /// subscription set matches the peer's negotiated codec set, so
    /// this represents a bug: fail loud.
    PayloadTypeTranslationFailed { codec: CodecKind, rid: SimulcastRid },
    /// Subscriber channel lagged past recovery. WebRTC's NACK + PLI
    /// feedback handles transient losses, so the forwarder recovers
    /// naturally; this variant exists for logging / metrics not for
    /// failure semantics.
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
                "PT translation returned None for {}:{} (pool/peer codec set mismatch)",
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
/// Layer changes come from WebRTC bandwidth-estimate feedback (TWCC,
/// phase 4). For now: static default `RID_FULL`, updates via
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
// to the peer's RTP track from a dedicated task. That design can't
// work: the `RTCPeerConnection` instance is owned by the `WebRtcPeer`
// driver task (`display/webrtc.rs`), and a separate forwarder task
// has no path to call into it. Moving the forwarder responsibilities
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
// placeholder `run` method that had no path to the peer's RTC
// instance.

// ---------------------------------------------------------------------------
// Helper: derive PeerCodecPreferences from an offer SDP
// ---------------------------------------------------------------------------

/// Build [`PeerCodecPreferences`] from a browser's offer SDP.
///
/// The returned preferences contain only codecs whose **exact**
/// payload spec the encoder pool can actually match via rtc-rs's
/// PT/profile matcher. This matters for H.264: an offer with an
/// rtpmap of `H264/90000` but fmtp of `profile-level-id=64001f`
/// (High) and `packetization-mode=0` would previously end up as
/// `CodecKind::H264` in prefs, get subscribed, and then every
/// encoded frame would fail PT matching because the pool's encoder
/// produces Constrained Baseline / mode 1 — a silent-black-screen
/// class of bug that the whole 3c.0 contract exists to prevent.
///
/// The guard is [`crate::display::encode::has_compatible_h264_offer`],
/// which checks for a Baseline-family (profile_idc 0x42) variant
/// with packetization-mode 0 or 1 — the intersection of what our
/// VideoToolbox / VAAPI / libx264 backends produce and what rtc-rs
/// will actually negotiate against the encoder's cached
/// [`crate::display::encode::PayloadSpec::h264_constrained_baseline`].
///
/// VP9 / AV1 don't need the guard today (no backend; pool excludes
/// them at `on_demand_spawnable`), but including them unconditionally
/// in prefs is harmless and matches the "prefs advertise what the
/// peer supports, pool decides what's serveable" split.
pub fn codec_preferences_from_offer(sdp: &str) -> PeerCodecPreferences {
    let offered = crate::display::encode::parse_offered_codecs(sdp);
    let mut supported = Vec::new();
    for name in offered {
        match name.as_str() {
            "VP8" => supported.push(CodecKind::Vp8),
            "H264" => {
                // Only include H.264 if the offer carries a variant
                // that rtc-rs's PT matcher would accept against our
                // encoder's exact PayloadSpec. The older
                // `has_compatible_h264_offer` is broader than that
                // (it accepts packetization-mode 0, missing fmtp,
                // and any profile_idc = 0x42 regardless of
                // constraint_set1_flag) — rtc-rs rejects all of
                // those, so they'd result in silent black-screen
                // frame-drop. See the detailed rules next to
                // `offer_has_poolable_h264_variant`.
                if crate::display::encode::offer_has_poolable_h264_variant(sdp) {
                    supported.push(CodecKind::H264);
                }
            }
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

/// Inject `a=rid:<rid> recv` lines + `a=simulcast:recv <rids>` into
/// the m=video section of an Offer SDP. This is the canonical impl
/// for the recv-simulcast hint that rtc 0.9 (the answer side)
/// requires before it'll emit `a=simulcast:send` in the answer.
///
/// **Mirror in JS**: `injectRecvSimulcastIntoVideoOffer` in
/// `static/app.html`. The two implementations MUST stay in sync —
/// the JS version is what real browsers run before
/// `setLocalDescription`, this Rust version exists for unit testing
/// the bug-fix corner cases (video-only SDP with trailing CRLF;
/// video followed by m=application; idempotent re-call). Drift here
/// vs JS means the two sides advertise different RID sets to the
/// answerer and simulcast doesn't wire up.
///
/// ## Insertion-point logic
///
/// Naive `insert at end` breaks for video-only SDPs: an SDP ending
/// with `\r\n` produces a trailing empty string when split on CRLF,
/// and inserting at `lines.len()` puts the rid lines AFTER the blank
/// SDP terminator. Some parsers treat that as garbage and drop the
/// whole post-terminator block. The fix:
///
///   - If a later `m=` section exists, insert immediately before it
///     (pushes that section down — the canonical multi-m-section case).
///   - If `m=video` is the last `m=` section (video-only SDP), back
///     up past any trailing empty strings and insert before them.
///     The rid lines land as the LAST attribute lines of the m=video
///     section; the SDP terminator(s) stay at the end.
///
/// ## Idempotency
///
/// Returns `sdp` unchanged if the m=video section already declares
/// `a=simulcast:`. The browser side can re-call without checking
/// (the reconnect / re-offer paths recreate the RTCPeerConnection
/// from a previous offer's SDP).
///
/// ## No-op cases
///
/// - `rids` empty → return sdp unchanged.
/// - No `m=video` section → return sdp unchanged.
/// - `m=video` already has `a=simulcast:` → return sdp unchanged.
pub fn inject_recv_simulcast_into_video_offer(sdp: &str, rids: &[&str]) -> String {
    if rids.is_empty() {
        return sdp.to_string();
    }
    let mut lines: Vec<String> = sdp.split("\r\n").map(|s| s.to_string()).collect();

    let mut video_start: Option<usize> = None;
    let mut next_section: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("m=video") {
            video_start = Some(i);
        } else if line.starts_with("m=") && video_start.is_some() {
            next_section = Some(i);
            break;
        }
    }

    let video_start = match video_start {
        Some(i) => i,
        None => return sdp.to_string(),
    };

    // Insertion point: before next m= section if found, otherwise
    // back up past trailing empty lines (the CRLF-terminator-induced
    // blank from `split("\r\n")`) so we don't insert AFTER the SDP
    // body terminator.
    let insert_at = match next_section {
        Some(i) => i,
        None => {
            let mut i = lines.len();
            while i > video_start + 1 && lines[i - 1].is_empty() {
                i -= 1;
            }
            i
        }
    };

    // Idempotent: skip if m=video section already declares simulcast.
    for line in &lines[video_start..insert_at] {
        if line.starts_with("a=simulcast:") {
            return sdp.to_string();
        }
    }

    let mut inject: Vec<String> = rids.iter().map(|rid| format!("a=rid:{rid} recv")).collect();
    inject.push(format!("a=simulcast:recv {}", rids.join(";")));

    let tail = lines.split_off(insert_at);
    lines.extend(inject);
    lines.extend(tail);
    lines.join("\r\n")
}

/// Map [`crate::display::IceServer`]s onto the shape an
/// `RTCPeerConnection` constructor expects in its `iceServers` array.
///
/// The transform is small but has one non-obvious rule: `username` and
/// `credential` are dropped when empty, not just when `None`. Some
/// gateway-config code paths produce `Some("")` for unset credentials
/// (TOML default for missing keys, env-var-source returning an empty
/// string, etc.); browsers don't treat empty-string credentials as
/// "no credential" — depending on the browser they either silently
/// fail auth, log a warning, or refuse to gather candidates from that
/// server at all. Filtering empty strings here matches the JS mirror's
/// `if (s.username)` truthy check exactly.
///
/// **Mirror in JS**: `buildIceServersFromGatewayConfig` in
/// `static/app.html`. Both display paths (local primary display, peer
/// federation display) MUST go through the JS mirror to construct
/// their `RTCPeerConnection({ iceServers: ... })` config — so the two
/// can't drift in what they advertise to the browser's ICE agent. This
/// Rust function exists for unit-test coverage of the corner cases the
/// JS version must also handle (empty username, empty credential,
/// multiple servers, default-empty config).
pub fn ice_servers_to_rtc_peer_connection_config(
    servers: &[crate::display::IceServer],
) -> Vec<crate::display::IceServer> {
    servers
        .iter()
        .map(|s| crate::display::IceServer {
            urls: s.urls.clone(),
            username: s.username.as_ref().filter(|u| !u.is_empty()).cloned(),
            credential: s.credential.as_ref().filter(|c| !c.is_empty()).cloned(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::encode::pool::{EncoderId, EncoderSubscription, LayerSpec, SimulcastRid};
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
        // Skeleton SDP carrying the codec rtpmap lines that
        // `parse_offered_codecs` looks for. The H.264 line carries
        // a Constrained Baseline + packetization-mode 1 fmtp so the
        // strict `offer_has_poolable_h264_variant` gate admits it —
        // this test is about "do we see all three codec families,"
        // separate from the H.264-profile-specific tests below that
        // cover the edge cases of the strict gate.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=1\r\n",
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

    /// Finding 1 in the 3c.0a review: an offer that advertises H.264
    /// with an *incompatible* profile (e.g., High `64001f` + mode 2)
    /// must NOT produce `CodecKind::H264` in prefs, because the pool
    /// only produces Constrained Baseline / mode 1 and rtc-rs would
    /// PT-miss every frame. The legacy path's
    /// `is_compatible_h264_profile` does the exact check.
    #[test]
    fn codec_preferences_excludes_incompatible_h264_profile() {
        // H.264 High (profile_idc=0x64 = 100), packetization-mode=2 (well
        // beyond our encoder's max). VP8 on a separate PT so the peer
        // still has a compatible codec.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=64001f;packetization-mode=2\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(prefs.supports(CodecKind::Vp8), "VP8 must remain supported");
        assert!(
            !prefs.supports(CodecKind::H264),
            "H.264 High/mode 2 must NOT be claimed — encoder produces \
             Constrained Baseline/mode 1 only, rtc-rs would drop every frame"
        );
    }

    /// `codec_preferences_from_offer` must preserve the offer's PT
    /// order — the order is what the encoder pool uses to pick the
    /// active codec. The local DisplaySlot path relies on this:
    /// browser-side `setCodecPreferences` puts VP8 PT first in the
    /// offer so the simulcast path lands on VP8 (multi-RID-capable)
    /// rather than H.264 (single-encoding only). If ordering ever
    /// got sorted lexicographically or by enum discriminant the
    /// DisplaySlot simulcast negotiation would silently regress to
    /// H.264 + malformed `a=simulcast:send`.
    #[test]
    fn codec_preferences_preserves_offer_order_vp8_first_over_h264() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 107 96\r\n",
            "a=rtpmap:107 VP8/90000\r\n",
            "a=rtpmap:96 H264/90000\r\n",
            "a=fmtp:96 profile-level-id=42e01f;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert_eq!(
            prefs.supported,
            vec![CodecKind::Vp8, CodecKind::H264],
            "offer with VP8 PT first must yield Vp8 first in preferences"
        );
    }

    /// Mirror of the above with PT order reversed — confirms order
    /// derives from the offer, not from a fixed codec preference table.
    #[test]
    fn codec_preferences_preserves_offer_order_h264_first_over_vp8() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 107\r\n",
            "a=rtpmap:96 H264/90000\r\n",
            "a=fmtp:96 profile-level-id=42e01f;packetization-mode=1\r\n",
            "a=rtpmap:107 VP8/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert_eq!(
            prefs.supported,
            vec![CodecKind::H264, CodecKind::Vp8],
            "offer with H.264 PT first must yield H264 first in preferences"
        );
    }

    /// Complement of the above: Baseline + mode 1 is what our encoder
    /// produces, so it must be included.
    #[test]
    fn codec_preferences_includes_compatible_h264_baseline() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(prefs.supports(CodecKind::H264));
    }

    /// An offer with multiple H.264 variants — one compatible, one not —
    /// should still claim H.264 support. rtc-rs picks the compatible
    /// variant for negotiation; the incompatible one is ignored.
    #[test]
    fn codec_preferences_h264_mixed_variants_keeps_codec() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97 98\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=64001f;packetization-mode=0\r\n", // High, incompatible
            "a=rtpmap:98 H264/90000\r\n",
            "a=fmtp:98 profile-level-id=42e01f;packetization-mode=1\r\n", // Baseline, compatible
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            prefs.supports(CodecKind::H264),
            "H.264 must be claimed when at least one offered variant is compatible"
        );
    }

    // -----------------------------------------------------------------------
    // Strict H.264 filter tests (findings #1 revisited in 3c.0a review)
    //
    // Each of these variants was previously accepted by the legacy
    // `has_compatible_h264_offer` helper but would be rejected by
    // rtc-rs's PT/profile matcher against the encoder's exact
    // `PayloadSpec::h264_constrained_baseline`. Result: silent
    // black-screen frame drops. The new `offer_has_poolable_h264_variant`
    // gate must exclude each one.
    // -----------------------------------------------------------------------

    /// `42e01f` profile (Constrained Baseline, the match) but
    /// packetization-mode 0 (encoder produces mode 1). rtc-rs's
    /// matcher requires equality on packetization-mode.
    #[test]
    fn codec_preferences_excludes_h264_wrong_packetization_mode() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=0\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "packetization-mode 0 must be rejected — encoder emits mode 1 only"
        );
    }

    /// `42001f` profile (Baseline, NOT Constrained Baseline) at
    /// packetization-mode 1. rtc-rs's profile resolver maps this to
    /// `H264Profile::Baseline` while our encoder emits
    /// `ConstrainedBaseline`; the matcher requires profile equality.
    #[test]
    fn codec_preferences_excludes_h264_pure_baseline_without_cs1() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42001f;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "Pure Baseline (cs1 unset) must be rejected — encoder emits Constrained Baseline"
        );
    }

    /// H.264 rtpmap with NO fmtp line at all. `parse_h264_fmtp` treats
    /// missing fmtp as packetization-mode 0 + empty profile-level-id,
    /// and rtc-rs's PT matcher falls back to Baseline/Level 1 for
    /// missing profile-level-id. Both axes disagree with our encoder.
    #[test]
    fn codec_preferences_excludes_h264_with_no_fmtp() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "No fmtp implies rtc-rs-fallback Baseline + mode 0 — must be rejected"
        );
    }

    /// Correct profile + mode but offered level 5.0 (`4d0032` —
    /// actually Main at Level 5.0; use `42e028` for ConstrainedBaseline
    /// at Level 4.0 to keep profile family). Level 4.0 (0x28) > our
    /// encoder's Level 3.1 (0x1f); rtc-rs rejects when the offer's
    /// level exceeds ours.
    #[test]
    fn codec_preferences_excludes_h264_when_offered_level_exceeds_encoder() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e028;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "Level 4.0 offer exceeds encoder's Level 3.1 ceiling — must be rejected"
        );
    }

    /// Correct profile, correct mode, level LOWER than ours (3.0 vs 3.1).
    /// rtc-rs accepts `c1_level <= c0_level`, so this matches.
    #[test]
    fn codec_preferences_includes_h264_at_lower_level() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01e;packetization-mode=1\r\n", // Level 3.0
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            prefs.supports(CodecKind::H264),
            "Level 3.0 is below encoder's Level 3.1 — must be accepted (rtc-rs: c1_level <= c0_level)"
        );
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

    // -------------------------------------------------------------------
    // inject_recv_simulcast_into_video_offer — Phase 4c follow-up tests
    // -------------------------------------------------------------------

    /// Canonical multi-section case: video followed by m=application.
    /// Insertion happens immediately before m=application; the
    /// application section's attrs are preserved untouched and the
    /// rid+simulcast lines land inside the m=video section.
    #[test]
    fn inject_recv_simulcast_video_first_application_after() {
        let sdp = "v=0\r\n\
                   o=- 1 2 IN IP4 0.0.0.0\r\n\
                   s=-\r\n\
                   t=0 0\r\n\
                   a=group:BUNDLE 0 1\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   c=IN IP4 0.0.0.0\r\n\
                   a=mid:0\r\n\
                   a=recvonly\r\n\
                   a=rtpmap:96 VP8/90000\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   c=IN IP4 0.0.0.0\r\n\
                   a=mid:1\r\n\
                   a=sctp-port:5000\r\n";

        let out = inject_recv_simulcast_into_video_offer(sdp, &["f", "h", "q"]);

        let video_idx = out.find("m=video").expect("m=video preserved");
        let app_idx = out.find("m=application").expect("m=application preserved");
        let simulcast_idx = out
            .find("a=simulcast:recv f;h;q")
            .expect("a=simulcast:recv injected");
        let rid_f_idx = out.find("a=rid:f recv").expect("a=rid:f recv injected");

        assert!(video_idx < rid_f_idx, "rid:f after m=video");
        assert!(rid_f_idx < simulcast_idx, "rid lines before simulcast line");
        assert!(
            simulcast_idx < app_idx,
            "simulcast lines must land inside m=video, BEFORE m=application; \
             got:\n{out}"
        );

        // m=application section integrity: its own attrs come right
        // after its m= line, no garbage interleaved.
        let app_section = &out[app_idx..];
        assert!(app_section.contains("a=mid:1"));
        assert!(app_section.contains("a=sctp-port:5000"));
    }

    /// **The bug-fix scenario from review.** Video as final m= section
    /// with the SDP terminating in CRLF: `split("\r\n")` produces a
    /// trailing empty string. Naive `splice(lines.len(), ...)` would
    /// insert AFTER the blank SDP body terminator, putting the rid
    /// lines outside the m=video section where parsers may drop them.
    ///
    /// The fix: when no later m= section exists, back up past
    /// trailing empties before splicing. This test pins that no
    /// blank line appears strictly between m=video and a=simulcast
    /// in the output.
    #[test]
    fn inject_recv_simulcast_video_only_with_trailing_crlf() {
        let sdp = "v=0\r\n\
                   o=- 1 2 IN IP4 0.0.0.0\r\n\
                   s=-\r\n\
                   t=0 0\r\n\
                   a=group:BUNDLE 0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   c=IN IP4 0.0.0.0\r\n\
                   a=mid:0\r\n\
                   a=recvonly\r\n\
                   a=rtpmap:96 VP8/90000\r\n";

        let out = inject_recv_simulcast_into_video_offer(sdp, &["f", "h", "q"]);

        let lines: Vec<&str> = out.split("\r\n").collect();
        let video_idx = lines
            .iter()
            .position(|l| l.starts_with("m=video"))
            .expect("m=video preserved");
        let simulcast_idx = lines
            .iter()
            .position(|l| l.starts_with("a=simulcast:"))
            .expect("a=simulcast: injected");

        assert!(simulcast_idx > video_idx, "simulcast comes after m=video");
        for (i, line) in lines
            .iter()
            .enumerate()
            .take(simulcast_idx)
            .skip(video_idx + 1)
        {
            assert!(
                !line.is_empty(),
                "no blank line allowed between m=video and a=simulcast \
                 (would put simulcast outside the video section); found \
                 empty line at index {i}\nfull output:\n{out}"
            );
        }

        assert!(
            out.ends_with("\r\n"),
            "trailing CRLF terminator preserved; got tail bytes: {:?}",
            out.as_bytes().get(out.len().saturating_sub(4)..)
        );
    }

    /// Video as final m= section without the trailing CRLF (split
    /// produces no trailing empty string). Insertion happens at
    /// `lines.len()` directly — the back-up-past-empties branch
    /// short-circuits when there ARE no trailing empties.
    #[test]
    fn inject_recv_simulcast_video_only_no_trailing_crlf() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:0";

        let out = inject_recv_simulcast_into_video_offer(sdp, &["f"]);

        let lines: Vec<&str> = out.split("\r\n").collect();
        let mid_idx = lines
            .iter()
            .position(|l| l.starts_with("a=mid"))
            .expect("a=mid preserved");
        let rid_idx = lines
            .iter()
            .position(|l| l.starts_with("a=rid:f"))
            .expect("a=rid:f injected");
        let simulcast_idx = lines
            .iter()
            .position(|l| l.starts_with("a=simulcast:"))
            .expect("a=simulcast: injected");

        assert!(
            mid_idx < rid_idx,
            "rid line after pre-existing m=video attrs"
        );
        assert!(rid_idx < simulcast_idx);
    }

    /// Idempotent: an SDP that already declares `a=simulcast:` in its
    /// m=video section is returned unchanged (reconnect / re-offer
    /// safety — the browser-side caller doesn't have to check before
    /// re-applying).
    #[test]
    fn inject_recv_simulcast_idempotent_on_already_present() {
        let sdp = "v=0\r\n\
                   o=- 1 2 IN IP4 0.0.0.0\r\n\
                   s=-\r\n\
                   t=0 0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=mid:0\r\n\
                   a=rid:f recv\r\n\
                   a=simulcast:recv f;h;q\r\n";

        let out = inject_recv_simulcast_into_video_offer(sdp, &["f", "h", "q"]);
        assert_eq!(out, sdp, "no-op on SDP that already declares a=simulcast");
    }

    /// Empty `rids` slice → no-op. Caller bug if this is reached in
    /// production; the helper just returns the input unchanged.
    #[test]
    fn inject_recv_simulcast_empty_rids_is_noop() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:0\r\n";
        assert_eq!(inject_recv_simulcast_into_video_offer(sdp, &[]), sdp);
    }

    /// No m=video section → no-op. Audio-only offers don't apply for
    /// our display flow but the helper is defensively safe.
    #[test]
    fn inject_recv_simulcast_no_video_section_is_noop() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";
        assert_eq!(
            inject_recv_simulcast_into_video_offer(sdp, &["f", "h", "q"]),
            sdp
        );
    }

    /// SDP ending with multiple trailing CRLFs (`\r\n\r\n`) — split
    /// produces multiple trailing empties. The back-up loop must
    /// step past all of them so the simulcast line still lands inside
    /// the m=video section, not after the blank lines.
    #[test]
    fn inject_recv_simulcast_video_only_with_multiple_trailing_crlfs() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:0\r\n\r\n\r\n";

        let out = inject_recv_simulcast_into_video_offer(sdp, &["f"]);

        let lines: Vec<&str> = out.split("\r\n").collect();
        let video_idx = lines.iter().position(|l| l.starts_with("m=video")).unwrap();
        let simulcast_idx = lines
            .iter()
            .position(|l| l.starts_with("a=simulcast:"))
            .unwrap();
        for (i, line) in lines
            .iter()
            .enumerate()
            .take(simulcast_idx)
            .skip(video_idx + 1)
        {
            assert!(
                !line.is_empty(),
                "no empty line allowed between m=video and a=simulcast; \
                 found empty at index {i}\nfull output:\n{out}"
            );
        }
    }

    // -----------------------------------------------------------------
    // ice_servers_to_rtc_peer_connection_config — JS-mirror coverage
    // -----------------------------------------------------------------

    use crate::display::IceServer;

    fn srv(urls: &[&str], username: Option<&str>, credential: Option<&str>) -> IceServer {
        IceServer {
            urls: urls.iter().map(|s| (*s).to_string()).collect(),
            username: username.map(|s| s.to_string()),
            credential: credential.map(|s| s.to_string()),
        }
    }

    /// Empty input list → empty output. The JS mirror ternary
    /// `(config && config.ice_servers) ? .map(...) : []` yields the
    /// same. This is the trust-the-network-default that ships in
    /// trusted-LAN deployments where no STUN/TURN is needed.
    #[test]
    fn ice_servers_empty_input_yields_empty_output() {
        let out = ice_servers_to_rtc_peer_connection_config(&[]);
        assert!(out.is_empty());
    }

    /// Single STUN-only entry — only `urls` field carries through.
    /// Username/credential are `None`, must NOT be serialized as
    /// `"username": null` (RTCIceServer rejects non-string types).
    /// `IceServer`'s `#[serde(skip_serializing_if = "Option::is_none")]`
    /// covers the wire side of this; the test confirms the field is
    /// `None` so the skip kicks in.
    #[test]
    fn ice_servers_stun_only_drops_credential_fields() {
        let input = vec![srv(&["stun:stun.example:3478"], None, None)];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].urls, vec!["stun:stun.example:3478".to_string()]);
        assert!(out[0].username.is_none());
        assert!(out[0].credential.is_none());
    }

    /// TURN with credentials passes both through verbatim.
    #[test]
    fn ice_servers_turn_with_credentials_passes_through() {
        let input = vec![srv(
            &["turn:turn.example:3478?transport=tcp"],
            Some("user1"),
            Some("pass1"),
        )];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].username.as_deref(), Some("user1"));
        assert_eq!(out[0].credential.as_deref(), Some("pass1"));
    }

    /// Empty-string username gets filtered to None — matches the JS
    /// mirror's `if (s.username)` truthy check. Without this, browsers
    /// silently fail TURN auth or refuse to gather candidates from a
    /// server with empty credentials. The test name calls out "matches
    /// JS truthy check" so future readers know this isn't a stylistic
    /// choice — it's a correctness invariant tying the two sides.
    #[test]
    fn ice_servers_empty_string_username_filtered_to_none_matches_js_truthy() {
        let input = vec![srv(
            &["turn:turn.example:3478"],
            Some(""),
            Some("validpass"),
        )];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        assert!(
            out[0].username.is_none(),
            "empty-string username must be filtered to None"
        );
        assert_eq!(out[0].credential.as_deref(), Some("validpass"));
    }

    /// Empty-string credential filtered to None — symmetric to the
    /// username case.
    #[test]
    fn ice_servers_empty_string_credential_filtered_to_none_matches_js_truthy() {
        let input = vec![srv(&["turn:turn.example:3478"], Some("user1"), Some(""))];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        assert_eq!(out[0].username.as_deref(), Some("user1"));
        assert!(
            out[0].credential.is_none(),
            "empty-string credential must be filtered to None"
        );
    }

    /// Multiple URLs in one entry preserved as the same array — the
    /// RTCIceServer `urls` field accepts both `string` and `string[]`,
    /// and we always pass the array form for consistency.
    #[test]
    fn ice_servers_multiple_urls_in_one_entry_preserved() {
        let input = vec![srv(
            &["stun:stun.example:3478", "stun:stun-backup.example:3478"],
            None,
            None,
        )];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        assert_eq!(out[0].urls.len(), 2);
        assert_eq!(out[0].urls[0], "stun:stun.example:3478");
        assert_eq!(out[0].urls[1], "stun:stun-backup.example:3478");
    }

    /// Multiple servers in the input — each maps independently. Common
    /// real-world shape: a STUN-only entry plus a TURN-with-creds
    /// entry as a fallback for restrictive NATs.
    #[test]
    fn ice_servers_multiple_servers_each_map_independently() {
        let input = vec![
            srv(&["stun:stun.example:3478"], None, None),
            srv(
                &["turn:turn.example:3478?transport=tcp"],
                Some("user2"),
                Some("pass2"),
            ),
        ];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        assert_eq!(out.len(), 2);
        assert!(out[0].username.is_none());
        assert_eq!(out[1].username.as_deref(), Some("user2"));
    }

    /// Verifies the output serializes to the JSON shape an
    /// `RTCPeerConnection` constructor accepts: `urls` is a JSON
    /// array, `username`/`credential` are JSON strings when present,
    /// the keys are absent (not `null`) when filtered out. This is
    /// the wire-level contract with the browser's WebRTC layer.
    #[test]
    fn ice_servers_serialize_to_browser_compatible_json_shape() {
        let input = vec![
            srv(&["stun:stun.example:3478"], None, None),
            srv(&["turn:turn.example:3478"], Some("user"), Some("pass")),
        ];
        let out = ice_servers_to_rtc_peer_connection_config(&input);
        let json = serde_json::to_value(&out).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // STUN entry: only `urls` key.
        let stun = arr[0].as_object().unwrap();
        assert!(stun.contains_key("urls"));
        assert!(!stun.contains_key("username"));
        assert!(!stun.contains_key("credential"));
        assert!(stun["urls"].is_array());

        // TURN entry: all three keys, all strings.
        let turn = arr[1].as_object().unwrap();
        assert_eq!(turn["username"].as_str(), Some("user"));
        assert_eq!(turn["credential"].as_str(), Some("pass"));
        assert!(turn["urls"].is_array());
    }
}
