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

use crate::display::webrtc::WebRtcPeer;
use crate::display::PeerId;
use std::collections::HashMap;
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
}
