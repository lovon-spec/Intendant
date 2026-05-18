//! Capability-based task routing across federated peers.
//!
//! The coordinator sits above [`PeerRegistry`] and [`PeerHandle`]:
//! it takes a [`TaskRequest`] with required capabilities, selects
//! an eligible peer from the registry, and dispatches through
//! [`PeerHandle::delegate_task`]. The peer selection is the only
//! decision this module makes — everything else (transport
//! encoding, actor lifecycle, reconnect) is handled by the layers
//! below.
//!
//! ## Selection strategy (phase 1)
//!
//! First eligible peer in lexicographic `PeerId` order. Eligible
//! means:
//!
//! 1. `ConnectionState::Connected` — a peer in `Reconnecting` or
//!    `Disconnected` is not a viable target even if its card
//!    advertises the right capabilities.
//! 2. Every capability in `TaskRequest::required_capabilities` is
//!    present in the peer's `AgentCard::capabilities`.
//!
//! The lexicographic tie-break is deterministic given the same
//! registry state, which matters for idempotent retry logic
//! (retrying the same request without changing the peer set
//! routes to the same peer). A round-robin or weighted strategy
//! is a phase-2 extension — swap the sort + first-pick with a
//! pluggable `RoutingStrategy` trait when the need arises.

use crate::peer::card::Capability;
use crate::peer::event::TaskId;
use crate::peer::handle::{ConnectionState, PeerHandle};
use crate::peer::id::PeerId;
use crate::peer::registry::PeerRegistry;
use crate::peer::traits::PeerTask;
use crate::peer::PeerError;
use std::fmt;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A task delegation request with routing metadata.
///
/// `required_capabilities` tells the coordinator what the
/// executing peer must be able to do; `task` is the wire-level
/// payload the winning peer receives via its transport's
/// `PeerTransport::send(DelegateTask { task })`.
#[derive(Debug)]
pub struct TaskRequest {
    pub required_capabilities: Vec<Capability>,
    pub task: PeerTask,
}

/// The result of a successfully routed task.
#[derive(Debug)]
pub struct RoutedTask {
    /// Which peer was selected.
    pub peer_id: PeerId,
    /// The peer-assigned task id (from `PeerOpAck::TaskId`).
    pub task_id: TaskId,
}

/// Errors from the coordinator's routing logic.
#[derive(Debug)]
pub enum CoordinatorError {
    /// No connected peer matches the required capabilities.
    NoRoute {
        required: Vec<Capability>,
        /// Peers that were in the registry but didn't qualify
        /// (for diagnostics — the caller can see *why* no route
        /// was found: wrong capabilities? all disconnected?).
        considered: Vec<PeerId>,
    },
    /// A peer was selected but the delegation failed at the
    /// transport layer.
    DelegationFailed { peer: PeerId, error: PeerError },
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoute {
                required,
                considered,
            } => {
                write!(
                    f,
                    "no route: required capabilities {:?}, considered {} peer(s)",
                    required,
                    considered.len()
                )
            }
            Self::DelegationFailed { peer, error } => {
                write!(f, "delegation to {} failed: {}", peer, error)
            }
        }
    }
}

impl std::error::Error for CoordinatorError {}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// Capability-based task router.
///
/// Cheaply cloneable (holds a `PeerRegistry` clone which is
/// `Arc`-backed internally). Thread the same coordinator into
/// the agent loop, MCP handlers, and the dashboard API so they
/// all route through the same registry state.
#[derive(Clone)]
pub struct Coordinator {
    registry: PeerRegistry,
}

impl Coordinator {
    pub fn new(registry: PeerRegistry) -> Self {
        Self { registry }
    }

    /// Route a task to an eligible peer.
    ///
    /// See the [module docs](self) for the selection strategy.
    /// Returns [`RoutedTask`] on success, [`CoordinatorError`] on
    /// failure. The caller is responsible for retry / fallback
    /// policy — the coordinator makes a single attempt at the
    /// first matching peer and does not internally retry against
    /// alternatives.
    pub async fn route_task(&self, request: TaskRequest) -> Result<RoutedTask, CoordinatorError> {
        let handles = self.registry.list();

        let mut eligible: Vec<&PeerHandle> = handles
            .iter()
            .filter(|h| matches!(h.connection_state(), ConnectionState::Connected))
            .filter(|h| {
                let card = h.card_snapshot();
                request
                    .required_capabilities
                    .iter()
                    .all(|req| card.capabilities.contains(req))
            })
            .collect();

        if eligible.is_empty() {
            return Err(CoordinatorError::NoRoute {
                required: request.required_capabilities,
                considered: handles.iter().map(|h| h.id().clone()).collect(),
            });
        }

        // Stable tie-break: lexicographic order by PeerId string.
        eligible.sort_by(|a, b| a.id().as_str().cmp(b.id().as_str()));
        let winner = eligible[0];
        let peer_id = winner.id().clone();

        match winner.delegate_task(request.task).await {
            Ok(task_id) => Ok(RoutedTask { peer_id, task_id }),
            Err(error) => Err(CoordinatorError::DelegationFailed {
                peer: peer_id,
                error,
            }),
        }
    }

    /// Convenience: list peers that are connected and have all
    /// the given capabilities. Useful for UI that wants to show
    /// "which peers could handle this task?" without actually
    /// dispatching.
    pub fn eligible_peers(&self, required: &[Capability]) -> Vec<PeerHandle> {
        self.registry
            .list()
            .into_iter()
            .filter(|h| matches!(h.connection_state(), ConnectionState::Connected))
            .filter(|h| {
                let card = h.card_snapshot();
                required.iter().all(|req| card.capabilities.contains(req))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
    use crate::peer::event::TaggedPeerEvent;
    use crate::peer::id::PeerKind;
    use crate::peer::transport::IntendantWsTransport;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
    use std::time::{Duration, Instant};
    use tokio::sync::{broadcast, mpsc};

    async fn spawn_gateway() -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (tx, _) = broadcast::channel::<String>(16);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        (port, handle)
    }

    fn make_registry() -> (PeerRegistry, mpsc::Sender<TaggedPeerEvent>) {
        let (log_tx, _log_rx) = mpsc::channel(64);
        let registry = PeerRegistry::new(log_tx.clone());
        (registry, log_tx)
    }

    fn fake_card(label: &str, ws_url: &str, caps: Vec<Capability>) -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::Intendant, label),
            label: label.to_string(),
            version: "test".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.to_string(),
            }],
            capabilities: caps,
            auth: AuthRequirements::none(),
        }
    }

    /// Wait until a peer reaches Connected state, or give up.
    async fn wait_connected(handle: &PeerHandle) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(handle.connection_state(), ConnectionState::Connected) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// Wait until a peer is in Reconnecting (i.e. NOT connected).
    async fn wait_reconnecting(handle: &PeerHandle) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(
                handle.connection_state(),
                ConnectionState::Reconnecting { .. }
            ) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// A connected peer with matching capabilities gets the task.
    #[tokio::test]
    async fn route_to_peer_with_matching_capabilities() {
        let (port, gw) = spawn_gateway().await;
        let (registry, _) = make_registry();
        let card = fake_card(
            "worker-a",
            &format!("ws://127.0.0.1:{port}/ws"),
            vec![Capability::ComputerUse, Capability::Knowledge],
        );
        let peer_id = registry.add_peer_with_card(card).await.unwrap();
        let handle = registry.get(&peer_id).unwrap();
        assert!(wait_connected(&handle).await, "peer never connected");

        let coordinator = Coordinator::new(registry.clone());
        let result = coordinator
            .route_task(TaskRequest {
                required_capabilities: vec![Capability::ComputerUse],
                task: PeerTask {
                    instructions: "take a screenshot".into(),
                    context: serde_json::Value::Null,
                    client_correlation_id: None,
                },
            })
            .await;

        let routed = result.expect("route should succeed");
        assert_eq!(routed.peer_id, peer_id);

        registry.remove_peer(&peer_id).await.unwrap();
        gw.abort();
    }

    /// A peer in `Reconnecting` state is not eligible even if its
    /// card advertises the right capabilities. The coordinator
    /// returns `NoRoute` when the only candidates are disconnected.
    #[tokio::test]
    async fn route_skips_disconnected_peers() {
        // Probe-then-release an ephemeral port → definitely refused.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = probe.local_addr().unwrap().port();
        drop(probe);

        let (registry, _) = make_registry();
        let card = fake_card(
            "dead-peer",
            &format!("ws://127.0.0.1:{dead_port}/ws"),
            vec![Capability::ComputerUse],
        );
        let peer_id = registry.add_peer_with_card(card).await.unwrap();
        let handle = registry.get(&peer_id).unwrap();
        assert!(
            wait_reconnecting(&handle).await,
            "peer should be Reconnecting on a dead port"
        );

        let coordinator = Coordinator::new(registry.clone());
        let result = coordinator
            .route_task(TaskRequest {
                required_capabilities: vec![Capability::ComputerUse],
                task: PeerTask {
                    instructions: "won't reach".into(),
                    context: serde_json::Value::Null,
                    client_correlation_id: None,
                },
            })
            .await;

        match result {
            Err(CoordinatorError::NoRoute { considered, .. }) => {
                assert_eq!(considered.len(), 1);
                assert_eq!(considered[0], peer_id);
            }
            other => panic!("expected NoRoute, got {other:?}"),
        }

        registry.remove_peer(&peer_id).await.unwrap();
    }

    /// When no peer in the registry advertises the required
    /// capabilities, even if they're all connected, the
    /// coordinator returns `NoRoute`.
    #[tokio::test]
    async fn route_returns_no_route_on_capability_mismatch() {
        let (port, gw) = spawn_gateway().await;
        let (registry, _) = make_registry();
        let card = fake_card(
            "no-phone",
            &format!("ws://127.0.0.1:{port}/ws"),
            vec![Capability::ComputerUse, Capability::Knowledge],
        );
        let peer_id = registry.add_peer_with_card(card).await.unwrap();
        let handle = registry.get(&peer_id).unwrap();
        assert!(wait_connected(&handle).await);

        let coordinator = Coordinator::new(registry.clone());
        let result = coordinator
            .route_task(TaskRequest {
                required_capabilities: vec![Capability::Phone],
                task: PeerTask {
                    instructions: "call someone".into(),
                    context: serde_json::Value::Null,
                    client_correlation_id: None,
                },
            })
            .await;

        match result {
            Err(CoordinatorError::NoRoute { required, .. }) => {
                assert!(matches!(&required[0], Capability::Phone));
            }
            other => panic!("expected NoRoute, got {other:?}"),
        }

        registry.remove_peer(&peer_id).await.unwrap();
        gw.abort();
    }

    /// When multiple connected peers match the required
    /// capabilities, the coordinator picks the one with the
    /// lexicographically smallest PeerId. Calling route_task
    /// again with the same state produces the same result.
    #[tokio::test]
    async fn stable_tiebreak_lexicographic_by_peer_id() {
        let (port_a, gw_a) = spawn_gateway().await;
        let (port_b, gw_b) = spawn_gateway().await;
        let (registry, _) = make_registry();

        // "bravo" sorts after "alpha".
        let card_a = fake_card(
            "alpha",
            &format!("ws://127.0.0.1:{port_a}/ws"),
            vec![Capability::ComputerUse],
        );
        let card_b = fake_card(
            "bravo",
            &format!("ws://127.0.0.1:{port_b}/ws"),
            vec![Capability::ComputerUse],
        );
        let id_a = registry.add_peer_with_card(card_a).await.unwrap();
        let id_b = registry.add_peer_with_card(card_b).await.unwrap();
        let ha = registry.get(&id_a).unwrap();
        let hb = registry.get(&id_b).unwrap();
        assert!(wait_connected(&ha).await, "alpha never connected");
        assert!(wait_connected(&hb).await, "bravo never connected");

        let coordinator = Coordinator::new(registry.clone());

        // Route twice — both times should pick "alpha" (sorts first).
        for _ in 0..2 {
            let routed = coordinator
                .route_task(TaskRequest {
                    required_capabilities: vec![Capability::ComputerUse],
                    task: PeerTask {
                        instructions: "do something".into(),
                        context: serde_json::Value::Null,
                        client_correlation_id: None,
                    },
                })
                .await
                .expect("route should succeed");
            assert_eq!(
                routed.peer_id, id_a,
                "expected alpha (lexicographically first), got {}",
                routed.peer_id
            );
        }

        registry.remove_peer(&id_a).await.unwrap();
        registry.remove_peer(&id_b).await.unwrap();
        gw_a.abort();
        gw_b.abort();
    }

    /// `eligible_peers` returns only the connected+matching subset,
    /// useful for UI "which peers could handle this?" queries.
    #[tokio::test]
    async fn eligible_peers_filters_correctly() {
        let (port, gw) = spawn_gateway().await;
        let (registry, _) = make_registry();
        let card = fake_card(
            "worker",
            &format!("ws://127.0.0.1:{port}/ws"),
            vec![Capability::ComputerUse, Capability::Knowledge],
        );
        let peer_id = registry.add_peer_with_card(card).await.unwrap();
        let handle = registry.get(&peer_id).unwrap();
        assert!(wait_connected(&handle).await);

        let coordinator = Coordinator::new(registry.clone());

        // Matches.
        let matches = coordinator.eligible_peers(&[Capability::ComputerUse]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id(), &peer_id);

        // Doesn't match (Phone not advertised).
        let no_match = coordinator.eligible_peers(&[Capability::Phone]);
        assert!(no_match.is_empty());

        registry.remove_peer(&peer_id).await.unwrap();
        gw.abort();
    }

    /// An empty registry produces `NoRoute` with an empty
    /// `considered` list.
    #[tokio::test]
    async fn route_on_empty_registry_returns_no_route() {
        let (registry, _) = make_registry();
        let coordinator = Coordinator::new(registry);
        let result = coordinator
            .route_task(TaskRequest {
                required_capabilities: vec![Capability::ComputerUse],
                task: PeerTask {
                    instructions: "nothing".into(),
                    context: serde_json::Value::Null,
                    client_correlation_id: None,
                },
            })
            .await;
        match result {
            Err(CoordinatorError::NoRoute { considered, .. }) => {
                assert!(considered.is_empty());
            }
            other => panic!("expected NoRoute, got {other:?}"),
        }
    }
}
