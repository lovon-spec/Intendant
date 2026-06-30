//! Daemon-scoped WebRTC control tunnel for dashboard RPC experiments.
//!
//! The dashboard still uses HTTP plus the main WebSocket by default. This
//! module provides the first substrate for a future public-origin dashboard:
//! WebSocket signaling creates a direct browser-to-daemon WebRTC data channel,
//! then the channel carries small JSON RPC frames.

use crate::daemon_identity::{b64u, DaemonIdentity};
use crate::error::CallerError;
use crate::event::{AppEvent, ControlMsg};
use crate::types::LogLevel;
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{Read as _, Seek as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
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
const CONTROL_BYTE_STREAM_CHUNK_BYTES: usize = 16 * 1024;
const CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT: usize = 16;
const CONTROL_RESPONSE_MAX_CREDIT_GRANT: usize = 64;
const CONTROL_BINDING_TTL_MS: i64 = 5 * 60 * 1000;
const DASHBOARD_MEDIA_CLIP_MAX_FRAMES: usize = 1000;
static NEXT_DASHBOARD_DISPLAY_PEER_ID: AtomicU64 = AtomicU64::new(1);
const CONTROL_FEATURES: &[&str] = &[
    "ping",
    "config",
    "api_agent_card",
    "api_cached_bootstrap_events",
    "api_browser_workspace_snapshot",
    "api_state_snapshot",
    "api_display_bootstrap",
    "api_display_webrtc_signal",
    "api_display_input_authority_snapshot",
    "api_display_input_authority_request",
    "api_display_input_authority_release",
    "api_session_log_replay",
    "api_external_session_activity_replay",
    "api_dashboard_bootstrap",
    "status",
    "events",
    "response_chunks",
    "response_credit",
    "stream_frames",
    "byte_streams",
    "upload_frames",
    "terminal_frames",
    "tui_frames",
    "presence_frames",
    "presence_active_handoff",
    "presence_tool_request",
    "api_session_current_uploads",
    "api_session_current_upload_raw",
    "api_presence_video_frame",
    "api_media_annotation_attach",
    "api_media_annotation_submit",
    "api_media_clip_start",
    "api_media_clip_frame",
    "api_media_clip_end",
    "api_media_clip_cancel",
    "api_peers",
    "api_sessions",
    "api_sessions_stream",
    "api_session_detail",
    "api_session_report",
    "api_session_delete",
    "api_session_agent_output",
    "api_session_current_agent_output",
    "api_session_current_history",
    "api_session_current_rollback",
    "api_session_current_redo",
    "api_session_current_prune",
    "api_session_current_changes",
    "api_session_context_snapshot",
    "api_session_current_upload_delete",
    "api_transfer_jobs",
    "api_transfer_job_create",
    "api_transfer_job_delete",
    "api_transfer_download_read",
    "api_transfer_upload_chunk",
    "api_transfer_upload_commit",
    "api_fs_stat",
    "api_fs_list",
    "api_fs_mkdir",
    "api_fs_read",
    "api_sessions_search",
    "api_settings",
    "api_settings_save",
    "api_control_msg",
    "api_session_control_msg",
    "api_dashboard_action_msg",
    "api_diagnostics_visual_freshness",
    "api_key_status",
    "api_api_keys_save",
    "api_voice_session",
    "api_project_root",
    "api_displays",
    "api_recordings",
    "api_recording_asset",
    "api_session_recordings",
    "api_session_recording_asset",
    "api_session_frame_asset",
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
    "api_peer_webrtc_signal",
    "api_peer_file_transfer_signal",
    "api_peer_dashboard_control_signal",
    "api_peer_pairing_invite",
    "api_peer_pairing_join",
    "api_peer_pairing_request_access",
    "api_peer_pairing_request_access_poll",
    "api_peer_pairing_requests",
    "api_peer_pairing_request_decision",
    "api_peer_pairing_identities",
    "api_peer_pairing_identity_revoke",
    "api_access_overview",
    "api_dashboard_targets",
    "api_coordinator_route",
];
const UDP_BUF_LEN: usize = 2000;
const COMMAND_CHANNEL: usize = 16;
const TCP_OUT_QUEUE: usize = 256;
type TcpFrameSender = mpsc::Sender<Vec<u8>>;

pub struct DashboardControlRegistry {
    config: crate::web_gateway::WebGatewayConfig,
    broadcast_tx: tokio::sync::broadcast::Sender<String>,
    bus: crate::event::EventBus,
    peer_registry: Option<crate::peer::PeerRegistry>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
    worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
    terminal_registry: Arc<crate::terminal::TerminalRegistry>,
    web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
    task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    agent_card: serde_json::Value,
    bootstrap_caches: DashboardBootstrapCaches,
    display_authority: Option<DashboardDisplayAuthorityBridge>,
    presence: Option<DashboardPresenceBridge>,
    ice_config: crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    identity: Mutex<Option<Arc<DaemonIdentity>>>,
    peers: Mutex<HashMap<String, DashboardControlPeer>>,
}

#[derive(Clone, Debug)]
pub enum DashboardControlGrant {
    TrustedLocal,
    Peer {
        fingerprint: String,
        label: String,
        profile: String,
        filesystem: crate::peer::access_policy::FilesystemAccessPolicy,
    },
}

impl Default for DashboardControlGrant {
    fn default() -> Self {
        Self::TrustedLocal
    }
}

impl DashboardControlGrant {
    fn label(&self) -> &str {
        match self {
            Self::TrustedLocal => "trusted-local",
            Self::Peer { label, .. } => label.as_str(),
        }
    }

    fn profile(&self) -> Option<&str> {
        match self {
            Self::TrustedLocal => None,
            Self::Peer { profile, .. } => Some(profile.as_str()),
        }
    }

    fn filesystem(&self) -> Option<&crate::peer::access_policy::FilesystemAccessPolicy> {
        match self {
            Self::TrustedLocal => None,
            Self::Peer { filesystem, .. } => Some(filesystem),
        }
    }

    fn wire_kind(&self) -> &'static str {
        match self {
            Self::TrustedLocal => "trusted-local",
            Self::Peer { .. } => "peer",
        }
    }
}

#[derive(Clone, Default)]
pub struct DashboardBootstrapCaches {
    pub(crate) last_usage_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_live_usage_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_status_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_autonomy_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_external_agent_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_user_display_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) attached_external_sessions: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

type DashboardPresenceFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone)]
pub struct DashboardPresenceBridge {
    connect: Arc<dyn Fn(DashboardPresenceConnectRequest) -> DashboardPresenceFuture + Send + Sync>,
    disconnect:
        Arc<dyn Fn(DashboardPresenceDisconnectRequest) -> DashboardPresenceFuture + Send + Sync>,
    make_active:
        Arc<dyn Fn(DashboardPresenceMakeActiveRequest) -> DashboardPresenceFuture + Send + Sync>,
    cleanup: Arc<dyn Fn(String) -> DashboardPresenceFuture + Send + Sync>,
    record_voice_log: Arc<dyn Fn(String) + Send + Sync>,
}

#[derive(Clone)]
pub struct DashboardPresenceConnectRequest {
    pub session_id: String,
    pub control_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub server_session_id: Option<String>,
    pub last_event_seq: u64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub passive: bool,
}

#[derive(Clone)]
pub struct DashboardPresenceDisconnectRequest {
    pub session_id: String,
}

#[derive(Clone)]
pub struct DashboardPresenceMakeActiveRequest {
    pub session_id: String,
    pub control_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl DashboardPresenceBridge {
    pub fn new(
        connect: impl Fn(DashboardPresenceConnectRequest) -> DashboardPresenceFuture
            + Send
            + Sync
            + 'static,
        disconnect: impl Fn(DashboardPresenceDisconnectRequest) -> DashboardPresenceFuture
            + Send
            + Sync
            + 'static,
        make_active: impl Fn(DashboardPresenceMakeActiveRequest) -> DashboardPresenceFuture
            + Send
            + Sync
            + 'static,
        cleanup: impl Fn(String) -> DashboardPresenceFuture + Send + Sync + 'static,
        record_voice_log: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self {
            connect: Arc::new(connect),
            disconnect: Arc::new(disconnect),
            make_active: Arc::new(make_active),
            cleanup: Arc::new(cleanup),
            record_voice_log: Arc::new(record_voice_log),
        }
    }

    async fn connect(&self, request: DashboardPresenceConnectRequest) {
        (self.connect)(request).await
    }

    async fn disconnect(&self, request: DashboardPresenceDisconnectRequest) {
        (self.disconnect)(request).await
    }

    async fn make_active(&self, request: DashboardPresenceMakeActiveRequest) {
        (self.make_active)(request).await
    }

    async fn cleanup(&self, session_id: String) {
        (self.cleanup)(session_id).await
    }

    fn record_voice_log(&self, text: String) {
        (self.record_voice_log)(text)
    }
}

#[derive(Clone)]
pub struct DashboardDisplayAuthorityBridge {
    snapshot: Arc<dyn Fn(&str, &[u32]) -> Vec<serde_json::Value> + Send + Sync>,
    state_frame: Arc<dyn Fn(&str, u32) -> Option<serde_json::Value> + Send + Sync>,
    request: Arc<dyn Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync>,
    release: Arc<dyn Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync>,
    input_authorized: Arc<dyn Fn(&str, u32) -> bool + Send + Sync>,
    cleanup: Arc<dyn Fn(&str) + Send + Sync>,
    subscribe: Arc<dyn Fn() -> tokio::sync::broadcast::Receiver<u32> + Send + Sync>,
}

impl DashboardDisplayAuthorityBridge {
    pub fn new(
        snapshot: impl Fn(&str, &[u32]) -> Vec<serde_json::Value> + Send + Sync + 'static,
        state_frame: impl Fn(&str, u32) -> Option<serde_json::Value> + Send + Sync + 'static,
        request: impl Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync + 'static,
        release: impl Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync + 'static,
        input_authorized: impl Fn(&str, u32) -> bool + Send + Sync + 'static,
        cleanup: impl Fn(&str) + Send + Sync + 'static,
        subscribe: impl Fn() -> tokio::sync::broadcast::Receiver<u32> + Send + Sync + 'static,
    ) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            state_frame: Arc::new(state_frame),
            request: Arc::new(request),
            release: Arc::new(release),
            input_authorized: Arc::new(input_authorized),
            cleanup: Arc::new(cleanup),
            subscribe: Arc::new(subscribe),
        }
    }

    fn snapshot(&self, session_id: &str, display_ids: &[u32]) -> Vec<serde_json::Value> {
        (self.snapshot)(session_id, display_ids)
    }

    fn state_frame(&self, session_id: &str, display_id: u32) -> Option<serde_json::Value> {
        (self.state_frame)(session_id, display_id)
    }

    fn request(&self, session_id: &str, display_id: u32) -> Vec<serde_json::Value> {
        (self.request)(session_id, display_id)
    }

    fn release(&self, session_id: &str, display_id: u32) -> Vec<serde_json::Value> {
        (self.release)(session_id, display_id)
    }

    fn input_authorized(&self, session_id: &str, display_id: u32) -> bool {
        (self.input_authorized)(session_id, display_id)
    }

    fn cleanup(&self, session_id: &str) {
        (self.cleanup)(session_id)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<u32> {
        (self.subscribe)()
    }
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
        terminal_registry: Arc<crate::terminal::TerminalRegistry>,
        web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
        task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
        agent_card: serde_json::Value,
        bootstrap_caches: DashboardBootstrapCaches,
        display_authority: Option<DashboardDisplayAuthorityBridge>,
        presence: Option<DashboardPresenceBridge>,
        ice_config: crate::display::IceConfig,
        tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
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
            terminal_registry,
            web_tui_tx,
            task_tx,
            agent_card,
            bootstrap_caches,
            display_authority,
            presence,
            ice_config,
            tcp_peer_registry,
            identity: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
        }
    }

    pub async fn answer_offer(
        &self,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
    ) -> Result<DashboardControlAnswer, String> {
        self.answer_offer_with_grant(
            offer_sdp,
            session_grant,
            client_nonce,
            DashboardControlGrant::TrustedLocal,
        )
        .await
    }

    pub async fn answer_offer_with_grant(
        &self,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        grant: DashboardControlGrant,
    ) -> Result<DashboardControlAnswer, String> {
        let session_id = uuid::Uuid::new_v4().to_string();
        self.answer_offer_with_session_id_and_grant(
            session_id,
            offer_sdp,
            session_grant,
            client_nonce,
            grant,
        )
        .await
    }

    pub async fn answer_offer_with_session_id_and_grant(
        &self,
        session_id: String,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        grant: DashboardControlGrant,
    ) -> Result<DashboardControlAnswer, String> {
        self.answer_offer_with_session_id_grant_and_tcp(
            session_id,
            offer_sdp,
            session_grant,
            client_nonce,
            grant,
            None,
        )
        .await
    }

    pub async fn answer_offer_with_session_id_grant_and_tcp(
        &self,
        session_id: String,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        grant: DashboardControlGrant,
        tcp_advertised_addr: Option<SocketAddr>,
    ) -> Result<DashboardControlAnswer, String> {
        let identity = self.identity().await?;
        let (peer, answer_sdp, binding) = DashboardControlPeer::answer_offer(
            session_id.clone(),
            offer_sdp,
            session_grant,
            client_nonce,
            &self.config,
            self.broadcast_tx.clone(),
            self.bus.clone(),
            self.peer_registry.clone(),
            self.mcp_server.clone(),
            self.shared_session.clone(),
            self.project_root.clone(),
            self.worktree_inventory_cache.clone(),
            self.terminal_registry.clone(),
            self.web_tui_tx.clone(),
            self.task_tx.clone(),
            self.agent_card.clone(),
            self.bootstrap_caches.clone(),
            self.display_authority.clone(),
            self.presence.clone(),
            self.ice_config.clone(),
            Arc::clone(&self.tcp_peer_registry),
            tcp_advertised_addr,
            identity,
            grant,
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
        if let Some(bridge) = &self.display_authority {
            bridge.cleanup(session_id);
        }
        if let Some(bridge) = &self.presence {
            bridge.cleanup(session_id.to_string()).await;
        }
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
    pub expires_unix_ms: i64,
    pub offer_sha256: String,
    pub answer_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_grant_sha256: Option<String>,
    pub signature: String,
}

impl DashboardControlBinding {
    pub fn new(
        identity: &DaemonIdentity,
        session_id: String,
        offer_sdp: &str,
        answer_sdp: &str,
        session_grant: Option<&str>,
        client_nonce: Option<&str>,
    ) -> Self {
        let daemon_public_key = identity.public_key_b64u();
        let created_unix_ms = chrono::Utc::now().timestamp_millis();
        let expires_unix_ms = created_unix_ms + CONTROL_BINDING_TTL_MS;
        let offer_sha256 = sha256_b64u(offer_sdp.as_bytes());
        let answer_sha256 = sha256_b64u(answer_sdp.as_bytes());
        let client_nonce = client_nonce
            .map(str::trim)
            .filter(|nonce| !nonce.is_empty())
            .map(str::to_string);
        let session_grant_sha256 = session_grant
            .map(str::trim)
            .filter(|grant| !grant.is_empty())
            .map(|grant| sha256_b64u(grant.as_bytes()));
        let mut binding = Self {
            protocol: CONTROL_SIGNATURE_CONTEXT,
            session_id,
            daemon_public_key,
            created_unix_ms,
            expires_unix_ms,
            offer_sha256,
            answer_sha256,
            client_nonce,
            session_grant_sha256,
            signature: String::new(),
        };
        let payload = binding.signing_payload();
        binding.signature = identity.sign_b64u(payload.as_bytes());
        binding
    }

    pub fn signing_payload(&self) -> String {
        let mut payload = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.protocol,
            self.session_id,
            self.daemon_public_key,
            self.created_unix_ms,
            self.expires_unix_ms,
            self.offer_sha256,
            self.answer_sha256,
        );
        if let Some(client_nonce) = &self.client_nonce {
            payload.push('\n');
            payload.push_str(client_nonce);
        }
        if let Some(session_grant_sha256) = &self.session_grant_sha256 {
            payload.push('\n');
            payload.push_str(session_grant_sha256);
        }
        payload
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
        session_grant: Option<String>,
        client_nonce: Option<String>,
        config: &crate::web_gateway::WebGatewayConfig,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        bus: crate::event::EventBus,
        peer_registry: Option<crate::peer::PeerRegistry>,
        mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
        worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
        terminal_registry: Arc<crate::terminal::TerminalRegistry>,
        web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
        task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
        agent_card: serde_json::Value,
        bootstrap_caches: DashboardBootstrapCaches,
        display_authority: Option<DashboardDisplayAuthorityBridge>,
        presence: Option<DashboardPresenceBridge>,
        ice_config: crate::display::IceConfig,
        tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
        tcp_advertised_addr: Option<SocketAddr>,
        identity: Arc<DaemonIdentity>,
        grant: DashboardControlGrant,
    ) -> Result<(Self, String, DashboardControlBinding), CallerError> {
        let local_ufrag = new_control_ice_fragment();
        let local_pwd = new_control_ice_password();
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_ice_credentials(local_ufrag.clone(), local_pwd);
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

        let mut peer_registration = None;
        let mut tcp_conn_rx = None;
        let mut tcp_advertised = None;
        if let Some(advertised) =
            tcp_advertised_addr.filter(|a| !a.ip().is_loopback() && !a.ip().is_unspecified())
        {
            let (registration, rx) = tcp_peer_registry.register(local_ufrag.clone());
            peer_registration = Some(registration);
            tcp_conn_rx = Some(rx);
            tcp_advertised = Some(advertised);
            let candidate = tcp_host_candidate_init(advertised);
            if let Err(e) = rtc.add_local_candidate(candidate) {
                eprintln!(
                    "[dashboard/control] failed to add TCP host candidate {advertised}: {e}"
                );
            } else {
                eprintln!(
                    "[dashboard/control] ICE-TCP enabled on {advertised} for ufrag {local_ufrag}"
                );
            }
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
        let binding = DashboardControlBinding::new(
            &identity,
            session_id.clone(),
            &offer_sdp,
            &answer_sdp,
            session_grant.as_deref(),
            client_nonce.as_deref(),
        );
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
            terminal_registry,
            web_tui_tx,
            task_tx,
            agent_card,
            bootstrap_caches,
            display_authority,
            presence,
            ice_config,
            tcp_peer_registry,
            media_clip_ops: Arc::new(Mutex::new(HashMap::new())),
            control_frames_tx: None,
            display_peer_id: NEXT_DASHBOARD_DISPLAY_PEER_ID.fetch_add(1, Ordering::Relaxed),
            grant,
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL);
        let shutdown = CancellationToken::new();
        tokio::spawn(control_driver(
            rtc,
            sockets,
            tcp_conn_rx,
            tcp_advertised,
            peer_registration,
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
    terminal_registry: Arc<crate::terminal::TerminalRegistry>,
    web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
    task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    agent_card: serde_json::Value,
    bootstrap_caches: DashboardBootstrapCaches,
    display_authority: Option<DashboardDisplayAuthorityBridge>,
    presence: Option<DashboardPresenceBridge>,
    ice_config: crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    media_clip_ops: Arc<Mutex<HashMap<String, DashboardMediaClipOperation>>>,
    control_frames_tx: Option<mpsc::UnboundedSender<serde_json::Value>>,
    display_peer_id: crate::display::PeerId,
    grant: DashboardControlGrant,
}

#[derive(Debug)]
struct DashboardMediaClipOperation {
    stream: String,
    note: String,
    inject: bool,
    in_secs: f64,
    out_secs: f64,
    fps: u32,
    expected_frames: usize,
    frames: Vec<(String, String)>,
}

enum ControlCommand {
    AddIceCandidate(String),
}

#[derive(Debug)]
struct InboundPacket {
    proto: TransportProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

struct ControlTaskResponse {
    id: String,
    frame: serde_json::Value,
    byte_stream: Option<ControlByteStream>,
    done: bool,
}

struct ControlByteStream {
    id: String,
    stream_id: String,
    content_type: String,
    filename: Option<String>,
    bytes: Vec<u8>,
    result: serde_json::Value,
}

struct InboundUploadState {
    method: String,
    params: serde_json::Value,
    tmp: tempfile::NamedTempFile,
    total_bytes: usize,
    expected_chunks: usize,
    next_seq: usize,
    received_bytes: usize,
}

struct DashboardTuiConnection {
    internal_id: String,
    forwarder: tokio::task::JoinHandle<()>,
}

struct OutboundControlQueue {
    frames: VecDeque<QueuedControlFrame>,
}

enum QueuedControlFrame {
    Immediate { request_id: String, text: String },
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
            let matches_chunk = chunk_id.map(|id| queued.chunk_id == id).unwrap_or(false);
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
    mut tcp_conn_rx: Option<mpsc::Receiver<crate::display::webrtc::AcceptedTcpConnection>>,
    tcp_advertised: Option<SocketAddr>,
    _tcp_registration: Option<crate::display::webrtc::PeerRegistration>,
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
                                proto: TransportProtocol::UDP,
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
    let mut tcp_senders: HashMap<SocketAddr, TcpFrameSender> = HashMap::new();
    let mut channels: HashMap<String, rtc::data_channel::RTCDataChannelId> = HashMap::new();
    let (task_tx, mut task_rx) = mpsc::channel::<ControlTaskResponse>(64);
    let mut pending_requests: HashMap<String, CancellationToken> = HashMap::new();
    let mut outbound_queue = OutboundControlQueue::new();
    let mut inbound_uploads: HashMap<String, InboundUploadState> = HashMap::new();
    let (terminal_events_tx, mut terminal_events_rx) =
        mpsc::unbounded_channel::<serde_json::Value>();
    runtime.control_frames_tx = Some(terminal_events_tx.clone());
    let mut terminal_forwarders: HashMap<(String, String), tokio::task::JoinHandle<()>> =
        HashMap::new();
    let mut tui_connections: HashMap<String, DashboardTuiConnection> = HashMap::new();
    let mut display_authority_rx = runtime
        .display_authority
        .as_ref()
        .map(DashboardDisplayAuthorityBridge::subscribe);

    loop {
        let timeout_at = match drain_control_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut tcp_senders,
            &mut channels,
            &mut runtime,
            &task_tx,
            &mut pending_requests,
            &mut outbound_queue,
            &mut inbound_uploads,
            &terminal_events_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
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
                        transport_protocol: pkt.proto,
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
            Some(accepted) = async {
                match tcp_conn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let crate::display::webrtc::AcceptedTcpConnection {
                    remote_addr,
                    local_addr: real_local,
                    first_frame,
                    stream,
                } = accepted;
                let Some(fake_local) = tcp_advertised else {
                    eprintln!(
                        "[dashboard/control] TCP connection from {remote_addr} but no advertised local configured, dropping"
                    );
                    continue;
                };
                eprintln!(
                    "[dashboard/control] ICE-TCP connection from {remote_addr} -> {real_local} (rtc sees {fake_local})"
                );
                let (read_half, write_half) = stream.into_split();
                let (tcp_out_tx, mut tcp_out_rx) = mpsc::channel::<Vec<u8>>(TCP_OUT_QUEUE);
                tcp_senders.insert(remote_addr, tcp_out_tx);

                let writer_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut write_half = write_half;
                    loop {
                        tokio::select! {
                            biased;
                            _ = writer_shutdown.cancelled() => break,
                            frame = tcp_out_rx.recv() => match frame {
                                Some(contents) => {
                                    if let Err(e) =
                                        crate::display::webrtc::write_rfc4571_frame(&mut write_half, &contents).await
                                    {
                                        eprintln!(
                                            "[dashboard/control] ICE-TCP writer for {remote_addr} failed, tearing down connection: {e}"
                                        );
                                        writer_shutdown.cancel();
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut write_half).await;
                });

                let reader_tx = inbound_tx.clone();
                let reader_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut read_half = read_half;
                    loop {
                        tokio::select! {
                            _ = reader_shutdown.cancelled() => break,
                            frame = crate::display::webrtc::read_rfc4571_frame(&mut read_half) => match frame {
                                Ok(bytes) => {
                                    let pkt = InboundPacket {
                                        proto: TransportProtocol::TCP,
                                        source: remote_addr,
                                        destination: fake_local,
                                        bytes,
                                        received_at: Instant::now(),
                                    };
                                    if reader_tx.send(pkt).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[dashboard/control] ICE-TCP reader for {remote_addr} exiting: {e}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                });

                let input = TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr: fake_local,
                        peer_addr: remote_addr,
                        transport_protocol: TransportProtocol::TCP,
                        ecn: None,
                    },
                    message: BytesMut::from(first_frame.as_slice()),
                };
                if let Err(e) = rtc.handle_read(input) {
                    eprintln!("[dashboard/control] handle_read(first TCP frame) failed: {e:?}");
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
                    let task_id = task_response.id.clone();
                    let done = task_response.done;
                    send_control_task_response(
                        &mut rtc,
                        &channels,
                        &mut outbound_queue,
                        runtime.response_credit_enabled,
                        task_response,
                    );
                    if done {
                        pending_requests.remove(&task_id);
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
            Some(frame) = terminal_events_rx.recv() => {
                send_control_text(&mut rtc, &channels, frame.to_string());
                let _ = rtc.handle_timeout(Instant::now());
            }
            authority = async {
                match display_authority_rx.as_mut() {
                    Some(rx) => Some(rx.recv().await),
                    None => std::future::pending::<Option<Result<u32, tokio::sync::broadcast::error::RecvError>>>().await,
                }
            }, if runtime.events_subscribed && display_authority_rx.is_some() => {
                match authority {
                    Some(Ok(display_id)) => {
                        send_display_authority_event(&mut rtc, &channels, &mut runtime, display_id);
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        let snapshots = display_authority_snapshot_frames(&runtime).await;
                        for frame in snapshots {
                            send_event_payload(&mut rtc, &channels, &mut runtime, frame);
                        }
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) | None => {
                        display_authority_rx = runtime
                            .display_authority
                            .as_ref()
                            .map(DashboardDisplayAuthorityBridge::subscribe);
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
    for (_, handle) in terminal_forwarders {
        handle.abort();
        let _ = handle.await;
    }
    close_dashboard_tui_connections(&runtime, &mut tui_connections).await;
    if let Some(bridge) = &runtime.presence {
        bridge.cleanup(runtime.session_id.clone()).await;
    }
    for handle in forwarder_handles {
        let _ = handle.await;
    }
}

fn send_event_payload<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    payload: serde_json::Value,
) {
    runtime.events_sent = runtime.events_sent.saturating_add(1);
    let frame = serde_json::json!({
        "t": "event",
        "seq": runtime.events_sent,
        "payload": payload,
    });
    send_control_text(rtc, channels, frame.to_string());
}

fn send_display_authority_event<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    display_id: u32,
) {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return;
    };
    if let Some(frame) = bridge.state_frame(&runtime.session_id, display_id) {
        send_event_payload(rtc, channels, runtime, frame);
    }
}

async fn drain_control_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_senders: &mut HashMap<SocketAddr, TcpFrameSender>,
    channels: &mut HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    outbound_queue: &mut OutboundControlQueue,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
    tui_connections: &mut HashMap<String, DashboardTuiConnection>,
) -> Result<Instant, ()> {
    while let Some(t) = rtc.poll_write() {
        if t.transport.transport_protocol == TransportProtocol::UDP {
            if t.transport.local_addr.is_ipv4() != t.transport.peer_addr.is_ipv4() {
                continue;
            }
            if t.transport.local_addr.ip().is_loopback() != t.transport.peer_addr.ip().is_loopback() {
                continue;
            }
        }
        match t.transport.transport_protocol {
            TransportProtocol::UDP => {
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
            TransportProtocol::TCP => {
                let Some(sender) = tcp_senders.get(&t.transport.peer_addr) else {
                    continue;
                };
                let contents = t.message.to_vec();
                match sender.try_send(contents) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tcp_senders.remove(&t.transport.peer_addr);
                    }
                }
            }
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
        if let Some(response) = control_frame_response(
            text,
            runtime,
            task_tx,
            pending_requests,
            outbound_queue,
            inbound_uploads,
            terminal_events_tx,
            terminal_forwarders,
            tui_connections,
        ) {
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

fn send_control_task_response<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
    response_credit_enabled: bool,
    response: ControlTaskResponse,
) {
    if let Some(byte_stream) = response.byte_stream {
        send_control_byte_stream(
            rtc,
            channels,
            outbound_queue,
            response_credit_enabled,
            byte_stream,
        );
    } else {
        send_control_frame(
            rtc,
            channels,
            outbound_queue,
            response_credit_enabled,
            response.frame,
        );
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

fn send_control_byte_stream<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
    response_credit_enabled: bool,
    byte_stream: ControlByteStream,
) {
    match byte_stream_frame_text_parts(byte_stream, CONTROL_BYTE_STREAM_CHUNK_BYTES) {
        ControlFrameTexts::Immediate(frames) => {
            for text in frames {
                send_control_text(rtc, channels, text);
            }
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
            start, chunks, end, ..
        } => {
            let mut frames = Vec::with_capacity(chunks.len() + 2);
            frames.push(start);
            frames.extend(chunks);
            frames.push(end);
            frames
        }
    }
}

fn byte_stream_frame_text_parts(
    byte_stream: ControlByteStream,
    chunk_bytes: usize,
) -> ControlFrameTexts {
    let request_id = byte_stream.id;
    let chunk_id = byte_stream.stream_id;
    if request_id.is_empty() || chunk_id.is_empty() || chunk_bytes == 0 {
        return ControlFrameTexts::Immediate(Vec::new());
    }

    let total_bytes = byte_stream.bytes.len();
    let chunk_count = total_bytes.div_ceil(chunk_bytes);
    let start = serde_json::json!({
        "t": "byte_stream_start",
        "id": request_id,
        "stream_id": chunk_id,
        "encoding": "base64",
        "content_type": byte_stream.content_type,
        "filename": byte_stream.filename,
        "total_bytes": total_bytes,
        "chunks": chunk_count,
    })
    .to_string();
    let mut chunks = Vec::with_capacity(chunk_count);
    for (seq, chunk) in byte_stream.bytes.chunks(chunk_bytes).enumerate() {
        chunks.push(
            serde_json::json!({
                "t": "byte_stream_chunk",
                "id": request_id,
                "stream_id": chunk_id,
                "seq": seq,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .to_string(),
        );
    }
    let end = serde_json::json!({
        "t": "byte_stream_end",
        "id": request_id,
        "stream_id": chunk_id,
        "ok": true,
        "chunks": chunk_count,
        "result": byte_stream.result,
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
fn byte_stream_frame_texts(byte_stream: ControlByteStream, chunk_bytes: usize) -> Vec<String> {
    match byte_stream_frame_text_parts(byte_stream, chunk_bytes) {
        ControlFrameTexts::Immediate(frames) => frames,
        ControlFrameTexts::Chunked {
            start, chunks, end, ..
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
            start, chunks, end, ..
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

fn dashboard_control_error_response(id: String, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "error": message.into(),
    })
}

fn runtime_allows_operation(
    runtime: &ControlRuntime,
    op: crate::peer::access_policy::PeerOperation,
) -> bool {
    match &runtime.grant {
        DashboardControlGrant::TrustedLocal => true,
        DashboardControlGrant::Peer { profile, .. } => {
            crate::peer::access_policy::profile_allows_operation(profile, op)
        }
    }
}

fn dashboard_control_frame_operation(t: &str) -> Option<crate::peer::access_policy::PeerOperation> {
    use crate::peer::access_policy::PeerOperation;
    match t {
        "display_input" => Some(PeerOperation::DisplayInput),
        "terminal_open" | "terminal_input" | "terminal_resize" | "terminal_close" => {
            Some(PeerOperation::Terminal)
        }
        "tui_subscribe" | "tui_key" | "tui_resize" | "tui_unsubscribe" | "tui_close" => {
            Some(PeerOperation::RuntimeControl)
        }
        "presence_frame" => Some(PeerOperation::Message),
        "upload_start" | "upload_chunk" | "upload_end" => Some(PeerOperation::FilesystemWrite),
        _ => None,
    }
}

fn dashboard_control_method_operation(
    method: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    use crate::peer::access_policy::PeerOperation;
    match method {
        "status" | "api_agent_card" => Some(PeerOperation::PresenceRead),
        "api_cached_bootstrap_events" | "subscribe_events" | "unsubscribe_events" => {
            Some(PeerOperation::SessionInspect)
        }
        "config" => Some(PeerOperation::RuntimeControl),
        "api_access_overview" | "api_dashboard_targets" => Some(PeerOperation::AccessInspect),
        "api_peer_pairing_requests" | "api_peer_pairing_identities" => {
            Some(PeerOperation::AccessInspect)
        }
        "api_peer_pairing_request_decision" | "api_peer_pairing_identity_revoke" => {
            Some(PeerOperation::AccessManage)
        }
        "api_peer_pairing_invite" => Some(PeerOperation::AccessManage),
        "api_peers" | "api_peer_eligible" => Some(PeerOperation::PeerInspect),
        "api_peer_add"
        | "api_peer_remove"
        | "api_peer_message"
        | "api_peer_task"
        | "api_peer_approval"
        | "api_peer_webrtc_signal"
        | "api_peer_file_transfer_signal"
        | "api_peer_dashboard_control_signal"
        | "api_peer_pairing_join"
        | "api_peer_pairing_request_access"
        | "api_peer_pairing_request_access_poll"
        | "api_coordinator_route" => Some(PeerOperation::PeerManage),
        "api_sessions"
        | "api_sessions_stream"
        | "api_session_detail"
        | "api_session_report"
        | "api_session_agent_output"
        | "api_sessions_search"
        | "api_session_recordings"
        | "api_session_recording_asset"
        | "api_session_frame_asset"
        | "api_worktrees" => Some(PeerOperation::SessionInspect),
        "api_session_delete"
        | "api_session_current_history"
        | "api_session_current_rollback"
        | "api_session_current_redo"
        | "api_session_current_prune"
        | "api_session_current_changes"
        | "api_session_current_uploads"
        | "api_session_current_upload_raw"
        | "api_session_current_upload_delete"
        | "api_session_current_agent_output"
        | "api_session_context_snapshot"
        | "api_session_control_msg"
        | "api_worktrees_scan"
        | "api_worktrees_remove" => Some(PeerOperation::SessionManage),
        "api_transfer_jobs" | "api_transfer_download_read" | "api_fs_stat" | "api_fs_list"
        | "api_fs_read" => Some(PeerOperation::FilesystemRead),
        "api_transfer_job_create"
        | "api_transfer_job_delete"
        | "api_transfer_upload_commit"
        | "api_fs_mkdir" => Some(PeerOperation::FilesystemWrite),
        "api_display_bootstrap" | "api_display_webrtc_signal" | "api_displays" => {
            Some(PeerOperation::DisplayView)
        }
        "api_display_input_authority_snapshot"
        | "api_display_input_authority_request"
        | "api_display_input_authority_release"
        | "api_diagnostics_visual_freshness" => Some(PeerOperation::DisplayInput),
        "api_control_msg" | "api_dashboard_action_msg" | "api_mcp_tool_call" => {
            Some(PeerOperation::Message)
        }
        "api_settings"
        | "api_settings_save"
        | "api_key_status"
        | "api_api_keys_save"
        | "api_project_root" => Some(PeerOperation::Settings),
        "api_voice_session"
        | "api_presence_video_frame"
        | "api_media_annotation_attach"
        | "api_media_annotation_submit"
        | "api_media_clip_start"
        | "api_media_clip_frame"
        | "api_media_clip_end"
        | "api_media_clip_cancel" => Some(PeerOperation::RuntimeControl),
        "api_recordings" | "api_recording_asset" => Some(PeerOperation::RuntimeControl),
        "api_browser_workspace_snapshot"
        | "api_state_snapshot"
        | "api_session_log_replay"
        | "api_external_session_activity_replay"
        | "api_dashboard_bootstrap"
        | "api_managed_context_records"
        | "api_managed_context_anchors"
        | "api_managed_context_fission" => Some(PeerOperation::SessionInspect),
        _ => None,
    }
}

fn dashboard_control_filesystem_path(params: Option<&serde_json::Value>) -> Option<String> {
    let params = params?;
    optional_string_param(params, &["path", "source_path", "sourcePath", "source"])
}

fn authorize_dashboard_control_filesystem(
    runtime: &ControlRuntime,
    op: crate::peer::access_policy::PeerOperation,
    params: Option<&serde_json::Value>,
) -> Result<(), String> {
    use crate::peer::access_policy::{FilesystemAccessKind, PeerOperation};
    let kind = match op {
        PeerOperation::FilesystemRead => FilesystemAccessKind::Read,
        PeerOperation::FilesystemWrite => FilesystemAccessKind::Write,
        _ => return Ok(()),
    };
    let Some(policy) = runtime.grant.filesystem() else {
        return Ok(());
    };
    let raw_path = dashboard_control_filesystem_path(params)
        .ok_or_else(|| "filesystem request missing path".to_string())?;
    let path = crate::web_gateway::expand_dashboard_fs_path(&raw_path)?;
    crate::peer::access_policy::filesystem_access_allowed(policy, kind, &path)
}

fn authorize_dashboard_control_method(
    runtime: &ControlRuntime,
    method: &str,
    params: Option<&serde_json::Value>,
) -> Result<(), String> {
    let Some(op) = dashboard_control_method_operation(method) else {
        return Ok(());
    };
    if !runtime_allows_operation(runtime, op) {
        let profile = runtime.grant.profile().unwrap_or("trusted-local");
        return Err(format!(
            "dashboard-control method {method} is not allowed for profile {profile}"
        ));
    }
    authorize_dashboard_control_filesystem(runtime, op, params)
}

fn authorize_dashboard_control_frame(
    runtime: &ControlRuntime,
    frame_type: &str,
) -> Result<(), String> {
    let Some(op) = dashboard_control_frame_operation(frame_type) else {
        return Ok(());
    };
    if runtime_allows_operation(runtime, op) {
        Ok(())
    } else {
        let profile = runtime.grant.profile().unwrap_or("trusted-local");
        Err(format!(
            "dashboard-control frame {frame_type} is not allowed for profile {profile}"
        ))
    }
}

fn control_frame_response(
    text: &str,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    outbound_queue: &mut OutboundControlQueue,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
    tui_connections: &mut HashMap<String, DashboardTuiConnection>,
) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let t = parsed.get("t").and_then(|v| v.as_str()).unwrap_or("");
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !matches!(t, "hello" | "ping" | "request") {
        if let Err(error) = authorize_dashboard_control_frame(runtime, t) {
            return Some(dashboard_control_error_response(id, error));
        }
    }
    match t {
        "hello" => {
            runtime.response_credit_enabled = parsed
                .get("features")
                .and_then(|features| features.as_array())
                .map(|features| {
                    features.iter().any(|feature| {
                        matches!(feature.as_str(), Some("response_credit") | Some("credit"))
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
        "display_input" => {
            spawn_dashboard_display_input(parsed, runtime.clone());
            None
        }
        "terminal_open" => {
            control_terminal_open_frame(parsed, runtime, terminal_events_tx, terminal_forwarders)
        }
        "terminal_input" => control_terminal_input_frame(parsed, runtime),
        "terminal_resize" => control_terminal_resize_frame(parsed, runtime),
        "terminal_close" => control_terminal_close_frame(parsed, runtime, terminal_forwarders),
        "tui_subscribe" => {
            control_tui_subscribe_frame(parsed, runtime, terminal_events_tx, tui_connections)
        }
        "tui_key" => control_tui_key_frame(parsed, runtime, tui_connections),
        "tui_resize" => control_tui_resize_frame(parsed, runtime, tui_connections),
        "tui_unsubscribe" => control_tui_unsubscribe_frame(parsed, runtime, tui_connections),
        "tui_close" => control_tui_close_frame(parsed, runtime, tui_connections),
        "presence_frame" => control_presence_frame(parsed, runtime.clone()),
        "upload_start" => control_upload_start_frame(id, parsed, pending_requests, inbound_uploads),
        "upload_chunk" => control_upload_chunk_frame(id, parsed, pending_requests, inbound_uploads),
        "upload_end" => control_upload_end_frame(
            id,
            parsed,
            runtime,
            task_tx,
            pending_requests,
            inbound_uploads,
        ),
        "request" => {
            let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let params = parsed.get("params").cloned();
            if let Err(error) =
                authorize_dashboard_control_method(runtime, method, params.as_ref())
            {
                return Some(dashboard_control_error_response(id, error));
            }
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
                "api_dashboard_targets" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::dashboard_targets_response_value(
                        &runtime.agent_card,
                        runtime.peer_registry.as_ref(),
                    ),
                })),
                "api_access_overview" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::access_overview_response_value(
                        &runtime.agent_card,
                        runtime.peer_registry.as_ref(),
                    ),
                })),
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
                        params,
                        task_tx.clone(),
                        pending_requests,
                    );
                    None
                }
                "api_sessions"
                | "api_session_detail"
                | "api_session_report"
                | "api_session_delete"
                | "api_session_agent_output"
                | "api_session_current_agent_output"
                | "api_session_current_history"
                | "api_session_current_rollback"
                | "api_session_current_redo"
                | "api_session_current_prune"
                | "api_session_current_changes"
                | "api_session_context_snapshot"
                | "api_session_current_uploads"
                | "api_session_current_upload_raw"
                | "api_session_current_upload_delete"
                | "api_transfer_jobs"
                | "api_transfer_job_create"
                | "api_transfer_job_delete"
                | "api_transfer_download_read"
                | "api_transfer_upload_commit"
                | "api_media_clip_start"
                | "api_media_clip_end"
                | "api_media_clip_cancel"
                | "api_fs_stat"
                | "api_fs_list"
                | "api_fs_mkdir"
                | "api_fs_read"
                | "api_sessions_search"
                | "api_settings"
                | "api_settings_save"
                | "api_control_msg"
                | "api_session_control_msg"
                | "api_dashboard_action_msg"
                | "api_diagnostics_visual_freshness"
                | "api_key_status"
                | "api_api_keys_save"
                | "api_voice_session"
                | "api_project_root"
                | "api_displays"
                | "api_recordings"
                | "api_recording_asset"
                | "api_session_recordings"
                | "api_session_recording_asset"
                | "api_session_frame_asset"
                | "api_browser_workspace_snapshot"
                | "api_state_snapshot"
                | "api_display_bootstrap"
                | "api_display_webrtc_signal"
                | "api_display_input_authority_snapshot"
                | "api_display_input_authority_request"
                | "api_display_input_authority_release"
                | "api_session_log_replay"
                | "api_external_session_activity_replay"
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
                | "api_peer_webrtc_signal"
                | "api_peer_file_transfer_signal"
                | "api_peer_dashboard_control_signal"
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
                        params,
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
            let upload_existed = inbound_uploads.remove(&id).is_some();
            let existed = pending_existed || queued_existed || upload_existed;
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

fn control_upload_error_response(
    id: String,
    status: u16,
    error: impl Into<String>,
) -> serde_json::Value {
    http_body_response(
        id,
        status,
        serde_json::json!({
            "ok": false,
            "error": error.into(),
        })
        .to_string(),
        "dashboard upload",
    )
}

fn control_upload_start_frame(
    id: String,
    frame: serde_json::Value,
    pending_requests: &mut HashMap<String, CancellationToken>,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
) -> Option<serde_json::Value> {
    if id.is_empty() {
        return Some(control_upload_error_response(id, 400, "missing request id"));
    }
    let method = frame.get("method").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(
        method,
        "api_session_current_upload"
            | "api_transfer_upload_chunk"
            | "api_media_annotation_attach"
            | "api_media_annotation_submit"
            | "api_media_clip_frame"
    ) {
        return Some(control_upload_error_response(
            id,
            400,
            format!("unknown upload method: {method}"),
        ));
    }
    let total_bytes = frame
        .get("total_bytes")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let expected_chunks = frame
        .get("chunks")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    if total_bytes > crate::web_gateway::UPLOAD_MAX_BYTES {
        return Some(control_upload_error_response(
            id,
            413,
            format!(
                "body too large: {} bytes (cap is {})",
                total_bytes,
                crate::web_gateway::UPLOAD_MAX_BYTES
            ),
        ));
    }
    if total_bytes > 0 && expected_chunks == 0 {
        return Some(control_upload_error_response(
            id,
            400,
            "missing upload chunks",
        ));
    }
    if total_bytes == 0 && expected_chunks != 0 {
        return Some(control_upload_error_response(
            id,
            400,
            "empty upload declared chunks",
        ));
    }
    let tmp = match tempfile::NamedTempFile::new() {
        Ok(tmp) => tmp,
        Err(e) => {
            return Some(control_upload_error_response(
                id,
                500,
                format!("create tempfile: {e}"),
            ));
        }
    };
    if let Some(previous) = pending_requests.remove(&id) {
        previous.cancel();
    }
    inbound_uploads.remove(&id);
    pending_requests.insert(id.clone(), CancellationToken::new());
    inbound_uploads.insert(
        id,
        InboundUploadState {
            method: method.to_string(),
            params: frame
                .get("params")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
            tmp,
            total_bytes,
            expected_chunks,
            next_seq: 0,
            received_bytes: 0,
        },
    );
    None
}

fn control_upload_chunk_frame(
    id: String,
    frame: serde_json::Value,
    pending_requests: &mut HashMap<String, CancellationToken>,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
) -> Option<serde_json::Value> {
    let Some(upload) = inbound_uploads.get_mut(&id) else {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(id, 400, "unknown upload id"));
    };
    let seq = frame
        .get("seq")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if seq != upload.next_seq {
        inbound_uploads.remove(&id);
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            400,
            "upload chunk sequence mismatch",
        ));
    }
    let data = match frame.get("data").and_then(|value| value.as_str()) {
        Some(data) => data,
        None => {
            inbound_uploads.remove(&id);
            pending_requests.remove(&id);
            return Some(control_upload_error_response(
                id,
                400,
                "missing upload chunk data",
            ));
        }
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => bytes,
        Err(_) => {
            inbound_uploads.remove(&id);
            pending_requests.remove(&id);
            return Some(control_upload_error_response(
                id,
                400,
                "invalid upload chunk data",
            ));
        }
    };
    upload.received_bytes = upload.received_bytes.saturating_add(bytes.len());
    if upload.received_bytes > upload.total_bytes {
        inbound_uploads.remove(&id);
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            400,
            "upload exceeded declared size",
        ));
    }
    if let Err(e) = upload.tmp.as_file_mut().write_all(&bytes) {
        inbound_uploads.remove(&id);
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            500,
            format!("write upload tempfile: {e}"),
        ));
    }
    upload.next_seq = upload.next_seq.saturating_add(1);
    None
}

fn control_upload_end_frame(
    id: String,
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
) -> Option<serde_json::Value> {
    let Some(mut upload) = inbound_uploads.remove(&id) else {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(id, 400, "unknown upload id"));
    };
    let final_chunks = frame
        .get("chunks")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if final_chunks != upload.expected_chunks
        || upload.next_seq != upload.expected_chunks
        || upload.received_bytes != upload.total_bytes
    {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(id, 400, "incomplete upload"));
    }
    if let Err(e) = upload.tmp.as_file_mut().flush() {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            500,
            format!("flush upload tempfile: {e}"),
        ));
    }
    let runtime = runtime.clone();
    let task_tx = task_tx.clone();
    tokio::spawn(async move {
        let response = match upload.method.as_str() {
            "api_session_current_upload" => {
                api_session_current_upload_task_response(id.clone(), upload, runtime).await
            }
            "api_transfer_upload_chunk" => {
                api_transfer_upload_chunk_task_response(id.clone(), upload, runtime).await
            }
            "api_media_annotation_attach" => {
                api_media_annotation_upload_task_response(id.clone(), upload, runtime, false).await
            }
            "api_media_annotation_submit" => {
                api_media_annotation_upload_task_response(id.clone(), upload, runtime, true).await
            }
            "api_media_clip_frame" => {
                api_media_clip_frame_upload_task_response(id.clone(), upload, runtime).await
            }
            "api_presence_video_frame" => {
                api_presence_video_frame_upload_task_response(id.clone(), upload, runtime).await
            }
            method => ControlTaskResponse {
                id: id.clone(),
                frame: control_upload_error_response(
                    id.clone(),
                    400,
                    format!("unknown upload method: {method}"),
                ),
                byte_stream: None,
                done: true,
            },
        };
        let _ = task_tx.send(response).await;
    });
    None
}

fn terminal_frame_key(frame: &serde_json::Value) -> (String, String) {
    let host_id = frame
        .get("host_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("local")
        .to_string();
    let terminal_id = frame
        .get("terminal_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("shell-0")
        .to_string();
    (host_id, terminal_id)
}

fn terminal_frame_dimension(frame: &serde_json::Value, key: &str, default: u16) -> u16 {
    frame
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn control_terminal_open_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let cols = terminal_frame_dimension(&frame, "cols", 80);
    let rows = terminal_frame_dimension(&frame, "rows", 24);
    let forwarder_key = (host_id.clone(), terminal_id.clone());
    if let Some(handle) = terminal_forwarders.remove(&forwarder_key) {
        handle.abort();
    }
    let registry = runtime.terminal_registry.clone();
    let terminal_events_tx = terminal_events_tx.clone();
    let handle = tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id: host_id.clone(),
            terminal_id: terminal_id.clone(),
        };
        match registry.open_or_attach(key, cols, rows).await {
            Ok(session) => {
                let (tx, mut rx) = mpsc::unbounded_channel();
                session.attach(tx);
                let _ = terminal_events_tx.send(serde_json::json!({
                    "t": "terminal_opened",
                    "host_id": host_id.clone(),
                    "terminal_id": terminal_id.clone(),
                }));
                while let Some(event) = rx.recv().await {
                    let frame = match event {
                        crate::terminal::TerminalEvent::Output(bytes) => {
                            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            serde_json::json!({
                                "t": "terminal_output",
                                "host_id": host_id.clone(),
                                "terminal_id": terminal_id.clone(),
                                "data": data,
                            })
                        }
                        crate::terminal::TerminalEvent::Exited { status } => {
                            serde_json::json!({
                                "t": "terminal_exited",
                                "host_id": host_id.clone(),
                                "terminal_id": terminal_id.clone(),
                                "status": status,
                            })
                        }
                    };
                    if terminal_events_tx.send(frame).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = terminal_events_tx.send(serde_json::json!({
                    "t": "terminal_error",
                    "host_id": host_id,
                    "terminal_id": terminal_id,
                    "error": e,
                }));
            }
        }
    });
    terminal_forwarders.insert(forwarder_key, handle);
    None
}

fn control_terminal_input_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let data_b64 = frame
        .get("data")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
        return None;
    };
    let registry = runtime.terminal_registry.clone();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id,
            terminal_id,
        };
        if let Some(session) = registry.get(&key).await {
            session.write_input(&data);
        }
    });
    None
}

fn control_terminal_resize_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let cols = terminal_frame_dimension(&frame, "cols", 80);
    let rows = terminal_frame_dimension(&frame, "rows", 24);
    let registry = runtime.terminal_registry.clone();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id,
            terminal_id,
        };
        if let Some(session) = registry.get(&key).await {
            session.resize(cols, rows);
        }
    });
    None
}

fn control_terminal_close_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    if let Some(handle) = terminal_forwarders.remove(&(host_id.clone(), terminal_id.clone())) {
        handle.abort();
    }
    let registry = runtime.terminal_registry.clone();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id,
            terminal_id,
        };
        registry.close(&key).await;
    });
    None
}

fn tui_frame_connection_id(frame: &serde_json::Value) -> String {
    frame
        .get("connection_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(128).collect::<String>())
        .unwrap_or_else(|| "tui-0".to_string())
}

fn tui_internal_connection_id(runtime: &ControlRuntime, connection_id: &str) -> String {
    format!("dashboard-control:{}:{}", runtime.session_id, connection_id)
}

fn tui_send_error(
    events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    connection_id: String,
    error: impl Into<String>,
) {
    let _ = events_tx.send(serde_json::json!({
        "t": "tui_error",
        "connection_id": connection_id,
        "error": error.into(),
    }));
}

fn dashboard_tui_output_frame(connection_id: &str, message: &str) -> Option<serde_json::Value> {
    let mut value = serde_json::from_str::<serde_json::Value>(message).ok()?;
    let serde_json::Value::Object(ref mut object) = value else {
        return None;
    };
    if object.get("t").and_then(|value| value.as_str()) != Some("term") {
        return None;
    }
    object.insert("t".to_string(), serde_json::json!("tui_term"));
    object.insert(
        "connection_id".to_string(),
        serde_json::json!(connection_id),
    );
    if let Some(data) = object.get("d").cloned() {
        object.insert("base64".to_string(), data);
    }
    Some(value)
}

fn control_tui_subscribe_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    tui_connections: &mut HashMap<String, DashboardTuiConnection>,
) -> Option<serde_json::Value> {
    let connection_id = tui_frame_connection_id(&frame);
    let cols = terminal_frame_dimension(&frame, "cols", 80);
    let rows = terminal_frame_dimension(&frame, "rows", 24);
    let Some(web_tui_tx) = runtime.web_tui_tx.as_ref() else {
        tui_send_error(
            terminal_events_tx,
            connection_id,
            "web tui renderer is not available",
        );
        return None;
    };

    if !tui_connections.contains_key(&connection_id) {
        let internal_id = tui_internal_connection_id(runtime, &connection_id);
        let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<String>();
        let outbound_tx = terminal_events_tx.clone();
        let outbound_connection_id = connection_id.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(message) = direct_rx.recv().await {
                let Some(frame) = dashboard_tui_output_frame(&outbound_connection_id, &message)
                else {
                    continue;
                };
                if outbound_tx.send(frame).is_err() {
                    break;
                }
            }
        });
        if web_tui_tx
            .send(crate::tui::web::WebTuiCommand::AddConnection {
                id: internal_id.clone(),
                direct_tx,
                cols,
                rows,
            })
            .is_err()
        {
            forwarder.abort();
            tui_send_error(
                terminal_events_tx,
                connection_id,
                "web tui command loop is closed",
            );
            return None;
        }
        tui_connections.insert(
            connection_id.clone(),
            DashboardTuiConnection {
                internal_id,
                forwarder,
            },
        );
    }

    if let Some(conn) = tui_connections.get(&connection_id) {
        let _ = web_tui_tx.send(crate::tui::web::WebTuiCommand::Resize {
            id: conn.internal_id.clone(),
            cols,
            rows,
        });
        let _ = web_tui_tx.send(crate::tui::web::WebTuiCommand::Subscribe {
            id: conn.internal_id.clone(),
        });
    }
    None
}

fn control_tui_key_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    tui_connections: &HashMap<String, DashboardTuiConnection>,
) -> Option<serde_json::Value> {
    let connection_id = tui_frame_connection_id(&frame);
    let Some(conn) = tui_connections.get(&connection_id) else {
        return None;
    };
    let Some(key) = crate::tui::web::parse_web_key(&frame) else {
        return None;
    };
    if let Some(web_tui_tx) = runtime.web_tui_tx.as_ref() {
        let _ = web_tui_tx.send(crate::tui::web::WebTuiCommand::Key {
            id: conn.internal_id.clone(),
            key,
        });
    }
    None
}

fn control_tui_resize_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    tui_connections: &HashMap<String, DashboardTuiConnection>,
) -> Option<serde_json::Value> {
    let connection_id = tui_frame_connection_id(&frame);
    let Some(conn) = tui_connections.get(&connection_id) else {
        return None;
    };
    let cols = terminal_frame_dimension(&frame, "cols", 80);
    let rows = terminal_frame_dimension(&frame, "rows", 24);
    if let Some(web_tui_tx) = runtime.web_tui_tx.as_ref() {
        let _ = web_tui_tx.send(crate::tui::web::WebTuiCommand::Resize {
            id: conn.internal_id.clone(),
            cols,
            rows,
        });
    }
    None
}

fn control_tui_unsubscribe_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    tui_connections: &HashMap<String, DashboardTuiConnection>,
) -> Option<serde_json::Value> {
    let connection_id = tui_frame_connection_id(&frame);
    let Some(conn) = tui_connections.get(&connection_id) else {
        return None;
    };
    if let Some(web_tui_tx) = runtime.web_tui_tx.as_ref() {
        let _ = web_tui_tx.send(crate::tui::web::WebTuiCommand::Unsubscribe {
            id: conn.internal_id.clone(),
        });
    }
    None
}

fn control_tui_close_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    tui_connections: &mut HashMap<String, DashboardTuiConnection>,
) -> Option<serde_json::Value> {
    let connection_id = tui_frame_connection_id(&frame);
    let Some(conn) = tui_connections.remove(&connection_id) else {
        return None;
    };
    let DashboardTuiConnection {
        internal_id,
        forwarder,
    } = conn;
    if let Some(web_tui_tx) = runtime.web_tui_tx.as_ref() {
        let _ =
            web_tui_tx.send(crate::tui::web::WebTuiCommand::RemoveConnection { id: internal_id });
    }
    forwarder.abort();
    None
}

async fn close_dashboard_tui_connections(
    runtime: &ControlRuntime,
    tui_connections: &mut HashMap<String, DashboardTuiConnection>,
) {
    let web_tui_tx = runtime.web_tui_tx.as_ref();
    for (_, conn) in tui_connections.drain() {
        let DashboardTuiConnection {
            internal_id,
            forwarder,
        } = conn;
        if let Some(web_tui_tx) = web_tui_tx {
            let _ = web_tui_tx
                .send(crate::tui::web::WebTuiCommand::RemoveConnection { id: internal_id });
        }
        forwarder.abort();
        let _ = forwarder.await;
    }
}

fn control_presence_frame(
    frame: serde_json::Value,
    runtime: ControlRuntime,
) -> Option<serde_json::Value> {
    let id = frame
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let payload = frame
        .get("frame")
        .or_else(|| frame.get("payload"))
        .cloned()
        .unwrap_or(frame);
    tokio::spawn(async move {
        handle_dashboard_presence_frame(payload, runtime).await;
    });
    if id.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "t": "presence_ack",
            "id": id,
            "ok": true,
        }))
    }
}

async fn handle_dashboard_presence_frame(frame: serde_json::Value, runtime: ControlRuntime) {
    let frame_type = frame
        .get("t")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match frame_type {
        "presence_connect" => dashboard_presence_connect(frame, runtime).await,
        "presence_disconnect" => dashboard_presence_disconnect(runtime).await,
        "make_active" => dashboard_make_active(frame, runtime).await,
        "voice_log" => dashboard_voice_log(frame, runtime).await,
        "presence_checkpoint" => dashboard_presence_checkpoint(frame, runtime).await,
        "voice_diagnostic" => dashboard_voice_diagnostic(frame, runtime).await,
        "live_usage_update" => dashboard_live_usage_update(frame, runtime).await,
        "tool_request" => dashboard_tool_request(frame, runtime).await,
        "async_query" => dashboard_async_query(frame, runtime).await,
        _ => {
            eprintln!("[dashboard/control] ignored unsupported presence frame: {frame_type}");
        }
    }
}

fn dashboard_control_emit_browser_event(runtime: &ControlRuntime, payload: serde_json::Value) {
    if let Some(tx) = &runtime.control_frames_tx {
        let _ = tx.send(serde_json::json!({
            "t": "event",
            "payload": payload,
        }));
    }
}

async fn dashboard_presence_connect(frame: serde_json::Value, runtime: ControlRuntime) {
    let server_session_id = frame
        .get("server_session_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let last_event_seq = frame
        .get("last_event_seq")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let provider = frame
        .get("provider")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("provider")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("model")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let passive = frame
        .get("passive")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    if let (Some(bridge), Some(control_tx)) =
        (runtime.presence.as_ref(), runtime.control_frames_tx.clone())
    {
        bridge
            .connect(DashboardPresenceConnectRequest {
                session_id: runtime.session_id.clone(),
                control_tx,
                server_session_id,
                last_event_seq,
                provider,
                model,
                passive,
            })
            .await;
        return;
    }

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);

    if let Some(ctx) = &query_ctx {
        let conversation_ctx = crate::presence::build_conversation_context(&ctx.log_dir, 20);
        if let Some(ps) = &ctx.presence_session {
            let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
            session.set_connected(true);
            let state = ctx
                .agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let welcome = session.build_welcome(last_event_seq, &state);
            dashboard_control_emit_browser_event(
                &runtime,
                serde_json::json!({
                    "t": "presence_welcome",
                    "session_id": welcome.session_id,
                    "state": welcome.state,
                    "events": welcome.events,
                    "last_checkpoint_summary": welcome.last_checkpoint_summary,
                    "current_seq": welcome.current_seq,
                    "is_active": true,
                    "conversation_context": conversation_ctx,
                }),
            );
        } else {
            dashboard_control_emit_browser_event(
                &runtime,
                serde_json::json!({
                    "t": "presence_welcome",
                    "is_active": true,
                    "conversation_context": conversation_ctx,
                }),
            );
        }
    } else {
        dashboard_control_emit_browser_event(
            &runtime,
            serde_json::json!({
                "t": "presence_welcome",
                "is_active": true,
            }),
        );
    }

    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_connected(provider.as_deref(), model.as_deref());
        }
    }
    runtime.bus.send(AppEvent::PresenceConnected {
        server_session_id,
        last_event_seq,
        live_provider: provider,
        live_model: model,
    });
}

async fn dashboard_presence_disconnect(runtime: ControlRuntime) {
    if let Some(bridge) = runtime.presence.as_ref() {
        bridge
            .disconnect(DashboardPresenceDisconnectRequest {
                session_id: runtime.session_id.clone(),
            })
            .await;
        return;
    }

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            ps.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_connected(false);
        }
    }
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_disconnected();
        }
    }
    runtime.bus.send(AppEvent::PresenceDisconnected);
}

async fn dashboard_make_active(frame: serde_json::Value, runtime: ControlRuntime) {
    let provider = frame
        .get("provider")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("provider")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("model")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    if let (Some(bridge), Some(control_tx)) =
        (runtime.presence.as_ref(), runtime.control_frames_tx.clone())
    {
        bridge
            .make_active(DashboardPresenceMakeActiveRequest {
                session_id: runtime.session_id.clone(),
                control_tx,
                provider,
                model,
            })
            .await;
        return;
    }
    dashboard_control_emit_browser_event(
        &runtime,
        serde_json::json!({
            "t": "active_granted",
            "is_active": true,
            "handover_context": "",
            "conversation_context": null,
        }),
    );
}

async fn dashboard_voice_log(frame: serde_json::Value, runtime: ControlRuntime) {
    let text = frame
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let seq = frame
        .get("seq")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let tool_context = frame
        .get("tool_context")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    if let Some(bridge) = runtime.presence.as_ref() {
        bridge.record_voice_log(text.clone());
    }
    let active = runtime.shared_session.read().await;
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_log(&text, seq, tool_context.as_deref());
        }
    }
    runtime.bus.send(AppEvent::VoiceLog {
        text,
        seq,
        tool_context,
    });
}

async fn dashboard_presence_checkpoint(frame: serde_json::Value, runtime: ControlRuntime) {
    let summary = frame
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let last_event_seq = frame
        .get("last_event_seq")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            let checkpoint = presence_core::PresenceCheckpoint {
                summary: summary.clone(),
                last_event_seq,
            };
            let ack = ps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_checkpoint(checkpoint);
            dashboard_control_emit_browser_event(
                &runtime,
                serde_json::json!({
                    "t": "presence_checkpoint_ack",
                    "seq": ack.seq,
                }),
            );
        }
    }
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_checkpoint(&summary, last_event_seq);
        }
    }
    runtime.bus.send(AppEvent::PresenceCheckpointReceived {
        summary,
        last_event_seq,
    });
}

async fn dashboard_voice_diagnostic(frame: serde_json::Value, runtime: ControlRuntime) {
    let kind = frame
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let detail = frame
        .get("detail")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let active = runtime.shared_session.read().await;
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_diagnostic(&kind, &detail);
        }
    }
    runtime.bus.send(AppEvent::VoiceDiagnostic { kind, detail });
}

fn json_u64(frame: &serde_json::Value, key: &str) -> u64 {
    frame.get(key).and_then(|value| value.as_u64()).unwrap_or(0)
}

async fn dashboard_live_usage_update(frame: serde_json::Value, runtime: ControlRuntime) {
    runtime.bus.send(AppEvent::LiveUsageUpdate {
        provider: frame
            .get("provider")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        model: frame
            .get("model")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        input_tokens: json_u64(&frame, "input_tokens"),
        output_tokens: json_u64(&frame, "output_tokens"),
        cached_tokens: json_u64(&frame, "cached_tokens"),
        total_tokens: json_u64(&frame, "total_tokens"),
        thinking_tokens: json_u64(&frame, "thinking_tokens"),
        input_text_tokens: json_u64(&frame, "input_text_tokens"),
        input_audio_tokens: json_u64(&frame, "input_audio_tokens"),
        input_image_tokens: json_u64(&frame, "input_image_tokens"),
        cached_text_tokens: json_u64(&frame, "cached_text_tokens"),
        cached_audio_tokens: json_u64(&frame, "cached_audio_tokens"),
        cached_image_tokens: json_u64(&frame, "cached_image_tokens"),
        output_text_tokens: json_u64(&frame, "output_text_tokens"),
        output_audio_tokens: json_u64(&frame, "output_audio_tokens"),
    });
}

fn dashboard_preview_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn dashboard_tool_result_frame(
    kind: &str,
    req_id: String,
    tool: Option<String>,
    query_result: crate::presence::ToolQueryResult,
) -> serde_json::Value {
    let mut response = serde_json::json!({
        "t": kind,
        "id": req_id,
        "result": query_result.text,
    });
    if let Some(tool) = tool {
        response["tool"] = serde_json::Value::String(tool);
    }
    if !query_result.images.is_empty() {
        let images = query_result
            .images
            .iter()
            .map(|img| {
                serde_json::json!({
                    "mime_type": img.media_type,
                    "data": img.data,
                })
            })
            .collect();
        response["images"] = serde_json::Value::Array(images);
    }
    response
}

async fn dashboard_tool_request(frame: serde_json::Value, runtime: ControlRuntime) {
    let req_id = frame
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let tool = frame
        .get("tool")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let args = frame
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    let args_preview = serde_json::to_string(&args)
        .map(|s| dashboard_preview_text(&s, 200))
        .unwrap_or_default();
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[tool_request] {}({})", tool, args_preview),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let frame_registry = active.frame_registry.clone();
    drop(active);

    let state = query_ctx
        .as_ref()
        .map(|ctx| {
            ctx.agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();
    let action = crate::presence::dispatch_tool_call(&tool, &args, &state);

    let query_result = if let crate::presence::PresenceAction::SubmitTask(envelope) = action {
        let msg = format!("Task submitted: {}", envelope.task);
        if let Some(tx) = runtime.task_tx.as_ref() {
            let _ = tx.send(envelope).await;
        } else {
            let ctrl_action = crate::presence::PresenceAction::SubmitTask(envelope);
            if let Some((ctrl, _)) = crate::presence::action_to_control_msg(&ctrl_action) {
                runtime.bus.send(AppEvent::ControlCommand(ctrl));
            }
        }
        crate::presence::ToolQueryResult::text(msg)
    } else if let Some((ctrl, msg)) = crate::presence::action_to_control_msg(&action) {
        runtime.bus.send(AppEvent::ControlCommand(ctrl));
        crate::presence::ToolQueryResult::text(msg)
    } else {
        match action {
            crate::presence::PresenceAction::TextResult(text) => {
                crate::presence::ToolQueryResult::text(text)
            }
            crate::presence::PresenceAction::NeedsIO {
                tool_name,
                args: io_args,
            } => {
                if let Some(ctx) = query_ctx.as_ref() {
                    crate::presence::handle_tool_query(
                        &ctx.agent_state,
                        &ctx.project_root,
                        &ctx.log_dir,
                        &ctx.knowledge_path,
                        &tool_name,
                        &io_args,
                        frame_registry.as_ref(),
                        ctx.context_injection.as_ref(),
                    )
                    .await
                    .unwrap_or_else(|| {
                        crate::presence::ToolQueryResult::text(format!("Unknown tool: {}", tool))
                    })
                } else {
                    crate::presence::ToolQueryResult::text(
                        "Presence query context not available".to_string(),
                    )
                }
            }
            _ => unreachable!(),
        }
    };

    runtime.bus.send(AppEvent::PresenceLog {
        message: format!(
            "[tool_response] {} -> {}",
            tool,
            dashboard_preview_text(&query_result.text, 200)
        ),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    dashboard_control_emit_browser_event(
        &runtime,
        dashboard_tool_result_frame("tool_response", req_id, None, query_result),
    );
}

async fn dashboard_async_query(frame: serde_json::Value, runtime: ControlRuntime) {
    let req_id = frame
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let tool = frame
        .get("tool")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let args = frame
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[async_query] {}", tool),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let frame_registry = active.frame_registry.clone();
    drop(active);

    let query_result = if let Some(ctx) = query_ctx.as_ref() {
        crate::presence::handle_tool_query(
            &ctx.agent_state,
            &ctx.project_root,
            &ctx.log_dir,
            &ctx.knowledge_path,
            &tool,
            &args,
            frame_registry.as_ref(),
            ctx.context_injection.as_ref(),
        )
        .await
        .unwrap_or_else(|| {
            crate::presence::ToolQueryResult::text(format!("Unknown query tool: {}", tool))
        })
    } else {
        crate::presence::ToolQueryResult::text("Presence query context not available".to_string())
    };

    runtime.bus.send(AppEvent::PresenceLog {
        message: format!(
            "[async_query_result] {} -> {}",
            tool,
            dashboard_preview_text(&query_result.text, 200)
        ),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    dashboard_control_emit_browser_event(
        &runtime,
        dashboard_tool_result_frame("async_query_result", req_id, Some(tool), query_result),
    );
}

fn spawn_dashboard_display_input(frame: serde_json::Value, runtime: ControlRuntime) {
    tokio::spawn(async move {
        let display_id = frame
            .get("display_id")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let Some(event) = frame.get("event").cloned() else {
            return;
        };
        let Ok(input_event) = serde_json::from_value::<crate::display::InputEvent>(event) else {
            return;
        };
        let Some(bridge) = runtime.display_authority.as_ref() else {
            return;
        };
        if !bridge.input_authorized(&runtime.session_id, display_id) {
            return;
        }
        let session_registry = {
            let session = runtime.shared_session.read().await;
            session.session_registry.clone()
        };
        let Some(session_registry) = session_registry else {
            return;
        };
        let display_session = {
            let registry = session_registry.read().await;
            registry.get(display_id)
        };
        if let Some(display_session) = display_session {
            if let Err(e) = display_session.inject_input(input_event).await {
                eprintln!("[dashboard/control] display input injection failed: {e}");
            }
        }
    });
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
    result.insert(
        "protocol".to_string(),
        serde_json::json!(CONTROL_PROTOCOL_VERSION),
    );
    result.insert(
        "session_id".to_string(),
        serde_json::json!(runtime.session_id),
    );
    result.insert(
        "daemon_public_key".to_string(),
        serde_json::json!(runtime.daemon_public_key),
    );
    result.insert(
        "created_unix_ms".to_string(),
        serde_json::json!(runtime.created_unix_ms),
    );
    result.insert("features".to_string(), serde_json::json!(CONTROL_FEATURES));
    result.insert(
        "transport".to_string(),
        serde_json::json!("webrtc-datachannel"),
    );
    result.insert(
        "events_subscribed".to_string(),
        serde_json::json!(runtime.events_subscribed),
    );
    result.insert(
        "events_sent".to_string(),
        serde_json::json!(runtime.events_sent),
    );
    result.insert(
        "response_credit_enabled".to_string(),
        serde_json::json!(runtime.response_credit_enabled),
    );
    result.insert(
        "grant_kind".to_string(),
        serde_json::json!(runtime.grant.wire_kind()),
    );
    result.insert(
        "grant_label".to_string(),
        serde_json::json!(runtime.grant.label()),
    );
    if let Some(profile) = runtime.grant.profile() {
        result.insert("grant_profile".to_string(), serde_json::json!(profile));
    }

    let peer_registry_available = runtime.peer_registry.is_some();
    let presence_read = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::PresenceRead,
    );
    let session_inspect = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::SessionInspect,
    );
    let session_manage = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::SessionManage,
    );
    let fs_read = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::FilesystemRead,
    );
    let fs_write = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::FilesystemWrite,
    );
    let terminal =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::Terminal);
    let display_view =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::DisplayView);
    let display_input =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::DisplayInput);
    let settings =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::Settings);
    let runtime_control = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::RuntimeControl,
    );
    let access_inspect = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::AccessInspect,
    );
    let access_manage = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::AccessManage,
    );
    let peer_inspect = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::PeerInspect,
    );
    let peer_manage =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::PeerManage);
    let message =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::Message);
    let capabilities = [
        ("access_inspect_available", access_inspect),
        ("access_manage_available", access_manage),
        ("peer_inspect_available", peer_inspect),
        ("peer_manage_available", peer_manage),
        ("api_peers_available", peer_registry_available && peer_inspect),
        ("api_access_overview_available", access_inspect),
        ("api_dashboard_targets_available", access_inspect),
        ("api_agent_card_available", presence_read),
        ("api_cached_bootstrap_events_available", session_inspect),
        ("api_browser_workspace_snapshot_available", session_inspect),
        ("api_state_snapshot_available", session_inspect),
        ("api_display_bootstrap_available", display_view),
        (
            "api_display_input_authority_available",
            runtime.display_authority.is_some() && display_input,
        ),
        ("api_display_webrtc_signal_available", display_view),
        ("api_session_log_replay_available", session_inspect),
        (
            "api_external_session_activity_replay_available",
            session_inspect,
        ),
        ("api_dashboard_bootstrap_available", session_inspect),
        ("byte_streams_available", true),
        ("upload_frames_available", fs_write),
        ("terminal_frames_available", terminal),
        ("tui_frames_available", runtime.web_tui_tx.is_some()),
        ("presence_frames_available", message),
        (
            "presence_active_handoff_available",
            runtime.presence.is_some() && message,
        ),
        ("presence_tool_request_available", message),
        ("api_presence_video_frame_available", runtime_control),
        ("api_sessions_available", session_inspect),
        ("api_sessions_stream_available", session_inspect),
        ("api_session_detail_available", session_inspect),
        ("api_session_report_available", session_inspect),
        ("api_session_delete_available", session_manage),
        ("api_session_agent_output_available", session_inspect),
        ("api_session_current_agent_output_available", session_manage),
        ("api_session_current_history_available", session_manage),
        ("api_session_current_rollback_available", session_manage),
        ("api_session_current_redo_available", session_manage),
        ("api_session_current_prune_available", session_manage),
        ("api_session_current_changes_available", session_manage),
        ("api_session_context_snapshot_available", session_manage),
        ("api_session_current_uploads_available", session_manage),
        ("api_session_current_upload_available", session_manage),
        ("api_session_current_upload_raw_available", session_manage),
        (
            "api_session_current_upload_delete_available",
            session_manage,
        ),
        (
            "api_transfer_jobs_available",
            runtime.project_root.is_some() && fs_read,
        ),
        (
            "api_transfer_job_create_available",
            runtime.project_root.is_some() && fs_write,
        ),
        (
            "api_transfer_job_delete_available",
            runtime.project_root.is_some() && fs_write,
        ),
        (
            "api_transfer_download_read_available",
            runtime.project_root.is_some() && fs_read,
        ),
        (
            "api_transfer_upload_chunk_available",
            runtime.project_root.is_some() && fs_write,
        ),
        (
            "api_transfer_upload_commit_available",
            runtime.project_root.is_some() && fs_write,
        ),
        ("api_media_editor_available", runtime_control),
        ("api_media_annotation_attach_available", runtime_control),
        ("api_media_annotation_submit_available", runtime_control),
        ("api_media_clip_start_available", runtime_control),
        ("api_media_clip_frame_available", runtime_control),
        ("api_media_clip_end_available", runtime_control),
        ("api_media_clip_cancel_available", runtime_control),
        ("api_fs_stat_available", fs_read),
        ("api_fs_list_available", fs_read),
        ("api_fs_mkdir_available", fs_write),
        ("api_fs_read_available", fs_read),
        ("api_sessions_search_available", session_inspect),
        ("api_settings_available", settings),
        (
            "api_settings_save_available",
            runtime.project_root.is_some() && settings,
        ),
        ("api_control_msg_available", message),
        ("api_session_control_msg_available", session_manage),
        ("api_dashboard_action_msg_available", message),
        ("api_diagnostics_visual_freshness_available", display_input),
        ("api_key_status_available", settings),
        ("api_api_keys_save_available", settings),
        ("api_voice_session_available", runtime_control),
        ("api_project_root_available", settings),
        ("api_displays_available", display_view),
        ("api_recordings_available", runtime_control),
        ("api_recording_asset_available", runtime_control),
        ("api_session_recordings_available", session_inspect),
        ("api_session_recording_asset_available", session_inspect),
        ("api_session_frame_asset_available", session_inspect),
        ("api_worktrees_available", session_inspect),
        ("api_worktrees_scan_available", session_manage),
        ("api_worktrees_remove_available", session_manage),
        ("api_managed_context_available", session_inspect),
        ("api_mcp_tool_call_available", runtime.mcp_server.is_some() && message),
        ("api_peer_mutations_available", peer_registry_available && peer_manage),
        (
            "api_peer_webrtc_signal_available",
            peer_registry_available && peer_manage,
        ),
        (
            "api_peer_file_transfer_signal_available",
            peer_registry_available && peer_manage,
        ),
        (
            "api_peer_dashboard_control_signal_available",
            peer_registry_available && peer_manage,
        ),
        ("api_peer_pairing_available", peer_manage || access_manage),
        ("api_peer_pairing_invite_available", access_manage),
        ("api_peer_pairing_join_available", peer_manage),
        (
            "api_peer_pairing_request_access_available",
            peer_manage,
        ),
        (
            "api_peer_pairing_request_decision_available",
            access_manage,
        ),
        (
            "api_peer_pairing_requests_available",
            access_inspect || access_manage,
        ),
        (
            "api_peer_pairing_identities_available",
            access_inspect || access_manage,
        ),
        (
            "api_peer_pairing_identity_revoke_available",
            access_manage,
        ),
        ("api_coordinator_available", peer_registry_available && peer_manage),
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
        let response = match method.as_str() {
            "api_session_report" => {
                api_session_report_task_response(id.clone(), params.as_ref(), &runtime).await
            }
            "api_session_current_upload_raw" => {
                api_session_current_upload_raw_task_response(id.clone(), params.as_ref(), &runtime)
                    .await
            }
            "api_recording_asset" => {
                api_recording_asset_task_response(id.clone(), params.as_ref(), &runtime).await
            }
            "api_session_recording_asset" => {
                api_session_recording_asset_task_response(id.clone(), params.as_ref()).await
            }
            "api_session_frame_asset" => {
                api_session_frame_asset_task_response(id.clone(), params.as_ref()).await
            }
            "api_fs_read" => api_fs_read_task_response(id.clone(), params.as_ref()).await,
            "api_transfer_download_read" => {
                api_transfer_download_read_task_response(id.clone(), params.as_ref(), &runtime)
                    .await
            }
            _ => {
                let frame =
                    control_request_response(id.clone(), method, params, runtime, cancel).await;
                ControlTaskResponse {
                    id,
                    frame,
                    byte_stream: None,
                    done: true,
                }
            }
        };
        let _ = task_tx.send(response).await;
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
                        byte_stream: None,
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
        "api_session_agent_output" => api_session_agent_output_response(id, params.as_ref()).await,
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
        }
        "api_session_context_snapshot" => {
            api_session_context_snapshot_response(id, params.as_ref()).await
        }
        "api_session_current_uploads" => api_session_current_uploads_response(id, &runtime).await,
        "api_session_current_upload_delete" => {
            api_session_current_upload_delete_response(id, params.as_ref(), &runtime).await
        }
        "api_transfer_jobs" => api_transfer_jobs_response(id, &runtime).await,
        "api_transfer_job_create" => {
            api_transfer_job_create_response(id, params.as_ref(), &runtime).await
        }
        "api_transfer_job_delete" => {
            api_transfer_job_delete_response(id, params.as_ref(), &runtime).await
        }
        "api_transfer_upload_commit" => {
            api_transfer_upload_commit_response(id, params.as_ref(), &runtime).await
        }
        "api_media_clip_start" => {
            api_media_clip_start_response(id, params.as_ref(), &runtime).await
        }
        "api_media_clip_end" => api_media_clip_end_response(id, params.as_ref(), &runtime).await,
        "api_media_clip_cancel" => {
            api_media_clip_cancel_response(id, params.as_ref(), &runtime).await
        }
        "api_fs_stat" => api_fs_stat_response(id, params.as_ref()).await,
        "api_fs_list" => api_fs_list_response(id, params.as_ref()).await,
        "api_fs_mkdir" => api_fs_mkdir_response(id, params.as_ref()).await,
        "api_sessions_search" => api_sessions_search_response(id, params.as_ref(), cancel).await,
        "api_settings" => api_settings_response(id, &runtime).await,
        "api_settings_save" => api_settings_save_response(id, params.as_ref(), &runtime).await,
        "api_control_msg" => api_control_msg_response(id, params.as_ref(), &runtime).await,
        "api_session_control_msg" => {
            api_session_control_msg_response(id, params.as_ref(), &runtime).await
        }
        "api_dashboard_action_msg" => {
            api_dashboard_action_msg_response(id, params.as_ref(), &runtime).await
        }
        "api_diagnostics_visual_freshness" => {
            api_diagnostics_visual_freshness_response(id, params.as_ref()).await
        }
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
        "api_display_bootstrap" => api_display_bootstrap_response(id, &runtime).await,
        "api_display_webrtc_signal" => {
            api_display_webrtc_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_display_input_authority_snapshot" => {
            api_display_input_authority_snapshot_response(id, &runtime).await
        }
        "api_display_input_authority_request" => {
            api_display_input_authority_request_response(id, params.as_ref(), &runtime).await
        }
        "api_display_input_authority_release" => {
            api_display_input_authority_release_response(id, params.as_ref(), &runtime).await
        }
        "api_session_log_replay" => api_session_log_replay_response(id, &runtime).await,
        "api_external_session_activity_replay" => {
            api_external_session_activity_replay_response(id, &runtime).await
        }
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
        "api_peer_webrtc_signal" => {
            api_peer_webrtc_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_file_transfer_signal" => {
            api_peer_file_transfer_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_dashboard_control_signal" => {
            api_peer_dashboard_control_signal_response(id, params.as_ref(), &runtime).await
        }
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
            byte_stream: None,
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
                        byte_stream: None,
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
                byte_stream: None,
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
                byte_stream: None,
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
    let before = control_session_detail_before(&params);
    let body = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_detail_response_body_with_page(
            &session_id,
            &source,
            limit,
            before,
        )
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

async fn api_session_report_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = optional_string_param(&params, &["session_id", "sessionId", "id"])
        .unwrap_or_else(|| "current".to_string());
    let (session_log, query_ctx) = {
        let session = runtime.shared_session.read().await;
        (session.session_log.clone(), session.query_ctx.clone())
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_report_zip_for_request(
            &session_id,
            session_log.as_ref(),
            query_ctx.as_ref(),
        )
    })
    .await;
    let report = match result {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => {
            let (status, error) = match err {
                crate::web_gateway::SessionReportZipError::InvalidSessionId => {
                    (400, "invalid session id".to_string())
                }
                crate::web_gateway::SessionReportZipError::NotFound => {
                    (404, "Session not found".to_string())
                }
                crate::web_gateway::SessionReportZipError::Build(error) => {
                    (500, format!("Failed to build report: {error}"))
                }
            };
            let frame = http_body_response(
                id.clone(),
                status,
                serde_json::json!({
                    "ok": false,
                    "error": error,
                })
                .to_string(),
                "session report",
            );
            return ControlTaskResponse {
                id,
                frame,
                byte_stream: None,
                done: true,
            };
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("session report task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let size = report.bytes.len();
    let filename = report.filename;
    let content_type = "application/zip".to_string();
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:session-report"),
            content_type: content_type.clone(),
            filename: Some(filename.clone()),
            bytes: report.bytes,
            result: serde_json::json!({
                "ok": true,
                "filename": filename,
                "content_type": content_type,
                "size": size,
            }),
        }),
        done: true,
    }
}

async fn api_session_current_upload_raw_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let Some(upload_id) = optional_string_param(&params, &["id", "upload_id", "uploadId"]) else {
        return ControlTaskResponse {
            id: id.clone(),
            frame: http_body_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": "missing upload id" }).to_string(),
                "upload raw",
            ),
            byte_stream: None,
            done: true,
        };
    };
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(offset) => offset.unwrap_or(0),
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    400,
                    serde_json::json!({ "ok": false, "error": error }).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(length) => length,
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    400,
                    serde_json::json!({ "ok": false, "error": error }).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
    };
    let Some(root) = runtime.project_root.clone() else {
        return ControlTaskResponse {
            id: id.clone(),
            frame: http_body_response(
                id,
                404,
                serde_json::json!({ "ok": false, "error": "no project root" }).to_string(),
                "upload raw",
            ),
            byte_stream: None,
            done: true,
        };
    };
    let session_log = {
        let session = runtime.shared_session.read().await;
        session.session_log.clone()
    };
    let session_dir_result = match session_log {
        Some(ref slog) => slog
            .lock()
            .map(|log| log.dir().to_path_buf())
            .map_err(|_| "session log lock poisoned".to_string()),
        None => Ok(crate::web_gateway::pending_upload_session_dir(&root)),
    };
    let session_dir = match session_dir_result {
        Ok(session_dir) => session_dir,
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    500,
                    serde_json::json!({ "ok": false, "error": error }).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
    };
    let upload_id_for_stream = upload_id.clone();
    let read_result = tokio::task::spawn_blocking(move || {
        let Some(descriptor) = crate::upload_store::find_upload(&upload_id, &session_dir, &root)
        else {
            return Err((
                404,
                serde_json::json!({ "ok": false, "error": "upload not found" }),
            ));
        };
        let metadata = std::fs::metadata(&descriptor.path).map_err(|e| {
            (
                500,
                serde_json::json!({ "ok": false, "error": format!("stat upload: {e}") }),
            )
        })?;
        let total_size = metadata.len();
        if offset > total_size {
            return Err((
                416,
                serde_json::json!({
                    "ok": false,
                    "error": "range start beyond upload size",
                    "total_size": total_size,
                }),
            ));
        }
        let available = total_size.saturating_sub(offset);
        let requested = length.unwrap_or(available).min(available);
        if requested > crate::web_gateway::UPLOAD_MAX_BYTES as u64 {
            return Err((
                413,
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "range too large: {} bytes (cap is {})",
                        requested,
                        crate::web_gateway::UPLOAD_MAX_BYTES
                    ),
                }),
            ));
        }
        let transfer_len = usize::try_from(requested).map_err(|_| {
            (
                413,
                serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
            )
        })?;
        let mut file = std::fs::File::open(&descriptor.path).map_err(|e| {
            (
                500,
                serde_json::json!({ "ok": false, "error": format!("open upload: {e}") }),
            )
        })?;
        file.seek(std::io::SeekFrom::Start(offset)).map_err(|e| {
            (
                500,
                serde_json::json!({ "ok": false, "error": format!("seek upload: {e}") }),
            )
        })?;
        let mut bytes = vec![0u8; transfer_len];
        file.read_exact(&mut bytes).map_err(|e| {
            (
                500,
                serde_json::json!({ "ok": false, "error": format!("read upload: {e}") }),
            )
        })?;
        let end = offset.saturating_add(requested);
        let descriptor_id = descriptor.id.clone();
        let descriptor_name = descriptor.name.clone();
        let descriptor_mime = descriptor.mime.clone();
        Ok((
            descriptor_name.clone(),
            descriptor_mime.clone(),
            bytes,
            serde_json::json!({
                "ok": true,
                "id": descriptor_id,
                "name": descriptor_name,
                "filename": descriptor_name,
                "mime": descriptor_mime,
                "content_type": descriptor_mime,
                "size": requested,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
            }),
        ))
    })
    .await;
    let (filename, content_type, bytes, result) = match read_result {
        Ok(Ok(value)) => value,
        Ok(Err((status, body))) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(id, status, body.to_string(), "upload raw"),
                byte_stream: None,
                done: true,
            };
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("upload raw task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:upload:{upload_id_for_stream}"),
            content_type,
            filename: Some(filename),
            bytes,
            result,
        }),
        done: true,
    }
}

async fn api_session_current_uploads_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (project_root, session_dir) = match active_upload_handles(runtime).await {
        Ok(handles) => handles,
        Err(error) => {
            return http_body_response(
                id,
                500,
                serde_json::json!({ "error": error }).to_string(),
                "current uploads",
            );
        }
    };
    let Some(root) = project_root else {
        return http_body_response(
            id,
            404,
            serde_json::json!({ "error": "no project root" }).to_string(),
            "current uploads",
        );
    };
    let session_dir =
        session_dir.unwrap_or_else(|| crate::web_gateway::pending_upload_session_dir(&root));
    let result = tokio::task::spawn_blocking(move || {
        serde_json::to_string(&crate::upload_store::list_uploads(&session_dir, &root))
            .unwrap_or_else(|_| "[]".to_string())
    })
    .await;
    match result {
        Ok(body) => json_body_response(id, body, "current uploads"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("current uploads task failed: {e}"),
        }),
    }
}

async fn api_session_current_upload_task_response(
    id: String,
    upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let name = optional_string_param(&params, &["name", "filename", "file_name"])
        .unwrap_or_else(|| "upload.bin".to_string());
    let mime = optional_string_param(&params, &["mime", "content_type", "contentType"])
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let requested_destination = optional_string_param(&params, &["destination"])
        .as_deref()
        .and_then(crate::upload_store::UploadDestination::from_str)
        .unwrap_or(crate::upload_store::UploadDestination::Task);
    let (session_log, daemon_session_id) = {
        let session = runtime.shared_session.read().await;
        (
            session.session_log.clone(),
            Some(runtime.session_id.clone()),
        )
    };
    let project_root = runtime.project_root.clone();
    let bus = runtime.bus.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::current_upload_commit_response_body(
            project_root.as_deref(),
            session_log.as_ref(),
            daemon_session_id.as_deref(),
            &name,
            &mime,
            requested_destination,
            upload.tmp,
            upload.received_bytes,
            &bus,
        )
    })
    .await;
    let frame = match result {
        Ok((status, body)) => {
            http_body_response(id.clone(), status_line_code(status), body, "current upload")
        }
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id.clone(),
            "ok": false,
            "error": format!("upload commit task failed: {e}"),
        }),
    };
    ControlTaskResponse {
        id,
        frame,
        byte_stream: None,
        done: true,
    }
}

fn media_http_response(id: String, status: u16, body: serde_json::Value) -> serde_json::Value {
    http_body_response(id, status, body.to_string(), "dashboard media")
}

fn media_error_task_response(
    id: String,
    status: u16,
    error: impl Into<String>,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: media_http_response(
            id,
            status,
            serde_json::json!({
                "ok": false,
                "error": error.into(),
            }),
        ),
        byte_stream: None,
        done: true,
    }
}

fn media_task_response(id: String, status: u16, body: serde_json::Value) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: media_http_response(id, status, body),
        byte_stream: None,
        done: true,
    }
}

fn read_inbound_upload_bytes(upload: &mut InboundUploadState) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(upload.received_bytes);
    upload
        .tmp
        .as_file_mut()
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("seek upload tempfile: {e}"))?;
    upload
        .tmp
        .as_file_mut()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read upload tempfile: {e}"))?;
    if bytes.len() != upload.received_bytes {
        return Err(format!(
            "upload byte count changed while committing: expected {}, got {}",
            upload.received_bytes,
            bytes.len()
        ));
    }
    Ok(bytes)
}

async fn dashboard_media_session_handles(
    runtime: &ControlRuntime,
) -> (
    Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    Option<crate::web_gateway::WebQueryCtx>,
) {
    let session = runtime.shared_session.read().await;
    (session.frame_registry.clone(), session.query_ctx.clone())
}

async fn register_dashboard_media_frame(
    registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    frame_id: &str,
    stream: &str,
    note: Option<String>,
    bytes: &[u8],
    log_label: &str,
) -> (String, bool) {
    let Some(registry) = registry else {
        return (String::new(), false);
    };
    let meta = presence_core::FrameMeta {
        frame_id: frame_id.to_string(),
        stream: stream.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        sent_to_live: false,
        live_resolution: None,
        hq_resolution: None,
        note,
    };
    let mut reg = registry.write().await;
    match reg.register(meta, bytes) {
        Ok(path) => (path.display().to_string(), true),
        Err(e) => {
            eprintln!("{log_label} frame registry write failed: {e}");
            (String::new(), false)
        }
    }
}

async fn api_presence_video_frame_upload_task_response(
    id: String,
    mut upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let frame_id = string_param(&params, &["frame_id", "frameId"]);
    if frame_id.is_empty() {
        return media_error_task_response(id, 400, "missing frame_id");
    }
    let stream = optional_string_param(&params, &["stream", "stream_name", "streamName"])
        .unwrap_or_else(|| "cam0".to_string());
    let bytes = match read_inbound_upload_bytes(&mut upload) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return media_error_task_response(id, 400, "empty video frame upload"),
        Err(e) => return media_error_task_response(id, 500, e),
    };
    let (registered, recorded) =
        register_dashboard_presence_video_frame(&runtime, &frame_id, &stream, &bytes).await;
    media_task_response(
        id,
        200,
        serde_json::json!({
            "t": "presence_video_frame_saved",
            "ok": true,
            "frame_id": frame_id,
            "stream": stream,
            "registered": registered,
            "recorded": recorded,
        }),
    )
}

async fn register_dashboard_presence_video_frame(
    runtime: &ControlRuntime,
    frame_id: &str,
    stream: &str,
    jpeg_bytes: &[u8],
) -> (bool, bool) {
    let session = runtime.shared_session.read().await;
    let frame_registry = session.frame_registry.clone();
    let recording_registry = session.recording_registry.clone();
    drop(session);

    let mut registered = false;
    if let Some(registry) = frame_registry {
        let meta = presence_core::FrameMeta {
            frame_id: frame_id.to_string(),
            stream: stream.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            sent_to_live: true,
            live_resolution: Some("768x768".to_string()),
            hq_resolution: None,
            note: None,
        };
        let mut reg = registry.write().await;
        match reg.register(meta, jpeg_bytes) {
            Ok(_) => registered = true,
            Err(e) => eprintln!("presence video frame registry write failed: {e}"),
        }
    }

    let mut recorded = false;
    if let Some(registry) = recording_registry {
        let mut rec = registry.write().await;
        if rec.is_enabled() {
            if !rec.is_recording(stream) && crate::recording::is_ffmpeg_available() {
                match rec.start_stream(stream).await {
                    Ok(()) => {
                        runtime.bus.send(AppEvent::RecordingStarted {
                            stream_name: stream.to_string(),
                        });
                    }
                    Err(e) => eprintln!("presence video recording start failed: {e}"),
                }
            }
            if let Err(e) = rec.feed_frame(stream, jpeg_bytes).await {
                eprintln!("presence video recording frame failed: {e}");
            } else {
                recorded = true;
            }
        }
    }

    (registered, recorded)
}

fn inject_annotation_context(
    query_ctx: Option<&crate::web_gateway::WebQueryCtx>,
    note: &str,
    data_b64: String,
) -> bool {
    let Some(ctx) = query_ctx else {
        return false;
    };
    let Some(ciq) = ctx.context_injection.as_ref() else {
        return false;
    };
    let Ok(mut queue) = ciq.lock() else {
        return false;
    };
    let label = if note.is_empty() {
        "[User Annotation] User highlighted something on the screen.".to_string()
    } else {
        format!("[User Annotation] {note}")
    };
    queue.push(crate::event::ContextInjection {
        text: label,
        images: vec![crate::conversation::ImageData {
            media_type: "image/jpeg".to_string(),
            data: data_b64,
        }],
        source: crate::event::InjectionSource::User,
        target_session_id: None,
        steer_id: None,
    });
    true
}

fn inject_clip_context(
    query_ctx: Option<&crate::web_gateway::WebQueryCtx>,
    _clip_id: &str,
    clip: &DashboardMediaClipOperation,
) -> bool {
    let Some(ctx) = query_ctx else {
        return false;
    };
    let Some(ciq) = ctx.context_injection.as_ref() else {
        return false;
    };
    let Ok(mut queue) = ciq.lock() else {
        return false;
    };
    let frames_registered = clip.frames.len();
    let label = if clip.note.is_empty() {
        format!(
            "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps)",
            clip.stream, clip.in_secs, clip.out_secs, frames_registered, clip.fps,
        )
    } else {
        format!(
            "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps). {}",
            clip.stream, clip.in_secs, clip.out_secs, frames_registered, clip.fps, clip.note,
        )
    };
    let images = clip
        .frames
        .iter()
        .map(|(_, data)| crate::conversation::ImageData {
            media_type: "image/jpeg".to_string(),
            data: data.clone(),
        })
        .collect();
    queue.push(crate::event::ContextInjection {
        text: label,
        images,
        source: crate::event::InjectionSource::User,
        target_session_id: None,
        steer_id: None,
    });
    true
}

async fn api_media_annotation_upload_task_response(
    id: String,
    mut upload: InboundUploadState,
    runtime: ControlRuntime,
    submit: bool,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let frame_id = string_param(&params, &["frame_id", "frameId"]);
    if frame_id.is_empty() {
        return media_error_task_response(id, 400, "missing frame_id");
    }
    let stream = optional_string_param(&params, &["stream"]).unwrap_or_else(|| "annotation".into());
    let note = optional_string_param(&params, &["note"]).unwrap_or_default();
    let inject = params
        .get("inject")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let bytes = match read_inbound_upload_bytes(&mut upload) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return media_error_task_response(id, 400, "empty media upload"),
        Err(e) => return media_error_task_response(id, 500, e),
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let (registry, query_ctx) = dashboard_media_session_handles(&runtime).await;
    let (saved_path, registered) = register_dashboard_media_frame(
        registry,
        &frame_id,
        &stream,
        if note.is_empty() {
            None
        } else {
            Some(note.clone())
        },
        &bytes,
        if submit {
            "annotation"
        } else {
            "annotation_attach"
        },
    )
    .await;

    if submit {
        let injected_to_queue =
            inject && inject_annotation_context(query_ctx.as_ref(), &note, data_b64);
        let status_label = if inject {
            if injected_to_queue {
                " (sent to agent)"
            } else {
                " (saved - no agent connected)"
            }
        } else {
            ""
        };
        runtime.bus.send(AppEvent::PresenceLog {
            message: format!("[annotation] {frame_id} on {stream}{status_label}"),
            level: Some(LogLevel::Info),
            turn: None,
        });
        media_task_response(
            id,
            200,
            serde_json::json!({
                "t": "annotation_saved",
                "ok": registered,
                "frame_id": frame_id,
                "stream": stream,
                "path": saved_path,
                "injected": injected_to_queue,
            }),
        )
    } else {
        runtime.bus.send(AppEvent::PresenceLog {
            message: format!("[annotation] {frame_id} attached (pending)"),
            level: Some(LogLevel::Info),
            turn: None,
        });
        media_task_response(
            id,
            200,
            serde_json::json!({
                "t": "annotation_attached",
                "ok": registered,
                "frame_id": frame_id,
                "stream": stream,
                "path": saved_path,
                "note": note,
            }),
        )
    }
}

fn f64_param(params: &serde_json::Value, name: &str, default: f64) -> f64 {
    params
        .get(name)
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        })
        .unwrap_or(default)
}

fn usize_param(params: &serde_json::Value, name: &str, default: usize) -> usize {
    params
        .get(name)
        .and_then(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .or_else(|| value.as_str().and_then(|text| text.parse::<usize>().ok()))
        })
        .unwrap_or(default)
}

async fn api_media_clip_start_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    if clip_id.is_empty() {
        return media_http_response(
            id,
            400,
            serde_json::json!({"ok": false, "error": "missing clip_id"}),
        );
    }
    let total_frames = usize_param(&params, "total_frames", 0);
    if total_frames > DASHBOARD_MEDIA_CLIP_MAX_FRAMES {
        return media_http_response(
            id,
            413,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "clip has {total_frames} frames; cap is {DASHBOARD_MEDIA_CLIP_MAX_FRAMES}"
                ),
            }),
        );
    }
    let fps = usize_param(&params, "fps", 2).max(1) as u32;
    let op = DashboardMediaClipOperation {
        stream: optional_string_param(&params, &["stream"]).unwrap_or_else(|| "recording".into()),
        note: optional_string_param(&params, &["note"]).unwrap_or_default(),
        inject: params
            .get("inject")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        in_secs: f64_param(&params, "in_secs", 0.0),
        out_secs: f64_param(&params, "out_secs", 0.0),
        fps,
        expected_frames: total_frames,
        frames: Vec::with_capacity(total_frames),
    };
    let mut ops = runtime.media_clip_ops.lock().await;
    if ops.contains_key(&clip_id) {
        return media_http_response(
            id,
            409,
            serde_json::json!({"ok": false, "error": "clip operation already exists"}),
        );
    }
    ops.insert(clip_id.clone(), op);
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[clip] started {clip_id} ({total_frames} frames, {fps}fps)"),
        level: Some(LogLevel::Debug),
        turn: None,
    });
    media_http_response(
        id,
        200,
        serde_json::json!({
            "t": "media_clip_started",
            "ok": true,
            "op_id": clip_id,
            "clip_id": clip_id,
            "expected_frames": total_frames,
        }),
    )
}

async fn api_media_clip_frame_upload_task_response(
    id: String,
    mut upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    let frame_id = string_param(&params, &["frame_id", "frameId"]);
    if clip_id.is_empty() {
        return media_error_task_response(id, 400, "missing clip_id");
    }
    if frame_id.is_empty() {
        return media_error_task_response(id, 400, "missing frame_id");
    }
    let bytes = match read_inbound_upload_bytes(&mut upload) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return media_error_task_response(id, 400, "empty media upload"),
        Err(e) => return media_error_task_response(id, 500, e),
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let requested_index = usize_param(&params, "frame_index", usize::MAX);
    let frames_received = {
        let mut ops = runtime.media_clip_ops.lock().await;
        let Some(op) = ops.get_mut(&clip_id) else {
            return media_error_task_response(id, 404, "unknown clip operation");
        };
        let next_index = op.frames.len();
        if requested_index != usize::MAX && requested_index != next_index {
            return media_error_task_response(
                id,
                409,
                format!("clip frame index mismatch: expected {next_index}, got {requested_index}"),
            );
        }
        if op.expected_frames > 0 && next_index >= op.expected_frames {
            return media_error_task_response(id, 409, "clip frame count exceeded");
        }
        op.frames.push((frame_id.clone(), data_b64));
        op.frames.len()
    };
    let (registry, _) = dashboard_media_session_handles(&runtime).await;
    let (_, registered) = register_dashboard_media_frame(
        registry,
        &frame_id,
        &format!("clip:{clip_id}"),
        None,
        &bytes,
        "clip",
    )
    .await;
    media_task_response(
        id,
        200,
        serde_json::json!({
            "t": "media_clip_frame_saved",
            "ok": true,
            "registered": registered,
            "op_id": clip_id,
            "clip_id": clip_id,
            "frame_id": frame_id,
            "frames_received": frames_received,
        }),
    )
}

async fn api_media_clip_end_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    if clip_id.is_empty() {
        return media_http_response(
            id,
            400,
            serde_json::json!({"ok": false, "error": "missing clip_id"}),
        );
    }
    let frames_sent = usize_param(&params, "frames_sent", usize::MAX);
    let clip = {
        let mut ops = runtime.media_clip_ops.lock().await;
        let Some(op) = ops.get(&clip_id) else {
            return media_http_response(
                id,
                404,
                serde_json::json!({"ok": false, "error": "unknown clip operation"}),
            );
        };
        let frames_registered = op.frames.len();
        if frames_sent != usize::MAX && frames_sent != frames_registered {
            return media_http_response(
                id,
                409,
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "clip frame count mismatch: expected {frames_registered}, got {frames_sent}"
                    ),
                }),
            );
        }
        if op.expected_frames > 0 && op.expected_frames != frames_registered {
            return media_http_response(
                id,
                409,
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "clip incomplete: expected {}, got {}",
                        op.expected_frames, frames_registered
                    ),
                }),
            );
        }
        ops.remove(&clip_id).expect("clip op existed")
    };
    let (_, query_ctx) = dashboard_media_session_handles(runtime).await;
    let injected = clip.inject && inject_clip_context(query_ctx.as_ref(), &clip_id, &clip);
    let frames_registered = clip.frames.len();
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!(
            "[clip] {clip_id} - {frames_registered} frames{}",
            if injected {
                " (sent to agent)"
            } else {
                " (saved)"
            }
        ),
        level: Some(LogLevel::Info),
        turn: None,
    });
    media_http_response(
        id,
        200,
        serde_json::json!({
            "t": "clip_saved",
            "ok": true,
            "op_id": clip_id,
            "clip_id": clip_id,
            "frames_registered": frames_registered,
            "injected": injected,
        }),
    )
}

async fn api_media_clip_cancel_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    if clip_id.is_empty() {
        return media_http_response(
            id,
            400,
            serde_json::json!({"ok": false, "error": "missing clip_id"}),
        );
    }
    let existed = runtime
        .media_clip_ops
        .lock()
        .await
        .remove(&clip_id)
        .is_some();
    media_http_response(
        id,
        200,
        serde_json::json!({
            "t": "media_clip_cancelled",
            "ok": true,
            "op_id": clip_id,
            "clip_id": clip_id,
            "existed": existed,
        }),
    )
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

async fn api_session_agent_output_response(
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
    let body_text = params_body_text(Some(&params));
    let response = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_agent_output_post_response(&body_text, &session_id, &source)
    })
    .await;
    match response {
        Ok(response) => http_wire_response(id, response, "session agent output"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({"error": format!("session output task failed: {e}")}).to_string(),
            "session agent output",
        ),
    }
}

async fn api_session_current_history_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, _) = active_history_handles(runtime).await;
    let (status_line, body) = crate::web_gateway::handle_history_get(file_watcher.as_ref()).await;
    http_body_response(id, status_line_code(status_line), body, "session history")
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
    http_body_response(id, status_line_code(status_line), body, "session rollback")
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
        Ok((status_line, body)) => {
            http_body_response(id, status_line_code(status_line), body, "session changes")
        }
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
        Ok((status_line, body)) => {
            http_body_response(id, status_line_code(status_line), body, "context snapshot")
        }
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
                runtime
                    .bus
                    .send(crate::event::AppEvent::UploadDeleted { id });
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

fn transfer_project_root(runtime: &ControlRuntime) -> Result<PathBuf, serde_json::Value> {
    runtime.project_root.clone().ok_or_else(|| {
        serde_json::json!({
            "ok": false,
            "error": "project root unavailable",
        })
    })
}

fn transfer_http_error_response(
    id: String,
    status: u16,
    error: impl Into<String>,
    label: &str,
) -> serde_json::Value {
    http_body_response(
        id,
        status,
        serde_json::json!({
            "ok": false,
            "error": error.into(),
        })
        .to_string(),
        label,
    )
}

fn transfer_store_error_response(
    id: String,
    error: crate::transfer_store::TransferStoreError,
    label: &str,
) -> serde_json::Value {
    transfer_http_error_response(id, error.status, error.message, label)
}

fn transfer_id_param(params: &serde_json::Value) -> String {
    string_param(
        params,
        &[
            "id",
            "job_id",
            "jobId",
            "resume_token",
            "resumeToken",
            "token",
        ],
    )
}

fn transfer_store_task_error(
    error: tokio::task::JoinError,
    label: &str,
) -> crate::transfer_store::TransferStoreError {
    crate::transfer_store::TransferStoreError::new(500, format!("{label} task failed: {error}"))
}

fn transfer_json_error_message(body: &serde_json::Value) -> String {
    body.get("error")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| body.to_string())
}

fn transfer_artifact_type(artifact: &serde_json::Value) -> String {
    string_param(artifact, &["type", "kind", "source_kind", "sourceKind"]).to_ascii_lowercase()
}

async fn transfer_create_download_job_from_params(
    project_root: PathBuf,
    params: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    if let Some(artifact) = params
        .get("artifact")
        .filter(|value| value.is_object())
        .cloned()
    {
        return transfer_create_artifact_download_job(project_root, artifact, runtime).await;
    }
    let path = string_param(&params, &["path", "source_path", "sourcePath", "source"]);
    if path.is_empty() {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing path",
        ));
    }
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_download_job(&project_root, &path)
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "transfer create"))?
}

async fn transfer_create_upload_job_from_params(
    project_root: PathBuf,
    params: serde_json::Value,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let destination = string_param(
        &params,
        &["destination", "destination_path", "destinationPath", "path"],
    );
    if destination.is_empty() {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing destination",
        ));
    }
    let original_name = optional_string_param(
        &params,
        &[
            "name",
            "filename",
            "file_name",
            "fileName",
            "original_name",
            "originalName",
        ],
    )
    .unwrap_or_else(|| "upload.bin".to_string());
    let mime = optional_string_param(&params, &["mime", "content_type", "contentType"])
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let total_size = optional_u64_param(
        &params,
        &[
            "total_size",
            "totalSize",
            "total_bytes",
            "totalBytes",
            "size",
        ],
    )
    .map_err(|error| crate::transfer_store::TransferStoreError::new(400, error))?;
    let conflict = optional_string_param(
        &params,
        &[
            "conflict",
            "conflict_policy",
            "conflictPolicy",
            "if_exists",
            "ifExists",
        ],
    )
    .unwrap_or_else(|| "fail".to_string());
    let conflict_policy =
        crate::transfer_store::TransferConflictPolicy::from_str(&conflict.to_ascii_lowercase())
            .ok_or_else(|| {
                crate::transfer_store::TransferStoreError::new(
                    400,
                    "conflict policy must be fail, rename, or overwrite",
                )
            })?;
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_upload_job(
            &project_root,
            &destination,
            &original_name,
            &mime,
            total_size,
            conflict_policy,
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "transfer create"))?
}

async fn transfer_create_artifact_download_job(
    project_root: PathBuf,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    match transfer_artifact_type(&artifact).as_str() {
        "session_report" | "session-report" => {
            transfer_create_session_report_download_job(project_root, artifact, runtime).await
        }
        "staged_upload" | "staged-upload" | "upload" => {
            transfer_create_staged_upload_download_job(project_root, artifact, runtime).await
        }
        "recording_asset" | "recording-asset" => {
            transfer_create_recording_asset_download_job(project_root, artifact, runtime, false)
                .await
        }
        "session_recording_asset" | "session-recording-asset" => {
            transfer_create_recording_asset_download_job(project_root, artifact, runtime, true)
                .await
        }
        "session_frame_asset" | "session-frame-asset" | "frame_asset" | "frame-asset" => {
            transfer_create_session_frame_download_job(project_root, artifact).await
        }
        "" => Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing artifact type",
        )),
        other => Err(crate::transfer_store::TransferStoreError::new(
            400,
            format!("unsupported transfer artifact type: {other}"),
        )),
    }
}

async fn transfer_create_session_report_download_job(
    project_root: PathBuf,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let session_id = optional_string_param(&artifact, &["session_id", "sessionId", "id"])
        .unwrap_or_else(|| "current".to_string());
    let (session_log, query_ctx) = {
        let session = runtime.shared_session.read().await;
        (session.session_log.clone(), session.query_ctx.clone())
    };
    let report = tokio::task::spawn_blocking({
        let session_id = session_id.clone();
        move || {
            crate::web_gateway::session_report_zip_for_request(
                &session_id,
                session_log.as_ref(),
                query_ctx.as_ref(),
            )
        }
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "session report transfer"))?
    .map_err(|err| {
        let (status, message) = match err {
            crate::web_gateway::SessionReportZipError::InvalidSessionId => {
                (400, "invalid session id".to_string())
            }
            crate::web_gateway::SessionReportZipError::NotFound => {
                (404, "Session not found".to_string())
            }
            crate::web_gateway::SessionReportZipError::Build(error) => {
                (500, format!("Failed to build report: {error}"))
            }
        };
        crate::transfer_store::TransferStoreError::new(status, message)
    })?;
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_download_job_from_bytes(
            &project_root,
            report.bytes,
            &report.filename,
            "application/zip",
            "session_report",
            Some("Session report".to_string()),
            Some(artifact),
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "session report transfer"))?
}

async fn transfer_create_staged_upload_download_job(
    project_root: PathBuf,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let upload_id = transfer_id_param(&artifact);
    if upload_id.is_empty() {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing upload id",
        ));
    }
    let (upload_root, session_dir) = active_upload_handles(runtime)
        .await
        .map_err(|error| crate::transfer_store::TransferStoreError::new(500, error))?;
    let upload_root = upload_root.unwrap_or_else(|| project_root.clone());
    let session_dir =
        session_dir.unwrap_or_else(|| crate::web_gateway::pending_upload_session_dir(&upload_root));
    tokio::task::spawn_blocking(move || {
        let descriptor = crate::upload_store::find_upload(&upload_id, &session_dir, &upload_root)
            .ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(404, "upload not found")
        })?;
        crate::transfer_store::create_download_job_from_path(
            &project_root,
            descriptor.path.clone(),
            Some(descriptor.name.clone()),
            Some(descriptor.mime.clone()),
            Some("staged_upload".to_string()),
            Some(format!("Staged upload {}", descriptor.name)),
            Some(artifact),
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "staged upload transfer"))?
}

async fn transfer_create_recording_asset_download_job(
    project_root: PathBuf,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
    session_scoped: bool,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let stream_name = optional_string_param(&artifact, &["stream_name", "streamName", "stream"])
        .ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(400, "missing stream_name")
        })?;
    if !recording_stream_name_is_safe(&stream_name) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid stream_name",
        ));
    }
    let asset =
        optional_string_param(&artifact, &["asset", "filename", "path"]).ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(400, "missing recording asset")
        })?;
    if !recording_asset_name_is_safe(&asset) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid recording asset",
        ));
    }
    let resolved = if session_scoped {
        let session_id = string_param(&artifact, &["session_id", "sessionId", "id"]);
        if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
            return Err(crate::transfer_store::TransferStoreError::new(
                400,
                "invalid session id",
            ));
        }
        let session_dir = crate::web_gateway::resolve_session_dir(&session_id);
        resolve_session_recording_asset(session_dir, &stream_name, &asset)
    } else {
        let Some(registry) = active_recording_registry(runtime).await else {
            return Err(crate::transfer_store::TransferStoreError::new(
                404,
                "recording registry unavailable",
            ));
        };
        resolve_live_recording_asset(registry, &stream_name, &asset).await
    }
    .map_err(|(status, body)| {
        crate::transfer_store::TransferStoreError::new(status, transfer_json_error_message(&body))
    })?;
    transfer_create_recording_asset_job(project_root, artifact, stream_name, asset, resolved).await
}

async fn transfer_create_recording_asset_job(
    project_root: PathBuf,
    artifact: serde_json::Value,
    stream_name: String,
    asset: String,
    resolved: RecordingAsset,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    match resolved {
        RecordingAsset::Bytes {
            bytes,
            content_type,
            filename,
        } => tokio::task::spawn_blocking(move || {
            crate::transfer_store::create_download_job_from_bytes(
                &project_root,
                bytes,
                &filename,
                content_type,
                "recording_asset",
                Some(format!("{stream_name} {asset}")),
                Some(artifact),
            )
        })
        .await
        .map_err(|e| transfer_store_task_error(e, "recording artifact transfer"))?,
        RecordingAsset::File {
            path,
            content_type,
            filename,
        } => tokio::task::spawn_blocking(move || {
            crate::transfer_store::create_download_job_from_path(
                &project_root,
                path,
                Some(filename),
                Some(content_type.to_string()),
                Some("recording_asset".to_string()),
                Some(format!("{stream_name} {asset}")),
                Some(artifact),
            )
        })
        .await
        .map_err(|e| transfer_store_task_error(e, "recording artifact transfer"))?,
    }
}

async fn transfer_create_session_frame_download_job(
    project_root: PathBuf,
    artifact: serde_json::Value,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let session_id = string_param(&artifact, &["session_id", "sessionId", "id"]);
    if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid session id",
        ));
    }
    let filename = optional_string_param(&artifact, &["filename", "frame", "asset", "name"])
        .ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(400, "missing frame filename")
        })?;
    if !session_frame_filename_is_safe(&filename) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid frame filename",
        ));
    }
    let session_dir = crate::web_gateway::resolve_session_dir(&session_id)
        .ok_or_else(|| crate::transfer_store::TransferStoreError::new(404, "session not found"))?;
    let path = session_dir.join("frames").join(&filename);
    if !path.exists() {
        return Err(crate::transfer_store::TransferStoreError::new(
            404,
            "frame not found",
        ));
    }
    let content_type = if filename.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_download_job_from_path(
            &project_root,
            path,
            Some(filename.clone()),
            Some(content_type.to_string()),
            Some("session_frame_asset".to_string()),
            Some(format!("{session_id} {filename}")),
            Some(artifact),
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "session frame transfer"))?
}

async fn api_transfer_jobs_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let project_root = match transfer_project_root(runtime) {
        Ok(project_root) => project_root,
        Err(body) => return http_body_response(id, 404, body.to_string(), "transfer jobs"),
    };
    let result =
        tokio::task::spawn_blocking(move || crate::transfer_store::list_jobs(&project_root)).await;
    let jobs = match result {
        Ok(jobs) => jobs,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("transfer jobs task failed: {e}"),
            });
        }
    };
    http_body_response(
        id,
        200,
        serde_json::json!({
            "ok": true,
            "jobs": jobs,
        })
        .to_string(),
        "transfer jobs",
    )
}

async fn api_transfer_job_create_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let project_root = match transfer_project_root(runtime) {
        Ok(project_root) => project_root,
        Err(body) => return http_body_response(id, 404, body.to_string(), "transfer create"),
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let kind = string_param(&params, &["kind", "type"]);
    let kind = match crate::transfer_store::TransferKind::from_str(&kind.to_ascii_lowercase()) {
        Some(kind) => kind,
        None => {
            return transfer_http_error_response(
                id,
                400,
                "transfer kind must be download or upload",
                "transfer create",
            );
        }
    };
    let result = match kind {
        crate::transfer_store::TransferKind::Download => {
            transfer_create_download_job_from_params(project_root, params, runtime).await
        }
        crate::transfer_store::TransferKind::Upload => {
            transfer_create_upload_job_from_params(project_root, params).await
        }
    };
    match result {
        Ok(job) => http_body_response(
            id,
            200,
            serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            "transfer create",
        ),
        Err(error) => transfer_store_error_response(id, error, "transfer create"),
    }
}

async fn api_transfer_job_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let project_root = match transfer_project_root(runtime) {
        Ok(project_root) => project_root,
        Err(body) => return http_body_response(id, 404, body.to_string(), "transfer delete"),
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let job_id = transfer_id_param(&params);
    if job_id.is_empty() {
        return transfer_http_error_response(id, 400, "missing id", "transfer delete");
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::delete_job(&project_root, &job_id)
    })
    .await;
    match result {
        Ok(Ok(deleted)) => http_body_response(
            id,
            200,
            serde_json::json!({
                "ok": true,
                "deleted": deleted,
            })
            .to_string(),
            "transfer delete",
        ),
        Ok(Err(error)) => transfer_store_error_response(id, error, "transfer delete"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("transfer delete task failed: {e}"),
        }),
    }
}

async fn api_transfer_upload_commit_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let project_root = match transfer_project_root(runtime) {
        Ok(project_root) => project_root,
        Err(body) => {
            return http_body_response(id, 404, body.to_string(), "transfer upload commit")
        }
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let job_id = transfer_id_param(&params);
    if job_id.is_empty() {
        return transfer_http_error_response(id, 400, "missing id", "transfer upload commit");
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::commit_upload_job(&project_root, &job_id)
    })
    .await;
    match result {
        Ok(Ok(job)) => http_body_response(
            id,
            200,
            serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            "transfer upload commit",
        ),
        Ok(Err(error)) => transfer_store_error_response(id, error, "transfer upload commit"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("transfer upload commit task failed: {e}"),
        }),
    }
}

async fn api_transfer_upload_chunk_task_response(
    id: String,
    upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let project_root = match transfer_project_root(&runtime) {
        Ok(project_root) => project_root,
        Err(body) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(id, 404, body.to_string(), "transfer upload chunk"),
                byte_stream: None,
                done: true,
            };
        }
    };
    let job_id = transfer_id_param(&upload.params);
    if job_id.is_empty() {
        return ControlTaskResponse {
            id: id.clone(),
            frame: transfer_http_error_response(id, 400, "missing id", "transfer upload chunk"),
            byte_stream: None,
            done: true,
        };
    }
    let offset = match optional_u64_param(&upload.params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: transfer_http_error_response(id, 400, error, "transfer upload chunk"),
                byte_stream: None,
                done: true,
            };
        }
    };
    let chunk_len = upload.received_bytes as u64;
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::append_upload_tempfile(
            &project_root,
            &job_id,
            offset,
            upload.tmp,
            chunk_len,
        )
    })
    .await;
    let frame = match result {
        Ok(Ok(job)) => http_body_response(
            id.clone(),
            200,
            serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            "transfer upload chunk",
        ),
        Ok(Err(error)) => transfer_store_error_response(id.clone(), error, "transfer upload chunk"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("transfer upload chunk task failed: {e}"),
        }),
    };
    ControlTaskResponse {
        id,
        frame,
        byte_stream: None,
        done: true,
    }
}

async fn api_transfer_download_read_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let project_root = match transfer_project_root(runtime) {
        Ok(project_root) => project_root,
        Err(body) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(id, 404, body.to_string(), "transfer download"),
                byte_stream: None,
                done: true,
            };
        }
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let job_id = transfer_id_param(&params);
    if job_id.is_empty() {
        return transfer_download_error_task_response(id, 400, "missing id");
    }
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => return transfer_download_error_task_response(id, 400, error),
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(value) => value,
        Err(error) => return transfer_download_error_task_response(id, 400, error),
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::read_download_range(
            &project_root,
            &job_id,
            offset,
            length,
            crate::web_gateway::UPLOAD_MAX_BYTES as u64,
        )
    })
    .await;
    let (job, bytes, end) = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            return transfer_download_error_task_response(id, error.status, error.message);
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("transfer download task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let content_type = job
        .mime
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let filename = job.filename.clone();
    let total_size = job.total_size.unwrap_or(bytes.len() as u64);
    let size = bytes.len();
    let source_path = job
        .source_path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let job_value = serde_json::to_value(&job).unwrap_or_else(|_| serde_json::json!({}));
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:transfer-download"),
            content_type: content_type.clone(),
            filename: filename.clone(),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "id": job.id,
                "resume_token": job.resume_token,
                "path": source_path,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
                "completed_bytes": job.completed_bytes,
                "status": job.status,
                "job": job_value,
            }),
        }),
        done: true,
    }
}

fn transfer_download_error_task_response(
    id: String,
    status: u16,
    error: impl Into<String>,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: transfer_http_error_response(id, status, error, "transfer download"),
        byte_stream: None,
        done: true,
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

async fn api_fs_read_task_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let path_param = string_param(&params, &["path"]);
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return filesystem_read_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(value) => value,
        Err(error) => {
            return filesystem_read_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let path = match crate::web_gateway::expand_dashboard_fs_path(&path_param) {
        Ok(path) => path,
        Err(error) => {
            return filesystem_read_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty());
    let content_type = dashboard_fs_content_type(&path);
    let read_result = tokio::task::spawn_blocking({
        let path = path.clone();
        move || read_dashboard_fs_file_range(&path, offset, length)
    })
    .await;
    let (bytes, total_size, end, display_path) = match read_result {
        Ok(Ok(value)) => value,
        Ok(Err((status, body))) => return filesystem_read_error_task_response(id, status, body),
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("filesystem read task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let size = bytes.len();
    let stream_name = display_path.to_string_lossy().to_string();
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:fs-read"),
            content_type: content_type.clone(),
            filename: filename.clone(),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "path": stream_name,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
            }),
        }),
        done: true,
    }
}

fn dashboard_fs_content_type(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("css") => "text/css; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("md") | Some("markdown") | Some("txt") | Some("toml") | Some("yaml") | Some("yml") => {
            "text/plain; charset=utf-8"
        }
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("wasm") => "application/wasm",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn read_dashboard_fs_file_range(
    path: &std::path::Path,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64, PathBuf), (u16, serde_json::Value)> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        (
            404,
            serde_json::json!({ "ok": false, "error": format!("file not accessible: {e}") }),
        )
    })?;
    if !metadata.is_file() {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "path is not a regular file" }),
        ));
    }
    let total_size = metadata.len();
    let (start, transfer_len, end) = filesystem_read_range(total_size, offset, length)?;
    let mut file = std::fs::File::open(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("open file: {e}") }),
        )
    })?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("seek file: {e}") }),
        )
    })?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("read file: {e}") }),
        )
    })?;
    let display = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok((bytes, total_size, end, display))
}

fn filesystem_read_range(
    total_size: u64,
    offset: u64,
    length: Option<u64>,
) -> Result<(u64, usize, u64), (u16, serde_json::Value)> {
    if offset > total_size {
        return Err((
            416,
            serde_json::json!({
                "ok": false,
                "error": "range start beyond file size",
                "total_size": total_size,
            }),
        ));
    }
    let available = total_size.saturating_sub(offset);
    let requested = length.unwrap_or(available).min(available);
    if requested > crate::web_gateway::UPLOAD_MAX_BYTES as u64 {
        return Err((
            413,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "range too large: {} bytes (cap is {})",
                    requested,
                    crate::web_gateway::UPLOAD_MAX_BYTES
                ),
            }),
        ));
    }
    let transfer_len = usize::try_from(requested).map_err(|_| {
        (
            413,
            serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
        )
    })?;
    Ok((offset, transfer_len, offset.saturating_add(requested)))
}

fn filesystem_read_error_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: http_body_response(id, status, body.to_string(), "filesystem read"),
        byte_stream: None,
        done: true,
    }
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

async fn api_recording_asset_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let (stream_name, asset, offset, length) = match recording_asset_request_params(&params) {
        Ok(params) => params,
        Err((status, error)) => {
            return recording_asset_error_task_response(id, status, error);
        }
    };
    let Some(registry) = active_recording_registry(runtime).await else {
        return recording_asset_error_task_response(
            id,
            404,
            serde_json::json!({ "ok": false, "error": "recording registry unavailable" }),
        );
    };
    let resolved = resolve_live_recording_asset(registry, &stream_name, &asset).await;
    recording_asset_task_response(id, stream_name, asset, offset, length, resolved).await
}

async fn api_session_recording_asset_task_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
        return recording_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "invalid session id" }),
        );
    }
    let (stream_name, asset, offset, length) = match recording_asset_request_params(&params) {
        Ok(params) => params,
        Err((status, error)) => {
            return recording_asset_error_task_response(id, status, error);
        }
    };
    let session_dir = crate::web_gateway::resolve_session_dir(&session_id);
    let resolved = resolve_session_recording_asset(session_dir, &stream_name, &asset);
    recording_asset_task_response(id, stream_name, asset, offset, length, resolved).await
}

fn recording_asset_request_params(
    params: &serde_json::Value,
) -> Result<(String, String, u64, Option<u64>), (u16, serde_json::Value)> {
    let Some(stream_name) = optional_string_param(params, &["stream_name", "streamName", "stream"])
    else {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "missing stream_name" }),
        ));
    };
    if !recording_stream_name_is_safe(&stream_name) {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "invalid stream_name" }),
        ));
    }
    let Some(asset) = optional_string_param(params, &["asset", "filename", "path"]) else {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "missing recording asset" }),
        ));
    };
    if !recording_asset_name_is_safe(&asset) {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "invalid recording asset" }),
        ));
    }
    let offset = optional_u64_param(params, &["offset", "start"])
        .map_err(|error| (400, serde_json::json!({ "ok": false, "error": error })))?
        .unwrap_or(0);
    let length = optional_u64_param(params, &["length", "limit"])
        .map_err(|error| (400, serde_json::json!({ "ok": false, "error": error })))?;
    Ok((stream_name, asset, offset, length))
}

fn recording_stream_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 128
        && name.trim() == name
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn recording_asset_name_is_safe(asset: &str) -> bool {
    asset == "segments" || asset == "playlist.m3u8" || (recording_segment_filename_is_safe(asset))
}

fn recording_segment_filename_is_safe(filename: &str) -> bool {
    filename.starts_with("seg_")
        && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
        && filename.len() < 30
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
}

enum RecordingAsset {
    Bytes {
        bytes: Vec<u8>,
        content_type: &'static str,
        filename: String,
    },
    File {
        path: PathBuf,
        content_type: &'static str,
        filename: String,
    },
}

async fn resolve_live_recording_asset(
    registry: Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>,
    stream_name: &str,
    asset: &str,
) -> Result<RecordingAsset, (u16, serde_json::Value)> {
    let (session_dir, mut segments) = {
        let reg = registry.read().await;
        (reg.session_dir().to_path_buf(), reg.segments(stream_name))
    };
    if segments.is_empty() {
        let stream_dir = crate::debug::daemon_recordings_dir().join(stream_name);
        segments =
            crate::recording::parse_segment_csv_pub(&stream_dir.join("segments.csv"), &stream_dir);
    }
    resolve_recording_asset_from_dir_pair(
        Some(session_dir.join("recordings").join(stream_name)),
        Some(crate::debug::daemon_recordings_dir().join(stream_name)),
        segments,
        asset,
    )
}

fn resolve_session_recording_asset(
    session_dir: Option<PathBuf>,
    stream_name: &str,
    asset: &str,
) -> Result<RecordingAsset, (u16, serde_json::Value)> {
    let stream_dir = session_dir
        .as_ref()
        .map(|dir| dir.join("recordings").join(stream_name));
    let segments = stream_dir
        .as_ref()
        .map(|dir| crate::recording::parse_segment_csv_pub(&dir.join("segments.csv"), dir))
        .unwrap_or_default();
    resolve_recording_asset_from_dir_pair(stream_dir, None, segments, asset)
}

fn resolve_recording_asset_from_dir_pair(
    primary_dir: Option<PathBuf>,
    fallback_dir: Option<PathBuf>,
    segments: Vec<crate::recording::SegmentInfo>,
    asset: &str,
) -> Result<RecordingAsset, (u16, serde_json::Value)> {
    if asset == "segments" {
        let seg_json: Vec<serde_json::Value> = segments
            .iter()
            .map(|s| {
                serde_json::json!({
                    "filename": s.filename,
                    "start_secs": s.start_secs,
                    "end_secs": s.end_secs,
                })
            })
            .collect();
        let bytes = serde_json::to_vec(&seg_json).unwrap_or_else(|_| b"[]".to_vec());
        return Ok(RecordingAsset::Bytes {
            bytes,
            content_type: "application/json",
            filename: "segments.json".to_string(),
        });
    }
    if asset == "playlist.m3u8" {
        return Ok(RecordingAsset::Bytes {
            bytes: crate::web_gateway::recording_playlist_m3u8(&segments).into_bytes(),
            content_type: "application/vnd.apple.mpegurl",
            filename: "playlist.m3u8".to_string(),
        });
    }
    if !recording_segment_filename_is_safe(asset) {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "invalid recording asset" }),
        ));
    }
    let path = primary_dir
        .as_ref()
        .map(|dir| dir.join(asset))
        .filter(|path| path.exists())
        .or_else(|| {
            fallback_dir
                .as_ref()
                .map(|dir| dir.join(asset))
                .filter(|path| path.exists())
        });
    let Some(path) = path else {
        return Err((
            404,
            serde_json::json!({ "ok": false, "error": "recording asset not found" }),
        ));
    };
    let content_type = if asset.ends_with(".ts") {
        "video/mp2t"
    } else {
        "video/mp4"
    };
    Ok(RecordingAsset::File {
        path,
        content_type,
        filename: asset.to_string(),
    })
}

async fn recording_asset_task_response(
    id: String,
    stream_name: String,
    asset_name: String,
    offset: u64,
    length: Option<u64>,
    resolved: Result<RecordingAsset, (u16, serde_json::Value)>,
) -> ControlTaskResponse {
    let resolved_asset = match resolved {
        Ok(asset) => asset,
        Err((status, body)) => return recording_asset_error_task_response(id, status, body),
    };
    let read_result = match resolved_asset {
        RecordingAsset::Bytes {
            bytes,
            content_type,
            filename,
        } => {
            tokio::task::spawn_blocking(move || {
                read_recording_asset_bytes_range(bytes, offset, length).map(
                    |(bytes, total_size, end)| (bytes, total_size, end, content_type, filename),
                )
            })
            .await
        }
        RecordingAsset::File {
            path,
            content_type,
            filename,
        } => {
            tokio::task::spawn_blocking(move || {
                read_recording_asset_file_range(&path, offset, length).map(
                    |(bytes, total_size, end)| (bytes, total_size, end, content_type, filename),
                )
            })
            .await
        }
    };
    let (bytes, total_size, end, content_type, filename) = match read_result {
        Ok(Ok(value)) => value,
        Ok(Err((status, body))) => return recording_asset_error_task_response(id, status, body),
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("recording asset task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let size = bytes.len();
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:recording:{stream_name}:{asset_name}"),
            content_type: content_type.to_string(),
            filename: Some(filename.clone()),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "stream_name": stream_name,
                "asset": asset_name,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
            }),
        }),
        done: true,
    }
}

fn read_recording_asset_bytes_range(
    bytes: Vec<u8>,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), (u16, serde_json::Value)> {
    let total_size = bytes.len() as u64;
    let (start, transfer_len, end) = recording_asset_range(total_size, offset, length)?;
    let start = usize::try_from(start).map_err(|_| {
        (
            413,
            serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
        )
    })?;
    Ok((bytes[start..start + transfer_len].to_vec(), total_size, end))
}

fn read_recording_asset_file_range(
    path: &std::path::Path,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), (u16, serde_json::Value)> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("stat recording asset: {e}") }),
        )
    })?;
    let total_size = metadata.len();
    let (start, transfer_len, end) = recording_asset_range(total_size, offset, length)?;
    let mut file = std::fs::File::open(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("open recording asset: {e}") }),
        )
    })?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("seek recording asset: {e}") }),
        )
    })?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("read recording asset: {e}") }),
        )
    })?;
    Ok((bytes, total_size, end))
}

fn recording_asset_range(
    total_size: u64,
    offset: u64,
    length: Option<u64>,
) -> Result<(u64, usize, u64), (u16, serde_json::Value)> {
    if offset > total_size {
        return Err((
            416,
            serde_json::json!({
                "ok": false,
                "error": "range start beyond recording asset size",
                "total_size": total_size,
            }),
        ));
    }
    let available = total_size.saturating_sub(offset);
    let requested = length.unwrap_or(available).min(available);
    if requested > crate::web_gateway::UPLOAD_MAX_BYTES as u64 {
        return Err((
            413,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "range too large: {} bytes (cap is {})",
                    requested,
                    crate::web_gateway::UPLOAD_MAX_BYTES
                ),
            }),
        ));
    }
    let transfer_len = usize::try_from(requested).map_err(|_| {
        (
            413,
            serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
        )
    })?;
    Ok((offset, transfer_len, offset.saturating_add(requested)))
}

fn recording_asset_error_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: http_body_response(id, status, body.to_string(), "recording asset"),
        byte_stream: None,
        done: true,
    }
}

async fn api_session_frame_asset_task_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
        return session_frame_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "invalid session id" }),
        );
    }
    let Some(filename) = optional_string_param(&params, &["filename", "frame", "asset", "name"])
    else {
        return session_frame_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "missing frame filename" }),
        );
    };
    if !session_frame_filename_is_safe(&filename) {
        return session_frame_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "invalid frame filename" }),
        );
    }
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return session_frame_asset_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(value) => value,
        Err(error) => {
            return session_frame_asset_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };

    let Some(session_dir) = crate::web_gateway::resolve_session_dir(&session_id) else {
        return session_frame_asset_error_task_response(
            id,
            404,
            serde_json::json!({ "ok": false, "error": "session not found" }),
        );
    };
    let path = session_dir.join("frames").join(&filename);
    if !path.exists() {
        return session_frame_asset_error_task_response(
            id,
            404,
            serde_json::json!({ "ok": false, "error": "frame not found" }),
        );
    }
    let content_type = if filename.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    let read_result =
        tokio::task::spawn_blocking(move || read_frame_asset_file_range(&path, offset, length))
            .await;
    let (bytes, total_size, end) = match read_result {
        Ok(Ok(value)) => value,
        Ok(Err((status, body))) => {
            return session_frame_asset_error_task_response(id, status, body)
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("session frame task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let size = bytes.len();
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:session-frame:{session_id}:{filename}"),
            content_type: content_type.to_string(),
            filename: Some(filename.clone()),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "session_id": session_id,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
            }),
        }),
        done: true,
    }
}

fn session_frame_filename_is_safe(filename: &str) -> bool {
    (filename.ends_with(".jpg") || filename.ends_with(".png"))
        && filename.len() < 80
        && !filename.is_empty()
        && filename.trim() == filename
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
}

fn read_frame_asset_file_range(
    path: &std::path::Path,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), (u16, serde_json::Value)> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("stat session frame: {e}") }),
        )
    })?;
    let total_size = metadata.len();
    let (start, transfer_len, end) = recording_asset_range(total_size, offset, length)?;
    let mut file = std::fs::File::open(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("open session frame: {e}") }),
        )
    })?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("seek session frame: {e}") }),
        )
    })?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("read session frame: {e}") }),
        )
    })?;
    Ok((bytes, total_size, end))
}

fn session_frame_asset_error_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: http_body_response(id, status, body.to_string(), "session frame asset"),
        byte_stream: None,
        done: true,
    }
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
    frames.extend(display_ready_bootstrap_frames(runtime).await);
    let mut replayed_external_session_ids = HashSet::new();
    if let Some(frame) =
        response_result(api_session_log_replay_response("bootstrap-replay".into(), runtime).await)
    {
        if let Some(external_session_id) = frame
            .get("external_session_id")
            .and_then(|value| value.as_str())
        {
            replayed_external_session_ids.insert(external_session_id.to_string());
        }
        frames.push(frame);
    }
    frames.extend(external_session_activity_replay_frames(
        runtime,
        &replayed_external_session_ids,
    ));
    frames.extend(display_authority_snapshot_frames(runtime).await);
    let frame_count = frames.len();
    let omitted = dashboard_bootstrap_omitted(runtime);

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
            "omitted": omitted,
        },
    })
}

async fn api_display_bootstrap_response(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let mut frames = display_ready_bootstrap_frames(runtime).await;
    frames.extend(display_authority_snapshot_frames(runtime).await);
    let frame_count = frames.len();
    let omitted = display_bootstrap_omitted(runtime);
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
            "omitted": omitted,
        },
    })
}

async fn api_display_webrtc_signal_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let signal = string_param(&params, &["signal", "kind", "type", "t"]);
    match signal.as_str() {
        "offer" | "display_offer" => api_display_webrtc_offer_response(id, &params, runtime).await,
        "ice" | "candidate" | "display_ice" => {
            api_display_webrtc_ice_response(id, &params, runtime).await
        }
        _ => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "missing or unknown display webrtc signal",
        }),
    }
}

async fn api_display_webrtc_offer_response(
    id: String,
    params: &serde_json::Value,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let display_id = display_id_param(Some(params));
    let sdp = string_param(params, &["sdp", "offer", "offer_sdp"]);
    if sdp.is_empty() {
        return missing_param_response(id, "sdp");
    }
    let Some(display_session) = active_display_session(runtime, display_id).await else {
        return display_signal_error_response(id, 404, display_id, "display session not found");
    };

    let (ice_tx, mut ice_rx) = mpsc::channel::<(crate::display::PeerId, String)>(64);
    if let Some(control_frames_tx) = runtime.control_frames_tx.clone() {
        tokio::spawn(async move {
            while let Some((_peer_id, candidate_json)) = ice_rx.recv().await {
                let candidate =
                    serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default();
                let payload = serde_json::json!({
                    "t": "display_ice",
                    "display_id": display_id,
                    "candidate": candidate,
                });
                let frame = serde_json::json!({
                    "t": "event",
                    "payload": payload,
                });
                if control_frames_tx.send(frame).is_err() {
                    break;
                }
            }
        });
    }

    let input_authorized = dashboard_display_input_authorizer(
        runtime.display_authority.clone(),
        runtime.session_id.clone(),
        display_id,
    );
    let authority_handler = crate::display::webrtc::noop_authority_handler();
    match display_session
        .handle_offer(
            runtime.display_peer_id,
            &sdp,
            &runtime.ice_config,
            Some(Arc::clone(&runtime.tcp_peer_registry)),
            None,
            ice_tx,
            input_authorized,
            authority_handler,
        )
        .await
    {
        Ok(answer_sdp) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": {
                "t": "display_answer",
                "display_id": display_id,
                "sdp": answer_sdp,
            },
        }),
        Err(e) => display_signal_error_response(
            id,
            502,
            display_id,
            &format!("display offer failed: {e}"),
        ),
    }
}

async fn api_display_webrtc_ice_response(
    id: String,
    params: &serde_json::Value,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let display_id = display_id_param(Some(params));
    let Some(candidate) = params.get("candidate").cloned() else {
        return missing_param_response(id, "candidate");
    };
    if candidate.is_null() {
        return missing_param_response(id, "candidate");
    }
    let Some(display_session) = active_display_session(runtime, display_id).await else {
        return display_signal_error_response(id, 404, display_id, "display session not found");
    };
    let candidate = candidate.to_string();
    let peer_id = runtime.display_peer_id;
    tokio::spawn(async move {
        if let Err(e) = display_session.add_ice_candidate(peer_id, &candidate).await {
            eprintln!(
                "[dashboard/control] display ICE candidate failed for display {display_id}: {e}"
            );
        }
    });
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "display_id": display_id,
        },
    })
}

async fn active_display_session(
    runtime: &ControlRuntime,
    display_id: u32,
) -> Option<Arc<crate::display::DisplaySession>> {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    }?;
    let registry = session_registry.read().await;
    registry.get(display_id)
}

fn dashboard_display_input_authorizer(
    display_authority: Option<DashboardDisplayAuthorityBridge>,
    session_id: String,
    display_id: u32,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || match display_authority.as_ref() {
        Some(bridge) => bridge.input_authorized(&session_id, display_id),
        None => true,
    })
}

fn display_signal_error_response(
    id: String,
    status: u16,
    display_id: u32,
    error: &str,
) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "status": status,
        "display_id": display_id,
        "error": error,
    })
}

async fn api_display_input_authority_snapshot_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let frames = display_authority_snapshot_frames(runtime).await;
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "available": runtime.display_authority.is_some(),
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

async fn api_display_input_authority_request_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return display_authority_unavailable_response(id);
    };
    let display_id = display_id_param(params);
    let frames = bridge.request(&runtime.session_id, display_id);
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "display_id": display_id,
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

async fn api_display_input_authority_release_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return display_authority_unavailable_response(id);
    };
    let display_id = display_id_param(params);
    let frames = bridge.release(&runtime.session_id, display_id);
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "display_id": display_id,
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

fn display_authority_unavailable_response(id: String) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": false,
            "available": false,
            "_httpStatus": 503,
            "_httpOk": false,
            "error": "display input authority unavailable",
        },
    })
}

fn display_id_param(params: Option<&serde_json::Value>) -> u32 {
    params
        .and_then(|params| {
            params
                .get("display_id")
                .or_else(|| params.get("displayId"))
                .or_else(|| params.get("id"))
        })
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

async fn display_authority_snapshot_frames(runtime: &ControlRuntime) -> Vec<serde_json::Value> {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return Vec::new();
    };
    let display_ids = active_display_ids(runtime).await;
    bridge.snapshot(&runtime.session_id, &display_ids)
}

fn dashboard_bootstrap_omitted(runtime: &ControlRuntime) -> Vec<&'static str> {
    if runtime.display_authority.is_some() {
        Vec::new()
    } else {
        vec!["display_input_authority_state"]
    }
}

fn display_bootstrap_omitted(runtime: &ControlRuntime) -> Vec<&'static str> {
    if runtime.display_authority.is_some() {
        Vec::new()
    } else {
        vec!["display_input_authority_state"]
    }
}

async fn display_ready_bootstrap_frames(runtime: &ControlRuntime) -> Vec<serde_json::Value> {
    let display_ids = active_display_ids(runtime).await;
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    let Some(session_registry) = session_registry else {
        return Vec::new();
    };

    let registry = session_registry.read().await;
    display_ids
        .into_iter()
        .filter_map(|display_id| {
            registry.get(display_id).map(|session| {
                let (width, height) = session.resolution();
                serde_json::json!({
                    "event": "display_ready",
                    "display_id": display_id,
                    "width": width,
                    "height": height,
                })
            })
        })
        .collect()
}

async fn active_display_ids(runtime: &ControlRuntime) -> Vec<u32> {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    let Some(session_registry) = session_registry else {
        return Vec::new();
    };

    let registry = session_registry.read().await;
    let mut display_ids = registry.display_ids();
    display_ids.sort_unstable();
    display_ids
}

async fn api_external_session_activity_replay_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let frames = external_session_activity_replay_frames(runtime, &HashSet::new());
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

fn external_session_activity_replay_frames(
    runtime: &ControlRuntime,
    skip_session_ids: &HashSet<String>,
) -> Vec<serde_json::Value> {
    let mut active_external_sessions: Vec<(String, String)> = runtime
        .bootstrap_caches
        .attached_external_sessions
        .lock()
        .ok()
        .map(|guard| {
            guard
                .iter()
                .map(|(session_id, source)| (session_id.clone(), source.clone()))
                .collect()
        })
        .unwrap_or_default();
    active_external_sessions.sort_by(|a, b| a.0.cmp(&b.0));
    active_external_sessions
        .into_iter()
        .filter(|(session_id, _)| !skip_session_ids.contains(session_id))
        .filter_map(|(session_id, source)| {
            crate::web_gateway::external_session_activity_replay_for_websocket(&source, &session_id)
                .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
        })
        .collect()
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
    query_ctx
        .as_ref()
        .map(|ctx| ctx.log_dir.clone())
        .or_else(|| {
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
        let body =
            crate::web_gateway::scan_worktree_inventory_response(&home, project_root.as_deref());
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
        Ok((status_line, body)) => {
            http_body_response(id, status_line_code(status_line), body, "worktree remove")
        }
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
    let ctrl = match dashboard_control_msg_from_params(id.clone(), params) {
        Ok(ctrl) => ctrl,
        Err(response) => return response,
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

async fn api_session_control_msg_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let ctrl = match dashboard_control_msg_from_params(id.clone(), params) {
        Ok(ctrl) => ctrl,
        Err(response) => return response,
    };
    if !dashboard_session_control_msg_allowed(&ctrl) {
        return http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "control action not available over dashboard session WebRTC: {}",
                    dashboard_control_msg_action(&ctrl)
                ),
            })
            .to_string(),
            "session control message",
        );
    }
    let action = dashboard_control_msg_action(&ctrl);
    dispatch_dashboard_control_msg(&runtime.bus, ctrl, "session-control");
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

async fn api_dashboard_action_msg_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let ctrl = match dashboard_control_msg_from_params(id.clone(), params) {
        Ok(ctrl) => ctrl,
        Err(response) => return response,
    };
    if !dashboard_action_msg_allowed(&ctrl) {
        return http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "control action not available over dashboard action WebRTC: {}",
                    dashboard_control_msg_action(&ctrl)
                ),
            })
            .to_string(),
            "dashboard action message",
        );
    }
    let action = dashboard_control_msg_action(&ctrl);
    let marker_apply = match &ctrl {
        ControlMsg::SetDiagnosticsVisualMarker {
            display_id,
            enabled,
        } => {
            let display_id = display_id.unwrap_or(0);
            Some((
                display_id,
                apply_dashboard_diagnostics_visual_marker(runtime, display_id, *enabled).await,
            ))
        }
        _ => None,
    };
    dispatch_dashboard_control_msg(&runtime.bus, ctrl, "dashboard-action");
    let mut result = serde_json::json!({
        "ok": true,
        "action": action,
    });
    if let Some((display_id, marker_result)) = marker_apply {
        if let Some(result_obj) = result.as_object_mut() {
            result_obj.insert("display_id".to_string(), serde_json::json!(display_id));
            result_obj.insert(
                "registry_available".to_string(),
                serde_json::json!(marker_result.registry_available),
            );
            result_obj.insert(
                "active_display_updated".to_string(),
                serde_json::json!(marker_result.active_display_updated),
            );
        }
    }
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": result,
    })
}

async fn api_diagnostics_visual_freshness_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return missing_param_response(id, "session_id");
    }
    let body = params
        .get("body")
        .or_else(|| params.get("ndjson"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .unwrap_or_default();
    if body.is_empty() {
        return missing_param_response(id, "body");
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::diagnostics::append_visual_freshness_record(&session_id, body.as_bytes())
    })
    .await;
    let (status, body) = match result {
        Ok(Ok(written)) => (
            200,
            serde_json::json!({"ok": true, "written": written}).to_string(),
        ),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidInput => {
            (400, serde_json::json!({"error": e.to_string()}).to_string())
        }
        Ok(Err(e)) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
        Err(e) => (
            500,
            serde_json::json!({"error": format!("diagnostics append task failed: {e}")})
                .to_string(),
        ),
    };
    http_body_response(id, status, body, "diagnostics visual freshness")
}

fn dashboard_control_msg_from_params(
    id: String,
    params: Option<&serde_json::Value>,
) -> Result<ControlMsg, serde_json::Value> {
    let Some(params) = params else {
        return Err(missing_param_response(id, "message"));
    };
    let message = params
        .get("message")
        .or_else(|| params.get("control_msg"))
        .or_else(|| params.get("controlMsg"))
        .cloned()
        .unwrap_or_else(|| params.clone());
    serde_json::from_value::<ControlMsg>(message).map_err(|e| {
        http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!("invalid control message: {e}"),
            })
            .to_string(),
            "control message",
        )
    })
}

fn dispatch_dashboard_control_msg(bus: &crate::event::EventBus, ctrl: ControlMsg, scope: &str) {
    let action = dashboard_control_msg_action(&ctrl);
    bus.send(AppEvent::PresenceLog {
        message: format!("[dashboard-control:{scope}] ControlMsg: {action}"),
        level: Some(crate::types::LogLevel::Debug),
        turn: None,
    });
    bus.send(AppEvent::ControlCommand(ctrl));
}

#[derive(Debug, Clone, Copy)]
struct DiagnosticsVisualMarkerApply {
    registry_available: bool,
    active_display_updated: bool,
}

async fn apply_dashboard_diagnostics_visual_marker(
    runtime: &ControlRuntime,
    display_id: u32,
    enabled: bool,
) -> DiagnosticsVisualMarkerApply {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    let Some(session_registry) = session_registry else {
        eprintln!(
            "[dashboard/control] diagnostics visual marker request for display {display_id} ({enabled}) ignored; no session registry"
        );
        return DiagnosticsVisualMarkerApply {
            registry_available: false,
            active_display_updated: false,
        };
    };

    let active_display_updated = session_registry
        .write()
        .await
        .set_diagnostics_visual_marker(display_id, enabled);
    eprintln!(
        "[dashboard/control] diagnostics visual marker for display {display_id} = {enabled}{}",
        if active_display_updated {
            ""
        } else {
            " (pending)"
        },
    );
    DiagnosticsVisualMarkerApply {
        registry_available: true,
        active_display_updated,
    }
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

fn dashboard_session_control_msg_allowed(ctrl: &ControlMsg) -> bool {
    matches!(
        ctrl,
        ControlMsg::Approve { .. }
            | ControlMsg::Deny { .. }
            | ControlMsg::Skip { .. }
            | ControlMsg::ApproveAll { .. }
            | ControlMsg::RenameSession { .. }
            | ControlMsg::ConfigureSessionAgent { .. }
            | ControlMsg::StopSession { .. }
            | ControlMsg::RestartSession { .. }
            | ControlMsg::CreateSession { .. }
            | ControlMsg::StartTask { .. }
            | ControlMsg::ResumeSession { .. }
            | ControlMsg::FollowUp { .. }
            | ControlMsg::CancelFollowUp { .. }
            | ControlMsg::EditUserMessage { .. }
            | ControlMsg::Interrupt { .. }
            | ControlMsg::Steer { .. }
            | ControlMsg::CancelSteer { .. }
    )
}

fn dashboard_action_msg_allowed(ctrl: &ControlMsg) -> bool {
    matches!(
        ctrl,
        ControlMsg::CodexThreadAction { .. }
            | ControlMsg::GeminiThreadAction { .. }
            | ControlMsg::TakeDisplay { .. }
            | ControlMsg::ReleaseDisplay { .. }
            | ControlMsg::GrantUserDisplay { .. }
            | ControlMsg::RevokeUserDisplay { .. }
            | ControlMsg::CreateBrowserWorkspace { .. }
            | ControlMsg::CloseBrowserWorkspace { .. }
            | ControlMsg::AcquireBrowserWorkspace { .. }
            | ControlMsg::ReleaseBrowserWorkspace { .. }
            | ControlMsg::SetupDebugScreen
            | ControlMsg::TeardownDebugScreen
            | ControlMsg::StartDebugRecording
            | ControlMsg::StopDebugRecording
            | ControlMsg::StartRecording { .. }
            | ControlMsg::StopRecording { .. }
            | ControlMsg::DeleteRecording { .. }
            | ControlMsg::SetDiagnosticsVisualMarker { .. }
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
        ControlMsg::PeerFileTransferSignal { .. } => "peer_file_transfer_signal",
        ControlMsg::PeerDashboardControlSignal { .. } => "peer_dashboard_control_signal",
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

async fn api_peer_webrtc_signal_response(
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
        crate::web_gateway::peers_webrtc_signal(registry, &peer_id, &body_text, &runtime.bus).await;
    http_body_response(id, status, body, "peer webrtc signal")
}

async fn api_peer_file_transfer_signal_response(
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
    let (status, body) = crate::web_gateway::peers_file_transfer_signal(
        registry,
        &peer_id,
        &body_text,
        &runtime.bus,
    )
    .await;
    http_body_response(id, status, body, "peer file-transfer signal")
}

async fn api_peer_dashboard_control_signal_response(
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
    let (status, body) = crate::web_gateway::peers_dashboard_control_signal(
        registry,
        &peer_id,
        &body_text,
        &runtime.bus,
    )
    .await;
    http_body_response(id, status, body, "peer dashboard-control signal")
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

fn control_session_detail_before(params: &serde_json::Value) -> Option<usize> {
    for name in ["before", "page_before", "pageBefore"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        if value.is_null() {
            return None;
        }
        if let Some(number) = value.as_u64() {
            return usize::try_from(number).ok();
        }
        if let Some(text) = value.as_str() {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            return text.parse::<usize>().ok();
        }
        return None;
    }
    None
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

fn optional_u64_param(params: &serde_json::Value, names: &[&str]) -> Result<Option<u64>, String> {
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

fn new_control_ice_fragment() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect()
}

fn new_control_ice_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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

fn tcp_host_candidate_init(addr: SocketAddr) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:9001 1 tcp 1677721855 {} {} typ host tcptype passive generation 0",
            addr.ip(),
            addr.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
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
            terminal_registry: Arc::new(crate::terminal::TerminalRegistry::new(
                std::env::temp_dir(),
            )),
            web_tui_tx: None,
            task_tx: None,
            bootstrap_caches: DashboardBootstrapCaches::default(),
            display_authority: None,
            presence: None,
            ice_config: crate::display::IceConfig::default(),
            tcp_peer_registry: crate::display::webrtc::TcpPeerRegistry::new(),
            media_clip_ops: Arc::new(Mutex::new(HashMap::new())),
            control_frames_tx: None,
            display_peer_id: 1,
            grant: DashboardControlGrant::TrustedLocal,
        }
    }

    struct DashboardControlStubDisplayBackend;

    #[async_trait::async_trait]
    impl crate::display::DisplayBackend for DashboardControlStubDisplayBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<crate::display::Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }

        async fn stop_capture(&self) {}

        async fn inject_input(
            &self,
            _event: crate::display::InputEvent,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }

        fn kind(&self) -> &'static str {
            "dashboard-control-stub"
        }
    }

    fn test_control_frame_response(
        text: &str,
        runtime: &mut ControlRuntime,
        task_tx: &mpsc::Sender<ControlTaskResponse>,
        pending_requests: &mut HashMap<String, CancellationToken>,
        outbound_queue: &mut OutboundControlQueue,
    ) -> Option<serde_json::Value> {
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let mut tui_connections: HashMap<String, DashboardTuiConnection> = HashMap::new();
        control_frame_response(
            text,
            runtime,
            task_tx,
            pending_requests,
            outbound_queue,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
    }

    fn test_upload_state(
        method: &str,
        params: serde_json::Value,
        bytes: &[u8],
    ) -> InboundUploadState {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.as_file_mut().write_all(bytes).unwrap();
        tmp.as_file_mut().flush().unwrap();
        InboundUploadState {
            method: method.to_string(),
            params,
            tmp,
            total_bytes: bytes.len(),
            expected_chunks: if bytes.is_empty() { 0 } else { 1 },
            next_seq: if bytes.is_empty() { 0 } else { 1 },
            received_bytes: bytes.len(),
        }
    }

    #[test]
    fn binding_signature_payload_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let identity = DaemonIdentity::load_or_create(dir.path().join("identity.pk8")).unwrap();
        let binding = DashboardControlBinding::new(
            &identity,
            "session-1".into(),
            "offer",
            "answer",
            None,
            None,
        );
        assert!(crate::daemon_identity::verify_b64u(
            &binding.daemon_public_key,
            binding.signing_payload().as_bytes(),
            &binding.signature,
        ));
        assert_eq!(binding.protocol, CONTROL_SIGNATURE_CONTEXT);
        assert_eq!(binding.offer_sha256, sha256_b64u(b"offer"));
        assert_eq!(binding.answer_sha256, sha256_b64u(b"answer"));
        assert!(binding.expires_unix_ms > binding.created_unix_ms);
        assert_eq!(
            binding.expires_unix_ms - binding.created_unix_ms,
            CONTROL_BINDING_TTL_MS
        );
        assert_eq!(binding.client_nonce, None);
        assert_eq!(binding.session_grant_sha256, None);

        let granted = DashboardControlBinding::new(
            &identity,
            "session-2".into(),
            "offer-2",
            "answer-2",
            Some("connect-session-grant"),
            Some("browser-client-nonce"),
        );
        let expected_grant_hash = sha256_b64u(b"connect-session-grant");
        assert_eq!(
            granted.client_nonce.as_deref(),
            Some("browser-client-nonce")
        );
        assert_eq!(
            granted.session_grant_sha256.as_deref(),
            Some(expected_grant_hash.as_str())
        );
        assert!(granted.signing_payload().ends_with(
            granted
                .session_grant_sha256
                .as_deref()
                .expect("grant hash should be present")
        ));
        assert!(crate::daemon_identity::verify_b64u(
            &granted.daemon_public_key,
            granted.signing_payload().as_bytes(),
            &granted.signature,
        ));
    }

    #[test]
    fn peer_dashboard_grants_split_access_and_peer_permissions() {
        let (tx, _rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut peer_root = runtime();
        peer_root.grant = DashboardControlGrant::Peer {
            fingerprint: "fingerprint".into(),
            label: "peer-root".into(),
            profile: "peer-root".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy::default(),
        };

        let status = test_control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(status["ok"], true);
        assert_eq!(status["result"]["access_inspect_available"], true);
        assert_eq!(status["result"]["access_manage_available"], false);
        assert_eq!(status["result"]["peer_inspect_available"], true);
        assert_eq!(status["result"]["peer_manage_available"], true);
        assert_eq!(status["result"]["api_access_overview_available"], true);
        assert_eq!(status["result"]["api_dashboard_targets_available"], true);
        assert_eq!(
            status["result"]["api_peer_pairing_invite_available"],
            false
        );
        assert_eq!(status["result"]["api_peer_pairing_join_available"], true);
        assert_eq!(
            status["result"]["api_peer_pairing_request_decision_available"],
            false
        );
        assert_eq!(
            status["result"]["api_peer_pairing_identity_revoke_available"],
            false
        );

        let overview = test_control_frame_response(
            r#"{"t":"request","id":"a1","method":"api_access_overview"}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(overview["ok"], true);

        let revoke = test_control_frame_response(
            r#"{"t":"request","id":"r1","method":"api_peer_pairing_identity_revoke","params":{"identity":"peer-a"}}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(revoke["ok"], false);
        assert!(revoke["error"]
            .as_str()
            .unwrap_or("")
            .contains("not allowed for profile peer-root"));

        let invite = test_control_frame_response(
            r#"{"t":"request","id":"i1","method":"api_peer_pairing_invite","params":{}}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(invite["ok"], false);
        assert!(invite["error"]
            .as_str()
            .unwrap_or("")
            .contains("not allowed for profile peer-root"));

        let mut peer_operator = runtime();
        peer_operator.grant = DashboardControlGrant::Peer {
            fingerprint: "fingerprint".into(),
            label: "peer-operator".into(),
            profile: "peer-operator".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy::default(),
        };
        let denied = test_control_frame_response(
            r#"{"t":"request","id":"a2","method":"api_access_overview"}"#,
            &mut peer_operator,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(denied["ok"], false);
        assert!(denied["error"]
            .as_str()
            .unwrap_or("")
            .contains("not allowed for profile peer-operator"));
    }

    #[tokio::test]
    async fn control_frames_answer_hello_ping_and_config() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let hello = test_control_frame_response(
            r#"{"t":"hello","id":"h1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert_eq!(hello["session_id"], "session-1");

        let ping = test_control_frame_response(
            r#"{"t":"ping","id":"p1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(ping["t"], "pong");
        assert_eq!(ping["id"], "p1");

        let config = test_control_frame_response(
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

        let card = test_control_frame_response(
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
        let cached_bootstrap = test_control_frame_response(
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
        assert_eq!(cached_bootstrap["result"]["events"][0]["event"], "status");
        assert_eq!(
            cached_bootstrap["result"]["events"][1]["event"],
            "autonomy_changed"
        );

        let status = test_control_frame_response(
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
        assert_eq!(status["result"]["api_display_bootstrap_available"], true);
        assert_eq!(
            status["result"]["api_display_webrtc_signal_available"],
            true
        );
        assert_eq!(status["result"]["api_session_log_replay_available"], true);
        assert_eq!(
            status["result"]["api_external_session_activity_replay_available"],
            true
        );
        assert_eq!(status["result"]["api_dashboard_bootstrap_available"], true);
        assert_eq!(status["result"]["byte_streams_available"], true);
        assert_eq!(status["result"]["upload_frames_available"], true);
        assert_eq!(status["result"]["presence_frames_available"], true);
        assert_eq!(status["result"]["presence_active_handoff_available"], false);
        assert_eq!(status["result"]["presence_tool_request_available"], true);
        assert_eq!(status["result"]["access_inspect_available"], true);
        assert_eq!(status["result"]["access_manage_available"], true);
        assert_eq!(status["result"]["peer_inspect_available"], true);
        assert_eq!(status["result"]["peer_manage_available"], true);
        assert_eq!(status["result"]["api_presence_video_frame_available"], true);
        assert_eq!(status["result"]["api_sessions_available"], true);
        assert_eq!(status["result"]["api_sessions_stream_available"], true);
        assert_eq!(status["result"]["api_session_detail_available"], true);
        assert_eq!(status["result"]["api_session_report_available"], true);
        assert_eq!(status["result"]["api_session_delete_available"], true);
        assert_eq!(status["result"]["api_session_agent_output_available"], true);
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
            status["result"]["api_session_current_uploads_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_raw_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_delete_available"],
            true
        );
        assert_eq!(status["result"]["api_transfer_jobs_available"], false);
        assert_eq!(status["result"]["api_transfer_job_create_available"], false);
        assert_eq!(status["result"]["api_transfer_job_delete_available"], false);
        assert_eq!(
            status["result"]["api_transfer_download_read_available"],
            false
        );
        assert_eq!(
            status["result"]["api_transfer_upload_chunk_available"],
            false
        );
        assert_eq!(
            status["result"]["api_transfer_upload_commit_available"],
            false
        );
        assert_eq!(status["result"]["api_fs_stat_available"], true);
        assert_eq!(status["result"]["api_fs_list_available"], true);
        assert_eq!(status["result"]["api_fs_mkdir_available"], true);
        assert_eq!(status["result"]["api_fs_read_available"], true);
        assert_eq!(status["result"]["api_sessions_search_available"], true);
        assert_eq!(status["result"]["api_settings_available"], true);
        assert_eq!(status["result"]["api_settings_save_available"], false);
        assert_eq!(status["result"]["api_control_msg_available"], true);
        assert_eq!(status["result"]["api_session_control_msg_available"], true);
        assert_eq!(status["result"]["api_dashboard_action_msg_available"], true);
        assert_eq!(
            status["result"]["api_diagnostics_visual_freshness_available"],
            true
        );
        assert_eq!(status["result"]["api_key_status_available"], true);
        assert_eq!(status["result"]["api_api_keys_save_available"], true);
        assert_eq!(status["result"]["api_voice_session_available"], true);
        assert_eq!(status["result"]["api_project_root_available"], true);
        assert_eq!(status["result"]["api_displays_available"], true);
        assert_eq!(status["result"]["api_recordings_available"], true);
        assert_eq!(status["result"]["api_recording_asset_available"], true);
        assert_eq!(status["result"]["api_session_recordings_available"], true);
        assert_eq!(
            status["result"]["api_session_recording_asset_available"],
            true
        );
        assert_eq!(status["result"]["api_worktrees_available"], true);
        assert_eq!(status["result"]["api_worktrees_scan_available"], true);
        assert_eq!(status["result"]["api_worktrees_remove_available"], true);
        assert_eq!(status["result"]["api_mcp_tool_call_available"], false);
        assert_eq!(status["result"]["api_peer_mutations_available"], false);
        assert_eq!(status["result"]["api_peer_webrtc_signal_available"], false);
        assert_eq!(status["result"]["api_peer_pairing_available"], true);
        assert_eq!(
            status["result"]["api_peer_pairing_invite_available"],
            true
        );
        assert_eq!(status["result"]["api_peer_pairing_join_available"], true);
        assert_eq!(
            status["result"]["api_peer_pairing_request_access_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_request_decision_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_requests_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_identities_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_identity_revoke_available"],
            true
        );
        assert_eq!(status["result"]["api_coordinator_available"], false);

        let peers = test_control_frame_response(
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

        let subscribed = test_control_frame_response(
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

        let project_root = test_control_frame_response(
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

        let queued = test_control_frame_response(
            r#"{"t":"request","id":"q1","method":"api_sessions","params":{"limit":1}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("q1"));
        let cancelled = test_control_frame_response(
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
    async fn presence_frame_routes_voice_log() {
        let mut rt = runtime();
        let mut events = rt.bus.subscribe();
        let (task_tx, _task_rx) = mpsc::channel(1);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let ack = test_control_frame_response(
            r#"{"t":"presence_frame","id":"p1","frame":{"t":"voice_log","text":"hello from connect","seq":7,"tool_context":"debug"}}"#,
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
        )
        .expect("presence frame should ack when id is present");

        assert_eq!(ack["t"], "presence_ack");
        assert_eq!(ack["id"], "p1");
        assert_eq!(ack["ok"], true);

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("voice log event should arrive")
            .expect("event bus should be open");
        match event {
            AppEvent::VoiceLog {
                text,
                seq,
                tool_context,
            } => {
                assert_eq!(text, "hello from connect");
                assert_eq!(seq, 7);
                assert_eq!(tool_context.as_deref(), Some("debug"));
            }
            other => panic!("expected VoiceLog, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn presence_frame_routes_tool_request_response() {
        let mut rt = runtime();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        rt.control_frames_tx = Some(control_tx);
        let (task_tx, _task_rx) = mpsc::channel(1);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let ack = test_control_frame_response(
            r#"{"t":"presence_frame","id":"p1","frame":{"t":"tool_request","id":"req_1","tool":"check_status","args":{}}}"#,
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
        )
        .expect("presence frame should ack when id is present");

        assert_eq!(ack["t"], "presence_ack");
        assert_eq!(ack["id"], "p1");
        assert_eq!(ack["ok"], true);

        let frame = tokio::time::timeout(Duration::from_secs(1), control_rx.recv())
            .await
            .expect("tool response event should arrive")
            .expect("control frame channel should stay open");
        assert_eq!(frame["t"], "event");
        let payload = &frame["payload"];
        assert_eq!(payload["t"], "tool_response");
        assert_eq!(payload["id"], "req_1");
        assert!(!payload["result"].as_str().unwrap_or("").is_empty());
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
        assert!(rejected["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available over dashboard WebRTC"));
    }

    #[tokio::test]
    async fn api_session_control_msg_dispatches_lifecycle_actions_only() {
        let rt = runtime();
        let mut events = rt.bus.subscribe();
        let response = api_session_control_msg_response(
            "session-ctrl1".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "interrupt",
                    "session_id": "session-a",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["action"], "interrupt");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::Interrupt { session_id, .. }) = event {
                assert_eq!(session_id.as_deref(), Some("session-a"));
                saw_control = true;
                break;
            }
        }
        assert!(saw_control, "session control message did not reach the bus");

        let accepted_create = api_session_control_msg_response(
            "session-ctrl2".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "create_session",
                    "task": "noop",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(accepted_create["t"], "response");
        assert_eq!(accepted_create["ok"], true);
        assert_eq!(accepted_create["result"]["ok"], true);
        assert_eq!(accepted_create["result"]["action"], "create_session");

        let rejected_settings = api_session_control_msg_response(
            "session-ctrl3".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_codex_sandbox",
                    "mode": "workspace-write",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(rejected_settings["t"], "response");
        assert_eq!(rejected_settings["ok"], true);
        assert_eq!(rejected_settings["result"]["ok"], false);
        assert_eq!(rejected_settings["result"]["_httpStatus"], 400);
        assert!(rejected_settings["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available over dashboard session WebRTC"));
    }

    #[tokio::test]
    async fn api_dashboard_action_msg_dispatches_small_dashboard_actions_only() {
        let rt = runtime();
        let mut events = rt.bus.subscribe();
        let response = api_dashboard_action_msg_response(
            "dash-action1".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "close_browser_workspace",
                    "workspace_id": "workspace-a",
                    "reason": "test",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["action"], "close_browser_workspace");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::CloseBrowserWorkspace {
                workspace_id,
                ..
            }) = event
            {
                assert_eq!(workspace_id, "workspace-a");
                saw_control = true;
                break;
            }
        }
        assert!(
            saw_control,
            "dashboard action message did not reach the bus"
        );

        let accepted_thread = api_dashboard_action_msg_response(
            "dash-action2".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "codex_thread_action",
                    "session_id": "session-a",
                    "op": "new",
                    "params": {},
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(accepted_thread["t"], "response");
        assert_eq!(accepted_thread["ok"], true);
        assert_eq!(accepted_thread["result"]["action"], "codex_thread_action");

        let rejected_settings = api_dashboard_action_msg_response(
            "dash-action3".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_codex_sandbox",
                    "mode": "workspace-write",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(rejected_settings["t"], "response");
        assert_eq!(rejected_settings["ok"], true);
        assert_eq!(rejected_settings["result"]["ok"], false);
        assert_eq!(rejected_settings["result"]["_httpStatus"], 400);
        assert!(rejected_settings["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available over dashboard action WebRTC"));
    }

    #[tokio::test]
    async fn api_diagnostics_visual_freshness_appends_ndjson_batch() {
        let session_id = format!("dashboard-control-test-vf-{}", std::process::id());
        if let Some(path) = crate::diagnostics::visual_freshness_path(&session_id) {
            let _ = std::fs::remove_file(&path);
        }
        let ndjson = "{\"t\":\"session_start\"}\n{\"t\":\"summary\"}\n";
        let response = api_diagnostics_visual_freshness_response(
            "diag-vf".to_string(),
            Some(&serde_json::json!({
                "session_id": session_id.clone(),
                "body": ndjson,
            })),
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["_httpStatus"], 200);
        assert_eq!(response["result"]["written"], ndjson.len());

        let path =
            crate::diagnostics::visual_freshness_path(&session_id).expect("diagnostics path");
        let written = std::fs::read_to_string(&path).expect("diagnostics transcript");
        assert_eq!(written, ndjson);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn api_dashboard_action_msg_applies_diagnostics_visual_marker_to_display_registry() {
        let rt = runtime();
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let display_session = Arc::new(crate::display::DisplaySession::new(
            2,
            Arc::new(DashboardControlStubDisplayBackend),
        ));
        registry
            .write()
            .await
            .insert(2, Arc::clone(&display_session));
        {
            let mut session = rt.shared_session.write().await;
            session.session_registry = Some(Arc::clone(&registry));
        }

        let response = api_dashboard_action_msg_response(
            "dash-action-marker".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_diagnostics_visual_marker",
                    "display_id": 2,
                    "enabled": true,
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(
            response["result"]["action"],
            "set_diagnostics_visual_marker"
        );
        assert_eq!(response["result"]["display_id"], 2);
        assert_eq!(response["result"]["registry_available"], true);
        assert_eq!(response["result"]["active_display_updated"], true);
        assert!(
            display_session.diagnostics_visual_marker_enabled(),
            "dashboard-control RPC did not toggle the live display session"
        );
    }

    #[tokio::test]
    async fn control_frame_routes_session_control_msg_requests() {
        let mut rt = runtime();
        let mut events = rt.bus.subscribe();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let immediate = test_control_frame_response(
            r#"{"t":"request","id":"session-ctrl-frame","method":"api_session_control_msg","params":{"message":{"action":"interrupt","session_id":"session-frame"}}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(immediate.is_none(), "session control request should spawn");

        let task = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.id, "session-ctrl-frame");
        assert!(task.done);
        assert_eq!(task.frame["t"], "response");
        assert_eq!(task.frame["ok"], true);
        assert_eq!(task.frame["result"]["action"], "interrupt");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::Interrupt { session_id, .. }) = event {
                assert_eq!(session_id.as_deref(), Some("session-frame"));
                saw_control = true;
                break;
            }
        }
        assert!(
            saw_control,
            "frame-routed session control did not reach bus"
        );
    }

    #[tokio::test]
    async fn control_frame_routes_dashboard_action_msg_requests() {
        let mut rt = runtime();
        let mut events = rt.bus.subscribe();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let immediate = test_control_frame_response(
            r#"{"t":"request","id":"dash-action-frame","method":"api_dashboard_action_msg","params":{"message":{"action":"close_browser_workspace","workspace_id":"workspace-frame"}}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(immediate.is_none(), "dashboard action request should spawn");

        let task = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.id, "dash-action-frame");
        assert!(task.done);
        assert_eq!(task.frame["t"], "response");
        assert_eq!(task.frame["ok"], true);
        assert_eq!(task.frame["result"]["action"], "close_browser_workspace");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::CloseBrowserWorkspace {
                workspace_id,
                ..
            }) = event
            {
                assert_eq!(workspace_id, "workspace-frame");
                saw_control = true;
                break;
            }
        }
        assert!(
            saw_control,
            "frame-routed dashboard action did not reach bus"
        );
    }

    #[tokio::test]
    async fn current_agent_output_without_active_log_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = test_control_frame_response(
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
    async fn session_report_rpc_returns_zip_for_active_log() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-report");
        let log = crate::session_log::SessionLog::open(session_dir.clone()).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{\"ok\":true}\n").unwrap();
        std::fs::create_dir_all(session_dir.join("turns")).unwrap();
        std::fs::write(
            session_dir.join("turns").join("turn_001_stdout.txt"),
            "hello\n",
        )
        .unwrap();

        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.session_log = Some(Arc::new(std::sync::Mutex::new(log)));
        }
        let report = api_session_report_task_response(
            "report1".to_string(),
            Some(&serde_json::json!({})),
            &rt,
        )
        .await;
        assert!(report.done);
        assert_eq!(report.id, "report1");
        assert!(report.byte_stream.is_some());
        let stream = report.byte_stream.unwrap();
        assert_eq!(stream.id, "report1");
        assert_eq!(stream.stream_id, "report1:session-report");
        assert_eq!(stream.content_type, "application/zip");
        assert!(stream.filename.as_deref().unwrap_or("").ends_with(".zip"));
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["content_type"], "application/zip");
        assert!(stream.result["filename"]
            .as_str()
            .unwrap_or("")
            .ends_with(".zip"));
        assert_eq!(
            stream.result["size"].as_u64().unwrap(),
            stream.bytes.len() as u64
        );
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(stream.bytes)).unwrap();
        assert!(zip.by_name("summary.json").is_ok());
        assert!(zip.by_name("turns/turn_001_stdout.txt").is_ok());

        let invalid = api_session_report_task_response(
            "report2".to_string(),
            Some(&serde_json::json!({ "session_id": "../bad" })),
            &rt,
        )
        .await;
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
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
            let queued =
                test_control_frame_response(&frame, &mut rt, &tx, &mut pending, &mut outbound);
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

        let queued = test_control_frame_response(
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
        assert_eq!(
            missing_selector["result"]["error"],
            "missing snapshot selector"
        );
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
    async fn current_uploads_lists_pending_uploads() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        {
            let mut session = rt.shared_session.write().await;
            session.project_root_for_changes = Some(project.path().to_path_buf());
        }
        let bytes = b"dashboard list upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "listed.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();

        let response = api_session_current_uploads_response("uploads1".to_string(), &rt).await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        let uploads = response["result"].as_array().expect("uploads array");
        assert!(
            uploads.iter().any(|upload| upload["id"] == descriptor.id),
            "upload list did not include committed descriptor: {response}"
        );
    }

    #[tokio::test]
    async fn peer_webrtc_signal_returns_http_error_metadata() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(8);
        let mut rt = runtime();
        rt.peer_registry = Some(crate::peer::PeerRegistry::new(log_tx));

        let params = serde_json::json!({
            "peer_id": "missing-peer",
            "display_id": 0,
            "session_id": "dashboard-test-session",
            "signal": { "kind": "close" },
        });
        let response =
            api_peer_webrtc_signal_response("webrtc1".to_string(), Some(&params), &rt).await;

        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["_httpOk"], false);
        assert_eq!(response["result"]["_httpStatus"], 404);
        assert_eq!(response["result"]["error"], "peer not found");
    }

    #[tokio::test]
    async fn current_upload_raw_streams_requested_range() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let bytes = b"dashboard raw upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "raw.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();

        let response = api_session_current_upload_raw_task_response(
            "raw1".to_string(),
            Some(&serde_json::json!({
                "id": descriptor.id,
                "offset": 10,
                "length": 6,
            })),
            &rt,
        )
        .await;
        assert!(response.done);
        assert_eq!(response.id, "raw1");
        assert!(response.byte_stream.is_some());
        let stream = response.byte_stream.unwrap();
        assert_eq!(stream.id, "raw1");
        assert_eq!(stream.stream_id, format!("raw1:upload:{}", descriptor.id));
        assert_eq!(stream.content_type, "text/plain");
        assert_eq!(stream.filename.as_deref(), Some("raw.txt"));
        assert_eq!(stream.bytes, &bytes[10..16]);
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["id"], descriptor.id);
        assert_eq!(stream.result["name"], "raw.txt");
        assert_eq!(stream.result["filename"], "raw.txt");
        assert_eq!(stream.result["mime"], "text/plain");
        assert_eq!(stream.result["content_type"], "text/plain");
        assert_eq!(stream.result["size"], 6);
        assert_eq!(stream.result["total_size"], bytes.len());
        assert_eq!(stream.result["offset"], 10);
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 16);
        assert_eq!(stream.result["resumable"], true);

        let invalid = api_session_current_upload_raw_task_response(
            "raw2".to_string(),
            Some(&serde_json::json!({
                "id": descriptor.id,
                "offset": bytes.len() + 1,
                "length": 1,
            })),
            &rt,
        )
        .await;
        assert!(invalid.done);
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["t"], "response");
        assert_eq!(invalid.frame["ok"], true);
        assert_eq!(invalid.frame["result"]["_httpStatus"], 416);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
        assert_eq!(
            invalid.frame["result"]["error"],
            "range start beyond upload size"
        );
    }

    #[tokio::test]
    async fn upload_frames_commit_pending_upload() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let mut events = rt.bus.subscribe();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let mut tui_connections = HashMap::new();
        let bytes = b"hello upload";
        let first = &bytes[..6];
        let second = &bytes[6..];

        let start = serde_json::json!({
            "t": "upload_start",
            "id": "up1",
            "method": "api_session_current_upload",
            "params": {
                "name": "note.txt",
                "mime": "text/plain",
                "destination": "task",
            },
            "encoding": "base64",
            "total_bytes": bytes.len(),
            "chunks": 2,
        });
        assert!(control_frame_response(
            &start.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());
        assert!(pending.contains_key("up1"));

        for (seq, chunk) in [first, second].into_iter().enumerate() {
            let frame = serde_json::json!({
                "t": "upload_chunk",
                "id": "up1",
                "seq": seq,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            });
            assert!(control_frame_response(
                &frame.to_string(),
                &mut rt,
                &tx,
                &mut pending,
                &mut outbound,
                &mut inbound_uploads,
                &terminal_tx,
                &mut terminal_forwarders,
                &mut tui_connections,
            )
            .is_none());
        }

        let end = serde_json::json!({
            "t": "upload_end",
            "id": "up1",
            "chunks": 2,
        });
        assert!(control_frame_response(
            &end.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());

        let response = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "up1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["_httpOk"], true);
        assert_eq!(response.frame["result"]["name"], "note.txt");
        assert_eq!(response.frame["result"]["mime"], "text/plain");
        assert_eq!(response.frame["result"]["size"], bytes.len());
        let path = response.frame["result"]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            AppEvent::UploadReady { descriptor } => {
                assert_eq!(descriptor.name, "note.txt");
                assert_eq!(descriptor.size, bytes.len() as u64);
            }
            other => panic!("expected upload ready event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn media_annotation_upload_registers_frame() {
        let session_dir = tempfile::tempdir().unwrap();
        let rt = runtime();
        let registry = Arc::new(tokio::sync::RwLock::new(crate::frames::FrameRegistry::new(
            session_dir.path(),
        )));
        {
            let mut session = rt.shared_session.write().await;
            session.frame_registry = Some(registry.clone());
        }
        let bytes = b"jpeg annotation bytes";
        let upload = test_upload_state(
            "api_media_annotation_submit",
            serde_json::json!({
                "frame_id": "ann-test-1",
                "stream": "annotation",
                "note": "look here",
                "inject": false,
            }),
            bytes,
        );

        let response =
            api_media_annotation_upload_task_response("ann1".into(), upload, rt.clone(), true)
                .await;

        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["t"], "annotation_saved");
        assert_eq!(response.frame["result"]["ok"], true);
        assert_eq!(response.frame["result"]["frame_id"], "ann-test-1");
        assert_eq!(response.frame["result"]["injected"], false);
        let stored = registry.read().await.read_hq("ann-test-1").unwrap();
        assert_eq!(stored, bytes);
    }

    #[tokio::test]
    async fn media_clip_operation_commits_ordered_frames() {
        let session_dir = tempfile::tempdir().unwrap();
        let rt = runtime();
        let registry = Arc::new(tokio::sync::RwLock::new(crate::frames::FrameRegistry::new(
            session_dir.path(),
        )));
        {
            let mut session = rt.shared_session.write().await;
            session.frame_registry = Some(registry.clone());
        }

        let start = api_media_clip_start_response(
            "clip-start".into(),
            Some(&serde_json::json!({
                "clip_id": "clip-test-1",
                "stream": "recording",
                "fps": 2,
                "total_frames": 1,
                "inject": false,
            })),
            &rt,
        )
        .await;
        assert_eq!(start["result"]["_httpStatus"], 200);
        assert_eq!(start["result"]["t"], "media_clip_started");

        let bytes = b"jpeg clip frame";
        let frame_upload = test_upload_state(
            "api_media_clip_frame",
            serde_json::json!({
                "clip_id": "clip-test-1",
                "frame_id": "clip-test-1-f000",
                "frame_index": 0,
            }),
            bytes,
        );
        let frame = api_media_clip_frame_upload_task_response(
            "clip-frame".into(),
            frame_upload,
            rt.clone(),
        )
        .await;
        assert_eq!(frame.frame["result"]["_httpStatus"], 200);
        assert_eq!(frame.frame["result"]["t"], "media_clip_frame_saved");
        assert_eq!(frame.frame["result"]["frames_received"], 1);
        assert_eq!(
            registry.read().await.read_hq("clip-test-1-f000").unwrap(),
            bytes
        );

        let end = api_media_clip_end_response(
            "clip-end".into(),
            Some(&serde_json::json!({
                "clip_id": "clip-test-1",
                "frames_sent": 1,
            })),
            &rt,
        )
        .await;
        assert_eq!(end["result"]["_httpStatus"], 200);
        assert_eq!(end["result"]["t"], "clip_saved");
        assert_eq!(end["result"]["frames_registered"], 1);
        assert_eq!(end["result"]["injected"], false);
    }

    #[tokio::test]
    async fn upload_frames_commit_zero_byte_upload() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let mut tui_connections = HashMap::new();

        let start = serde_json::json!({
            "t": "upload_start",
            "id": "up-empty",
            "method": "api_session_current_upload",
            "params": {
                "name": "empty.txt",
                "mime": "text/plain",
                "destination": "task",
            },
            "encoding": "base64",
            "total_bytes": 0,
            "chunks": 0,
        });
        assert!(control_frame_response(
            &start.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());
        assert!(pending.contains_key("up-empty"));

        let end = serde_json::json!({
            "t": "upload_end",
            "id": "up-empty",
            "chunks": 0,
        });
        assert!(control_frame_response(
            &end.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());

        let response = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "up-empty");
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["_httpOk"], true);
        assert_eq!(response.frame["result"]["name"], "empty.txt");
        assert_eq!(response.frame["result"]["size"], 0);
        let path = response.frame["result"]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn terminal_frames_open_input_and_forward_output() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.terminal_registry = Arc::new(crate::terminal::TerminalRegistry::new(
            project.path().to_path_buf(),
        ));
        let (task_tx, _task_rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let mut tui_connections = HashMap::new();
        let terminal_id = "dash-control-test-shell";

        let open = serde_json::json!({
            "t": "terminal_open",
            "host_id": "local",
            "terminal_id": terminal_id,
            "cols": 80,
            "rows": 24,
        });
        assert!(control_frame_response(
            &open.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());

        let opened = tokio::time::timeout(Duration::from_secs(3), terminal_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened["t"], "terminal_opened");
        assert_eq!(opened["terminal_id"], terminal_id);

        let token = "dashboard_terminal_frame_ok";
        let input = serde_json::json!({
            "t": "terminal_input",
            "host_id": "local",
            "terminal_id": terminal_id,
            "data": base64::engine::general_purpose::STANDARD
                .encode(format!("printf '{token}\\n'\r").as_bytes()),
        });
        assert!(control_frame_response(
            &input.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_token = false;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), terminal_rx.recv()).await {
                Ok(Some(frame)) if frame["t"] == "terminal_output" => {
                    let data = frame["data"].as_str().unwrap_or("");
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap_or_default();
                    if String::from_utf8_lossy(&bytes).contains(token) {
                        saw_token = true;
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {}
            }
        }
        assert!(
            saw_token,
            "did not receive terminal output over control frames"
        );

        let close = serde_json::json!({
            "t": "terminal_close",
            "host_id": "local",
            "terminal_id": terminal_id,
        });
        let _ = control_frame_response(
            &close.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        );
        for (_, handle) in terminal_forwarders {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn tui_frames_bridge_web_tui_output() {
        let mut rt = runtime();
        let (web_tui_tx, mut web_tui_rx) =
            mpsc::unbounded_channel::<crate::tui::web::WebTuiCommand>();
        rt.web_tui_tx = Some(web_tui_tx);
        let (task_tx, _task_rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let mut tui_connections = HashMap::new();
        let connection_id = "dashboard-tui-test";

        let status = status_response_frame("status1".to_string(), &rt);
        assert_eq!(status["result"]["tui_frames_available"], true);

        let subscribe = serde_json::json!({
            "t": "tui_subscribe",
            "connection_id": connection_id,
            "cols": 100,
            "rows": 30,
        });
        assert!(control_frame_response(
            &subscribe.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        )
        .is_none());

        let command = tokio::time::timeout(Duration::from_secs(1), web_tui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let direct_tx = match command {
            crate::tui::web::WebTuiCommand::AddConnection {
                id,
                direct_tx,
                cols,
                rows,
            } => {
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
                assert!(id.contains(connection_id));
                direct_tx
            }
            _ => panic!("expected AddConnection"),
        };
        direct_tx
            .send(
                serde_json::json!({
                    "t": "term",
                    "d": base64::engine::general_purpose::STANDARD.encode(b"tui frame bytes"),
                })
                .to_string(),
            )
            .unwrap();

        let forwarded = tokio::time::timeout(Duration::from_secs(1), terminal_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forwarded["t"], "tui_term");
        assert_eq!(forwarded["connection_id"], connection_id);
        assert_eq!(forwarded["base64"], forwarded["d"]);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(forwarded["base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"tui frame bytes");

        let close = serde_json::json!({
            "t": "tui_close",
            "connection_id": connection_id,
        });
        let _ = control_frame_response(
            &close.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
            &mut tui_connections,
        );
        assert!(tui_connections.is_empty());
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

        let workspace_snapshot = api_browser_workspace_snapshot_response("bw1".to_string()).await;
        assert_eq!(workspace_snapshot["t"], "response");
        assert_eq!(workspace_snapshot["ok"], true);
        assert_eq!(
            workspace_snapshot["result"]["t"],
            "browser_workspace_snapshot"
        );
        assert!(workspace_snapshot["result"]["workspaces"]
            .as_array()
            .is_some());
    }

    #[tokio::test]
    async fn recording_asset_rpc_streams_segments_and_media_ranges() {
        let session_dir = tempfile::tempdir().unwrap();
        let stream_dir = session_dir.path().join("recordings").join("display_0");
        std::fs::create_dir_all(&stream_dir).unwrap();
        std::fs::write(
            stream_dir.join("segments.csv"),
            "seg_00000.mp4,0,1.25\nseg_00001.ts,1.25,2.00\n",
        )
        .unwrap();
        let media = b"recording segment bytes";
        std::fs::write(stream_dir.join("seg_00000.mp4"), media).unwrap();
        let ts_media = b"recording transport stream bytes";
        std::fs::write(stream_dir.join("seg_00001.ts"), ts_media).unwrap();

        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.recording_registry = Some(Arc::new(tokio::sync::RwLock::new(
                crate::recording::RecordingRegistry::new(
                    session_dir.path(),
                    crate::project::RecordingConfig::default(),
                ),
            )));
        }

        let segments = api_recording_asset_task_response(
            "rec-asset1".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "segments",
            })),
            &rt,
        )
        .await;
        assert!(segments.done);
        assert!(segments.byte_stream.is_some());
        let stream = segments.byte_stream.unwrap();
        assert_eq!(stream.content_type, "application/json");
        assert_eq!(stream.filename.as_deref(), Some("segments.json"));
        let json: serde_json::Value = serde_json::from_slice(&stream.bytes).unwrap();
        assert_eq!(json[0]["filename"], "seg_00000.mp4");
        assert_eq!(json[1]["filename"], "seg_00001.ts");
        assert_eq!(stream.result["stream_name"], "display_0");
        assert_eq!(stream.result["asset"], "segments");
        assert_eq!(stream.result["resumable"], true);

        let playlist = api_recording_asset_task_response(
            "rec-asset-playlist".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "playlist.m3u8",
            })),
            &rt,
        )
        .await;
        assert!(playlist.done);
        assert!(playlist.byte_stream.is_some());
        let stream = playlist.byte_stream.unwrap();
        assert_eq!(stream.content_type, "application/vnd.apple.mpegurl");
        assert_eq!(stream.filename.as_deref(), Some("playlist.m3u8"));
        let playlist_text = String::from_utf8(stream.bytes).unwrap();
        assert!(playlist_text.contains("#EXTM3U"));
        assert!(playlist_text.contains("seg_00001.ts"));

        let segment = api_recording_asset_task_response(
            "rec-asset2".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "seg_00000.mp4",
                "offset": 10,
                "length": 7,
            })),
            &rt,
        )
        .await;
        assert!(segment.done);
        assert!(segment.byte_stream.is_some());
        let stream = segment.byte_stream.unwrap();
        assert_eq!(
            stream.stream_id,
            "rec-asset2:recording:display_0:seg_00000.mp4"
        );
        assert_eq!(stream.content_type, "video/mp4");
        assert_eq!(stream.filename.as_deref(), Some("seg_00000.mp4"));
        assert_eq!(stream.bytes, b"segment");
        assert_eq!(stream.result["size"], 7);
        assert_eq!(stream.result["total_size"], media.len());
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 17);

        let ts_segment = api_recording_asset_task_response(
            "rec-asset-ts".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "seg_00001.ts",
                "offset": 10,
                "length": 9,
            })),
            &rt,
        )
        .await;
        assert!(ts_segment.done);
        assert!(ts_segment.byte_stream.is_some());
        let stream = ts_segment.byte_stream.unwrap();
        assert_eq!(stream.content_type, "video/mp2t");
        assert_eq!(stream.filename.as_deref(), Some("seg_00001.ts"));
        assert_eq!(stream.bytes, b"transport");
        assert_eq!(stream.result["size"], 9);
        assert_eq!(stream.result["total_size"], ts_media.len());
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 19);

        let invalid = api_recording_asset_task_response(
            "rec-asset3".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "../seg_00000.mp4",
            })),
            &rt,
        )
        .await;
        assert!(invalid.done);
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn session_frame_asset_rpc_streams_validated_frame_ranges() {
        let session_id = format!(
            "dashboard-frame-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let session_dir = crate::platform::home_dir()
            .join(".intendant")
            .join("logs")
            .join(&session_id);
        let frames_dir = session_dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let frame_bytes = b"dashboard frame bytes";
        std::fs::write(frames_dir.join("ann-test.png"), frame_bytes).unwrap();

        let response = api_session_frame_asset_task_response(
            "frame-asset1".to_string(),
            Some(&serde_json::json!({
                "session_id": &session_id,
                "filename": "ann-test.png",
                "offset": 10,
                "length": 5,
            })),
        )
        .await;
        let _ = std::fs::remove_dir_all(&session_dir);

        assert!(response.done);
        assert!(response.byte_stream.is_some());
        let stream = response.byte_stream.unwrap();
        assert_eq!(stream.content_type, "image/png");
        assert_eq!(stream.filename.as_deref(), Some("ann-test.png"));
        assert_eq!(stream.bytes, b"frame");
        assert_eq!(
            stream.stream_id,
            format!("frame-asset1:session-frame:{session_id}:ann-test.png")
        );
        assert_eq!(stream.result["session_id"], session_id);
        assert_eq!(stream.result["filename"], "ann-test.png");
        assert_eq!(stream.result["size"], 5);
        assert_eq!(stream.result["total_size"], frame_bytes.len());
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 15);
        assert_eq!(stream.result["resumable"], true);

        let invalid = api_session_frame_asset_task_response(
            "frame-asset2".to_string(),
            Some(&serde_json::json!({
                "session_id": "current",
                "filename": "../ann-test.png",
            })),
        )
        .await;
        assert!(invalid.done);
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
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
    async fn display_bootstrap_rpc_returns_empty_frames_without_active_displays() {
        let rt = runtime();
        let bootstrap = api_display_bootstrap_response("disp1".to_string(), &rt).await;
        assert_eq!(bootstrap["t"], "response");
        assert_eq!(bootstrap["id"], "disp1");
        assert_eq!(bootstrap["ok"], true);
        assert_eq!(bootstrap["result"]["frame_count"], 0);
        assert_eq!(bootstrap["result"]["frames"].as_array().unwrap().len(), 0);
        assert!(bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("display_input_authority_state")));
    }

    #[tokio::test]
    async fn display_webrtc_signal_rpc_reports_missing_display() {
        let rt = runtime();
        let params = serde_json::json!({
            "signal": "offer",
            "display_id": 99,
            "sdp": "synthetic-offer",
        });
        let response =
            api_display_webrtc_signal_response("sig1".to_string(), Some(&params), &rt).await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["id"], "sig1");
        assert_eq!(response["ok"], false);
        assert_eq!(response["status"], 404);
        assert_eq!(response["display_id"], 99);
        assert_eq!(response["error"], "display session not found");
    }

    #[tokio::test]
    async fn external_session_activity_replay_rpc_returns_empty_frames_without_attached_sessions() {
        let rt = runtime();
        let replay = api_external_session_activity_replay_response("ext1".to_string(), &rt).await;
        assert_eq!(replay["t"], "response");
        assert_eq!(replay["id"], "ext1");
        assert_eq!(replay["ok"], true);
        assert_eq!(replay["result"]["frame_count"], 0);
        assert_eq!(replay["result"]["frames"].as_array().unwrap().len(), 0);
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
        assert!(!bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("display_ready")));
        assert!(bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("display_input_authority_state")));
        assert!(!bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("external_session_activity_replay")));
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
            let queued =
                test_control_frame_response(&frame, &mut rt, &tx, &mut pending, &mut outbound);
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

    #[tokio::test]
    async fn fs_read_returns_bounded_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"filesystem read fixture").unwrap();

        let response = api_fs_read_task_response(
            "fs-read".to_string(),
            Some(&serde_json::json!({
                "path": file.to_string_lossy(),
                "offset": 11,
                "length": 4,
            })),
        )
        .await;

        assert!(response.byte_stream.is_some());
        let stream = response.byte_stream.unwrap();
        assert_eq!(stream.content_type, "text/plain; charset=utf-8");
        assert_eq!(stream.filename.as_deref(), Some("note.txt"));
        assert_eq!(stream.bytes, b"read");
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["range_start"].as_u64(), Some(11));
        assert_eq!(stream.result["range_end"].as_u64(), Some(15));
        assert_eq!(
            stream.result["total_size"].as_u64(),
            Some("filesystem read fixture".len() as u64)
        );
        assert_eq!(stream.result["resumable"], true);
    }

    #[tokio::test]
    async fn fs_read_rejects_relative_paths_and_directories() {
        let dir = tempfile::tempdir().unwrap();

        let relative = api_fs_read_task_response(
            "fs-read-relative".to_string(),
            Some(&serde_json::json!({
                "path": "relative/path",
            })),
        )
        .await;
        assert!(relative.byte_stream.is_none());
        assert_eq!(relative.frame["t"], "response");
        assert_eq!(relative.frame["result"]["_httpStatus"], 400);
        assert_eq!(relative.frame["result"]["_httpOk"], false);
        assert!(relative.frame["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("path must be absolute"));

        let directory = api_fs_read_task_response(
            "fs-read-dir".to_string(),
            Some(&serde_json::json!({
                "path": dir.path().to_string_lossy(),
            })),
        )
        .await;
        assert!(directory.byte_stream.is_none());
        assert_eq!(directory.frame["result"]["_httpStatus"], 400);
        assert_eq!(directory.frame["result"]["_httpOk"], false);
        assert!(directory.frame["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a regular file"));
    }

    #[tokio::test]
    async fn transfer_download_job_persists_and_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source = dir.path().join("fixture.txt");
        std::fs::write(&source, b"durable download fixture").unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project.clone());

        let status = status_response_frame("transfer-status".to_string(), &rt);
        assert_eq!(status["result"]["api_transfer_jobs_available"], true);
        assert_eq!(status["result"]["api_transfer_job_create_available"], true);
        assert_eq!(
            status["result"]["api_transfer_download_read_available"],
            true
        );

        let create = api_transfer_job_create_response(
            "transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "path": source.to_string_lossy(),
            })),
            &rt,
        )
        .await;
        assert_eq!(create["t"], "response");
        assert_eq!(create["ok"], true);
        assert_eq!(create["result"]["ok"], true);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let resume_token = create["result"]["job"]["resume_token"]
            .as_str()
            .unwrap()
            .to_string();

        let list = api_transfer_jobs_response("transfer-list".to_string(), &rt).await;
        assert_eq!(list["result"]["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(list["result"]["jobs"][0]["id"], job_id);

        let read = api_transfer_download_read_task_response(
            "transfer-read".to_string(),
            Some(&serde_json::json!({
                "resume_token": resume_token,
                "offset": 8,
                "length": 8,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        assert!(read.byte_stream.is_some());
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.id, "transfer-read");
        assert_eq!(stream.stream_id, "transfer-read:transfer-download");
        assert_eq!(stream.content_type, "text/plain; charset=utf-8");
        assert_eq!(stream.filename.as_deref(), Some("fixture.txt"));
        assert_eq!(stream.bytes, b"download");
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["id"], job_id);
        assert_eq!(stream.result["range_start"], 8);
        assert_eq!(stream.result["range_end"], 16);
        assert_eq!(stream.result["resumable"], true);
        assert_eq!(
            stream.result["total_size"].as_u64(),
            Some("durable download fixture".len() as u64)
        );
    }

    #[tokio::test]
    async fn transfer_session_report_artifact_materializes_and_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let session_dir = dir.path().join("session-report");
        std::fs::create_dir_all(&project).unwrap();
        let log = crate::session_log::SessionLog::open(session_dir.clone()).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{\"ok\":true}\n").unwrap();
        std::fs::create_dir_all(session_dir.join("turns")).unwrap();
        std::fs::write(
            session_dir.join("turns").join("turn_001_stdout.txt"),
            "hello\n",
        )
        .unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);
        {
            let mut session = rt.shared_session.write().await;
            session.session_log = Some(Arc::new(std::sync::Mutex::new(log)));
        }

        let create = api_transfer_job_create_response(
            "report-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "session_report",
                    "session_id": "current",
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(create["result"]["job"]["source_kind"], "session_report");
        assert_eq!(create["result"]["job"]["source_label"], "Session report");
        assert_eq!(create["result"]["job"]["managed_source"], true);
        assert_eq!(
            create["result"]["job"]["artifact"]["type"],
            "session_report"
        );
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let total_size = create["result"]["job"]["total_size"].as_u64().unwrap();

        let read = api_transfer_download_read_task_response(
            "report-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 0,
                "length": total_size,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "application/zip");
        assert!(stream.filename.as_deref().unwrap_or("").ends_with(".zip"));
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(stream.bytes)).unwrap();
        assert!(zip.by_name("summary.json").is_ok());
        assert!(zip.by_name("turns/turn_001_stdout.txt").is_ok());
    }

    #[tokio::test]
    async fn transfer_staged_upload_artifact_reads_byte_stream() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let bytes = b"dashboard raw upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "raw.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();

        let create = api_transfer_job_create_response(
            "staged-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "staged_upload",
                    "id": descriptor.id,
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(create["result"]["job"]["source_kind"], "staged_upload");
        assert_eq!(
            create["result"]["job"]["source_label"],
            "Staged upload raw.txt"
        );
        assert_eq!(create["result"]["job"]["filename"], "raw.txt");
        assert_eq!(create["result"]["job"]["managed_source"], false);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();

        let read = api_transfer_download_read_task_response(
            "staged-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 10,
                "length": 6,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "text/plain");
        assert_eq!(stream.filename.as_deref(), Some("raw.txt"));
        assert_eq!(stream.bytes, &bytes[10..16]);
        assert_eq!(stream.result["resumable"], true);
    }

    #[tokio::test]
    async fn transfer_recording_asset_artifact_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let session_dir = dir.path().join("recording-session");
        let stream_dir = session_dir.join("recordings").join("display_0");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&stream_dir).unwrap();
        std::fs::write(stream_dir.join("segments.csv"), "seg_00000.mp4,0,1.25\n").unwrap();
        let media = b"recording segment bytes";
        std::fs::write(stream_dir.join("seg_00000.mp4"), media).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);
        {
            let mut session = rt.shared_session.write().await;
            session.recording_registry = Some(Arc::new(tokio::sync::RwLock::new(
                crate::recording::RecordingRegistry::new(
                    &session_dir,
                    crate::project::RecordingConfig::default(),
                ),
            )));
        }

        let create = api_transfer_job_create_response(
            "recording-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "recording_asset",
                    "stream_name": "display_0",
                    "asset": "seg_00000.mp4",
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(create["result"]["job"]["source_kind"], "recording_asset");
        assert_eq!(
            create["result"]["job"]["source_label"],
            "display_0 seg_00000.mp4"
        );
        assert_eq!(create["result"]["job"]["managed_source"], false);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();

        let read = api_transfer_download_read_task_response(
            "recording-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 10,
                "length": 7,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "video/mp4");
        assert_eq!(stream.filename.as_deref(), Some("seg_00000.mp4"));
        assert_eq!(stream.bytes, b"segment");
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 17);
    }

    #[tokio::test]
    async fn transfer_session_frame_artifact_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let session_id = format!(
            "dashboard-frame-transfer-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let session_dir = crate::platform::home_dir()
            .join(".intendant")
            .join("logs")
            .join(&session_id);
        let frames_dir = session_dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let frame_bytes = b"dashboard frame bytes";
        std::fs::write(frames_dir.join("ann-test.png"), frame_bytes).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);

        let create = api_transfer_job_create_response(
            "frame-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "session_frame_asset",
                    "session_id": &session_id,
                    "filename": "ann-test.png",
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(
            create["result"]["job"]["source_kind"],
            "session_frame_asset"
        );
        assert_eq!(create["result"]["job"]["filename"], "ann-test.png");
        assert_eq!(create["result"]["job"]["managed_source"], false);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();

        let read = api_transfer_download_read_task_response(
            "frame-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 10,
                "length": 5,
            })),
            &rt,
        )
        .await;
        let _ = std::fs::remove_dir_all(&session_dir);
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "image/png");
        assert_eq!(stream.filename.as_deref(), Some("ann-test.png"));
        assert_eq!(stream.bytes, b"frame");
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 15);
    }

    #[tokio::test]
    async fn transfer_upload_chunks_commit_to_arbitrary_destination() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let dest_dir = dir.path().join("dest");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("out.txt");

        let mut rt = runtime();
        rt.project_root = Some(project);

        let create = api_transfer_job_create_response(
            "upload-create".to_string(),
            Some(&serde_json::json!({
                "kind": "upload",
                "destination": dest.to_string_lossy(),
                "name": "out.txt",
                "mime": "text/plain",
                "total_size": 11,
                "conflict": "fail",
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let resume_token = create["result"]["job"]["resume_token"]
            .as_str()
            .unwrap()
            .to_string();

        let first = api_transfer_upload_chunk_task_response(
            "upload-chunk-1".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({
                    "id": job_id,
                    "offset": 0,
                }),
                b"hello ",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(first.frame["result"]["ok"], true);
        assert_eq!(first.frame["result"]["job"]["completed_bytes"], 6);
        assert_eq!(first.frame["result"]["job"]["status"], "running");

        let second = api_transfer_upload_chunk_task_response(
            "upload-chunk-2".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({
                    "resume_token": resume_token,
                    "offset": 6,
                }),
                b"world",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(second.frame["result"]["ok"], true);
        assert_eq!(second.frame["result"]["job"]["completed_bytes"], 11);
        assert_eq!(second.frame["result"]["job"]["status"], "ready");

        let commit = api_transfer_upload_commit_response(
            "upload-commit".to_string(),
            Some(&serde_json::json!({ "id": job_id })),
            &rt,
        )
        .await;
        assert_eq!(commit["result"]["ok"], true);
        assert_eq!(commit["result"]["job"]["status"], "completed");
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn control_frame_routes_transfer_jobs_request() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = test_control_frame_response(
            r#"{"t":"request","id":"transfer-jobs-frame","method":"api_transfer_jobs"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("transfer-jobs-frame"));

        let response = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "transfer-jobs-frame");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["ok"], true);
        assert_eq!(
            response.frame["result"]["jobs"].as_array().unwrap().len(),
            0
        );
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

        let hello = test_control_frame_response(
            r#"{"t":"hello","id":"h1","features":["response_credit"]}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert!(rt.response_credit_enabled);

        let status = test_control_frame_response(
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
        assert!(test_control_frame_response(
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

        let cancelled = test_control_frame_response(
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
    fn byte_stream_frames_are_chunked_and_credit_addressable() {
        let bytes: Vec<u8> = (0..73).map(|i| (i % 251) as u8).collect();
        let stream = ControlByteStream {
            id: "download-1".to_string(),
            stream_id: "download-1:file".to_string(),
            content_type: "application/octet-stream".to_string(),
            filename: Some("artifact.bin".to_string()),
            bytes: bytes.clone(),
            result: serde_json::json!({
                "ok": true,
                "filename": "artifact.bin",
                "size": bytes.len(),
            }),
        };
        let frames = byte_stream_frame_texts(stream, 13);
        assert_eq!(frames.len(), 8, "expected start + 6 chunks + end");

        let start: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(start["t"], "byte_stream_start");
        assert_eq!(start["id"], "download-1");
        assert_eq!(start["stream_id"], "download-1:file");
        assert_eq!(start["encoding"], "base64");
        assert_eq!(start["content_type"], "application/octet-stream");
        assert_eq!(start["filename"], "artifact.bin");
        assert_eq!(start["total_bytes"], bytes.len());
        assert_eq!(start["chunks"], 6);

        let mut decoded = Vec::new();
        for (seq, text) in frames[1..frames.len() - 1].iter().enumerate() {
            let chunk: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(chunk["t"], "byte_stream_chunk");
            assert_eq!(chunk["id"], "download-1");
            assert_eq!(chunk["stream_id"], "download-1:file");
            assert_eq!(chunk["seq"], seq);
            decoded.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(decoded, bytes);

        let end: serde_json::Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(end["t"], "byte_stream_end");
        assert_eq!(end["id"], "download-1");
        assert_eq!(end["stream_id"], "download-1:file");
        assert_eq!(end["chunks"], 6);
        assert_eq!(end["result"]["filename"], "artifact.bin");
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
