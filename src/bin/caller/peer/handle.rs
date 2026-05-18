//! The public [`PeerHandle`] struct, its command envelope, the
//! [`ConnectionState`] enum, and the [`spawn_peer`] constructor.
//!
//! A `PeerHandle` is what the registry stores and what the rest of
//! the code interacts with. It's a concrete struct (not a trait
//! object): a per-peer actor task owns the [`PeerTransport`] by value
//! and the handle holds channels + watch snapshots. This eliminates
//! trait-object downcasting and keeps heterogeneous peer storage
//! simple — the registry is just `HashMap<PeerId, PeerHandle>`.
//!
//! ## State model
//!
//! Two watch-backed states, deliberately separate:
//!
//! - [`ConnectionState`] — transport lifecycle (connecting, connected,
//!   reconnecting, etc). Transitions owned exclusively by the actor.
//! - [`PeerStatus`] — operational status reported by the peer itself
//!   (idle, working, needs approval, error). Updated from inbound
//!   [`PeerEvent::StatusChanged`] events.
//!
//! The dashboard composes them: e.g. "disconnected (last seen:
//! working)" combines `ConnectionState::Disconnected` with the last
//! observed `PeerStatus::Working`.

use crate::peer::card::AgentCard;
use crate::peer::event::{
    ApprovalDecision, MessageId, PeerEvent, PeerMessage, PeerStatus, TaggedPeerEvent, TaskId,
    TaskUpdate, WebRtcSessionId, WebRtcSignal,
};
use crate::peer::id::PeerId;
use crate::peer::traits::{PeerOp, PeerOpAck, PeerTask, PeerTransport, TransportFeatures};
use crate::peer::PeerError;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

// ---------------------------------------------------------------------------
// Channel capacities
// ---------------------------------------------------------------------------

/// Bounded capacity for the per-handle command channel.
/// Low volume — commands are user/coordinator initiated.
pub const COMMANDS_CAPACITY: usize = 64;

/// Bounded capacity for the transport→actor event channel.
/// Sized for streaming model output bursts. When this fills, the
/// transport's send side backpressures, which is correct behavior
/// when a downstream sink (log, broadcast) is saturated.
pub const EVENTS_CAPACITY: usize = 1024;

/// Broadcast capacity for the actor→subscribers fan-out.
/// Slow UI subscribers lag and skip rather than blocking the actor.
/// Durable consumers go through the registry's log sink, not here.
pub const BROADCAST_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Transport lifecycle state, owned by the per-peer actor task.
///
/// Distinct from [`PeerStatus`] by design — this describes the *wire
/// connection*, not the peer's *operational state*. The dashboard
/// reads both: e.g. a peer could be in
/// `ConnectionState::Reconnecting { attempt: 3 }` while its last
/// observed `PeerStatus` is still `Working`.
///
/// Copy-able so `watch::Receiver::borrow()` is allocation-free.
/// Serialized via the internally-tagged `state` discriminator so
/// the `/api/peers` response embeds connection state cleanly in a
/// flat JSON object for the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    /// Actor task spawned, pre-connect.
    Initializing,
    /// `transport.connect()` in flight.
    Connecting,
    /// Connect succeeded; main command/event loop running.
    Connected,
    /// Transport disconnected, waiting in backoff before retrying.
    /// `attempt` is the number of failed reconnect attempts since the
    /// last successful connect (resets to 0 on every success).
    Reconnecting { attempt: u32 },
    /// Explicit shutdown requested; cleanup in progress.
    Disconnecting,
    /// Terminal state — actor task has exited.
    Disconnected,
}

// ---------------------------------------------------------------------------
// Command envelope
// ---------------------------------------------------------------------------

/// Commands sent from the handle to the actor. Internal to the peer
/// module — callers use [`PeerHandle`] methods which wrap these.
pub(crate) enum PeerCommand {
    Send {
        op: PeerOp,
        responder: oneshot::Sender<Result<PeerOpAck, PeerError>>,
    },
    Disconnect,
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// Registry-facing handle for one peer. Cheaply cloneable
/// (`Arc`-backed); every clone refers to the same underlying actor
/// and channels.
#[derive(Clone)]
pub struct PeerHandle {
    inner: Arc<PeerHandleInner>,
}

struct PeerHandleInner {
    id: PeerId,
    features: TransportFeatures,
    connection: watch::Receiver<ConnectionState>,
    status: watch::Receiver<PeerStatus>,
    card: watch::Receiver<Arc<AgentCard>>,
    commands: mpsc::Sender<PeerCommand>,
    events: broadcast::Sender<PeerEvent>,
    /// Browser-side TCP via URL — immutable for the lifetime of the
    /// handle. Set at `spawn_peer` time from the operator's
    /// `AddPeerRequest.browser_tcp_via_url` or
    /// `PeerConfig.browser_tcp_via_url`. Surfaces on
    /// [`PeerSnapshot::browser_tcp_via_url`] so the dashboard can
    /// pick it over `ws_url` when sending federated WebRTC offers.
    browser_tcp_via_url: Option<String>,
}

impl PeerHandle {
    pub fn id(&self) -> &PeerId {
        &self.inner.id
    }

    /// Snapshot of the peer's current Agent Card. Cheap: returns an
    /// `Arc<AgentCard>` that's stable for the caller's use. When the
    /// peer re-issues its card on reconnect, subsequent calls return
    /// the new one.
    pub fn card_snapshot(&self) -> Arc<AgentCard> {
        self.inner.card.borrow().clone()
    }

    /// Subscribe to card updates. Useful for UIs that reactively
    /// re-render when a peer advertises new capabilities.
    pub fn card_updates(&self) -> watch::Receiver<Arc<AgentCard>> {
        self.inner.card.clone()
    }

    pub fn status(&self) -> PeerStatus {
        *self.inner.status.borrow()
    }

    pub fn status_updates(&self) -> watch::Receiver<PeerStatus> {
        self.inner.status.clone()
    }

    pub fn connection_state(&self) -> ConnectionState {
        *self.inner.connection.borrow()
    }

    pub fn connection_updates(&self) -> watch::Receiver<ConnectionState> {
        self.inner.connection.clone()
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.inner.connection.borrow(), ConnectionState::Connected)
    }

    pub fn features(&self) -> TransportFeatures {
        self.inner.features
    }

    /// Serializable snapshot of this peer's externally-visible state at
    /// call time. Cheap: reads the watch channels (no lock contention,
    /// no cross-task communication) and clones the card. Safe to call
    /// concurrently with peer state changes; the snapshot reflects
    /// whatever values were observable at call time.
    ///
    /// Used by both `GET /api/peers` (one snapshot per registry entry)
    /// and the dashboard push event stream emitted by [`PeerRegistry`]
    /// (one snapshot per state change). One type, two surfaces; the
    /// browser handler treats either source identically.
    pub fn snapshot(&self) -> PeerSnapshot {
        let card = self.card_snapshot();
        let ws_url = card.transports.iter().find_map(|t| match t {
            crate::peer::card::TransportSpec::IntendantWs { url } => Some(url.clone()),
            _ => None,
        });
        let capabilities: Vec<serde_json::Value> = card
            .capabilities
            .iter()
            .filter_map(|c| serde_json::to_value(c).ok())
            .collect();
        PeerSnapshot {
            id: self.id().as_str().to_string(),
            label: card.label.clone(),
            version: card.version.clone(),
            git_sha: card.git_sha.clone(),
            connection_state: self.connection_state(),
            status: self.status(),
            ws_url,
            capabilities,
            browser_tcp_via_url: self.inner.browser_tcp_via_url.clone(),
        }
    }

    /// Operator-supplied browser-side TCP via URL for this peer.
    /// Exposed here for diagnostics; the dashboard reads the same
    /// value out of [`PeerSnapshot::browser_tcp_via_url`].
    pub fn browser_tcp_via_url(&self) -> Option<&str> {
        self.inner.browser_tcp_via_url.as_deref()
    }

    /// Subscribe to the peer's event stream. Fan-out is lossy for
    /// lagging subscribers — [`TaggedPeerEvent`]s land on the session
    /// log via the registry's durable sink, so missed broadcast
    /// events are recoverable from the log (which is the authoritative
    /// record for replay).
    pub fn subscribe(&self) -> broadcast::Receiver<PeerEvent> {
        self.inner.events.subscribe()
    }

    // ---- Op methods ----

    pub async fn send_message(&self, msg: PeerMessage) -> Result<MessageId, PeerError> {
        if !self.features().send_message {
            return Err(PeerError::UnsupportedCapability("send_message".into()));
        }
        match self.exec(PeerOp::SendMessage { message: msg }).await? {
            PeerOpAck::MessageId(id) => Ok(id),
            other => Err(PeerError::Transport(format!(
                "expected MessageId ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn delegate_task(&self, task: PeerTask) -> Result<TaskId, PeerError> {
        if !self.features().task_delegation {
            return Err(PeerError::UnsupportedCapability("task_delegation".into()));
        }
        match self.exec(PeerOp::DelegateTask { task }).await? {
            PeerOpAck::TaskId(id) => Ok(id),
            other => Err(PeerError::Transport(format!(
                "expected TaskId ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn cancel_task(&self, task: &TaskId) -> Result<(), PeerError> {
        if !self.features().task_cancel {
            return Err(PeerError::UnsupportedCapability("task_cancel".into()));
        }
        match self.exec(PeerOp::CancelTask { task: task.clone() }).await? {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn query_task(&self, task: &TaskId) -> Result<TaskUpdate, PeerError> {
        if !self.features().task_query {
            return Err(PeerError::UnsupportedCapability("task_query".into()));
        }
        match self
            .exec(PeerOp::QueryTaskStatus { task: task.clone() })
            .await?
        {
            PeerOpAck::TaskStatus(u) => Ok(u),
            other => Err(PeerError::Transport(format!(
                "expected TaskStatus ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn invoke(
        &self,
        capability: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, PeerError> {
        if !self.features().invoke_capability {
            return Err(PeerError::UnsupportedCapability("invoke_capability".into()));
        }
        match self
            .exec(PeerOp::InvokeCapability {
                name: capability.to_string(),
                args,
            })
            .await?
        {
            PeerOpAck::Value(v) => Ok(v),
            other => Err(PeerError::Transport(format!(
                "expected Value ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn resolve_approval(
        &self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), PeerError> {
        if !self.features().resolve_approval {
            return Err(PeerError::UnsupportedCapability("resolve_approval".into()));
        }
        match self
            .exec(PeerOp::ResolveApproval {
                request_id: request_id.to_string(),
                decision,
            })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Send one leg of a WebRTC signaling exchange to this peer.
    /// Returns immediately on dispatch; the peer's response (Answer,
    /// trickled IceCandidates) flows back asynchronously through the
    /// per-peer event stream as [`PeerEvent::WebRtcSignal`].
    pub async fn webrtc_signal(
        &self,
        display_id: u32,
        session_id: WebRtcSessionId,
        signal: WebRtcSignal,
    ) -> Result<(), PeerError> {
        if !self.features().webrtc_signal {
            return Err(PeerError::UnsupportedCapability("webrtc_signal".into()));
        }
        match self
            .exec(PeerOp::WebRtcSignal {
                display_id,
                session_id,
                signal,
            })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Request explicit disconnect. Awaits until the actor has
    /// transitioned to [`ConnectionState::Disconnected`] so callers
    /// know the transport is actually torn down when this returns.
    pub async fn disconnect(&self) -> Result<(), PeerError> {
        // Fire the command; mapping SendError to NotConnected is
        // correct — if the actor is already gone, the effect we want
        // (disconnected) has already happened.
        if self
            .inner
            .commands
            .send(PeerCommand::Disconnect)
            .await
            .is_err()
        {
            return Ok(());
        }
        let mut rx = self.inner.connection.clone();
        loop {
            if matches!(*rx.borrow(), ConnectionState::Disconnected) {
                return Ok(());
            }
            if rx.changed().await.is_err() {
                // Sender dropped → actor is gone → effectively disconnected.
                return Ok(());
            }
        }
    }

    // ---- Internal exec helper ----

    /// Send a command to the actor and await the response.
    ///
    /// Uses `.send().await`, not `try_send`, so load pressure from a
    /// slow actor propagates naturally to the caller as wait time
    /// rather than spurious `NotConnected` errors. `NotConnected` is
    /// only returned when the command channel is actually closed
    /// (actor has exited).
    async fn exec(&self, op: PeerOp) -> Result<PeerOpAck, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .commands
            .send(PeerCommand::Send { op, responder: tx })
            .await
            .map_err(|_| PeerError::NotConnected)?;
        rx.await.map_err(|_| PeerError::NotConnected)?
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Serializable snapshot of one peer's externally-visible state.
///
/// Built from a [`PeerHandle`] via [`PeerHandle::snapshot`]. Used by:
/// - `GET /api/peers` as the canonical list payload the dashboard reads
///   at startup and after add/remove operations.
/// - Dashboard push events emitted by [`crate::peer::registry::PeerRegistry`]
///   so the browser updates rows in-place without re-fetching the full
///   list.
///
/// `Deserialize` is derived only because `OutboundEvent` round-trips
/// through serde and embeds this type — local Rust code constructs
/// snapshots from a handle, never from JSON. The dashboard deserializes
/// at the JS layer.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PeerSnapshot {
    pub id: String,
    pub label: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    pub connection_state: ConnectionState,
    pub status: PeerStatus,
    /// Native Intendant WebSocket URL from the peer's card, if any.
    /// The browser uses this to open a secondary WASM connection for
    /// live event streaming (the `/api/peers` payload is a state
    /// snapshot; live per-peer events still flow through the WASM path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
    /// Capability list serialized to opaque JSON values so the dashboard
    /// renders badges without the snapshot type having to re-derive
    /// the full Capability schema. Each element matches the wire format
    /// of [`crate::peer::card::Capability`] (`{kind: "computer-use"}` for
    /// built-in variants, `{kind: "custom", name: "..."}` for `Custom`).
    pub capabilities: Vec<serde_json::Value>,
    /// Operator-supplied URL the browser uses to reach this peer's
    /// HTTP port for WebRTC ICE-TCP. Decoupled from `ws_url` (the
    /// primary-side via URL) so browsers on a different network
    /// position from the primary can still form a TCP ICE pair. When
    /// `None`, the dashboard falls back to `ws_url` — identical to
    /// the slice 3a.2 behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_tcp_via_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn a new peer actor task and return the public handle.
///
/// `build_transport` is called exactly once with the sender side of
/// the transport→actor event channel, and must return a boxed
/// transport that pushes [`PeerEvent`]s to that sender. Typical use:
///
/// ```ignore
/// let handle = spawn_peer(peer_id, initial_card, log_sink, |events_tx| {
///     Box::new(IntendantWsTransport::new(url, events_tx))
/// });
/// ```
///
/// `initial_card` is the "last known card" — typically whatever was
/// fetched at discovery time from the peer's
/// `/.well-known/agent-card.json`, with any operator overrides
/// (via_urls, pinned fingerprints) already applied by the caller.
/// The actor overwrites it with the card returned from
/// `transport.connect()` as soon as the first handshake completes,
/// applying `via_urls` to the fresh card so the operator's override
/// persists across reconnects.
///
/// `via_urls` is the same list the caller passed through
/// [`crate::peer::PeerRegistry::add_peer_with_credentials`] — stored
/// on the actor and re-applied to every card it publishes. Empty
/// means "no override; trust what the peer advertises."
///
/// `browser_tcp_via_url` is operator-supplied metadata for the
/// dashboard: the URL the **browser** uses to reach this peer's
/// HTTP port for WebRTC ICE-TCP. Orthogonal to `via_urls` (which
/// governs how the primary reaches the peer's /ws). Stored on
/// [`PeerHandle`] and surfaced via
/// [`PeerSnapshot::browser_tcp_via_url`]; the dashboard reads it
/// back and sends it as the `advertise_tcp_via_url` hint in the
/// federated WebRTC offer. `None` falls back to `ws_url` — slice
/// 3a.2 behavior.
pub fn spawn_peer<F>(
    id: PeerId,
    initial_card: AgentCard,
    via_urls: Vec<String>,
    browser_tcp_via_url: Option<String>,
    log_sink: mpsc::Sender<TaggedPeerEvent>,
    build_transport: F,
) -> PeerHandle
where
    F: FnOnce(mpsc::Sender<PeerEvent>) -> Box<dyn PeerTransport>,
{
    let (events_in_tx, events_in_rx) = mpsc::channel::<PeerEvent>(EVENTS_CAPACITY);
    let (events_out_tx, _) = broadcast::channel::<PeerEvent>(BROADCAST_CAPACITY);
    let (commands_tx, commands_rx) = mpsc::channel::<PeerCommand>(COMMANDS_CAPACITY);
    let (connection_tx, connection_rx) = watch::channel(ConnectionState::Initializing);
    let (status_tx, status_rx) = watch::channel(PeerStatus::Idle);
    let (card_tx, card_rx) = watch::channel(Arc::new(initial_card));

    let transport = build_transport(events_in_tx);
    let features = transport.features();

    let actor = crate::peer::actor::PeerActor {
        peer_id: id.clone(),
        transport,
        commands_rx,
        events_in_rx,
        events_out_tx: events_out_tx.clone(),
        log_sink,
        connection_tx,
        status_tx,
        card_tx,
        seq: 0,
        via_urls,
    };

    tokio::spawn(actor.run());

    PeerHandle {
        inner: Arc::new(PeerHandleInner {
            id,
            features,
            connection: connection_rx,
            status: status_rx,
            card: card_rx,
            commands: commands_tx,
            events: events_out_tx,
            browser_tcp_via_url,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn connection_state_is_copy_and_equatable() {
        let a = ConnectionState::Reconnecting { attempt: 3 };
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(ConnectionState::Connecting, ConnectionState::Connected);
    }

    #[test]
    fn channel_capacities_are_nonzero() {
        // Guard against accidentally setting a capacity to 0, which
        // turns a bounded mpsc into a rendezvous channel and would
        // change backpressure semantics silently.
        assert!(COMMANDS_CAPACITY > 0);
        assert!(EVENTS_CAPACITY > 0);
        assert!(BROADCAST_CAPACITY > 0);
    }

    /// Ensure `disconnect` returns promptly when the actor is in
    /// reconnect backoff. This is the regression guard for the
    /// bug where `remove_peer` would block indefinitely if a peer
    /// went unreachable — the actor was sleeping in the backoff
    /// phase and not polling the command channel, so
    /// `PeerCommand::Disconnect` sat queued and `disconnect`
    /// waited forever for `ConnectionState` to reach `Disconnected`.
    ///
    /// The fix: drain commands inside the reconnect sleep via
    /// `tokio::select!` so Disconnect short-circuits the backoff.
    /// This test points a transport at a definitely-refused port,
    /// waits for the actor to transition into `Reconnecting`,
    /// then calls `disconnect` with a 2-second timeout. If the
    /// select in the reconnect phase is removed or breaks, the
    /// test times out.
    #[tokio::test]
    async fn disconnect_short_circuits_reconnect_backoff() {
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use tokio::sync::mpsc;

        // Reserve-then-release an ephemeral port to get a TCP port
        // that's almost certainly refused on the next connect.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "unreachable"),
            label: "unreachable".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            None,
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );

        // Wait until the actor fails the first connect and enters
        // the reconnect phase. Poll instead of a fixed sleep so the
        // test is robust against scheduler jitter.
        let enter_deadline = Instant::now() + Duration::from_secs(3);
        let entered_reconnect = loop {
            if matches!(
                handle.connection_state(),
                ConnectionState::Reconnecting { .. }
            ) {
                break true;
            }
            if Instant::now() > enter_deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert!(
            entered_reconnect,
            "actor never transitioned to Reconnecting (current state: {:?})",
            handle.connection_state()
        );

        // Now call disconnect. Without the fix, this would block
        // until the backoff sleep elapsed (up to 30s on later
        // attempts) or forever if the remote stayed down. With the
        // fix, it should return within the 2-second timeout.
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(2), handle.disconnect()).await;
        assert!(
            result.is_ok(),
            "disconnect timed out during reconnect backoff"
        );
        result.unwrap().expect("disconnect returned Err");
        let elapsed = start.elapsed();
        // Tighter cap than the timeout — an overshoot here means
        // we spent most of the window waiting, which indicates the
        // select isn't actually short-circuiting.
        assert!(
            elapsed < Duration::from_millis(1500),
            "disconnect took {elapsed:?} — expected <1.5s"
        );

        assert_eq!(
            handle.connection_state(),
            ConnectionState::Disconnected,
            "actor didn't transition to Disconnected"
        );
    }

    /// Operator-supplied `browser_tcp_via_url` round-trips through
    /// `spawn_peer` into the `PeerHandle` and surfaces on
    /// `PeerSnapshot`. This locks the contract the dashboard relies
    /// on: the server stores the URL at peer-registration time and
    /// hands it back on every `/api/peers` query so the Add Peer
    /// form's configured value survives browser reloads.
    #[tokio::test]
    async fn browser_tcp_via_url_persists_through_snapshot() {
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use tokio::sync::mpsc;

        // Any local-only WS URL works; the actor will try to connect
        // (and fail, since nothing's listening) — but that's fine,
        // the snapshot we care about reflects the initial card +
        // the constructor-supplied browser_tcp_via_url, not the
        // post-connect state.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let ws_url = format!("ws://127.0.0.1:{port}/ws");

        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "bp-test"),
            label: "bp-test".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let browser_url = "ws://192.168.1.42:8766/ws".to_string();
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            Some(browser_url.clone()),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );

        let snap = handle.snapshot();
        assert_eq!(
            snap.browser_tcp_via_url.as_deref(),
            Some(browser_url.as_str()),
            "snapshot must expose the constructor-supplied browser URL"
        );
        assert_eq!(
            handle.browser_tcp_via_url(),
            Some(browser_url.as_str()),
            "getter mirrors snapshot"
        );
        // Belt-and-suspenders: the None case doesn't crash.
        // (Constructed separately to avoid re-using the same card id,
        // which would trip the duplicate-registration path if this
        // were a real registry — spawn_peer itself doesn't check,
        // but clarity matters.)
    }

    /// `None` for `browser_tcp_via_url` surfaces as `None` on the
    /// snapshot — no surprising empty-string conversion. Important
    /// because the dashboard distinguishes "operator didn't set a
    /// browser URL" (fall back to ws_url) from "operator explicitly
    /// wants this URL"; an empty string would collapse both cases.
    #[tokio::test]
    async fn browser_tcp_via_url_none_stays_none() {
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use tokio::sync::mpsc;

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let ws_url = format!("ws://127.0.0.1:{port}/ws");

        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "bp-none"),
            label: "bp-none".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            None,
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );
        assert!(handle.snapshot().browser_tcp_via_url.is_none());
        assert!(handle.browser_tcp_via_url().is_none());
    }
}
