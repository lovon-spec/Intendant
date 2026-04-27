//! `TwccTapInterceptor` — taps incoming RTCP at the rtc-interceptor
//! chain to surface `TransportLayerCC` feedback that rtc 0.9 otherwise
//! consumes internally without delivering to `RTCMessage::RtcpPacket`.
//!
//! ## Why this exists
//!
//! Task #51 spike 3 (commit `b842a82`, reverted `0e28db6`) confirmed
//! end-to-end that:
//!
//!   1. The browser sends `RTCP RTPFB FMT=15 (TransportLayerCC)`
//!      packets continuously over the wire (verified via tcpdump on
//!      the daemon's UDP port — byte 0 = `0x8F`, byte 1 = `0xCD`).
//!   2. rtc 0.9 receives those RTCP bytes at its UDP socket (it must
//!      — RTCP shares the transport with RTP).
//!   3. rtc 0.9's interceptor pipeline CONSUMES the RTCP and never
//!      surfaces it via `RTCMessage::RtcpPacket` to the app boundary.
//!   4. rtc 0.9's `RemoteInboundRtpStreamStats` accumulator stays at
//!      all-zero defaults regardless of which interceptors are wired.
//!
//! The fix the operator landed: drop in a custom interceptor IN OUR
//! CODE that taps inbound RTCP at the same layer rtc itself processes
//! it. The tap mutates nothing — it observes `Packet::Rtcp`, downcasts
//! each `Box<dyn rtcp::Packet>` to `TransportLayerCc`, projects a
//! compact event onto an `mpsc` channel for the capacity policy to
//! consume, then forwards the original packet unchanged so the rest of
//! the chain (rtcp_reports, twcc_sender_only, ...) sees identical
//! behavior.
//!
//! ## Why NOT use rtc's stats path
//!
//! Spikes 1 and 2 confirmed
//! `RTCRemoteInboundRtpStreamStats` always reports all-zero defaults,
//! regardless of whether the SR/RR + TWCC interceptors are wired.
//! Reading from rtc's stats accumulator returns no usable signal on
//! this rtc 0.9 / WKWebView combination. Direct parsing of incoming
//! `TransportLayerCc` packets is the operator-directed alternative.
//!
//! ## Chain placement
//!
//! `Registry::with(...)` puts the new interceptor OUTERMOST — it sees
//! inbound packets first on `handle_read`. This is what we want: the
//! tap should observe TLC before any later interceptor in the chain
//! has a chance to consume or alter them. Wire as the LAST `.with()`
//! after `configure_rtcp_reports` + `configure_twcc_sender_only`.
//!
//! ## What the tap deliberately doesn't do
//!
//! - **No mutation.** Every `handle_read` ends with
//!   `self.next.handle_read(msg)` passing the same `msg` it received.
//! - **No RR processing.** Spikes 1 and 2 already proved RR-derived
//!   stats are unhelpful on this stack. If RR turns out to be useful
//!   later (different rtc version, different browser), add a sibling
//!   `RrTapInterceptor` rather than overloading this one.
//! - **No adaptation decisions.** First-acceptance is "events emit
//!   at browser cadence and the stream changes shape under loss." The
//!   capacity policy reads `TwccEvent` and decides what to do; the
//!   tap is a sensor, not an actuator.

use rtc::interceptor::{Interceptor, Packet, StreamInfo, TaggedPacket, interceptor};
// `rtc` re-exports `sansio` and `shared` from the webrtc-rs workspace
// (rtc/lib.rs:656-659). The `#[interceptor]` attribute macro from
// rtc-interceptor-derive expands to code that references both via
// bare paths (`sansio::Protocol`, `shared::error::Error`), so we
// have to bring them into scope here at the call site for the macro
// expansion to typecheck.
use rtc::sansio;
use rtc::shared;
// `#[interceptor]` emits `type Error = Error;` (rtc-interceptor-derive
// 0.9 lib.rs:279) — that bare `Error` resolves at the call site, so we
// need the concrete type imported with the unqualified name `Error`.
use rtc::shared::error::Error;
use std::time::Instant;
use tokio::sync::mpsc;

/// Compact projection of one `TransportLayerCc` packet observed at
/// the rtc-interceptor chain's outermost `handle_read` boundary.
///
/// Field semantics map directly to RFC draft-holmer-rmcat-transport-
/// wide-cc-extensions-01 / `rtcp::TransportLayerCc`:
///
/// - [`Self::base_sequence_number`]: lowest TWCC sequence number this
///   feedback packet covers.
/// - [`Self::packet_status_count`]: total number of TWCC sequence
///   numbers reported (received + not-received together).
/// - [`Self::fb_pkt_count`]: monotonic feedback-packet counter from
///   the receiver — non-monotonic gaps signal a TLC was dropped on
///   the wire.
/// - [`Self::received`]: number of packets the receiver actually got
///   in the `[base, base + packet_status_count)` window. Computed
///   from `recv_deltas.len()` — TLC only emits a delta entry for
///   received packets; a not-received packet contributes a status
///   chunk symbol but no delta.
/// - [`Self::lost`]: `packet_status_count - received`.
///
/// Capacity-policy callers should sum across events within their
/// observation window. Per-event values are not directly comparable
/// across feedback rounds because TWCC sequence numbers are
/// transport-wide (cover all encodings) and `fb_pkt_count` cycles
/// independently per browser session.
#[derive(Debug, Clone, Copy)]
pub struct TwccEvent {
    /// Time the tap observed this packet (reads from the
    /// `TaggedPacket::now` field rtc populates from the incoming
    /// transport message). Useful for windowing.
    pub at: Instant,
    /// SSRC the receiver claimed for itself in the RTCP header.
    /// Browsers typically use a single sender-SSRC across all TLC
    /// packets in one session.
    pub sender_ssrc: u32,
    /// SSRC of the media this TLC reports on. For simulcast send
    /// the browser may issue per-layer TLCs (one media_ssrc per
    /// per-RID encoding) OR a single aggregate-ssrc TLC; spike 3
    /// observed the latter in WKWebView.
    pub media_ssrc: u32,
    pub base_sequence_number: u16,
    pub packet_status_count: u16,
    pub fb_pkt_count: u8,
    /// Number of packets the receiver got out of `packet_status_count`.
    pub received: u32,
    /// `packet_status_count - received`.
    pub lost: u32,
}

/// Interceptor that observes `TransportLayerCc` packets without
/// mutating them.
///
/// Construct via [`TwccTapInterceptor::new`] and add to a
/// `rtc::interceptor::Registry` chain via `.with(|inner|
/// TwccTapInterceptor::new(inner, tx))`. Place LAST in the chain so
/// it sees inbound RTCP outermost.
///
/// Cheap to drop: the `mpsc::UnboundedSender` is closed on Drop;
/// the consumer side observes `None` from `recv()` and shuts down
/// its window aggregator cleanly.
#[derive(rtc::interceptor::Interceptor)]
pub struct TwccTapInterceptor<P: Interceptor> {
    #[next]
    next: P,
    event_tx: mpsc::UnboundedSender<TwccEvent>,
}

#[interceptor]
impl<P: Interceptor> TwccTapInterceptor<P> {
    /// Override of `sansio::Protocol::handle_read`. Inspects the
    /// inbound message; if it's `Packet::Rtcp`, scans the packet
    /// vector for `TransportLayerCc` and emits one [`TwccEvent`]
    /// per TLC packet. Other RTCP types (RR / SR / PLI / FIR / NACK
    /// / unknown) are ignored at this layer — they're handled by
    /// later interceptors in the chain or by rtc's internal
    /// pipeline as they always have been.
    ///
    /// **Always** forwards `msg` to `self.next.handle_read(msg)`
    /// at the end. The tap is a passive sensor; behavioural
    /// changes from registering it must be zero.
    #[overrides]
    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message {
            for pkt in packets {
                if let Some(tlc) = pkt
                    .as_any()
                    .downcast_ref::<rtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc>()
                {
                    let received = tlc.recv_deltas.len() as u32;
                    let total = tlc.packet_status_count as u32;
                    let lost = total.saturating_sub(received);
                    // Send on an unbounded channel; if the receiver is
                    // gone we silently drop. The tap must never block
                    // the chain on a slow consumer.
                    let _ = self.event_tx.send(TwccEvent {
                        at: msg.now,
                        sender_ssrc: tlc.sender_ssrc,
                        media_ssrc: tlc.media_ssrc,
                        base_sequence_number: tlc.base_sequence_number,
                        packet_status_count: tlc.packet_status_count,
                        fb_pkt_count: tlc.fb_pkt_count,
                        received,
                        lost,
                    });
                }
            }
        }
        // Always forward unchanged. The tap mutates nothing.
        self.next.handle_read(msg)
    }
}

impl<P: Interceptor> TwccTapInterceptor<P> {
    /// Wrap an existing interceptor. The returned struct is itself
    /// an `Interceptor` (per the `#[derive(Interceptor)]` macro),
    /// suitable for further `.with(...)` composition.
    pub fn new(next: P, event_tx: mpsc::UnboundedSender<TwccEvent>) -> Self {
        Self { next, event_tx }
    }
}
