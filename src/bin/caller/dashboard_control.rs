//! Daemon-scoped WebRTC control tunnel for dashboard RPC experiments.
//!
//! The dashboard still uses HTTP plus the main WebSocket by default. This
//! module provides the first substrate for a future public-origin dashboard:
//! WebSocket signaling creates a direct browser-to-daemon WebRTC data channel,
//! then the channel carries small JSON RPC frames.

use crate::daemon_identity::{b64u, DaemonIdentity};
use crate::error::CallerError;
use crate::event::{AppEvent, ControlMsg};
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
use std::collections::{HashMap, VecDeque};
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
const CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT: usize = 16;
const CONTROL_RESPONSE_MAX_CREDIT_GRANT: usize = 64;
const CONTROL_FEATURES: &[&str] = &[
    "ping",
    "config",
    "api_agent_card",
    "api_cached_bootstrap_events",
    "api_browser_workspace_snapshot",
    "api_state_snapshot",
    "api_session_log_replay",
    "api_dashboard_bootstrap",
    "status",
    "events",
    "response_chunks",
    "response_credit",
    "stream_frames",
    "api_peers",
    "api_sessions",
    "api_sessions_stream",
    "api_session_detail",
    "api_session_delete",
    "api_session_current_agent_output",
    "api_session_current_history",
    "api_session_current_rollback",
    "api_session_current_redo",
    "api_session_current_prune",
    "api_session_current_changes",
    "api_session_context_snapshot",
    "api_session_current_upload_delete",
    "api_fs_stat",
    "api_fs_list",
    "api_fs_mkdir",
    "api_sessions_search",
    "api_settings",
    "api_settings_save",
    "api_control_msg",
    "api_key_status",
    "api_api_keys_save",
    "api_voice_session",
    "api_project_root",
    "api_displays",
    "api_recordings",
    "api_session_recordings",
    "api_worktrees",
    "api_worktrees_scan",
    "api_worktrees_remove",
    "api_managed_context_records",
    "api_managed_context_anchors",
    "api_managed_context_fission",
    "api_mcp_tool_call",
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
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
    worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
    agent_card: serde_json::Value,
    bootstrap_caches: DashboardBootstrapCaches,
    identity: Mutex<Option<Arc<DaemonIdentity>>>,
    peers: Mutex<HashMap<String, DashboardControlPeer>>,
}

#[derive(Clone, Default)]
pub struct DashboardBootstrapCaches {
    pub(crate) last_usage_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_live_usage_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_status_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_autonomy_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_external_agent_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_user_display_json: Arc<std::sync::Mutex<Option<String>>>,
}

impl DashboardControlRegistry {
    pub fn new(
        config: crate::web_gateway::WebGatewayConfig,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        bus: crate::event::EventBus,
        peer_registry: Option<crate::peer::PeerRegistry>,
        mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
        worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
        agent_card: serde_json::Value,
        bootstrap_caches: DashboardBootstrapCaches,
    ) -> Self {
        Self {
            config,
            broadcast_tx,
            bus,
            peer_registry,
            mcp_server,
            shared_session,
            project_root,
            worktree_inventory_cache,
            agent_card,
            bootstrap_caches,
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
            self.mcp_server.clone(),
            self.shared_session.clone(),
            self.project_root.clone(),
            self.worktree_inventory_cache.clone(),
            self.agent_card.clone(),
            self.bootstrap_caches.clone(),
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
        mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
        worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
        agent_card: serde_json::Value,
        bootstrap_caches: DashboardBootstrapCaches,
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
            response_credit_enabled: false,
            config: serde_json::to_value(config).unwrap_or_else(|_| serde_json::json!({})),
            bus,
            peer_registry,
            mcp_server,
            shared_session,
            project_root,
            worktree_inventory_cache,
            agent_card,
            bootstrap_caches,
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
    response_credit_enabled: bool,
    config: serde_json::Value,
    bus: crate::event::EventBus,
    peer_registry: Option<crate::peer::PeerRegistry>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
    worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
    agent_card: serde_json::Value,
    bootstrap_caches: DashboardBootstrapCaches,
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
    done: bool,
}

struct OutboundControlQueue {
    frames: VecDeque<QueuedControlFrame>,
}

enum QueuedControlFrame {
    Immediate {
        request_id: String,
        text: String,
    },
    Chunked(QueuedChunkedFrame),
}

struct QueuedChunkedFrame {
    request_id: String,
    chunk_id: String,
    start: String,
    chunks: Vec<String>,
    end: String,
    next_chunk: usize,
    credit: usize,
    started: bool,
}

enum ControlFrameTexts {
    Immediate(Vec<String>),
    Chunked {
        request_id: String,
        chunk_id: String,
        start: String,
        chunks: Vec<String>,
        end: String,
    },
}

impl OutboundControlQueue {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn enqueue_immediate(&mut self, request_id: String, text: String) {
        self.frames
            .push_back(QueuedControlFrame::Immediate { request_id, text });
    }

    fn enqueue_chunked(
        &mut self,
        request_id: String,
        chunk_id: String,
        start: String,
        chunks: Vec<String>,
        end: String,
    ) {
        self.cancel_chunk(&chunk_id);
        self.frames
            .push_back(QueuedControlFrame::Chunked(QueuedChunkedFrame {
                request_id,
                chunk_id,
                start,
                chunks,
                end,
                next_chunk: 0,
                credit: CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT,
                started: false,
            }));
    }

    fn grant_credit(&mut self, request_id: &str, chunk_id: Option<&str>, chunks: usize) {
        if chunks == 0 {
            return;
        }
        let granted = chunks.min(CONTROL_RESPONSE_MAX_CREDIT_GRANT);
        for frame in &mut self.frames {
            let QueuedControlFrame::Chunked(queued) = frame else {
                continue;
            };
            let matches_chunk = chunk_id
                .map(|id| queued.chunk_id == id)
                .unwrap_or(false);
            if matches_chunk || (chunk_id.is_none() && queued.request_id == request_id) {
                queued.credit = queued.credit.saturating_add(granted);
            }
        }
    }

    fn cancel(&mut self, request_id: &str) -> bool {
        let before = self.frames.len();
        self.frames.retain(|frame| match frame {
            QueuedControlFrame::Immediate {
                request_id: queued_id,
                ..
            } => queued_id != request_id,
            QueuedControlFrame::Chunked(queued) => {
                queued.request_id != request_id && queued.chunk_id != request_id
            }
        });
        self.frames.len() != before
    }

    fn cancel_chunk(&mut self, chunk_id: &str) -> bool {
        let before = self.frames.len();
        self.frames.retain(|frame| match frame {
            QueuedControlFrame::Immediate { .. } => true,
            QueuedControlFrame::Chunked(queued) => queued.chunk_id != chunk_id,
        });
        self.frames.len() != before
    }
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
    let mut outbound_queue = OutboundControlQueue::new();

    loop {
        let timeout_at = match drain_control_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut channels,
            &mut runtime,
            &task_tx,
            &mut pending_requests,
            &mut outbound_queue,
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
                if pending_requests.contains_key(&task_response.id) {
                    send_control_frame(
                        &mut rtc,
                        &channels,
                        &mut outbound_queue,
                        runtime.response_credit_enabled,
                        task_response.frame,
                    );
                    if task_response.done {
                        pending_requests.remove(&task_response.id);
                    }
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
    outbound_queue: &mut OutboundControlQueue,
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
        if let Some(response) =
            control_frame_response(text, runtime, task_tx, pending_requests, outbound_queue)
        {
            send_control_frame(
                rtc,
                channels,
                outbound_queue,
                runtime.response_credit_enabled,
                response,
            );
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

    drain_queued_control_frames(rtc, channels, outbound_queue);

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
    outbound_queue: &mut OutboundControlQueue,
    response_credit_enabled: bool,
    frame: serde_json::Value,
) {
    let request_id = frame
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match control_frame_text_parts(
        frame,
        CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES,
        CONTROL_RESPONSE_CHUNK_BYTES,
    ) {
        ControlFrameTexts::Immediate(frames) => {
            for text in frames {
                if response_credit_enabled && !outbound_queue.is_empty() && !request_id.is_empty() {
                    outbound_queue.enqueue_immediate(request_id.clone(), text);
                } else {
                    send_control_text(rtc, channels, text);
                }
            }
            drain_queued_control_frames(rtc, channels, outbound_queue);
        }
        ControlFrameTexts::Chunked {
            request_id,
            chunk_id,
            start,
            chunks,
            end,
        } => {
            if response_credit_enabled {
                outbound_queue.enqueue_chunked(request_id, chunk_id, start, chunks, end);
                drain_queued_control_frames(rtc, channels, outbound_queue);
            } else {
                send_control_text(rtc, channels, start);
                for text in chunks {
                    send_control_text(rtc, channels, text);
                }
                send_control_text(rtc, channels, end);
            }
        }
    }
}

#[cfg(test)]
fn control_frame_texts(frame: serde_json::Value) -> Vec<String> {
    match control_frame_text_parts(
        frame,
        CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES,
        CONTROL_RESPONSE_CHUNK_BYTES,
    ) {
        ControlFrameTexts::Immediate(frames) => frames,
        ControlFrameTexts::Chunked {
            start,
            chunks,
            end,
            ..
        } => {
            let mut frames = Vec::with_capacity(chunks.len() + 2);
            frames.push(start);
            frames.extend(chunks);
            frames.push(end);
            frames
        }
    }
}

fn control_frame_text_parts(
    frame: serde_json::Value,
    threshold_bytes: usize,
    chunk_bytes: usize,
) -> ControlFrameTexts {
    let text = frame.to_string();
    let frame_type = frame.get("t").and_then(|v| v.as_str());
    if !matches!(frame_type, Some("response") | Some("stream_event"))
        || text.len() <= threshold_bytes
        || chunk_bytes == 0
    {
        return ControlFrameTexts::Immediate(vec![text]);
    }
    let request_id = frame
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if request_id.is_empty() {
        return ControlFrameTexts::Immediate(vec![text]);
    }
    let chunk_id = frame
        .get("chunk_id")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            frame
                .get("seq")
                .and_then(|v| v.as_u64())
                .map(|seq| format!("{request_id}:{seq}"))
        })
        .unwrap_or_else(|| request_id.clone());

    let bytes = text.as_bytes();
    let chunk_count = bytes.len().div_ceil(chunk_bytes);
    let start = serde_json::json!({
        "t": "response_start",
        "id": request_id,
        "chunk_id": chunk_id,
        "encoding": "base64-json-frame",
        "total_bytes": bytes.len(),
        "chunks": chunk_count,
    })
    .to_string();
    let mut chunks = Vec::with_capacity(chunk_count);
    for (seq, chunk) in bytes.chunks(chunk_bytes).enumerate() {
        chunks.push(
            serde_json::json!({
                "t": "response_chunk",
                "id": request_id,
                "chunk_id": chunk_id,
                "seq": seq,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .to_string(),
        );
    }
    let end = serde_json::json!({
        "t": "response_end",
        "id": request_id,
        "chunk_id": chunk_id,
        "chunks": chunk_count,
    })
    .to_string();
    ControlFrameTexts::Chunked {
        request_id,
        chunk_id,
        start,
        chunks,
        end,
    }
}

#[cfg(test)]
fn chunk_control_response_frame(
    frame: serde_json::Value,
    threshold_bytes: usize,
    chunk_bytes: usize,
) -> Vec<String> {
    match control_frame_text_parts(frame, threshold_bytes, chunk_bytes) {
        ControlFrameTexts::Immediate(frames) => frames,
        ControlFrameTexts::Chunked {
            start,
            chunks,
            end,
            ..
        } => {
            let mut frames = Vec::with_capacity(chunks.len() + 2);
            frames.push(start);
            frames.extend(chunks);
            frames.push(end);
            frames
        }
    }
}

fn drain_queued_control_frames<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
) {
    loop {
        let mut pop_front = false;
        let mut completed_end: Option<String> = None;
        match outbound_queue.frames.front_mut() {
            Some(QueuedControlFrame::Immediate { text, .. }) => {
                send_control_text(rtc, channels, text.clone());
                pop_front = true;
            }
            Some(QueuedControlFrame::Chunked(queued)) => {
                if !queued.started {
                    send_control_text(rtc, channels, queued.start.clone());
                    queued.started = true;
                }
            while queued.credit > 0 && queued.next_chunk < queued.chunks.len() {
                let text = queued.chunks[queued.next_chunk].clone();
                queued.next_chunk += 1;
                queued.credit -= 1;
                send_control_text(rtc, channels, text);
            }
            if queued.next_chunk >= queued.chunks.len() {
                completed_end = Some(queued.end.clone());
            }
            }
            None => break,
        }
        if let Some(end) = completed_end {
            outbound_queue.frames.pop_front();
            send_control_text(rtc, channels, end);
            continue;
        }
        if pop_front {
            outbound_queue.frames.pop_front();
            continue;
        }
        break;
    }
}

fn control_frame_response(
    text: &str,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    outbound_queue: &mut OutboundControlQueue,
) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let t = parsed.get("t").and_then(|v| v.as_str()).unwrap_or("");
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match t {
        "hello" => {
            runtime.response_credit_enabled = parsed
                .get("features")
                .and_then(|features| features.as_array())
                .map(|features| {
                    features.iter().any(|feature| {
                        matches!(
                            feature.as_str(),
                            Some("response_credit") | Some("credit")
                        )
                    })
                })
                .unwrap_or(false);
            Some(serde_json::json!({
                "t": "hello_ack",
                "id": id,
                "protocol": CONTROL_PROTOCOL_VERSION,
                "session_id": runtime.session_id,
                "daemon_public_key": runtime.daemon_public_key,
                "features": CONTROL_FEATURES,
            }))
        }
        "ping" => Some(serde_json::json!({
            "t": "pong",
            "id": id,
            "unix_ms": chrono::Utc::now().timestamp_millis(),
        })),
        "request" => {
            let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
            match method {
                "status" => Some(status_response_frame(id, runtime)),
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
                "api_agent_card" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": runtime.agent_card,
                })),
                "api_cached_bootstrap_events" => Some(cached_bootstrap_events_response_frame(
                    id,
                    &runtime.bootstrap_caches,
                )),
                "api_sessions_stream" => {
                    spawn_control_stream(
                        id,
                        method.to_string(),
                        parsed.get("params").cloned(),
                        task_tx.clone(),
                        pending_requests,
                    );
                    None
                }
                "api_sessions"
                | "api_session_detail"
                | "api_session_delete"
                | "api_session_current_agent_output"
                | "api_session_current_history"
                | "api_session_current_rollback"
                | "api_session_current_redo"
                | "api_session_current_prune"
                | "api_session_current_changes"
                | "api_session_context_snapshot"
                | "api_session_current_upload_delete"
                | "api_fs_stat"
                | "api_fs_list"
                | "api_fs_mkdir"
                | "api_sessions_search"
                | "api_settings"
                | "api_settings_save"
                | "api_control_msg"
                | "api_key_status"
                | "api_api_keys_save"
                | "api_voice_session"
                | "api_project_root"
                | "api_displays"
                | "api_recordings"
                | "api_session_recordings"
                | "api_browser_workspace_snapshot"
                | "api_state_snapshot"
                | "api_session_log_replay"
                | "api_dashboard_bootstrap"
                | "api_worktrees"
                | "api_worktrees_scan"
                | "api_worktrees_remove"
                | "api_managed_context_records"
                | "api_managed_context_anchors"
                | "api_managed_context_fission"
                | "api_mcp_tool_call"
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
            let pending_existed = pending_requests
                .remove(&id)
                .map(|token| {
                    token.cancel();
                    true
                })
                .unwrap_or(false);
            let queued_existed = outbound_queue.cancel(&id);
            let existed = pending_existed || queued_existed;
            Some(cancelled_control_response(id, existed))
        }
        "credit" => {
            let chunks = parsed
                .get("chunks")
                .and_then(|value| value.as_u64())
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            let chunk_id = parsed.get("chunk_id").and_then(|value| value.as_str());
            outbound_queue.grant_credit(&id, chunk_id, chunks);
            None
        }
        _ => Some(serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("unknown frame type: {t}"),
        })),
    }
}

fn cached_bootstrap_events_response_frame(
    id: String,
    caches: &DashboardBootstrapCaches,
) -> serde_json::Value {
    let mut events = Vec::new();
    let mut malformed = Vec::new();
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "usage",
        &caches.last_usage_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "live_usage",
        &caches.last_live_usage_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "status",
        &caches.last_status_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "autonomy",
        &caches.last_autonomy_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "external_agent",
        &caches.last_external_agent_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "user_display",
        &caches.last_user_display_json,
    );
    let event_count = events.len();

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "events": events,
            "event_count": event_count,
            "malformed_sources": malformed,
            "omitted": [
                "state_snapshot",
                "browser_workspace_snapshot",
                "display_ready",
                "display_input_authority_state",
                "session_log_replay",
                "external_session_activity_replay"
            ],
        },
    })
}

fn push_cached_bootstrap_event(
    events: &mut Vec<serde_json::Value>,
    malformed: &mut Vec<&'static str>,
    name: &'static str,
    cache: &Arc<std::sync::Mutex<Option<String>>>,
) {
    let Some(line) = cache.lock().ok().and_then(|guard| guard.clone()) else {
        return;
    };
    match serde_json::from_str::<serde_json::Value>(&line) {
        Ok(value) => events.push(value),
        Err(_) => malformed.push(name),
    }
}

fn status_response_frame(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    result.insert("protocol".to_string(), serde_json::json!(CONTROL_PROTOCOL_VERSION));
    result.insert("session_id".to_string(), serde_json::json!(runtime.session_id));
    result.insert(
        "daemon_public_key".to_string(),
        serde_json::json!(runtime.daemon_public_key),
    );
    result.insert(
        "created_unix_ms".to_string(),
        serde_json::json!(runtime.created_unix_ms),
    );
    result.insert("features".to_string(), serde_json::json!(CONTROL_FEATURES));
    result.insert("transport".to_string(), serde_json::json!("webrtc-datachannel"));
    result.insert(
        "events_subscribed".to_string(),
        serde_json::json!(runtime.events_subscribed),
    );
    result.insert("events_sent".to_string(), serde_json::json!(runtime.events_sent));
    result.insert(
        "response_credit_enabled".to_string(),
        serde_json::json!(runtime.response_credit_enabled),
    );

    let peer_registry_available = runtime.peer_registry.is_some();
    let capabilities = [
        ("api_peers_available", peer_registry_available),
        ("api_agent_card_available", true),
        ("api_cached_bootstrap_events_available", true),
        ("api_browser_workspace_snapshot_available", true),
        ("api_state_snapshot_available", true),
        ("api_session_log_replay_available", true),
        ("api_dashboard_bootstrap_available", true),
        ("api_sessions_available", true),
        ("api_sessions_stream_available", true),
        ("api_session_detail_available", true),
        ("api_session_delete_available", true),
        ("api_session_current_agent_output_available", true),
        ("api_session_current_history_available", true),
        ("api_session_current_rollback_available", true),
        ("api_session_current_redo_available", true),
        ("api_session_current_prune_available", true),
        ("api_session_current_changes_available", true),
        ("api_session_context_snapshot_available", true),
        ("api_session_current_upload_delete_available", true),
        ("api_fs_stat_available", true),
        ("api_fs_list_available", true),
        ("api_fs_mkdir_available", true),
        ("api_sessions_search_available", true),
        ("api_settings_available", true),
        ("api_settings_save_available", runtime.project_root.is_some()),
        ("api_control_msg_available", true),
        ("api_key_status_available", true),
        ("api_api_keys_save_available", true),
        ("api_voice_session_available", true),
        ("api_project_root_available", true),
        ("api_displays_available", true),
        ("api_recordings_available", true),
        ("api_session_recordings_available", true),
        ("api_worktrees_available", true),
        ("api_worktrees_scan_available", true),
        ("api_worktrees_remove_available", true),
        ("api_managed_context_available", true),
        ("api_mcp_tool_call_available", runtime.mcp_server.is_some()),
        ("api_peer_mutations_available", peer_registry_available),
        ("api_peer_pairing_available", true),
        ("api_coordinator_available", peer_registry_available),
    ];
    for (name, available) in capabilities {
        result.insert(name.to_string(), serde_json::json!(available));
    }

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": serde_json::Value::Object(result),
    })
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
        let _ = task_tx
            .send(ControlTaskResponse {
                id,
                frame,
                done: true,
            })
            .await;
    });
}

fn spawn_control_stream(
    id: String,
    method: String,
    params: Option<serde_json::Value>,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
) {
    if let Some(previous) = pending_requests.remove(&id) {
        previous.cancel();
    }
    let cancel = CancellationToken::new();
    pending_requests.insert(id.clone(), cancel.clone());
    tokio::spawn(async move {
        match method.as_str() {
            "api_sessions_stream" => {
                stream_sessions_response(id, params.as_ref(), task_tx, cancel).await;
            }
            _ => {
                let frame = serde_json::json!({
                    "t": "stream_end",
                    "id": id,
                    "ok": false,
                    "error": format!("unknown stream method: {method}"),
                });
                let _ = task_tx
                    .send(ControlTaskResponse {
                        id,
                        frame,
                        done: true,
                    })
                    .await;
            }
        }
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
        "api_session_delete" => api_session_delete_response(id, params.as_ref()).await,
        "api_session_current_agent_output" => {
            api_session_current_agent_output_response(id, params.as_ref(), &runtime).await
        }
        "api_session_current_history" => api_session_current_history_response(id, &runtime).await,
        "api_session_current_rollback" => {
            api_session_current_rollback_response(id, params.as_ref(), &runtime).await
        }
        "api_session_current_redo" => api_session_current_redo_response(id, &runtime).await,
        "api_session_current_prune" => api_session_current_prune_response(id, &runtime).await,
        "api_session_current_changes" => {
            api_session_current_changes_response(id, params.as_ref(), &runtime).await
        },
        "api_session_context_snapshot" => {
            api_session_context_snapshot_response(id, params.as_ref()).await
        },
        "api_session_current_upload_delete" => {
            api_session_current_upload_delete_response(id, params.as_ref(), &runtime).await
        },
        "api_fs_stat" => api_fs_stat_response(id, params.as_ref()).await,
        "api_fs_list" => api_fs_list_response(id, params.as_ref()).await,
        "api_fs_mkdir" => api_fs_mkdir_response(id, params.as_ref()).await,
        "api_sessions_search" => api_sessions_search_response(id, params.as_ref(), cancel).await,
        "api_settings" => api_settings_response(id, &runtime).await,
        "api_settings_save" => api_settings_save_response(id, params.as_ref(), &runtime).await,
        "api_control_msg" => api_control_msg_response(id, params.as_ref(), &runtime).await,
        "api_key_status" => json_body_response(
            id,
            crate::web_gateway::api_key_status_response_body(),
            "api key status",
        ),
        "api_api_keys_save" => api_api_keys_save_response(id, params.as_ref()).await,
        "api_voice_session" => api_voice_session_response(id, &runtime).await,
        "api_project_root" => json_body_response(
            id,
            crate::web_gateway::project_root_response_body(runtime.project_root.as_deref()),
            "project root",
        ),
        "api_displays" => api_displays_response(id, &runtime).await,
        "api_recordings" => api_recordings_response(id, &runtime).await,
        "api_session_recordings" => api_session_recordings_response(id, params.as_ref()).await,
        "api_browser_workspace_snapshot" => api_browser_workspace_snapshot_response(id).await,
        "api_state_snapshot" => api_state_snapshot_response(id, &runtime).await,
        "api_session_log_replay" => api_session_log_replay_response(id, &runtime).await,
        "api_dashboard_bootstrap" => api_dashboard_bootstrap_response(id, &runtime).await,
        "api_worktrees" => api_worktrees_response(id, &runtime).await,
        "api_worktrees_scan" => api_worktrees_scan_response(id, &runtime).await,
        "api_worktrees_remove" => {
            api_worktrees_remove_response(id, params.as_ref(), &runtime).await
        }
        "api_managed_context_records" => {
            api_managed_context_response(id, "records", params.as_ref(), &runtime).await
        }
        "api_managed_context_anchors" => {
            api_managed_context_response(id, "anchors", params.as_ref(), &runtime).await
        }
        "api_managed_context_fission" => {
            api_managed_context_response(id, "fission", params.as_ref(), &runtime).await
        }
        "api_mcp_tool_call" => api_mcp_tool_call_response(id, params.as_ref(), &runtime).await,
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

async fn stream_sessions_response(
    id: String,
    params: Option<&serde_json::Value>,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    cancel: CancellationToken,
) {
    let request_line = sessions_stream_request_line(params);
    let (line_tx, line_rx) = mpsc::channel::<String>(64);
    let stream_task = tokio::task::spawn_blocking(move || {
        crate::web_gateway::stream_sessions_from_request(&request_line, line_tx);
    });
    stream_json_lines_response(
        id,
        "api_sessions_stream".to_string(),
        line_rx,
        stream_task,
        task_tx,
        cancel,
    )
    .await;
}

async fn stream_json_lines_response(
    id: String,
    method: String,
    mut line_rx: mpsc::Receiver<String>,
    stream_task: tokio::task::JoinHandle<()>,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }

    if task_tx
        .send(ControlTaskResponse {
            id: id.clone(),
            frame: serde_json::json!({
                "t": "stream_start",
                "id": id,
                "method": method,
            }),
            done: false,
        })
        .await
        .is_err()
    {
        return;
    }

    let mut seq: u64 = 0;
    while let Some(line) = line_rx.recv().await {
        if cancel.is_cancelled() {
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(event) => event,
            Err(e) => {
                let frame = serde_json::json!({
                    "t": "stream_end",
                    "id": id,
                    "ok": false,
                    "error": format!("session stream returned invalid JSON: {e}"),
                });
                let _ = task_tx
                    .send(ControlTaskResponse {
                        id,
                        frame,
                        done: true,
                    })
                    .await;
                return;
            }
        };
        let chunk_id = format!("{id}:{seq}");
        let frame = serde_json::json!({
            "t": "stream_event",
            "id": id,
            "seq": seq,
            "chunk_id": chunk_id,
            "event": event,
        });
        seq = seq.saturating_add(1);
        if task_tx
            .send(ControlTaskResponse {
                id: id.clone(),
                frame,
                done: false,
            })
            .await
            .is_err()
        {
            return;
        }
    }

    let frame = match stream_task.await {
        Ok(()) => serde_json::json!({
            "t": "stream_end",
            "id": id,
            "ok": true,
            "result": {
                "events": seq,
            },
        }),
        Err(e) => serde_json::json!({
            "t": "stream_end",
            "id": id,
            "ok": false,
            "error": format!("session stream task failed: {e}"),
        }),
    };
    if !cancel.is_cancelled() {
        let _ = task_tx
            .send(ControlTaskResponse {
                id,
                frame,
                done: true,
            })
            .await;
    }
}

fn sessions_stream_request_line(params: Option<&serde_json::Value>) -> String {
    let Some(params) = params else {
        return "GET /api/sessions/stream HTTP/1.1".to_string();
    };
    let Some(limit_value) = params.get("limit") else {
        return "GET /api/sessions/stream HTTP/1.1".to_string();
    };
    let limit = match limit_value {
        serde_json::Value::String(value) => {
            let value = value.trim();
            if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("full") {
                "all".to_string()
            } else {
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| *limit > 0)
                    .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT)
                    .to_string()
            }
        }
        serde_json::Value::Number(value) => value
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT)
            .to_string(),
        _ => CONTROL_DEFAULT_SESSION_LIMIT.to_string(),
    };
    format!("GET /api/sessions/stream?limit={limit} HTTP/1.1")
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

async fn api_session_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    let target =
        optional_string_param(&params, &["target"]).unwrap_or_else(|| "session".to_string());
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::delete_session_data(&session_id, &target)
    })
    .await;
    match result {
        Ok(body) => json_body_response(id, body, "session delete"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("session delete task failed: {e}"),
        }),
    }
}

async fn api_session_current_agent_output_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    match active_session_log_dir(runtime).await {
        Ok(Some(log_dir)) => http_wire_response(
            id,
            crate::web_gateway::current_agent_output_post_response(&body_text, &log_dir),
            "agent output",
        ),
        Ok(None) => http_body_response(
            id,
            404,
            serde_json::json!({"error": "no active session log"}).to_string(),
            "agent output",
        ),
        Err(error) => http_body_response(
            id,
            500,
            serde_json::json!({"error": error}).to_string(),
            "agent output",
        ),
    }
}

async fn api_session_current_history_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, _) = active_history_handles(runtime).await;
    let (status_line, body) = crate::web_gateway::handle_history_get(file_watcher.as_ref()).await;
    http_body_response(
        id,
        status_line_code(status_line),
        body,
        "session history",
    )
}

async fn api_session_current_rollback_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (file_watcher, agent_state) = active_history_handles(runtime).await;
    let (status_line, body) = crate::web_gateway::handle_history_rollback(
        &body_text,
        file_watcher.as_ref(),
        agent_state.as_ref(),
        &runtime.bus,
    )
    .await;
    http_body_response(
        id,
        status_line_code(status_line),
        body,
        "session rollback",
    )
}

async fn api_session_current_redo_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, agent_state) = active_history_handles(runtime).await;
    let (status_line, body) =
        crate::web_gateway::handle_history_redo(file_watcher.as_ref(), agent_state.as_ref()).await;
    http_body_response(id, status_line_code(status_line), body, "session redo")
}

async fn api_session_current_prune_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, _) = active_history_handles(runtime).await;
    let (status_line, body) = crate::web_gateway::handle_history_prune(file_watcher.as_ref()).await;
    http_body_response(id, status_line_code(status_line), body, "session prune")
}

async fn api_session_current_changes_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let request_line = changes_request_line(params);
    let (snapshot_dir, project_root) = active_changes_handles(runtime).await;
    let home = crate::platform::home_dir();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::handle_changes_request_for_home(
            &request_line,
            snapshot_dir.as_deref(),
            project_root.as_deref(),
            &home,
        )
    })
    .await;
    match result {
        Ok((status_line, body)) => http_body_response(
            id,
            status_line_code(status_line),
            body,
            "session changes",
        ),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("session changes task failed: {e}"),
        }),
    }
}

async fn api_session_context_snapshot_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return missing_param_response(id, "session_id");
    }
    let source = optional_string_param(&params, &["source"]).unwrap_or_else(|| "intendant".into());
    let file = optional_string_param(&params, &["file"]);
    let request_id = optional_string_param(&params, &["request_id", "requestId"]);
    let request_index = match optional_u64_param(&params, &["request_index", "requestIndex"]) {
        Ok(value) => value,
        Err(error) => {
            return http_body_response(
                id,
                400,
                serde_json::json!({ "error": error }).to_string(),
                "context snapshot",
            );
        }
    };
    let ts = optional_string_param(&params, &["ts"]);
    let home = crate::platform::home_dir();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_context_snapshot_response_body(
            &home,
            &session_id,
            &source,
            file,
            request_id,
            request_index,
            ts,
        )
    })
    .await;
    match result {
        Ok((status_line, body)) => http_body_response(
            id,
            status_line_code(status_line),
            body,
            "context snapshot",
        ),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("context snapshot task failed: {e}"),
        }),
    }
}

async fn api_session_current_upload_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let upload_id = string_param(&params, &["upload_id", "uploadId", "id"]);
    let (project_root, session_dir) = match active_upload_handles(runtime).await {
        Ok(handles) => handles,
        Err(error) => {
            return http_body_response(
                id,
                500,
                serde_json::json!({ "error": error }).to_string(),
                "upload delete",
            );
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::current_upload_delete_response_body(
            project_root.as_deref(),
            session_dir.as_deref(),
            &upload_id,
        )
    })
    .await;
    match result {
        Ok((status_line, body, deleted_id)) => {
            if let Some(id) = deleted_id {
                runtime.bus.send(crate::event::AppEvent::UploadDeleted { id });
            }
            http_body_response(id, status_line_code(status_line), body, "upload delete")
        }
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("upload delete task failed: {e}"),
        }),
    }
}

async fn api_fs_stat_response(id: String, params: Option<&serde_json::Value>) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let path = string_param(&params, &["path"]);
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::dashboard_fs_stat_response_body(&path)
    })
    .await;
    match result {
        Ok(Ok(body)) => http_body_response(id, 200, body, "filesystem stat"),
        Ok(Err(error)) => http_body_response(
            id,
            400,
            serde_json::json!({ "error": error }).to_string(),
            "filesystem stat",
        ),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem stat task failed: {e}"),
        }),
    }
}

async fn api_fs_list_response(id: String, params: Option<&serde_json::Value>) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let path = string_param(&params, &["path"]);
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::dashboard_fs_list_response_body(&path)
    })
    .await;
    match result {
        Ok(Ok(body)) => http_body_response(id, 200, body, "filesystem list"),
        Ok(Err(error)) => http_body_response(
            id,
            400,
            serde_json::json!({ "error": error }).to_string(),
            "filesystem list",
        ),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem list task failed: {e}"),
        }),
    }
}

async fn api_fs_mkdir_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let path = string_param(&params, &["path"]);
    let (status_line, body) = crate::web_gateway::dashboard_fs_mkdir_response_body(&path);
    http_body_response(id, status_line_code(&status_line), body, "filesystem mkdir")
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

async fn api_voice_session_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let provider = runtime
        .config
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("gemini");
    let model = runtime
        .config
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match crate::web_gateway::mint_session_token(provider, model).await {
        Ok(body) => http_body_response(id, 200, body, "voice session"),
        Err(msg) => http_body_response(
            id,
            502,
            serde_json::json!({ "error": msg }).to_string(),
            "voice session",
        ),
    }
}

async fn api_recordings_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let recording_registry = active_recording_registry(runtime).await;
    json_body_response(
        id,
        crate::web_gateway::recordings_list_response_body(recording_registry).await,
        "recordings",
    )
}

async fn api_session_recordings_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    let (status_line, body) =
        crate::web_gateway::session_recordings_list_response_body(&session_id);
    http_body_response(
        id,
        status_line_code(status_line),
        body,
        "session recordings",
    )
}

async fn api_browser_workspace_snapshot_response(id: String) -> serde_json::Value {
    let workspaces = crate::browser_workspace::list_workspaces().await;
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "t": "browser_workspace_snapshot",
            "workspaces": workspaces,
        },
    })
}

async fn api_state_snapshot_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let (daemon_session_id, query_ctx, session_log) = {
        let session = runtime.shared_session.read().await;
        (
            session.daemon_session_id.clone(),
            session.query_ctx.clone(),
            session.session_log.clone(),
        )
    };
    let state = query_ctx
        .as_ref()
        .map(|ctx| {
            ctx.agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();
    let bootstrap_session_id = daemon_session_id
        .or_else(|| {
            query_ctx
                .as_ref()
                .and_then(|ctx| control_replay_session_id_from_dir(&ctx.log_dir))
        })
        .or_else(|| session_log.as_ref().and_then(control_session_log_id))
        .unwrap_or_default();

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "t": "state_snapshot",
            "state": state,
            "connection_id": runtime.session_id.clone(),
            "config": runtime.config.clone(),
            "session_id": bootstrap_session_id,
        },
    })
}

async fn api_session_log_replay_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let replay_log_dir = active_replay_log_dir(runtime).await;
    let mut replay = replay_log_dir
        .as_ref()
        .and_then(|log_dir| {
            crate::web_gateway::session_log_replay_payload_for_websocket_bootstrap(log_dir)
        })
        .and_then(|(payload, external_session_id)| {
            let mut value = serde_json::from_str::<serde_json::Value>(&payload).ok()?;
            if let (Some(external_session_id), Some(map)) =
                (external_session_id, value.as_object_mut())
            {
                map.insert(
                    "external_session_id".to_string(),
                    serde_json::Value::String(external_session_id),
                );
            }
            Some(value)
        })
        .unwrap_or_else(|| {
            serde_json::json!({
                "t": "log_replay",
                "entries": [],
                "available": false,
            })
        });
    if let Some(map) = replay.as_object_mut() {
        map.entry("available".to_string())
            .or_insert(serde_json::Value::Bool(true));
    }

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": replay,
    })
}

async fn api_dashboard_bootstrap_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let mut frames = Vec::new();
    if let Some(frame) =
        response_result(api_state_snapshot_response("bootstrap-state".into(), runtime).await)
    {
        frames.push(frame);
    }
    if let Some(result) = response_result(cached_bootstrap_events_response_frame(
        "bootstrap-cached".into(),
        &runtime.bootstrap_caches,
    )) {
        if let Some(events) = result.get("events").and_then(|value| value.as_array()) {
            frames.extend(events.iter().cloned());
        }
    }
    if let Some(frame) =
        response_result(api_browser_workspace_snapshot_response("bootstrap-browser".into()).await)
    {
        frames.push(frame);
    }
    if let Some(frame) =
        response_result(api_session_log_replay_response("bootstrap-replay".into(), runtime).await)
    {
        frames.push(frame);
    }
    let frame_count = frames.len();

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
            "omitted": [
                "display_ready",
                "display_input_authority_state",
                "external_session_activity_replay"
            ],
        },
    })
}

fn response_result(response: serde_json::Value) -> Option<serde_json::Value> {
    response.get("result").cloned()
}

fn control_replay_session_id_from_dir(log_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(log_dir.join("session_meta.json"))
        .ok()
        .and_then(|meta| serde_json::from_str::<crate::session_log::SessionMeta>(&meta).ok())
        .map(|meta| meta.session_id)
        .filter(|session_id| !session_id.trim().is_empty())
        .or_else(|| {
            log_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|session_id| !session_id.trim().is_empty())
        })
}

fn control_session_log_id(
    session_log: &Arc<std::sync::Mutex<crate::session_log::SessionLog>>,
) -> Option<String> {
    session_log
        .lock()
        .ok()
        .map(|log| log.session_id().to_string())
        .filter(|id| !id.trim().is_empty())
}

async fn active_replay_log_dir(runtime: &ControlRuntime) -> Option<PathBuf> {
    let (query_ctx, session_log) = {
        let session = runtime.shared_session.read().await;
        (session.query_ctx.clone(), session.session_log.clone())
    };
    query_ctx.as_ref().map(|ctx| ctx.log_dir.clone()).or_else(|| {
        session_log
            .as_ref()
            .and_then(|log| log.lock().ok().map(|log| log.dir().to_path_buf()))
    })
}

async fn api_worktrees_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let body = runtime
        .worktree_inventory_cache
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_else(crate::web_gateway::empty_worktree_inventory_response);
    json_body_response(id, body, "worktrees")
}

async fn api_worktrees_scan_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let project_root = runtime.project_root.clone();
    let cache = runtime.worktree_inventory_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        let body = crate::web_gateway::scan_worktree_inventory_response(
            &home,
            project_root.as_deref(),
        );
        if let Ok(mut guard) = cache.lock() {
            *guard = Some(body.clone());
        }
        body
    })
    .await;
    match result {
        Ok(body) => json_body_response(id, body, "worktree scan"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({
                "error": format!("worktree scan task failed: {e}")
            })
            .to_string(),
            "worktree scan",
        ),
    }
}

async fn api_worktrees_remove_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let cache = runtime.worktree_inventory_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = crate::web_gateway::remove_worktree_inventory_response(&home, &body_text);
        if result.0 == "200 OK" {
            if let Ok(mut guard) = cache.lock() {
                *guard = None;
            }
        }
        result
    })
    .await;
    match result {
        Ok((status_line, body)) => http_body_response(
            id,
            status_line_code(status_line),
            body,
            "worktree remove",
        ),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({
                "ok": false,
                "error": format!("worktree removal task failed: {e}")
            })
            .to_string(),
            "worktree remove",
        ),
    }
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

async fn api_mcp_tool_call_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let mcp_id = params
        .get("mcp_id")
        .or_else(|| params.get("rpc_id"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!(id.clone()));
    let Some(server) = runtime.mcp_server.as_ref() else {
        return http_body_response(
            id,
            503,
            mcp_error_body(mcp_id, -32603, "MCP server not available"),
            "mcp tool call",
        );
    };
    let session_id = optional_string_param(
        &params,
        &["session_id", "session", "intendant_session", "sessionId"],
    );
    if session_id.is_none() {
        return http_body_response(
            id,
            400,
            mcp_error_body(mcp_id, -32602, "missing session_id"),
            "mcp tool call",
        );
    }
    let name = string_param(&params, &["name", "tool", "tool_name"]);
    if name.is_empty() {
        return http_body_response(
            id,
            400,
            mcp_error_body(mcp_id, -32602, "missing tool name"),
            "mcp tool call",
        );
    }
    let arguments = params
        .get("arguments")
        .or_else(|| params.get("args"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let managed_context = optional_managed_context_param(&params);
    match server
        .call_tool_by_name_for_session(&name, arguments, session_id.as_deref(), managed_context)
        .await
    {
        Ok(result) => {
            let result = serde_json::to_value(result).unwrap_or_else(|e| {
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Failed to serialize MCP tool result: {}", e),
                    }],
                    "isError": true,
                })
            });
            http_body_response(
                id,
                200,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": mcp_id,
                    "result": result,
                })
                .to_string(),
                "mcp tool call",
            )
        }
        Err(error) => http_body_response(
            id,
            200,
            mcp_error_body(mcp_id, -32603, &error),
            "mcp tool call",
        ),
    }
}

fn mcp_error_body(id: serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
    .to_string()
}

fn optional_managed_context_param(params: &serde_json::Value) -> Option<bool> {
    for name in ["managed_context", "managedContext", "codex_managed_context"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        if let Some(flag) = value.as_bool() {
            return Some(flag);
        }
        if let Some(mode) = value.as_str() {
            return Some(crate::project::codex_managed_context_enabled(mode));
        }
    }
    None
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

async fn api_control_msg_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(params) = params else {
        return missing_param_response(id, "message");
    };
    let message = params
        .get("message")
        .or_else(|| params.get("control_msg"))
        .or_else(|| params.get("controlMsg"))
        .cloned()
        .unwrap_or_else(|| params.clone());
    let ctrl = match serde_json::from_value::<ControlMsg>(message) {
        Ok(ctrl) => ctrl,
        Err(e) => {
            return http_body_response(
                id,
                400,
                serde_json::json!({
                    "ok": false,
                    "error": format!("invalid control message: {e}"),
                })
                .to_string(),
                "control message",
            )
        }
    };
    if !dashboard_control_msg_allowed(&ctrl) {
        return http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "control action not available over dashboard WebRTC: {}",
                    dashboard_control_msg_action(&ctrl)
                ),
            })
            .to_string(),
            "control message",
        );
    }
    let action = dashboard_control_msg_action(&ctrl);
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[dashboard-control] ControlMsg: {action}"),
        level: Some(crate::types::LogLevel::Debug),
        turn: None,
    });
    runtime.bus.send(AppEvent::ControlCommand(ctrl));
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "action": action,
        },
    })
}

fn dashboard_control_msg_allowed(ctrl: &ControlMsg) -> bool {
    matches!(
        ctrl,
        ControlMsg::SetAutonomy { .. }
            | ControlMsg::SetApprovalRule { .. }
            | ControlMsg::SetExternalAgent { .. }
            | ControlMsg::SetCodexCommand { .. }
            | ControlMsg::SetCodexManagedCommand { .. }
            | ControlMsg::SetCodexSandbox { .. }
            | ControlMsg::SetCodexApprovalPolicy { .. }
            | ControlMsg::SetCodexModel { .. }
            | ControlMsg::SetCodexReasoningEffort { .. }
            | ControlMsg::SetCodexServiceTier { .. }
            | ControlMsg::SetCodexWebSearch { .. }
            | ControlMsg::SetCodexNetworkAccess { .. }
            | ControlMsg::SetCodexWritableRoots { .. }
            | ControlMsg::SetCodexManagedContext { .. }
            | ControlMsg::SetCodexContextArchive { .. }
            | ControlMsg::SetGeminiModel { .. }
            | ControlMsg::SetGeminiApprovalMode { .. }
            | ControlMsg::SetGeminiSandbox { .. }
            | ControlMsg::SetGeminiExtensions { .. }
            | ControlMsg::SetGeminiAllowedMcpServers { .. }
            | ControlMsg::SetGeminiIncludeDirectories { .. }
            | ControlMsg::SetGeminiDebug { .. }
            | ControlMsg::SetVerbosity { .. }
    )
}

fn dashboard_control_msg_action(ctrl: &ControlMsg) -> &'static str {
    match ctrl {
        ControlMsg::Status { .. } => "status",
        ControlMsg::Usage => "usage",
        ControlMsg::Approve { .. } => "approve",
        ControlMsg::Deny { .. } => "deny",
        ControlMsg::Skip { .. } => "skip",
        ControlMsg::ApproveAll { .. } => "approve_all",
        ControlMsg::Input { .. } => "input",
        ControlMsg::SetAutonomy { .. } => "set_autonomy",
        ControlMsg::SetApprovalRule { .. } => "set_approval_rule",
        ControlMsg::SetExternalAgent { .. } => "set_external_agent",
        ControlMsg::SetCodexCommand { .. } => "set_codex_command",
        ControlMsg::SetCodexManagedCommand { .. } => "set_codex_managed_command",
        ControlMsg::SetCodexSandbox { .. } => "set_codex_sandbox",
        ControlMsg::SetCodexApprovalPolicy { .. } => "set_codex_approval_policy",
        ControlMsg::SetCodexModel { .. } => "set_codex_model",
        ControlMsg::SetCodexReasoningEffort { .. } => "set_codex_reasoning_effort",
        ControlMsg::SetCodexServiceTier { .. } => "set_codex_service_tier",
        ControlMsg::SetCodexWebSearch { .. } => "set_codex_web_search",
        ControlMsg::SetCodexNetworkAccess { .. } => "set_codex_network_access",
        ControlMsg::SetCodexWritableRoots { .. } => "set_codex_writable_roots",
        ControlMsg::SetCodexManagedContext { .. } => "set_codex_managed_context",
        ControlMsg::SetCodexContextArchive { .. } => "set_codex_context_archive",
        ControlMsg::CodexThreadAction { .. } => "codex_thread_action",
        ControlMsg::RenameSession { .. } => "rename_session",
        ControlMsg::ConfigureSessionAgent { .. } => "configure_session_agent",
        ControlMsg::StopSession { .. } => "stop_session",
        ControlMsg::RestartSession { .. } => "restart_session",
        ControlMsg::ResumeSession { .. } => "resume_session",
        ControlMsg::SetGeminiModel { .. } => "set_gemini_model",
        ControlMsg::SetGeminiApprovalMode { .. } => "set_gemini_approval_mode",
        ControlMsg::SetGeminiSandbox { .. } => "set_gemini_sandbox",
        ControlMsg::SetGeminiExtensions { .. } => "set_gemini_extensions",
        ControlMsg::SetGeminiAllowedMcpServers { .. } => "set_gemini_allowed_mcp_servers",
        ControlMsg::SetGeminiIncludeDirectories { .. } => "set_gemini_include_directories",
        ControlMsg::SetGeminiDebug { .. } => "set_gemini_debug",
        ControlMsg::GeminiThreadAction { .. } => "gemini_thread_action",
        ControlMsg::SetVerbosity { .. } => "set_verbosity",
        ControlMsg::ScheduleControllerRestart { .. } => "schedule_controller_restart",
        ControlMsg::ControllerTurnComplete { .. } => "controller_turn_complete",
        ControlMsg::GetRestartStatus => "get_restart_status",
        ControlMsg::CancelControllerRestart { .. } => "cancel_controller_restart",
        ControlMsg::RequestControllerLoopHalt { .. } => "request_controller_loop_halt",
        ControlMsg::ClearControllerLoopHalt => "clear_controller_loop_halt",
        ControlMsg::InterveneControllerLoop { .. } => "intervene_controller_loop",
        ControlMsg::GetControllerLoopStatus => "get_controller_loop_status",
        ControlMsg::CreateSession { .. } => "create_session",
        ControlMsg::StartTask { .. } => "start_task",
        ControlMsg::FollowUp { .. } => "follow_up",
        ControlMsg::CancelFollowUp { .. } => "cancel_follow_up",
        ControlMsg::EditUserMessage { .. } => "edit_user_message",
        ControlMsg::QueryDetail { .. } => "query_detail",
        ControlMsg::RecallMemory { .. } => "recall_memory",
        ControlMsg::TakeDisplay { .. } => "take_display",
        ControlMsg::ReleaseDisplay { .. } => "release_display",
        ControlMsg::GrantUserDisplay { .. } => "grant_user_display",
        ControlMsg::RevokeUserDisplay { .. } => "revoke_user_display",
        ControlMsg::CreateBrowserWorkspace { .. } => "create_browser_workspace",
        ControlMsg::CloseBrowserWorkspace { .. } => "close_browser_workspace",
        ControlMsg::AcquireBrowserWorkspace { .. } => "acquire_browser_workspace",
        ControlMsg::ReleaseBrowserWorkspace { .. } => "release_browser_workspace",
        ControlMsg::ListDisplays => "list_displays",
        ControlMsg::InvokeSkill { .. } => "invoke_skill",
        ControlMsg::Quit => "quit",
        ControlMsg::SetupDebugScreen => "setup_debug_screen",
        ControlMsg::TeardownDebugScreen => "teardown_debug_screen",
        ControlMsg::StartDebugRecording => "start_debug_recording",
        ControlMsg::StopDebugRecording => "stop_debug_recording",
        ControlMsg::StartRecording { .. } => "start_recording",
        ControlMsg::StopRecording { .. } => "stop_recording",
        ControlMsg::DeleteRecording { .. } => "delete_recording",
        ControlMsg::Interrupt { .. } => "interrupt",
        ControlMsg::Steer { .. } => "steer",
        ControlMsg::CancelSteer { .. } => "cancel_steer",
        ControlMsg::WebRtcSignal { .. } => "webrtc_signal",
        ControlMsg::RequestDisplayInputAuthority { .. } => "request_display_input_authority",
        ControlMsg::ReleaseDisplayInputAuthority { .. } => "release_display_input_authority",
        ControlMsg::SetDiagnosticsVisualMarker { .. } => "set_diagnostics_visual_marker",
    }
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

fn changes_request_line(params: Option<&serde_json::Value>) -> String {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let path = string_param(&params, &["path", "file", "file_path", "filePath"]);
    let query = request_query_string_param(&params);
    let mut target = "/api/session/current/changes".to_string();
    if !path.trim().is_empty() {
        target.push('/');
        target.push_str(&percent_encode_path_value(path.trim()));
    }
    if !query.is_empty() {
        target.push('?');
        target.push_str(&query);
    }
    format!("GET {target} HTTP/1.1")
}

fn request_query_string_param(params: &serde_json::Value) -> String {
    string_param(params, &["query", "search"])
        .trim()
        .trim_start_matches('?')
        .chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '#')
        .collect()
}

fn percent_encode_path_value(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
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

async fn active_history_handles(
    runtime: &ControlRuntime,
) -> (
    Option<crate::file_watcher::SharedFileWatcher>,
    Option<Arc<std::sync::Mutex<crate::presence::AgentStateSnapshot>>>,
) {
    let session = runtime.shared_session.read().await;
    let file_watcher = session.file_watcher.clone();
    let agent_state = session
        .query_ctx
        .as_ref()
        .map(|ctx| Arc::clone(&ctx.agent_state));
    (file_watcher, agent_state)
}

async fn active_changes_handles(runtime: &ControlRuntime) -> (Option<PathBuf>, Option<PathBuf>) {
    let session = runtime.shared_session.read().await;
    (
        session.snapshot_dir.clone(),
        session.project_root_for_changes.clone(),
    )
}

async fn active_upload_handles(
    runtime: &ControlRuntime,
) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    let (project_root, session_log) = {
        let session = runtime.shared_session.read().await;
        (
            session.project_root_for_changes.clone(),
            session.session_log.clone(),
        )
    };
    let session_dir = match session_log {
        Some(log) => Some(
            log.lock()
                .map_err(|_| "session log lock poisoned".to_string())?
                .dir()
                .to_path_buf(),
        ),
        None => None,
    };
    Ok((project_root, session_dir))
}

async fn active_recording_registry(
    runtime: &ControlRuntime,
) -> Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>> {
    let session = runtime.shared_session.read().await;
    session.recording_registry.clone()
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

fn optional_string_param(params: &serde_json::Value, names: &[&str]) -> Option<String> {
    let value = string_param(params, names);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn optional_u64_param(
    params: &serde_json::Value,
    names: &[&str],
) -> Result<Option<u64>, String> {
    for name in names {
        let Some(value) = params.get(*name) else {
            continue;
        };
        if value.is_null() {
            return Ok(None);
        }
        if let Some(number) = value.as_u64() {
            return Ok(Some(number));
        }
        if let Some(text) = value.as_str() {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            return text
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("invalid {name}"));
        }
        return Err(format!("invalid {name}"));
    }
    Ok(None)
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
            response_credit_enabled: false,
            config: serde_json::json!({"provider":"openai"}),
            agent_card: serde_json::json!({
                "id": "intendant:test-daemon",
                "label": "test-daemon",
            }),
            bus: crate::event::EventBus::new(),
            peer_registry: None,
            mcp_server: None,
            shared_session: crate::web_gateway::ActiveSessionState::empty(),
            project_root: None,
            worktree_inventory_cache: Arc::new(std::sync::Mutex::new(None)),
            bootstrap_caches: DashboardBootstrapCaches::default(),
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
        let mut outbound = OutboundControlQueue::new();
        let hello = control_frame_response(
            r#"{"t":"hello","id":"h1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert_eq!(hello["session_id"], "session-1");

        let ping = control_frame_response(
            r#"{"t":"ping","id":"p1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(ping["t"], "pong");
        assert_eq!(ping["id"], "p1");

        let config = control_frame_response(
            r#"{"t":"request","id":"r1","method":"config"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(config["t"], "response");
        assert_eq!(config["ok"], true);
        assert_eq!(config["result"]["provider"], "openai");

        let card = control_frame_response(
            r#"{"t":"request","id":"c1","method":"api_agent_card"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(card["t"], "response");
        assert_eq!(card["ok"], true);
        assert_eq!(card["result"]["id"], "intendant:test-daemon");
        assert_eq!(card["result"]["label"], "test-daemon");

        {
            let mut guard = rt.bootstrap_caches.last_status_json.lock().unwrap();
            *guard = Some(r#"{"event":"status","session_id":"s-1"}"#.to_string());
        }
        {
            let mut guard = rt.bootstrap_caches.last_autonomy_json.lock().unwrap();
            *guard = Some(r#"{"event":"autonomy_changed","mode":"ask"}"#.to_string());
        }
        let cached_bootstrap = control_frame_response(
            r#"{"t":"request","id":"b1","method":"api_cached_bootstrap_events"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(cached_bootstrap["t"], "response");
        assert_eq!(cached_bootstrap["ok"], true);
        assert_eq!(cached_bootstrap["result"]["event_count"], 2);
        assert_eq!(
            cached_bootstrap["result"]["events"][0]["event"],
            "status"
        );
        assert_eq!(
            cached_bootstrap["result"]["events"][1]["event"],
            "autonomy_changed"
        );

        let status = control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(status["t"], "response");
        assert_eq!(status["ok"], true);
        assert_eq!(status["result"]["session_id"], "session-1");
        assert_eq!(status["result"]["created_unix_ms"], 123);
        assert_eq!(status["result"]["transport"], "webrtc-datachannel");
        assert_eq!(status["result"]["events_subscribed"], false);
        assert_eq!(status["result"]["response_credit_enabled"], false);
        assert_eq!(status["result"]["api_peers_available"], false);
        assert_eq!(status["result"]["api_agent_card_available"], true);
        assert_eq!(
            status["result"]["api_cached_bootstrap_events_available"],
            true
        );
        assert_eq!(
            status["result"]["api_browser_workspace_snapshot_available"],
            true
        );
        assert_eq!(status["result"]["api_state_snapshot_available"], true);
        assert_eq!(status["result"]["api_session_log_replay_available"], true);
        assert_eq!(status["result"]["api_dashboard_bootstrap_available"], true);
        assert_eq!(status["result"]["api_sessions_available"], true);
        assert_eq!(status["result"]["api_sessions_stream_available"], true);
        assert_eq!(status["result"]["api_session_detail_available"], true);
        assert_eq!(status["result"]["api_session_delete_available"], true);
        assert_eq!(
            status["result"]["api_session_current_agent_output_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_history_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_rollback_available"],
            true
        );
        assert_eq!(status["result"]["api_session_current_redo_available"], true);
        assert_eq!(
            status["result"]["api_session_current_prune_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_changes_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_context_snapshot_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_delete_available"],
            true
        );
        assert_eq!(status["result"]["api_fs_stat_available"], true);
        assert_eq!(status["result"]["api_fs_list_available"], true);
        assert_eq!(status["result"]["api_fs_mkdir_available"], true);
        assert_eq!(status["result"]["api_sessions_search_available"], true);
        assert_eq!(status["result"]["api_settings_available"], true);
        assert_eq!(status["result"]["api_settings_save_available"], false);
        assert_eq!(status["result"]["api_control_msg_available"], true);
        assert_eq!(status["result"]["api_key_status_available"], true);
        assert_eq!(status["result"]["api_api_keys_save_available"], true);
        assert_eq!(status["result"]["api_voice_session_available"], true);
        assert_eq!(status["result"]["api_project_root_available"], true);
        assert_eq!(status["result"]["api_displays_available"], true);
        assert_eq!(status["result"]["api_recordings_available"], true);
        assert_eq!(status["result"]["api_session_recordings_available"], true);
        assert_eq!(status["result"]["api_worktrees_available"], true);
        assert_eq!(status["result"]["api_worktrees_scan_available"], true);
        assert_eq!(status["result"]["api_worktrees_remove_available"], true);
        assert_eq!(status["result"]["api_mcp_tool_call_available"], false);
        assert_eq!(status["result"]["api_peer_mutations_available"], false);
        assert_eq!(status["result"]["api_peer_pairing_available"], true);
        assert_eq!(status["result"]["api_coordinator_available"], false);

        let peers = control_frame_response(
            r#"{"t":"request","id":"a1","method":"api_peers"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
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
            &mut outbound,
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
            &mut outbound,
        );
        assert!(project_root.is_none());
        assert!(pending.contains_key("pr1"));
        let project_root = rx.recv().await.unwrap();
        assert!(pending.remove(&project_root.id).is_some());
        assert_eq!(project_root.id, "pr1");
        assert!(project_root.done);
        let project_root = project_root.frame;
        assert_eq!(project_root["t"], "response");
        assert_eq!(project_root["ok"], true);
        assert!(project_root["result"].get("project_root").is_some());

        let queued = control_frame_response(
            r#"{"t":"request","id":"q1","method":"api_sessions","params":{"limit":1}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("q1"));
        let cancelled = control_frame_response(
            r#"{"t":"cancel","id":"q1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(cancelled["t"], "response");
        assert_eq!(cancelled["ok"], false);
        assert_eq!(cancelled["cancelled"], true);
        assert!(pending.get("q1").is_none());
    }

    #[tokio::test]
    async fn api_voice_session_preserves_endpoint_error_metadata() {
        let mut rt = runtime();
        rt.config = serde_json::json!({
            "provider": "unsupported-voice-provider",
            "model": "unused",
        });
        let response = api_voice_session_response("voice1".to_string(), &rt).await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["id"], "voice1");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["_httpStatus"], 502);
        assert_eq!(response["result"]["_httpOk"], false);
        assert_eq!(
            response["result"]["error"],
            "Unknown provider: unsupported-voice-provider"
        );
    }

    #[tokio::test]
    async fn api_mcp_tool_call_reports_unavailable_server_as_http_error() {
        let rt = runtime();
        let response = api_mcp_tool_call_response(
            "mcp1".to_string(),
            Some(&serde_json::json!({
                "mcp_id": 7,
                "session_id": "session-1",
                "name": "get_status",
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["id"], "mcp1");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["_httpStatus"], 503);
        assert_eq!(response["result"]["_httpOk"], false);
        assert_eq!(response["result"]["id"], 7);
        assert_eq!(response["result"]["error"]["code"], -32603);
        assert_eq!(
            response["result"]["error"]["message"],
            "MCP server not available"
        );
    }

    #[tokio::test]
    async fn api_control_msg_dispatches_allowlisted_settings_actions_only() {
        let rt = runtime();
        let mut events = rt.bus.subscribe();
        let response = api_control_msg_response(
            "ctrl1".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_codex_sandbox",
                    "mode": "workspace-write",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["action"], "set_codex_sandbox");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::SetCodexSandbox { mode }) = event {
                assert_eq!(mode, "workspace-write");
                saw_control = true;
                break;
            }
        }
        assert!(saw_control, "allowed control message did not reach the bus");

        let rejected = api_control_msg_response(
            "ctrl2".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "create_session",
                    "task": "do something",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(rejected["t"], "response");
        assert_eq!(rejected["ok"], true);
        assert_eq!(rejected["result"]["ok"], false);
        assert_eq!(rejected["result"]["_httpStatus"], 400);
        assert!(
            rejected["result"]["error"]
                .as_str()
                .unwrap_or("")
                .contains("not available over dashboard WebRTC")
        );
    }

    #[tokio::test]
    async fn current_agent_output_without_active_log_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = control_frame_response(
            r#"{"t":"request","id":"out1","method":"api_session_current_agent_output","params":{"ids":["missing-output"]}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("out1"));

        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "out1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["error"], "no active session log");
        assert_eq!(response.frame["result"]["_httpStatus"], 404);
        assert_eq!(response.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn session_delete_rpc_preserves_body_shape() {
        let invalid_session = api_session_delete_response(
            "del1".to_string(),
            Some(&serde_json::json!({ "session_id": "../bad" })),
        )
        .await;
        assert_eq!(invalid_session["t"], "response");
        assert_eq!(invalid_session["ok"], true);
        assert_eq!(invalid_session["result"]["ok"], false);
        assert_eq!(invalid_session["result"]["error"], "invalid session id");
    }

    #[tokio::test]
    async fn current_history_without_file_watcher_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        for (idx, (method, params)) in [
            ("api_session_current_history", serde_json::json!({})),
            (
                "api_session_current_rollback",
                serde_json::json!({
                    "round_id": 1,
                    "revert_files": true,
                    "revert_conversation": false,
                }),
            ),
            ("api_session_current_redo", serde_json::json!({})),
            ("api_session_current_prune", serde_json::json!({})),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("hist{idx}");
            let frame = serde_json::json!({
                "t": "request",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string();
            let queued = control_frame_response(&frame, &mut rt, &tx, &mut pending, &mut outbound);
            assert!(queued.is_none());
            assert!(pending.contains_key(&id));

            let response = rx.recv().await.unwrap();
            assert!(pending.remove(&response.id).is_some());
            assert_eq!(response.id, id);
            assert!(response.done);
            assert_eq!(response.frame["t"], "response");
            assert_eq!(response.frame["ok"], true);
            assert_eq!(response.frame["result"]["error"], "file watcher not active");
            assert_eq!(response.frame["result"]["_httpStatus"], 503);
            assert_eq!(response.frame["result"]["_httpOk"], false);
        }
    }

    #[tokio::test]
    async fn current_changes_without_file_watcher_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = control_frame_response(
            r#"{"t":"request","id":"chg1","method":"api_session_current_changes","params":{}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("chg1"));

        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "chg1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["error"], "file watcher not active");
        assert_eq!(response.frame["result"]["_httpStatus"], 503);
        assert_eq!(response.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn context_snapshot_rpc_preserves_http_status() {
        let invalid_session = api_session_context_snapshot_response(
            "ctx1".to_string(),
            Some(&serde_json::json!({
                "session_id": "../bad",
                "file": "snapshot.json",
            })),
        )
        .await;
        assert_eq!(invalid_session["t"], "response");
        assert_eq!(invalid_session["ok"], true);
        assert_eq!(invalid_session["result"]["error"], "invalid session id");
        assert_eq!(invalid_session["result"]["_httpStatus"], 400);
        assert_eq!(invalid_session["result"]["_httpOk"], false);

        let missing_selector = api_session_context_snapshot_response(
            "ctx2".to_string(),
            Some(&serde_json::json!({
                "session_id": "missing-session",
            })),
        )
        .await;
        assert_eq!(missing_selector["result"]["error"], "missing snapshot selector");
        assert_eq!(missing_selector["result"]["_httpStatus"], 400);
        assert_eq!(missing_selector["result"]["_httpOk"], false);

        let invalid_index = api_session_context_snapshot_response(
            "ctx3".to_string(),
            Some(&serde_json::json!({
                "session_id": "missing-session",
                "request_index": "abc",
            })),
        )
        .await;
        assert_eq!(invalid_index["result"]["error"], "invalid request_index");
        assert_eq!(invalid_index["result"]["_httpStatus"], 400);
        assert_eq!(invalid_index["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn current_upload_delete_preserves_http_status() {
        let rt_no_root = runtime();
        let no_root = api_session_current_upload_delete_response(
            "upl1".to_string(),
            Some(&serde_json::json!({ "id": "missing-upload" })),
            &rt_no_root,
        )
        .await;
        assert_eq!(no_root["t"], "response");
        assert_eq!(no_root["ok"], true);
        assert_eq!(no_root["result"]["error"], "no project root");
        assert_eq!(no_root["result"]["_httpStatus"], 404);
        assert_eq!(no_root["result"]["_httpOk"], false);

        let dir = tempfile::tempdir().unwrap();
        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.project_root_for_changes = Some(dir.path().to_path_buf());
        }
        let missing_id = api_session_current_upload_delete_response(
            "upl2".to_string(),
            Some(&serde_json::json!({})),
            &rt,
        )
        .await;
        assert_eq!(missing_id["result"]["error"], "missing upload id");
        assert_eq!(missing_id["result"]["_httpStatus"], 400);
        assert_eq!(missing_id["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn recording_rpcs_preserve_shapes_and_status() {
        let rt = runtime();

        let recordings = api_recordings_response("rec1".to_string(), &rt).await;
        assert_eq!(recordings["t"], "response");
        assert_eq!(recordings["ok"], true);
        assert!(recordings["result"].as_array().is_some());

        let invalid_session = api_session_recordings_response(
            "rec2".to_string(),
            Some(&serde_json::json!({ "session_id": "../bad" })),
        )
        .await;
        assert_eq!(invalid_session["t"], "response");
        assert_eq!(invalid_session["ok"], true);
        assert_eq!(invalid_session["result"]["error"], "invalid session id");
        assert_eq!(invalid_session["result"]["_httpStatus"], 400);
        assert_eq!(invalid_session["result"]["_httpOk"], false);

        let workspace_snapshot =
            api_browser_workspace_snapshot_response("bw1".to_string()).await;
        assert_eq!(workspace_snapshot["t"], "response");
        assert_eq!(workspace_snapshot["ok"], true);
        assert_eq!(
            workspace_snapshot["result"]["t"],
            "browser_workspace_snapshot"
        );
        assert!(workspace_snapshot["result"]["workspaces"].as_array().is_some());
    }

    #[tokio::test]
    async fn state_snapshot_rpc_returns_bootstrap_message_shape() {
        let rt = runtime();
        let snapshot = api_state_snapshot_response("snap1".to_string(), &rt).await;
        assert_eq!(snapshot["t"], "response");
        assert_eq!(snapshot["id"], "snap1");
        assert_eq!(snapshot["ok"], true);
        assert_eq!(snapshot["result"]["t"], "state_snapshot");
        assert_eq!(snapshot["result"]["connection_id"], "session-1");
        assert_eq!(snapshot["result"]["config"]["provider"], "openai");
        assert_eq!(snapshot["result"]["session_id"], "");
        assert!(snapshot["result"]["state"].is_object());
    }

    #[tokio::test]
    async fn session_log_replay_rpc_returns_empty_replay_without_active_log() {
        let rt = runtime();
        let replay = api_session_log_replay_response("replay1".to_string(), &rt).await;
        assert_eq!(replay["t"], "response");
        assert_eq!(replay["id"], "replay1");
        assert_eq!(replay["ok"], true);
        assert_eq!(replay["result"]["t"], "log_replay");
        assert_eq!(replay["result"]["available"], false);
        assert_eq!(replay["result"]["entries"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dashboard_bootstrap_rpc_returns_ordered_bootstrap_frames() {
        let rt = runtime();
        let bootstrap = api_dashboard_bootstrap_response("boot1".to_string(), &rt).await;
        assert_eq!(bootstrap["t"], "response");
        assert_eq!(bootstrap["id"], "boot1");
        assert_eq!(bootstrap["ok"], true);
        let frames = bootstrap["result"]["frames"].as_array().unwrap();
        assert_eq!(bootstrap["result"]["frame_count"], frames.len());
        assert_eq!(frames[0]["t"], "state_snapshot");
        assert_eq!(frames[1]["t"], "browser_workspace_snapshot");
        assert_eq!(frames[2]["t"], "log_replay");
        assert!(
            bootstrap["result"]["omitted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("display_ready"))
        );
    }

    #[tokio::test]
    async fn worktree_rpcs_preserve_cache_and_error_status() {
        let rt = runtime();
        {
            let mut cache = rt.worktree_inventory_cache.lock().unwrap();
            *cache = Some(
                serde_json::json!({
                    "worktrees": [{ "path": "/tmp/wt", "branch": "feature" }],
                    "summary": { "worktrees": 1 },
                })
                .to_string(),
            );
        }

        let cached = api_worktrees_response("wt1".to_string(), &rt).await;
        assert_eq!(cached["t"], "response");
        assert_eq!(cached["ok"], true);
        assert_eq!(cached["result"]["summary"]["worktrees"], 1);

        let invalid_remove =
            api_worktrees_remove_response("wt2".to_string(), Some(&serde_json::json!({})), &rt)
                .await;
        assert_eq!(invalid_remove["t"], "response");
        assert_eq!(invalid_remove["ok"], true);
        assert_eq!(invalid_remove["result"]["ok"], false);
        assert_eq!(invalid_remove["result"]["_httpStatus"], 400);
        assert_eq!(invalid_remove["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn fs_stat_and_list_preserve_http_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), b"hello").unwrap();

        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        for (idx, (method, path)) in [
            ("api_fs_stat", dir.path().to_string_lossy().to_string()),
            ("api_fs_list", dir.path().to_string_lossy().to_string()),
            ("api_fs_stat", "relative/path".to_string()),
            ("api_fs_mkdir", dir.path().to_string_lossy().to_string()),
            ("api_fs_mkdir", "relative/path".to_string()),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("fs{idx}");
            let is_list = method == "api_fs_list";
            let is_mkdir = method == "api_fs_mkdir";
            let is_bad_path = path == "relative/path";
            let frame = serde_json::json!({
                "t": "request",
                "id": id,
                "method": method,
                "params": { "path": path.clone() },
            })
            .to_string();
            let queued = control_frame_response(&frame, &mut rt, &tx, &mut pending, &mut outbound);
            assert!(queued.is_none());
            assert!(pending.contains_key(&id));

            let response = rx.recv().await.unwrap();
            assert!(pending.remove(&response.id).is_some());
            assert_eq!(response.id, id);
            assert!(response.done);
            assert_eq!(response.frame["t"], "response");
            assert_eq!(response.frame["ok"], true);

            if is_mkdir && is_bad_path {
                assert_eq!(response.frame["result"]["_httpStatus"], 400);
                assert_eq!(response.frame["result"]["_httpOk"], false);
                assert!(response.frame["result"]["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("path must be absolute"));
            } else if is_mkdir {
                assert_eq!(response.frame["result"]["ok"], true);
                assert_eq!(response.frame["result"]["already_exists"], true);
                assert_eq!(response.frame["result"]["_httpStatus"], 200);
                assert_eq!(response.frame["result"]["_httpOk"], true);
            } else if is_list {
                assert!(response.frame["result"]["entries"].is_array());
                assert_eq!(response.frame["result"]["_httpStatus"], 200);
                assert_eq!(response.frame["result"]["_httpOk"], true);
            } else if is_bad_path {
                assert_eq!(response.frame["result"]["_httpStatus"], 400);
                assert_eq!(response.frame["result"]["_httpOk"], false);
                assert!(response.frame["result"]["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("path must be absolute"));
            } else {
                assert_eq!(response.frame["result"]["exists"], true);
                assert_eq!(response.frame["result"]["is_dir"], true);
                assert_eq!(response.frame["result"]["_httpStatus"], 200);
                assert_eq!(response.frame["result"]["_httpOk"], true);
            }
        }
    }

    #[test]
    fn changes_rpc_params_build_request_lines() {
        let params = serde_json::json!({
            "path": "src/file name.rs",
            "query": "session_id=abc&source=codex",
        });
        assert_eq!(
            changes_request_line(Some(&params)),
            "GET /api/session/current/changes/src%2Ffile%20name.rs?session_id=abc&source=codex HTTP/1.1"
        );

        let params = serde_json::json!({
            "path": "/tmp/a+b c",
            "query": "?backend_session_id=thread%2F1#ignored",
        });
        assert_eq!(
            changes_request_line(Some(&params)),
            "GET /api/session/current/changes/%2Ftmp%2Fa%2Bb%20c?backend_session_id=thread%2F1 HTTP/1.1"
        );

        assert_eq!(
            changes_request_line(None),
            "GET /api/session/current/changes HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn control_frames_negotiate_and_apply_response_credit() {
        let mut rt = runtime();
        let (tx, _rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let hello = control_frame_response(
            r#"{"t":"hello","id":"h1","features":["response_credit"]}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert!(rt.response_credit_enabled);

        let status = control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(status["result"]["response_credit_enabled"], true);

        outbound.enqueue_chunked(
            "large".into(),
            "large:0".into(),
            "start".into(),
            vec!["chunk".into()],
            "end".into(),
        );
        if let Some(QueuedControlFrame::Chunked(queued)) = outbound.frames.front_mut() {
            queued.credit = 0;
        }
        assert!(control_frame_response(
            r#"{"t":"credit","id":"large","chunk_id":"large:0","chunks":3}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .is_none());
        let Some(QueuedControlFrame::Chunked(queued)) = outbound.frames.front() else {
            panic!("expected queued chunked frame");
        };
        assert_eq!(queued.credit, 3);

        let cancelled = control_frame_response(
            r#"{"t":"cancel","id":"large"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(cancelled["cancelled"], true);
        assert!(outbound.frames.is_empty());
    }

    #[tokio::test]
    async fn control_stream_json_lines_emit_lifecycle_frames() {
        let (line_tx, line_rx) = mpsc::channel::<String>(8);
        let stream_task = tokio::spawn(async move {
            for line in [
                r#"{"type":"start","limit":1,"quick_limit":1}"#,
                r#"{"type":"session","partial":true,"session":{"session_id":"s1"}}"#,
                r#"{"type":"done"}"#,
            ] {
                line_tx.send(format!("{line}\n")).await.unwrap();
            }
        });
        let (task_tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);

        stream_json_lines_response(
            "stream1".to_string(),
            "api_sessions_stream".to_string(),
            line_rx,
            stream_task,
            task_tx,
            CancellationToken::new(),
        )
        .await;

        let mut frames = Vec::new();
        while let Some(task) = rx.recv().await {
            frames.push(task);
            if frames.last().unwrap().done {
                break;
            }
        }

        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0].frame["t"], "stream_start");
        assert_eq!(frames[0].frame["method"], "api_sessions_stream");
        assert!(!frames[0].done);
        assert_eq!(frames[1].frame["t"], "stream_event");
        assert_eq!(frames[1].frame["seq"], 0);
        assert_eq!(frames[1].frame["event"]["type"], "start");
        assert_eq!(frames[2].frame["event"]["session"]["session_id"], "s1");
        assert_eq!(frames[3].frame["event"]["type"], "done");
        assert_eq!(frames[4].frame["t"], "stream_end");
        assert_eq!(frames[4].frame["ok"], true);
        assert_eq!(frames[4].frame["result"]["events"], 3);
        assert!(frames[4].done);
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
            sessions_stream_request_line(Some(&serde_json::json!({}))),
            "GET /api/sessions/stream HTTP/1.1"
        );
        assert_eq!(
            sessions_stream_request_line(Some(&serde_json::json!({"limit": "all"}))),
            "GET /api/sessions/stream?limit=all HTTP/1.1"
        );
        assert_eq!(
            sessions_stream_request_line(Some(&serde_json::json!({"limit": 25}))),
            "GET /api/sessions/stream?limit=25 HTTP/1.1"
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
    fn oversized_stream_event_frames_are_chunked_with_chunk_ids() {
        let frame = serde_json::json!({
            "t": "stream_event",
            "id": "stream-1",
            "seq": 7,
            "chunk_id": "stream-1:7",
            "event": {
                "type": "replace",
                "sessions": ["x".repeat(128)]
            }
        });
        let frames = chunk_control_response_frame(frame.clone(), 40, 24);
        assert!(frames.len() > 3, "expected stream event chunking");

        let start: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(start["t"], "response_start");
        assert_eq!(start["id"], "stream-1");
        assert_eq!(start["chunk_id"], "stream-1:7");

        let end: serde_json::Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(end["t"], "response_end");
        assert_eq!(end["id"], "stream-1");
        assert_eq!(end["chunk_id"], "stream-1:7");

        let mut bytes = Vec::new();
        for text in &frames[1..frames.len() - 1] {
            let chunk: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(chunk["t"], "response_chunk");
            assert_eq!(chunk["id"], "stream-1");
            assert_eq!(chunk["chunk_id"], "stream-1:7");
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(String::from_utf8(bytes).unwrap(), frame.to_string());
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
