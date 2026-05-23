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
use crate::peer::event::{PeerEvent, TaggedPeerEvent};
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
    /// A peer-emitted [`PeerEvent`] forwarded from the per-peer
    /// transport's broadcast. Lets the dashboard subscribe to
    /// per-peer activity (logs, model output, approval requests,
    /// etc.) through the same registry channel as add/remove/state-
    /// change events instead of opening a separate WebSocket per
    /// peer from the browser.
    ///
    /// This is the server-side leg of the per-peer event stream
    /// migration: the local primary daemon already has the peer's
    /// PeerEvent stream via `PeerHandle::subscribe`, so reflecting
    /// those events out to dashboard clients through the existing
    /// push pipe is cheap and removes the browser's per-secondary
    /// WebSocket plumbing once the UI side migrates.
    PeerEventForwarded { peer: PeerId, event: PeerEvent },
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
        self.add_peer_with_via_and_auth(card_url, Vec::new(), None)
            .await
    }

    /// Variant of [`add_peer`] that lets the connecting operator
    /// override the card's transport URLs at peer-add time. See
    /// [`add_peer_with_via_and_auth`] for the auth-aware variant.
    pub async fn add_peer_with_via(
        &self,
        card_url: &str,
        via_urls: Vec<String>,
    ) -> Result<PeerId, PeerError> {
        self.add_peer_with_via_and_auth(card_url, via_urls, None)
            .await
    }

    /// Wrapper over [`add_peer_with_credentials`] that doesn't pass
    /// pinned-fingerprint overrides or a browser-side TCP via URL.
    /// Kept for existing call sites that don't need either; new code
    /// should use `add_peer_with_credentials` directly.
    pub async fn add_peer_with_via_and_auth(
        &self,
        card_url: &str,
        via_urls: Vec<String>,
        bearer_token: Option<String>,
    ) -> Result<PeerId, PeerError> {
        self.add_peer_with_credentials(card_url, via_urls, bearer_token, Vec::new(), None)
            .await
    }

    /// Full add-peer entry point: card_url + optional via override +
    /// optional outbound bearer token + optional operator-side pinned
    /// fingerprint override. The token is sent on the initial card
    /// fetch (via `fetch_card`) and stored on the per-peer transport
    /// for subsequent requests.
    ///
    /// When `via_urls` is non-empty, the fetched card's `transports`
    /// field is replaced with one [`TransportSpec::IntendantWs`] per
    /// supplied URL, in the given preference order — the operator
    /// is asserting "reach this peer at these URLs, ignore what its
    /// card advertised."
    ///
    /// When `override_pinned_fingerprints` is non-empty, the fetched
    /// card's `auth.transport` is REPLACED with a fresh
    /// `PinnedMutualTls { server_cert_fingerprints: override... }`.
    /// Eliminates the TOFU window from card-driven pinning: the
    /// operator distrusts the card's auth claim and pins against
    /// fingerprints they got out-of-band. Empty list (the default)
    /// preserves the card's auth.transport unchanged — that's the
    /// "trust the card" path covered by follow-up B's auto-advertise.
    ///
    /// Use cases for `via_urls`:
    /// - Connecting daemon knows about a port-forward / proxy / named
    ///   tunnel that the advertising peer's card doesn't list.
    /// - Connecting daemon is on a Tailscale tailnet that the
    ///   advertising peer is also on, but the peer's card only lists
    ///   its LAN URL because that's what its `--advertise-url` set up.
    /// - Operator wants to force a specific path for testing.
    ///
    /// Use case for `bearer_token`: the peer requires application-
    /// layer auth (its card advertises
    /// `auth.application = Some(Bearer)`) and the operator has the
    /// matching credential in `[[peer]] bearer_token` in
    /// intendant.toml.
    ///
    /// Use case for `override_pinned_fingerprints`: the peer's card
    /// declares some fingerprint set (or none), and the operator
    /// wants to substitute their own out-of-band-trusted fingerprint
    /// list. Replaces the entire `auth.transport` field with
    /// `PinnedMutualTls { server_cert_fingerprints: override }`.
    ///
    /// The card's identity (`id`, `label`, `version`, `capabilities`,
    /// `auth.application`) is preserved — only `transports` and
    /// (conditionally) `auth.transport` are overridden. The peer is
    /// still uniquely identified by `card.id`, so duplicate
    /// registration still rejects with the same error.
    ///
    /// `browser_tcp_via_url` is orthogonal operator metadata: the
    /// URL the **browser** uses to reach the peer's HTTP port for
    /// WebRTC ICE-TCP. Stored on the resulting [`PeerHandle`] and
    /// surfaced to the dashboard via
    /// [`PeerSnapshot::browser_tcp_via_url`]; the dashboard sends it
    /// as the `advertise_tcp_via_url` hint in federated WebRTC
    /// offers. `None` falls back to the primary-side via URL (slice
    /// 3a.2 behavior). See [`crate::project::PeerConfig`]'s field
    /// of the same name for the motivation.
    pub async fn add_peer_with_credentials(
        &self,
        card_url: &str,
        via_urls: Vec<String>,
        bearer_token: Option<String>,
        override_pinned_fingerprints: Vec<String>,
        browser_tcp_via_url: Option<String>,
    ) -> Result<PeerId, PeerError> {
        let mut card = fetch_card(
            card_url,
            bearer_token.as_deref(),
            &override_pinned_fingerprints,
        )
        .await?;
        // Apply the via-URL override to the initial card so the
        // first PeerSnapshot the dashboard sees (before the actor
        // completes its first connect) shows the operator's URL
        // — not the peer's self-advertised one that's likely
        // unreachable in NAT / tunnel topologies. Passed separately
        // to `add_peer_with_card_and_auth` so the actor can re-apply
        // it on every reconnect (the transport's `fetch_agent_card`
        // on reconnect would otherwise wipe it).
        if !via_urls.is_empty() {
            card.transports = via_urls
                .iter()
                .cloned()
                .map(|url| TransportSpec::IntendantWs { url })
                .collect();
        }
        if !override_pinned_fingerprints.is_empty() {
            card.auth.transport = crate::peer::card::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints: override_pinned_fingerprints,
            };
        }
        self.add_peer_with_card_and_auth(card, via_urls, bearer_token, browser_tcp_via_url)
            .await
    }

    /// Variant of [`add_peer`] that accepts a pre-fetched or
    /// locally-constructed card. Useful for:
    /// - Tests that don't want to spin up an HTTP fetch
    /// - Config-driven peer registration where the card is built
    ///   from `intendant.toml` `[[peer]]` sections
    /// - Loopback registration (registering the local daemon as a
    ///   "peer" of itself, for dashboard symmetry)
    pub async fn add_peer_with_card(&self, card: AgentCard) -> Result<PeerId, PeerError> {
        self.add_peer_with_card_and_auth(card, Vec::new(), None, None)
            .await
    }

    /// Auth-aware variant of [`add_peer_with_card`] — stores the
    /// outbound bearer token on each per-peer transport so it's
    /// sent on subsequent connects (the agent-card HTTP fetch and
    /// the WebSocket upgrade), and parses pinned fingerprints from
    /// the card's `auth.transport = PinnedMutualTls` for use by the
    /// custom rustls verifier on subsequent TLS connects.
    ///
    /// Pinning fingerprints come *from the card* (TOFU-style) — the
    /// initial card fetch in `fetch_card` happens with default trust,
    /// and once we have the card we use its declared fingerprints
    /// for every subsequent connect. The card itself is the trust
    /// assertion. Operator-side override (out-of-band fingerprint
    /// supplied via `[[peer]]` config) is a follow-up that goes
    /// through this same path.
    pub async fn add_peer_with_card_and_auth(
        &self,
        card: AgentCard,
        via_urls: Vec<String>,
        bearer_token: Option<String>,
        browser_tcp_via_url: Option<String>,
    ) -> Result<PeerId, PeerError> {
        if self.inner.peers.read().unwrap().contains_key(&card.id) {
            return Err(PeerError::Rejected {
                code: "already_registered".into(),
                message: format!("peer {} is already in the registry", card.id),
            });
        }

        // Filter the card's transports to the ones this build can speak,
        // preserving the card's preference order. The first one whose
        // `connect()` succeeds wins (handled by `MultiTransport`).
        let supported_specs = pick_supported_transports(&card.transports);
        if supported_specs.is_empty() {
            return Err(PeerError::CardFetch(format!(
                "peer {} advertises no transport this build supports: {:?}",
                card.id, card.transports
            )));
        }

        // Parse pinned fingerprints once, surface parse errors at
        // registry-add time rather than at first connect. Empty
        // fingerprint list (when the card doesn't require pinning)
        // is the no-op case — transports treat empty as "default
        // TLS verification."
        let pinned_fingerprints =
            parse_card_pinned_fingerprints(&card.auth.transport).map_err(|e| {
                PeerError::Auth(format!(
                    "peer {} card has invalid pinned fingerprint: {e}",
                    card.id
                ))
            })?;

        let peer_id = card.id.clone();
        let log_sink = self.inner.log_sink.clone();

        let handle = spawn_peer(
            peer_id.clone(),
            card,
            via_urls,
            browser_tcp_via_url,
            log_sink,
            move |events_tx| {
                // Build one concrete transport per supported spec (each
                // gets its own clone of `events_tx`, `bearer_token`, and
                // `pinned_fingerprints`) and wrap them in a
                // `MultiTransport` that probes them in order on connect.
                let candidates: Vec<Box<dyn crate::peer::traits::PeerTransport>> = supported_specs
                    .iter()
                    .map(|spec| {
                        build_transport(
                            spec,
                            events_tx.clone(),
                            bearer_token.clone(),
                            pinned_fingerprints.clone(),
                        )
                    })
                    .collect();
                Box::new(crate::peer::transport::MultiTransport::new(candidates))
            },
        );

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
        spawn_state_observer(handle.clone(), self.inner.events.clone());
        spawn_event_forwarder(handle, self.inner.events.clone());

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
fn spawn_state_observer(handle: PeerHandle, events: broadcast::Sender<RegistryEvent>) {
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

/// Spawn the per-peer event forwarder task. Subscribes to the peer's
/// [`PeerEvent`] broadcast (via `PeerHandle::subscribe`) and republishes
/// each event as [`RegistryEvent::PeerEventForwarded`] tagged with the
/// peer's id, so the registry's single broadcast carries both
/// membership/state events *and* per-peer activity events.
///
/// Lossy on the input side: if the dashboard fan-out lags behind a
/// chatty peer (e.g. streaming model deltas), the per-peer broadcast's
/// `Lagged` error is logged at debug and the forwarder skips ahead.
/// This is intentional — durable per-peer events land on the session
/// log via the registry's [`TaggedPeerEvent`] log sink, so the dashboard
/// can drop intermediate frames safely.
///
/// Exits cleanly when the per-peer actor's broadcast sender drops
/// (`RecvError::Closed`), same lifetime model as `spawn_state_observer`.
///
/// Critical: we extract `peer_id` and `peer_events` *before* spawning,
/// so the closure captures only those — not the `PeerHandle` itself.
/// `PeerHandle` holds a clone of the peer's broadcast `Sender` inside
/// `PeerHandleInner`; if the spawn closure captured the handle, that
/// clone would keep the channel alive and `peer_events.recv()` would
/// never see `Closed` even after the peer actor exits and the registry
/// drops its handle. The result would be one stuck task per peer-add
/// over the lifetime of the registry.
fn spawn_event_forwarder(handle: PeerHandle, events: broadcast::Sender<RegistryEvent>) {
    let peer_id = handle.id().clone();
    let mut peer_events = handle.subscribe();
    // Drop the handle before the spawn so its inner broadcast Sender
    // refcount is released. The Receiver we just took stays valid
    // independently — broadcast Receivers can outlive every Sender,
    // they just see `Closed` once all Senders drop.
    drop(handle);
    tokio::spawn(async move {
        loop {
            match peer_events.recv().await {
                Ok(event) => {
                    let _ = events.send(RegistryEvent::PeerEventForwarded {
                        peer: peer_id.clone(),
                        event,
                    });
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow consumers fall behind. Skip the missed
                    // window and keep streaming the current event.
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Fetch an Agent Card from the given URL via HTTP GET, optionally
/// sending `Authorization: Bearer <token>`.
///
/// Separate from [`IntendantWsTransport::fetch_agent_card`] because
/// the transport fetches as part of its own connect handshake (off
/// a WS URL), while the registry fetches from a card URL directly
/// (as provided by a user adding a peer). Small duplication; kept
/// here so the registry doesn't depend on a specific transport
/// implementation for its discovery step.
///
/// When `pinned_fingerprints` is non-empty, the HTTP client is built
/// with the same custom rustls config the transport's
/// [`IntendantWsTransport::fetch_agent_card`] uses — pinning the
/// server cert's SHA-256 fingerprint instead of validating against
/// the webpki root store. Without this, an operator who supplies
/// out-of-band fingerprints for a self-signed-TLS peer (the only
/// thing they *can* supply, since the cert isn't CA-issued) would
/// still hit a cert-validation failure on this discovery fetch
/// before the pin-aware transport layer ever runs. Threading the
/// pin here keeps the discovery fetch and the connect fetch on the
/// same trust decision.
async fn fetch_card(
    card_url: &str,
    bearer_token: Option<&str>,
    pinned_fingerprints: &[String],
) -> Result<AgentCard, PeerError> {
    let mut client_builder = reqwest::Client::builder().timeout(CARD_FETCH_TIMEOUT);
    if !pinned_fingerprints.is_empty() {
        let verifier = crate::peer::transport::pinning::PinnedFingerprintVerifier::from_strings(
            pinned_fingerprints,
        )
        .map_err(|e| PeerError::Auth(format!("invalid pinned fingerprint: {e}")))?;
        let config = crate::peer::transport::pinning::pinned_client_config(verifier);
        client_builder = client_builder.use_preconfigured_tls(config);
    }
    let client = client_builder
        .build()
        .map_err(|e| PeerError::CardFetch(format!("build http client: {e}")))?;
    let mut request = client.get(card_url);
    if let Some(token) = bearer_token {
        request = request.bearer_auth(token);
    }
    let response = request
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

/// Filter the card's transports list to the variants this build
/// supports, preserving the card's original preference order.
/// Unknown variants (from forward-compat fallback) and unimplemented
/// variants are skipped. Returns an empty `Vec` when the card
/// advertises only transports we don't speak — the caller treats
/// that as a hard failure.
///
/// Phase 1: only `IntendantWs`. As A2A / OpenClaw / MCP transports
/// land, add their match arms here so cards advertising mixed
/// transport lists (e.g. an Intendant peer that also exposes A2A)
/// produce a multi-element supported set the connecting daemon can
/// probe through.
fn pick_supported_transports(transports: &[TransportSpec]) -> Vec<TransportSpec> {
    transports
        .iter()
        .filter(|spec| matches!(spec, TransportSpec::IntendantWs { .. }))
        .cloned()
        .collect()
}

/// Build a concrete transport from a selected spec. Factored out
/// so the closure passed to `spawn_peer` stays readable.
fn build_transport(
    spec: &TransportSpec,
    events_tx: mpsc::Sender<crate::peer::event::PeerEvent>,
    bearer_token: Option<String>,
    pinned_fingerprints: Vec<crate::peer::transport::pinning::Fingerprint>,
) -> Box<dyn crate::peer::traits::PeerTransport> {
    match spec {
        TransportSpec::IntendantWs { url } => Box::new(IntendantWsTransport::with_credentials(
            url.clone(),
            events_tx,
            crate::peer::transport::intendant::TransportCredentials {
                bearer_token,
                pinned_fingerprints,
            },
        )),
        other => {
            // Should be unreachable: `pick_supported_transports`
            // filters to variants this function knows. If we get
            // here it means somebody added a transport kind to
            // the selector without the matching constructor arm —
            // crash loudly rather than silently failing the spawn.
            panic!("unsupported transport spec reached build_transport: {other:?}")
        }
    }
}

/// Parse pinned fingerprints from a card's `TransportAuth`.
/// Returns an empty Vec when the card doesn't require pinning
/// (None / MutualTls / Unknown), or a Vec of pre-parsed fingerprints
/// when the card requires `PinnedMutualTls`. Parse errors are
/// returned so the registry surfaces them at peer-add time rather
/// than at first connect.
fn parse_card_pinned_fingerprints(
    transport_auth: &crate::peer::card::TransportAuth,
) -> Result<Vec<crate::peer::transport::pinning::Fingerprint>, String> {
    use crate::peer::card::TransportAuth;
    match transport_auth {
        TransportAuth::PinnedMutualTls {
            server_cert_fingerprints,
        } => {
            let mut out = Vec::with_capacity(server_cert_fingerprints.len());
            for s in server_cert_fingerprints {
                let fp = crate::peer::transport::pinning::parse_fingerprint(s)?;
                out.push(fp);
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::peer::card::{AuthRequirements, Capability};
    use crate::peer::id::PeerKind;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::Duration;

    /// Spin up a real web gateway on an ephemeral port and return
    /// `(port, gateway handle)`. Tests use this as a live peer
    /// target.
    async fn spawn_test_peer() -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(64);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
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
            auth: AuthRequirements::none(),
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
            auth: AuthRequirements::none(),
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

    /// Per-peer events emitted by a peer's transport (PeerEvent::Connected,
    /// ActivityStarted, Log, ApprovalRequested, etc.) are forwarded as
    /// `RegistryEvent::PeerEventForwarded` tagged with the originating
    /// peer's id. Verifies the spawn_event_forwarder path end-to-end —
    /// when the IntendantWsTransport completes its handshake with the
    /// target gateway, the actor emits PeerEvent::Connected, the per-peer
    /// broadcast surfaces it, and the forwarder republishes it through
    /// the registry's channel.
    #[tokio::test]
    async fn peer_events_are_forwarded() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);
        let mut events = reg.subscribe();

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("forward-test", &ws_url);
        let id = reg.add_peer_with_card(card).await.unwrap();

        // Drain everything until we see a PeerEventForwarded with a
        // matching peer id, or time out. The target gateway is idle
        // so the only PeerEvent we can rely on is the Connected one
        // emitted by the actor after its first successful handshake.
        let mut saw_forwarded = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let evt = tokio::time::timeout(Duration::from_millis(200), events.recv()).await;
            match evt {
                Ok(Ok(RegistryEvent::PeerEventForwarded { peer, .. })) => {
                    assert_eq!(peer, id);
                    saw_forwarded = true;
                    break;
                }
                Ok(Ok(_)) => continue, // PeerAdded / PeerStateChanged
                _ => break,
            }
        }
        assert!(
            saw_forwarded,
            "did not observe a PeerEventForwarded event within 3s"
        );

        reg.remove_peer(&id).await.unwrap();
        gateway.abort();
    }

    /// Regression guard for a leak in `spawn_event_forwarder`: an
    /// earlier version captured the full `PeerHandle` in the spawn
    /// closure, which kept the per-peer broadcast `Sender` alive
    /// inside `PeerHandleInner`. The forwarder's `peer_events.recv()`
    /// then never saw `RecvError::Closed` after the actor exited and
    /// the registry dropped its handle — one stuck task per peer-add
    /// over the registry's lifetime.
    ///
    /// Verifies the fix by subscribing to the peer's broadcast
    /// independently, removing the peer, and asserting the receiver
    /// observes `Closed` within a reasonable deadline. If the
    /// forwarder were still holding a Sender, our independent
    /// receiver would never see Closed and the timeout would fire.
    #[tokio::test]
    async fn event_forwarder_releases_handle_after_remove() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let card = fake_card("leak-test", &ws_url);
        let id = reg.add_peer_with_card(card).await.unwrap();

        // Subscribe to the peer's broadcast directly, then drop our
        // handle so we don't ourselves keep a Sender alive. The
        // registry still holds one, plus the actor holds one.
        let handle = reg.get(&id).expect("peer in registry");
        let mut peer_events = handle.subscribe();
        drop(handle);

        // Removing the peer drops the registry's handle and signals
        // the actor to disconnect. After the actor's run() exits
        // (dropping its events_out_tx) and any forwarder task
        // releases its handle, our independent receiver should see
        // RecvError::Closed.
        reg.remove_peer(&id).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match peer_events.recv().await {
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "broadcast did not close within 3s after remove — \
             spawn_event_forwarder is leaking the PeerHandle"
        );

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

    /// Regression guard: via_urls must persist across the actor's
    /// first successful connect. The transport's `fetch_agent_card()`
    /// on connect returns a fresh card (with the peer's
    /// self-advertised transports); without intervention that card
    /// overwrites the registry's initial patched card in the watch
    /// channel, which wipes the operator's override. The actor
    /// reapplies via_urls on every fresh card it publishes — this
    /// test exercises that path end-to-end against a real gateway.
    ///
    /// Before the fix, `card_snapshot().transports` would revert to
    /// the gateway's auto-detected URLs within ~100ms of add. After
    /// the fix, it stays pinned to the via URL for the lifetime of
    /// the actor (including across reconnects — not exercised here
    /// because tearing down the gateway mid-test is fragile; the
    /// preservation logic is the same in both paths of
    /// `PeerActor::run` that call `apply_via_override`).
    #[tokio::test]
    async fn add_peer_with_via_persists_across_initial_card_refresh() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        // Reach the test gateway via localhost, but declare a via URL
        // that's the SAME localhost:port pattern so the actor can
        // actually connect. The test would also work with an
        // unreachable via URL (actor just retries forever) but
        // connectable is more faithful to the real-world case.
        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let via_url = format!("ws://127.0.0.1:{port}/ws");
        let via_urls = vec![via_url.clone()];
        let peer_id = reg
            .add_peer_with_via(&card_url, via_urls)
            .await
            .expect("add_peer_with_via succeeds");
        let handle = reg.get(&peer_id).expect("peer is in registry");

        // Wait until the actor has finished its first connect — the
        // exact bug window. Without the fix, the card in the watch
        // channel flips from the via URL to the gateway's
        // auto-detected URLs as soon as ConnectionState becomes
        // Connected.
        let mut conn_rx = handle.connection_updates();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(*conn_rx.borrow(), crate::peer::ConnectionState::Connected) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "actor didn't reach Connected within 2s; current state: {:?}",
                    *conn_rx.borrow()
                );
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let _ = tokio::time::timeout(remaining, conn_rx.changed()).await;
        }

        // Give the card watch channel a tick to settle after the
        // Connected transition — the actor sends card_tx before
        // connection_tx in `run()` so by the time we see Connected
        // the card is already updated, but tests run fast enough
        // that a short yield keeps this robust against scheduling.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let card = handle.card_snapshot();
        assert_eq!(
            card.transports.len(),
            1,
            "via override should yield exactly one transport; got: {:?}",
            card.transports
        );
        match &card.transports[0] {
            TransportSpec::IntendantWs { url } => {
                assert_eq!(
                    url, &via_url,
                    "via URL should persist across the actor's connect-time card refresh"
                );
            }
            other => panic!("expected IntendantWs, got {other:?}"),
        }

        reg.remove_peer(&peer_id).await.unwrap();
        gateway.abort();
    }

    /// `add_peer_with_via` overrides the card's transports with the
    /// operator-supplied URLs. The card's identity (id, label,
    /// capabilities, auth) is preserved — only `transports` changes.
    /// This is the connecting-side knob for cases where the operator
    /// knows the topology better than the advertising peer's card
    /// does (port-forwards, proxies, named tunnels).
    #[tokio::test]
    async fn add_peer_with_via_replaces_card_transports() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let via_urls = vec!["ws://override.example:9999/ws".to_string()];
        let peer_id = reg
            .add_peer_with_via(&card_url, via_urls.clone())
            .await
            .expect("add_peer_with_via succeeds even when via target isn't reachable — connect happens async");

        let handle = reg.get(&peer_id).expect("peer is in registry");
        let card = handle.card_snapshot();

        // Transports replaced with the via URLs verbatim.
        assert_eq!(card.transports.len(), 1);
        match &card.transports[0] {
            TransportSpec::IntendantWs { url } => {
                assert_eq!(url, "ws://override.example:9999/ws");
            }
            other => panic!("expected IntendantWs, got {other:?}"),
        }

        // Identity preserved — id from the original card, label
        // present (exact value depends on the test host's
        // `resolve_host_label`, so just assert non-empty).
        assert_eq!(card.id, peer_id);
        assert!(!card.label.is_empty(), "label preserved from card");

        reg.remove_peer(&peer_id).await.unwrap();
        gateway.abort();
    }

    /// Empty `via_urls` is the no-op case — `add_peer_with_via` falls
    /// through to the default fetch-and-store behavior, leaving the
    /// card's transports untouched. This is what `add_peer` itself
    /// calls under the hood.
    #[tokio::test]
    async fn add_peer_with_via_empty_preserves_card_transports() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let peer_id = reg
            .add_peer_with_via(&card_url, Vec::new())
            .await
            .expect("add_peer_with_via with empty via succeeds");

        let handle = reg.get(&peer_id).expect("peer is in registry");
        let card = handle.card_snapshot();

        // The card's original transports survive — at least one entry
        // and at least one is IntendantWs (the gateway's auto-detected
        // advertise list).
        assert!(!card.transports.is_empty(), "card has transports");
        assert!(
            card.transports
                .iter()
                .any(|t| matches!(t, TransportSpec::IntendantWs { .. })),
            "at least one IntendantWs transport survived: {:?}",
            card.transports
        );

        reg.remove_peer(&peer_id).await.unwrap();
        gateway.abort();
    }

    /// Multiple via URLs all become `IntendantWs` entries in the card
    /// in the supplied preference order. `MultiTransport` then probes
    /// them top-down on connect (covered by separate MultiTransport
    /// tests in `peer/transport/multi.rs`).
    #[tokio::test]
    async fn add_peer_with_via_multiple_urls_preserve_order() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let via_urls = vec![
            "ws://primary.example:9001/ws".to_string(),
            "ws://fallback.example:9002/ws".to_string(),
        ];
        let peer_id = reg
            .add_peer_with_via(&card_url, via_urls.clone())
            .await
            .expect("add_peer_with_via succeeds");

        let handle = reg.get(&peer_id).expect("peer is in registry");
        let card = handle.card_snapshot();

        assert_eq!(card.transports.len(), 2);
        let urls: Vec<&String> = card
            .transports
            .iter()
            .filter_map(|t| match t {
                TransportSpec::IntendantWs { url } => Some(url),
                _ => None,
            })
            .collect();
        assert_eq!(urls, vec![&via_urls[0], &via_urls[1]]);

        reg.remove_peer(&peer_id).await.unwrap();
        gateway.abort();
    }

    /// `add_peer_with_credentials` with non-empty
    /// `override_pinned_fingerprints` REPLACES the card's
    /// `auth.transport` with `PinnedMutualTls` containing the
    /// operator-supplied fingerprints. Eliminates the TOFU window
    /// when the operator got the fingerprint out-of-band.
    #[tokio::test]
    async fn add_peer_with_credentials_pinned_override_replaces_card_auth() {
        use crate::peer::card::TransportAuth;
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        // Operator-supplied override — must end up in the stored card's
        // auth.transport regardless of what the fetched card claimed.
        let override_pinned =
            vec!["11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff".to_string()];
        let peer_id = reg
            .add_peer_with_credentials(&card_url, Vec::new(), None, override_pinned.clone(), None)
            .await
            .expect("add_peer_with_credentials with valid override succeeds");

        let handle = reg.get(&peer_id).expect("peer is in registry");
        let card = handle.card_snapshot();
        match &card.auth.transport {
            TransportAuth::PinnedMutualTls {
                server_cert_fingerprints,
            } => {
                assert_eq!(server_cert_fingerprints, &override_pinned);
            }
            other => panic!("expected PinnedMutualTls override, got {other:?}"),
        }
        // Application-layer auth from the original card is preserved
        // (override only touches transport, not application).
        assert!(
            card.auth.application.is_none(),
            "application auth preserved (None in test fixture)"
        );

        reg.remove_peer(&peer_id).await.unwrap();
        gateway.abort();
    }

    /// `add_peer_with_credentials` with empty `override_pinned_fingerprints`
    /// preserves the card's `auth.transport` exactly. The empty case is
    /// the no-op default — operator hasn't configured pinning, trust the
    /// card's claim.
    #[tokio::test]
    async fn add_peer_with_credentials_empty_override_preserves_card_auth() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let peer_id = reg
            .add_peer_with_credentials(&card_url, Vec::new(), None, Vec::new(), None)
            .await
            .expect("empty override succeeds");

        let handle = reg.get(&peer_id).expect("peer is in registry");
        let card = handle.card_snapshot();
        // Test gateway advertises None auth by default; preserved.
        assert_eq!(
            card.auth,
            crate::peer::card::AuthRequirements::none(),
            "card auth preserved unchanged"
        );

        reg.remove_peer(&peer_id).await.unwrap();
        gateway.abort();
    }

    /// `add_peer_with_credentials` with malformed override
    /// fingerprint fails at registry-add time with `PeerError::Auth`
    /// (because `add_peer_with_card_and_auth` parses every pinned
    /// fingerprint via `parse_card_pinned_fingerprints` and that's
    /// where the format error surfaces).
    #[tokio::test]
    async fn add_peer_with_credentials_rejects_malformed_override() {
        let (port, gateway) = spawn_test_peer().await;
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);

        let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
        let result = reg
            .add_peer_with_credentials(
                &card_url,
                Vec::new(),
                None,
                vec!["not-a-fingerprint".into()],
                None,
            )
            .await;
        match result {
            Err(PeerError::Auth(msg)) => {
                assert!(msg.contains("invalid pinned fingerprint"), "msg: {msg}");
            }
            other => panic!("expected PeerError::Auth, got {other:?}"),
        }
        assert!(reg.is_empty());
        gateway.abort();
    }

    /// `parse_card_pinned_fingerprints` returns an empty Vec when the
    /// card's `auth.transport` doesn't require pinning. None /
    /// MutualTls / Unknown all skip the pinning path — the transport
    /// then uses default TLS verification.
    #[test]
    fn parse_card_pinned_fingerprints_empty_for_non_pinned_variants() {
        use crate::peer::card::TransportAuth;
        assert!(parse_card_pinned_fingerprints(&TransportAuth::None)
            .unwrap()
            .is_empty());
        assert!(parse_card_pinned_fingerprints(&TransportAuth::MutualTls)
            .unwrap()
            .is_empty());
        assert!(parse_card_pinned_fingerprints(&TransportAuth::Unknown)
            .unwrap()
            .is_empty());
    }

    /// `parse_card_pinned_fingerprints` parses every fingerprint
    /// from a `PinnedMutualTls` transport-auth into pre-validated
    /// bytes. Caller (registry) doesn't have to handle parse errors
    /// at connect time.
    #[test]
    fn parse_card_pinned_fingerprints_parses_pinned_variant() {
        use crate::peer::card::TransportAuth;
        let auth = TransportAuth::PinnedMutualTls {
            server_cert_fingerprints: vec![
                "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
                "11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00".into(),
            ],
        };
        let fps = parse_card_pinned_fingerprints(&auth).unwrap();
        assert_eq!(fps.len(), 2);
        assert_eq!(fps[0][0], 0xaa);
        assert_eq!(fps[1][0], 0x11);
    }

    /// Bad fingerprint in the card surfaces as a parse error — the
    /// registry caller wraps this into `PeerError::Auth` so the
    /// add-peer path fails cleanly at registration rather than
    /// silently dropping the pinning requirement.
    #[test]
    fn parse_card_pinned_fingerprints_reports_bad_entry() {
        use crate::peer::card::TransportAuth;
        let auth = TransportAuth::PinnedMutualTls {
            server_cert_fingerprints: vec!["definitely-not-a-fingerprint".into()],
        };
        let err = parse_card_pinned_fingerprints(&auth).unwrap_err();
        assert!(err.contains("64 hex chars") || err.contains("non-hex"));
    }

    /// End-to-end: add_peer_with_card_and_auth on a card whose
    /// `auth.transport = PinnedMutualTls` with a malformed
    /// fingerprint fails with `PeerError::Auth` and includes the
    /// peer id for diagnostic context.
    #[tokio::test]
    async fn add_peer_with_pinned_card_rejects_malformed_fingerprint() {
        use crate::peer::card::{AuthRequirements, TransportAuth};
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);
        let mut card = fake_card("bad-pin", "ws://x/ws");
        card.auth = AuthRequirements {
            transport: TransportAuth::PinnedMutualTls {
                server_cert_fingerprints: vec!["zzz-not-hex".into()],
            },
            application: None,
        };
        let result = reg
            .add_peer_with_card_and_auth(card, Vec::new(), None, None)
            .await;
        match result {
            Err(PeerError::Auth(msg)) => {
                assert!(msg.contains("intendant:bad-pin"), "msg: {msg}");
                assert!(msg.contains("invalid pinned fingerprint"), "msg: {msg}");
            }
            other => panic!("expected PeerError::Auth, got {other:?}"),
        }
        assert!(
            reg.is_empty(),
            "failed registration should not store the peer"
        );
    }

    /// End-to-end: a well-formed pinned card registers cleanly.
    /// The transport's pinning kicks in at connect time (which uses
    /// http:// in tests, so pinning is silently inactive — the
    /// success here just proves the parse + register path doesn't
    /// reject a valid PinnedMutualTls card).
    #[tokio::test]
    async fn add_peer_with_pinned_card_accepts_valid_fingerprint() {
        use crate::peer::card::{AuthRequirements, TransportAuth};
        let (log_tx, _log_rx) = mpsc::channel::<TaggedPeerEvent>(64);
        let reg = PeerRegistry::new(log_tx);
        let mut card = fake_card("good-pin", "ws://x/ws");
        card.auth = AuthRequirements {
            transport: TransportAuth::PinnedMutualTls {
                server_cert_fingerprints: vec![
                    "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
                ],
            },
            application: None,
        };
        let peer_id = reg
            .add_peer_with_card_and_auth(card, Vec::new(), None, None)
            .await
            .expect("valid pinned card should register");
        assert_eq!(peer_id.label(), "good-pin");
        reg.remove_peer(&peer_id).await.unwrap();
    }

    /// `pick_supported_transports` skips variants this build doesn't
    /// support, including the `Unknown` forward-compat fallback and
    /// future transport kinds like A2A. Preference order from the
    /// card is preserved in the returned Vec — first-supported wins
    /// in `MultiTransport::connect`.
    #[test]
    fn pick_supported_transports_filters_unsupported() {
        let transports = vec![
            TransportSpec::Unknown,
            TransportSpec::A2A {
                url: "https://x".into(),
            },
            TransportSpec::IntendantWs {
                url: "ws://x/ws".into(),
            },
        ];
        let picked = pick_supported_transports(&transports);
        assert_eq!(picked.len(), 1);
        assert!(matches!(picked[0], TransportSpec::IntendantWs { .. }));
    }

    /// `pick_supported_transports` preserves card preference order.
    /// A card listing two `IntendantWs` URLs (e.g. LAN preferred,
    /// Tailscale fallback) yields a Vec in the same order, which
    /// `MultiTransport` then probes top-down.
    #[test]
    fn pick_supported_transports_preserves_preference_order() {
        let transports = vec![
            TransportSpec::IntendantWs {
                url: "ws://lan/ws".into(),
            },
            TransportSpec::A2A {
                url: "https://x".into(),
            },
            TransportSpec::IntendantWs {
                url: "ws://tail/ws".into(),
            },
        ];
        let picked = pick_supported_transports(&transports);
        assert_eq!(picked.len(), 2);
        match (&picked[0], &picked[1]) {
            (TransportSpec::IntendantWs { url: a }, TransportSpec::IntendantWs { url: b }) => {
                assert_eq!(a, "ws://lan/ws");
                assert_eq!(b, "ws://tail/ws");
            }
            _ => panic!("expected two IntendantWs variants in card order"),
        }
    }

    /// Returns an empty Vec when no supported variant is in the list.
    /// The caller (`add_peer_with_card`) treats empty as a hard error.
    #[test]
    fn pick_supported_transports_returns_empty_when_no_supported() {
        let transports = vec![
            TransportSpec::Unknown,
            TransportSpec::A2A {
                url: "https://x".into(),
            },
        ];
        assert!(pick_supported_transports(&transports).is_empty());
    }
}
