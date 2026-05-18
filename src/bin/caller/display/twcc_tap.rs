//! `TwccTapInterceptor` — observes browser-side TWCC feedback at
//! rtc 0.9's interceptor chain and publishes a 1-second health
//! aggregate that the capacity policy consumes.
//!
//! ## Why this is the signal source
//!
//! rtc 0.9's interceptor pipeline consumes inbound RTCP internally
//! and never surfaces it via [`rtc::peer_connection::RTCMessage::RtcpPacket`]
//! to the application boundary, **and** its
//! `RTCRemoteInboundRtpStreamStats` accumulator stays at all-zero
//! defaults regardless of which interceptors are wired. Both gaps
//! were verified end-to-end with packet captures of the
//! [`rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc`]
//! frames the browser sends continuously over the wire (RTCP PT=205,
//! FMT=15) — they reach the daemon's UDP socket, traverse rtc's
//! pipeline, and disappear without ever reaching the app or
//! advancing rtc's stats fields.
//!
//! Tapping inbound RTCP at the same interceptor layer rtc itself
//! processes it on is the only place we can observe TLC without
//! patching rtc 0.9. The tap mutates nothing — it observes
//! [`Packet::Rtcp`], downcasts each [`Box<dyn rtcp::Packet>`] to
//! `TransportLayerCc`, projects a compact [`TwccEvent`] onto an
//! `mpsc` channel, and forwards the original packet unchanged so
//! the rest of the chain (rtcp_reports, twcc_sender_only, …) sees
//! identical behaviour. RTCP types other than TLC pass through with
//! no observation overhead.
//!
//! ## Two-stage signal pipeline
//!
//! - [`TwccTapInterceptor`] runs inside rtc's interceptor chain and
//!   emits one [`TwccEvent`] per observed TLC packet onto an
//!   unbounded `mpsc` channel. Cheap (one downcast + struct copy
//!   per RTCP frame), non-blocking (unbounded channel; if the
//!   consumer is gone, the send drops silently), purely passive.
//! - [`spawn_twcc_health_aggregator`] consumes the event stream and
//!   publishes a [`TwccHealth`] snapshot every second on a
//!   `watch::Sender<Option<TwccHealth>>`. The watch receiver is the
//!   stable subscription surface the capacity policy reads — one
//!   emission per second, with `None` for both "no signal yet"
//!   AND "no TWCC events arrived this window." Silence is not
//!   recovery; an empty window publishes `None` so the policy's
//!   "no signal → preserve state" arm fires. The channel can
//!   transition `None → Some(_) → None → Some(_)` as feedback
//!   arrives and goes silent across windows.
//!
//! ## Chain placement
//!
//! Wire the tap LAST in `Registry::with(...)` ordering so it sees
//! inbound packets first on `handle_read`. `Registry::with` puts the
//! supplied wrapper outermost, so the call sequence
//!
//!   `Registry::new() →
//!    configure_rtcp_reports(.) →
//!    configure_twcc_sender_only(.) →
//!    .with(|inner| TwccTapInterceptor::new(inner, tx))`
//!
//! produces a chain whose outermost layer is the tap. The tap
//! observes, then forwards to twcc_sender_only, then rtcp_reports,
//! then rtc's internals — keeping the existing stack's behaviour
//! intact.
//!
//! ## What the tap does NOT do
//!
//! - **No mutation.** Every `handle_read` ends with
//!   `self.next.handle_read(msg)` passing the same `msg` it
//!   received. Behavioural changes from registering the tap are
//!   zero by construction.
//! - **No RR processing.** RR-derived stats are not actionable on
//!   this stack (rtc 0.9 + WKWebView). If a future version
//!   surfaces RR usefully, add a sibling `RrTapInterceptor`
//!   rather than overloading this one.
//! - **No adaptation decisions.** The tap is a sensor; the
//!   capacity policy in [`crate::display::aggregator`] decides
//!   what to do with the signal.
//!
//! ## Lifecycle
//!
//! The tap and the aggregator both shut down on the same edge:
//! when the parent `Rtc` is dropped (peer disconnect / display
//! teardown), the interceptor chain drops, the tap's
//! `mpsc::UnboundedSender` drops, the aggregator's `recv()`
//! returns `None`, and its task exits without external
//! cancellation. A `CancellationToken` is also accepted by
//! [`spawn_twcc_health_aggregator`] for symmetric shutdown with
//! the rest of the display task tree.

use rtc::interceptor::{interceptor, Interceptor, Packet, StreamInfo, TaggedPacket};
// `rtc` re-exports `sansio` from the webrtc-rs workspace
// (rtc/lib.rs:656-659). The `#[interceptor]` attribute macro from
// rtc-interceptor-derive expands to code that references it via
// the bare path `sansio::Protocol`, so we have to bring it into
// scope here at the call site for the macro expansion to typecheck.
use rtc::sansio;
// `#[interceptor]` emits `type Error = Error;` (rtc-interceptor-derive
// 0.9 lib.rs:279) — that bare `Error` resolves at the call site, so we
// need the concrete type imported with the unqualified name `Error`.
use rtc::shared::error::Error;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
/// - [`Self::lost`]: `packet_status_count - received` (saturating).
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
    /// per-RID encoding) OR a single aggregate-ssrc TLC; WKWebView
    /// emits the latter.
    pub media_ssrc: u32,
    pub base_sequence_number: u16,
    pub packet_status_count: u16,
    pub fb_pkt_count: u8,
    /// Number of packets the receiver got out of `packet_status_count`.
    pub received: u32,
    /// `packet_status_count - received`.
    pub lost: u32,
}

/// One-second aggregate of TWCC feedback that the capacity policy
/// consumes via [`watch::Receiver<Option<TwccHealth>>`].
///
/// Field semantics:
///
/// - [`Self::at`] — wall-clock end of the aggregation window. Used
///   by the policy as the "now" the loss reading is current at.
/// - [`Self::loss_fraction`] — `lost_packets / reported_packets`,
///   in the closed range `0.0..=1.0`. `0.0` when no packets were
///   reported in the window (empty / silent window) — distinguish
///   that case via [`Self::reported_packets`] or [`Self::batches`].
/// - [`Self::reported_packets`] — sum of `packet_status_count`
///   across every [`TwccEvent`] in the window.
/// - [`Self::received_packets`] — sum of `received`.
/// - [`Self::lost_packets`] — sum of `lost`.
/// - [`Self::last_fb_pkt_count`] — receiver-side TLC counter from
///   the last event in the window. Always `Some(_)` in a published
///   `TwccHealth` (the aggregator only constructs `TwccHealth` for
///   non-empty windows; empty windows publish `None` on the watch
///   channel instead). Counter wraps at 255 — gaps imply at least
///   one TLC was lost on the wire.
/// - [`Self::batches`] — number of `TwccEvent`s aggregated in this
///   window. Always `>= 1` in a published `TwccHealth`. The
///   defensive guard in
///   [`crate::display::aggregator::step_aggregate_layer_capacity`]
///   treats `batches == 0` as "no signal" anyway, so the invariant
///   isn't load-bearing — but in practice the aggregator never
///   emits a `TwccHealth` with `batches == 0`.
///
/// **Watch-channel contract**: `None` means either "no health
/// snapshot has been published yet" OR "the most recent window
/// had zero TWCC events." Empty windows publish `None`, never
/// `Some(empty_health)` — silence is not recovery, and the policy
/// must short-circuit to its "no signal → preserve state" arm in
/// both cases. The channel can therefore transition
/// `None → Some(_) → None → Some(_)` as the browser oscillates
/// between sending feedback and going silent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwccHealth {
    pub at: Instant,
    pub loss_fraction: f64,
    pub reported_packets: u64,
    pub received_packets: u64,
    pub lost_packets: u64,
    pub last_fb_pkt_count: Option<u8>,
    pub batches: u32,
}

/// Per-window accumulator state. Internal to
/// [`spawn_twcc_health_aggregator`]; lifted out as a struct so the
/// snapshot logic is unit-testable in isolation.
#[derive(Debug, Default, Clone, Copy)]
struct WindowAccumulator {
    batches: u32,
    reported: u64,
    received: u64,
    lost: u64,
    last_fb_pkt_count: Option<u8>,
}

impl WindowAccumulator {
    fn add(&mut self, event: &TwccEvent) {
        self.batches = self.batches.saturating_add(1);
        self.reported = self
            .reported
            .saturating_add(event.packet_status_count as u64);
        self.received = self.received.saturating_add(event.received as u64);
        self.lost = self.lost.saturating_add(event.lost as u64);
        self.last_fb_pkt_count = Some(event.fb_pkt_count);
    }

    fn snapshot(&self, at: Instant) -> TwccHealth {
        let loss_fraction = if self.reported > 0 {
            (self.lost as f64) / (self.reported as f64)
        } else {
            0.0
        };
        TwccHealth {
            at,
            loss_fraction,
            reported_packets: self.reported,
            received_packets: self.received,
            lost_packets: self.lost,
            last_fb_pkt_count: self.last_fb_pkt_count,
            batches: self.batches,
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Window cadence for [`TwccHealth`] snapshots.
///
/// Browsers send TLC at ~10-50 Hz (varies with bitrate / link
/// activity); 1 Hz aggregation gives the capacity policy a steady
/// signal without flooding. Matches the policy's debounce
/// granularity (5 s drop / 1 s restore from
/// [`crate::display::aggregator::CapacityPolicyConfig`]).
const HEALTH_WINDOW: Duration = Duration::from_secs(1);

/// Drain [`TwccEvent`]s from `event_rx` and publish a
/// [`TwccHealth`] snapshot every [`HEALTH_WINDOW`] on `health_tx`.
///
/// **Empty windows publish `None`, not `Some(empty_health)`.**
/// A window with zero TWCC events means "the browser sent us no
/// feedback during this second" — silence, not recovery. Emitting
/// `Some(TwccHealth { batches: 0, loss_fraction: 0.0, .. })`
/// would be read as "loss is zero, link is healthy" by the
/// capacity policy, which could incorrectly resume upper
/// simulcast layers during feedback silence. Publishing `None`
/// preserves the policy's "no signal → preserve state"
/// invariant: silence keeps whatever the last decision was, only
/// real loss readings move the policy.
///
/// One emission per window, no missed-window catch-up:
/// non-empty windows publish `Some(snapshot)`, empty windows
/// publish `None`. The watch channel can transition
/// `None → Some(_) → None → Some(_)` as feedback arrives and
/// goes silent across windows.
///
/// ## Shutdown
///
/// Two sources, either of which terminates the task:
///
/// - `shutdown.cancelled()` — explicit cancellation from the
///   display teardown path.
/// - `event_rx.recv()` returns `None` — the tap's
///   `mpsc::UnboundedSender` was dropped (the parent `Rtc` is
///   gone). Last partial window is NOT published; the watch
///   channel retains its previous value until subscribers drop.
///
/// `health_tx.send(...)` returning `Err` (no watch receivers
/// alive) is **not** a terminating source: the implementation
/// ignores the result so the aggregator keeps draining events to
/// prevent the tap's `mpsc` from filling. Subscribers can drop
/// and re-subscribe without disrupting the aggregator's lifecycle.
///
/// The returned `JoinHandle` resolves when one of the two
/// terminating sources fires.
pub fn spawn_twcc_health_aggregator(
    mut event_rx: mpsc::UnboundedReceiver<TwccEvent>,
    health_tx: watch::Sender<Option<TwccHealth>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut window_start = Instant::now();
        let mut accumulator = WindowAccumulator::default();
        loop {
            let window_end = window_start + HEALTH_WINDOW;
            let timeout = tokio::time::sleep_until(tokio::time::Instant::from_std(window_end));
            tokio::pin!(timeout);
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                maybe_event = event_rx.recv() => match maybe_event {
                    Some(event) => accumulator.add(&event),
                    None => return,
                },
                _ = &mut timeout => {
                    let payload = if accumulator.batches == 0 {
                        // Empty window: silence is not recovery.
                        // Publish None so the policy treats this
                        // window as "no signal" and preserves
                        // state.
                        None
                    } else {
                        let snapshot = accumulator.snapshot(window_end);
                        // Operational observability: one line per
                        // non-empty window. Rate-limited by
                        // construction (≤ 1 Hz). Skipping empty
                        // windows keeps idle sessions quiet.
                        eprintln!(
                            "[twcc-health] reported={r} received={recv} lost={l} \
                             loss_fraction={lf:.4} batches={b} last_fb={fb:?}",
                            r = snapshot.reported_packets,
                            recv = snapshot.received_packets,
                            l = snapshot.lost_packets,
                            lf = snapshot.loss_fraction,
                            b = snapshot.batches,
                            fb = snapshot.last_fb_pkt_count,
                        );
                        Some(snapshot)
                    };
                    let _ = health_tx.send(payload);
                    accumulator.reset();
                    window_start = window_end;
                }
            }
        }
    })
}

/// Interceptor that observes `TransportLayerCc` packets without
/// mutating them.
///
/// Construct via [`TwccTapInterceptor::new`] and add to a
/// [`rtc::interceptor::Registry`] chain via `.with(|inner|
/// TwccTapInterceptor::new(inner, tx))`. Place LAST in the chain
/// so it sees inbound RTCP outermost.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(at: Instant, total: u16, received: u32, fb: u8) -> TwccEvent {
        let lost = (total as u32).saturating_sub(received);
        TwccEvent {
            at,
            sender_ssrc: 0xdeadbeef,
            media_ssrc: 0xfeedface,
            base_sequence_number: 0,
            packet_status_count: total,
            fb_pkt_count: fb,
            received,
            lost,
        }
    }

    // ----- WindowAccumulator -----

    #[test]
    fn empty_window_snapshot_has_zero_fields_none_fb_count() {
        let acc = WindowAccumulator::default();
        let now = Instant::now();
        let snap = acc.snapshot(now);
        assert_eq!(snap.batches, 0);
        assert_eq!(snap.reported_packets, 0);
        assert_eq!(snap.received_packets, 0);
        assert_eq!(snap.lost_packets, 0);
        assert_eq!(snap.loss_fraction, 0.0);
        assert_eq!(snap.last_fb_pkt_count, None);
        assert_eq!(snap.at, now);
    }

    #[test]
    fn accumulator_sums_across_events() {
        let now = Instant::now();
        let mut acc = WindowAccumulator::default();
        acc.add(&evt(now, 100, 90, 1));
        acc.add(&evt(now, 50, 25, 2));
        acc.add(&evt(now, 200, 200, 3));
        let snap = acc.snapshot(now);
        assert_eq!(snap.batches, 3);
        assert_eq!(snap.reported_packets, 350);
        assert_eq!(snap.received_packets, 315);
        assert_eq!(snap.lost_packets, 35);
        // 35/350 = 0.10
        assert!((snap.loss_fraction - 0.10).abs() < 1e-9);
        assert_eq!(snap.last_fb_pkt_count, Some(3));
    }

    #[test]
    fn loss_fraction_zero_when_reported_zero() {
        // packet_status_count=0 is permitted by the wire format;
        // the saturating div should produce 0.0, not NaN.
        let now = Instant::now();
        let mut acc = WindowAccumulator::default();
        acc.add(&evt(now, 0, 0, 7));
        let snap = acc.snapshot(now);
        assert_eq!(snap.loss_fraction, 0.0);
        assert_eq!(snap.batches, 1);
        assert_eq!(snap.last_fb_pkt_count, Some(7));
    }

    #[test]
    fn last_fb_pkt_count_takes_most_recent_through_wraparound() {
        // Counter wraps 255 → 0 — the aggregator just records what
        // it sees; gap detection is the policy's job. The "last"
        // event sets the field regardless of whether it numerically
        // increased.
        let now = Instant::now();
        let mut acc = WindowAccumulator::default();
        acc.add(&evt(now, 10, 10, 254));
        acc.add(&evt(now, 10, 10, 255));
        acc.add(&evt(now, 10, 10, 0)); // wrap
        let snap = acc.snapshot(now);
        assert_eq!(snap.last_fb_pkt_count, Some(0));
        assert_eq!(snap.batches, 3);
    }

    #[test]
    fn reset_returns_accumulator_to_empty() {
        let now = Instant::now();
        let mut acc = WindowAccumulator::default();
        acc.add(&evt(now, 100, 50, 1));
        acc.reset();
        let snap = acc.snapshot(now);
        assert_eq!(snap.batches, 0);
        assert_eq!(snap.reported_packets, 0);
        assert_eq!(snap.lost_packets, 0);
        assert_eq!(snap.last_fb_pkt_count, None);
    }

    // ----- spawn_twcc_health_aggregator -----

    /// Bounded helper: poll the watch receiver until `Some(_)` lands
    /// or `deadline` elapses. The aggregator's first snapshot fires
    /// on the first window boundary, ~1 s after `tokio::time::pause()`
    /// is advanced; tests `tokio::time::advance` past the boundary
    /// before awaiting this.
    async fn next_snapshot(rx: &mut watch::Receiver<Option<TwccHealth>>) -> TwccHealth {
        // `changed()` resolves when the value transitions; then read
        // and clone the new state. Returns the latest snapshot.
        rx.changed().await.expect("aggregator dropped sender");
        rx.borrow_and_update()
            .clone()
            .expect("first snapshot is Some")
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_publishes_one_snapshot_per_window() {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<TwccEvent>();
        let (health_tx, mut health_rx) = watch::channel(None);
        let token = CancellationToken::new();
        let _handle = spawn_twcc_health_aggregator(event_rx, health_tx, token.clone());

        let t0 = Instant::now();
        event_tx.send(evt(t0, 100, 95, 10)).unwrap();
        event_tx.send(evt(t0, 100, 90, 11)).unwrap();

        // Cross the first window boundary.
        tokio::time::advance(HEALTH_WINDOW + Duration::from_millis(50)).await;
        let snap = next_snapshot(&mut health_rx).await;
        assert_eq!(snap.batches, 2);
        assert_eq!(snap.reported_packets, 200);
        assert_eq!(snap.received_packets, 185);
        assert_eq!(snap.lost_packets, 15);
        assert!((snap.loss_fraction - 0.075).abs() < 1e-9);
        assert_eq!(snap.last_fb_pkt_count, Some(11));

        // Window resets — next window with one event publishes that
        // event's counts only, not cumulative.
        event_tx.send(evt(t0, 50, 50, 12)).unwrap();
        tokio::time::advance(HEALTH_WINDOW + Duration::from_millis(50)).await;
        let snap = next_snapshot(&mut health_rx).await;
        assert_eq!(snap.batches, 1);
        assert_eq!(snap.reported_packets, 50);
        assert_eq!(snap.lost_packets, 0);
        assert_eq!(snap.loss_fraction, 0.0);
        assert_eq!(snap.last_fb_pkt_count, Some(12));

        token.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_publishes_none_for_empty_window() {
        // Silence is not recovery. A window with no TWCC events
        // must publish `None` on the watch channel — never a
        // `Some(TwccHealth { batches: 0, loss_fraction: 0.0, .. })`
        // that the capacity policy would misread as "link is
        // healthy." Without this, sustained TLC silence would
        // trigger spurious upper-layer resume.
        let (_event_tx, event_rx) = mpsc::unbounded_channel::<TwccEvent>();
        let (health_tx, mut health_rx) = watch::channel(None);
        let token = CancellationToken::new();
        let _handle = spawn_twcc_health_aggregator(event_rx, health_tx, token.clone());

        tokio::time::advance(HEALTH_WINDOW + Duration::from_millis(50)).await;
        // The aggregator emits at the window boundary; `changed()`
        // fires regardless of value equality, so we await it once
        // and then read.
        health_rx
            .changed()
            .await
            .expect("aggregator dropped sender");
        assert_eq!(
            *health_rx.borrow_and_update(),
            None,
            "empty window must publish None, not Some(empty_health)",
        );

        token.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_emits_none_after_some_when_window_goes_silent() {
        // Real recovery scenario the policy must handle: a noisy
        // window publishes Some(snapshot), then the browser goes
        // silent. The next window must publish None so the policy
        // returns to "no signal" rather than continuing to react
        // to the stale Some — and especially must not see a fake
        // "loss=0" reading as recovery.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<TwccEvent>();
        let (health_tx, mut health_rx) = watch::channel(None);
        let token = CancellationToken::new();
        let _handle = spawn_twcc_health_aggregator(event_rx, health_tx, token.clone());

        let t0 = Instant::now();

        // Window 1: events arrive → Some.
        event_tx.send(evt(t0, 100, 90, 1)).unwrap();
        tokio::time::advance(HEALTH_WINDOW + Duration::from_millis(50)).await;
        health_rx
            .changed()
            .await
            .expect("aggregator dropped sender");
        let snap = *health_rx.borrow_and_update();
        assert!(snap.is_some(), "non-empty window must publish Some");

        // Window 2: silence → None, NOT Some(empty_health).
        tokio::time::advance(HEALTH_WINDOW + Duration::from_millis(50)).await;
        health_rx
            .changed()
            .await
            .expect("aggregator dropped sender");
        assert_eq!(
            *health_rx.borrow_and_update(),
            None,
            "silent window after noisy must publish None",
        );

        token.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_exits_when_event_channel_closes() {
        // Drop the event sender — aggregator's `recv()` returns
        // `None`, task exits without further snapshots.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<TwccEvent>();
        let (health_tx, _health_rx) = watch::channel(None);
        let token = CancellationToken::new();
        let handle = spawn_twcc_health_aggregator(event_rx, health_tx, token);

        drop(event_tx);
        // Don't even need to advance the clock — channel-close is
        // observed at the next select! tick.
        tokio::time::advance(Duration::from_millis(1)).await;
        // Bounded join: if the task didn't exit, the test will
        // hang and the runner's per-test timeout catches it.
        handle.await.expect("aggregator task panicked");
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_exits_on_cancellation() {
        let (_event_tx, event_rx) = mpsc::unbounded_channel::<TwccEvent>();
        let (health_tx, _health_rx) = watch::channel(None);
        let token = CancellationToken::new();
        let handle = spawn_twcc_health_aggregator(event_rx, health_tx, token.clone());

        token.cancel();
        tokio::time::advance(Duration::from_millis(1)).await;
        handle.await.expect("aggregator task panicked");
    }
}
