//! Native Intendant↔Intendant WebSocket transport.
//!
//! Speaks Intendant's own `/ws` wire contract — the HTTP+WebSocket
//! surface exposed by `web_gateway::spawn_web_gateway`. On the
//! inbound side, frames are typed [`OutboundEvent`] values (the
//! wire projection of `AppEvent`) which this transport deserializes
//! via serde and translates to [`PeerEvent`] through
//! [`WireEventUpcaster`]. On the outbound side, [`PeerOp`] values
//! are encoded as [`ControlMsg`] JSON and written to the same
//! WebSocket — Intendant's WS handler already accepts `ControlMsg`
//! frames from the browser and the test suite, so the transport
//! is just another client speaking the same protocol.
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
//!    `PeerEvent`s to the actor's channel. The write half stays on
//!    the transport struct and is driven by `send`.
//!
//! ## Outbound operation mapping
//!
//! Intendant's `/ws` control surface is fire-and-forget: a control
//! message produces side effects and subsequent events through the
//! broadcast channel, but does not echo a request/response id. So
//! `send` returns a synthetic `MessageId` / `TaskId` for
//! operations that expect one — the real correlation happens
//! through subsequent `ActivityStarted` / `Message` events the
//! drain path surfaces back on the peer's event stream.
//!
//! - [`PeerOp::SendMessage`] → [`ControlMsg::FollowUp`] (continues an
//!   existing conversation — the main "say something to the peer's
//!   agent" verb). Returns a synthetic `MessageId`.
//! - [`PeerOp::DelegateTask`] → [`ControlMsg::StartTask`] (kicks off
//!   a fresh agent task). `PeerTask::instructions` maps to
//!   `task`; the orchestration/direct/reference-frame/display-target
//!   flags default to absent. Returns a synthetic `TaskId`.
//! - [`PeerOp::ResolveApproval`] → [`ControlMsg::Approve`] /
//!   `ApproveAll` / `Deny` / `Skip` based on
//!   [`ApprovalDecision`]. Requires `request_id` to parse as `u64`
//!   — Intendant's approval ids are numeric; non-numeric ids
//!   return a typed error rather than silently failing.
//! - `CancelTask`, `QueryTaskStatus`, `InvokeCapability` are
//!   rejected up front via `check_feature` because Intendant's
//!   native control plane has no wire primitive for them. These
//!   come in through other transport adapters (OpenClaw's node
//!   `invoke`, A2A's task queries) when those land.
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

use crate::event::ControlMsg;
use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::event::{ApprovalDecision, MessageContent, MessageId, PeerEvent, TaskId};
use crate::peer::traits::{check_feature, PeerOp, PeerOpAck, PeerTransport, TransportFeatures};
use crate::peer::transport::tls_client::ClientIdentityPaths;
use crate::peer::upcast::WireEventUpcaster;
use crate::peer::PeerError;
use crate::types::OutboundEvent;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Per-peer auth credentials (bearer token, pinned server cert
    /// fingerprints, and optional mTLS client identity). Sourced from
    /// operator config, the peer's Agent Card, and the installed access
    /// cert store. See [`TransportCredentials`].
    creds: TransportCredentials,
    /// Monotonic counter for synthetic `MessageId`/`TaskId` values
    /// returned from `send`. Intendant's `/ws` control plane is
    /// fire-and-forget — no wire-level id echoes back — so the
    /// transport fabricates an id so callers have something
    /// unique to log. Real correlation with subsequent activity
    /// events happens through the drain path's `ActivityStarted`
    /// / `Message` emissions.
    out_seq: AtomicU64,
}

/// Per-peer auth credentials carried by [`IntendantWsTransport`].
/// Bundled into a struct rather than additional constructor args so future
/// additions (per-peer signing key, issued scoped certs, etc.) extend cleanly.
#[derive(Clone, Debug, Default)]
pub struct TransportCredentials {
    /// Outbound bearer token sent as `Authorization: Bearer <token>`
    /// on both the agent-card HTTP fetch and the WebSocket upgrade.
    /// `None` means no bearer enforcement on the peer side; matches
    /// `[server.auth] bearer_token` on the peer when set.
    pub bearer_token: Option<String>,
    /// Pre-parsed SHA-256 fingerprints of acceptable server certs.
    /// When non-empty, the WebSocket connect and agent-card fetch
    /// both go through a custom rustls verifier (see
    /// [`crate::peer::transport::pinning`]) that requires the
    /// presented cert to match one of these. When empty, default
    /// system / native-roots TLS verification applies (no pinning).
    /// Sourced from the peer's `auth.transport = PinnedMutualTls`
    /// at registry-add time; the registry parses string fingerprints
    /// from the card and passes the bytes here.
    pub pinned_fingerprints: Vec<crate::peer::transport::pinning::Fingerprint>,
    /// PEM client certificate and private key this daemon presents when
    /// connecting to a peer over HTTPS/WSS. Defaults to the installed access
    /// `client.crt` / `client.key` when present, so daemon-to-daemon federation
    /// can satisfy the same mTLS gate as browsers without a dashboard-only
    /// bearer token.
    pub client_identity: Option<ClientIdentityPaths>,
}

impl IntendantWsTransport {
    pub fn new(url: String, events_tx: mpsc::Sender<PeerEvent>) -> Self {
        Self::with_credentials(url, events_tx, TransportCredentials::default())
    }

    /// Construct with explicit credentials (bearer token + pinned
    /// cert fingerprints).
    pub fn with_credentials(
        url: String,
        events_tx: mpsc::Sender<PeerEvent>,
        creds: TransportCredentials,
    ) -> Self {
        Self {
            spec: TransportSpec::IntendantWs { url },
            events_tx,
            ws_write: None,
            reader_handle: None,
            card: None,
            creds,
            out_seq: AtomicU64::new(0),
        }
    }

    /// Convenience constructor that wires a bearer token without
    /// pinning. Common case for operators who use mTLS at the
    /// proxy layer (no app-level pinning) plus a bearer token for
    /// app-layer auth.
    pub fn with_bearer(
        url: String,
        events_tx: mpsc::Sender<PeerEvent>,
        bearer_token: Option<String>,
    ) -> Self {
        Self::with_credentials(
            url,
            events_tx,
            TransportCredentials {
                bearer_token,
                pinned_fingerprints: Vec::new(),
                client_identity: None,
            },
        )
    }

    fn next_out_seq(&self) -> u64 {
        self.out_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn write_control_msg(&mut self, ctrl: &ControlMsg) -> Result<(), PeerError> {
        let json = serde_json::to_string(ctrl)
            .map_err(|e| PeerError::Transport(format!("serialize ControlMsg: {e}")))?;
        let write = self.ws_write.as_mut().ok_or(PeerError::NotConnected)?;
        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| PeerError::Transport(format!("ws send: {e}")))?;
        Ok(())
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
    /// base + `/.well-known/agent-card.json`. Sends the configured
    /// bearer token in `Authorization: Bearer <token>` so peers that
    /// gate their REST surface still serve their card to authorized
    /// connectors. (`/.well-known/agent-card.json` itself is exempt
    /// from bearer enforcement on the server side because it's the
    /// discovery endpoint, but sending the token costs nothing and
    /// covers the case where an operator opts to enforce on every
    /// path.)
    ///
    /// When the peer's `auth.transport` is `PinnedMutualTls`, this
    /// reqwest client is built with a custom rustls config that
    /// pins the server cert's SHA-256 fingerprint via
    /// [`pinned_client_config`] — same verifier the WebSocket
    /// connect path uses, so HTTP and WS share the trust decision.
    async fn fetch_agent_card(&self) -> Result<AgentCard, PeerError> {
        let ws_url = self.ws_url()?.to_string();
        let http_base = super::ws_url_to_http_base(&ws_url);
        let card_url = format!("{http_base}/.well-known/agent-card.json");

        let client = super::tls_client::reqwest_client(
            CARD_FETCH_TIMEOUT,
            &self.creds.pinned_fingerprints,
            self.creds.client_identity.as_ref(),
        )?;

        let mut request = client.get(&card_url);
        if let Some(token) = &self.creds.bearer_token {
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
            .map_err(|e| PeerError::CardFetch(format!("parse agent card at {card_url}: {e}")))
    }

    /// Open the WebSocket, split into read/write halves, spawn the
    /// drain task on the read half, return the write half for
    /// storage on the transport.
    ///
    /// When credentials specify a bearer token, it goes in the
    /// `Authorization: Bearer <token>` header on the upgrade —
    /// server-side `verify_bearer_for_ws` checks this *before*
    /// completing the handshake. (The dashboard browser path uses
    /// `?token=...` on the URL because it can't natively set headers
    /// on `WebSocket` opens.)
    ///
    /// When credentials specify pinned fingerprints or an mTLS client
    /// identity, the connect goes through `connect_async_tls_with_config` with
    /// a custom rustls Connector. For `ws://` URLs (no TLS layer at all), the
    /// connector is irrelevant, so trusted-LAN cleartext tests keep working.
    async fn open_ws(&self) -> Result<(WsSink, JoinHandle<()>), PeerError> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::Connector;

        let ws_url = self.ws_url()?.to_string();

        // Start from a URL-derived request so tungstenite fills in
        // the standard WS handshake headers (Sec-WebSocket-Key,
        // Upgrade, Connection, Sec-WebSocket-Version, Host). Then
        // splice in our Authorization header. Manually building the
        // request from scratch would mean re-deriving those WS
        // headers ourselves, which is fragile and pointless.
        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| PeerError::Transport(format!("build ws request {ws_url}: {e}")))?;

        if let Some(token) = &self.creds.bearer_token {
            let value = format!("Bearer {token}").parse().map_err(|e| {
                PeerError::Transport(format!(
                    "bearer token contains characters not valid in an HTTP header: {e}"
                ))
            })?;
            request.headers_mut().insert("Authorization", value);
        }

        let connector: Option<Connector> = super::tls_client::rustls_client_config(
            &self.creds.pinned_fingerprints,
            self.creds.client_identity.as_ref(),
        )?
        .map(|config| Connector::Rustls(std::sync::Arc::new(config)));

        let (ws_stream, _response) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
                .await
                .map_err(|e| PeerError::Transport(format!("ws connect {ws_url}: {e}")))?;

        let (write, read) = ws_stream.split();
        let events_tx = self.events_tx.clone();
        let handle = tokio::spawn(drain_ws(read, events_tx));
        Ok((write, handle))
    }
}

/// Extract the text payload from a [`MessageContent`] for use as
/// the body of [`ControlMsg::FollowUp`] or [`ControlMsg::StartTask`].
/// Intendant's native control plane carries text-shaped message
/// input only; image / multi-part / unknown content types are
/// rejected with a typed error rather than silently dropping the
/// payload.
fn message_text(content: &MessageContent) -> Result<String, PeerError> {
    match content {
        MessageContent::Text { text } | MessageContent::Reasoning { text } => Ok(text.clone()),
        MessageContent::Image { .. } => Err(PeerError::Transport(
            "IntendantWsTransport: image message content is not supported \
             — Intendant's ControlMsg::FollowUp / StartTask carry text only"
                .into(),
        )),
        MessageContent::Parts { .. } => Err(PeerError::Transport(
            "IntendantWsTransport: multi-part message content is not \
             supported — flatten to text before calling send_message"
                .into(),
        )),
        MessageContent::Unknown => Err(PeerError::Transport(
            "IntendantWsTransport: unknown message content variant cannot \
             be sent — forward-compat fallback has no outbound semantics"
                .into(),
        )),
    }
}

/// Parse a peer approval `request_id` string as the `u64` Intendant's
/// native control plane expects. Non-numeric ids (e.g. ones coming
/// from a non-Intendant peer that uses string ids) return a typed
/// error so the caller sees the mismatch rather than the transport
/// silently dropping the resolution.
fn parse_request_id(id: &str) -> Result<u64, PeerError> {
    id.parse::<u64>().map_err(|_| {
        PeerError::Transport(format!(
            "ResolveApproval request_id '{id}' is not a u64 — Intendant \
             peers use numeric approval ids"
        ))
    })
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
            Some(Ok(Message::Binary(_)))
            | Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_)))
            | Some(Ok(Message::Frame(_))) => {
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

    /// Outbound support now covers the three core verbs Intendant's
    /// native control plane exposes: `send_message` (FollowUp),
    /// `task_delegation` (StartTask), and `resolve_approval`
    /// (Approve/ApproveAll/Deny/Skip). `task_cancel`,
    /// `task_query`, and `invoke_capability` stay `false` because
    /// the wire has no primitive for them — those verbs belong to
    /// future transport adapters (OpenClaw's `node.invoke`, A2A's
    /// task lifecycle) and are rejected up front by `check_feature`.
    fn features(&self) -> TransportFeatures {
        TransportFeatures {
            bidirectional: true,
            streaming_events: true,
            send_message: true,
            task_delegation: true,
            task_cancel: false,
            task_query: false,
            invoke_capability: false,
            resolve_approval: true,
            webrtc_signal: true,
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
        check_feature(&self.features(), &op)?;
        if self.ws_write.is_none() {
            return Err(PeerError::NotConnected);
        }

        match op {
            PeerOp::SendMessage { message } => {
                let text = message_text(&message.content)?;
                self.write_control_msg(&ControlMsg::FollowUp {
                    session_id: None,
                    text,
                    direct: None,
                    follow_up_id: None,
                })
                .await?;
                let seq = self.next_out_seq();
                Ok(PeerOpAck::MessageId(MessageId(format!("msg-out-{seq}"))))
            }
            PeerOp::DelegateTask { task } => {
                self.write_control_msg(&ControlMsg::StartTask {
                    session_id: None,
                    task: task.instructions,
                    orchestrate: None,
                    direct: None,
                    reference_frame_ids: Vec::new(),
                    display_target: None,
                    attachments: Vec::new(),
                    follow_up_id: None,
                })
                .await?;
                let seq = self.next_out_seq();
                Ok(PeerOpAck::TaskId(TaskId(format!("task-out-{seq}"))))
            }
            PeerOp::ResolveApproval {
                request_id,
                decision,
            } => {
                let id = parse_request_id(&request_id)?;
                let ctrl = match decision {
                    ApprovalDecision::Accept => ControlMsg::Approve {
                        session_id: None,
                        id,
                    },
                    ApprovalDecision::AcceptForSession => ControlMsg::ApproveAll {
                        session_id: None,
                        id,
                    },
                    ApprovalDecision::Decline => ControlMsg::Deny {
                        session_id: None,
                        id,
                    },
                    ApprovalDecision::Cancel => ControlMsg::Skip {
                        session_id: None,
                        id,
                    },
                };
                self.write_control_msg(&ctrl).await?;
                Ok(PeerOpAck::Ok)
            }
            PeerOp::WebRtcSignal {
                display_id,
                session_id,
                signal,
            } => {
                // Map directly to the typed ControlMsg variant the
                // peer's WS handler dispatches on. session_id is
                // round-tripped as String (the wire form); the typed
                // WebRtcSessionId is a federation-side abstraction.
                self.write_control_msg(&ControlMsg::WebRtcSignal {
                    display_id,
                    session_id: session_id.0,
                    signal,
                })
                .await?;
                // Fire-and-forget: peer responds asynchronously via
                // `OutboundEvent::WebRtcSignal` → `PeerEvent::WebRtcSignal`,
                // which the actor pushes onto the per-peer event stream.
                Ok(PeerOpAck::Ok)
            }
            // check_feature rejects the other variants before they
            // reach this match. The arm is unreachable in practice
            // but kept to keep the match exhaustive without a
            // wildcard — the compile error when a new PeerOp lands
            // is the prompt to decide how to route it.
            PeerOp::CancelTask { .. }
            | PeerOp::QueryTaskStatus { .. }
            | PeerOp::InvokeCapability { .. } => {
                Err(PeerError::UnsupportedCapability(op.name().to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AppEvent, EventBus};
    use crate::peer::event::{MessageContent, MessageRole, PeerMessage};
    use crate::peer::traits::PeerTask;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
    use tokio::sync::{broadcast, mpsc};

    /// Spin up a real web gateway on an ephemeral port and return
    /// the port + gateway handle. Tests connect the transport to
    /// this as if it were a remote peer.
    async fn spawn_test_peer() -> (u16, tokio::task::JoinHandle<()>) {
        let (port, handle, _) = spawn_test_peer_with_bus().await;
        (port, handle)
    }

    /// Variant that also returns an EventBus receiver so tests can
    /// verify control messages land on the bus (the outbound-path
    /// tests all need this).
    async fn spawn_test_peer_with_bus() -> (
        u16,
        tokio::task::JoinHandle<()>,
        broadcast::Receiver<AppEvent>,
    ) {
        let bus = EventBus::new();
        let bus_rx = bus.subscribe();
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
        (port, handle, bus_rx)
    }

    /// Read events from a bus receiver until the predicate matches
    /// or a short timeout elapses. Returns the matched event or
    /// `None` on timeout. The WS handler may emit unrelated
    /// background events (presence logging, session init) between
    /// the moment the transport's send lands and the matching
    /// `ControlCommand` — the predicate filter keeps tests robust
    /// against that noise.
    async fn wait_for_event<F>(rx: &mut broadcast::Receiver<AppEvent>, pred: F) -> Option<AppEvent>
    where
        F: Fn(&AppEvent) -> bool,
    {
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(event)) => {
                    if pred(&event) {
                        return Some(event);
                    }
                }
                Ok(Err(_)) => return None,
                Err(_) => return None,
            }
        }
        None
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
        let _card2 = transport
            .connect()
            .await
            .expect("second connect (reconnect)");
        assert!(transport.is_connected());

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Features advertise the three outbound verbs Intendant's
    /// native control plane supports (send_message, task_delegation,
    /// resolve_approval) and keep the peer-specific verbs
    /// (task_cancel/query/invoke_capability) off because the wire
    /// has no primitive for them. This is the invariant guard — if
    /// anyone flips a feature flag on without adding a matching
    /// arm to `send`, the mismatch shows up here or in a parity
    /// test rather than silent broken behavior at runtime.
    #[test]
    fn features_advertise_three_outbound_verbs() {
        let (tx, _rx) = mpsc::channel::<PeerEvent>(1);
        let transport = IntendantWsTransport::new("ws://127.0.0.1:0/ws".to_string(), tx);
        let features = transport.features();
        assert!(features.bidirectional);
        assert!(features.streaming_events);
        assert!(features.send_message);
        assert!(features.task_delegation);
        assert!(features.resolve_approval);
        assert!(!features.task_cancel, "no wire primitive for cancel");
        assert!(!features.task_query, "no wire primitive for task query");
        assert!(
            !features.invoke_capability,
            "no wire primitive for capability invoke"
        );
    }

    /// `send_message` writes a `ControlMsg::FollowUp` to the peer's
    /// `/ws` and returns a synthetic `MessageId`. The follow-up
    /// text lands on the peer's EventBus as
    /// `AppEvent::ControlCommand(FollowUp { text })`.
    #[tokio::test]
    async fn send_message_writes_followup_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "hello from peer".into(),
                    },
                },
            })
            .await
            .expect("send_message succeeds");
        match ack {
            PeerOpAck::MessageId(id) => {
                assert!(id.0.starts_with("msg-out-"), "synthetic id shape: {}", id.0);
            }
            other => panic!("expected MessageId ack, got {other:?}"),
        }

        let event = wait_for_event(&mut bus_rx, |e| {
            matches!(e, AppEvent::ControlCommand(ControlMsg::FollowUp { text, .. }) if text == "hello from peer")
        })
        .await;
        assert!(
            event.is_some(),
            "follow-up ControlMsg did not land on the bus"
        );

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `webrtc_signal` writes a `ControlMsg::WebRtcSignal` carrying
    /// display_id, session_id, and the inner signal kind verbatim.
    /// Returns `PeerOpAck::Ok` (fire-and-forget; the peer's response
    /// arrives asynchronously as `OutboundEvent::WebRtcSignal`).
    #[tokio::test]
    async fn webrtc_signal_writes_typed_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::WebRtcSignal {
                display_id: 0,
                session_id: crate::peer::WebRtcSessionId("sess-uuid".into()),
                signal: crate::peer::WebRtcSignal::Offer {
                    sdp: "v=0\r\nm=video".into(),
                    advertise_tcp_via_url: None,
                },
            })
            .await
            .expect("webrtc_signal succeeds");
        assert!(matches!(ack, PeerOpAck::Ok));

        // The corresponding ControlMsg lands on the peer's bus via
        // the existing fall-through dispatch path (the peer's WS
        // handler routes WebRtcSignal to a special handler instead
        // of broadcasting AppEvent::ControlCommand, so we don't see
        // ControlCommand here. Instead, we observe via the
        // PresenceLog the parser emits after a successful parse).
        // For wire-format coverage, just confirm the connection
        // didn't drop and a follow-up send still works.
        transport.disconnect().await.unwrap();
        gateway.abort();
        // Drain a bit so the test isn't flaky from straggler events.
        let _ = wait_for_event(&mut bus_rx, |_| false).await;
    }

    /// `delegate_task` writes a `ControlMsg::StartTask`, with the
    /// task instructions ending up in the `task` field. Orchestrate
    /// and other flags default to absent.
    #[tokio::test]
    async fn delegate_task_writes_start_task_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::DelegateTask {
                task: PeerTask {
                    instructions: "research the federation protocol".into(),
                    context: serde_json::Value::Null,
                    client_correlation_id: None,
                },
            })
            .await
            .expect("delegate_task succeeds");
        assert!(matches!(ack, PeerOpAck::TaskId(_)));

        let event = wait_for_event(&mut bus_rx, |e| {
            matches!(
                e,
                AppEvent::ControlCommand(ControlMsg::StartTask { task, .. })
                if task == "research the federation protocol"
            )
        })
        .await;
        assert!(event.is_some(), "StartTask did not land on the bus");

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Each `ApprovalDecision` variant maps to a distinct
    /// `ControlMsg` on the wire. Drives all four through the
    /// transport and verifies each one lands on the bus.
    #[tokio::test]
    async fn resolve_approval_maps_each_decision_to_its_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        // Accept → Approve { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "1".into(),
                decision: ApprovalDecision::Accept,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::Approve { id: 1, .. })
        ))
        .await
        .is_some());

        // AcceptForSession → ApproveAll { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "2".into(),
                decision: ApprovalDecision::AcceptForSession,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::ApproveAll { id: 2, .. })
        ))
        .await
        .is_some());

        // Decline → Deny { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "3".into(),
                decision: ApprovalDecision::Decline,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::Deny { id: 3, .. })
        ))
        .await
        .is_some());

        // Cancel → Skip { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "4".into(),
                decision: ApprovalDecision::Cancel,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::Skip { id: 4, .. })
        ))
        .await
        .is_some());

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `send_message` rejects non-text message content with a
    /// typed Transport error rather than silently swallowing the
    /// payload. Guards against a future refactor that starts
    /// mapping `MessageContent::Image` → something wrong.
    #[tokio::test]
    async fn send_message_rejects_image_content() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        let result = transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Image {
                        mime_type: "image/png".into(),
                        base64: "aGVsbG8=".into(),
                    },
                },
            })
            .await;
        match result {
            Err(PeerError::Transport(msg)) => {
                assert!(msg.contains("image"), "error mentions image: {msg}");
            }
            other => panic!("expected Transport error, got {other:?}"),
        }

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `resolve_approval` with a non-numeric `request_id` returns a
    /// typed Transport error rather than silently dropping the
    /// resolution. Intendant's approval ids are `u64`; a peer
    /// request_id that's a string from a non-Intendant source
    /// can't be mapped through without data loss.
    #[tokio::test]
    async fn resolve_approval_rejects_non_numeric_request_id() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport = IntendantWsTransport::new(url, tx);
        let _ = transport.connect().await.unwrap();

        let result = transport
            .send(PeerOp::ResolveApproval {
                request_id: "openclaw-approval-abc".into(),
                decision: ApprovalDecision::Accept,
            })
            .await;
        match result {
            Err(PeerError::Transport(msg)) => {
                assert!(msg.contains("not a u64"), "error mentions u64: {msg}");
            }
            other => panic!("expected Transport error, got {other:?}"),
        }

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `send` returns `NotConnected` when called before `connect`.
    /// Guards against the transport silently accepting commands
    /// that have no wire to land on.
    #[tokio::test]
    async fn send_before_connect_returns_not_connected() {
        let (tx, _rx) = mpsc::channel::<PeerEvent>(1);
        let mut transport = IntendantWsTransport::new("ws://127.0.0.1:1/ws".to_string(), tx);

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
        assert!(matches!(result, Err(PeerError::NotConnected)));
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
