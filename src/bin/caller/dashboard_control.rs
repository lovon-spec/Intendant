//! Daemon-scoped WebRTC control tunnel for dashboard RPC experiments.
//!
//! The dashboard still uses HTTP plus the main WebSocket by default. This
//! module provides the first substrate for a future public-origin dashboard:
//! WebSocket signaling creates a direct browser-to-daemon WebRTC data channel,
//! then the channel carries small JSON RPC frames.

use crate::daemon_identity::{b64u, DaemonIdentity};
use crate::error::CallerError;
use base64::Engine as _;
use bytes::BytesMut;
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate, RTCIceCandidateInit,
    RTCIceServer,
};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

const CONTROL_CHANNEL_LABEL: &str = "intendant-dashboard-control";
const CONTROL_PROTOCOL_VERSION: u32 = 1;
const CONTROL_SIGNATURE_CONTEXT: &str = "intendant-dashboard-control-v1";
const CONTROL_DEFAULT_SESSION_LIMIT: usize = 600;
const CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES: usize = 64 * 1024;
const CONTROL_RESPONSE_CHUNK_BYTES: usize = 16 * 1024;
const CONTROL_FEATURES: &[&str] = &[
    "ping",
    "config",
    "status",
    "events",
    "response_chunks",
    "api_peers",
    "api_sessions",
    "api_session_detail",
    "api_sessions_search",
    "api_settings",
    "api_settings_save",
    "api_key_status",
    "api_api_keys_save",
    "api_project_root",
    "api_displays",
    "api_managed_context_records",
    "api_managed_context_anchors",
    "api_managed_context_fission",
    "api_peer_add",
    "api_peer_remove",
    "api_peer_eligible",
    "api_peer_message",
    "api_peer_task",
    "api_peer_approval",
    "api_peer_pairing_invite",
    "api_peer_pairing_join",
    "api_peer_pairing_request_access",
    "api_peer_pairing_request_access_poll",
    "api_peer_pairing_requests",
    "api_peer_pairing_request_decision",
    "api_peer_pairing_identities",
    "api_peer_pairing_identity_revoke",
    "api_coordinator_route",
];
const UDP_BUF_LEN: usize = 2000;
const COMMAND_CHANNEL: usize = 16;

pub struct DashboardControlRegistry {
    config: crate::web_gateway::WebGatewayConfig,
    broadcast_tx: tokio::sync::broadcast::Sender<String>,
    bus: crate::event::EventBus,
    peer_registry: Option<crate::peer::PeerRegistry>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
    identity: Mutex<Option<Arc<DaemonIdentity>>>,
    peers: Mutex<HashMap<String, DashboardControlPeer>>,
}

impl DashboardControlRegistry {
    pub fn new(
        config: crate::web_gateway::WebGatewayConfig,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        bus: crate::event::EventBus,
        peer_registry: Option<crate::peer::PeerRegistry>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            broadcast_tx,
            bus,
            peer_registry,
            shared_session,
            project_root,
            identity: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
        }
    }

    pub async fn answer_offer(&self, offer_sdp: String) -> Result<DashboardControlAnswer, String> {
        let identity = self.identity().await?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let (peer, answer_sdp, binding) = DashboardControlPeer::answer_offer(
            session_id.clone(),
            offer_sdp,
            &self.config,
            self.broadcast_tx.clone(),
            self.bus.clone(),
            self.peer_registry.clone(),
            self.shared_session.clone(),
            self.project_root.clone(),
            identity,
        )
        .await
        .map_err(|e| e.to_string())?;
        self.peers.lock().await.insert(session_id.clone(), peer);
        Ok(DashboardControlAnswer {
            session_id,
            sdp: answer_sdp,
            binding,
        })
    }

    pub async fn add_ice_candidate(
        &self,
        session_id: &str,
        candidate_json: &serde_json::Value,
    ) -> Result<bool, String> {
        let peers = self.peers.lock().await;
        let Some(peer) = peers.get(session_id) else {
            return Ok(false);
        };
        peer.add_ice_candidate(candidate_json).await?;
        Ok(true)
    }

    pub async fn close(&self, session_id: &str) {
        if let Some(peer) = self.peers.lock().await.remove(session_id) {
            peer.close().await;
        }
    }

    async fn identity(&self) -> Result<Arc<DaemonIdentity>, String> {
        let mut guard = self.identity.lock().await;
        if let Some(identity) = guard.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(DaemonIdentity::load_or_create_default()?);
        *guard = Some(Arc::clone(&identity));
        Ok(identity)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardControlAnswer {
    pub session_id: String,
    pub sdp: String,
    pub binding: DashboardControlBinding,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardControlBinding {
    pub protocol: &'static str,
    pub session_id: String,
    pub daemon_public_key: String,
    pub created_unix_ms: i64,
    pub offer_sha256: String,
    pub answer_sha256: String,
    pub signature: String,
}

impl DashboardControlBinding {
    pub fn new(
        identity: &DaemonIdentity,
        session_id: String,
        offer_sdp: &str,
        answer_sdp: &str,
    ) -> Self {
        let daemon_public_key = identity.public_key_b64u();
        let created_unix_ms = chrono::Utc::now().timestamp_millis();
        let offer_sha256 = sha256_b64u(offer_sdp.as_bytes());
        let answer_sha256 = sha256_b64u(answer_sdp.as_bytes());
        let mut binding = Self {
            protocol: CONTROL_SIGNATURE_CONTEXT,
            session_id,
            daemon_public_key,
            created_unix_ms,
            offer_sha256,
            answer_sha256,
            signature: String::new(),
        };
        let payload = binding.signing_payload();
        binding.signature = identity.sign_b64u(payload.as_bytes());
        binding
    }

    pub fn signing_payload(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.protocol,
            self.session_id,
            self.daemon_public_key,
            self.created_unix_ms,
            self.offer_sha256,
            self.answer_sha256,
        )
    }
}

pub struct DashboardControlPeer {
    command_tx: mpsc::Sender<ControlCommand>,
    shutdown: CancellationToken,
}

impl DashboardControlPeer {
    async fn answer_offer(
        session_id: String,
        offer_sdp: String,
        config: &crate::web_gateway::WebGatewayConfig,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        bus: crate::event::EventBus,
        peer_registry: Option<crate::peer::PeerRegistry>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
        identity: Arc<DaemonIdentity>,
    ) -> Result<(Self, String, DashboardControlBinding), CallerError> {
        let mut setting_engine = SettingEngine::default();
        setting_engine
            .set_answering_dtls_role(RTCDtlsRole::Server)
            .map_err(|e| CallerError::WebRtc(format!("set answering DTLS role: {e}")))?;

        let rtc_config = RTCConfigurationBuilder::new()
            .with_ice_servers(to_rtc_ice_servers(&config.ice_servers))
            .build();
        let mut rtc = RTCPeerConnectionBuilder::new()
            .with_configuration(rtc_config)
            .with_setting_engine(setting_engine)
            .build()
            .map_err(|e| CallerError::WebRtc(format!("build control rtc peer: {e}")))?;

        let mut sockets = Vec::new();
        for ip in crate::access::routable_local_addrs(true) {
            let socket = match UdpSocket::bind(SocketAddr::new(ip, 0)).await {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("[dashboard/control] skipping UDP bind on {ip}: {e}");
                    continue;
                }
            };
            let local = match socket.local_addr() {
                Ok(local) => local,
                Err(e) => {
                    eprintln!("[dashboard/control] skipping UDP socket on {ip}: {e}");
                    continue;
                }
            };
            let candidate = udp_host_candidate_init(local)?;
            match rtc.add_local_candidate(candidate) {
                Ok(()) => sockets.push(Arc::new(socket)),
                Err(e) => eprintln!("[dashboard/control] skipping UDP host candidate {local}: {e}"),
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound for dashboard control".into(),
            ));
        }

        let offer = RTCSessionDescription::offer(offer_sdp.clone())
            .map_err(|e| CallerError::WebRtc(format!("parse control offer: {e}")))?;
        rtc.set_remote_description(offer)
            .map_err(|e| CallerError::WebRtc(format!("set control remote offer: {e}")))?;
        let answer = rtc
            .create_answer(None)
            .map_err(|e| CallerError::WebRtc(format!("create control answer: {e}")))?;
        rtc.set_local_description(answer.clone())
            .map_err(|e| CallerError::WebRtc(format!("set control local answer: {e}")))?;

        let answer_sdp = answer.sdp;
        let binding =
            DashboardControlBinding::new(&identity, session_id.clone(), &offer_sdp, &answer_sdp);
        let runtime = ControlRuntime {
            session_id,
            daemon_public_key: identity.public_key_b64u(),
            created_unix_ms: binding.created_unix_ms,
            events_subscribed: false,
            events_sent: 0,
            config: serde_json::to_value(config).unwrap_or_else(|_| serde_json::json!({})),
            bus,
            peer_registry,
            shared_session,
            project_root,
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL);
        let shutdown = CancellationToken::new();
        tokio::spawn(control_driver(
            rtc,
            sockets,
            runtime,
            broadcast_tx.subscribe(),
            command_rx,
            shutdown.clone(),
        ));
        Ok((
            Self {
                command_tx,
                shutdown,
            },
            answer_sdp,
            binding,
        ))
    }

    async fn add_ice_candidate(&self, candidate_json: &serde_json::Value) -> Result<(), String> {
        let candidate_str = candidate_json
            .get("candidate")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if candidate_str.is_empty() {
            return Ok(());
        }
        let resolved = match crate::display::webrtc::resolve_mdns_in_candidate(candidate_str).await
        {
            Ok(candidate) => candidate,
            Err(e) => {
                eprintln!("[dashboard/control] mDNS resolve failed: {e}, dropping candidate");
                return Ok(());
            }
        };
        self.command_tx
            .send(ControlCommand::AddIceCandidate(resolved))
            .await
            .map_err(|_| "dashboard control driver gone".to_string())
    }

    async fn close(self) {
        self.shutdown.cancel();
    }
}

#[derive(Clone)]
struct ControlRuntime {
    session_id: String,
    daemon_public_key: String,
    created_unix_ms: i64,
    events_subscribed: bool,
    events_sent: u64,
    config: serde_json::Value,
    bus: crate::event::EventBus,
    peer_registry: Option<crate::peer::PeerRegistry>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
}

enum ControlCommand {
    AddIceCandidate(String),
}

#[derive(Debug)]
struct InboundPacket {
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

struct ControlTaskResponse {
    id: String,
    frame: serde_json::Value,
}

async fn control_driver<I: rtc::interceptor::Interceptor + Send + Sync + 'static>(
    mut rtc: RTCPeerConnection<I>,
    sockets: Vec<Arc<UdpSocket>>,
    mut runtime: ControlRuntime,
    mut event_rx: tokio::sync::broadcast::Receiver<String>,
    mut command_rx: mpsc::Receiver<ControlCommand>,
    shutdown: CancellationToken,
) {
    let mut sockets_by_addr: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundPacket>(64);
    let mut forwarder_handles = Vec::new();
    for sock in sockets {
        let local = match sock.local_addr() {
            Ok(local) => local,
            Err(_) => continue,
        };
        sockets_by_addr.insert(local, Arc::clone(&sock));
        let tx = inbound_tx.clone();
        let shutdown = shutdown.clone();
        forwarder_handles.push(tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUF_LEN];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    recv = sock.recv_from(&mut buf) => match recv {
                        Ok((n, source)) => {
                            let pkt = InboundPacket {
                                source,
                                destination: local,
                                bytes: buf[..n].to_vec(),
                                received_at: Instant::now(),
                            };
                            if tx.send(pkt).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("[dashboard/control] UDP recv failed on {local}: {e}");
                            break;
                        }
                    }
                }
            }
        }));
    }
    drop(inbound_tx);

    let mut channels: HashMap<String, rtc::data_channel::RTCDataChannelId> = HashMap::new();
    let (task_tx, mut task_rx) = mpsc::channel::<ControlTaskResponse>(64);
    let mut pending_requests: HashMap<String, CancellationToken> = HashMap::new();

    loop {
        let timeout_at = match drain_control_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut channels,
            &mut runtime,
            &task_tx,
            &mut pending_requests,
        )
        .await
        {
            Ok(timeout_at) => timeout_at,
            Err(()) => {
                shutdown.cancel();
                break;
            }
        };
        let timeout_dur = timeout_at
            .saturating_duration_since(Instant::now())
            .max(Duration::from_micros(1));

        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(pkt) = inbound_rx.recv() => {
                let input = TaggedBytesMut {
                    now: pkt.received_at,
                    transport: TransportContext {
                        local_addr: pkt.destination,
                        peer_addr: pkt.source,
                        transport_protocol: TransportProtocol::UDP,
                        ecn: None,
                    },
                    message: BytesMut::from(pkt.bytes.as_slice()),
                };
                if let Err(e) = rtc.handle_read(input) {
                    eprintln!("[dashboard/control] handle_read failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    ControlCommand::AddIceCandidate(candidate) => {
                        let init = RTCIceCandidateInit {
                            candidate,
                            sdp_mid: None,
                            sdp_mline_index: None,
                            username_fragment: None,
                            url: None,
                        };
                        if let Err(e) = rtc.add_remote_candidate(init) {
                            eprintln!("[dashboard/control] parse remote candidate failed: {e}");
                        }
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            Some(task_response) = task_rx.recv() => {
                if pending_requests.remove(&task_response.id).is_some() {
                    send_control_frame(&mut rtc, &channels, task_response.frame);
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            event = event_rx.recv(), if runtime.events_subscribed => {
                match event {
                    Ok(line) => {
                        runtime.events_sent = runtime.events_sent.saturating_add(1);
                        let payload = serde_json::from_str::<serde_json::Value>(&line)
                            .unwrap_or_else(|_| serde_json::json!({"raw": line}));
                        let frame = serde_json::json!({
                            "t": "event",
                            "seq": runtime.events_sent,
                            "payload": payload,
                        });
                        send_control_text(&mut rtc, &channels, frame.to_string());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        let frame = serde_json::json!({
                            "t": "event_gap",
                            "skipped": skipped,
                        });
                        send_control_text(&mut rtc, &channels, frame.to_string());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        runtime.events_subscribed = false;
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            _ = tokio::time::sleep(timeout_dur) => {
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!("[dashboard/control] handle_timeout failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
        }
    }

    for (_, token) in pending_requests {
        token.cancel();
    }
    for handle in forwarder_handles {
        let _ = handle.await;
    }
}

async fn drain_control_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    channels: &mut HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
) -> Result<Instant, ()> {
    while let Some(t) = rtc.poll_write() {
        if t.transport.transport_protocol != TransportProtocol::UDP {
            continue;
        }
        if t.transport.local_addr.is_ipv4() != t.transport.peer_addr.is_ipv4() {
            continue;
        }
        if t.transport.local_addr.ip().is_loopback() != t.transport.peer_addr.ip().is_loopback() {
            continue;
        }
        let Some(sock) = sockets_by_addr.get(&t.transport.local_addr) else {
            eprintln!(
                "[dashboard/control] UDP transmit from unknown source {}, dropping",
                t.transport.local_addr
            );
            continue;
        };
        if let Err(e) = sock.send_to(&t.message, t.transport.peer_addr).await {
            eprintln!(
                "[dashboard/control] udp send {} -> {} failed: {e}",
                t.transport.local_addr, t.transport.peer_addr
            );
        }
    }

    while let Some(message) = rtc.poll_read() {
        let RTCMessage::DataChannelMessage(cid, msg) = message else {
            continue;
        };
        let label = channels
            .iter()
            .find_map(|(label, id)| (*id == cid).then(|| label.clone()));
        if label.as_deref() != Some(CONTROL_CHANNEL_LABEL) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&msg.data) else {
            continue;
        };
        if let Some(response) = control_frame_response(text, runtime, task_tx, pending_requests) {
            send_control_frame(rtc, channels, response);
        }
    }

    while let Some(event) = rtc.poll_event() {
        match event {
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
                let label = rtc
                    .data_channel(cid)
                    .map(|channel| channel.label().to_string())
                    .unwrap_or_else(|| format!("channel-{cid}"));
                eprintln!("[dashboard/control] data channel open: {label}");
                channels.insert(label, cid);
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
                channels.retain(|_, id| *id != cid);
            }
            RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                eprintln!("[dashboard/control] connection: {state:?}");
                if matches!(
                    state,
                    rtc::peer_connection::state::RTCPeerConnectionState::Failed
                        | rtc::peer_connection::state::RTCPeerConnectionState::Closed
                ) {
                    return Err(());
                }
            }
            RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                eprintln!("[dashboard/control] ICE: {state:?}");
            }
            _ => {}
        }
    }

    Ok(rtc
        .poll_timeout()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400)))
}

fn send_control_text<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    text: String,
) {
    let Some(cid) = channels.get(CONTROL_CHANNEL_LABEL).copied() else {
        return;
    };
    if let Some(mut channel) = rtc.data_channel(cid) {
        if let Err(e) = channel.send_text(text) {
            eprintln!("[dashboard/control] data channel write failed: {e:?}");
        }
    }
}

fn send_control_frame<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    frame: serde_json::Value,
) {
    for text in control_frame_texts(frame) {
        send_control_text(rtc, channels, text);
    }
}

fn control_frame_texts(frame: serde_json::Value) -> Vec<String> {
    chunk_control_response_frame(
        frame,
        CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES,
        CONTROL_RESPONSE_CHUNK_BYTES,
    )
}

fn chunk_control_response_frame(
    frame: serde_json::Value,
    threshold_bytes: usize,
    chunk_bytes: usize,
) -> Vec<String> {
    let text = frame.to_string();
    if frame.get("t").and_then(|v| v.as_str()) != Some("response")
        || text.len() <= threshold_bytes
        || chunk_bytes == 0
    {
        return vec![text];
    }
    let id = frame
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return vec![text];
    }

    let bytes = text.as_bytes();
    let chunk_count = bytes.len().div_ceil(chunk_bytes);
    let mut frames = Vec::with_capacity(chunk_count + 2);
    frames.push(
        serde_json::json!({
            "t": "response_start",
            "id": id,
            "encoding": "base64-json-frame",
            "total_bytes": bytes.len(),
            "chunks": chunk_count,
        })
        .to_string(),
    );
    for (seq, chunk) in bytes.chunks(chunk_bytes).enumerate() {
        frames.push(
            serde_json::json!({
                "t": "response_chunk",
                "id": id,
                "seq": seq,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .to_string(),
        );
    }
    frames.push(
        serde_json::json!({
            "t": "response_end",
            "id": id,
            "chunks": chunk_count,
        })
        .to_string(),
    );
    frames
}

fn control_frame_response(
    text: &str,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let t = parsed.get("t").and_then(|v| v.as_str()).unwrap_or("");
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match t {
        "hello" => Some(serde_json::json!({
            "t": "hello_ack",
            "id": id,
            "protocol": CONTROL_PROTOCOL_VERSION,
            "session_id": runtime.session_id,
            "daemon_public_key": runtime.daemon_public_key,
            "features": CONTROL_FEATURES,
        })),
        "ping" => Some(serde_json::json!({
            "t": "pong",
            "id": id,
            "unix_ms": chrono::Utc::now().timestamp_millis(),
        })),
        "request" => {
            let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
            match method {
                "status" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": {
                        "protocol": CONTROL_PROTOCOL_VERSION,
                        "session_id": runtime.session_id,
                        "daemon_public_key": runtime.daemon_public_key,
                        "created_unix_ms": runtime.created_unix_ms,
                        "features": CONTROL_FEATURES,
                        "transport": "webrtc-datachannel",
                        "events_subscribed": runtime.events_subscribed,
                        "events_sent": runtime.events_sent,
                        "api_peers_available": runtime.peer_registry.is_some(),
                        "api_sessions_available": true,
                        "api_session_detail_available": true,
                        "api_sessions_search_available": true,
                        "api_settings_available": true,
                        "api_settings_save_available": runtime.project_root.is_some(),
                        "api_key_status_available": true,
                        "api_api_keys_save_available": true,
                        "api_project_root_available": true,
                        "api_displays_available": true,
                        "api_managed_context_available": true,
                        "api_peer_mutations_available": runtime.peer_registry.is_some(),
                        "api_peer_pairing_available": true,
                        "api_coordinator_available": runtime.peer_registry.is_some(),
                    },
                })),
                "api_peers" => match runtime.peer_registry.as_ref() {
                    Some(registry) => {
                        let result = serde_json::from_str::<serde_json::Value>(
                            &crate::web_gateway::peers_list_response_body(registry),
                        )
                        .unwrap_or_else(|_| serde_json::json!({"peers":[]}));
                        Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        }))
                    }
                    None => Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": false,
                        "error": "peer registry unavailable",
                    })),
                },
                "subscribe_events" => {
                    runtime.events_subscribed = true;
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": {
                            "subscribed": true,
                        },
                    }))
                }
                "unsubscribe_events" => {
                    runtime.events_subscribed = false;
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": {
                            "subscribed": false,
                        },
                    }))
                }
                "config" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": runtime.config,
                })),
                "api_sessions"
                | "api_session_detail"
                | "api_sessions_search"
                | "api_settings"
                | "api_settings_save"
                | "api_key_status"
                | "api_api_keys_save"
                | "api_project_root"
                | "api_displays"
                | "api_managed_context_records"
                | "api_managed_context_anchors"
                | "api_managed_context_fission"
                | "api_peer_add"
                | "api_peer_remove"
                | "api_peer_eligible"
                | "api_peer_message"
                | "api_peer_task"
                | "api_peer_approval"
                | "api_peer_pairing_invite"
                | "api_peer_pairing_join"
                | "api_peer_pairing_request_access"
                | "api_peer_pairing_request_access_poll"
                | "api_peer_pairing_requests"
                | "api_peer_pairing_request_decision"
                | "api_peer_pairing_identities"
                | "api_peer_pairing_identity_revoke"
                | "api_coordinator_route" => {
                    spawn_control_request(
                        id,
                        method.to_string(),
                        parsed.get("params").cloned(),
                        runtime.clone(),
                        task_tx.clone(),
                        pending_requests,
                    );
                    None
                }
                _ => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("unknown method: {method}"),
                })),
            }
        }
        "cancel" => {
            let existed = pending_requests
                .remove(&id)
                .map(|token| {
                    token.cancel();
                    true
                })
                .unwrap_or(false);
            Some(cancelled_control_response(id, existed))
        }
        _ => Some(serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("unknown frame type: {t}"),
        })),
    }
}

fn spawn_control_request(
    id: String,
    method: String,
    params: Option<serde_json::Value>,
    runtime: ControlRuntime,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
) {
    if let Some(previous) = pending_requests.remove(&id) {
        previous.cancel();
    }
    let cancel = CancellationToken::new();
    pending_requests.insert(id.clone(), cancel.clone());
    tokio::spawn(async move {
        let frame = control_request_response(id.clone(), method, params, runtime, cancel).await;
        let _ = task_tx.send(ControlTaskResponse { id, frame }).await;
    });
}

async fn control_request_response(
    id: String,
    method: String,
    params: Option<serde_json::Value>,
    runtime: ControlRuntime,
    cancel: CancellationToken,
) -> serde_json::Value {
    if cancel.is_cancelled() {
        return cancelled_control_response(id, true);
    }
    match method.as_str() {
        "api_sessions" => api_sessions_response(id, params.as_ref()).await,
        "api_session_detail" => api_session_detail_response(id, params.as_ref()).await,
        "api_sessions_search" => api_sessions_search_response(id, params.as_ref(), cancel).await,
        "api_settings" => api_settings_response(id, &runtime).await,
        "api_settings_save" => api_settings_save_response(id, params.as_ref(), &runtime).await,
        "api_key_status" => json_body_response(
            id,
            crate::web_gateway::api_key_status_response_body(),
            "api key status",
        ),
        "api_api_keys_save" => api_api_keys_save_response(id, params.as_ref()).await,
        "api_project_root" => json_body_response(
            id,
            crate::web_gateway::project_root_response_body(runtime.project_root.as_deref()),
            "project root",
        ),
        "api_displays" => api_displays_response(id, &runtime).await,
        "api_managed_context_records" => {
            api_managed_context_response(id, "records", params.as_ref(), &runtime).await
        }
        "api_managed_context_anchors" => {
            api_managed_context_response(id, "anchors", params.as_ref(), &runtime).await
        }
        "api_managed_context_fission" => {
            api_managed_context_response(id, "fission", params.as_ref(), &runtime).await
        }
        "api_peer_add" => api_peer_add_response(id, params.as_ref(), &runtime).await,
        "api_peer_remove" => api_peer_remove_response(id, params.as_ref(), &runtime).await,
        "api_peer_eligible" => api_peer_eligible_response(id, params.as_ref(), &runtime).await,
        "api_peer_message" => api_peer_message_response(id, params.as_ref(), &runtime).await,
        "api_peer_task" => api_peer_task_response(id, params.as_ref(), &runtime).await,
        "api_peer_approval" => api_peer_approval_response(id, params.as_ref(), &runtime).await,
        "api_peer_pairing_invite" => api_peer_pairing_invite_response(id, params.as_ref()).await,
        "api_peer_pairing_join" => {
            api_peer_pairing_join_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_pairing_request_access" => {
            api_peer_pairing_request_access_response(id, params.as_ref()).await
        }
        "api_peer_pairing_request_access_poll" => {
            api_peer_pairing_request_access_poll_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_pairing_requests" => api_peer_pairing_requests_response(id).await,
        "api_peer_pairing_request_decision" => {
            api_peer_pairing_request_decision_response(id, params.as_ref()).await
        }
        "api_peer_pairing_identities" => api_peer_pairing_identities_response(id).await,
        "api_peer_pairing_identity_revoke" => {
            api_peer_pairing_identity_revoke_response(id, params.as_ref()).await
        }
        "api_coordinator_route" => {
            api_coordinator_route_response(id, params.as_ref(), &runtime).await
        }
        _ => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("unknown method: {method}"),
        }),
    }
}

fn cancelled_control_response(id: String, existed: bool) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "cancelled": true,
        "error": if existed {
            "request cancelled"
        } else {
            "request not found or already completed"
        },
    })
}

async fn api_session_detail_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "missing session_id",
        });
    }
    let source = string_param(&params, &["source"]).trim().to_string();
    let source = if source.is_empty() {
        "intendant".to_string()
    } else {
        source
    };
    let limit = control_session_detail_limit(&params);
    let body = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_detail_response_body(&session_id, &source, limit)
    })
    .await;
    let body = match body {
        Ok(body) => body,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("session detail task failed: {e}"),
            });
        }
    };
    json_body_response(id, body, "session detail")
}

async fn api_sessions_search_response(
    id: String,
    params: Option<&serde_json::Value>,
    cancel: CancellationToken,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let query = string_param(&params, &["q", "query"]);
    let source_filter = string_param(&params, &["source", "source_filter", "sourceFilter"]);
    let source_filter = if source_filter.is_empty() {
        "all".to_string()
    } else {
        source_filter
    };
    let mode = string_param(&params, &["mode"]);
    let project_filter = control_project_filter(&params);
    let body = crate::web_gateway::sessions_search_response_body_with_cancel(
        query,
        source_filter,
        mode,
        project_filter,
        cancel,
    )
    .await;
    json_body_response(id, body, "session search")
}

async fn api_settings_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let runtime_settings = {
        let session = runtime.shared_session.read().await;
        session.runtime_settings.clone()
    };
    json_body_response(
        id,
        crate::web_gateway::settings_get_response_body(
            runtime.project_root.as_deref(),
            &runtime_settings,
        )
        .await,
        "settings",
    )
}

async fn api_displays_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    json_body_response(
        id,
        crate::web_gateway::displays_response_body(&session_registry).await,
        "displays",
    )
}

async fn api_managed_context_response(
    id: String,
    kind: &'static str,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let Some(request_line) = managed_context_request_line(kind, &params) else {
        return missing_param_response(id, "query");
    };
    let active_log_dir = match active_session_log_dir(runtime).await {
        Ok(dir) => dir,
        Err(error) => {
            return http_body_response(
                id,
                500,
                serde_json::json!({ "error": error }).to_string(),
                "managed context",
            );
        }
    };
    let home = crate::platform::home_dir();
    let response = tokio::task::spawn_blocking(move || match kind {
        "records" => crate::web_gateway::managed_context_records_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
        "anchors" => crate::web_gateway::managed_context_anchors_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
        "fission" => crate::web_gateway::managed_context_fission_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
        _ => crate::web_gateway::managed_context_records_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
    })
    .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("managed context task failed: {e}"),
            });
        }
    };
    http_wire_response(id, response, "managed context")
}

async fn api_settings_save_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status_line, body) = crate::web_gateway::settings_post_result(
        &body_text,
        runtime.project_root.as_deref(),
        &runtime.bus,
    );
    http_body_response(id, status_line_code(status_line), body, "settings save")
}

async fn api_api_keys_save_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    http_body_response(
        id,
        200,
        crate::web_gateway::handle_set_api_keys(&body_text),
        "api keys save",
    )
}

async fn api_peer_add_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) =
        crate::web_gateway::peers_add(registry, runtime.project_root.as_deref(), &body_text).await;
    http_body_response(id, status, body, "peer add")
}

async fn api_peer_remove_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_remove(registry, &body_text).await;
    http_body_response(id, status, body, "peer remove")
}

async fn api_peer_eligible_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let query = control_capability_query(&params);
    let (status, body) = crate::web_gateway::peers_eligible(registry, &query);
    http_body_response(id, status, body, "eligible peers")
}

async fn api_peer_message_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_send_message(registry, &peer_id, &body_text).await;
    http_body_response(id, status, body, "peer message")
}

async fn api_peer_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_delegate_task(registry, &peer_id, &body_text).await;
    http_body_response(id, status, body, "peer task")
}

async fn api_peer_approval_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_resolve_approval(registry, &peer_id, &body_text).await;
    http_body_response(id, status, body, "peer approval")
}

async fn api_peer_pairing_invite_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_invite(&body_text);
    http_body_response(id, status, body, "peer pairing invite")
}

async fn api_peer_pairing_join_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_join(
        registry,
        runtime.project_root.as_deref(),
        &body_text,
    )
    .await;
    http_body_response(id, status, body, "peer pairing join")
}

async fn api_peer_pairing_request_access_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_request_access(&body_text).await;
    http_body_response(id, status, body, "peer access request")
}

async fn api_peer_pairing_request_access_poll_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_request_access_poll(
        runtime.peer_registry.as_ref(),
        runtime.project_root.as_deref(),
        &body_text,
    )
    .await;
    http_body_response(id, status, body, "peer access request poll")
}

async fn api_peer_pairing_requests_response(id: String) -> serde_json::Value {
    let (status, body) = crate::web_gateway::peers_pairing_requests_list();
    http_body_response(id, status, body, "peer access requests")
}

async fn api_peer_pairing_request_decision_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let request_id = string_param(&params, &["request_id", "requestId", "code", "id"]);
    if request_id.is_empty() {
        return missing_param_response(id, "request_id");
    }
    let op = string_param(&params, &["op", "decision", "action"]);
    let op = if op.is_empty() {
        "approve".to_string()
    } else {
        op
    };
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_pairing_request_decision(&request_id, &op, &body_text);
    http_body_response(id, status, body, "peer access request decision")
}

async fn api_peer_pairing_identities_response(id: String) -> serde_json::Value {
    let (status, body) = crate::web_gateway::peers_pairing_identities_list();
    http_body_response(id, status, body, "peer identities")
}

async fn api_peer_pairing_identity_revoke_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_identity_revoke(&body_text);
    http_body_response(id, status, body, "peer identity revoke")
}

async fn api_coordinator_route_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::coordinator_route(registry, &body_text).await;
    http_body_response(id, status, body, "coordinator route")
}

fn json_body_response(id: String, body: String, label: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(result) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": result,
        }),
        Err(_) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("{label} returned invalid JSON"),
        }),
    }
}

fn http_body_response(id: String, status: u16, body: String, label: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut result) => {
            if let serde_json::Value::Object(map) = &mut result {
                map.insert("_httpStatus".to_string(), serde_json::json!(status));
                map.insert(
                    "_httpOk".to_string(),
                    serde_json::json!((200..300).contains(&status)),
                );
            }
            serde_json::json!({
                "t": "response",
                "id": id,
                "ok": true,
                "result": result,
            })
        }
        Err(_) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("{label} returned invalid JSON"),
        }),
    }
}

fn http_wire_response(id: String, response: String, label: &str) -> serde_json::Value {
    let (status, body) = split_http_response(&response);
    http_body_response(id, status, body.to_string(), label)
}

fn split_http_response(response: &str) -> (u16, &str) {
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", response));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1.1 "))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(200);
    (status, body)
}

fn status_line_code(status_line: &str) -> u16 {
    status_line
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(500)
}

fn params_body_text(params: Option<&serde_json::Value>) -> String {
    serde_json::to_string(&params.cloned().unwrap_or_else(|| serde_json::json!({})))
        .unwrap_or_else(|_| "{}".to_string())
}

fn missing_param_response(id: String, name: &str) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "error": format!("missing {name}"),
    })
}

fn peer_registry_unavailable_response(id: String) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "error": "peer registry unavailable",
    })
}

async fn api_sessions_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let limit = control_session_limit(&params);
    let ids = control_session_ids(&params);
    let body = tokio::task::spawn_blocking(move || {
        crate::web_gateway::sessions_list_response_body(limit, &ids)
    })
    .await;
    let body = match body {
        Ok(body) => body,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("session list task failed: {e}"),
            });
        }
    };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(result) if result.is_array() => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": result,
        }),
        _ => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "session list returned invalid JSON",
        }),
    }
}

fn control_session_limit(params: &serde_json::Value) -> Option<usize> {
    match params.get("limit") {
        Some(serde_json::Value::String(value)) => {
            let value = value.trim();
            if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("full") {
                None
            } else {
                Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|limit| *limit > 0)
                        .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT),
                )
            }
        }
        Some(serde_json::Value::Number(value)) => Some(
            value
                .as_u64()
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| *limit > 0)
                .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT),
        ),
        _ => Some(CONTROL_DEFAULT_SESSION_LIMIT),
    }
}

fn control_session_ids(params: &serde_json::Value) -> Vec<String> {
    match params.get("ids") {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str())
            .flat_map(split_control_session_ids)
            .collect(),
        Some(serde_json::Value::String(value)) => split_control_session_ids(value).collect(),
        Some(value) => split_control_session_ids(&value.to_string()).collect(),
        None => Vec::new(),
    }
}

fn control_session_detail_limit(params: &serde_json::Value) -> Option<usize> {
    match params.get("limit") {
        Some(serde_json::Value::String(value)) => {
            let value = value.trim();
            if value.is_empty()
                || value.eq_ignore_ascii_case("all")
                || value.eq_ignore_ascii_case("full")
            {
                None
            } else {
                value.parse::<usize>().ok().filter(|limit| *limit > 0)
            }
        }
        Some(serde_json::Value::Number(value)) => value
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0),
        _ => None,
    }
}

fn control_project_filter(params: &serde_json::Value) -> Vec<String> {
    for name in ["projects", "project_filter", "projectFilter"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        match value {
            serde_json::Value::Array(values) => {
                return values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            serde_json::Value::String(value) => {
                if let Ok(values) = serde_json::from_str::<Vec<String>>(value) {
                    return values
                        .into_iter()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .collect();
                }
                return value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            value if !value.is_null() => return vec![value.to_string()],
            _ => {}
        }
    }
    Vec::new()
}

fn control_capability_query(params: &serde_json::Value) -> String {
    let capabilities = match params.get("capabilities") {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        Some(serde_json::Value::String(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    capabilities
        .iter()
        .map(|cap| format!("capability={cap}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn managed_context_request_line(kind: &str, params: &serde_json::Value) -> Option<String> {
    let raw_query = string_param(params, &["query", "search"]);
    let query = if raw_query.trim().is_empty() {
        managed_context_query_from_params(params)
    } else {
        raw_query.trim().trim_start_matches('?').to_string()
    };
    if query.is_empty() {
        return None;
    }
    Some(format!("GET /api/managed-context/{kind}?{query} HTTP/1.1"))
}

fn managed_context_query_from_params(params: &serde_json::Value) -> String {
    let mut pairs = Vec::new();
    for name in [
        "session_id",
        "session",
        "backend_session_id",
        "intendant_session_id",
        "wrapper_session_id",
    ] {
        let value = string_param(params, &[name]);
        if !value.is_empty() {
            pairs.push(format!("{name}={}", percent_encode_query_value(&value)));
        }
    }
    pairs.join("&")
}

fn percent_encode_query_value(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

async fn active_session_log_dir(runtime: &ControlRuntime) -> Result<Option<PathBuf>, String> {
    let session_log = {
        let session = runtime.shared_session.read().await;
        session.session_log.clone()
    };
    let Some(session_log) = session_log else {
        return Ok(None);
    };
    session_log
        .lock()
        .map(|log| Some(log.dir().to_path_buf()))
        .map_err(|_| "session log lock poisoned".to_string())
}

fn string_param(params: &serde_json::Value, names: &[&str]) -> String {
    for name in names {
        if let Some(value) = params.get(*name) {
            if let Some(text) = value.as_str() {
                return text.trim().to_string();
            }
            if !value.is_null() {
                return value.to_string();
            }
        }
    }
    String::new()
}

fn split_control_session_ids(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn to_rtc_ice_servers(servers: &[crate::display::IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|server| RTCIceServer {
            urls: server.urls.clone(),
            username: server.username.clone().unwrap_or_default(),
            credential: server.credential.clone().unwrap_or_default(),
        })
        .collect()
}

fn udp_host_candidate_init(addr: SocketAddr) -> Result<RTCIceCandidateInit, CallerError> {
    let candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: addr.ip().to_string(),
            port: addr.port(),
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .map_err(|e| CallerError::WebRtc(format!("build UDP host candidate: {e}")))?;
    RTCIceCandidate::from(&candidate)
        .to_json()
        .map_err(|e| CallerError::WebRtc(format!("serialize UDP host candidate: {e}")))
}

fn sha256_b64u(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    b64u(&digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> ControlRuntime {
        ControlRuntime {
            session_id: "session-1".into(),
            daemon_public_key: "pubkey".into(),
            created_unix_ms: 123,
            events_subscribed: false,
            events_sent: 0,
            config: serde_json::json!({"provider":"openai"}),
            bus: crate::event::EventBus::new(),
            peer_registry: None,
            shared_session: crate::web_gateway::ActiveSessionState::empty(),
            project_root: None,
        }
    }

    #[test]
    fn binding_signature_payload_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let identity = DaemonIdentity::load_or_create(dir.path().join("identity.pk8")).unwrap();
        let binding =
            DashboardControlBinding::new(&identity, "session-1".into(), "offer", "answer");
        assert!(crate::daemon_identity::verify_b64u(
            &binding.daemon_public_key,
            binding.signing_payload().as_bytes(),
            &binding.signature,
        ));
        assert_eq!(binding.protocol, CONTROL_SIGNATURE_CONTEXT);
        assert_eq!(binding.offer_sha256, sha256_b64u(b"offer"));
        assert_eq!(binding.answer_sha256, sha256_b64u(b"answer"));
    }

    #[tokio::test]
    async fn control_frames_answer_hello_ping_and_config() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let hello =
            control_frame_response(r#"{"t":"hello","id":"h1"}"#, &mut rt, &tx, &mut pending)
                .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert_eq!(hello["session_id"], "session-1");

        let ping = control_frame_response(r#"{"t":"ping","id":"p1"}"#, &mut rt, &tx, &mut pending)
            .unwrap();
        assert_eq!(ping["t"], "pong");
        assert_eq!(ping["id"], "p1");

        let config = control_frame_response(
            r#"{"t":"request","id":"r1","method":"config"}"#,
            &mut rt,
            &tx,
            &mut pending,
        )
        .unwrap();
        assert_eq!(config["t"], "response");
        assert_eq!(config["ok"], true);
        assert_eq!(config["result"]["provider"], "openai");

        let status = control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut rt,
            &tx,
            &mut pending,
        )
        .unwrap();
        assert_eq!(status["t"], "response");
        assert_eq!(status["ok"], true);
        assert_eq!(status["result"]["session_id"], "session-1");
        assert_eq!(status["result"]["created_unix_ms"], 123);
        assert_eq!(status["result"]["transport"], "webrtc-datachannel");
        assert_eq!(status["result"]["events_subscribed"], false);
        assert_eq!(status["result"]["api_peers_available"], false);
        assert_eq!(status["result"]["api_sessions_available"], true);
        assert_eq!(status["result"]["api_session_detail_available"], true);
        assert_eq!(status["result"]["api_sessions_search_available"], true);
        assert_eq!(status["result"]["api_settings_available"], true);
        assert_eq!(status["result"]["api_settings_save_available"], false);
        assert_eq!(status["result"]["api_key_status_available"], true);
        assert_eq!(status["result"]["api_api_keys_save_available"], true);
        assert_eq!(status["result"]["api_project_root_available"], true);
        assert_eq!(status["result"]["api_displays_available"], true);
        assert_eq!(status["result"]["api_peer_mutations_available"], false);
        assert_eq!(status["result"]["api_peer_pairing_available"], true);
        assert_eq!(status["result"]["api_coordinator_available"], false);

        let peers = control_frame_response(
            r#"{"t":"request","id":"a1","method":"api_peers"}"#,
            &mut rt,
            &tx,
            &mut pending,
        )
        .unwrap();
        assert_eq!(peers["t"], "response");
        assert_eq!(peers["ok"], false);
        assert_eq!(peers["error"], "peer registry unavailable");

        let subscribed = control_frame_response(
            r#"{"t":"request","id":"e1","method":"subscribe_events"}"#,
            &mut rt,
            &tx,
            &mut pending,
        )
        .unwrap();
        assert_eq!(subscribed["t"], "response");
        assert_eq!(subscribed["ok"], true);
        assert_eq!(subscribed["result"]["subscribed"], true);
        assert!(rt.events_subscribed);

        let project_root = control_frame_response(
            r#"{"t":"request","id":"pr1","method":"api_project_root"}"#,
            &mut rt,
            &tx,
            &mut pending,
        );
        assert!(project_root.is_none());
        assert!(pending.contains_key("pr1"));
        let project_root = rx.recv().await.unwrap();
        assert!(pending.remove(&project_root.id).is_some());
        assert_eq!(project_root.id, "pr1");
        let project_root = project_root.frame;
        assert_eq!(project_root["t"], "response");
        assert_eq!(project_root["ok"], true);
        assert!(project_root["result"].get("project_root").is_some());

        let queued = control_frame_response(
            r#"{"t":"request","id":"q1","method":"api_sessions","params":{"limit":1}}"#,
            &mut rt,
            &tx,
            &mut pending,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("q1"));
        let cancelled =
            control_frame_response(r#"{"t":"cancel","id":"q1"}"#, &mut rt, &tx, &mut pending)
                .unwrap();
        assert_eq!(cancelled["t"], "response");
        assert_eq!(cancelled["ok"], false);
        assert_eq!(cancelled["cancelled"], true);
        assert!(pending.get("q1").is_none());
    }

    #[test]
    fn session_rpc_params_parse_limits_and_ids() {
        assert_eq!(
            control_session_limit(&serde_json::json!({})),
            Some(CONTROL_DEFAULT_SESSION_LIMIT)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": 25})),
            Some(25)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": 0})),
            Some(CONTROL_DEFAULT_SESSION_LIMIT)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": "nope"})),
            Some(CONTROL_DEFAULT_SESSION_LIMIT)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": "all"})),
            None
        );
        assert_eq!(control_session_detail_limit(&serde_json::json!({})), None);
        assert_eq!(
            control_session_detail_limit(&serde_json::json!({"limit": 25})),
            Some(25)
        );
        assert_eq!(
            control_session_detail_limit(&serde_json::json!({"limit": "25"})),
            Some(25)
        );
        assert_eq!(
            control_session_detail_limit(&serde_json::json!({"limit": "all"})),
            None
        );
        assert_eq!(
            control_session_ids(&serde_json::json!({"ids": "a,b, c"})),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            control_session_ids(&serde_json::json!({"ids": ["a,b", "c"]})),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            control_project_filter(&serde_json::json!({"projects": ["a", " b "]})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            control_project_filter(&serde_json::json!({"projects": "[\"a\",\"b\"]"})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            control_project_filter(&serde_json::json!({"projects": "a,b"})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            control_capability_query(
                &serde_json::json!({"capabilities": ["display", "custom:gpu"]})
            ),
            "capability=display&capability=custom:gpu"
        );
    }

    #[test]
    fn managed_context_rpc_params_build_request_lines() {
        assert_eq!(
            managed_context_request_line(
                "records",
                &serde_json::json!({"query": "session_id=wrapper&backend_session_id=thread"})
            )
            .unwrap(),
            "GET /api/managed-context/records?session_id=wrapper&backend_session_id=thread HTTP/1.1"
        );
        assert_eq!(
            managed_context_request_line(
                "anchors",
                &serde_json::json!({
                    "session_id": "wrapper id",
                    "backend_session_id": "thread/1",
                    "intendant_session_id": "daemon+session"
                })
            )
            .unwrap(),
            "GET /api/managed-context/anchors?session_id=wrapper+id&backend_session_id=thread%2F1&intendant_session_id=daemon%2Bsession HTTP/1.1"
        );
        assert!(managed_context_request_line("fission", &serde_json::json!({})).is_none());
    }

    #[test]
    fn http_wire_response_preserves_http_status_metadata() {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"error\":\"missing\"}";
        let frame = http_wire_response("m1".into(), response.into(), "managed context");
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true);
        assert_eq!(frame["result"]["error"], "missing");
        assert_eq!(frame["result"]["_httpStatus"], 404);
        assert_eq!(frame["result"]["_httpOk"], false);
    }

    #[test]
    fn oversized_response_frames_are_chunked_and_reassemble() {
        let frame = serde_json::json!({
            "t": "response",
            "id": "large-1",
            "ok": true,
            "result": {
                "text": "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
            }
        });
        let original = frame.to_string();
        let frames = chunk_control_response_frame(frame, 40, 12);
        assert!(frames.len() > 3, "expected start/chunks/end frames");

        let start: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(start["t"], "response_start");
        assert_eq!(start["id"], "large-1");
        assert_eq!(start["encoding"], "base64-json-frame");
        assert_eq!(start["total_bytes"], original.len());

        let end: serde_json::Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(end["t"], "response_end");
        assert_eq!(end["id"], "large-1");
        assert_eq!(end["chunks"], start["chunks"]);

        let mut bytes = Vec::new();
        for (seq, text) in frames[1..frames.len() - 1].iter().enumerate() {
            let chunk: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(chunk["t"], "response_chunk");
            assert_eq!(chunk["id"], "large-1");
            assert_eq!(chunk["seq"], seq);
            let encoded = chunk["data"].as_str().unwrap();
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
            );
        }
        assert_eq!(String::from_utf8(bytes).unwrap(), original);
    }

    #[test]
    fn default_response_chunks_stay_below_datachannel_edge() {
        let frame = serde_json::json!({
            "t": "response",
            "id": "large-2",
            "ok": true,
            "result": {
                "text": "x".repeat(CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES * 4)
            }
        });
        let frames = control_frame_texts(frame);
        assert!(frames.len() > 3, "expected default chunking");

        for text in frames {
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            if parsed["t"] == "response_chunk" {
                assert!(
                    text.len() < 32 * 1024,
                    "chunk frame is too close to common DataChannel limits: {} bytes",
                    text.len()
                );
            }
        }
    }

    #[test]
    fn small_or_non_response_frames_are_not_chunked() {
        let response = serde_json::json!({"t":"response","id":"small","ok":true,"result":{}});
        assert_eq!(chunk_control_response_frame(response, 4096, 16).len(), 1);
        let event = serde_json::json!({"t":"event","id":"e1","payload":{"text":"large enough"}});
        assert_eq!(chunk_control_response_frame(event, 1, 1).len(), 1);
    }
}
