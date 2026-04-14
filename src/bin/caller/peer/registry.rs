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
use crate::peer::handle::{spawn_peer, PeerHandle};
use crate::peer::id::PeerId;
use crate::peer::transport::IntendantWsTransport;
use crate::peer::PeerError;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

const CARD_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

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
}

impl PeerRegistry {
    pub fn new(log_sink: mpsc::Sender<TaggedPeerEvent>) -> Self {
        Self {
            inner: Arc::new(PeerRegistryInner {
                peers: RwLock::new(HashMap::new()),
                log_sink,
            }),
        }
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
            .insert(peer_id.clone(), handle);
        Ok(peer_id)
    }

    /// Remove a peer from the registry and request explicit
    /// disconnect on its handle. The actor task exits cleanly
    /// (transitions through Disconnecting → Disconnected) before
    /// this method returns.
    pub async fn remove_peer(&self, id: &PeerId) -> Result<(), PeerError> {
        let handle = {
            let mut peers = self.inner.peers.write().unwrap();
            peers.remove(id)
        };
        let handle = handle.ok_or_else(|| PeerError::NotFound(id.as_str().to_string()))?;
        handle.disconnect().await
    }
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
