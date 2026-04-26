//! Display-level layer-pool orchestration.
//!
//! ## Phase 4d.2: zero-peer gating
//!
//! Single responsibility: pause the always-on encoder pool's
//! simulcast layers when no WebRTC peers are connected, resume
//! them when the first peer arrives. **CPU saver only** — does
//! not make per-peer or capacity-based layer decisions.
//! Bandwidth-driven downgrade/upgrade is 4d.3's job, on a real
//! receiver-feedback signal (RTCP RR `fraction_lost`, TWCC
//! arrival feedback, browser-side `getStats`).
//!
//! ## Why display-level, not encode-level
//!
//! This module owns the policy that ties **peer presence**
//! ([`crate::display::webrtc::WebRtcPeer`]) to **encoder pool
//! lifecycle** ([`crate::display::encode::pool::EncoderPool`]).
//! Putting it under `encode/` would force `encode/` to depend
//! upward on `webrtc` (a module that's `encode/`'s consumer, not
//! its peer), inverting the dependency graph. Living at the
//! display level lets the aggregator consume both cleanly without
//! pushing webrtc-awareness down into the encoder primitive layer.
//!
//! ## State machine
//!
//! Three states, one instance per display, ticks every 1s:
//!
//! - [`AggregatorState::Active`]: at least one WebRTC peer is
//!   attached. Pool runs normally; aggregator does nothing.
//! - [`AggregatorState::IdlePending`]: peers just dropped to zero,
//!   debounce timer running. If a peer arrives before the debounce
//!   expires, we go back to `Active` without ever pausing — protects
//!   against thrashing on brief disconnect/reconnect cycles
//!   (browser refresh, network blip, federation rehandshake).
//! - [`AggregatorState::Idle`]: zero peers, all simulcast layers
//!   paused. On first peer arrival we issue
//!   [`AggregatorAction::ResumeAllSimulcast`] and go back to `Active`.
//!
//! ## Resume restores **all** layers (not just floor)
//!
//! 4d.2 is CPU gating, not quality adaptation. Resuming only the
//! floor layer would be a user-visible quality regression for any
//! peer joining a session that was idle for ≥5s — that peer would
//! see quarter-resolution video until 4d.3 lands and a real
//! receiver-feedback signal can decide higher layers are
//! sustainable. Resuming all layers preserves today's "all
//! simulcast layers always active when peers are connected"
//! behavior, just adding CPU savings during idle. 4d.3 will pause
//! upper layers selectively based on per-peer link health.
//!
//! ## Action handling is injected (testability)
//!
//! [`spawn_zero_peer_aggregator`] takes a `Box<dyn Fn(AggregatorAction)>`
//! closure rather than a direct [`crate::display::encode::pool::EncoderPool`]
//! reference. The closure pattern keeps the aggregator's state machine
//! pure (testable without spawning a real pool, capturing rids, or
//! constructing fake encoder backends) and lets the production wiring
//! at [`crate::display::DisplaySession::start`] capture the pool +
//! layer-rid snapshot in one place.

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

/// Side-effecting action the aggregator can request. Applied via
/// the closure passed to [`spawn_zero_peer_aggregator`]; production
/// wiring loops over the captured simulcast-layer rid set and calls
/// [`crate::display::encode::pool::EncoderPool::pause_layer`] /
/// [`crate::display::encode::pool::EncoderPool::resume_layer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregatorAction {
    /// Pause every always-on simulcast layer in the pool. Issued
    /// once on transition into [`AggregatorState::Idle`].
    PauseAllSimulcast,
    /// Resume every always-on simulcast layer in the pool. Issued
    /// once on transition out of [`AggregatorState::Idle`].
    ///
    /// Pool's `resume_layer` already forces a keyframe on the
    /// paused→active edge (4d.0 review fix), so the joining peer
    /// gets a decodable keyframe within one encode tick.
    ResumeAllSimulcast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggregatorState {
    Active,
    IdlePending { since: Instant },
    Idle,
}

/// Pure transition function — no side effects, no async, no I/O.
///
/// Returns `(next_state, optional_action)`. The caller (the spawn
/// loop, or a test) is responsible for applying any returned action.
fn transition(
    prev: AggregatorState,
    peer_count: usize,
    now: Instant,
) -> (AggregatorState, Option<AggregatorAction>) {
    match prev {
        AggregatorState::Active if peer_count == 0 => {
            (AggregatorState::IdlePending { since: now }, None)
        }
        AggregatorState::Active => (AggregatorState::Active, None),

        AggregatorState::IdlePending { .. } if peer_count >= 1 => {
            (AggregatorState::Active, None)
        }
        AggregatorState::IdlePending { since } if now >= since + PAUSE_DEBOUNCE => {
            (
                AggregatorState::Idle,
                Some(AggregatorAction::PauseAllSimulcast),
            )
        }
        AggregatorState::IdlePending { since } => {
            (AggregatorState::IdlePending { since }, None)
        }

        AggregatorState::Idle if peer_count >= 1 => {
            (
                AggregatorState::Active,
                Some(AggregatorAction::ResumeAllSimulcast),
            )
        }
        AggregatorState::Idle => (AggregatorState::Idle, None),
    }
}

/// Spawn the zero-peer aggregator task for one display.
///
/// `peers` is shared with the [`crate::display::DisplaySession`]
/// peer registry — the aggregator only `read()`s it, never mutates,
/// and only consults `len()`.
///
/// `on_action` applies the requested side effect. Production wiring
/// captures `Arc<EncoderPool>` plus the `Vec<SimulcastRid>` snapshot
/// taken at session start; tests pass a recording closure to
/// observe the action sequence without constructing a pool.
///
/// The task exits cleanly on `shutdown.cancelled()`. The returned
/// `JoinHandle` is awaited by [`crate::display::DisplaySession::stop`].
pub fn spawn_zero_peer_aggregator(
    peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>>,
    on_action: Box<dyn Fn(AggregatorAction) + Send + Sync>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = AggregatorState::Active;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // We do NOT discard the immediate-first tick: `interval()`
        // fires its first tick at construction (Burst), and we want
        // the first observation to happen at spawn time so a session
        // that starts with zero peers begins the debounce countdown
        // immediately rather than wasting one TICK of idle CPU.
        // Pool init and peer-registry init both complete before the
        // aggregator is spawned (see `DisplaySession::start`), so
        // there's no init race to wait out.

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let peer_count = peers.read().await.len();
                    let (next, action) = transition(state, peer_count, Instant::now());
                    state = next;
                    if let Some(a) = action {
                        on_action(a);
                    }
                }
            }
        }
    })
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
        LayerCapacityState::Wanted if over_budget => {
            LayerCapacityState::PendingDrop { since: now }
        }
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
        LayerCapacityState::PendingDrop { since }
            if now >= since + config.drop_debounce =>
        {
            LayerCapacityState::Dropped
        }
        LayerCapacityState::PendingDrop { since } => {
            LayerCapacityState::PendingDrop { since }
        }

        LayerCapacityState::Dropped if healthy => {
            LayerCapacityState::PendingRestore { since: now }
        }
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
        LayerCapacityState::PendingRestore { since }
            if now >= since + config.restore_debounce =>
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Pure transition tests --------------------------------------------

    #[test]
    fn active_with_peers_stays_active_no_action() {
        // "fresh session, first peer connects" lives here:
        // session starts in Active; peers stay >= 1; nothing fires.
        // Confirms 4d.2 doesn't perturb the "all simulcast layers
        // active by default" behavior the encoder pool starts in.
        let (s, a) = transition(AggregatorState::Active, 3, Instant::now());
        assert_eq!(s, AggregatorState::Active);
        assert_eq!(a, None);
    }

    #[test]
    fn active_zero_peers_enters_idle_pending_no_action() {
        let now = Instant::now();
        let (s, a) = transition(AggregatorState::Active, 0, now);
        assert_eq!(s, AggregatorState::IdlePending { since: now });
        assert_eq!(a, None, "no pause until debounce expires");
    }

    #[test]
    fn idle_pending_peer_arrives_returns_to_active_no_action() {
        // Browser-refresh / federation-reconnect blip: peer briefly
        // gone, comes back well within PAUSE_DEBOUNCE.
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let (s, a) = transition(pending, 2, t + Duration::from_secs(1));
        assert_eq!(s, AggregatorState::Active);
        assert_eq!(a, None, "no pause issued; debounce protected");
    }

    #[test]
    fn idle_pending_pre_debounce_holds() {
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let just_before = t + PAUSE_DEBOUNCE - Duration::from_millis(1);
        let (s, a) = transition(pending, 0, just_before);
        assert_eq!(s, AggregatorState::IdlePending { since: t });
        assert_eq!(a, None);
    }

    #[test]
    fn idle_pending_post_debounce_pauses_all() {
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let post = t + PAUSE_DEBOUNCE;
        let (s, a) = transition(pending, 0, post);
        assert_eq!(s, AggregatorState::Idle);
        assert_eq!(a, Some(AggregatorAction::PauseAllSimulcast));
    }

    #[test]
    fn idle_zero_peers_stays_idle() {
        let (s, a) = transition(AggregatorState::Idle, 0, Instant::now());
        assert_eq!(s, AggregatorState::Idle);
        assert_eq!(a, None);
    }

    #[test]
    fn idle_first_peer_resumes_all_layers() {
        // The whole point of choosing ResumeAllSimulcast over
        // ResumeFloor: a post-idle joiner gets full quality, not a
        // quarter-res regression. 4d.3 will pause upper layers
        // selectively based on per-peer link health, but until then
        // 4d.2 must NOT silently downgrade.
        let (s, a) = transition(AggregatorState::Idle, 1, Instant::now());
        assert_eq!(s, AggregatorState::Active);
        assert_eq!(
            a,
            Some(AggregatorAction::ResumeAllSimulcast),
            "4d.2 restores ALL simulcast layers — not just floor — \
             so a peer joining post-idle gets full quality, not a \
             quarter-res regression",
        );
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
        let (s, _) = transition(AggregatorState::Active, 0, t0);
        let (s, _) = transition(s, 2, t0 + Duration::from_secs(1));
        assert_eq!(s, AggregatorState::Active, "blip resolved");
        let (s, _) = transition(s, 0, t0 + Duration::from_secs(4));
        assert!(matches!(s, AggregatorState::IdlePending { .. }));
        let (s, a) = transition(s, 0, t0 + Duration::from_secs(8));
        assert!(matches!(s, AggregatorState::IdlePending { .. }));
        assert_eq!(a, None, "still 1s before debounce expires");
        let (s, a) = transition(s, 0, t0 + Duration::from_secs(9));
        assert_eq!(s, AggregatorState::Idle);
        assert_eq!(a, Some(AggregatorAction::PauseAllSimulcast));
    }

    // ----- Spawn-loop integration test --------------------------------------

    /// Verify the spawn function actually issues `PauseAllSimulcast`
    /// after `PAUSE_DEBOUNCE` at zero peers. Uses a recording
    /// closure (no real `EncoderPool` required); pure transition
    /// tests cover the resume edge, since synthesizing a
    /// `WebRtcPeer` to bump `peers.len()` is heavyweight and the
    /// spawn-site `DisplaySession::start` integration test covers
    /// the resume wiring end-to-end.
    ///
    /// Polls with a generous timeout instead of a fixed sleep to
    /// avoid flake on overloaded test runners — the action only has
    /// to land *eventually* within the deadline, not at any
    /// specific tick. `Instant::now()` reads inside the spawn loop
    /// are real wallclock (Tokio's mock clock doesn't advance
    /// Instant), so test runtimes under load can drift the action
    /// past `PAUSE_DEBOUNCE` by a tick or two; the deadline
    /// generously covers that.
    #[tokio::test]
    async fn spawn_records_pause_after_zero_peer_debounce() {
        use std::sync::Mutex as StdMutex;

        let peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let recorded: Arc<StdMutex<Vec<AggregatorAction>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let recorded_for_closure = Arc::clone(&recorded);
        let on_action: Box<dyn Fn(AggregatorAction) + Send + Sync> =
            Box::new(move |a| {
                recorded_for_closure.lock().unwrap().push(a);
            });
        let shutdown = CancellationToken::new();
        let handle = spawn_zero_peer_aggregator(
            Arc::clone(&peers),
            on_action,
            shutdown.clone(),
        );

        // Poll with a generous timeout. PAUSE_DEBOUNCE + 5s of
        // tolerance handles tick drift on a loaded runtime; we exit
        // the loop as soon as the action lands.
        let deadline = Instant::now() + PAUSE_DEBOUNCE + Duration::from_secs(5);
        loop {
            if !recorded.lock().unwrap().is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                let actions = recorded.lock().unwrap().clone();
                shutdown.cancel();
                let _ = handle.await;
                panic!(
                    "no aggregator action recorded within \
                     PAUSE_DEBOUNCE + 5s ({}s total); got {actions:?}",
                    (PAUSE_DEBOUNCE + Duration::from_secs(5)).as_secs(),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let actions = recorded.lock().unwrap().clone();
        assert_eq!(
            actions,
            vec![AggregatorAction::PauseAllSimulcast],
            "expected exactly one PauseAllSimulcast within deadline; \
             got {actions:?}",
        );

        shutdown.cancel();
        let _ = handle.await;
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
        PeerLayerHealth {
            fraction_lost,
            packets_lost_total: 0,
            round_trip_time_seconds: 0.0,
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
        let s = step_layer_capacity_state(
            LayerCapacityState::Wanted,
            Some(&h),
            &cfg(),
            t0(),
        );
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_wanted_with_over_budget_enters_pending_drop() {
        let h = health(0.10); // 10%, well over 5% threshold
        let now = t0();
        let s = step_layer_capacity_state(
            LayerCapacityState::Wanted,
            Some(&h),
            &cfg(),
            now,
        );
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
        let s = step_layer_capacity_state(
            LayerCapacityState::Dropped,
            Some(&h),
            &cfg(),
            t0(),
        );
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_partial_recovery_does_not_restore() {
        // 0.04 is between recovery (0.02) and threshold (0.05).
        // Since we're already Dropped, restoration requires
        // crossing the (lower) recovery threshold — wider hysteresis
        // band prevents oscillation around the threshold.
        let h = health(0.04);
        let s = step_layer_capacity_state(
            LayerCapacityState::Dropped,
            Some(&h),
            &cfg(),
            t0(),
        );
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_clear_recovery_enters_pending_restore() {
        let h = health(0.01); // ≤ 2% recovery
        let now = t0();
        let s = step_layer_capacity_state(
            LayerCapacityState::Dropped,
            Some(&h),
            &cfg(),
            now,
        );
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
        let s = step_layer_capacity_state(
            pending,
            Some(&gray_band),
            &cfg(),
            post,
        );
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
        assert!(layer_state_is_wanted(
            &LayerCapacityState::PendingDrop { since: Instant::now() }
        ));
        assert!(!layer_state_is_wanted(&LayerCapacityState::Dropped));
        assert!(!layer_state_is_wanted(
            &LayerCapacityState::PendingRestore { since: Instant::now() }
        ));
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
        let peer_a: HashSet<SimulcastRid> =
            [SimulcastRid::full(), SimulcastRid::quarter()].into_iter().collect();
        let peer_b: HashSet<SimulcastRid> =
            [SimulcastRid::half(), SimulcastRid::quarter()].into_iter().collect();
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
        let only_full: HashSet<SimulcastRid> =
            [SimulcastRid::full()].into_iter().collect();
        let agg = aggregate_wanted_layers(vec![only_full.clone()]);
        assert_eq!(agg, only_full);
    }
}
