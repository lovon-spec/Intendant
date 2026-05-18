//! Display-level layer-pool orchestration.
//!
//! ## Architecture
//!
//! One coordinator task per display owns
//! [`crate::display::encode::pool::EncoderPool::pause_layer`] /
//! [`crate::display::encode::pool::EncoderPool::resume_layer`]
//! decisions. Three policies vote — presence, aggregate-TWCC, and
//! per-RID RR — and the coordinator composes by intersection: a
//! layer is wanted iff every active policy agrees. **Pause wins;
//! resume requires consensus.** This replaces the previous
//! one-task-per-policy design that produced opposite actions when
//! one policy had signal and another defaulted to "Wanted."
//!
//! ## Why display-level, not encode-level
//!
//! This module ties **peer presence**
//! ([`crate::display::webrtc::WebRtcPeer`]) to **encoder pool
//! lifecycle** ([`crate::display::encode::pool::EncoderPool`]).
//! Putting it under `encode/` would force `encode/` to depend
//! upward on `webrtc` (a module that's `encode/`'s consumer, not
//! its peer), inverting the dependency graph. Living at the
//! display level lets the coordinator consume both cleanly
//! without pushing webrtc-awareness down into the encoder
//! primitive layer.
//!
//! ## Policies
//!
//! - **Presence policy**: zero-peer pause-after-debounce + resume-
//!   on-first-peer. State machine in [`transition`]; the
//!   coordinator votes empty wanted set when in `Idle`, full set
//!   otherwise. Same `IdlePending` debounce semantics as the
//!   previous standalone zero-peer aggregator (5 s by default,
//!   protects against browser-refresh / network-blip /
//!   federation-rehandshake thrashing).
//! - **Aggregate-TWCC policy**: per-peer cascaded loss-driven
//!   pause via [`AggregateLayerCapacity`] + [`step_aggregate_layer_capacity`]
//!   reading from [`crate::display::twcc_tap`]. Cascade pauses
//!   top first, then mid, on sustained loss; reverse on
//!   recovery. The actionable signal source on the rtc 0.9 +
//!   WKWebView stack.
//! - **Per-RID RR policy**: per-(peer, RID)
//!   [`LayerCapacityState`] off `RTCRemoteInboundRtpStreamStats`
//!   `fraction_lost`. Currently inert (rtc 0.9 doesn't populate
//!   the stats accumulator) but stays warm for future stacks.
//!
//! Each policy votes the full current rid set when it has no
//! useful signal — "no signal" means "no restriction," not "no
//! recovery." Only sustained, real signals narrow the wanted set.
//!
//! ## State pruning on zero peers
//!
//! When `peers.is_empty()` the coordinator clears all per-peer
//! subscriptions and policy state before the presence policy
//! fires its zero-peer pause. Reconnects (same `PeerId`) start
//! fresh: TWCC cascade at `AllUpperWanted`, per-RID at `Wanted`,
//! RTT-measurement counts at zero. No stale signal can re-trigger
//! a pause on a freshly-arrived peer.
//!
//! ## Action handling is injected (testability)
//!
//! [`spawn_layer_policy_coordinator`] takes
//! `Box<dyn Fn(CapacityAction)>` instead of a direct
//! [`crate::display::encode::pool::EncoderPool`] reference. The
//! closure pattern keeps the per-policy state machines pure
//! (testable without spawning a real pool, capturing rids, or
//! constructing fake encoder backends) and lets the production
//! wiring at [`crate::display::DisplaySession::start`] capture the
//! pool in one place.

use crate::display::encode::pool::SimulcastRid;
use crate::display::webrtc::{PeerLayerHealth, WebRtcPeer};
use crate::display::PeerId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// How long we wait at zero peers before pausing all simulcast layers.
///
/// 5s avoids thrashing on browser-refresh, brief-disconnect blips,
/// and federation reconnect cycles (the actor's reconnect backoff
/// starts at 500ms and rarely exceeds a few seconds for transient
/// drops). A peer that genuinely went away stays away beyond this
/// window; a peer that was momentarily disconnected reconnects
/// before the timer fires and we never pause.
const PAUSE_DEBOUNCE: Duration = Duration::from_secs(5);

/// How often the aggregator polls the peers map.
///
/// 1s gives sub-debounce-window resolution on the pause edge and
/// effectively-immediate response on the resume edge. Polling cost
/// is one `RwLock<HashMap>::read().await + .len()` per tick — sub-
/// microsecond and never contended.
const TICK: Duration = Duration::from_secs(1);

/// Presence state for the layer-policy coordinator's presence
/// policy: `Active` while peers are connected, `IdlePending`
/// during the [`PAUSE_DEBOUNCE`] window after peers drop to zero,
/// `Idle` after the debounce window has elapsed without peers
/// returning. The coordinator votes "all current rids" in
/// `Active` / `IdlePending` and "no rids" in `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggregatorState {
    Active,
    IdlePending { since: Instant },
    Idle,
}

/// Pure transition function for the presence state machine — no
/// side effects, no async, no I/O.
///
/// Returns the next state given the current peer count and clock.
/// The previous design also returned an `Option<AggregatorAction>`
/// for direct pause/resume callbacks; with the layer-policy
/// coordinator owning all pool actions, that information is
/// derived from the state itself (Idle → empty wanted set, others
/// → full wanted set) and the action enum has been removed.
fn transition(prev: AggregatorState, peer_count: usize, now: Instant) -> AggregatorState {
    match prev {
        AggregatorState::Active if peer_count == 0 => AggregatorState::IdlePending { since: now },
        AggregatorState::Active => AggregatorState::Active,

        AggregatorState::IdlePending { .. } if peer_count >= 1 => AggregatorState::Active,
        AggregatorState::IdlePending { since } if now >= since + PAUSE_DEBOUNCE => {
            AggregatorState::Idle
        }
        AggregatorState::IdlePending { since } => AggregatorState::IdlePending { since },

        AggregatorState::Idle if peer_count >= 1 => AggregatorState::Active,
        AggregatorState::Idle => AggregatorState::Idle,
    }
}

// ===========================================================================
// Phase 4d.3b: per-(peer, RID) capacity-decision policy
// ===========================================================================
//
// Pure data → data. Decides which simulcast layers a single peer wants
// based on that peer's per-RID receiver-feedback health (4d.3a's
// `RTCRemoteInboundRtpStreamStats`-derived signal). 4d.3c will own the
// per-(peer, RID) state map and the aggregation across peers + the
// pool actions; this layer just defines the state machine.
//
// **Why not egress-as-capacity** (rejected on 4d.2 review): the
// `observed_send_bitrate` watch is local egress; pausing a layer drops
// observed egress below its threshold and ratchets the layer paused
// permanently. RR-derived `fraction_lost` is a remote signal — it
// reports what the receiver actually saw. A paused layer doesn't
// influence its own RR (no traffic, no loss reports), so the ratchet
// trap doesn't apply.
//
// **Floor protection lives at the caller**, not in this module. The
// 4d.3c aggregator iterates policy over non-floor RIDs only; the
// floor (q for VP8 simulcast) is unconditionally wanted whenever any
// peer is connected (4d.2's zero-peer aggregator handles its
// pause-on-zero / resume-on-first-peer lifecycle). Keeping the
// `step_layer_capacity_state` function general lets it apply to any
// non-floor layer cleanly without special-casing.
//
// **No-signal handling**: `health: None` (no RR has arrived for this
// RID yet) preserves the current state. New peers / new RIDs stay
// `Wanted` rather than getting drop-considered on absence; existing
// `Dropped` layers don't accidentally restore on RR loss.

/// Thresholds + debounces for per-layer capacity decisions.
///
/// Two-threshold hysteresis: `fraction_lost_threshold` (over →
/// consider drop) and `fraction_lost_recovery` (under → consider
/// restore). The recovery band is wider than the drop band to avoid
/// oscillation on values hovering near the threshold — once a layer
/// is `Dropped`, the signal must improve clearly past the recovery
/// threshold to trigger restore.
///
/// Asymmetric debounces: drop slow, restore fast. Same rationale as
/// 4d.2's zero-peer gating — pausing on a transient loss spike is a
/// user-visible quality regression; restoring on a brief recovery
/// burst is a no-op if it flips back. Drop is the costly direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapacityPolicyConfig {
    /// `fraction_lost` strictly greater than this triggers a drop
    /// candidate evaluation. 0.05 (5%) is a conservative default —
    /// loss above this is "hurting decode" by typical WebRTC
    /// telemetry conventions.
    pub fraction_lost_threshold: f64,
    /// `fraction_lost` less than or equal to this triggers a restore
    /// candidate evaluation. Wider than the drop threshold (0.02 vs
    /// 0.05) so a layer hovering near the drop threshold doesn't
    /// oscillate Dropped ↔ PendingRestore on every tick.
    pub fraction_lost_recovery: f64,
    /// How long the over-budget signal must persist before a
    /// `PendingDrop` becomes `Dropped` (and the layer is paused).
    /// 5s tolerates transient packet-loss spikes (Wi-Fi interference,
    /// brief congestion bursts) without dropping the layer.
    pub drop_debounce: Duration,
    /// How long the healthy signal must persist before a
    /// `PendingRestore` becomes `Wanted` (and the layer is resumed).
    /// 1s — capacity recovery is good news, react fast; the
    /// asymmetric debounce vs `drop_debounce` reflects that a
    /// premature restore self-corrects (signal flips → back to
    /// Dropped) much more cheaply than a premature drop.
    pub restore_debounce: Duration,
}

impl Default for CapacityPolicyConfig {
    fn default() -> Self {
        Self {
            fraction_lost_threshold: 0.05,
            fraction_lost_recovery: 0.02,
            drop_debounce: Duration::from_secs(5),
            restore_debounce: Duration::from_secs(1),
        }
    }
}

/// Per-(peer, RID) hysteresis state for a non-floor simulcast layer.
///
/// Four states form a four-arm cycle. `Wanted` and `Dropped` are
/// terminal-until-signal-flips; `PendingDrop` and `PendingRestore`
/// are timer states.
///
/// "Wanted" semantics for the per-peer wanted set: `Wanted` and
/// `PendingDrop` both contribute (the layer is still being produced
/// while drop is pending). `Dropped` and `PendingRestore` both
/// don't (the layer is paused while restore is pending). See
/// [`layer_state_is_wanted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCapacityState {
    /// Layer is fully wanted; no drop pending.
    Wanted,
    /// Over-budget signal persisted; if it stays past `drop_debounce`
    /// from `since`, transition to `Dropped`. Brief recovery during
    /// this window cancels back to `Wanted`.
    PendingDrop { since: Instant },
    /// Layer is paused; not contributing to any peer's wanted set.
    Dropped,
    /// Healthy signal persisted; if it stays past `restore_debounce`
    /// from `since`, transition to `Wanted`. Over-budget during this
    /// window cancels back to `Dropped`.
    PendingRestore { since: Instant },
}

/// Pure transition for one (peer, RID) pair given the current health
/// signal and previous state. No side effects; caller owns state map.
///
/// `health: None` (no RR for this RID yet, or RR for this RID
/// dropped from the snapshot) preserves the current state — never
/// triggers a transition on the absence of signal alone. This is
/// load-bearing for new peers (no RR yet → stay `Wanted`) and for
/// RID churn (RR appearing/disappearing during renegotiation
/// shouldn't accidentally drop a layer).
pub fn step_layer_capacity_state(
    prev: LayerCapacityState,
    health: Option<&PeerLayerHealth>,
    config: &CapacityPolicyConfig,
    now: Instant,
) -> LayerCapacityState {
    let Some(h) = health else {
        return prev;
    };
    let over_budget = h.fraction_lost > config.fraction_lost_threshold;
    let healthy = h.fraction_lost <= config.fraction_lost_recovery;

    match prev {
        LayerCapacityState::Wanted if over_budget => LayerCapacityState::PendingDrop { since: now },
        LayerCapacityState::Wanted => LayerCapacityState::Wanted,

        LayerCapacityState::PendingDrop { .. } if !over_budget => {
            // Recovery during pending — cancel the drop. Note the
            // condition is `!over_budget`, not `healthy`: the
            // recovery threshold is for triggering restore *out of*
            // Dropped, not for cancelling a pending drop. Cancelling
            // on any improvement (anything ≤ threshold) avoids
            // dropping on a near-miss above threshold that
            // immediately settles.
            LayerCapacityState::Wanted
        }
        LayerCapacityState::PendingDrop { since } if now >= since + config.drop_debounce => {
            LayerCapacityState::Dropped
        }
        LayerCapacityState::PendingDrop { since } => LayerCapacityState::PendingDrop { since },

        LayerCapacityState::Dropped if healthy => LayerCapacityState::PendingRestore { since: now },
        LayerCapacityState::Dropped => LayerCapacityState::Dropped,

        LayerCapacityState::PendingRestore { .. } if !healthy => {
            // Signal stopped being clearly healthy during pending
            // restore — covers BOTH the gray-band case (above
            // recovery threshold but ≤ drop threshold) AND the
            // over-budget case (above drop threshold). Restore
            // requires the signal to remain `healthy` (≤
            // recovery) for the full debounce; any drift out of
            // healthy cancels back to Dropped without restarting
            // the drop debounce (we're already in the dropped
            // equilibrium and the signal hasn't recovered to the
            // standard the wider-hysteresis-band requires).
            //
            // Symmetric to PendingDrop's cancel-on-recovery: drop
            // cancels on any improvement (`!over_budget`); restore
            // cancels on any regression (`!healthy`).
            LayerCapacityState::Dropped
        }
        LayerCapacityState::PendingRestore { since } if now >= since + config.restore_debounce => {
            LayerCapacityState::Wanted
        }
        LayerCapacityState::PendingRestore { since } => {
            LayerCapacityState::PendingRestore { since }
        }
    }
}

/// True if a layer in this state contributes to the per-peer wanted
/// set. `Wanted` and `PendingDrop` both contribute (layer still being
/// produced); `Dropped` and `PendingRestore` both don't.
pub fn layer_state_is_wanted(state: &LayerCapacityState) -> bool {
    matches!(
        state,
        LayerCapacityState::Wanted | LayerCapacityState::PendingDrop { .. }
    )
}

/// Compute one peer's wanted-layer set from its per-RID capacity-state
/// map. Caller (4d.3c aggregator) maintains the state map across
/// ticks; this is a pure projection.
pub fn per_peer_wanted_layers(
    states: &HashMap<SimulcastRid, LayerCapacityState>,
) -> HashSet<SimulcastRid> {
    states
        .iter()
        .filter(|(_, s)| layer_state_is_wanted(s))
        .map(|(rid, _)| rid.clone())
        .collect()
}

/// Aggregate wanted-layer sets across peers — union semantics. A
/// layer is in the aggregate iff at least one peer wants it.
///
/// The aggregator's pool action set derives from comparing this
/// aggregate to the previously-applied set: layers newly absent get
/// `pause_layer`, layers newly present get `resume_layer`. Idempotent
/// either way (pool methods no-op on redundant calls).
pub fn aggregate_wanted_layers(
    per_peer: impl IntoIterator<Item = HashSet<SimulcastRid>>,
) -> HashSet<SimulcastRid> {
    let mut out = HashSet::new();
    for set in per_peer {
        out.extend(set);
    }
    out
}

// ===========================================================================
// Phase 4d.3b: TWCC aggregate-loss capacity policy
// ===========================================================================
//
// Receivers on this stack (notably WKWebView) report TWCC feedback at
// the **session aggregate** level — one sender-SSRC, one stream of
// `TransportLayerCc` packets covering all simulcast encodings — not
// per-RID. Per-layer adaptation as in [`step_layer_capacity_state`]
// requires per-layer signal, which we don't have here.
//
// The aggregate-loss policy is the cascade for that gap: under
// sustained high TWCC loss, pause the upper simulcast layers in
// order (top, then middle), keeping the floor layer always active.
// Under sustained recovery, resume in reverse order. Asymmetric
// debouncing and hysteresis between [`CapacityPolicyConfig::fraction_lost_threshold`]
// and [`CapacityPolicyConfig::fraction_lost_recovery`] prevent
// flapping at the boundary.
//
// **Why not per-(peer, RID) like the existing 4d.3c policy:** the
// existing policy assumes `PeerLayerHealth` per RID — populated from
// rtc 0.9's `RTCRemoteInboundRtpStreamStats` which doesn't actually
// fire on this stack. The aggregate-loss policy is the practical
// substitute: one signal per peer (not per layer), driving a peer-
// wide cascade rather than per-layer adaptation. Per-RID adaptation
// reactivates as a 4d.3c concern when receivers expose per-layer
// TLC.
//
// **Why not just reuse `step_layer_capacity_state` driven by the
// same aggregate signal across all non-floor RIDs:** the existing
// machine is parallel — every RID's state advances independently
// from the same signal, so they'd all enter `PendingDrop` at the
// same instant and all transition to `Dropped` at the same instant.
// That's a cliff, not a cascade. The directive calls for cascaded
// pause (top first, middle only after top has been paused for an
// additional drop_debounce) so the bandwidth pressure from pausing
// top can be observed before deciding whether middle also needs to
// go. The cascade requires explicit between-RID ordering that a
// parallel per-RID machine can't express.

/// Stable + pending positions in the aggregate-loss cascade.
///
/// Three stable positions (`AllUpperWanted`, `TopPaused`,
/// `OnlyFloor`) bracket four pending positions that drive the
/// transitions between them. The pending positions all carry their
/// `since: Instant` so the state machine can compute "this signal
/// has persisted long enough" without external timer state.
///
/// Layer naming is deliberately abstract — `top`, `mid`, `floor` —
/// so the policy can be exercised in tests without committing to a
/// specific RID identifier ("f", "h", "q" for VP8 simulcast). The
/// production wiring resolves these to concrete `SimulcastRid`s via
/// [`aggregate_state_wanted_layers`].
///
/// Initial state for a freshly-constructed peer is
/// [`AggregateLayerCapacity::AllUpperWanted`] — the encoder pool
/// produces all layers by default; no over-budget signal has been
/// observed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateLayerCapacity {
    /// All upper layers wanted; no over-budget signal persisted.
    AllUpperWanted,
    /// Over-budget signal arrived; counting down to pause top.
    /// Cancels back to `AllUpperWanted` if the signal recovers
    /// before `drop_debounce` elapses.
    PendingPauseTop { since: Instant },
    /// Top paused; mid and floor still wanted. Equilibrium between
    /// the two cascades: enter `PendingPauseMid` if loss persists,
    /// enter `PendingResumeTop` if loss recovers cleanly. Stays here
    /// in the gray band between recovery and threshold.
    TopPaused,
    /// Top paused, loss still high after `drop_debounce` elapsed in
    /// `TopPaused`. Counting down to also pause mid. Cancels back
    /// to `TopPaused` on any improvement.
    PendingPauseMid { since: Instant },
    /// Both upper layers paused; only floor active. Loss must
    /// recover cleanly to leave this state.
    OnlyFloor,
    /// Recovery from `OnlyFloor` underway; counting down to resume
    /// mid. Cancels back to `OnlyFloor` on regression.
    PendingResumeMid { since: Instant },
    /// Recovery from `TopPaused` underway; counting down to resume
    /// top (i.e. return to `AllUpperWanted`). Cancels back to
    /// `TopPaused` on regression.
    PendingResumeTop { since: Instant },
}

/// Pure transition for one peer's aggregate-loss state given the
/// most recent [`crate::display::twcc_tap::TwccHealth`] reading.
/// No side effects; caller owns state.
///
/// `health = None` (no snapshot from the aggregator yet, or
/// subscriber hasn't been polled) preserves the current state —
/// never triggers a transition on absence of signal alone. This is
/// load-bearing for new peers (no TWCC yet → stay
/// `AllUpperWanted`) and for transient subscriber lag.
///
/// **Empty-window `Some(_)` readings preserve state too.** Silence
/// is not recovery: a `TwccHealth { batches: 0, ..}` or
/// `reported_packets: 0` reading represents "no TLC arrived during
/// this window," not "the link is healthy." Treating empty-Some
/// as healthy would resume upper layers under sustained feedback
/// silence, which is the opposite of what we want — silence likely
/// means the receiver itself can't get bytes through to us, so the
/// link is in worse shape than the most recent loss reading
/// suggested.
///
/// The aggregator at [`crate::display::twcc_tap::spawn_twcc_health_aggregator`]
/// publishes `None` for empty windows precisely so the policy
/// short-circuits via the `let Some(h) = health` arm above. The
/// `batches == 0 || reported_packets == 0` guard here is
/// defense-in-depth — even if some future code path constructs a
/// `Some(empty_health)` and feeds it in, the policy must not act
/// on it.
pub fn step_aggregate_layer_capacity(
    prev: AggregateLayerCapacity,
    health: Option<&crate::display::twcc_tap::TwccHealth>,
    config: &CapacityPolicyConfig,
    now: Instant,
) -> AggregateLayerCapacity {
    let Some(h) = health else {
        return prev;
    };
    if h.batches == 0 || h.reported_packets == 0 {
        return prev;
    }
    let over_budget = h.loss_fraction > config.fraction_lost_threshold;
    let healthy = h.loss_fraction <= config.fraction_lost_recovery;

    match prev {
        // ----- Stable: AllUpperWanted -----
        AggregateLayerCapacity::AllUpperWanted if over_budget => {
            AggregateLayerCapacity::PendingPauseTop { since: now }
        }
        AggregateLayerCapacity::AllUpperWanted => AggregateLayerCapacity::AllUpperWanted,

        // ----- Pending: PendingPauseTop -----
        // Cancel-on-improvement: any drop below threshold cancels.
        // (Same as `step_layer_capacity_state`'s PendingDrop arm —
        // cancelling on `!over_budget` rather than `healthy` keeps
        // a borderline-but-improving signal from triggering a drop.)
        AggregateLayerCapacity::PendingPauseTop { .. } if !over_budget => {
            AggregateLayerCapacity::AllUpperWanted
        }
        AggregateLayerCapacity::PendingPauseTop { since }
            if now >= since + config.drop_debounce =>
        {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingPauseTop { since } => {
            AggregateLayerCapacity::PendingPauseTop { since }
        }

        // ----- Stable: TopPaused -----
        // Cascade: still over-budget after top is paused → start
        // counting down to pause mid as well.
        AggregateLayerCapacity::TopPaused if over_budget => {
            AggregateLayerCapacity::PendingPauseMid { since: now }
        }
        // Recovery: cleanly healthy → start counting down to resume
        // top. Has to be `healthy` (≤ recovery threshold), not just
        // `!over_budget`, to avoid toggling out of TopPaused on
        // gray-band readings.
        AggregateLayerCapacity::TopPaused if healthy => {
            AggregateLayerCapacity::PendingResumeTop { since: now }
        }
        AggregateLayerCapacity::TopPaused => AggregateLayerCapacity::TopPaused,

        // ----- Pending: PendingPauseMid -----
        AggregateLayerCapacity::PendingPauseMid { .. } if !over_budget => {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingPauseMid { since }
            if now >= since + config.drop_debounce =>
        {
            AggregateLayerCapacity::OnlyFloor
        }
        AggregateLayerCapacity::PendingPauseMid { since } => {
            AggregateLayerCapacity::PendingPauseMid { since }
        }

        // ----- Pending: PendingResumeTop -----
        // Symmetric to PendingDrop's cancel-on-recovery in the
        // per-RID machine: restore cancels on any regression
        // (`!healthy`), not just `over_budget`. Restoring requires
        // a clean, persisted healthy signal across the entire
        // restore_debounce window.
        AggregateLayerCapacity::PendingResumeTop { .. } if !healthy => {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingResumeTop { since }
            if now >= since + config.restore_debounce =>
        {
            AggregateLayerCapacity::AllUpperWanted
        }
        AggregateLayerCapacity::PendingResumeTop { since } => {
            AggregateLayerCapacity::PendingResumeTop { since }
        }

        // ----- Stable: OnlyFloor -----
        AggregateLayerCapacity::OnlyFloor if healthy => {
            AggregateLayerCapacity::PendingResumeMid { since: now }
        }
        AggregateLayerCapacity::OnlyFloor => AggregateLayerCapacity::OnlyFloor,

        // ----- Pending: PendingResumeMid -----
        AggregateLayerCapacity::PendingResumeMid { .. } if !healthy => {
            AggregateLayerCapacity::OnlyFloor
        }
        AggregateLayerCapacity::PendingResumeMid { since }
            if now >= since + config.restore_debounce =>
        {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingResumeMid { since } => {
            AggregateLayerCapacity::PendingResumeMid { since }
        }
    }
}

/// Project an [`AggregateLayerCapacity`] state to the wanted-RID
/// set, given the cascade's upper layers in spec order (top first,
/// then mid if present, etc.).
///
/// Floor RID is always wanted while peers are present (presence
/// policy owns the zero-peer pause); this function returns only
/// the *upper* layers and is meant to be unioned with `{floor}`
/// at the caller.
///
/// "Wanted" semantics: a layer is in the set iff the encoder pool
/// should currently be producing it. Pending-pause states still
/// produce (we haven't decided to pause yet); pending-resume
/// states do not (we paused, and haven't decided to restart yet).
///
/// **Variable-arity cascade**: the state machine has 7 states
/// designed for a top + mid cascade, but `upper_layers` may be
/// shorter (e.g., 2-layer simulcast at very small dims, where
/// `vp8_simulcast` emits only `full + half` with `half` as floor —
/// only `full` remains as the upper layer). The projection
/// handles all cases:
///
/// - `AllUpperWanted` / `PendingPauseTop` → all of `upper_layers`
///   (cascade hasn't started pausing yet).
/// - `TopPaused` / `PendingPauseMid` / `PendingResumeTop` → all
///   of `upper_layers[1..]` (top dropped). For 1-layer cascades
///   this is empty — equivalent to `OnlyFloor`.
/// - `OnlyFloor` / `PendingResumeMid` → empty.
pub fn aggregate_state_wanted_upper_layers(
    state: AggregateLayerCapacity,
    upper_layers: &[SimulcastRid],
) -> HashSet<SimulcastRid> {
    match state {
        AggregateLayerCapacity::AllUpperWanted | AggregateLayerCapacity::PendingPauseTop { .. } => {
            upper_layers.iter().cloned().collect()
        }
        AggregateLayerCapacity::TopPaused
        | AggregateLayerCapacity::PendingPauseMid { .. }
        | AggregateLayerCapacity::PendingResumeTop { .. } => {
            upper_layers.iter().skip(1).cloned().collect()
        }
        AggregateLayerCapacity::OnlyFloor | AggregateLayerCapacity::PendingResumeMid { .. } => {
            HashSet::new()
        }
    }
}

// ===========================================================================
// Shared layer-action vocabulary + helpers
// ===========================================================================
//
// `CapacityAction`, `fresh_health`, and `diff_wanted_aggregate`
// are the cross-policy primitives the layer-policy coordinator
// composes per tick. They were originally introduced for the
// per-RID-RR `spawn_capacity_aggregator` (now subsumed by
// [`spawn_layer_policy_coordinator`]) and remain useful as the
// shared output type and pure diff/freshness helpers.

/// Side-effecting action the layer-policy coordinator emits at the
/// end of each tick. Production wiring at
/// [`crate::display::DisplaySession::start`] maps each variant to
/// [`crate::display::encode::pool::EncoderPool::pause_layer`] /
/// [`crate::display::encode::pool::EncoderPool::resume_layer`]
/// with `CodecKind::Vp8` (the always-on simulcast codec). The
/// per-RID-RR and aggregate-TWCC policies vote for a wanted set;
/// the coordinator composes via intersection and emits one
/// `CapacityAction` per layer whose actual pool state diverges
/// from the composed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityAction {
    /// Pause one simulcast layer (a non-floor RID the aggregate no
    /// longer wants).
    PauseLayer(SimulcastRid),
    /// Resume one simulcast layer (a non-floor RID newly present in
    /// the aggregate). Pool's `resume_layer` already forces a
    /// keyframe on the paused→active edge per the 4d.0 review fix,
    /// so a peer waiting on this layer gets a decodable frame
    /// within one encode tick.
    ResumeLayer(SimulcastRid),
}

/// **Phase 4d.3c review fix**: returns the health entry only if its
/// RTT-measurement count is strictly greater than the previously-
/// observed count for this peer-RID. `None` from the helper means
/// "no fresh RR since last observation" — pass-through to
/// [`step_layer_capacity_state`] as "no signal," which preserves
/// the layer's current state without advancing the debounce.
///
/// **Why**: rtc 0.9 keeps surfacing the most recent RR-derived
/// values every poll until the next RR arrives. Without this
/// freshness check, a single bad RR from minutes ago would be
/// re-presented every aggregator tick and complete a 5s drop
/// debounce all on its own — even if the link recovered or the
/// peer simply stopped sending RRs. Comparing `round_trip_time_measurements`
/// (monotonically non-decreasing in rtc 0.9's RR processing
/// pipeline) against a per-(peer, RID) prev-count snapshot is the
/// freshness discriminator.
///
/// `None` input passes through as `None` — preserves the no-RR
/// contract from 4d.3a's pre-RR filter.
pub fn fresh_health<'a>(
    raw: Option<&'a PeerLayerHealth>,
    prev_count: u64,
) -> Option<&'a PeerLayerHealth> {
    raw.filter(|h| h.round_trip_time_measurements > prev_count)
}

/// Pure: compute the action sequence for one tick by diffing the
/// previously-applied wanted set against the current aggregate.
/// Iteration is bounded by `all_non_floor_rids` (not by either
/// HashSet) so the action ordering is stable across runs and tests.
///
/// Layers in `prev_applied` but missing from `current_aggregate` →
/// `PauseLayer`; layers in `current_aggregate` but missing from
/// `prev_applied` → `ResumeLayer`. Layers present in both, or
/// absent from both, produce no action (idempotent at the pool
/// layer either way, but skipping the no-op call keeps the
/// closure-invocation count down — useful in tests with a
/// recording sink).
pub fn diff_wanted_aggregate(
    prev_applied: &HashSet<SimulcastRid>,
    current_aggregate: &HashSet<SimulcastRid>,
    all_non_floor_rids: &[SimulcastRid],
) -> Vec<CapacityAction> {
    let mut out = Vec::new();
    for rid in all_non_floor_rids {
        let was = prev_applied.contains(rid);
        let is = current_aggregate.contains(rid);
        if was && !is {
            out.push(CapacityAction::PauseLayer(rid.clone()));
        } else if !was && is {
            out.push(CapacityAction::ResumeLayer(rid.clone()));
        }
    }
    out
}

// ===========================================================================
// Phase 4d.3b: layer-policy composition
// ===========================================================================
//
// **One coordinator owns pool.pause_layer / resume_layer.** The
// presence policy (zero-peer debounce), aggregate-TWCC policy
// (cascaded loss-driven pause), and per-RID RR policy (per-layer
// fraction-lost) are composed by intersection: a layer is wanted
// iff EVERY policy votes for it. Pause wins; resume requires every
// active policy to agree.
//
// This replaces the previous design that ran each policy as an
// independent task writing to the pool. With independent writers,
// the per-RID policy's "no signal → default Wanted" semantic would
// see a layer paused by the TWCC task and immediately resume it on
// its next tick — idempotent pool methods don't make opposite
// actions benign. The composer eliminates the conflict by
// resolving policy votes before any pool action fires, so each tick
// produces exactly one set of decisions.
//
// **Each policy votes the full current rid set when it has no
// useful signal.** That way "no signal" means "no restriction" and
// the intersection is unaffected. Policies only narrow the wanted
// set when they have a real reason to (sustained loss, sustained
// presence absence, etc.).
//
// **Per-peer state pruning on zero peers.** When `peers.is_empty()`
// the coordinator clears all per-peer subscriptions and state
// before the presence policy fires its zero-peer pause. Reconnects
// (same `PeerId`) start fresh: TWCC state at `AllUpperWanted`,
// per-RID state at `Wanted`, RTT-measurement counts at zero. No
// stale signal can re-trigger a pause on a freshly-arrived peer.

/// Compose the final wanted-layer set from per-policy wanted sets.
///
/// Intersection semantics: a layer is wanted iff EVERY input set
/// contains it. Iterates `current_rids` (not the input sets) so
/// the output is bounded to the current pool layout — a layer
/// removed by `EncoderPool::on_resize` since the policy state was
/// last refreshed never appears in the result.
///
/// `presence_active = false` short-circuits to empty (zero-peer
/// idle pauses everything, regardless of what the per-peer
/// policies might have voted before pruning).
///
/// `twcc_union` and `rr_union` are the union-across-peers wanted
/// sets from the aggregate-TWCC and per-RID-RR policies
/// respectively. Each policy returns the full `current_rids` set
/// when it has no peers to evaluate (no restriction), so the
/// intersection during a no-peers tick reduces to whatever the
/// presence policy decided.
///
/// **#57 — `pinned_layers` overrides intersection narrowing.** Each
/// peer with exactly one negotiated RID
/// ([`crate::display::webrtc::WebRtcPeer::active_rids`]) contributes
/// that RID to `pinned_layers`. After computing the regular
/// presence × twcc × rr intersection, those pinned RIDs are
/// unconditionally unioned into the effective wanted set (still
/// bounded by `current_rids` — pinning a RID the pool isn't
/// producing right now is silently dropped). Rationale: a peer that
/// negotiated a single RID can't fall back to the floor — its
/// WebRTC track only has one encoding, so pausing that layer
/// starves the peer rather than degrading it.
///
/// **#48 — `demanded_layers` is the hard upper bound.** The union
/// of all live peers' negotiated active RIDs
/// ([`crate::display::webrtc::WebRtcPeer::active_rids`]) defines
/// the set of RIDs ANY live peer can actually consume. Layers
/// outside this set are wasted CPU — the encoder produces frames
/// that no peer's WebRTC track can decode. The bound is applied
/// LAST: `effective = ((twcc∩rr) ∪ pinned) ∩ demanded`. Local
/// DisplaySlot demands `{f,h,q}` (multi-RID offer), so its
/// behavior is unchanged. Federated `q`-only demands `{q}`, so
/// `f`/`h` pause regardless of loss state. Mixed local+federated
/// demands `{f,h,q}` (union) for as long as the local peer is
/// live. Zero peers → demanded is empty → effective is empty
/// (encoders pause immediately, no debounce-window-of-wasted-CPU).
///
/// The single-RID peer's pin is by construction ⊆ demanded
/// (their pin is their only negotiated RID, which is in their
/// own contribution to the union), so the demanded bound never
/// strips a pinned RID. Both invariants — pin and demand — hold
/// simultaneously.
pub fn compose_effective_wanted(
    presence_active: bool,
    twcc_union: &HashSet<SimulcastRid>,
    rr_union: &HashSet<SimulcastRid>,
    current_rids: &[SimulcastRid],
    pinned_layers: &HashSet<SimulcastRid>,
    demanded_layers: &HashSet<SimulcastRid>,
) -> HashSet<SimulcastRid> {
    if !presence_active {
        return HashSet::new();
    }
    let mut effective: HashSet<SimulcastRid> = current_rids
        .iter()
        .filter(|rid| twcc_union.contains(*rid) && rr_union.contains(*rid))
        .cloned()
        .collect();
    // #57: union in pinned RIDs (single-negotiated-RID peers must
    // keep their layer active regardless of TWCC/RR loss). Bounded
    // by current_rids so a stale pin from a since-resized pool
    // never resurrects a vanished layer.
    for rid in pinned_layers {
        if current_rids.contains(rid) {
            effective.insert(rid.clone());
        }
    }
    // #48: hard upper bound — keep only layers some live peer
    // actually demands. Applied LAST so it wins over both the
    // policy votes and the pin (though by construction pin ⊆
    // demand, so this is a no-op for pinned layers). Empty
    // demand (zero peers, or all peers somehow without RIDs)
    // collapses to empty effective — encoders pause immediately
    // rather than waiting for the presence-debounce.
    effective.retain(|rid| demanded_layers.contains(rid));
    effective
}

/// Spawn the single layer-policy coordinator for one display.
///
/// One task, three policies, one writer. Subscribes to each peer's
/// [`crate::display::webrtc::WebRtcPeer::subscribe_twcc_health`]
/// and
/// [`crate::display::webrtc::WebRtcPeer::subscribe_remote_inbound_health`]
/// watches lazily on first observation, advances per-peer state for
/// each policy on every tick, composes via intersection through
/// [`compose_effective_wanted`], diffs once against actual pool
/// state via `is_layer_paused`, and emits exactly one set of
/// pause/resume actions through `on_action`.
///
/// `get_current_rids` returns the pool's current VP8 simulcast
/// layer set in spec order (descending bitrate; the last entry is
/// the floor). Production wiring captures `Arc<EncoderPool>` and
/// returns `pool.always_on_ids()` filtered to `CodecKind::Vp8`;
/// the coordinator derives floor + upper-layers from this list on
/// every tick, so a `pool.on_resize` that grows or shrinks the
/// layer set takes effect at most one tick later.
///
/// `is_layer_paused` queries the pool's actual current pause state
/// — `Some(true)` if currently paused, `Some(false)` if active,
/// `None` if no slot exists for that RID. Diffing against actual
/// state (rather than an internal `last_applied`) handles
/// `EncoderPool::on_resize` regenerating layers ACTIVE: the next
/// tick's diff sees them active and re-pauses if the policy still
/// excludes them.
///
/// `on_action` applies the requested side effect, mapping to
/// `pool.pause_layer` / `pool.resume_layer` with `CodecKind::Vp8`.
///
/// `config` is shared across the aggregate-TWCC and per-RID-RR
/// policies — both use the same threshold/debounce constants
/// (default 0.05 / 0.02 / 5 s drop / 1 s restore).
///
/// Returned `JoinHandle` exits cleanly on `shutdown.cancelled()`.
pub fn spawn_layer_policy_coordinator(
    peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>>,
    get_current_rids: Box<dyn Fn() -> Vec<SimulcastRid> + Send + Sync>,
    is_layer_paused: Box<dyn Fn(&SimulcastRid) -> Option<bool> + Send + Sync>,
    on_action: Box<dyn Fn(CapacityAction) + Send + Sync>,
    config: CapacityPolicyConfig,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // ---- Per-policy state ----

        // Presence: same `transition`-driven debounce machine as the
        // pre-composition zero-peer aggregator. Lives on the
        // coordinator (not per-peer) because presence is a
        // display-level property.
        let mut presence_state = AggregatorState::Active;

        // Aggregate TWCC: one cascade state per peer.
        let mut twcc_state: HashMap<PeerId, AggregateLayerCapacity> = HashMap::new();
        let mut twcc_subs: HashMap<
            PeerId,
            tokio::sync::watch::Receiver<Option<crate::display::twcc_tap::TwccHealth>>,
        > = HashMap::new();

        // Per-RID RR: one per-(peer, RID) state, plus the
        // RTT-measurement-count snapshot the freshness filter
        // uses. Layer-set changes (resize) are tolerated by
        // pruning stale RID entries each tick — `current_rids`
        // is the source of truth.
        let mut rr_state: HashMap<(PeerId, SimulcastRid), LayerCapacityState> = HashMap::new();
        let mut rr_subs: HashMap<
            PeerId,
            tokio::sync::watch::Receiver<HashMap<SimulcastRid, PeerLayerHealth>>,
        > = HashMap::new();
        let mut prev_measurement_count: HashMap<(PeerId, SimulcastRid), u64> = HashMap::new();

        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let now = Instant::now();

                    // ---- Snapshot peers + advance presence ----
                    let current_peers = peers.read().await;
                    let peer_count = current_peers.len();

                    presence_state = transition(presence_state, peer_count, now);
                    let presence_active = !matches!(
                        presence_state,
                        AggregatorState::Idle
                    );

                    // ---- Prune per-peer state on zero peers ----
                    // Reconnects (same PeerId) start fresh: no stale
                    // TWCC cascade or RR layer state can survive an
                    // idle window.
                    if peer_count == 0 {
                        twcc_subs.clear();
                        rr_subs.clear();
                        twcc_state.clear();
                        rr_state.clear();
                        prev_measurement_count.clear();
                    } else {
                        twcc_subs.retain(|id, _| current_peers.contains_key(id));
                        rr_subs.retain(|id, _| current_peers.contains_key(id));
                        for (id, peer) in current_peers.iter() {
                            twcc_subs
                                .entry(id.clone())
                                .or_insert_with(|| peer.subscribe_twcc_health());
                            rr_subs
                                .entry(id.clone())
                                .or_insert_with(|| {
                                    peer.subscribe_remote_inbound_health()
                                });
                        }
                        twcc_state.retain(|pid, _| current_peers.contains_key(pid));
                        rr_state
                            .retain(|(pid, _), _| current_peers.contains_key(pid));
                        prev_measurement_count
                            .retain(|(pid, _), _| current_peers.contains_key(pid));
                    }

                    let peer_ids: Vec<PeerId> =
                        current_peers.keys().cloned().collect();
                    // #57: per-tick pinned-layer set. Each peer with
                    // exactly one negotiated RID contributes that RID
                    // to the pin set, which `compose_effective_wanted`
                    // unions into the result regardless of TWCC/RR
                    // loss. Multi-RID peers (`len() > 1`) don't pin —
                    // they have the floor as fallback. Computed before
                    // `drop(current_peers)` so we don't re-acquire the
                    // peers lock just to read this.
                    let pinned_layers: HashSet<SimulcastRid> = current_peers
                        .values()
                        .filter_map(|peer| {
                            let rids = peer.active_rids();
                            if rids.len() == 1 {
                                Some(rids[0].clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    // #48: per-tick demanded-layer set (union of every
                    // live peer's negotiated active_rids). The
                    // composer uses this as a hard upper bound on the
                    // effective wanted set — layers outside this set
                    // produce frames no live peer can decode and are
                    // pure CPU waste. Computed alongside pinned so we
                    // walk peers once per tick. Zero peers ⇒ empty
                    // demand ⇒ effective collapses to empty (encoders
                    // pause immediately).
                    let demanded_layers: HashSet<SimulcastRid> = current_peers
                        .values()
                        .flat_map(|peer| peer.active_rids().iter().cloned())
                        .collect();
                    drop(current_peers);

                    // ---- Refresh current_rids; derive floor + upper ----
                    let current_rids = get_current_rids();
                    if current_rids.is_empty() {
                        // No simulcast layers at all — nothing for the
                        // pool actions to operate on. Per-peer state
                        // would be meaningless without rids; advance
                        // presence above and skip the rest.
                        continue;
                    }
                    // Spec order: descending bitrate, last entry is
                    // floor. Upper = everything before floor.
                    let floor = current_rids.last().unwrap().clone();
                    let upper_layers: Vec<SimulcastRid> =
                        current_rids[..current_rids.len() - 1].to_vec();

                    // ---- Aggregate-TWCC vote ----
                    // No peers → no restriction (full current set).
                    // With peers → step each peer's cascade state
                    // against its current TWCC health, project to
                    // wanted upper layers, add floor, union across
                    // peers.
                    let twcc_union: HashSet<SimulcastRid> = if peer_ids.is_empty() {
                        current_rids.iter().cloned().collect()
                    } else {
                        let mut per_peer: Vec<HashSet<SimulcastRid>> =
                            Vec::with_capacity(peer_ids.len());
                        for peer_id in &peer_ids {
                            let prev = twcc_state
                                .get(peer_id)
                                .copied()
                                .unwrap_or(AggregateLayerCapacity::AllUpperWanted);
                            // SAFE: peer_subs populated from
                            // current_peers above; entry exists.
                            let health =
                                twcc_subs.get(peer_id).unwrap().borrow().clone();
                            let next = step_aggregate_layer_capacity(
                                prev,
                                health.as_ref(),
                                &config,
                                now,
                            );
                            twcc_state.insert(peer_id.clone(), next);
                            let mut wanted = aggregate_state_wanted_upper_layers(
                                next,
                                &upper_layers,
                            );
                            wanted.insert(floor.clone());
                            per_peer.push(wanted);
                        }
                        aggregate_wanted_layers(per_peer)
                    };

                    // ---- Per-RID RR vote ----
                    // Same shape: no peers → no restriction. Per-peer
                    // wanted = floor + per-RID Wanted-state-projection
                    // for non-floor RIDs. Default per-RID state is
                    // Wanted, so a peer with no RR ever yet votes for
                    // every layer (no restriction).
                    let rr_union: HashSet<SimulcastRid> = if peer_ids.is_empty() {
                        current_rids.iter().cloned().collect()
                    } else {
                        let mut per_peer: Vec<HashSet<SimulcastRid>> =
                            Vec::with_capacity(peer_ids.len());
                        for peer_id in &peer_ids {
                            let health_map =
                                rr_subs.get(peer_id).unwrap().borrow().clone();
                            let mut peer_wanted: HashSet<SimulcastRid> =
                                HashSet::new();
                            peer_wanted.insert(floor.clone());
                            for rid in &upper_layers {
                                let key = (peer_id.clone(), rid.clone());
                                let prev = rr_state
                                    .get(&key)
                                    .copied()
                                    .unwrap_or(LayerCapacityState::Wanted);
                                let raw_health = health_map.get(rid);
                                let prev_count = prev_measurement_count
                                    .get(&key)
                                    .copied()
                                    .unwrap_or(0);
                                let fresh = fresh_health(raw_health, prev_count);
                                let next = step_layer_capacity_state(
                                    prev, fresh, &config, now,
                                );
                                rr_state.insert(key.clone(), next);
                                if let Some(h) = raw_health {
                                    prev_measurement_count.insert(
                                        key,
                                        h.round_trip_time_measurements,
                                    );
                                }
                                if layer_state_is_wanted(&next) {
                                    peer_wanted.insert(rid.clone());
                                }
                            }
                            per_peer.push(peer_wanted);
                        }
                        aggregate_wanted_layers(per_peer)
                    };

                    // ---- Compose + diff + apply ----
                    let effective_wanted = compose_effective_wanted(
                        presence_active,
                        &twcc_union,
                        &rr_union,
                        &current_rids,
                        &pinned_layers,
                        &demanded_layers,
                    );

                    let actual_active: HashSet<SimulcastRid> = current_rids
                        .iter()
                        .filter(|rid| {
                            matches!(is_layer_paused(rid), Some(false))
                        })
                        .cloned()
                        .collect();

                    let actions = diff_wanted_aggregate(
                        &actual_active,
                        &effective_wanted,
                        &current_rids,
                    );
                    for action in actions {
                        on_action(action);
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Pure transition tests --------------------------------------------

    #[test]
    fn active_with_peers_stays_active_no_action() {
        // "fresh session, first peer connects" lives here:
        // session starts in Active; peers stay >= 1; nothing fires.
        // Confirms 4d.2 doesn't perturb the encoder pool's
        // 'all-layers-Active' starting state for the policy state
        // machine. Whether layers actually emit frames is a separate
        // question owned by the demand-bound (#48); this test
        // exercises only the per-policy transition.
        let s = transition(AggregatorState::Active, 3, Instant::now());
        assert_eq!(s, AggregatorState::Active);
    }

    #[test]
    fn active_zero_peers_enters_idle_pending_no_action() {
        let now = Instant::now();
        let s = transition(AggregatorState::Active, 0, now);
        assert_eq!(s, AggregatorState::IdlePending { since: now });
    }

    #[test]
    fn idle_pending_peer_arrives_returns_to_active_no_action() {
        // Browser-refresh / federation-reconnect blip: peer briefly
        // gone, comes back well within PAUSE_DEBOUNCE.
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let s = transition(pending, 2, t + Duration::from_secs(1));
        assert_eq!(s, AggregatorState::Active);
    }

    #[test]
    fn idle_pending_pre_debounce_holds() {
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let just_before = t + PAUSE_DEBOUNCE - Duration::from_millis(1);
        let s = transition(pending, 0, just_before);
        assert_eq!(s, AggregatorState::IdlePending { since: t });
    }

    #[test]
    fn idle_pending_post_debounce_pauses_all() {
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let post = t + PAUSE_DEBOUNCE;
        let s = transition(pending, 0, post);
        assert_eq!(s, AggregatorState::Idle);
    }

    #[test]
    fn idle_zero_peers_stays_idle() {
        let s = transition(AggregatorState::Idle, 0, Instant::now());
        assert_eq!(s, AggregatorState::Idle);
    }

    #[test]
    fn idle_first_peer_resumes_all_layers() {
        // The whole point of choosing ResumeAllSimulcast over
        // ResumeFloor: a post-idle joiner gets full quality, not a
        // quarter-res regression. 4d.3 will pause upper layers
        // selectively based on per-peer link health, but until then
        // 4d.2 must NOT silently downgrade.
        let s = transition(AggregatorState::Idle, 1, Instant::now());
        assert_eq!(s, AggregatorState::Active);
    }

    #[test]
    fn debounce_resets_on_re_idle_after_active_blip() {
        // Sequence:
        //   t=0  Active,  peers=0 -> IdlePending{0}
        //   t=1  Pending, peers=2 -> Active           (cancel pause)
        //   t=4  Active,  peers=0 -> IdlePending{4}   (NEW since)
        //   t=8  Pending, peers=0 -> still Pending    (4+5=9, not yet 8)
        //   t=9  Pending, peers=0 -> Idle + PauseAll
        // Confirms `since` is re-snapshotted on each Active→Pending
        // edge — a previous pending epoch's `since` doesn't bleed
        // through to count down a later epoch's debounce.
        let t0 = Instant::now();
        let s = transition(AggregatorState::Active, 0, t0);
        let s = transition(s, 2, t0 + Duration::from_secs(1));
        assert_eq!(s, AggregatorState::Active, "blip resolved");
        let s = transition(s, 0, t0 + Duration::from_secs(4));
        assert!(matches!(s, AggregatorState::IdlePending { .. }));
        let s = transition(s, 0, t0 + Duration::from_secs(8));
        assert!(matches!(s, AggregatorState::IdlePending { .. }));
        let s = transition(s, 0, t0 + Duration::from_secs(9));
        assert_eq!(s, AggregatorState::Idle);
    }

    // -----------------------------------------------------------------
    // Phase 4d.3b: capacity-policy state-machine tests
    // -----------------------------------------------------------------

    fn t0() -> Instant {
        Instant::now()
    }

    fn cfg() -> CapacityPolicyConfig {
        CapacityPolicyConfig::default()
    }

    fn health(fraction_lost: f64) -> PeerLayerHealth {
        // `round_trip_time_measurements: 1` — synthesizes a single
        // RR observation. The freshness check (`fresh_health`)
        // runs at a higher layer (the spawn loop), not in the pure
        // policy tests; these tests pass `Some(&health)` directly
        // to `step_layer_capacity_state` to exercise its state-
        // machine transitions. The measurement count is irrelevant
        // for the policy itself but must be set so the field
        // exists.
        PeerLayerHealth {
            fraction_lost,
            packets_lost_total: 0,
            round_trip_time_seconds: 0.0,
            round_trip_time_measurements: 1,
        }
    }

    fn health_with_measurements(fraction_lost: f64, measurements: u64) -> PeerLayerHealth {
        PeerLayerHealth {
            fraction_lost,
            packets_lost_total: 0,
            round_trip_time_seconds: 0.0,
            round_trip_time_measurements: measurements,
        }
    }

    // ----- step_layer_capacity_state -----

    #[test]
    fn capacity_step_no_signal_preserves_state() {
        // Load-bearing for new peers: no RR yet → stay Wanted.
        // Also load-bearing for RR churn: an RID disappearing from
        // the snapshot mid-session must not cascade-drop the layer.
        for prev in [
            LayerCapacityState::Wanted,
            LayerCapacityState::PendingDrop { since: t0() },
            LayerCapacityState::Dropped,
            LayerCapacityState::PendingRestore { since: t0() },
        ] {
            assert_eq!(
                step_layer_capacity_state(prev, None, &cfg(), t0()),
                prev,
                "no-signal must preserve state {prev:?}",
            );
        }
    }

    #[test]
    fn capacity_step_wanted_with_healthy_signal_stays_wanted() {
        let h = health(0.01); // well under 5% threshold
        let s = step_layer_capacity_state(LayerCapacityState::Wanted, Some(&h), &cfg(), t0());
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_wanted_with_over_budget_enters_pending_drop() {
        let h = health(0.10); // 10%, well over 5% threshold
        let now = t0();
        let s = step_layer_capacity_state(LayerCapacityState::Wanted, Some(&h), &cfg(), now);
        assert_eq!(s, LayerCapacityState::PendingDrop { since: now });
    }

    #[test]
    fn capacity_step_pending_drop_with_recovery_cancels_back_to_wanted() {
        // Brief over-budget triggered PendingDrop; signal then
        // recovers anywhere ≤ threshold (not necessarily under
        // recovery threshold — cancelling a pending drop on any
        // improvement is the conservative choice).
        let now = t0();
        let pending = LayerCapacityState::PendingDrop { since: now };
        let later = now + Duration::from_secs(1);
        let h = health(0.04); // ≤ 5% threshold but > 2% recovery
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), later);
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_pending_drop_pre_debounce_holds() {
        let now = t0();
        let pending = LayerCapacityState::PendingDrop { since: now };
        let just_before = now + cfg().drop_debounce - Duration::from_millis(1);
        let h = health(0.10);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), just_before);
        assert_eq!(s, LayerCapacityState::PendingDrop { since: now });
    }

    #[test]
    fn capacity_step_pending_drop_post_debounce_drops() {
        let now = t0();
        let pending = LayerCapacityState::PendingDrop { since: now };
        let post = now + cfg().drop_debounce;
        let h = health(0.10);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), post);
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_continued_loss_stays_dropped() {
        let h = health(0.10);
        let s = step_layer_capacity_state(LayerCapacityState::Dropped, Some(&h), &cfg(), t0());
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_partial_recovery_does_not_restore() {
        // 0.04 is between recovery (0.02) and threshold (0.05).
        // Since we're already Dropped, restoration requires
        // crossing the (lower) recovery threshold — wider hysteresis
        // band prevents oscillation around the threshold.
        let h = health(0.04);
        let s = step_layer_capacity_state(LayerCapacityState::Dropped, Some(&h), &cfg(), t0());
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_clear_recovery_enters_pending_restore() {
        let h = health(0.01); // ≤ 2% recovery
        let now = t0();
        let s = step_layer_capacity_state(LayerCapacityState::Dropped, Some(&h), &cfg(), now);
        assert_eq!(s, LayerCapacityState::PendingRestore { since: now });
    }

    #[test]
    fn capacity_step_pending_restore_pre_debounce_holds() {
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let just_before = now + cfg().restore_debounce - Duration::from_millis(1);
        let h = health(0.01);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), just_before);
        assert_eq!(s, LayerCapacityState::PendingRestore { since: now });
    }

    #[test]
    fn capacity_step_pending_restore_post_debounce_restores_to_wanted() {
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let post = now + cfg().restore_debounce;
        let h = health(0.01);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), post);
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_pending_restore_with_over_budget_returns_to_dropped() {
        // Signal flipped back during pending restore — return to
        // Dropped without restarting the drop debounce (we're
        // already in the dropped equilibrium).
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let later = now + Duration::from_millis(500);
        let h = health(0.10);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), later);
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_pending_restore_gray_band_cancels_back_to_dropped() {
        // **4d.3b review fix regression**: PendingRestore must NOT
        // restore on gray-band loss (between the recovery
        // threshold and the drop threshold). Restore requires the
        // signal to remain clearly `healthy` (≤ recovery threshold)
        // through the full debounce window; any drift back into
        // gray-band cancels the restore.
        //
        // Asymmetric to drop's cancel-on-recovery: drop cancels on
        // any improvement (signal ≤ drop threshold), but restore
        // requires the signal to stay below the wider recovery
        // threshold. Without this, the policy would restore on
        // signals that haven't actually recovered to the wider-
        // hysteresis-band's standard — the same gray-band
        // oscillation the dual-threshold design exists to prevent.
        //
        // Test setup: enter PendingRestore at fraction_lost = 0.01
        // (clearly healthy), then drift to 0.04 (gray-band: above
        // recovery 0.02 but ≤ drop threshold 0.05) at exactly the
        // post-debounce moment. Without this fix the helper would
        // hit the post-debounce arm and restore to Wanted, which
        // is wrong: the signal isn't clearly healthy any more.
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let post = now + cfg().restore_debounce;
        let gray_band = health(0.04);
        let s = step_layer_capacity_state(pending, Some(&gray_band), &cfg(), post);
        assert_eq!(
            s,
            LayerCapacityState::Dropped,
            "gray-band signal during PendingRestore must cancel \
             back to Dropped — restore requires the signal to stay \
             ≤ recovery threshold through the full debounce; got {s:?}"
        );
    }

    #[test]
    fn capacity_step_pending_restore_gray_band_cancels_immediately_pre_debounce() {
        // Same fix, pre-debounce: gray-band signal during the
        // restore-pending window cancels immediately, doesn't wait
        // for the debounce to elapse. Confirms the cancel arm
        // (`!healthy`) takes precedence over the timer arm.
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let pre = now + Duration::from_millis(500);
        let gray_band = health(0.03);
        let s = step_layer_capacity_state(pending, Some(&gray_band), &cfg(), pre);
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    // ----- layer_state_is_wanted -----

    #[test]
    fn layer_state_is_wanted_includes_wanted_and_pending_drop() {
        // Both the steady "Wanted" state AND the in-flight
        // "PendingDrop" state contribute to the wanted set: while a
        // drop is pending, the encoder is still producing the
        // layer, so peers count it as wanted.
        assert!(layer_state_is_wanted(&LayerCapacityState::Wanted));
        assert!(layer_state_is_wanted(&LayerCapacityState::PendingDrop {
            since: Instant::now()
        }));
        assert!(!layer_state_is_wanted(&LayerCapacityState::Dropped));
        assert!(!layer_state_is_wanted(
            &LayerCapacityState::PendingRestore {
                since: Instant::now()
            }
        ));
    }

    // ----- step_aggregate_layer_capacity -----

    fn twcc(loss_fraction: f64) -> crate::display::twcc_tap::TwccHealth {
        // Synthetic TwccHealth for state-machine tests. The state
        // machine reads only `loss_fraction`; other fields are
        // present to satisfy the type but irrelevant.
        crate::display::twcc_tap::TwccHealth {
            at: Instant::now(),
            loss_fraction,
            reported_packets: 100,
            received_packets: ((1.0 - loss_fraction) * 100.0) as u64,
            lost_packets: (loss_fraction * 100.0) as u64,
            last_fb_pkt_count: Some(0),
            batches: 1,
        }
    }

    #[test]
    fn aggregate_no_signal_preserves_state() {
        // None health must never trigger a transition. Load-bearing
        // for new peers (no aggregator snapshot yet) and for
        // transient subscriber lag.
        for prev in [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ] {
            assert_eq!(
                step_aggregate_layer_capacity(prev, None, &cfg(), t0()),
                prev,
                "no-signal must preserve state {prev:?}",
            );
        }
    }

    /// Synthesize an "empty-window" `TwccHealth`: a `Some(_)` value
    /// the aggregator should never publish in practice (it emits
    /// `None` for empty windows by design), but which the state
    /// machine must defensively treat as "no signal" anyway. Used
    /// by the empty-window-preserves-state suite below.
    fn twcc_empty() -> crate::display::twcc_tap::TwccHealth {
        crate::display::twcc_tap::TwccHealth {
            at: Instant::now(),
            loss_fraction: 0.0,
            reported_packets: 0,
            received_packets: 0,
            lost_packets: 0,
            last_fb_pkt_count: None,
            batches: 0,
        }
    }

    /// A pathological `Some(_)` shape we should also ignore: a
    /// non-zero `batches` count with `reported_packets == 0`. Could
    /// arise if a future code path counted "events seen" without
    /// checking whether they carried any reported packets.
    fn twcc_batches_no_reports() -> crate::display::twcc_tap::TwccHealth {
        crate::display::twcc_tap::TwccHealth {
            at: Instant::now(),
            loss_fraction: 0.0,
            reported_packets: 0,
            received_packets: 0,
            lost_packets: 0,
            last_fb_pkt_count: Some(7),
            batches: 3,
        }
    }

    #[test]
    fn aggregate_empty_window_preserves_state() {
        // The aggregator publishes `None` on empty windows by
        // design. This guard is defense-in-depth: even if a
        // `Some(empty_health)` reaches the state machine, every
        // state must short-circuit to `prev`. Silence is not
        // recovery; it must not advance the cascade.
        //
        // Specifically asserts the user-listed invariants:
        //   - OnlyFloor + empty window stays OnlyFloor
        //   - TopPaused + empty window stays TopPaused
        //   - Pending pause/resume states are not advanced by
        //     empty windows
        let states = [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ];
        for prev in states {
            // Both empty-Some shapes must preserve every state.
            for empty in [twcc_empty(), twcc_batches_no_reports()] {
                // Even after a full debounce window: empty must
                // not let pending states advance.
                let after_debounce = t0() + cfg().drop_debounce;
                assert_eq!(
                    step_aggregate_layer_capacity(prev, Some(&empty), &cfg(), after_debounce,),
                    prev,
                    "empty-window Some({empty:?}) must preserve state {prev:?}",
                );
            }
        }
    }

    #[test]
    fn aggregate_pending_pause_top_does_not_advance_on_empty_window() {
        // Specifically called out by the user: empty windows must
        // not let a pending-pause timer advance to the paused
        // state. Even at exactly drop_debounce, an empty reading
        // must keep the timer pending.
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: start },
            Some(&twcc_empty()),
            &cfg(),
            start + cfg().drop_debounce,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingPauseTop { since: start },
            "empty window must not advance the drop debounce",
        );
    }

    #[test]
    fn aggregate_pending_resume_mid_does_not_advance_on_empty_window() {
        // Symmetric to the pause case: empty windows must not let
        // a pending-resume timer advance, since silence is not
        // recovery either.
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingResumeMid { since: start },
            Some(&twcc_empty()),
            &cfg(),
            start + cfg().restore_debounce,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingResumeMid { since: start },
            "empty window must not advance the restore debounce",
        );
    }

    #[test]
    fn aggregate_all_wanted_enters_pending_on_over_budget() {
        // 0.10 > threshold 0.05 → PendingPauseTop with `since: now`.
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::AllUpperWanted,
            Some(&twcc(0.10)),
            &cfg(),
            now,
        );
        assert_eq!(next, AggregateLayerCapacity::PendingPauseTop { since: now });
    }

    #[test]
    fn aggregate_pending_pause_top_cancels_on_recovery_below_threshold() {
        // Mid-debounce recovery must cancel back to AllUpperWanted.
        // Cancel on `!over_budget`, not `healthy` — same rationale
        // as `step_layer_capacity_state`'s PendingDrop arm.
        let now = t0();
        // 0.04 ≤ threshold 0.05 (and is in the gray band) — should
        // cancel even though it's not `healthy`.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: now },
            Some(&twcc(0.04)),
            &cfg(),
            now + Duration::from_secs(2),
        );
        assert_eq!(next, AggregateLayerCapacity::AllUpperWanted);
    }

    #[test]
    fn aggregate_pending_pause_top_advances_at_drop_debounce() {
        // After exactly drop_debounce of sustained over-budget,
        // transition to TopPaused.
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: start },
            Some(&twcc(0.10)),
            &cfg(),
            start + cfg().drop_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_pending_pause_top_holds_before_debounce_elapses() {
        let start = t0();
        let just_before = start + cfg().drop_debounce - Duration::from_millis(1);
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: start },
            Some(&twcc(0.10)),
            &cfg(),
            just_before,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingPauseTop { since: start }
        );
    }

    #[test]
    fn aggregate_top_paused_cascades_into_pending_pause_mid() {
        // Once Top is paused, sustained over-budget kicks the
        // mid-cascade. NOT a parallel evaluation of mid against
        // its own debounce — the cascade waits until top has
        // settled into TopPaused before considering mid.
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::TopPaused,
            Some(&twcc(0.10)),
            &cfg(),
            now,
        );
        assert_eq!(next, AggregateLayerCapacity::PendingPauseMid { since: now });
    }

    #[test]
    fn aggregate_top_paused_starts_resume_on_clean_recovery() {
        // Cleanly healthy (≤ recovery threshold) → start counting
        // down to resume top. NOT triggered by mere `!over_budget`
        // (that's the gray band) — TopPaused must see clearly
        // healthy.
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::TopPaused,
            Some(&twcc(0.01)),
            &cfg(),
            now,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingResumeTop { since: now }
        );
    }

    #[test]
    fn aggregate_top_paused_holds_in_gray_band() {
        // 0.04 is between recovery (0.02) and threshold (0.05) —
        // should stay TopPaused (no resume countdown, no further
        // pause cascade). This is the hysteresis band that prevents
        // flapping at the boundary.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::TopPaused,
            Some(&twcc(0.04)),
            &cfg(),
            t0(),
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_pending_pause_mid_advances_at_drop_debounce() {
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseMid { since: start },
            Some(&twcc(0.10)),
            &cfg(),
            start + cfg().drop_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::OnlyFloor);
    }

    #[test]
    fn aggregate_pending_pause_mid_cancels_on_recovery() {
        // Same cancel-on-improvement semantic as PendingPauseTop:
        // any drop below threshold (not necessarily into healthy)
        // cancels, returning to TopPaused.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            Some(&twcc(0.04)),
            &cfg(),
            t0() + Duration::from_secs(2),
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_only_floor_starts_resume_on_clean_recovery() {
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::OnlyFloor,
            Some(&twcc(0.01)),
            &cfg(),
            now,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingResumeMid { since: now }
        );
    }

    #[test]
    fn aggregate_only_floor_stays_in_gray_band() {
        // Once OnlyFloor, the gray band keeps us pinned — restore
        // requires cleanly healthy.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::OnlyFloor,
            Some(&twcc(0.04)),
            &cfg(),
            t0(),
        );
        assert_eq!(next, AggregateLayerCapacity::OnlyFloor);
    }

    #[test]
    fn aggregate_pending_resume_mid_advances_at_restore_debounce() {
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingResumeMid { since: start },
            Some(&twcc(0.01)),
            &cfg(),
            start + cfg().restore_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_pending_resume_mid_cancels_on_regression() {
        // Symmetric to PendingResume in the per-RID machine:
        // restore requires sustained healthy. ANY drift out of
        // healthy (gray band OR over-budget) cancels back to
        // OnlyFloor.
        for fraction in [0.04, 0.10] {
            let next = step_aggregate_layer_capacity(
                AggregateLayerCapacity::PendingResumeMid { since: t0() },
                Some(&twcc(fraction)),
                &cfg(),
                t0() + Duration::from_millis(500),
            );
            assert_eq!(
                next,
                AggregateLayerCapacity::OnlyFloor,
                "regression to {fraction} must cancel pending resume",
            );
        }
    }

    #[test]
    fn aggregate_pending_resume_top_advances_at_restore_debounce() {
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingResumeTop { since: start },
            Some(&twcc(0.01)),
            &cfg(),
            start + cfg().restore_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::AllUpperWanted);
    }

    #[test]
    fn aggregate_pending_resume_top_cancels_on_regression() {
        for fraction in [0.04, 0.10] {
            let next = step_aggregate_layer_capacity(
                AggregateLayerCapacity::PendingResumeTop { since: t0() },
                Some(&twcc(fraction)),
                &cfg(),
                t0() + Duration::from_millis(500),
            );
            assert_eq!(
                next,
                AggregateLayerCapacity::TopPaused,
                "regression to {fraction} must cancel pending resume",
            );
        }
    }

    #[test]
    fn aggregate_full_cascade_drop_then_recover_in_reverse_order() {
        // Walk the full state machine: AllUpperWanted →
        // PendingPauseTop → TopPaused → PendingPauseMid →
        // OnlyFloor → PendingResumeMid → TopPaused →
        // PendingResumeTop → AllUpperWanted. This is the
        // f-then-h drop, h-then-f recovery directive verbatim.
        let cfg = cfg();
        let mut now = t0();
        let mut state = AggregateLayerCapacity::AllUpperWanted;
        let high = twcc(0.10);
        let low = twcc(0.01);

        // 1. AllUpperWanted + over-budget → PendingPauseTop
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingPauseTop { .. }
        ));

        // 2. ... drop_debounce later → TopPaused
        now += cfg.drop_debounce;
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::TopPaused);

        // 3. Still over-budget → PendingPauseMid
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingPauseMid { .. }
        ));

        // 4. ... another drop_debounce → OnlyFloor
        now += cfg.drop_debounce;
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::OnlyFloor);

        // 5. Recovery → PendingResumeMid
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingResumeMid { .. }
        ));

        // 6. ... restore_debounce later → TopPaused (mid resumed)
        now += cfg.restore_debounce;
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::TopPaused);

        // 7. Still healthy → PendingResumeTop
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingResumeTop { .. }
        ));

        // 8. ... restore_debounce later → AllUpperWanted (full
        //    recovery)
        now += cfg.restore_debounce;
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::AllUpperWanted);
    }

    #[test]
    fn aggregate_no_flap_in_gray_band_oscillation() {
        // Loss oscillating between 0.04 and 0.06 around the
        // threshold (0.05) within a single drop_debounce window
        // must not trigger a pause. The cancel-on-improvement
        // semantic resets the PendingPauseTop timer back to
        // AllUpperWanted on every dip below threshold.
        let cfg = cfg();
        let mut now = t0();
        let mut state = AggregateLayerCapacity::AllUpperWanted;
        for tick in 0..10 {
            let fraction = if tick % 2 == 0 { 0.06 } else { 0.04 };
            state = step_aggregate_layer_capacity(state, Some(&twcc(fraction)), &cfg, now);
            now += Duration::from_millis(500);
        }
        // Through 5 seconds of oscillation, should never reach
        // TopPaused — the timer keeps getting cancelled.
        assert!(
            matches!(
                state,
                AggregateLayerCapacity::AllUpperWanted
                    | AggregateLayerCapacity::PendingPauseTop { .. }
            ),
            "oscillation must not advance past PendingPauseTop, got {state:?}"
        );
    }

    // ----- aggregate_state_wanted_upper_layers -----

    #[test]
    fn aggregate_projection_all_wanted_returns_full_upper_set() {
        let top = SimulcastRid::full();
        let mid = SimulcastRid::half();
        let upper = [top.clone(), mid.clone()];
        for state in [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &upper);
            assert_eq!(wanted, HashSet::from([top.clone(), mid.clone()]));
        }
    }

    #[test]
    fn aggregate_projection_top_paused_drops_top_keeps_rest() {
        let top = SimulcastRid::full();
        let mid = SimulcastRid::half();
        let upper = [top.clone(), mid.clone()];
        for state in [
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &upper);
            assert_eq!(wanted, HashSet::from([mid.clone()]));
        }
    }

    #[test]
    fn aggregate_projection_only_floor_returns_empty_upper() {
        let top = SimulcastRid::full();
        let mid = SimulcastRid::half();
        let upper = [top.clone(), mid.clone()];
        for state in [
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &upper);
            assert_eq!(wanted, HashSet::new());
        }
    }

    /// 1-upper-layer cascade — e.g., `vp8_simulcast` at very small
    /// dims emits only `full + half` with `half` as floor, leaving
    /// `full` as the sole upper layer. The state machine still has
    /// 7 states (it doesn't care about layer count) but the
    /// projection collapses gracefully:
    ///
    /// - `AllUpperWanted` / `PendingPauseTop` → {full}
    /// - everything else → {} (no `mid` to keep)
    #[test]
    fn aggregate_projection_one_upper_layer_pauses_top_alone() {
        let top = SimulcastRid::full();
        let upper = [top.clone()];

        // All-upper-wanted states: full set.
        for state in [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &upper);
            assert_eq!(
                wanted,
                HashSet::from([top.clone()]),
                "1-layer cascade: pre-pause states must keep top",
            );
        }

        // All other states: no mid to fall back on, everything past
        // PendingPauseTop projects to empty.
        for state in [
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &upper);
            assert_eq!(
                wanted,
                HashSet::new(),
                "1-layer cascade: post-pause states must drop top: {state:?}",
            );
        }
    }

    #[test]
    fn aggregate_projection_zero_upper_layers_always_empty() {
        // Floor-only pool layout: nothing for the cascade to act on.
        // Every state projects to empty regardless.
        let upper: [SimulcastRid; 0] = [];
        for state in [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &upper);
            assert_eq!(wanted, HashSet::new());
        }
    }

    // ----- compose_effective_wanted -----

    fn vp8_three_layer_set() -> Vec<SimulcastRid> {
        vec![
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ]
    }

    fn full_three_layer_union() -> HashSet<SimulcastRid> {
        vp8_three_layer_set().into_iter().collect()
    }

    /// User-listed test #1: TWCC says pause full, RR has no signal
    /// → full stays paused; RR does not resume it.
    ///
    /// The intersection rule means RR's "no signal → vote for
    /// everything" can't override TWCC's "pause this." Pause wins.
    #[test]
    fn compose_twcc_pauses_rr_no_signal_full_stays_paused() {
        let current = vp8_three_layer_set();
        // TWCC excludes `full` (sustained loss → TopPaused →
        // projection drops top + floor union).
        let twcc_union: HashSet<SimulcastRid> = [SimulcastRid::half(), SimulcastRid::quarter()]
            .into_iter()
            .collect();
        // RR has no signal: each peer's per-RID state defaults to
        // Wanted, so peer wants all layers; union is the full set.
        let rr_union = full_three_layer_union();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &HashSet::new(),
            &full_three_layer_union(),
        );

        assert!(
            !effective.contains(&SimulcastRid::full()),
            "TWCC's pause must hold even when RR has no opinion; \
             effective = {effective:?}",
        );
        assert!(effective.contains(&SimulcastRid::half()));
        assert!(effective.contains(&SimulcastRid::quarter()));
    }

    /// User-listed test #2: presence zero-peer debounce pauses
    /// all layers even if TWCC/RR would otherwise want them.
    ///
    /// `presence_active = false` short-circuits to empty regardless
    /// of the other unions — this is the post-debounce idle state.
    #[test]
    fn compose_presence_idle_pauses_everything() {
        let current = vp8_three_layer_set();
        // Both per-peer policies vote for everything (would be the
        // case if TWCC and RR both saw healthy signals or no
        // signals across all peers).
        let twcc_union = full_three_layer_union();
        let rr_union = full_three_layer_union();

        let effective = compose_effective_wanted(
            false,
            &twcc_union,
            &rr_union,
            &current,
            &HashSet::new(),
            &full_three_layer_union(),
        );

        assert_eq!(
            effective,
            HashSet::new(),
            "presence-idle must short-circuit to empty regardless \
             of TWCC/RR votes; effective = {effective:?}",
        );
    }

    /// User-listed test #3: peer returns after idle → effective
    /// wanted resumes per fresh-default policy state, not stale.
    ///
    /// The pruning behaviour (clearing per-peer state when peers
    /// become empty) lives in [`spawn_layer_policy_coordinator`].
    /// At the composition layer, we verify the consequence: when
    /// the per-policy unions reflect fresh-default state (full set
    /// from each side, since `AggregateLayerCapacity::AllUpperWanted`
    /// projects to all upper + floor, and `LayerCapacityState::Wanted`
    /// keeps every RID), the effective wanted set is the full
    /// current rid set — i.e., everything resumes.
    #[test]
    fn compose_fresh_default_state_resumes_all_layers() {
        let current = vp8_three_layer_set();
        let twcc_union = full_three_layer_union();
        let rr_union = full_three_layer_union();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &HashSet::new(),
            &full_three_layer_union(),
        );

        assert_eq!(
            effective,
            full_three_layer_union(),
            "fresh-default state on both per-peer policies must \
             resume every layer; effective = {effective:?}",
        );
    }

    /// User-listed test #4: empty TWCC window votes "no
    /// restriction / no transition," never recovery.
    ///
    /// Empty windows are short-circuited at two places: the
    /// aggregator publishes `None` rather than `Some(empty_health)`,
    /// and `step_aggregate_layer_capacity` guards on `batches == 0
    /// || reported_packets == 0`. Both arms preserve the previous
    /// state. At the composition layer, that surfaces as: the TWCC
    /// union for the empty-window case equals the previous
    /// non-empty-window union — silence doesn't drift the wanted
    /// set toward recovery.
    ///
    /// This test verifies the consequence: if TWCC's union still
    /// excludes `full` (state was TopPaused before silence) and
    /// the silence doesn't change that, effective continues to
    /// exclude `full`.
    #[test]
    fn compose_empty_twcc_window_does_not_resume_paused_layer() {
        let current = vp8_three_layer_set();
        // TWCC was at TopPaused; full is not in the wanted set.
        // After an empty window, the state preserves (per existing
        // empty-window guards), so the union still excludes full.
        let twcc_union: HashSet<SimulcastRid> = [SimulcastRid::half(), SimulcastRid::quarter()]
            .into_iter()
            .collect();
        let rr_union = full_three_layer_union();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &HashSet::new(),
            &full_three_layer_union(),
        );

        assert!(
            !effective.contains(&SimulcastRid::full()),
            "empty TWCC window must not resume a paused layer — \
             silence is not recovery; effective = {effective:?}",
        );
    }

    /// User-listed test #5: current rid changes after resize → diff
    /// against actual pool state, regenerated layers are re-paused
    /// if policy still excludes them.
    ///
    /// Composition is bounded by `current_rids`: a layer present
    /// in the per-policy unions but absent from `current_rids`
    /// (e.g., a layer the previous tick had, before the pool
    /// regenerated to a different layout) does not appear in
    /// `effective_wanted`. This is what makes the diff-against-
    /// pool-state pattern correct: even if the policy state still
    /// references a removed RID, the composition surfaces only
    /// what `current_rids` says is live this tick.
    #[test]
    fn compose_bounded_by_current_rids_after_resize() {
        // Resize shrunk the pool from {full, half, quarter} to
        // {full, half} — `quarter` is no longer a current rid.
        let current = vec![SimulcastRid::full(), SimulcastRid::half()];
        // Per-policy unions still vote for `quarter` (stale state
        // from before the resize). Composition must not include
        // `quarter` in the effective set.
        let twcc_union = full_three_layer_union();
        let rr_union = full_three_layer_union();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &HashSet::new(),
            &full_three_layer_union(),
        );

        assert!(
            !effective.contains(&SimulcastRid::quarter()),
            "compose must filter to current_rids; effective = {effective:?}",
        );
        assert!(effective.contains(&SimulcastRid::full()));
        assert!(effective.contains(&SimulcastRid::half()));
        assert_eq!(effective.len(), 2);
    }

    /// Intersection semantics: a layer must appear in BOTH unions
    /// to land in effective. A layer in twcc_union but missing
    /// from rr_union (e.g., a per-RID RR signal genuinely flagged
    /// it as Dropped, hypothetically) is excluded.
    #[test]
    fn compose_intersection_excludes_layer_missing_from_either_union() {
        let current = vp8_three_layer_set();
        let twcc_union = full_three_layer_union();
        // RR genuinely says half is Dropped (hypothetical — RR
        // doesn't fire on the rtc 0.9 stack today, but the
        // composition logic must still respect it).
        let rr_union: HashSet<SimulcastRid> = [SimulcastRid::full(), SimulcastRid::quarter()]
            .into_iter()
            .collect();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &HashSet::new(),
            &full_three_layer_union(),
        );

        assert!(!effective.contains(&SimulcastRid::half()));
        assert!(effective.contains(&SimulcastRid::full()));
        assert!(effective.contains(&SimulcastRid::quarter()));
    }

    /// Both unions empty (e.g., no peers + per-policy "no signal"
    /// fallbacks somehow disagree, defensively): effective is
    /// empty when either union is empty, since intersection with
    /// empty is empty. This is a defensive case — production
    /// callers should always pass non-empty unions when peers
    /// are present (each policy votes the full set when it has
    /// no signal), but the function shape doesn't enforce that.
    #[test]
    fn compose_empty_union_yields_empty_effective() {
        let current = vp8_three_layer_set();
        let empty: HashSet<SimulcastRid> = HashSet::new();
        let full = full_three_layer_union();

        assert_eq!(
            compose_effective_wanted(
                true,
                &empty,
                &full,
                &current,
                &HashSet::new(),
                &full_three_layer_union()
            ),
            HashSet::new(),
        );
        assert_eq!(
            compose_effective_wanted(
                true,
                &full,
                &empty,
                &current,
                &HashSet::new(),
                &full_three_layer_union()
            ),
            HashSet::new(),
        );
    }

    // ----- #57: pinned-layer regression tests --------------------------------

    /// Acceptance #57.1: a single-RID `[f]` peer pinning `f` keeps
    /// `f` in the effective wanted set even when the TWCC policy
    /// has voted to drop it under high loss.
    ///
    /// Setup: TWCC union excludes `f` (only `h;q` survive, the
    /// classic post-cascade state); RR union is full (no per-RID
    /// signal). Without the pin the intersection drops `f` →
    /// `[h, q]` → `PauseLayer(f)` would fire. With the pin, `f`
    /// is unioned back in.
    #[test]
    fn compose_pinned_full_overrides_twcc_pause() {
        let current = vp8_three_layer_set();
        let twcc_union: HashSet<SimulcastRid> = [SimulcastRid::half(), SimulcastRid::quarter()]
            .into_iter()
            .collect();
        let rr_union = full_three_layer_union();
        let pinned: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::full()).collect();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &pinned,
            &full_three_layer_union(),
        );
        assert!(
            effective.contains(&SimulcastRid::full()),
            "pinned `f` must survive TWCC pause vote, got {effective:?}",
        );
        assert!(effective.contains(&SimulcastRid::half()));
        assert!(effective.contains(&SimulcastRid::quarter()));
    }

    /// Acceptance #57.2: a multi-RID local peer (no entries in
    /// the pin set) still allows cascade pause `f` then `h`. This
    /// is the "no regression for the common local DisplaySlot
    /// path" check — the pin set is empty when no single-RID
    /// peers exist, so the standard intersection result holds
    /// verbatim.
    #[test]
    fn compose_no_pin_allows_cascade_pause() {
        let current = vp8_three_layer_set();
        // TWCC has cascaded `f`, then `h` — only floor `q` survives.
        let twcc_union: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();
        let rr_union = full_three_layer_union();
        let no_pin: HashSet<SimulcastRid> = HashSet::new();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &no_pin,
            &full_three_layer_union(),
        );
        let expected: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();
        assert_eq!(
            effective, expected,
            "without pin, cascaded TWCC pauses both `f` and `h`",
        );
    }

    /// Acceptance #57.3: when a single-RID peer disconnects,
    /// the pin is gone (the coordinator re-derives the pin set
    /// each tick from `current_peers.values()`), and the policy
    /// can pause the previously-pinned layer normally.
    ///
    /// Tick 1: pin = {f}, TWCC says drop f → effective contains f
    /// (from #57.1). Tick 2 (peer disconnected): pin = {}, same
    /// TWCC vote → effective drops f. The compose function is
    /// pure — this test just confirms the empty-pin case behaves
    /// like the pre-#57 baseline.
    #[test]
    fn compose_pin_removed_on_disconnect_restores_pause_authority() {
        let current = vp8_three_layer_set();
        let twcc_union: HashSet<SimulcastRid> = [SimulcastRid::half(), SimulcastRid::quarter()]
            .into_iter()
            .collect();
        let rr_union = full_three_layer_union();

        // While pinned: f survives TWCC drop.
        let with_pin: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::full()).collect();
        let pinned_effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &with_pin,
            &full_three_layer_union(),
        );
        assert!(pinned_effective.contains(&SimulcastRid::full()));

        // After the peer disconnects, pin set is empty. Same
        // TWCC vote → f drops normally. This is identical to the
        // baseline `compose_twcc_pauses_rr_no_signal_full_stays_paused`
        // expectation; restated here to make the disconnect
        // semantic explicit.
        let no_pin: HashSet<SimulcastRid> = HashSet::new();
        let unpinned_effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &no_pin,
            &full_three_layer_union(),
        );
        assert!(
            !unpinned_effective.contains(&SimulcastRid::full()),
            "after pin removal, TWCC pause authority restored — \
             `f` must drop, got {unpinned_effective:?}",
        );
        assert_eq!(
            unpinned_effective,
            [SimulcastRid::half(), SimulcastRid::quarter()]
                .into_iter()
                .collect::<HashSet<_>>(),
        );
    }

    /// Defensive: pinning a RID the pool isn't producing right now
    /// (e.g. stale pin from before a `pool.on_resize` shrunk the
    /// layer set) is silently bounded by `current_rids` — it does
    /// not resurrect a vanished layer in the wanted set.
    #[test]
    fn compose_pin_bounded_by_current_rids() {
        // current_rids = [h, q] only (full removed by hypothetical
        // resize). Pin still says {f}. Should NOT include f.
        let current = vec![SimulcastRid::half(), SimulcastRid::quarter()];
        let twcc_union = full_three_layer_union();
        let rr_union = full_three_layer_union();
        let stale_pin: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::full()).collect();

        let effective = compose_effective_wanted(
            true,
            &twcc_union,
            &rr_union,
            &current,
            &stale_pin,
            &full_three_layer_union(),
        );
        assert!(
            !effective.contains(&SimulcastRid::full()),
            "stale pin must be bounded by current_rids, got {effective:?}",
        );
        assert_eq!(
            effective,
            [SimulcastRid::half(), SimulcastRid::quarter()]
                .into_iter()
                .collect::<HashSet<_>>(),
        );
    }

    /// Defensive: presence_active=false short-circuits to empty,
    /// pin set notwithstanding. Idle pause is the strongest
    /// signal in the composer — a pinned single-RID peer that
    /// disconnects cleanly enters the pin removal path (above);
    /// the idle short-circuit only fires after every peer is
    /// gone, so there's no real conflict, but the contract
    /// should be explicit.
    #[test]
    fn compose_idle_pause_overrides_even_pinned_layer() {
        let current = vp8_three_layer_set();
        let full_union = full_three_layer_union();
        let pinned: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::full()).collect();

        let effective = compose_effective_wanted(
            false,
            &full_union,
            &full_union,
            &current,
            &pinned,
            &full_three_layer_union(),
        );
        assert!(
            effective.is_empty(),
            "presence_active=false must short-circuit even with pin, \
             got {effective:?}",
        );
    }

    // ----- #48: demanded-RID upper bound ---------------------------------

    /// **#48 acceptance #1**: federated `q`-only peer demands `{q}`.
    /// Even though no loss has been observed (TWCC + RR votes both
    /// say "all layers wanted") and no pin is needed, `f` and `h`
    /// MUST drop out of the effective set because no live peer
    /// can decode them. CPU regression fix: 3-encoder waste was
    /// the cause of the 358% CPU on the macOS primary.
    #[test]
    fn compose_demand_q_only_drops_unconsumed_f_and_h() {
        let current = vp8_three_layer_set();
        let no_loss = full_three_layer_union();
        let pinned: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();
        let demanded: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();

        let effective =
            compose_effective_wanted(true, &no_loss, &no_loss, &current, &pinned, &demanded);
        assert_eq!(
            effective,
            std::iter::once(SimulcastRid::quarter()).collect::<HashSet<_>>(),
            "q-only demand must drop f and h regardless of loss state",
        );
    }

    /// **#48 acceptance #2**: an opt-in multi-RID peer (offer carries
    /// `a=simulcast:recv f;h;q`) demands `{f,h,q}`. All three layers
    /// stay active (assuming no loss). Default single-RID viewers
    /// (local DisplaySlot post-#58 demands `f` only; federated
    /// post-#48 demands `q` only) exercise the narrower-demand
    /// scenarios in adjacent tests.
    #[test]
    fn compose_demand_multi_rid_opt_in_keeps_all_layers() {
        let current = vp8_three_layer_set();
        let no_loss = full_three_layer_union();
        let no_pin: HashSet<SimulcastRid> = HashSet::new();
        let demanded = full_three_layer_union();

        let effective =
            compose_effective_wanted(true, &no_loss, &no_loss, &current, &no_pin, &demanded);
        assert_eq!(effective, full_three_layer_union());
    }

    /// **#48 acceptance #3**: mixed local + federated peers.
    /// Demand is the union: `{f,h,q}` (local) ∪ `{q}` (federated)
    /// = `{f,h,q}`. Local gets all layers; federated gets q from
    /// the same union. Single set, no per-peer routing needed at
    /// the encoder level.
    #[test]
    fn compose_demand_mixed_local_and_federated_unions_to_full() {
        let current = vp8_three_layer_set();
        let no_loss = full_three_layer_union();
        let pinned: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();
        let mut demanded = full_three_layer_union();
        demanded.insert(SimulcastRid::quarter());

        let effective =
            compose_effective_wanted(true, &no_loss, &no_loss, &current, &pinned, &demanded);
        assert_eq!(effective, full_three_layer_union());
    }

    /// **#48 acceptance #4**: zero peers ⇒ empty demand ⇒ empty
    /// effective. Encoders pause immediately, no debounce-window
    /// of wasted CPU. The presence-policy short-circuit at
    /// `presence_active=false` still applies, but the demanded
    /// bound also independently fires — defense in depth.
    #[test]
    fn compose_zero_peers_empty_demand_pauses_all_layers() {
        let current = vp8_three_layer_set();
        let no_loss = full_three_layer_union();
        let no_pin: HashSet<SimulcastRid> = HashSet::new();
        let no_demand: HashSet<SimulcastRid> = HashSet::new();

        let effective =
            compose_effective_wanted(true, &no_loss, &no_loss, &current, &no_pin, &no_demand);
        assert!(
            effective.is_empty(),
            "empty demand must pause all layers, got {effective:?}",
        );
    }

    /// **#48 + #57 interplay**: demanded bound never strips a
    /// pinned RID — by construction, a single-RID peer's pin IS
    /// in their own contribution to the demand union. Pinned
    /// `q` for federated; demanded includes `q` (same peer);
    /// even under high loss the effective set keeps `q`.
    #[test]
    fn compose_pin_and_demand_invariants_compose_safely() {
        let current = vp8_three_layer_set();
        let twcc_union: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();
        let rr_union = full_three_layer_union();
        let pinned: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();
        let demanded: HashSet<SimulcastRid> = std::iter::once(SimulcastRid::quarter()).collect();

        let effective =
            compose_effective_wanted(true, &twcc_union, &rr_union, &current, &pinned, &demanded);
        assert_eq!(
            effective,
            std::iter::once(SimulcastRid::quarter()).collect::<HashSet<_>>(),
            "pin + demand must both leave q active",
        );
    }

    // ----- spawn_layer_policy_coordinator -----

    /// **Spawn-loop smoke test**: drive the coordinator through one
    /// `PAUSE_DEBOUNCE` window with no peers and verify it emits
    /// the expected `PauseLayer` actions when the presence policy
    /// transitions to `Idle`.
    ///
    /// What this proves end-to-end (beyond what the pure compose
    /// tests cover):
    ///
    /// 1. `transition` actually advances inside the spawn loop —
    ///    Active → IdlePending → Idle (presence policy fires).
    /// 2. The diff is taken against `is_layer_paused`'s actual
    ///    pool state (not against an internal `last_applied`).
    /// 3. `on_action` is invoked once per layer the diff says
    ///    needs to flip.
    ///
    /// Uses a recording closure on `on_action` instead of a real
    /// `EncoderPool` — the spawn loop's own behaviour is what
    /// we're validating, not the pool. Generous timeout matches
    /// the deleted `spawn_zero_peer_aggregator` smoke test
    /// pattern: the action only has to land *eventually* within
    /// `PAUSE_DEBOUNCE + 5s`, not on any specific tick. Tokio's
    /// mock clock doesn't advance `Instant`, so test runtimes
    /// under load can drift past `PAUSE_DEBOUNCE` by a tick or
    /// two; the deadline absorbs that.
    #[tokio::test]
    async fn spawn_coordinator_pauses_all_layers_after_zero_peer_debounce() {
        use std::sync::Mutex as StdMutex;

        let peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let recorded: Arc<StdMutex<Vec<CapacityAction>>> = Arc::new(StdMutex::new(Vec::new()));
        let recorded_for_closure = Arc::clone(&recorded);
        let on_action: Box<dyn Fn(CapacityAction) + Send + Sync> = Box::new(move |a| {
            recorded_for_closure.lock().unwrap().push(a);
        });
        let get_current_rids: Box<dyn Fn() -> Vec<SimulcastRid> + Send + Sync> =
            Box::new(|| vp8_three_layer_set());
        // Pool reports every layer ACTIVE — the diff after the
        // presence policy fires Idle must produce a PauseLayer
        // for each.
        let is_layer_paused: Box<dyn Fn(&SimulcastRid) -> Option<bool> + Send + Sync> =
            Box::new(|_| Some(false));

        let shutdown = CancellationToken::new();
        let handle = spawn_layer_policy_coordinator(
            Arc::clone(&peers),
            get_current_rids,
            is_layer_paused,
            on_action,
            CapacityPolicyConfig::default(),
            shutdown.clone(),
        );

        // Poll with a generous timeout. PAUSE_DEBOUNCE + 5s of
        // tolerance handles tick drift on a loaded runtime; we
        // exit the loop as soon as we see all three actions
        // (or hit the deadline and panic).
        let deadline = Instant::now() + PAUSE_DEBOUNCE + Duration::from_secs(5);
        loop {
            if recorded.lock().unwrap().len() >= 3 {
                break;
            }
            if Instant::now() >= deadline {
                let actions = recorded.lock().unwrap().clone();
                shutdown.cancel();
                let _ = handle.await;
                panic!(
                    "expected 3 PauseLayer actions within PAUSE_DEBOUNCE + 5s \
                     ({}s total); got {actions:?}",
                    (PAUSE_DEBOUNCE + Duration::from_secs(5)).as_secs(),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let actions = recorded.lock().unwrap().clone();
        // diff_wanted_aggregate iterates `current_rids` in spec
        // order (descending bitrate: full, half, quarter), so the
        // emitted action sequence is deterministic.
        assert_eq!(
            actions,
            vec![
                CapacityAction::PauseLayer(SimulcastRid::full()),
                CapacityAction::PauseLayer(SimulcastRid::half()),
                CapacityAction::PauseLayer(SimulcastRid::quarter()),
            ],
            "expected one PauseLayer per current_rid in spec order",
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ----- per_peer_wanted_layers -----

    #[test]
    fn per_peer_wanted_layers_filters_to_wanted_states() {
        let now = Instant::now();
        let mut states: HashMap<SimulcastRid, LayerCapacityState> = HashMap::new();
        states.insert(SimulcastRid::full(), LayerCapacityState::Wanted);
        states.insert(SimulcastRid::half(), LayerCapacityState::Dropped);
        states.insert(
            SimulcastRid::quarter(),
            LayerCapacityState::PendingDrop { since: now },
        );

        let wanted = per_peer_wanted_layers(&states);
        assert_eq!(wanted.len(), 2);
        assert!(wanted.contains(&SimulcastRid::full()));
        assert!(wanted.contains(&SimulcastRid::quarter()));
        assert!(!wanted.contains(&SimulcastRid::half()));
    }

    #[test]
    fn per_peer_wanted_layers_empty_state_map_returns_empty_set() {
        let states: HashMap<SimulcastRid, LayerCapacityState> = HashMap::new();
        assert!(per_peer_wanted_layers(&states).is_empty());
    }

    // ----- aggregate_wanted_layers -----

    #[test]
    fn aggregate_wanted_layers_unions_per_peer_sets() {
        let peer_a: HashSet<SimulcastRid> = [SimulcastRid::full(), SimulcastRid::quarter()]
            .into_iter()
            .collect();
        let peer_b: HashSet<SimulcastRid> = [SimulcastRid::half(), SimulcastRid::quarter()]
            .into_iter()
            .collect();
        let agg = aggregate_wanted_layers(vec![peer_a, peer_b]);
        // Union: full ∪ half ∪ quarter = all three.
        assert_eq!(agg.len(), 3);
        assert!(agg.contains(&SimulcastRid::full()));
        assert!(agg.contains(&SimulcastRid::half()));
        assert!(agg.contains(&SimulcastRid::quarter()));
    }

    #[test]
    fn aggregate_wanted_layers_empty_input_returns_empty_set() {
        let agg: HashSet<SimulcastRid> =
            aggregate_wanted_layers(std::iter::empty::<HashSet<SimulcastRid>>());
        assert!(agg.is_empty());
    }

    #[test]
    fn aggregate_wanted_layers_one_peer_single_set() {
        let only_full: HashSet<SimulcastRid> = [SimulcastRid::full()].into_iter().collect();
        let agg = aggregate_wanted_layers(vec![only_full.clone()]);
        assert_eq!(agg, only_full);
    }

    // -----------------------------------------------------------------
    // Phase 4d.3c: diff_wanted_aggregate + spawn smoke test
    // -----------------------------------------------------------------

    fn vp8_non_floor_rids() -> Vec<SimulcastRid> {
        // VP8 simulcast: full / half / quarter (descending bitrate);
        // floor = quarter; non-floor = [full, half] in spec order.
        vec![SimulcastRid::full(), SimulcastRid::half()]
    }

    #[test]
    fn diff_wanted_no_change_no_actions() {
        // Steady state: aggregate matches what was last applied.
        // Test all four "no change" cases to ensure the diff
        // genuinely respects equality (not just non-empty intersection).
        for set in [
            HashSet::<SimulcastRid>::new(),
            [SimulcastRid::full()].into_iter().collect(),
            [SimulcastRid::half()].into_iter().collect(),
            [SimulcastRid::full(), SimulcastRid::half()]
                .into_iter()
                .collect(),
        ] {
            let actions = diff_wanted_aggregate(&set, &set, &vp8_non_floor_rids());
            assert!(
                actions.is_empty(),
                "no-change diff fired actions for {set:?}: {actions:?}",
            );
        }
    }

    #[test]
    fn diff_wanted_layer_dropped_fires_pause_action() {
        // full was applied, now no longer wanted. Pause action fires.
        let prev: HashSet<SimulcastRid> = [SimulcastRid::full(), SimulcastRid::half()]
            .into_iter()
            .collect();
        let current: HashSet<SimulcastRid> = [SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(&prev, &current, &vp8_non_floor_rids());
        assert_eq!(
            actions,
            vec![CapacityAction::PauseLayer(SimulcastRid::full())]
        );
    }

    #[test]
    fn diff_wanted_layer_added_fires_resume_action() {
        // full was paused, now wanted again. Resume action fires.
        let prev: HashSet<SimulcastRid> = [SimulcastRid::half()].into_iter().collect();
        let current: HashSet<SimulcastRid> = [SimulcastRid::full(), SimulcastRid::half()]
            .into_iter()
            .collect();
        let actions = diff_wanted_aggregate(&prev, &current, &vp8_non_floor_rids());
        assert_eq!(
            actions,
            vec![CapacityAction::ResumeLayer(SimulcastRid::full())]
        );
    }

    /// **4d.3c review fix regression**: pool.on_resize regenerates
    /// always-on handles ACTIVE — the resize-spawned handles do
    /// not preserve any prior pause state. If the aggregator
    /// tracked `last_applied` internally and never re-queried, a
    /// resize would silently reactivate paused upper layers and
    /// the aggregator would emit no action because its internal
    /// snapshot still believed those layers were paused.
    ///
    /// The fix replaces internal `last_applied` with a per-tick
    /// query of actual pool state via the `is_layer_paused`
    /// closure. After resize, the pool reports `Some(false)`
    /// (active) for the regenerated handles; if the policy still
    /// wants the smaller set, the diff against actual fires
    /// pause for the unwanted layers on the very next tick.
    ///
    /// Test pins the diff semantics directly: actual = full+half
    /// active (post-resize state), aggregate = half only (policy
    /// hasn't changed) → must emit PauseLayer(full). Without the
    /// fix, this test would still pass at the diff level (it
    /// always tested wanted-vs-applied), but the aggregator
    /// would never pass `actual_active` here — it would pass a
    /// stale snapshot. So the integration is what changed; the
    /// pure diff function's contract is the same.
    #[test]
    fn diff_wanted_after_pool_regen_pauses_unwanted_layers() {
        let actual_active: HashSet<SimulcastRid> = [SimulcastRid::full(), SimulcastRid::half()]
            .into_iter()
            .collect();
        let aggregate: HashSet<SimulcastRid> = [SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(&actual_active, &aggregate, &vp8_non_floor_rids());
        assert_eq!(
            actions,
            vec![CapacityAction::PauseLayer(SimulcastRid::full())],
            "post-on_resize: full reactivated by pool, policy still \
             wants {{half}} → must re-pause full",
        );
    }

    #[test]
    fn diff_wanted_mixed_pause_and_resume_in_spec_order() {
        // full was wanted (now paused); half was paused (now wanted).
        // Iteration order follows `vp8_non_floor_rids()` spec order so
        // tests + downstream consumers see deterministic ordering.
        let prev: HashSet<SimulcastRid> = [SimulcastRid::full()].into_iter().collect();
        let current: HashSet<SimulcastRid> = [SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(&prev, &current, &vp8_non_floor_rids());
        assert_eq!(
            actions,
            vec![
                CapacityAction::PauseLayer(SimulcastRid::full()),
                CapacityAction::ResumeLayer(SimulcastRid::half()),
            ]
        );
    }

    // -----------------------------------------------------------------
    // Phase 4d.3c review fix: fresh_health + freshness composition tests
    // -----------------------------------------------------------------

    #[test]
    fn fresh_health_none_input_passes_through_as_none() {
        // `None` from the projection (no health entry for this RID)
        // is already "no signal." Freshness check just preserves
        // that — doesn't synthesize a phantom health from prev count.
        assert!(fresh_health(None, 0).is_none());
        assert!(fresh_health(None, 5).is_none());
    }

    #[test]
    fn fresh_health_count_advanced_returns_some() {
        let h = health_with_measurements(0.05, 3);
        assert!(
            fresh_health(Some(&h), 0).is_some(),
            "first observation: 0 → 3"
        );
        assert!(fresh_health(Some(&h), 2).is_some(), "advanced: 2 → 3");
    }

    #[test]
    fn fresh_health_count_unchanged_returns_none() {
        // The bug 4d.3c review fix targets: stale RR repeated tick
        // after tick must NOT register as fresh signal.
        let h = health_with_measurements(0.10, 5);
        assert!(
            fresh_health(Some(&h), 5).is_none(),
            "same count: must be filtered as stale; got Some"
        );
    }

    #[test]
    fn fresh_health_count_regressed_returns_none() {
        // Defends against rtc-side counter resets / unexpected state.
        // If the count somehow went backwards (e.g., RR
        // accumulator reset after renegotiation), treat as stale —
        // don't act on counter going down.
        let h = health_with_measurements(0.10, 2);
        assert!(
            fresh_health(Some(&h), 5).is_none(),
            "regressed count: must be filtered as stale"
        );
    }

    /// **4d.3c review fix regression**: a single bad RR must NOT
    /// complete the drop debounce on its own. The signal has to
    /// remain over-budget across multiple FRESH RRs through the
    /// full 5s debounce window.
    ///
    /// Composition of `fresh_health` + `step_layer_capacity_state`
    /// — the same composition the spawn loop uses, exercised
    /// directly with a controlled measurement-count series.
    #[test]
    fn stale_repeated_bad_rr_does_not_complete_drop_debounce() {
        let cfg = cfg();
        let bad_rr = health_with_measurements(0.10, 1);
        let mut prev_count: u64 = 0;
        let mut state = LayerCapacityState::Wanted;
        let t0 = Instant::now();

        // Tick 0: first observation, count advances 0 → 1, fresh
        // signal triggers PendingDrop.
        let fresh = fresh_health(Some(&bad_rr), prev_count);
        assert!(fresh.is_some(), "first observation must be fresh");
        state = step_layer_capacity_state(state, fresh, &cfg, t0);
        assert!(matches!(state, LayerCapacityState::PendingDrop { .. }));
        prev_count = bad_rr.round_trip_time_measurements;

        // Ticks 1..N: same RR re-presented every tick (count stays
        // at 1). Walk well past the drop debounce. State must NOT
        // advance to Dropped because every observation is stale.
        for tick_n in 1..10 {
            let fresh = fresh_health(Some(&bad_rr), prev_count);
            assert!(
                fresh.is_none(),
                "stale repeat at tick {tick_n}: count {} matches \
                 prev {prev_count} — must be filtered",
                bad_rr.round_trip_time_measurements,
            );
            // Pass `None` to the policy (the spawn loop does the
            // same after fresh_health filters out the entry).
            state = step_layer_capacity_state(state, fresh, &cfg, t0 + Duration::from_secs(tick_n));
            assert!(
                matches!(state, LayerCapacityState::PendingDrop { .. }),
                "tick {tick_n}: state must remain PendingDrop \
                 without fresh RRs; got {state:?}",
            );
        }
    }

    /// **4d.3c review fix regression**: with FRESH bad RRs every
    /// tick (measurement count strictly advancing), the drop
    /// debounce completes normally and the layer transitions to
    /// Dropped. Confirms the freshness filter doesn't break the
    /// happy path.
    #[test]
    fn fresh_repeated_bad_rrs_do_complete_drop_debounce() {
        let cfg = cfg();
        let mut prev_count: u64 = 0;
        let mut state = LayerCapacityState::Wanted;
        let t0 = Instant::now();
        let drop_secs = cfg.drop_debounce.as_secs();

        // For each tick, synthesize a fresh RR with an incrementing
        // measurement count (1, 2, 3, ...) and the same over-budget
        // fraction_lost. Walk through the drop debounce window.
        for tick_n in 0..=drop_secs {
            let bad_rr = health_with_measurements(0.10, (tick_n + 1) as u64);
            let fresh = fresh_health(Some(&bad_rr), prev_count);
            assert!(
                fresh.is_some(),
                "tick {tick_n}: count {} > prev {prev_count}, must be fresh",
                bad_rr.round_trip_time_measurements,
            );
            state = step_layer_capacity_state(state, fresh, &cfg, t0 + Duration::from_secs(tick_n));
            prev_count = bad_rr.round_trip_time_measurements;
        }

        // After drop_debounce elapsed with fresh bad RRs, the
        // layer must have transitioned to Dropped.
        assert_eq!(
            state,
            LayerCapacityState::Dropped,
            "fresh bad RRs across the full drop debounce window \
             must complete the transition to Dropped; got {state:?}",
        );
    }
}
