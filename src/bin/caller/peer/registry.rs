//! Server-side peer registry.
//!
//! Owns the `HashMap<PeerId, PeerHandle>` for all federated peers
//! the local daemon knows about, plus the construction path for
//! adding new peers (fetch Agent Card, pick a transport, spawn the
//! actor, store the handle).
//!
//! ## Log sink dependency injection
//!
//! The registry receives a pre-constructed
//! `mpsc::Sender<TaggedPeerEvent>` via its constructor and threads
//! it through to every peer actor's `spawn_peer` call. The writer
//! task on the receiver side is the caller's responsibility
//! (typically `main.rs` when it wires up the gateway) — keeping
//! the file I/O out of the registry makes it trivial to unit test
//! with a channel-based sink, and lets the caller choose between
//! JSONL file writer, in-memory buffer for tests, or a no-op
//! drain for throwaway diagnostic modes.
//!
//! ## Transport selection
//!
//! `add_peer` fetches a peer's Agent Card from its
//! `/.well-known/agent-card.json` URL, picks the first
//! [`TransportSpec`] in the card's `transports` list that this
//! build supports, constructs the corresponding transport, and
//! hands it to `spawn_peer`. Phase 1 only supports
//! [`IntendantWsTransport`]; non-Intendant transports in a card
//! are filtered out via `TransportSpec::Unknown` fallback (the
//! forward-compat discipline from the earlier pass) or skipped
//! explicitly for variants we recognize but haven't implemented
//! yet. If no supported transport is advertised, `add_peer` fails
//! cleanly with `PeerError::CardFetch`.

use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::event::TaggedPeerEvent;
use crate::peer::handle::{spawn_peer, PeerHandle, PeerSnapshot};
use crate::peer::id::PeerId;
use crate::peer::transport::IntendantWsTransport;
use crate::peer::PeerError;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

const CARD_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the registry's broadcast channel for [`RegistryEvent`].
///
/// Lossy by design: slow subscribers fall behind and skip rather than
/// blocking the registry. The HTTP `GET /api/peers` endpoint is the
/// recovery path — a subscriber that lags can re-sync from the full
/// list at any time.
pub const REGISTRY_BROADCAST_CAPACITY: usize = 64;

/// Push-stream event emitted by the registry when peer membership or
/// state changes. Consumed by the gateway translator that broadcasts
/// these to dashboard clients via the primary WebSocket so peer rows
/// update in-place without polling.
///
/// Snapshot-shaped (not delta-shaped): every event carries the full
/// [`PeerSnapshot`] for the affected peer (or just the id for removal),
/// so the browser handler treats each event as "replace the row" or
/// "remove the row" without reasoning about which fields changed.
#[derive(Debug, Clone)]
pub enum RegistryEvent {
    /// A peer was just added to the registry. The snapshot captures
    /// the peer's state at registration time (typically `Initializing`
    /// or `Connecting`).
    PeerAdded(PeerSnapshot),
    /// A peer was removed from the registry. Emitted before
    /// `PeerHandle::disconnect` is awaited so the dashboard updates
    /// immediately; any trailing `PeerStateChanged` from the per-peer
    /// observer task as the actor transitions to `Disconnected` will
    /// be ignored by the browser handler if the row is no longer in
    /// its local list.
    PeerRemoved(PeerId),
    /// A peer's connection state, status, or card changed. Carries a
    /// fresh snapshot reflecting the new values.
    PeerStateChanged(PeerSnapshot),
}

/// Server-side peer registry.
///
/// Cheap to clone — internally `Arc`-backed so the HTTP gateway,
/// the dashboard fan-out task, and the coordinator can all share
/// a reference without reboxing.
#[derive(Clone)]
pub struct PeerRegistry {
    inner: Arc<PeerRegistryInner>,
}

struct PeerRegistryInner {
    peers: RwLock<HashMap<PeerId, PeerHandle>>,
    log_sink: mpsc::Sender<TaggedPeerEvent>,
    events: broadcast::Sender<RegistryEvent>,
}

impl PeerRegistry {
    pub fn new(log_sink: mpsc::Sender<TaggedPeerEvent>) -> Self {
        let (events, _) = broadcast::channel(REGISTRY_BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(PeerRegistryInner {
                peers: RwLock::new(HashMap::new()),
                log_sink,
                events,
            }),
        }
    }

    /// Subscribe to the registry's push event stream. The receiver
    /// observes peer add / remove / state-change events for the lifetime
    /// of the registry. Lossy: lagging subscribers see
    /// [`broadcast::error::RecvError::Lagged`] and skip ahead. Recovery
    /// path is `GET /api/peers`, which always returns ground truth.
    pub fn subscribe(&self) -> broadcast::Receiver<RegistryEvent> {
        self.inner.events.subscribe()
    }

    /// Number of peers currently registered. Useful for tests and
    /// the aggregate dashboard indicator.
    pub fn len(&self) -> usize {
        self.inner.peers.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return a snapshot of all registered peer handles. Each
    /// handle is cheaply cloneable so the return value can be
    /// iterated and held without the read lock staying acquired.
    pub fn list(&self) -> Vec<PeerHandle> {
        self.inner.peers.read().unwrap().values().cloned().collect()
    }

    /// Look up a single peer by id.
    pub fn get(&self, id: &PeerId) -> Option<PeerHandle> {
        self.inner.peers.read().unwrap().get(id).cloned()
    }

    /// Fetch a peer's Agent Card from its `/.well-known/agent-card.json`
    /// URL, pick a supported transport, spawn the actor, and store
    /// the resulting handle. Returns the peer's id from the fetched
    /// card. If the peer is already registered (same id), returns
    /// [`PeerError::Rejected`] — idempotent re-registration is a
    /// follow-up concern.
    pub async fn add_peer(&self, card_url: &str) -> Result<PeerId, PeerError> {
        let card = fetch_card(card_url).await?;
        self.add_peer_with_card(card).await
    }

    /// Variant of [`add_peer`] that accepts a pre-fetched or
    /// locally-constructed card. Useful for:
    /// - Tests that don't want to spin up an HTTP fetch
    /// - Config-driven peer registration where the card is built
    ///   from `intendant.toml` `[[peer]]` sections
    /// - Loopback registration (registering the local daemon as a
    ///   "peer" of itself, for dashboard symmetry)
    pub async fn add_peer_with_card(&self, card: AgentCard) -> Result<PeerId, PeerError> {
        if self.inner.peers.read().unwrap().contains_key(&card.id) {
            return Err(PeerError::Rejected {
                code: "already_registered".into(),
                message: format!("peer {} is already in the registry", card.id),
            });
        }

        let spec = pick_supported_transport(&card.transports).ok_or_else(|| {
            PeerError::CardFetch(format!(
                "peer {} advertises no transport this build supports: {:?}",
                card.id, card.transports
            ))
        })?;

        let peer_id = card.id.clone();
        let log_sink = self.inner.log_sink.clone();

        let handle = spawn_peer(peer_id.clone(), card, log_sink, |events_tx| {
            build_transport(&spec, events_tx)
        });

        self.inner
            .peers
            .write()
            .unwrap()
            .insert(peer_id.clone(), handle.clone());

        // Emit the initial snapshot and start observing state changes.
        // Send errors are ignored on purpose: a registry with no current
        // subscribers is a normal startup state, not a failure mode —
        // the next subscriber will resync via `GET /api/peers`.
        let _ = self
            .inner
            .events
            .send(RegistryEvent::PeerAdded(handle.snapshot()));
        spawn_state_observer(handle, self.inner.events.clone());

        Ok(peer_id)
    }

    /// Remove a peer from the registry and request explicit
    /// disconnect on its handle. The actor task exits cleanly
    /// (transitions through Disconnecting → Disconnected) before
    /// this method returns.
    ///
    /// The `PeerRemoved` event is emitted *before* the disconnect
    /// completes so the dashboard reacts immediately. The per-peer
    /// observer task will exit cleanly when the handle's watch
    /// channels close as the actor terminates; any trailing
    /// `PeerStateChanged` it emits during the disconnecting transition
    /// is harmless (the browser handler ignores updates for unknown ids).
    pub async fn remove_peer(&self, id: &PeerId) -> Result<(), PeerError> {
        let handle = {
            let mut peers = self.inner.peers.write().unwrap();
            peers.remove(id)
        };
        let handle = handle.ok_or_else(|| PeerError::NotFound(id.as_str().to_string()))?;
        let _ = self
            .inner
            .events
            .send(RegistryEvent::PeerRemoved(id.clone()));
        handle.disconnect().await
    }
}

/// Spawn the per-peer observer task that watches a handle's
/// connection-state, status, and card watch channels and emits
/// [`RegistryEvent::PeerStateChanged`] whenever any of them change.
///
/// The task exits cleanly when all three watch sender sides close —
/// which happens automatically when the per-peer actor task terminates
/// (via explicit disconnect or transport-level shutdown). No cancellation
/// token is needed; the lifetime is tied to the handle's lifetime via
/// the watch channels.
fn spawn_state_observer(
    handle: PeerHandle,
    events: broadcast::Sender<RegistryEvent>,
) {
    tokio::spawn(async move {
        let mut conn_rx = handle.connection_updates();
        let mut status_rx = handle.status_updates();
        let mut card_rx = handle.card_updates();

        // Mark current values as observed so we only react to *changes*
        // from this point forward — the initial values are already
        // reflected in the `PeerAdded` snapshot the registry emitted.
        let _ = conn_rx.borrow_and_update();
        let _ = status_rx.borrow_and_update();
        let _ = card_rx.borrow_and_update();

        loop {
            let changed = tokio::select! {
                r = conn_rx.changed() => r,
                r = status_rx.changed() => r,
                r = card_rx.changed() => r,
            };
            if changed.is_err() {
                // One of the watch senders dropped — peer actor has
                // exited. Stop observing.
                break;
            }
            let _ = events.send(RegistryEvent::PeerStateChanged(handle.snapshot()));
        }
    });
}

/// Fetch an Agent Card from the given URL via HTTP GET.
///
/// Separate from [`IntendantWsTransport::fetch_agent_card`] because
/// the transport fetches as part of its own connect handshake (off
/// a WS URL), while the registry fetches from a card URL directly
/// (as provided by a user adding a peer). Small duplication; kept
/// here so the registry doesn't depend on a specific transport
/// implementation for its discovery step.
async fn fetch_card(card_url: &str) -> Result<AgentCard, PeerError> {
    let client = reqwest::Client::builder()
        .timeout(CARD_FETCH_TIMEOUT)
        .build()
        .map_err(|e| PeerError::CardFetch(format!("build http client: {e}")))?;
    let response = client
        .get(card_url)
        .send()
        .await
        .map_err(|e| PeerError::CardFetch(format!("GET {card_url}: {e}")))?;
    if !response.status().is_success() {
        return Err(PeerError::CardFetch(format!(
            "GET {card_url}: HTTP {}",
            response.status()
        )));
    }
    response
        .json::<AgentCard>()
        .await
        .map_err(|e| PeerError::CardFetch(format!("parse {card_url}: {e}")))
}

/// Walk the card's transports list and pick the first variant
/// this build supports. Phase 1: only `IntendantWs`. Unknown
/// variants (from forward-compat fallback) and unimplemented
/// variants are skipped.
fn pick_supported_transport(transports: &[TransportSpec]) -> Option<TransportSpec> {
    transports
        .iter()
        .find(|spec| matches!(spec, TransportSpec::IntendantWs { .. }))
        .cloned()
}

/// Build a concrete transport from a selected spec. Factored out
/// so the closure passed to `spawn_peer` stays readable.
fn build_transport(
    spec: &TransportSpec,
    events_tx: mpsc::Sender<crate::peer::event::PeerEvent>,
) -> Box<dyn crate::peer::traits::PeerTransport> {
    match spec {
        TransportSpec::IntendantWs { url } => {
            Box::new(IntendantWsTransport::new(url.clone(), events_tx))
        }
        other => {
            // Should be unreachable: `pick_supported_transport`
            // filters to variants this function knows. If we get
            // here it means somebody added a transport kind to
            // the selector without the matching constructor arm —
            // crash loudly rather than silently failing the spawn.
            panic!("unsupported transport spec reached build_transport: {other:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::card::{AuthScheme, Capability};
    use crate::peer::id::PeerKind;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
    use crate::event::EventBus;
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::Duration;

    /// Spin up a real web gateway on an ephemeral port and return
    /// `(port, gateway handle)`. Tests use this as a live peer
    /// target.
    async fn spawn_test_peer() -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(64);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        (port, handle)
    }

    /// Build a fake card for a synthetic peer. Used by tests that
    /// don't want to spin up an HTTP fetch path.
    fn fake_card(label: &str, ws_url: &str) -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::Intendant, label),
            label: label.to_string(),
            version: "0.1.0".into(),
            git_sha: Some("test".into()),
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.to_string(),
            }],
            capabilities: vec![Capability::ComputerUse, Capability::Knowledge],
            auth: AuthScheme::None,
        }
    }

    #[tokio::test]
    async fn new_registry_is_empty() {
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(16);
        let reg = PeerRegistry::new(log_tx);
        assert_eq!(reg.len(), 0);
        assert!(reg.is_empty());
        assert!(reg.list().is_empty());
    }

    /// End-to-end add_peer path: fetch the card from a live test
    /// gateway's `/.well-known/agent-card.json`, build an
    /// IntendantWsTransport, spawn the peer actor, store the
    /// handle. Verifies the registry mechanically integrates with
    /// the gateway's card endpoint added in the
    /// WebGatewayConfig split commit.
    #[tokio::test]
    async fn add_peer_fetches_card_and_registers() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let peer_id = reg.add_peer(&card_url).await.expect("add_peer succeeds");
        assert_eq!(peer_id.kind(), Some(PeerKind::Intendant));
        assert_eq!(reg.len(), 1);

        let handle = reg.get(&peer_id).expect("peer is in registry");
        assert_eq!(handle.id(), &peer_id);

        reg.remove_peer(&peer_id).await.unwrap();
        assert!(reg.is_empty());
        gateway.abort();
    }

    /// Adding the same peer twice (same id) rejects the second
    /// attempt instead of silently replacing the handle.
    #[tokio::test]
    async fn add_peer_rejects_duplicates() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        // Use the pre-fetched card path so both add_peer calls
        // deterministically target the same id regardless of
        // hostname resolution quirks.
        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("test-peer", &ws_url);

        let _first = reg.add_peer_with_card(card.clone()).await.unwrap();
        let second = reg.add_peer_with_card(card).await;
        match second {
            Err(PeerError::Rejected { code, .. }) => {
                assert_eq!(code, "already_registered");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(reg.len(), 1);
        gateway.abort();
    }

    /// A card with no supported transports fails cleanly. This
    /// guards the scenario where a peer advertises only future
    /// transport kinds (A2A, OpenClaw) that this build hasn't
    /// implemented yet — the registry should diagnose at add
    /// time, not silently attach to nothing.
    #[tokio::test]
    async fn add_peer_rejects_card_with_no_supported_transports() {
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(16);
        let reg = PeerRegistry::new(log_tx);

        let card = AgentCard {
            id: PeerId::new(PeerKind::OpenClaw, "future-peer"),
            label: "future-peer".into(),
            version: "9.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::A2A {
                url: "https://future/a2a".into(),
            }],
            capabilities: vec![],
            auth: AuthScheme::None,
        };

        match reg.add_peer_with_card(card).await {
            Err(PeerError::CardFetch(msg)) => {
                assert!(msg.contains("no transport"));
            }
            other => panic!("expected CardFetch error, got {other:?}"),
        }
        assert_eq!(reg.len(), 0);
    }

    /// `list()` returns handles that are safe to use after the
    /// lock is released.
    #[tokio::test]
    async fn list_returns_cloneable_handles() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("peer-a", &ws_url);
        let _ = reg.add_peer_with_card(card).await.unwrap();

        let peers = reg.list();
        assert_eq!(peers.len(), 1);
        // The handle remains usable after the registry's internal
        // read lock has been released.
        let h = peers.into_iter().next().unwrap();
        assert_eq!(h.id().as_str(), "intendant:peer-a");

        reg.remove_peer(h.id()).await.unwrap();
        gateway.abort();
    }

    /// `remove_peer` on an unknown id returns NotFound.
    #[tokio::test]
    async fn remove_unknown_peer_errors() {
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(16);
        let reg = PeerRegistry::new(log_tx);

        let unknown = PeerId::new(PeerKind::Intendant, "ghost");
        match reg.remove_peer(&unknown).await {
            Err(PeerError::NotFound(id)) => {
                assert_eq!(id, "intendant:ghost");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // RegistryEvent push-stream coverage
    // -----------------------------------------------------------------

    /// Registry emits `PeerAdded` carrying the new peer's initial
    /// snapshot. The snapshot reflects the card we registered with
    /// (label, version, capabilities) so the dashboard's row can be
    /// painted from this single event without a separate API roundtrip.
    #[tokio::test]
    async fn add_peer_emits_peer_added_event() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);
        let mut events = reg.subscribe();

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("subscriber-test", &ws_url);
        let id = reg.add_peer_with_card(card).await.unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("PeerAdded received within timeout")
            .expect("no recv error");
        match evt {
            RegistryEvent::PeerAdded(snap) => {
                assert_eq!(snap.id, "intendant:subscriber-test");
                assert_eq!(snap.label, "subscriber-test");
                assert!(!snap.capabilities.is_empty());
                assert!(snap.ws_url.is_some());
            }
            other => panic!("expected PeerAdded, got {other:?}"),
        }

        reg.remove_peer(&id).await.unwrap();
        gateway.abort();
    }

    /// Registry emits `PeerRemoved` when a peer is removed. The event
    /// carries the peer's id so the dashboard knows which row to drop.
    /// Trailing `PeerStateChanged` events from the per-peer observer
    /// task may also arrive (as the actor transitions to Disconnected)
    /// and the test tolerates them.
    #[tokio::test]
    async fn remove_peer_emits_peer_removed_event() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("remove-test", &ws_url);
        let id = reg.add_peer_with_card(card).await.unwrap();

        // Subscribe after add so we don't have to drain the PeerAdded.
        let mut events = reg.subscribe();
        reg.remove_peer(&id).await.unwrap();

        // Drain events for up to 2 seconds, looking for PeerRemoved
        // amid any trailing PeerStateChanged from the observer.
        let mut got_removed = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let evt = tokio::time::timeout(Duration::from_millis(100), events.recv()).await;
            if let Ok(Ok(RegistryEvent::PeerRemoved(removed_id))) = evt {
                assert_eq!(removed_id, id);
                got_removed = true;
                break;
            }
        }
        assert!(got_removed, "did not receive PeerRemoved within 2s");
        gateway.abort();
    }

    /// As the per-peer actor transitions through connection states,
    /// the observer task emits `PeerStateChanged` events with fresh
    /// snapshots. Verifies the watch-channel-driven push path works
    /// end-to-end (handle.snapshot read after a state change reflects
    /// the new state).
    #[tokio::test]
    async fn peer_state_changes_emit_push_events() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);
        let mut events = reg.subscribe();

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("state-test", &ws_url);
        let id = reg.add_peer_with_card(card).await.unwrap();

        // Drain PeerAdded, then look for at least one PeerStateChanged
        // (the actor will progress from Initializing → Connecting →
        // Connected as the test peer accepts the WebSocket).
        let mut saw_state_changed = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let evt = tokio::time::timeout(Duration::from_millis(200), events.recv()).await;
            match evt {
                Ok(Ok(RegistryEvent::PeerStateChanged(snap))) => {
                    assert_eq!(snap.id, "intendant:state-test");
                    saw_state_changed = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(
            saw_state_changed,
            "did not observe a PeerStateChanged event within 3s"
        );

        reg.remove_peer(&id).await.unwrap();
        gateway.abort();
    }

    /// The registry's broadcast supports multiple concurrent
    /// subscribers; both receive the same events. Validates that
    /// `subscribe()` is multi-consumer (each call returns an
    /// independent receiver).
    #[tokio::test]
    async fn multiple_subscribers_receive_same_events() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);
        let mut sub_a = reg.subscribe();
        let mut sub_b = reg.subscribe();

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("multi-sub", &ws_url);
        let id = reg.add_peer_with_card(card).await.unwrap();

        let evt_a = tokio::time::timeout(Duration::from_secs(1), sub_a.recv())
            .await
            .expect("sub_a timeout")
            .expect("sub_a recv error");
        let evt_b = tokio::time::timeout(Duration::from_secs(1), sub_b.recv())
            .await
            .expect("sub_b timeout")
            .expect("sub_b recv error");

        assert!(matches!(evt_a, RegistryEvent::PeerAdded(_)));
        assert!(matches!(evt_b, RegistryEvent::PeerAdded(_)));

        reg.remove_peer(&id).await.unwrap();
        gateway.abort();
    }

    /// `pick_supported_transport` skips variants this build
    /// doesn't support, including the `Unknown` forward-compat
    /// fallback and future transport kinds like A2A.
    #[test]
    fn pick_supported_transport_filters_unsupported() {
        let transports = vec![
            TransportSpec::Unknown,
            TransportSpec::A2A {
                url: "https://x".into(),
            },
            TransportSpec::IntendantWs {
                url: "ws://x/ws".into(),
            },
        ];
        let picked = pick_supported_transport(&transports).unwrap();
        assert!(matches!(picked, TransportSpec::IntendantWs { .. }));
    }

    /// Returns None when no supported variant is in the list.
    #[test]
    fn pick_supported_transport_returns_none_when_empty() {
        let transports = vec![
            TransportSpec::Unknown,
            TransportSpec::A2A {
                url: "https://x".into(),
            },
        ];
        assert!(pick_supported_transport(&transports).is_none());
    }
}
