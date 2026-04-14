//! Native Intendant↔Intendant WebSocket transport.
//!
//! Speaks Intendant's own `/ws` wire contract — the HTTP+WebSocket
//! surface exposed by `web_gateway::spawn_web_gateway`. On the
//! inbound side, frames are typed [`OutboundEvent`] values (the wire
//! projection of `AppEvent`) which this transport deserializes via
//! serde and translates to [`PeerEvent`] through
//! [`WireEventUpcaster`]. On the outbound side, phase 1 is **read-only**:
//! the transport advertises [`TransportFeatures`] with every
//! outbound op disabled, and `send` rejects with
//! `PeerError::UnsupportedCapability` for any operation. This is
//! not a TODO — it's an honest declaration of current capability.
//! Full send support (ControlMsg encoding for approvals, input,
//! and settings) lands in a follow-up commit once the drain path
//! has settled into production.
//!
//! ## Connection lifecycle
//!
//! `connect` is a two-step handshake:
//!
//! 1. **Agent Card discovery** — HTTP GET
//!    `/.well-known/agent-card.json` on the derived HTTP base URL.
//!    The returned card is cached on the transport and returned to
//!    the caller; the peer actor uses it as the canonical identity
//!    and refreshes the handle's watch snapshot.
//! 2. **WebSocket attach** — `tokio_tungstenite::connect_async` to
//!    the peer's `/ws` endpoint. The read half moves into a spawned
//!    drain task that deserializes frames and pushes upcast
//!    `PeerEvent`s to the actor's channel. The write half is
//!    retained on the transport struct for future send support.
//!
//! ## Disconnection signaling
//!
//! When the WebSocket read half closes (peer went away, network
//! error, peer restart), the drain task emits a synthetic
//! `PeerEvent::Disconnected` as its last event before exiting. The
//! per-peer actor matches on this variant as its signal to exit
//! the main loop and reconnect — the alternative (relying on the
//! `events_tx` channel close) would require the transport to drop
//! its own clone of the sender, which would make `disconnect` and
//! reconnect semantics much trickier.

use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::event::PeerEvent;
use crate::peer::traits::{
    check_feature, PeerOp, PeerOpAck, PeerTransport, TransportFeatures,
};
use crate::peer::upcast::WireEventUpcaster;
use crate::peer::PeerError;
use crate::types::OutboundEvent;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;

const CARD_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

pub struct IntendantWsTransport {
    spec: TransportSpec,
    events_tx: mpsc::Sender<PeerEvent>,
    ws_write: Option<WsSink>,
    reader_handle: Option<JoinHandle<()>>,
    card: Option<AgentCard>,
}

impl IntendantWsTransport {
    pub fn new(url: String, events_tx: mpsc::Sender<PeerEvent>) -> Self {
        Self {
            spec: TransportSpec::IntendantWs { url },
            events_tx,
            ws_write: None,
            reader_handle: None,
            card: None,
        }
    }

    fn ws_url(&self) -> Result<&str, PeerError> {
        match &self.spec {
            TransportSpec::IntendantWs { url } => Ok(url.as_str()),
            _ => Err(PeerError::Transport(
                "IntendantWsTransport constructed with non-IntendantWs spec".into(),
            )),
        }
    }

    /// Fetch the peer's Agent Card via HTTP GET on the derived HTTP
    /// base + `/.well-known/agent-card.json`.
    async fn fetch_agent_card(&self) -> Result<AgentCard, PeerError> {
        let ws_url = self.ws_url()?.to_string();
        let http_base = super::ws_url_to_http_base(&ws_url);
        let card_url = format!("{http_base}/.well-known/agent-card.json");

        let client = reqwest::Client::builder()
            .timeout(CARD_FETCH_TIMEOUT)
            .build()
            .map_err(|e| PeerError::CardFetch(format!("build http client: {e}")))?;

        let response = client
            .get(&card_url)
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
            .map_err(|e| PeerError::CardFetch(format!("parse agent card at {card_url}: {e}")))
    }

    /// Open the WebSocket, split into read/write halves, spawn the
    /// drain task on the read half, return the write half for
    /// storage on the transport.
    async fn open_ws(&self) -> Result<(WsSink, JoinHandle<()>), PeerError> {
        let ws_url = self.ws_url()?.to_string();
        let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| PeerError::Transport(format!("ws connect {ws_url}: {e}")))?;

        let (write, read) = ws_stream.split();
        let events_tx = self.events_tx.clone();
        let handle = tokio::spawn(drain_ws(read, events_tx));
        Ok((write, handle))
    }
}

/// Drain the WebSocket read half: parse each text frame as
/// [`OutboundEvent`] (forward-compat via the `Unknown` fallback),
/// upcast to [`PeerEvent`] through [`WireEventUpcaster`], and push
/// onto the actor's event channel. On connection close or error,
/// emit a synthetic `PeerEvent::Disconnected` so the actor can
/// trigger reconnect.
async fn drain_ws(
    mut read: futures_util::stream::SplitStream<WsStream>,
    events_tx: mpsc::Sender<PeerEvent>,
) {
    let mut upcaster = WireEventUpcaster::new();

    let disconnect_reason = loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                // Forward-compat via OutboundEvent::Unknown: unknown
                // event variants deserialize silently and the upcaster
                // drops them. Non-JSON frames (unlikely on this
                // endpoint) are also dropped silently — the drain
                // loop stays liberal in what it accepts.
                let Ok(outbound) = serde_json::from_str::<OutboundEvent>(&text) else {
                    continue;
                };
                for event in upcaster.upcast(&outbound) {
                    // If the actor's channel is full we back-pressure
                    // the reader by awaiting; if the channel is
                    // closed (actor is gone), exit cleanly.
                    if events_tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            Some(Ok(Message::Close(frame))) => {
                break frame
                    .map(|f| format!("peer closed: {} {}", f.code, f.reason))
                    .unwrap_or_else(|| "peer closed without reason".to_string());
            }
            Some(Ok(Message::Binary(_))) | Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {
                // Intendant's /ws doesn't speak binary; ping/pong is
                // handled by tungstenite under the hood.
                continue;
            }
            Some(Err(e)) => {
                break format!("ws read error: {e}");
            }
            None => {
                break "ws stream ended".to_string();
            }
        }
    };

    let _ = events_tx
        .send(PeerEvent::Disconnected {
            reason: disconnect_reason,
        })
        .await;
}

#[async_trait]
impl PeerTransport for IntendantWsTransport {
    fn spec(&self) -> &TransportSpec {
        &self.spec
    }

    /// Phase 1 is read-only. The drain path is fully implemented;
    /// outbound PeerOp support (send_message, resolve_approval,
    /// etc.) lands in a follow-up once the drain path has settled.
    /// Every outbound feature flag is `false` here so the
    /// [`crate::peer::handle::PeerHandle`] layer rejects sends up
    /// front with a clear [`PeerError::UnsupportedCapability`]
    /// rather than accepting commands that would silently fail.
    fn features(&self) -> TransportFeatures {
        TransportFeatures {
            bidirectional: true,
            streaming_events: true,
            send_message: false,
            task_delegation: false,
            task_cancel: false,
            task_query: false,
            invoke_capability: false,
            resolve_approval: false,
        }
    }

    async fn connect(&mut self) -> Result<AgentCard, PeerError> {
        // If a previous reader task is still running, tear it down
        // before reconnecting. This keeps `connect` idempotent: the
        // actor can call it on every retry attempt without leaking
        // tasks.
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(mut write) = self.ws_write.take() {
            let _ = write.close().await;
        }

        let card = self.fetch_agent_card().await?;
        let (write, reader_handle) = self.open_ws().await?;

        self.card = Some(card.clone());
        self.ws_write = Some(write);
        self.reader_handle = Some(reader_handle);

        Ok(card)
    }

    async fn disconnect(&mut self) -> Result<(), PeerError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(mut write) = self.ws_write.take() {
            let _ = write.close().await;
        }
        self.card = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.ws_write.is_some()
    }

    async fn send(&mut self, op: PeerOp) -> Result<PeerOpAck, PeerError> {
        // Phase 1: read-only transport. check_feature rejects every
        // op because features() advertises no outbound support.
        check_feature(&self.features(), &op)?;
        // Unreachable at runtime — check_feature returns Err on all
        // ops until the feature flags flip on. Kept for future
        // expansion: when send support lands, this arm becomes a
        // match on PeerOp with ControlMsg encoding.
        Err(PeerError::UnsupportedCapability(op.name().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
    use tokio::sync::{broadcast, mpsc};

    /// Spin up a real web gateway on an ephemeral port and return
    /// the port + gateway handle. Tests connect the transport to
    /// this as if it were a remote peer.
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
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        (port, handle)
    }

    /// Connect the transport to a test peer, fetch its card, verify
    /// the card identifies as Intendant, and assert the WebSocket
    /// is attached.
    #[tokio::test]
    async fn connect_fetches_card_and_attaches_ws() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);

        let card = transport.connect().await.expect("connect succeeds");
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::PeerKind::Intendant),
            "test peer should identify as Intendant"
        );
        assert!(transport.is_connected());

        transport.disconnect().await.unwrap();
        assert!(!transport.is_connected());
        gateway.abort();
    }

    /// `connect` is idempotent — calling it twice tears down the
    /// previous reader task before establishing a new one so the
    /// actor's reconnect loop doesn't leak resources.
    #[tokio::test]
    async fn connect_is_idempotent_for_reconnect() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);

        let _card1 = transport.connect().await.expect("first connect");
        assert!(transport.is_connected());
        let _card2 = transport.connect().await.expect("second connect (reconnect)");
        assert!(transport.is_connected());

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Phase 1 transport is read-only. All send ops return
    /// `UnsupportedCapability` so the handle layer can fail fast.
    #[tokio::test]
    async fn send_is_unsupported_in_phase_one() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        use crate::peer::event::{MessageContent, MessageRole, PeerMessage};
        let result = transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "hello".into(),
                    },
                },
            })
            .await;
        match result {
            Err(PeerError::UnsupportedCapability(op)) => {
                assert_eq!(op, "send_message");
            }
            other => panic!("expected UnsupportedCapability, got {other:?}"),
        }

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Features advertise the phase 1 read-only posture: streaming
    /// events yes, outbound ops no. This test is the invariant
    /// guard — if anyone flips a feature flag on without
    /// implementing the corresponding send arm, the mismatch shows
    /// up as a test discrepancy rather than silent broken behavior.
    #[test]
    fn phase_one_features_are_read_only() {
        let (tx, _rx) = mpsc::channel::<PeerEvent>(1);
        let transport =
            IntendantWsTransport::new("ws://127.0.0.1:0/ws".to_string(), tx);
        let features = transport.features();
        assert!(features.bidirectional);
        assert!(features.streaming_events);
        assert!(!features.send_message);
        assert!(!features.task_delegation);
        assert!(!features.task_cancel);
        assert!(!features.task_query);
        assert!(!features.invoke_capability);
        assert!(!features.resolve_approval);
    }

    /// Forward-compat: a peer sending an event variant we don't
    /// recognize parses via `OutboundEvent::Unknown`, the upcaster
    /// drops it, and the drain task keeps running rather than
    /// closing the connection.
    #[tokio::test]
    async fn drain_task_skips_unknown_wire_events() {
        // Build a minimal drain driver: a pair of mpsc channels
        // that mimic the WS frame stream. Since exercising the
        // real tokio_tungstenite read half requires a full WS
        // peer, we exercise the unknown-frame drop path through
        // the WireEventUpcaster directly — the drain_ws function
        // is a thin wrapper that delegates to the upcaster for
        // all parsing decisions.
        let mut upcaster = WireEventUpcaster::new();

        let json = r#"{"event":"holographic_projection_started","intensity":"high"}"#;
        let outbound: OutboundEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(outbound, OutboundEvent::Unknown));

        let events = upcaster.upcast(&outbound);
        assert!(
            events.is_empty(),
            "unknown wire event should produce no PeerEvents: {events:?}"
        );
    }
}
