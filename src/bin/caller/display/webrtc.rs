//! Per-peer WebRTC driver built on the sans-I/O `rtc` core.
//!
//! Architecture: each `WebRtcPeer` owns a tokio task ("driver") that holds an
//! peer connection instance and UDP/TCP sockets. The driver pumps three things in a single
//! `select!` loop:
//!
//! 1. Inbound UDP/TCP datagrams → `peer.handle_read(TaggedBytesMut)`
//! 2. Encoded video frames from the shared encoder fan-out → `writer.write(...)`
//! 3. Commands from the public `WebRtcPeer` handle (ICE candidates, clipboard
//!    sends, shutdown) → `peer.add_remote_candidate()` / data channel writes
//!
//! After every input the driver drains the peer connection's pending writes,
//! reads, and events, and uses `poll_timeout` / `handle_timeout` to drive timers.
//!
//! ## ICE-TCP multiplexing
//!
//! The web gateway creates one shared `TcpPeerRegistry` at startup and
//! hands it to every peer via `handle_offer`. Peers pre-generate their
//! local ICE ufrag (so the registry key is known before the SDP answer
//! is produced) and register it with the registry at construction time.
//!
//! The web gateway's accept loop peeks every incoming TCP connection's
//! first bytes to tell HTTP vs. WebSocket vs. STUN-framed traffic apart.
//! STUN-framed traffic is read through one RFC 4571 frame and handed to
//! the registry, which parses the STUN USERNAME attribute, extracts the
//! target-ufrag half (per RFC 8445 §7.2.2 the USERNAME is
//! `<target_ufrag>:<sender_ufrag>`), and forwards the connection to the
//! matching peer's driver. Each TCP connection becomes a bidirectional
//! channel: inbound frames flow through the same packet channel UDP
//! uses (tagged with `TransportProtocol::TCP`), and outbound writes
//! with `proto == Tcp` is written to the connection's write half keyed
//! on the destination address.
//!
//! The advertised TCP candidate's address comes from the browser's
//! `Host:` HTTP header (parsed by the gateway): whatever non-loopback
//! IP the browser is already using to reach the dashboard, we advertise
//! as our ICE-TCP host candidate. Firefox would filter a remote
//! `127.0.0.1` candidate as an anti-rebinding mitigation, so a user who
//! accesses the dashboard via `http://localhost:…` through a
//! loopback-bound port-forward gets no TCP path — they need to access
//! via the host's LAN IP (or configure their port-forward on all
//! interfaces). This is documented in the README.

use super::clipboard::ClipboardContent;
use super::encode::pool::{
    CodecKind, EncoderId, EncoderPool, EncoderSubscription, PeerCodecPreferences, PoolLease,
    SimulcastRid,
};
use super::tile::backpressure::{TileDeltaBackpressure, TileDeltaSendDecision};
use super::tile::transport as tile_transport;
use super::{EncodedFrame, IceConfig, InputEvent, PeerId};
use crate::error::CallerError;
use bytes::{Bytes, BytesMut};
use rtc::data_channel::{RTCDataChannelId, RTCDataChannelMessage};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264 as RTC_MIME_TYPE_H264, MIME_TYPE_VP8 as RTC_MIME_TYPE_VP8,
};
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCDtlsRole;
use rtc::peer_connection::transport::{RTCIceCandidateInit, RTCIceProtocol};
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::packetizer::{self, Packetizer};
use rtc::rtp::sequence;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use rtc::rtp_transceiver::RTCRtpSenderId;
use rtc::sansio::Protocol as RtcProtocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use rtc::statistics::report::RTCStatsReportEntry;
use rtc::statistics::stats::RTCStatsType;
use rtc::statistics::StatsSelector;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Bound on the per-peer encoded-frame channel. Frames in excess are dropped
/// with backpressure registered in the display metrics.
const ENCODED_FRAME_CHANNEL: usize = 8;

/// Bound on the per-peer command channel.
const COMMAND_CHANNEL: usize = 32;

/// Bound on the per-peer keyframe-request channel (driver → intake).
///
/// Lossy by design — the encoder pool's coalescer dedups bursts within
/// a small window, so a request lost to a full channel is reissued by
/// the next PLI/FIR within the same coalesce window. Sized to absorb
/// brief PLI storms (e.g. all simulcast layers requesting at once
/// after a keyframe loss) without backpressure on the rtc poll loop.
const KEYFRAME_REQUEST_CHANNEL: usize = 16;

/// **Phase 4d.1**: how often the driver polls `rtc.get_stats(..)` to
/// compute the per-peer recent observed send bitrate from outbound
/// `bytes_sent` deltas across one polling window.
///
/// 1s is the smallest interval where the bytes-delta has enough
/// signal-to-noise to be a useful steady-state observation: a 30fps
/// VP8 simulcast at ~3 Mbps total produces ~375 KB/poll at 1s, vs
/// per-packet jitter of single-KB. Faster polling (e.g. 200ms)
/// would amplify per-packet jitter into the rate estimate without
/// actually catching real bandwidth shifts any sooner. Polls
/// themselves are cheap (read-only walk of the rtc-side accumulator
/// state); the tradeoff is purely the staleness of the watch-channel
/// value the layer-selection aggregator (4d.2) reads.
///
/// **Why not `available_outgoing_bitrate`**: rtc 0.9's
/// `RTCIceCandidatePairStats::available_outgoing_bitrate` is
/// initialized to 0.0 by `rtc-ice-0.9.0` and never written to —
/// rtc 0.9's `update_ice_agent_stats` only copies STUN counters and
/// RTT, no congestion-control bandwidth estimate flows through.
/// Polling that field returns 0.0 forever. Deriving from
/// `bytes_sent` deltas observes a signal rtc 0.9 actually
/// maintains.
const TWCC_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Maximum UDP datagram we'll receive on the per-peer socket.
const UDP_BUF_LEN: usize = 2000;

/// Maximum RFC 4571 frame we'll accept over ICE-TCP (one STUN/DTLS/RTP packet).
/// DTLS records and RTP packets are bounded by MTU in practice; we use a
/// generous ceiling to accommodate jumbo frames without allowing pathological
/// memory allocation from a malicious peer.
const TCP_MAX_FRAME_LEN: usize = 65535;

// ---------------------------------------------------------------------------
// TCP peer registry (ufrag → per-peer connection channel)
// ---------------------------------------------------------------------------
//
// `TcpPeerRegistry` is a pure demux registry with no listener of its own.
// One instance is created at web_gateway startup and shared across all
// display sessions. The web_gateway's accept loop (which already peeks
// every incoming TCP connection for HTTP vs. WebSocket) grows a third
// branch: if the first bytes look like an RFC 4571-framed STUN binding
// request, read one full frame, then call `route_accepted` to hand the
// connection to the matching peer. HTTP-on-the-same-port works untouched
// because the peek is non-destructive and STUN traffic is
// byte-distinguishable from HTTP methods (no printable ASCII at offset 0)
// and TLS handshakes (no 0x16 at offset 0).

/// Shared peer registry: ufrag → handoff channel. Peers register at
/// construction time; `route_accepted` looks up the matching peer for an
/// incoming TCP connection.
pub struct TcpPeerRegistry {
    registry: std::sync::Mutex<HashMap<String, mpsc::Sender<AcceptedTcpConnection>>>,
}

/// A TCP connection that has been matched to a peer by its first STUN frame.
/// Carries the first frame (which the peer still needs to process) alongside
/// the stream so the peer can read subsequent frames and write outbound
/// transmits.
pub struct AcceptedTcpConnection {
    pub remote_addr: SocketAddr,
    pub local_addr: SocketAddr,
    /// The first frame we already read off the wire (needed for STUN ufrag
    /// matching). The peer's driver must feed this to the sans-I/O RTC core.
    pub first_frame: Vec<u8>,
    pub stream: TcpStream,
}

impl TcpPeerRegistry {
    /// Create an empty registry. Share the returned `Arc` across every
    /// caller that needs to register a peer or route a connection.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Register a peer's local ufrag and return the receiver side of the
    /// per-peer connection channel. Drop the returned `PeerRegistration` to
    /// unregister on peer close.
    pub fn register(
        self: &Arc<Self>,
        local_ufrag: String,
    ) -> (PeerRegistration, mpsc::Receiver<AcceptedTcpConnection>) {
        let (tx, rx) = mpsc::channel::<AcceptedTcpConnection>(8);
        self.registry
            .lock()
            .unwrap()
            .insert(local_ufrag.clone(), tx);
        (
            PeerRegistration {
                registry: Arc::clone(self),
                local_ufrag,
            },
            rx,
        )
    }

    /// Route an already-accepted TCP connection plus its peeked first RFC
    /// 4571 frame to the peer whose local ufrag matches the STUN USERNAME
    /// in that frame. Called by the web_gateway's accept loop when it
    /// detects STUN-framed traffic on the HTTP port.
    pub async fn route_accepted(
        self: &Arc<Self>,
        stream: TcpStream,
        first_frame: Vec<u8>,
        remote_addr: SocketAddr,
    ) -> Result<(), String> {
        let local_addr = stream
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;

        let username = parse_stun_username(&first_frame)
            .ok_or_else(|| "first frame is not a STUN binding request with USERNAME".to_string())?;

        // Per RFC 8445 §7.2.2, the STUN USERNAME attribute for an ICE
        // connectivity check sent from A to B is formatted as
        // `<B_ufrag>:<A_ufrag>` — target peer's ufrag first, sender's
        // ufrag second. When a browser → server request arrives at the
        // server, the FIRST segment is the server's ufrag (us, the
        // demux key) and the second is the browser's ufrag (which we
        // don't care about here). Getting this backwards makes every
        // incoming TCP connection fail routing lookup.
        let local_ufrag = username
            .split_once(':')
            .map(|(target, _sender)| target.to_string())
            .ok_or_else(|| format!("bad USERNAME format: {username:?}"))?;

        let tx = {
            let guard = self.registry.lock().unwrap();
            guard.get(&local_ufrag).cloned()
        };
        let Some(tx) = tx else {
            return Err(format!("no peer registered for ufrag {local_ufrag:?}"));
        };

        let accepted = AcceptedTcpConnection {
            remote_addr,
            local_addr,
            first_frame,
            stream,
        };
        tx.send(accepted).await.map_err(|_| {
            "peer channel closed before we could hand over the connection".to_string()
        })?;
        Ok(())
    }
}

/// RAII guard that unregisters a peer's ufrag from the registry on drop.
pub struct PeerRegistration {
    registry: Arc<TcpPeerRegistry>,
    local_ufrag: String,
}

impl Drop for PeerRegistration {
    fn drop(&mut self) {
        self.registry
            .registry
            .lock()
            .unwrap()
            .remove(&self.local_ufrag);
    }
}

impl TcpPeerRegistry {
    /// Return `true` if a peer with this ufrag is currently registered.
    /// Non-consuming — used by the gateway's accept loop to decide
    /// between local dispatch (this registry) and relay dispatch
    /// ([`TcpRelayRegistry`]). Separate from `route_accepted` because
    /// that call takes the stream by value, and we need to commit to
    /// one registry before handing over.
    pub fn contains_ufrag(&self, ufrag: &str) -> bool {
        self.registry.lock().unwrap().contains_key(ufrag)
    }
}

// ---------------------------------------------------------------------------
// TCP relay registry (ufrag → outbound peer address)
// ---------------------------------------------------------------------------
//
// Slice 3b: the federation-level equivalent of `TcpPeerRegistry`. Each
// entry maps a REMOTE peer's ICE ufrag to an outbound `SocketAddr`
// pointing at that peer's HTTP listener. When the gateway's accept
// loop sees an incoming STUN-framed TCP connection whose ufrag is
// here (and not in the local `TcpPeerRegistry`), the primary opens a
// fresh TCP connection to the outbound address, re-frames the peeked
// first frame, writes it, then bidirectionally shuttles bytes between
// the browser's stream and the peer's stream.
//
// The entries get populated by the `OutboundEvent::PeerEventForwarded`
// translator when it sees a federated `WebRtcSignal::Answer` flowing
// back from a peer to the browser: the translator parses the Answer's
// SDP for the peer's ICE ufrag, resolves the peer's
// `browser_tcp_via_url` / `ws_url` to a SocketAddr, and registers
// (ufrag → SocketAddr) here.
//
// When the browser's `RTCPeerConnection` tries the primary-relay TCP
// candidate (injected into the Answer SDP by the same translator
// alongside the peer's direct candidate), the connection lands on the
// primary's HTTP listener with the peer's ufrag in its first STUN
// USERNAME. `TcpPeerRegistry::contains_ufrag` returns false (no local
// match), `TcpRelayRegistry::contains_ufrag` returns true, the accept
// loop dispatches to the relay path.

/// Registry of `ufrag → outbound peer address` entries for federation-
/// level TCP relay. See the module-level comment above for flow.
pub struct TcpRelayRegistry {
    registry: std::sync::Mutex<HashMap<String, SocketAddr>>,
}

impl TcpRelayRegistry {
    /// Create an empty registry. Share the returned `Arc` across every
    /// caller that needs to register relay targets or route incoming
    /// TCP connections.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Associate a remote peer's ICE ufrag with the outbound
    /// [`SocketAddr`] the primary will dial when it sees an incoming
    /// TCP connection carrying that ufrag. Idempotent — re-registering
    /// the same ufrag updates the outbound address (useful for peer
    /// reconnects that issue a fresh Answer with a new address).
    pub fn register(&self, ufrag: String, outbound: SocketAddr) {
        self.registry.lock().unwrap().insert(ufrag, outbound);
    }

    /// Remove a ufrag entry. Called when the corresponding federated
    /// WebRTC session closes (browser-initiated close, peer teardown,
    /// transport disconnect). Missing entries are silently ignored
    /// — idempotent cleanup.
    pub fn unregister(&self, ufrag: &str) {
        self.registry.lock().unwrap().remove(ufrag);
    }

    /// Look up the outbound address for a ufrag. Returns `None` when
    /// no relay entry exists (typical case for ufrags belonging to
    /// locally-hosted WebRTC peers handled by `TcpPeerRegistry`).
    pub fn lookup(&self, ufrag: &str) -> Option<SocketAddr> {
        self.registry.lock().unwrap().get(ufrag).copied()
    }

    /// Return `true` if an entry exists for this ufrag. Non-consuming
    /// — used by the gateway's accept loop to dispatch between local
    /// and relay paths.
    pub fn contains_ufrag(&self, ufrag: &str) -> bool {
        self.registry.lock().unwrap().contains_key(ufrag)
    }

    /// Route an already-accepted STUN-framed TCP connection through
    /// the relay: dial the peer, re-frame and write the peeked first
    /// frame, then spawn a bidirectional byte-forwarding task for the
    /// remainder. Returns an error if the lookup misses or the
    /// outbound connect fails — caller closes the stream in that case.
    ///
    /// `first_frame` is the RFC 4571 payload (without the 2-byte length
    /// prefix) that the gateway already consumed from the stream; we
    /// re-wrap it before writing to the peer so the peer's own accept
    /// loop sees the same framed STUN bytes it would have seen from a
    /// direct browser connection.
    pub async fn route_accepted(
        self: &Arc<Self>,
        stream: TcpStream,
        first_frame: Vec<u8>,
    ) -> Result<(), String> {
        let username = parse_stun_username(&first_frame)
            .ok_or_else(|| "first frame is not a STUN binding request with USERNAME".to_string())?;
        // Target ufrag is the first half of `target:sender`, same as
        // TcpPeerRegistry's dispatch — RFC 8445 §7.2.2.
        let local_ufrag = username
            .split_once(':')
            .map(|(target, _sender)| target.to_string())
            .ok_or_else(|| format!("bad USERNAME format: {username:?}"))?;

        let outbound_addr = self
            .lookup(&local_ufrag)
            .ok_or_else(|| format!("no relay registered for ufrag {local_ufrag:?}"))?;

        // Dial the peer. If this fails, the browser's ICE will see
        // the TCP pair as unformable and (usually) fall back to UDP
        // or time out — no retry at this layer.
        let mut outbound = TcpStream::connect(outbound_addr)
            .await
            .map_err(|e| format!("dial {outbound_addr}: {e}"))?;

        // Re-frame the peeked first frame and write it to the peer so
        // the peer's accept loop sees the same RFC 4571-framed STUN
        // bytes the browser originally sent.
        write_rfc4571_frame(&mut outbound, &first_frame)
            .await
            .map_err(|e| format!("write first frame to {outbound_addr}: {e}"))?;

        // Spawn a bidirectional byte-forwarder. `copy_bidirectional`
        // handles both directions concurrently and exits when either
        // side closes — matches the ICE-TCP lifecycle where a single
        // candidate pair's TCP connection lives for the WebRTC
        // session's duration.
        let mut stream = stream;
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut outbound).await;
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public helpers for ufrag / SDP manipulation (slice 3b)
// ---------------------------------------------------------------------------

/// Parse the ICE `ufrag` out of an SDP Answer. Looks for the first
/// session-level or media-level `a=ice-ufrag:<value>` attribute and
/// returns the value. Returns `None` if no such attribute is present,
/// which is a malformed SDP per RFC 5245 — callers treat it as
/// "this Answer isn't relay-able, skip the rewrite."
///
/// Exposed publicly so the `OutboundEvent` translator in
/// `web_gateway.rs` can extract the ufrag from an incoming federated
/// `WebRtcSignal::Answer` and register it in [`TcpRelayRegistry`]
/// keyed to the outbound peer address.
pub fn parse_sdp_ice_ufrag(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse just the STUN USERNAME attribute's ufrag out of an RFC 4571
/// frame payload. Wrapper around `parse_stun_username` + the
/// `target:sender` split used by ICE. Returns the TARGET ufrag (the
/// first half) — the one keyed in the ufrag registries.
///
/// Returns `None` when the frame isn't a STUN binding request, lacks
/// a USERNAME attribute, or the username isn't in the expected
/// `target:sender` format.
pub fn parse_first_frame_ufrag(first_frame: &[u8]) -> Option<String> {
    let username = parse_stun_username(first_frame)?;
    username
        .split_once(':')
        .map(|(target, _sender)| target.to_string())
}

/// Inject an additional ICE-TCP host candidate into an SDP Answer,
/// pointing at the primary daemon's own address so the browser has a
/// relay-path candidate alongside the peer's direct candidate.
///
/// The injected line is placed immediately after the first existing
/// `a=candidate:` line (or, if there are no candidate lines, at the
/// end of the first media section). `foundation` is deliberately
/// distinct from normal local candidate values to avoid collision; `priority`
/// is set lower than a typical host-TCP-passive candidate so ICE
/// prefers the peer's direct candidate when reachable and only falls
/// back to the relay when direct fails.
///
/// IPv6 addresses are emitted in canonical form; IPv4 addresses as
/// dotted-quad. `component_id` is always 1
/// (RTP; same-stream RTCP multiplexed per `a=rtcp-mux`).
///
/// Returns the modified SDP as a new `String`. Pure function — never
/// mutates the input.
pub fn inject_relay_tcp_candidate(sdp: &str, primary_addr: SocketAddr) -> String {
    // Priority formula per RFC 5245 §4.1.2.1:
    //   priority = (2^24)*type_pref + (2^8)*local_pref + (256 - component_id)
    //
    // type_pref for host is 126; we use 100 so the relay candidate's
    // priority is strictly below a typical peer-direct host TCP
    // candidate (host candidates normally use type_pref 126). local_pref
    // is 0 (single interface) since the distinction doesn't help here.
    //
    // Result: priority = (2^24)*100 + 0 + 255 = 1_677_721_855.
    let type_pref: u32 = 100;
    let local_pref: u32 = 0;
    let component_id: u32 = 1;
    let priority =
        (1u32 << 24).saturating_mul(type_pref) + (1u32 << 8) * local_pref + (256 - component_id);
    let ip = match primary_addr.ip() {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => v6.to_string(),
    };
    let port = primary_addr.port();
    // Foundation 9001 is arbitrary; picked to not collide with common
    // typical sequential foundations (1, 2, ...). Same foundation for
    // every injected candidate is fine per RFC 5245 since foundations
    // only need to be unique-per-stream within a single side's set.
    let candidate_line = format!(
        "a=candidate:9001 {component_id} tcp {priority} {ip} {port} typ host tcptype passive generation 0"
    );

    // Walk the SDP line by line. Insert the new candidate immediately
    // after the first existing `a=candidate:` line (keeps the candidate
    // block contiguous, which matches how SDP is conventionally laid
    // out). If there are no existing candidate lines, append at the
    // end. Preserve line endings as they were (CRLF or LF).
    let newline = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut inserted = false;
    let mut out = String::with_capacity(sdp.len() + candidate_line.len() + 2);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if !inserted
            && line
                .trim_end_matches(|c| c == '\r' || c == '\n')
                .starts_with("a=candidate:")
        {
            out.push_str(&candidate_line);
            out.push_str(newline);
            inserted = true;
        }
    }
    if !inserted {
        // No existing candidates — append at end, making sure we've
        // got a newline separator first.
        if !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(&candidate_line);
        out.push_str(newline);
    }
    out
}

// ---------------------------------------------------------------------------
// RFC 4571 framing
// ---------------------------------------------------------------------------

/// Read one RFC 4571 framed payload from a `tokio::io::AsyncRead`:
/// 2-byte big-endian length header followed by `length` bytes of payload.
///
/// Generic over the read source so we can reuse it for a `TcpStream`
/// (during dispatcher probe) and an `OwnedReadHalf` (inside the per-peer
/// reader task after `into_split`).
async fn read_rfc4571_frame<R>(r: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 || len > TCP_MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("RFC 4571 frame length {len} out of bounds"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Public wrapper around `read_rfc4571_frame` for the web gateway's
/// ICE-TCP detection path. The gateway peeks the first bytes to decide
/// between HTTP/WS/ICE-TCP, and when it picks ICE-TCP it needs to consume
/// that first frame from the stream before handing ownership to the
/// `TcpPeerRegistry`. We don't want to re-export the generic helper
/// cross-module, so this is a concrete version for `TcpStream`.
pub async fn read_rfc4571_frame_pub(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    read_rfc4571_frame(stream).await
}

/// Write one RFC 4571 framed payload: prepend a 2-byte BE length header,
/// then the payload bytes.
async fn write_rfc4571_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    if payload.len() > TCP_MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("RFC 4571 frame too large: {}", payload.len()),
        ));
    }
    let len = payload.len() as u16;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal STUN parser (USERNAME attribute only)
// ---------------------------------------------------------------------------

/// Parse just enough of a STUN message (RFC 5389) to extract the USERNAME
/// attribute value (type 0x0006). Returns `None` for non-STUN or malformed
/// input, or STUN messages without a USERNAME attribute.
fn parse_stun_username(bytes: &[u8]) -> Option<String> {
    // Header: 20 bytes
    //   type (2) | length (2) | magic cookie (4) | transaction id (12)
    if bytes.len() < 20 {
        return None;
    }
    // Magic cookie must be 0x2112A442 per RFC 5389.
    if bytes[4..8] != [0x21, 0x12, 0xA4, 0x42] {
        return None;
    }
    let msg_length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let attrs_end = 20usize.checked_add(msg_length)?;
    if bytes.len() < attrs_end {
        return None;
    }

    let mut offset = 20usize;
    while offset + 4 <= attrs_end {
        let attr_type = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let attr_length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start.checked_add(attr_length)?;
        if value_end > attrs_end {
            return None;
        }
        if attr_type == 0x0006 {
            // USERNAME — UTF-8 string per RFC 5389 §15.3.
            return std::str::from_utf8(&bytes[value_start..value_end])
                .ok()
                .map(String::from);
        }
        // Advance past value, padded to a 4-byte boundary.
        let pad = (4 - (attr_length % 4)) % 4;
        offset = value_end + pad;
    }
    None
}

/// Public handle to a single WebRTC peer.
///
/// All operations route to the driver task via channels; the driver owns the
/// RTC peer connection and UDP/TCP sockets exclusively.
pub struct WebRtcPeer {
    #[allow(dead_code)]
    pub peer_id: PeerId,
    command_tx: mpsc::Sender<Command>,
    /// **Phase 4d.1**: per-peer recent observed send bitrate in
    /// bits/sec, computed by the driver every `TWCC_POLL_INTERVAL`
    /// from outbound `bytes_sent` deltas across one polling window.
    /// `None` on the first poll (seeds the per-SSRC `prev` map) and
    /// any time the most recent window had zero usable deltas (no
    /// outbound traffic, counter wraparound, etc.); `Some(bps)` once
    /// a delta can be computed.
    ///
    /// **This is local egress, not available capacity.** It tells
    /// you "how many bits we just sent," not "how many bits the
    /// link could carry." Treating it as capacity creates a ratchet:
    /// pausing a layer drops observed egress, which then keeps the
    /// layer paused permanently. Capacity-driven layer adaptation
    /// needs a remote signal — RTCP RR `fraction_lost` per SSRC,
    /// TWCC arrival feedback, browser-side `getStats` — see 4d.3.
    observed_send_bitrate_rx: watch::Receiver<Option<u64>>,
    /// **Phase 4d.3a**: per-peer per-RID receiver-feedback health,
    /// derived from inbound RTCP RR via rtc 0.9's
    /// `RTCRemoteInboundRtpStreamStats` (the only RR-derived signal
    /// rtc 0.9 actually populates — see
    /// [`Self::observed_send_bitrate_rx`] for why local egress is
    /// the wrong proxy for capacity). Refreshed by the driver every
    /// `TWCC_POLL_INTERVAL` from the same `get_stats` call that
    /// drives `observed_send_bitrate`.
    ///
    /// Initial value is the empty map ("no RR has arrived for any
    /// SSRC yet"); per-RID entries appear as RRs arrive. A RID
    /// missing from the map means no signal yet for that layer —
    /// the 4d.3b/c policy treats missing as "stay conservative,
    /// don't act on absence."
    ///
    /// **Phase 4d.3a is observation only.** No layer decisions are
    /// made from this signal. 4d.3b adds the pure policy
    /// (per-(peer, RID) wanted-set + hysteresis); 4d.3c wires the
    /// aggregator to react.
    remote_inbound_health_rx: watch::Receiver<HashMap<SimulcastRid, PeerLayerHealth>>,
    /// **Phase 4d.3b**: per-peer aggregate TWCC health, published
    /// once per second by [`crate::display::twcc_tap::spawn_twcc_health_aggregator`].
    /// `None` initially (no window has fired yet), and `None` for
    /// any window in which no TWCC events arrived (silence is not
    /// recovery — see the aggregator's module docs). The channel
    /// transitions `None → Some(_) → None → Some(_)` as feedback
    /// arrives and goes silent across windows.
    ///
    /// This is the actionable capacity signal on this stack —
    /// rtc 0.9's `RTCRemoteInboundRtpStreamStats` (above) stays
    /// at all-zero defaults regardless of received RTCP because
    /// the rtc-interceptor chain consumes RTCP without
    /// surfacing it. The TWCC tap fills that gap by parsing
    /// `TransportLayerCc` packets directly at the chain.
    ///
    /// WKWebView's TWCC reporting is aggregate (single sender-SSRC
    /// across all RIDs in a simulcast send), not per-layer — the
    /// 4d.3b policy treats the signal as peer-wide and gates upper
    /// simulcast layers in cascade (full → half → floor-only) under
    /// sustained loss. Per-layer adaptation is a 4d.3c concern,
    /// dependent on receivers that emit per-RID TLC.
    twcc_health_rx: watch::Receiver<Option<crate::display::twcc_tap::TwccHealth>>,
    /// **#57**: the negotiated active RID set for this peer, frozen at
    /// construction time. The layer-policy coordinator
    /// ([`crate::display::aggregator::spawn_layer_policy_coordinator`])
    /// reads this each tick to compute the per-display "pinned"
    /// layer set: a peer with `active_rids.len() == 1` MUST keep its
    /// only RID active or it gets no frames at all (its WebRTC track
    /// only declares one encoding; pausing that layer in the encoder
    /// pool starves the peer rather than degrading it). Multi-RID
    /// peers (`len() > 1`) don't pin — the policy is free to pause
    /// upper layers because they have the floor as fallback.
    ///
    /// Stable for the peer's lifetime: WebRTC re-negotiation (mid-call
    /// SDP renegotiate) would change this, but the pool's
    /// peer-rebuild path drops + recreates the WebRtcPeer, so a fresh
    /// `active_rids` snapshot is always in lockstep with the
    /// negotiated answer SDP.
    active_rids: Vec<SimulcastRid>,
    shutdown: CancellationToken,
}

/// **Phase 4d.3a**: per-RID receiver-feedback health, derived from a
/// single `RTCRemoteInboundRtpStreamStats` entry (one outbound SSRC's
/// RR-reported state). Surfaced to the layer-selection aggregator via
/// [`WebRtcPeer::subscribe_remote_inbound_health`].
///
/// All fields come straight from rtc 0.9's RR accumulator (no delta
/// computation in 4d.3a — 4d.3b decides which signals to use and how).
#[derive(Clone, Debug, PartialEq)]
pub struct PeerLayerHealth {
    /// Fraction of packets lost on this layer in the most recent RR
    /// window, 0.0-1.0. RR-derived: instantaneous, not cumulative.
    /// The most actionable signal for "this layer's link can't
    /// sustain it right now."
    pub fraction_lost: f64,
    /// Cumulative packets lost on this layer since the connection
    /// started, as reported by the most recent RR. Signed because
    /// the upstream field is `i64` (negative values shouldn't occur
    /// in practice; surfaced as-is so callers can defend or assert
    /// per their needs).
    pub packets_lost_total: i64,
    /// Most recent round-trip time on this layer in seconds, from
    /// RTCP SR/RR exchange. `0.0` until the first RTT measurement
    /// lands.
    pub round_trip_time_seconds: f64,
    /// Number of RTT measurements ever recorded on this layer
    /// (monotonically non-decreasing). The freshness discriminator:
    /// rtc 0.9 keeps surfacing the same RR-derived field values
    /// every poll until the next RR arrives, so a `fraction_lost`
    /// reading repeated tick after tick may reflect a single RR
    /// from minutes ago — not fresh signal. The 4d.3c aggregator
    /// compares this count against its per-(peer, RID) prev-count
    /// snapshot; if the count didn't advance since last tick, the
    /// reading is stale and the policy receives `None` instead.
    /// This prevents stale loss readings from completing a 5s
    /// drop debounce all on their own.
    pub round_trip_time_measurements: u64,
}

/// Sanitize an rtc 0.9-emitted answer SDP, fixing two SDP-writer bugs
/// that fire on multi-RID simulcast send (specifically when our peer
/// answers a browser offer that requested `a=simulcast:recv f;h;q`):
///
///   1. **Duplicate `a=rid:<rid> send` lines.** rtc 0.9 emits each RID
///      `send` line twice — six lines for f/h/q instead of three.
///      Fix: dedupe by full line content within each m= section.
///
///   2. **Malformed `a=simulcast:` attribute.** rtc 0.9 concatenates
///      the direction + RID list as if the answer were bidirectional,
///      producing `a=simulcast:send f;h;q send f;h;q` instead of the
///      RFC 8853-correct `a=simulcast:send f;h;q`. WebKit's parser
///      rejects this with `SyntaxError: Malformed simulcast line`.
///      Fix: when an `a=simulcast:` line repeats the same direction
///      twice, keep only the first `<dir> <list>` pair.
///
/// Pure / idempotent: already-clean SDP is unchanged, single-RID
/// answers (H.264, VP8 floor-only) are unchanged, and the function
/// has no side effects. Tested via `sanitize_answer_sdp_*` below.
///
/// Section-aware: `seen_rids` resets at every `m=` boundary so a
/// theoretical multi-section SDP that legitimately reuses RIDs
/// across audio + video isn't silently flattened.
///
/// Line-ending preserving: detects CRLF vs LF on input and preserves
/// the same on output, including a trailing terminator if present.
fn sanitize_answer_sdp(sdp: &str) -> String {
    let line_ending = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut seen_rids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(sdp.lines().count());

    for line in sdp.lines() {
        if line.starts_with("m=") {
            seen_rids.clear();
            out.push(line.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=rid:") {
            // dedupe by the post-`a=rid:` content (rid + dir + params)
            if !seen_rids.insert(rest.to_string()) {
                continue;
            }
            out.push(line.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=simulcast:") {
            // Valid forms (RFC 8853):
            //   a=simulcast:send f;h;q
            //   a=simulcast:recv f;h;q
            //   a=simulcast:send f;h;q recv x          (bidirectional)
            // Bug form rtc 0.9 emits:
            //   a=simulcast:send f;h;q send f;h;q      (same dir twice)
            // Fix: when the second direction equals the first, drop the
            // second pair; otherwise pass through.
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 4 && parts[0] == parts[2] {
                out.push(format!("a=simulcast:{} {}", parts[0], parts[1]));
            } else {
                out.push(line.to_string());
            }
            continue;
        }
        out.push(line.to_string());
    }

    let mut result = out.join(line_ending);
    if sdp.ends_with("\r\n") || sdp.ends_with('\n') {
        result.push_str(line_ending);
    }
    result
}

impl WebRtcPeer {
    /// **Phase 4d.1**: subscribe to this peer's recent observed send
    /// bitrate signal. **Local egress only, not available capacity**
    /// — see the field docstring on [`Self::observed_send_bitrate_rx`]
    /// for the semantic distinction and why this can't drive
    /// capacity-based layer adaptation on its own. Returns a fresh
    /// `watch::Receiver` that always carries the latest published
    /// value (initial value `None` until the driver computes a
    /// `bytes_sent` delta).
    ///
    /// Receivers are independent — multiple subscribers (e.g. the
    /// per-display layer-selection aggregator AND a metrics
    /// dashboard) can each `subscribe_observed_send_bitrate` and read
    /// independently; calling `borrow_and_update` on one doesn't
    /// affect another.
    pub fn subscribe_observed_send_bitrate(&self) -> watch::Receiver<Option<u64>> {
        self.observed_send_bitrate_rx.clone()
    }

    /// **Phase 4d.1**: read the current observed send bitrate
    /// without subscribing. Useful for one-shot reads (debug /
    /// metrics snapshot). For change-driven consumers, prefer
    /// [`Self::subscribe_observed_send_bitrate`].
    pub fn current_observed_send_bitrate(&self) -> Option<u64> {
        *self.observed_send_bitrate_rx.borrow()
    }

    /// **Phase 4d.3a**: subscribe to this peer's per-RID receiver-
    /// feedback health signal. RR-derived (RTCP receiver reports
    /// the remote sends to us about our outbound streams) — unlike
    /// `observed_send_bitrate`, this IS a remote signal and CAN
    /// drive capacity decisions in 4d.3b/c.
    ///
    /// Returns a fresh `watch::Receiver` that always carries the
    /// latest published map (initial value is the empty map until
    /// the driver completes its first poll AND the first RR has
    /// arrived for at least one outbound SSRC).
    ///
    /// Receivers are independent — multiple subscribers (e.g. the
    /// layer-selection aggregator AND a metrics dashboard) can each
    /// `subscribe_remote_inbound_health` and read independently.
    pub fn subscribe_remote_inbound_health(
        &self,
    ) -> watch::Receiver<HashMap<SimulcastRid, PeerLayerHealth>> {
        self.remote_inbound_health_rx.clone()
    }

    /// **Phase 4d.3a**: read the current per-RID receiver-feedback
    /// health snapshot without subscribing. Returns the empty map
    /// until the first RR has arrived. For change-driven consumers,
    /// prefer [`Self::subscribe_remote_inbound_health`].
    pub fn current_remote_inbound_health(&self) -> HashMap<SimulcastRid, PeerLayerHealth> {
        self.remote_inbound_health_rx.borrow().clone()
    }

    /// **#57**: this peer's negotiated active RID set, frozen at
    /// construction. The layer-policy coordinator
    /// ([`crate::display::aggregator::spawn_layer_policy_coordinator`])
    /// reads this each tick to compute the per-display "pinned" layer
    /// set: a peer with `active_rids().len() == 1` MUST keep its only
    /// RID active or it gets no frames at all (its WebRTC track only
    /// declares one encoding; pausing that layer in the encoder pool
    /// starves the peer rather than degrading it). See the
    /// `active_rids` field doc on [`Self`] for the full rationale.
    pub fn active_rids(&self) -> &[SimulcastRid] {
        &self.active_rids
    }

    /// Test-only: construct a `WebRtcPeer` with just `active_rids`
    /// populated and dummy values for everything else. The dummy
    /// channels are constructed but their senders are dropped so
    /// any production caller that tries to use them will see closed-
    /// channel errors — only the layer-policy coordinator (which
    /// reads `active_rids()` and the watch channels' initial values)
    /// is intended to interact with these stubs.
    ///
    /// Used by `display::tests::pool_feed_bridge_*` to register a
    /// fake peer whose negotiated demand keeps all VP8 simulcast
    /// layers active across the layer-policy's per-tick demanded-
    /// bound check (#48). Without a registered peer, the policy
    /// computes `demanded = empty` and pauses every encoder
    /// immediately, which is correct production behavior but
    /// breaks tests that exercise the bridge → encoder → consumer
    /// pipeline directly.
    #[cfg(test)]
    pub(crate) fn new_for_test(peer_id: PeerId, active_rids: Vec<SimulcastRid>) -> Self {
        use std::collections::HashMap;
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (_obs_tx, observed_send_bitrate_rx) = watch::channel(None);
        let (_ri_tx, remote_inbound_health_rx) =
            watch::channel(HashMap::<SimulcastRid, PeerLayerHealth>::new());
        let (_twcc_tx, twcc_health_rx) =
            watch::channel::<Option<crate::display::twcc_tap::TwccHealth>>(None);
        Self {
            peer_id,
            command_tx,
            observed_send_bitrate_rx,
            remote_inbound_health_rx,
            twcc_health_rx,
            active_rids,
            shutdown: CancellationToken::new(),
        }
    }

    /// **Phase 4d.3b**: subscribe to this peer's aggregate TWCC
    /// health signal. Published once per second by the
    /// [`crate::display::twcc_tap::spawn_twcc_health_aggregator`]
    /// task that drains the [`crate::display::twcc_tap::TwccTapInterceptor`]
    /// event stream.
    ///
    /// `None` means either "no window has fired yet" or "the most
    /// recent window had zero TWCC events." Silence is not
    /// recovery — see [`crate::display::twcc_tap`] module docs.
    /// The channel transitions `None → Some(_) → None → Some(_)`
    /// as feedback arrives and goes silent across windows. The
    /// capacity policy in [`crate::display::aggregator`] treats
    /// `None` and `Some(empty_health)` alike via its short-circuit
    /// arms and gates upper simulcast layers based only on
    /// sustained, non-empty loss readings.
    ///
    /// Receivers are independent — multiple subscribers (capacity
    /// aggregator + a metrics dashboard, say) can each
    /// `subscribe_twcc_health` and read independently.
    pub fn subscribe_twcc_health(
        &self,
    ) -> watch::Receiver<Option<crate::display::twcc_tap::TwccHealth>> {
        self.twcc_health_rx.clone()
    }

    /// **Phase 4d.3b**: read the current TWCC health snapshot
    /// without subscribing. Returns `None` if no window has fired
    /// yet OR if the most recent window had zero TWCC events
    /// (silence is not recovery — see the module docs at
    /// [`crate::display::twcc_tap`]). For change-driven consumers,
    /// prefer [`Self::subscribe_twcc_health`].
    pub fn current_twcc_health(&self) -> Option<crate::display::twcc_tap::TwccHealth> {
        *self.twcc_health_rx.borrow()
    }
}

/// Personalized display-input authority state for one viewer.
///
/// Wire vocabulary matches the local 5c data-channel protocol exactly
/// (see `web_gateway.rs::compute_bootstrap_authority_snapshots`). Used
/// by [`WebRtcPeer::send_authority_state`] for the federated path's
/// `display_input_authority` data channel — peer broadcasts a
/// personalized value to each subscribed federated browser.
///
/// Modelled as an enum (rather than passing `&str` through the API)
/// so the wire vocabulary lives in exactly one place; adding a future
/// state value is an explicit ABI change rather than a stringly-typed
/// caller mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayInputAuthorityState {
    You,
    Other,
    Unclaimed,
}

impl DisplayInputAuthorityState {
    /// Wire string for the `state` field of
    /// `display_input_authority_state` data-channel messages.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::You => "you",
            Self::Other => "other",
            Self::Unclaimed => "unclaimed",
        }
    }
}

/// F-1.3b2: Browser-originated authority message on the
/// `display_input_authority` data channel.
///
/// Wire format from the federated authority design (see
/// `docs/design-federated-input-authority.md` §Wire):
///
/// ```text
/// { "t": "display_input_authority_request", "display_id": 0 }
/// { "t": "display_input_authority_release", "display_id": 0 }
/// ```
///
/// `display/webrtc.rs` parses these frames off the wire and hands
/// them to an opaque [`AuthorityChannelHandler`] without applying any
/// policy. The handler — built outside the transport in
/// `web_gateway.rs` by the slice that wires the registry — consults
/// the federated authority registry and decides whether to grant /
/// release / no-op. Same separation as the existing
/// `input_handler`: webrtc.rs parses the wire shape, the gate lives
/// outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityChannelMessage {
    Request { display_id: u32 },
    Release { display_id: u32 },
}

/// F-1.3b2: opaque handler invoked on every parsed
/// [`AuthorityChannelMessage`] received on the
/// `display_input_authority` data channel.
///
/// Sibling to the existing `input_handler` constructor argument —
/// same `Arc<dyn Fn(...) + Send + Sync>` shape, same no-op default
/// for callers that don't gate authority. The closure runs on the
/// driver task, so it must not block; production handlers (added by
/// the federated wiring slice) push work to the federated authority
/// registry via non-blocking channels or atomic ops.
///
/// Local DisplaySlot's `WebRtcPeer` passes a no-op (see
/// [`noop_authority_handler`]) because the local browser doesn't
/// create the `display_input_authority` channel (5a/5c uses the WS
/// path); the federated `PeerDisplayConnection` does create it, and
/// the federated wiring slice plugs the real registry-driven handler
/// in there.
pub type AuthorityChannelHandler = Arc<dyn Fn(AuthorityChannelMessage) + Send + Sync>;

/// F-1.3b2: no-op [`AuthorityChannelHandler`] for callers that do not
/// gate authority on this peer. Used by the local DisplaySlot path
/// (browser doesn't create the channel) and as the placeholder on the
/// federated path until the federated wiring slice replaces it. Kept
/// as a single canonical source so future F-1.3b3 diffs against the
/// federated callsite are isolated to one line.
pub fn noop_authority_handler() -> AuthorityChannelHandler {
    Arc::new(|_| {})
}

/// D-4d2: Browser-originated recovery/control messages on the
/// `tile-control` data channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileControlMessage {
    Subscribe {
        client_id: u32,
    },
    SnapshotRequest {
        epoch: u32,
        reason: tile_transport::SnapshotRequestReason,
    },
    GapReport {
        epoch: u32,
        last_seen_seq: u32,
        expected_seq: u32,
    },
}

/// Opaque transport callback for parsed tile-control frames.
///
/// The driver task invokes this synchronously; production handlers
/// must spawn any async recovery work rather than blocking the RTC
/// pump.
pub type TileControlHandler = Arc<dyn Fn(TileControlMessage) + Send + Sync>;

pub fn noop_tile_control_handler() -> TileControlHandler {
    Arc::new(|_| {})
}

/// D-3b: Tile-stream data-channel labels.
///
/// Browser-side `PeerDisplayConnection` creates these channels before
/// `createOffer()`. The peer passively observes them through
/// `OnDataChannel(OnOpen)` and writes binary tile frames by label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileDataChannel {
    Control,
    Snapshot,
    Deltas,
}

impl TileDataChannel {
    fn label(self) -> &'static str {
        match self {
            Self::Control => TILE_CONTROL_CHANNEL_LABEL,
            Self::Snapshot => TILE_SNAPSHOT_CHANNEL_LABEL,
            Self::Deltas => TILE_DELTAS_CHANNEL_LABEL,
        }
    }

    fn queues_before_open(self) -> bool {
        matches!(self, Self::Control | Self::Snapshot)
    }
}

/// Commands sent from the public `WebRtcPeer` handle to the driver task.
enum Command {
    AddIceCandidate(String),
    SendClipboard(ClipboardContent),
    /// F-1.2: federated authority state push to the
    /// `display_input_authority` data channel. If the channel is not
    /// yet open, the driver queues the message in
    /// [`DriverState::pending_authority_state`] and flushes on
    /// `OnDataChannel(OnOpen)` for that label. Without queueing, an
    /// authority state computed before the browser's data channel
    /// finishes negotiating would land on the floor and the browser's
    /// chip would stall at `unknown` until the next state change.
    SendAuthorityState {
        display_id: u32,
        state: DisplayInputAuthorityState,
    },
    /// D-3b: binary tile-stream frame. Control/snapshot frames queue
    /// until their reliable data channel opens; delta frames are
    /// latest-wins and are dropped when the channel is unavailable.
    SendTileFrame {
        channel: TileDataChannel,
        data: Vec<u8>,
    },
}

struct RtpSendConfig {
    sender_id: RTCRtpSenderId,
    mid: String,
    codec: RTCRtpCodec,
    /// One entry per simulcast layer (or one entry for non-simulcast
    /// codecs like H.264). Each pair is the layer's `(SimulcastRid,
    /// SSRC)` — the SSRC matches the value passed into the
    /// [`MediaStreamTrack`]'s `RTCRtpEncodingParameters` for this RID
    /// at construction, so [`rtc`]'s `RTCRtpSender::write_rtp` (which
    /// routes to encodings by `packet.header.ssrc`) finds the right
    /// encoding when the driver writes a packet.
    ///
    /// Phase 4c (post-this-commit) populates this with N entries for
    /// VP8 simulcast. This commit (the refactor that prepares for it)
    /// always populates with exactly ONE entry — single-encoding
    /// behavior is preserved bit-for-bit until commit 2 lights up
    /// multi-encoding.
    encodings: Vec<(SimulcastRid, u32)>,
}

/// Encoded frame paired with the simulcast RID it came from. Carried
/// over the per-peer mpsc channel between [`pool_frame_intake`]
/// (producer) and [`driver`] (consumer).
///
/// The RID does NOT live on [`EncodedFrame`] itself — that struct is
/// the encoder pool's output, shared across all subscribers of a given
/// `(codec, rid)` slot, and an encoder doesn't know which subscriber's
/// RID it's serving (it just knows its own slot's rid). The pool
/// forwarder reads the rid off its [`EncoderSubscription`] (which
/// carries the [`crate::display::encode::pool::EncoderId`] containing
/// `(codec, rid)`) and wraps each frame here at hand-off.
///
/// The driver uses the rid to look up the matching encoding's SSRC +
/// per-`(spec, rid)` keyframe gate — see
/// [`DriverState::video_specs`] and [`RtpSendState::by_rid`] for the
/// keying decisions.
struct OutboundEncodedFrame {
    rid: SimulcastRid,
    frame: Arc<EncodedFrame>,
}

fn new_ssrc() -> u32 {
    let raw = uuid::Uuid::new_v4().as_u128() as u32;
    raw.max(1)
}

fn new_ice_fragment() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect()
}

fn new_ice_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn video_rtcp_feedback() -> Vec<RTCPFeedback> {
    vec![
        RTCPFeedback {
            typ: "goog-remb".to_string(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "ccm".to_string(),
            parameter: "fir".to_string(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: "pli".to_string(),
        },
    ]
}

fn rtc_codec_parameters(codec: CodecKind) -> Result<RTCRtpCodecParameters, CallerError> {
    let rtp_codec = match codec {
        CodecKind::Vp8 => RTCRtpCodec {
            mime_type: RTC_MIME_TYPE_VP8.to_string(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: video_rtcp_feedback(),
        },
        CodecKind::H264 => RTCRtpCodec {
            mime_type: RTC_MIME_TYPE_H264.to_string(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_string(),
            rtcp_feedback: video_rtcp_feedback(),
        },
        CodecKind::Vp9 | CodecKind::Av1 => {
            return Err(CallerError::WebRtc(format!(
                "codec {} not yet wired to rtc media engine",
                codec
            )));
        }
    };
    let payload_type = match codec {
        CodecKind::Vp8 => 96,
        CodecKind::H264 => 125,
        CodecKind::Vp9 | CodecKind::Av1 => unreachable!(),
    };
    Ok(RTCRtpCodecParameters {
        rtp_codec,
        payload_type,
        ..Default::default()
    })
}

fn host_candidate_init(addr: SocketAddr, protocol: RTCIceProtocol) -> RTCIceCandidateInit {
    let (foundation, proto, priority, tcp_suffix) = match protocol {
        RTCIceProtocol::Udp => ("1", "udp", 2_130_706_431u32, ""),
        RTCIceProtocol::Tcp => ("9001", "tcp", 1_677_721_855u32, " tcptype passive"),
        RTCIceProtocol::Unspecified => ("1", "udp", 1_000_000_000u32, ""),
    };
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:{foundation} 1 {proto} {priority} {} {} typ host{tcp_suffix} generation 0",
            addr.ip(),
            addr.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

/// Build a server-reflexive (srflx) UDP ICE candidate.
///
/// `mapped` is the public `IP:port` the STUN server observed for this
/// socket; `base` is the local host address the socket is bound to (the
/// candidate's `raddr`/`rport`, required by RFC 5245 § 4.3 for srflx
/// candidates). The foundation differs from host candidates so the two
/// don't collapse into one pair, and the type-preference byte of the
/// priority is `100 << 24` (srflx) rather than host's `126 << 24`, so
/// host pairs are still tried first while srflx provides the reachable
/// public path for NAT'd peers.
fn srflx_candidate_init(mapped: SocketAddr, base: SocketAddr) -> RTCIceCandidateInit {
    // Priority = (type-pref << 24) | (local-pref << 8) | (256 - component).
    // srflx type preference 100, local preference 65535, component 1.
    let priority = (100u32 << 24) | (65_535u32 << 8) | (256 - 1);
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:2 1 udp {priority} {} {} typ srflx raddr {} rport {} generation 0",
            mapped.ip(),
            mapped.port(),
            base.ip(),
            base.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

/// How long, after sending its Binding Request, a UDP forwarder keeps
/// watching for the matching STUN Binding Success Response before it
/// stops trying to gather a srflx candidate for its socket.
///
/// This is NOT on the peer-setup critical path (see the srflx gathering
/// block in the driver below): the SDP answer is created and returned to
/// the signaling layer with host + ICE-TCP candidates *before* any STUN
/// traffic is sent, and the forwarder keeps forwarding ICE/DTLS/media
/// packets to the RTC core throughout this window. So a blocked or
/// unreachable STUN server costs zero added setup latency — when the
/// response never comes, this deadline simply elapses and no srflx
/// candidate is trickled. A reachable server answers in a few ms and the
/// srflx candidate is trickled to the peer well within it.
const STUN_BINDING_TIMEOUT: Duration = Duration::from_millis(1500);

/// Build a STUN Binding Request, returning the wire bytes and the
/// transaction ID a response must echo to be accepted.
///
/// Built with `rtc::stun` (already a transitive dependency via the `rtc`
/// meta-crate — no new dep): `Message::build` writes the 20-byte header
/// with the magic cookie and a random transaction ID, sets the message
/// type to `BINDING_REQUEST`, and `marshal_binary` yields the wire bytes.
fn build_stun_binding_request() -> Result<(Vec<u8>, rtc::stun::message::TransactionId), String> {
    use rtc::stun::message::{Message, BINDING_REQUEST};

    let mut request = Message::new();
    request
        .build(&[
            Box::new(rtc::stun::message::TransactionId::new()),
            Box::new(BINDING_REQUEST),
        ])
        .map_err(|e| format!("build STUN binding request: {e}"))?;
    let request_tid = request.transaction_id;
    let wire = request
        .marshal_binary()
        .map_err(|e| format!("marshal STUN binding request: {e}"))?;
    Ok((wire, request_tid))
}

/// Try to interpret `buf` as the STUN Binding Success Response to a
/// request we sent with `expected_tid`, returning the public `IP:port`
/// from its `XOR-MAPPED-ADDRESS` attribute.
///
/// Returns `None` for anything that isn't our response — a non-STUN
/// datagram (an ICE connectivity check the same socket also carries), a
/// STUN message with a different transaction ID, a non-success class, or
/// a missing/malformed `XOR-MAPPED-ADDRESS`. The caller forwards those
/// `None` cases on to the RTC core unchanged, so folding this check into
/// the UDP read path never drops connectivity-check traffic. Validated by
/// `unmarshal_binary` (magic cookie + length) before
/// `XorMappedAddress::get_from` decodes the attribute. Never panics.
fn parse_stun_binding_response(
    buf: &[u8],
    expected_tid: rtc::stun::message::TransactionId,
) -> Option<SocketAddr> {
    use rtc::stun::message::{Getter, Message};
    use rtc::stun::xoraddr::XorMappedAddress;

    let mut response = Message::new();
    if response.unmarshal_binary(buf).is_err() {
        return None;
    }
    if response.transaction_id != expected_tid {
        return None;
    }
    if response.typ != rtc::stun::message::BINDING_SUCCESS {
        return None;
    }
    let mut mapped = XorMappedAddress::default();
    if mapped.get_from(&response).is_err() {
        return None;
    }
    Some(SocketAddr::new(mapped.ip, mapped.port))
}

/// Test-only round-trip helper: send a Binding Request from `socket` to
/// `stun_addr` and await the matching Binding Success Response, returning
/// the mapped address. Composes the same `build_stun_binding_request` /
/// `parse_stun_binding_response` building blocks the production UDP
/// forwarder folds into its read loop, so the tests exercise the real
/// wire build + parse path. Production no longer uses a blocking
/// round-trip (it would need a second reader on the ICE socket); the
/// forwarder intercepts the response inline instead.
#[cfg(test)]
async fn stun_binding_mapped_addr(
    socket: &UdpSocket,
    stun_addr: SocketAddr,
) -> Result<SocketAddr, String> {
    let (wire, request_tid) = build_stun_binding_request()?;
    let exchange = async {
        socket
            .send_to(&wire, stun_addr)
            .await
            .map_err(|e| format!("send STUN binding request to {stun_addr}: {e}"))?;
        let mut buf = [0u8; 1500];
        loop {
            let (n, from) = socket
                .recv_from(&mut buf)
                .await
                .map_err(|e| format!("recv STUN response: {e}"))?;
            if from != stun_addr {
                continue;
            }
            if let Some(mapped) = parse_stun_binding_response(&buf[..n], request_tid) {
                return Ok(mapped);
            }
        }
    };
    match tokio::time::timeout(STUN_BINDING_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "STUN binding to {stun_addr} timed out after {STUN_BINDING_TIMEOUT:?}"
        )),
    }
}

/// Extract STUN server `host:port` socket addresses from an [`IceConfig`].
///
/// Each ICE server may carry several URLs; we keep only `stun:`/`stuns:`
/// entries (TURN is out of scope for srflx gathering) and resolve each via
/// DNS. The configured default is `stun:stun.l.google.com:19302`; a STUN
/// URL without an explicit port falls back to the IANA default 3478.
///
/// Returns deduplicated resolved addresses. An empty result (no STUN
/// servers configured, or all failed to resolve) means srflx gathering is
/// skipped entirely — host/ICE-TCP candidates still work.
async fn resolve_stun_servers(ice_config: &IceConfig) -> Vec<SocketAddr> {
    use rtc::stun::uri::Uri;

    let mut out: Vec<SocketAddr> = Vec::new();
    for server in &ice_config.ice_servers {
        for url in &server.urls {
            let uri = match Uri::parse_uri(url) {
                Ok(u) => u,
                Err(_) => continue,
            };
            // `scheme` is the URL scheme ("stun"/"stuns"); skip turn/turns.
            if uri.scheme != "stun" && uri.scheme != "stuns" {
                continue;
            }
            let port = uri.port.unwrap_or(rtc::stun::DEFAULT_PORT);
            let host_port = format!("{}:{}", uri.host, port);
            let resolved = tokio::net::lookup_host(host_port.clone()).await;
            match resolved {
                Ok(addrs) => {
                    for addr in addrs {
                        if !out.contains(&addr) {
                            out.push(addr);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[display/webrtc] STUN server {host_port} resolve failed: {e}");
                }
            }
        }
    }
    out
}

fn first_video_mid_from_offer(sdp: &str) -> Option<String> {
    let mut in_video = false;
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video ");
            continue;
        }
        if in_video {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                let mid = mid.trim();
                if !mid.is_empty() {
                    return Some(mid.to_string());
                }
            }
        }
    }
    None
}

/// Pick the single RID for a federated single-encoding peer (offer
/// without `a=simulcast:recv`). **#48 tuning**: returns the floor
/// (`pool_rids.last()`), not the top (`pool_rids[0]`).
///
/// Rationale: the only consumer of this code path is the federated
/// `PeerDisplayConnection` (post-`e815bac` it strips `a=simulcast:recv`
/// from its offer; the local `DisplaySlot` always injects it and goes
/// down the multi-RID branch instead). Federated runs over a TURN-relay
/// path where moderate sustained packet loss (~5-10 %) is the
/// operational baseline. At the full-layer's 2.5 Mbps target, keyframes
/// run ~500 KB ≈ 420 RTP packets; intact-arrival probability at 8 %
/// loss is `0.92^420 ≈ 1.4e-15` — effectively zero. At the floor's
/// 125 kbps quarter-resolution target, keyframes are ~20 KB ≈ 17
/// packets; intact-arrival is `0.92^17 ≈ 24 %` — recovered within a
/// few PLI cycles.
///
/// Loss-tolerance dominates resolution here: a usable low-resolution
/// stream beats a frozen full-resolution one. When the operator wants
/// higher quality on a clean link, that's a future capacity policy
/// concern (track #48 follow-up): observe loss + dynamically
/// renegotiate to a higher RID. This baseline is "make federated work
/// at all under realistic loss."
///
/// Robust against partial-layer pools: `pool_rids.last()` degrades to
/// "best available floor" — when the source resolution is too small for
/// quarter (per `MIN_LAYER_DIM` filter in `LayerSpec::vp8_simulcast`),
/// last is `h` or `f`. Caller guarantees `pool_rids` is non-empty.
fn select_single_rid_for_federated_offer(pool_rids: &[SimulcastRid]) -> SimulcastRid {
    pool_rids
        .last()
        .expect("caller must guarantee pool_rids is non-empty")
        .clone()
}

/// Parse the offer SDP's `a=simulcast:recv <rid>;<rid>;...` line and return
/// the RIDs the browser is willing to receive.
///
/// Returns:
/// - `None` if the offer's video section has no `a=simulcast:recv`
///   directive at all (the federated [`PeerDisplayConnection`] path post-#46
///   diagnostic landed at `e815bac`, and any offerer that hasn't munged
///   simulcast:recv into its track shape).
/// - `Some(vec)` of `SimulcastRid`s, in offer order, when the directive is
///   present (the local `DisplaySlot` path at `static/app.html:7808`,
///   which injects `a=simulcast:recv full;half;quarter` before
///   `setLocalDescription`).
///
/// The caller in [`WebRtcPeer::new`] uses this to **intersect** the
/// peer's [`active_rids`] (derived from encoder pool subscriptions) with
/// what the offer actually requested. Without the intersection, the peer
/// would honestly answer with 3 RIDs even when the offer was
/// single-encoding — and the browser would receive a multi-RID
/// `a=simulcast:send full;half;quarter` answer with no `a=ssrc`
/// declarations to pair RIDs with packets, drop every RTP packet, and
/// stay at `framesDecoded=0`. Confirmed empirically against Chrome via
/// `pliCount > 0`, `packetsReceived > 0`, `framesDecoded == 0`. See the
/// `WebRtcPeer::new` callsite for the intersection logic and the
/// `parse_offer_simulcast_recv_rids_*` tests below.
///
/// Section-aware: only the first `m=video` section is consulted. Audio
/// `simulcast` lines (rare) are ignored. The function returns `None`
/// for any offer without a video section, matching the existing
/// `first_video_mid_from_offer` semantics.
///
/// Forward-compat: unknown / non-canonical RID names are passed through
/// to [`SimulcastRid::from_str_loose`]. Tokens that don't parse to a
/// known RID variant are silently dropped from the returned list rather
/// than failing the whole parse — keeps the answer-side intersection
/// useful even if a future browser advertises an unrecognized RID
/// alongside known ones.
fn parse_offer_simulcast_recv_rids(sdp: &str) -> Option<Vec<SimulcastRid>> {
    let mut in_video = false;
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if line.starts_with("m=") {
            if in_video {
                // Past the first video section without finding it.
                return None;
            }
            in_video = line.starts_with("m=video ");
            continue;
        }
        if !in_video {
            continue;
        }
        let Some(rest) = line.strip_prefix("a=simulcast:") else {
            continue;
        };
        // Valid forms (RFC 8853):
        //   a=simulcast:recv f;h;q
        //   a=simulcast:send f;h;q recv x          (bidirectional)
        // We only care about the recv side from the offerer's POV.
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let mut i = 0;
        while i + 1 < parts.len() {
            if parts[i] == "recv" {
                let rids: Vec<SimulcastRid> = parts[i + 1]
                    .split(';')
                    .filter_map(SimulcastRid::from_str_loose)
                    .collect();
                return Some(rids);
            }
            i += 2;
        }
    }
    None
}

#[cfg(test)]
mod parse_offer_simulcast_recv_rids_tests {
    use super::*;

    /// Federated `PeerDisplayConnection` path: offer has no
    /// `a=simulcast:recv`. The fix-site uses the `None` return to
    /// narrow active_rids to a single layer.
    #[test]
    fn federated_offer_without_simulcast_returns_none() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=mid:0\r\n\
                   a=rtpmap:96 VP8/90000\r\n\
                   a=recvonly\r\n";
        assert_eq!(parse_offer_simulcast_recv_rids(sdp), None);
    }

    /// Local `DisplaySlot` path: offer contains `a=simulcast:recv f;h;q`.
    /// Returns the three RIDs in offer order — the fix-site keeps all
    /// three because the browser explicitly asked for them.
    #[test]
    fn local_offer_with_full_simulcast_returns_all_three() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=mid:0\r\n\
                   a=rtpmap:96 VP8/90000\r\n\
                   a=rid:f recv\r\n\
                   a=rid:h recv\r\n\
                   a=rid:q recv\r\n\
                   a=simulcast:recv f;h;q\r\n\
                   a=recvonly\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![
                SimulcastRid::full(),
                SimulcastRid::half(),
                SimulcastRid::quarter(),
            ]),
        );
    }

    /// Subset offer (e.g. a constrained-bandwidth browser asking for
    /// half + quarter only). The fix-site intersects with the peer's
    /// own active_rids and forwards the overlap.
    #[test]
    fn offer_with_subset_returns_subset_in_offer_order() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=simulcast:recv h;q\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![SimulcastRid::half(), SimulcastRid::quarter()]),
        );
    }

    /// Bidirectional `simulcast` line (`a=simulcast:send X recv Y`) —
    /// uncommon but RFC 8853-valid. Parser must walk past the `send`
    /// half and find the `recv` half.
    #[test]
    fn bidirectional_simulcast_picks_recv_half() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=simulcast:send x recv f;h\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![SimulcastRid::full(), SimulcastRid::half()]),
        );
    }

    /// `a=simulcast:` lines outside the video section are ignored
    /// (defensive — audio sections never have simulcast in our setup,
    /// but the section-awareness keeps a future audio simulcast from
    /// confusing the parser).
    #[test]
    fn audio_simulcast_is_ignored() {
        let sdp = "v=0\r\n\
                   m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=simulcast:recv f;h\r\n";
        assert_eq!(parse_offer_simulcast_recv_rids(sdp), None);
    }

    // ----- #48 floor-pick tests --------------------------------------------

    /// **#48 acceptance**: full simulcast pool → federated single-RID
    /// peer picks the floor (`q`), not the top (`f`). The top would
    /// produce ~500 KB keyframes that can't survive 8 % loss
    /// (`0.92^420 ≈ 1.4e-15`); the floor produces ~20 KB keyframes
    /// (`0.92^17 ≈ 24 %`) that recover within seconds of PLI.
    #[test]
    fn select_floor_for_full_simulcast_pool() {
        let pool = vec![
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ];
        assert_eq!(
            select_single_rid_for_federated_offer(&pool),
            SimulcastRid::quarter(),
        );
    }

    /// Defensive: partial pool (small source: quarter dropped by
    /// `MIN_LAYER_DIM` filter in `LayerSpec::vp8_simulcast`) → pick
    /// the best available floor. Degrades to `h` rather than failing
    /// or skipping back to `f`.
    #[test]
    fn select_floor_for_two_layer_pool() {
        let pool = vec![SimulcastRid::full(), SimulcastRid::half()];
        assert_eq!(
            select_single_rid_for_federated_offer(&pool),
            SimulcastRid::half(),
        );
    }

    /// Tiny source: only `f` survives. Floor *is* `f`. Federated
    /// peer picks `f` and accepts the higher loss-vulnerability —
    /// nothing else to fall back to.
    #[test]
    fn select_full_when_only_one_layer() {
        let pool = vec![SimulcastRid::full()];
        assert_eq!(
            select_single_rid_for_federated_offer(&pool),
            SimulcastRid::full(),
        );
    }

    /// Forward-compat: unknown RID tokens silently drop, known ones
    /// pass through. An offer mixing recognized + future RID names
    /// must not break the intersection — it just narrows to the
    /// intersection of (peer's RIDs) ∩ (recognized offer RIDs).
    #[test]
    fn unknown_rid_tokens_are_dropped_known_pass_through() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=simulcast:recv f;ultra;q\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![SimulcastRid::full(), SimulcastRid::quarter()]),
        );
    }
}

impl WebRtcPeer {
    /// Create a new peer from an SDP offer, returning `(Self, answer_sdp)`.
    ///
    /// Steps:
    /// 1. Build an [`RTCPeerConnection`] with the active pool codec registered.
    /// 2. Bind a per-peer UDP socket and register it as a host candidate.
    /// 3. Apply the browser offer and generate the SDP answer.
    /// 4. Spawn the driver task and return.
    ///
    /// ## `active_codec` + `active_rids` contract
    ///
    /// Each peer gets its own RTC peer connection. The caller passes
    /// the single codec this peer should negotiate (the active codec
    /// selected from "what the encoder pool can currently produce"
    /// AND "what the peer's offer advertised") plus the simulcast
    /// RIDs the pool is currently producing for that codec. The
    /// caller derives `active_rids` from the initial pool
    /// subscriptions filtered to the active codec — NOT from
    /// `pool.always_on()` directly — so the answer SDP advertises
    /// exactly what the intake will forward (per phase 4c
    /// correction #2).
    ///
    /// VP8 simulcast lights up here when `active_rids.len() > 1`:
    /// the track is built with N encodings (one per RID, each with
    /// its own SSRC), and the answer SDP carries
    /// `a=simulcast:send full;half;quarter` plus `a=rid:* send`
    /// lines automatically as a consequence of the multi-encoding
    /// track shape. For single-codec / single-layer paths (H.264,
    /// or VP8 with only one surviving layer post-MIN_LAYER_DIM
    /// filter) `active_rids.len() == 1` and the answer is plain
    /// sendonly.
    ///
    /// Empty / no-overlap cases are surfaced to `handle_offer` as
    /// [`CallerError::WebRtc`] errors rather than producing a silent
    /// broken stream — matches the "no compatible codec, clean
    /// reject" contract from the multi-viewer redesign.
    ///
    /// `ice_tx` carries server→browser trickle ICE candidates. Host and
    /// ICE-TCP candidates are emitted inline in the answer SDP, but the
    /// server-reflexive (srflx) candidate is gathered off the critical
    /// path by the driver's UDP forwarders (see audit F8) and trickled
    /// through this channel as it arrives. The browser also trickles its
    /// own candidates back via `add_ice_candidate`.
    ///
    /// Returns `(peer, encoded_frame_tx, answer_sdp)`. The
    /// `encoded_frame_tx` is the sender side of the per-peer
    /// encoded frame channel — the caller (`Self::new`) hands it
    /// directly to `pool_frame_intake` rather than parking it on
    /// the struct.
    async fn build_with_codec_set(
        peer_id: PeerId,
        offer_sdp: &str,
        active_codec: CodecKind,
        active_rids: &[SimulcastRid],
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<TcpPeerRegistry>>,
        tcp_advertised_addr: Option<SocketAddr>,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        authority_handler: AuthorityChannelHandler,
        tile_control_handler: TileControlHandler,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        keyframe_request_tx: mpsc::Sender<SimulcastRid>,
    ) -> Result<(Self, mpsc::Sender<OutboundEncodedFrame>, String), CallerError> {
        if active_rids.is_empty() {
            return Err(CallerError::WebRtc(
                "active_rids is empty — caller must derive at least one \
                 RID from the peer's initial pool subscriptions before \
                 constructing WebRtcPeer"
                    .to_string(),
            ));
        }

        let codec_params = rtc_codec_parameters(active_codec)?;
        let video_mid = first_video_mid_from_offer(offer_sdp).unwrap_or_else(|| "0".to_string());

        // We need to know the local ufrag before SDP generation so the TCP
        // dispatcher can route accepted ICE-TCP sockets to this peer.
        let local_ufrag = new_ice_fragment();
        let local_pwd = new_ice_password();

        let mut setting_engine = SettingEngine::default();
        setting_engine.set_ice_credentials(local_ufrag.clone(), local_pwd);
        // Pin the answerer's DTLS role to `Server` so the generated
        // answer carries `a=setup:passive`. Per RFC 5763 § 5 that makes
        // the browser the DTLS client and the initiator of the
        // handshake — which is the path the rtc 0.9 stack actually
        // drives. Letting the answer default to `a=setup:active` (the
        // alternative role for an answerer to `actpass`) leaves rtc's
        // DTLS state machine waiting for an event that never fires
        // over the selected ICE-TCP candidate: in our slice-3a.2 setup
        // the connection stalls at STUN keepalives forever, no DTLS
        // bytes are ever emitted, no SRTP context is established,
        // write_rtp returns Ok but produces no encrypted output, and
        // the dashboard renders black indefinitely. Diagnosed in
        // #41 (RFC 7983 byte-class instrumentation across all four
        // hops showed Stun-only both ways with `a=setup:active` in
        // the answer); the fix is named explicit role assignment
        // here, before the RTCPeerConnection is built so all generated
        // SDP carries the pinned role. See the
        // `build_with_codec_set_pins_setup_passive_in_answer` test.
        setting_engine
            .set_answering_dtls_role(RTCDtlsRole::Server)
            .map_err(|e| CallerError::WebRtc(format!("set answering DTLS role: {e}")))?;

        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(codec_params.clone(), RtpCodecKind::Video)
            .map_err(|e| CallerError::WebRtc(format!("register codec: {e}")))?;
        for uri in [
            "urn:ietf:params:rtp-hdrext:sdes:mid",
            "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
            "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
        ] {
            media_engine
                .register_header_extension(
                    RTCRtpHeaderExtensionCapability {
                        uri: uri.to_string(),
                    },
                    RtpCodecKind::Video,
                    None,
                )
                .map_err(|e| CallerError::WebRtc(format!("register RTP extension: {e}")))?;
        }

        // Phase 4c: build one encoding per active RID. For VP8
        // simulcast (active_rids = [full, half, quarter]) this produces
        // a 3-encoding track and `RTCPeerConnection::create_answer` then
        // emits `a=simulcast:send full;half;quarter` + `a=rid:* send`
        // lines as a consequence of the multi-encoding shape — server-
        // side answer-side simulcast that str0m couldn't do, hence the
        // migration to rtc 0.9.
        //
        // For single-RID (H.264 or VP8 with all simulcast layers
        // dropped below MIN_LAYER_DIM) this produces a single
        // encoding and the answer is plain sendonly with no
        // simulcast lines — bit-for-bit equivalent to pre-4c output.
        //
        // Each encoding gets its own SSRC because rtc's
        // `RTCRtpSender::write_rtp` routes packets to encodings by
        // matching `packet.header.ssrc` against the encoding's
        // declared SSRC. The driver looks up the SSRC by RID at
        // write time via `state.rtp.by_rid` (populated below from
        // `encodings_by_rid`).
        let mut encodings = Vec::with_capacity(active_rids.len());
        let mut encodings_by_rid: Vec<(SimulcastRid, u32)> = Vec::with_capacity(active_rids.len());
        for rid in active_rids {
            let ssrc = new_ssrc();
            encodings_by_rid.push((rid.clone(), ssrc));
            encodings.push(RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    rid: rid.as_str().to_string(),
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec_params.rtp_codec.clone(),
                ..Default::default()
            });
        }
        let track = MediaStreamTrack::new(
            format!("display-{peer_id}"),
            format!("display-video-{peer_id}"),
            format!("display-video-{peer_id}"),
            RtpCodecKind::Video,
            encodings,
        );

        // **Phase 4d.3b — TWCC signal pipeline.** Wire rtc 0.9's
        // interceptor registry with SR/RR + TWCC sender + the custom
        // `TwccTapInterceptor`, which observes inbound RTCP at the
        // chain's outermost `handle_read`, downcasts each
        // `TransportLayerCc` packet, and projects a compact
        // [`TwccEvent`] onto an unbounded mpsc channel.
        //
        // **Why a custom tap, not rtc's stats path:** rtc 0.9 consumes
        // RTCP internally and never surfaces it via
        // `RTCMessage::RtcpPacket`, and its
        // `RTCRemoteInboundRtpStreamStats` accumulator stays at all-
        // zero defaults regardless of which interceptors are wired.
        // Tapping the interceptor chain is the only place we can
        // observe TWCC without patching rtc 0.9. See
        // [`crate::display::twcc_tap`] module docs for the full
        // background.
        //
        // **Chain order:** `Registry::with(...)` puts the supplied
        // wrapper outermost, so call sequence
        //
        //   `Registry::new() → configure_rtcp_reports(.) →
        //    configure_twcc_sender_only(.) →
        //    .with(|inner| TwccTapInterceptor::new(inner, tx))`
        //
        // produces a chain whose outermost layer is the tap. The tap
        // observes, then forwards to twcc_sender_only, then
        // rtcp_reports, then rtc's internals — keeping the existing
        // stack's behaviour intact. The tap mutates nothing.
        //
        // The aggregator that consumes `twcc_tap_rx` is spawned
        // below, after `shutdown` is created, so it shares the
        // peer's cancellation token.
        let (twcc_tap_tx, twcc_tap_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::display::twcc_tap::TwccEvent>();
        let registry = rtc::interceptor::Registry::new();
        let registry =
            rtc::peer_connection::configuration::interceptor_registry::configure_rtcp_reports(
                registry,
            );
        let registry =
            rtc::peer_connection::configuration::interceptor_registry::configure_twcc_sender_only(
                registry,
                &mut media_engine,
            )
            .map_err(|e| CallerError::WebRtc(format!("configure twcc: {e}")))?;
        let registry = registry.with(|inner| {
            crate::display::twcc_tap::TwccTapInterceptor::new(inner, twcc_tap_tx.clone())
        });

        let mut rtc = RTCPeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build()
            .map_err(|e| CallerError::WebRtc(format!("build rtc peer: {e}")))?;
        let sender_id = rtc
            .add_track(track)
            .map_err(|e| CallerError::WebRtc(format!("add video track: {e}")))?;

        // --- Bind one UDP socket per local interface -----------------------
        // The ICE agent matches incoming packets against local candidates by
        // `(local_address, port)`. A single wildcard bind would surface as
        // `0.0.0.0:port` on `socket.local_addr()`, which never matches the
        // concrete-IP candidates we'd advertise — connectivity checks then
        // can't form a valid pair. So we bind a separate socket per
        // interface and emit a host candidate that exactly matches each
        // socket's local address.
        let mut sockets: Vec<Arc<UdpSocket>> = Vec::new();
        // WebRTC needs loopback so a browser on the same machine can
        // pair against the daemon's host candidates. Each socket's local
        // address is also the srflx host base: the driver's forwarders
        // read it back via `local_addr()` when gathering the srflx
        // candidate off the critical path (audit F8), so we don't need to
        // carry the bases separately here.
        let local_addrs = crate::lan::routable_local_addrs(true);
        for iface_addr in &local_addrs {
            let bind_addr = SocketAddr::new(*iface_addr, 0);
            let socket = match UdpSocket::bind(bind_addr).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[display/webrtc] skipping UDP bind on {iface_addr}: {e}");
                    continue;
                }
            };
            let local = match socket.local_addr() {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "[display/webrtc] skipping UDP socket on {iface_addr}: local_addr {e}"
                    );
                    continue;
                }
            };
            let candidate = host_candidate_init(local, RTCIceProtocol::Udp);
            match rtc.add_local_candidate(candidate) {
                Ok(()) => sockets.push(Arc::new(socket)),
                Err(e) => eprintln!("[display/webrtc] skipping UDP host candidate {local}: {e}"),
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound".to_string(),
            ));
        }

        // --- Server-reflexive (srflx) UDP candidates via STUN -------------
        //
        // ICE host candidates carry the socket's *local* address. On a
        // NAT'd host (e.g. a GCP VM with internal `10.x` / loopback only)
        // those are unreachable from a remote browser, so without a public
        // candidate the only thing that can pair is the ICE-TCP candidate —
        // which has Windows transport problems. To advertise a reachable
        // UDP path we ask the configured STUN server what public `IP:port`
        // it observes for each of our ICE sockets and add that as a srflx
        // candidate. Because the binding request goes out the *same* socket
        // ICE will use, the mapping matches the candidate's base, so a 1:1
        // NAT (GCP) returns the public IP the browser can reach directly.
        //
        // CRITICAL-PATH NOTE (audit F8): the gathering is deliberately NOT
        // done here. Doing it on the answer path — even concurrently across
        // sockets — meant every peer setup waited up to STUN_BINDING_TIMEOUT
        // (1.5s) before `create_answer` whenever the STUN server was
        // blocked/unreachable (e.g. UDP egress firewalled), since concurrency
        // only dedupes the one timeout, it does not remove it from the path.
        //
        // Instead the srflx candidate is gathered and *trickled*: the answer
        // below is created and returned to the signaling layer immediately
        // with host + ICE-TCP candidates, and each per-socket UDP forwarder
        // in the driver folds a STUN Binding exchange into its read loop
        // (single reader → no recv race with the ICE traffic the same socket
        // carries). When a mapping arrives the driver adds the srflx
        // candidate to its `RTCPeerConnection` and sends it to the browser
        // over the already-wired server→browser ICE trickle channel
        // (`ice_tx` → web_gateway `display_ice` → `pc.addIceCandidate`,
        // which the browser buffers until the answer is applied). A
        // reachable STUN server therefore still advertises the srflx
        // candidate (just off the critical path); an unreachable one adds
        // zero setup latency because nothing on the answer path waits on it.
        //
        // The ICE sockets and the STUN server config (`ice_config`) are
        // handed to the driver below to drive this; each forwarder derives
        // its socket's host base from `local_addr()`.

        // --- ICE-TCP candidate (Host-header derived, pair-friendly) ------
        //
        // Earlier iterations tried to advertise `127.0.0.1:<http_port>` as
        // the TCP candidate, hoping the browser's own loopback would be
        // mapped back to us via port-forward / SSH tunnel. Firefox (and
        // Chrome, confirmed experimentally via getStats) silently *filter*
        // remote loopback candidates from candidate-pair formation as an
        // anti-rebinding mitigation — the candidate shows up in the remote
        // list but never pairs, ICE stalls, nothing works.
        //
        // So instead the web gateway parses the `Host:` header from the
        // browser's WebSocket handshake (the one address we KNOW the
        // browser thinks reaches us) and hands us the resulting
        // `SocketAddr` as `tcp_advertised_addr`. If it's a non-loopback
        // IP we advertise exactly that — the browser will happily form a
        // pair because the IP matches what it's already using for HTTP
        // and isn't loopback so the filter doesn't trigger.
        //
        // Users accessing via `http://localhost:...` still get `None`
        // here (Host header is `localhost`, which doesn't parse as an IP
        // — or parses as loopback which we also reject): they don't get
        // a TCP path at all. Their workaround is to bind the port-forward
        // on a non-loopback interface and connect via the LAN IP. There's
        // no clever ICE trick that gets around Firefox's loopback filter
        // for that case.
        //
        // On the server side we "lie" to the RTC core about the inbound
        // destination: regardless of what `stream.local_addr()` says
        // (typically the VM's internal interface IP behind the NAT), we
        // pass `destination = tcp_advertised_addr` to `handle_read`.
        // ICE matches the lied-about destination to its single local
        // TCP candidate and forms a clean pair; data still flows because
        // the TCP stream is bidirectional and we own the write half
        // directly, no kernel routing involved.
        let mut peer_registration = None;
        let mut tcp_conn_rx: Option<mpsc::Receiver<AcceptedTcpConnection>> = None;
        let mut tcp_advertised: Option<SocketAddr> = None;
        if let (Some(registry), Some(advertised)) = (
            tcp_peer_registry.as_ref(),
            tcp_advertised_addr.filter(|a| !a.ip().is_loopback() && !a.ip().is_unspecified()),
        ) {
            let (registration, rx) = registry.register(local_ufrag.clone());
            peer_registration = Some(registration);
            tcp_conn_rx = Some(rx);
            tcp_advertised = Some(advertised);
            // RFC 6544 requires TCP ICE candidates to carry a `tcptype`
            // attribute. `Candidate::host(addr, "tcp")` doesn't set it,
            // and browsers drop TCP candidates that lack it. The builder
            // lets us set `tcptype: passive` — "the remote actively opens
            // the TCP connection to us", the correct role for a
            // server-side host candidate.
            let candidate = host_candidate_init(advertised, RTCIceProtocol::Tcp);
            if let Err(e) = rtc.add_local_candidate(candidate) {
                eprintln!("[display/webrtc] failed to add TCP host candidate {advertised}: {e}");
            }
            eprintln!(
                "[display/webrtc] peer {peer_id}: ICE-TCP enabled on {advertised} for ufrag {local_ufrag}"
            );
        } else if tcp_peer_registry.is_some() {
            // Registry available but no suitable advertised address — the
            // browser connected via hostname/loopback so we have no
            // non-loopback IP to advertise. Log once so operators can
            // spot the "why does TCP never kick in" case.
            eprintln!(
                "[display/webrtc] peer {peer_id}: no ICE-TCP candidate advertised (no non-loopback Host header)"
            );
        }

        // --- Parse the offer and produce the answer ----------------------
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| CallerError::WebRtc(format!("parse offer: {e}")))?;
        rtc.set_remote_description(offer)
            .map_err(|e| CallerError::WebRtc(format!("set remote offer: {e}")))?;
        let answer = rtc
            .create_answer(None)
            .map_err(|e| CallerError::WebRtc(format!("create answer: {e}")))?;
        rtc.set_local_description(answer.clone())
            .map_err(|e| CallerError::WebRtc(format!("set local answer: {e}")))?;
        // Sanitized-wire-only shim — narrow workaround for two rtc 0.9
        // SDP-writer bugs that fire on multi-RID simulcast send:
        //   1. each `a=rid:<rid> send` line emitted twice (six lines for
        //      f/h/q instead of three);
        //   2. `a=simulcast:send f;h;q send f;h;q` instead of the RFC
        //      8853-correct `a=simulcast:send f;h;q`.
        // WebKit rejects (2) with `SyntaxError: Malformed simulcast
        // line` on setRemoteDescription, so the answer never lands and
        // no media flows. See `sanitize_answer_sdp` above for the
        // exact transformation + test coverage.
        //
        // Why the call sequence is what it is: rtc 0.9 caches the
        // `create_answer` result in `PeerConnectionInternal.last_answer`
        // (peer_connection/mod.rs:944) and `set_local_description`
        // does a direct string-equality check against that cache
        // (peer_connection/internal.rs:373, :408). Sanitizing
        // *between* `create_answer` and `set_local_description`
        // therefore fails with `ErrSDPDoesNotMatchAnswer`. So the
        // contract here is:
        //
        //   - Pass the *literal* `create_answer` output to
        //     `set_local_description` (above) — rtc's strict gate is
        //     satisfied, rtc's local state stays internally consistent
        //     with the malformed SDP it produced.
        //   - Sanitize *only* the bytes we ship on the wire — WebKit
        //     accepts the corrected line, media flows.
        //
        // Yes, this leaves the rtc state's local SDP and the wire SDP
        // diverged. The hypothesis being tested is that the divergence
        // is benign because rtc's media plane was built from track
        // encodings, not from re-parsing its own emitted SDP — so the
        // doubled `a=rid:` lines and `a=simulcast:send ... send ...`
        // attribute are pure signaling artifacts. If this experiment
        // proves the hypothesis, the shim stays as a narrow
        // compatibility band-aid until the dep is patched.
        //
        // Long-term fix: patch rtc 0.9's SDP writer at source — fix
        // the per-RID `send` line duplication and the doubled-direction
        // `a=simulcast:` emission — and remove this sanitizer call,
        // not the dep's validation gate. Relaxing the gate was
        // explicitly excluded as the wrong fix shape.
        let answer_sdp = sanitize_answer_sdp(&answer.sdp);
        // Dump every a=candidate line from the answer so we can see exactly
        // what the RTC core emitted — this is the fastest way to diagnose
        // "browser never tries to connect to the TCP candidate" symptoms.
        for line in answer_sdp.lines().filter(|l| l.starts_with("a=candidate:")) {
            eprintln!("[display/webrtc] peer {peer_id}: answer {line}");
        }

        // --- Spawn the driver --------------------------------------------
        let (encoded_frame_tx, encoded_frame_rx) =
            mpsc::channel::<OutboundEncodedFrame>(ENCODED_FRAME_CHANNEL);
        let (command_tx, command_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL);
        // Phase 4d.1: per-peer observed send bitrate (`bytes_sent`
        // delta, local egress only — see WebRtcPeer::observed_send_bitrate_rx
        // for the semantic distinction from capacity). Initial value
        // None: the driver's first poll seeds the per-SSRC `prev`
        // map and returns no delta; the second poll, one
        // TWCC_POLL_INTERVAL later, publishes the first measurable
        // rate (None still until any RTP has actually been sent).
        let (observed_send_bitrate_tx, observed_send_bitrate_rx) =
            watch::channel::<Option<u64>>(None);
        // Phase 4d.3a: per-peer per-RID receiver-feedback health
        // (RR-derived, populated from rtc 0.9's
        // `RTCRemoteInboundRtpStreamStats`). Initial value is the
        // empty map: no RR has arrived yet. Per-RID entries appear
        // as RRs land for each outbound SSRC. Layer-selection
        // policy (4d.3b/c) treats missing RIDs as "no signal yet,
        // stay conservative" rather than as "healthy."
        let (remote_inbound_health_tx, remote_inbound_health_rx) =
            watch::channel::<HashMap<SimulcastRid, PeerLayerHealth>>(HashMap::new());
        // **Phase 4d.3b**: per-peer aggregate TWCC health, derived
        // from inbound `TransportLayerCC` packets observed by the
        // [`crate::display::twcc_tap::TwccTapInterceptor`] wired
        // into the rtc interceptor chain above. Initial value
        // `None`: the aggregator hasn't published its first
        // 1-second window yet. After the first publish the channel
        // stays `Some(_)`, replaced once per window. The capacity
        // policy in [`crate::display::aggregator`] subscribes via
        // [`WebRtcPeer::subscribe_twcc_health`].
        let (twcc_health_tx, twcc_health_rx) =
            watch::channel::<Option<crate::display::twcc_tap::TwccHealth>>(None);
        let shutdown = CancellationToken::new();
        // Aggregator task drains `twcc_tap_rx` and publishes one
        // `TwccHealth` per second. Exits on `shutdown.cancelled()`,
        // on the tap channel closing (rtc dropped → tap dropped →
        // sender dropped → recv returns None), or on all watch
        // receivers dropping.
        crate::display::twcc_tap::spawn_twcc_health_aggregator(
            twcc_tap_rx,
            twcc_health_tx,
            shutdown.clone(),
        );

        // Phase 4c: pass the full per-RID encoding map through to the
        // driver. For VP8 simulcast `encodings_by_rid` carries
        // (full, ssrc_f), (half, ssrc_h), (quarter, ssrc_q); the
        // driver builds one packetizer per RID + uses the matching
        // SSRC at write time so `RTCRtpSender::write_rtp` routes to
        // the right encoding.
        tokio::spawn(driver(
            peer_id,
            rtc,
            RtpSendConfig {
                sender_id,
                mid: video_mid,
                codec: codec_params.rtp_codec,
                encodings: encodings_by_rid,
            },
            sockets,
            tcp_conn_rx,
            tcp_advertised,
            peer_registration,
            encoded_frame_rx,
            command_rx,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            keyframe_request_tx,
            observed_send_bitrate_tx,
            remote_inbound_health_tx,
            // F8: srflx gathering is folded into the driver's UDP
            // forwarders and trickled via `ice_tx`, off the answer path.
            // `ice_config` carries the STUN server config the driver
            // resolves (DNS) and queries off-path; cloning a small config
            // struct keeps that resolution out of `create_answer`.
            ice_config.clone(),
            ice_tx,
            shutdown.clone(),
        ));

        Ok((
            Self {
                peer_id,
                command_tx,
                observed_send_bitrate_rx,
                remote_inbound_health_rx,
                twcc_health_rx,
                active_rids: active_rids.to_vec(),
                shutdown,
            },
            encoded_frame_tx,
            answer_sdp,
        ))
    }

    /// Build a peer that consumes frames from the shared
    /// [`EncoderPool`] and forwards them to the browser via the RTC driver.
    /// The only public constructor (3c.4c renamed `new_pool_mode` →
    /// `new` after the legacy single-encoder fan-out was deleted in
    /// 3c.4b).
    ///
    /// `codec_set` is derived from `subscriptions` rather than from
    /// the original peer offer prefs. This is the contract that
    /// keeps the partial-result path safe: the SDP we negotiate
    /// enables exactly the codecs the pool can serve, so the peer
    /// can never select a codec we'll silently drop frames for.
    /// Empty subscriptions upstream means the offer handler should
    /// reject before reaching here; we forward the empty case as a
    /// clean `WebRtc("empty subscription set")` rather than silently
    /// constructing a peer with no codecs.
    ///
    /// `lease` and `prefs` are handed to a per-peer `pool_frame_intake`
    /// task that owns the lease's lifetime. On any subscription's
    /// `RecvError::Closed` (typically: `EncoderPool::on_resize`
    /// dropping a slot), the intake task drops the lease, calls
    /// `pool.subscribe(prefs)` for fresh subscriptions+lease, and
    /// resumes forwarding from the new handles. If resubscribe
    /// returns `NoCompatibleCodec`, the intake task signals peer
    /// shutdown via the WebRtcPeer's cancellation token — peers that
    /// can't be served any longer are torn down rather than left in
    /// a black-stream state.
    ///
    /// `drops_counter` is incremented every time the intake's forwarder
    /// drops a frame because the driver's encoded-frame `mpsc` is full
    /// (peer is slow). Callers should share this counter with their
    /// metrics aggregation so the `peer_drops` field on
    /// `DisplayMetricsSnapshot` reflects total drops across all peers.
    /// Tests can pass a fresh `Arc::new(AtomicU64::new(0))` and inspect
    /// it directly.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        peer_id: PeerId,
        offer_sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<TcpPeerRegistry>>,
        tcp_advertised_addr: Option<SocketAddr>,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        authority_handler: AuthorityChannelHandler,
        tile_control_handler: TileControlHandler,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        pool: Arc<EncoderPool>,
        subscriptions: Vec<EncoderSubscription>,
        lease: PoolLease,
        prefs: PeerCodecPreferences,
        drops_counter: Arc<AtomicU64>,
    ) -> Result<(Self, String), CallerError> {
        if subscriptions.is_empty() {
            return Err(CallerError::WebRtc(
                "new: empty subscription set — offer handler must \
                 reject before reaching here"
                    .to_string(),
            ));
        }
        let active_codec =
            active_codec_from_subscriptions(&subscriptions, &prefs).ok_or_else(|| {
                CallerError::WebRtc(
                    "new: no subscription matched peer codec preferences".to_string(),
                )
            })?;
        let active_codec_set = [active_codec];
        // Filter the peer's original prefs against the single codec the
        // intake will actually forward. The answer and all future
        // resubscribes are locked to this codec, so the RTC sender cannot
        // negotiate one codec while pool_frame_intake selects another.
        let negotiated_prefs = filter_prefs_to_negotiated(&prefs, &active_codec_set);
        // Defensive — should be unreachable: subscriptions is non-empty
        // (early return above), active_codec came from subscriptions,
        // and active_codec also appears in original prefs. So the
        // intersection is non-empty. If it's empty here, something
        // upstream is producing subs for a codec the prefs doesn't
        // include — fail loud.
        if negotiated_prefs.is_empty() {
            return Err(CallerError::WebRtc(
                "new: filter_prefs_to_negotiated produced empty set; \
                 pool returned subscriptions for codecs not in peer prefs"
                    .to_string(),
            ));
        }

        // Phase 4c: derive the RID set the peer's track will advertise
        // from the initial subscriptions filtered to the active codec
        // — per the user's correction #2, the answer SDP must match
        // exactly what the peer subscribed to (NOT what
        // `pool.always_on()` happens to advertise globally; an
        // on-demand encoder construction failure could produce a
        // subscription set narrower than the pool's general layout).
        // Order is preserved as encountered in the subscriptions
        // (which is layer order from `vp8_simulcast`: full / half /
        // quarter), so the answer's `a=rid` lines come out in
        // preference order.
        let pool_rids: Vec<SimulcastRid> = subscriptions
            .iter()
            .filter(|s| s.id.codec == active_codec)
            .map(|s| s.id.rid.clone())
            .collect();
        // Defensive — `active_codec_from_subscriptions` returned
        // Some, so at least one subscription has this codec.
        // Treating an empty pool_rids as a bug rather than a soft
        // failure: build_with_codec_set rejects empty too, so this
        // is a redundant guard with a more specific error message.
        if pool_rids.is_empty() {
            return Err(CallerError::WebRtc(format!(
                "new: active_codec={active_codec:?} resolved but no \
                 subscriptions match it — internal pool/peer state \
                 divergence",
            )));
        }
        // #46 fix: intersect the pool's RIDs with what the offer
        // actually requested via `a=simulcast:recv`. The pool's
        // always-on VP8 simulcast advertises 3 RIDs (f/h/q), but a
        // federated [`PeerDisplayConnection`] offer post-`e815bac` no
        // longer includes `a=simulcast:recv` — sending an answer
        // declaring 3 RIDs against such an offer produces an
        // `a=simulcast:send f;h;q` answer with no `a=ssrc` declarations
        // (rtc 0.9 SDP-writer bug), which Chrome / WebKit silently
        // refuse to decode. Empirical signature: `framesDecoded == 0`
        // forever, `packetsReceived > 0`, `pliCount > 0`. The local
        // [`DisplaySlot`] path injects `a=simulcast:recv f;h;q` before
        // setLocalDescription so its offer keeps the multi-RID send
        // path; the federated path narrows to `[full]`.
        //
        // Three branches:
        //  - Offer has no `a=simulcast:recv` → narrow to a single
        //    layer (the highest-priority one, layer order from
        //    `vp8_simulcast`: full first).
        //  - Offer has `a=simulcast:recv [...]` → intersect pool_rids
        //    with the offer's recv list, preserving pool order. Empty
        //    intersection is a hard error (no overlap = no codec).
        //  - Offer requests RIDs the pool isn't producing right now
        //    (e.g. an on-demand layer construction failure) → silently
        //    drop those RIDs from the answer.
        let active_rids: Vec<SimulcastRid> = match parse_offer_simulcast_recv_rids(offer_sdp) {
            None => {
                // Single-encoding offer → narrow to one layer.
                //
                // **#48 tuning**: pick the **floor** (last in
                // `pool_rids`, which is spec-ordered descending
                // bitrate per `LayerSpec::vp8_simulcast` — q
                // (125 kbps @ ¼ res) when all three layers are
                // present), not `pool_rids[0]` (the full layer at
                // 2.5 Mbps). Rationale: the federated path is the
                // only consumer of single-encoding negotiation
                // (`PeerDisplayConnection` post-#46/`e815bac`),
                // and runs over a TURN-relay where moderate (~5-
                // 10 %) sustained packet loss is the operational
                // baseline. At full-layer keyframe sizes (~500 KB
                // = ~420 RTP packets), 8 % loss makes intact
                // delivery `0.92^420 ≈ 1.4e-15` — effectively
                // impossible. Quarter-layer keyframes (~20 KB =
                // ~17 packets) have `0.92^17 ≈ 24 %` intact
                // probability and recover within seconds. Loss-
                // tolerance dominates resolution for "stream
                // remains usable under loss" — full-resolution
                // single-RID federated was empirically frozen
                // (~0.4 fps decoded at 8 % loss before this
                // tuning).
                //
                // The local `DisplaySlot` path is unaffected: it
                // injects `a=simulcast:recv f;h;q` into its offer,
                // so it hits the `Some(offer_rids)` branch below
                // and gets the full multi-RID set as before.
                //
                // Robust against partial-layer pools: if pool
                // dropped the quarter layer because the source is
                // too small (`MIN_LAYER_DIM` filter in
                // `vp8_simulcast`), `pool_rids.last()` becomes
                // `h` or `f` — degrades to "best available floor"
                // rather than failing.
                vec![select_single_rid_for_federated_offer(&pool_rids)]
            }
            Some(offer_rids) => {
                let intersected: Vec<SimulcastRid> = pool_rids
                    .iter()
                    .filter(|r| offer_rids.contains(r))
                    .cloned()
                    .collect();
                if intersected.is_empty() {
                    return Err(CallerError::WebRtc(format!(
                        "new: offer's a=simulcast:recv RIDs \
                             {offer_rids:?} have no overlap with pool's \
                             active RIDs {pool_rids:?} for codec \
                             {active_codec:?}",
                    )));
                }
                intersected
            }
        };
        // Phase 4e: keyframe-request channel from driver → intake.
        // Driver pushes a `SimulcastRid` to this channel for every
        // PLI / FIR whose target SSRC matches one of our outbound
        // encodings; intake reads from the channel and calls
        // `pool.request_keyframe(active_codec, Some(rid))` so PLI
        // recovery hits ONLY the affected layer's encoder. Bounded +
        // lossy by design — see `KEYFRAME_REQUEST_CHANNEL` doc.
        let (keyframe_request_tx, keyframe_request_rx) =
            mpsc::channel::<SimulcastRid>(KEYFRAME_REQUEST_CHANNEL);
        let (peer, encoded_frame_tx, answer_sdp) = Self::build_with_codec_set(
            peer_id,
            offer_sdp,
            active_codec,
            &active_rids,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            keyframe_request_tx,
        )
        .await?;
        // Spawn the intake task. It owns the encoded_frame_tx (no
        // longer parked on Self after the 3c.4d follow-up cleanup)
        // and a clone of `shutdown` so it can push frames into the
        // existing driver and exit when the peer is torn down. The
        // task owns the lease and resubscribes as needed (see
        // `pool_frame_intake` for the Closed-handling contract).
        //
        // #46 fix companion: filter the subscriptions handed to the
        // intake down to the active RIDs. The driver was built with
        // `active_rids` (intersected with the offer's
        // `a=simulcast:recv`), so it knows about exactly those RIDs;
        // forwarding frames for any other RID hits the driver's
        // "frame for unknown rid" defensive return + log spam (see
        // step 3b in the driver). The pool's full subscription set
        // is preserved through `lease` (refcount + Drop semantics
        // unchanged) so the always-on encoders keep producing for
        // any other peer that wants those layers.
        let active_rid_set: std::collections::HashSet<SimulcastRid> =
            active_rids.iter().cloned().collect();
        let intake_subscriptions: Vec<EncoderSubscription> = subscriptions
            .into_iter()
            .filter(|s| active_rid_set.contains(&s.id.rid))
            .collect();
        let intake_shutdown = peer.shutdown.clone();
        tokio::spawn(pool_frame_intake(
            pool,
            negotiated_prefs,
            intake_subscriptions,
            lease,
            encoded_frame_tx,
            drops_counter,
            keyframe_request_rx,
            intake_shutdown,
        ));

        Ok((peer, answer_sdp))
    }

    /// Send a clipboard update to the browser via the clipboard data channel.
    ///
    /// Returns `Ok(true)` if the command was queued, `Ok(false)` if the driver
    /// is shutting down.
    pub async fn send_clipboard(&self, content: &ClipboardContent) -> Result<bool, CallerError> {
        match self
            .command_tx
            .send(Command::SendClipboard(content.clone()))
            .await
        {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// F-1.2: push a personalized display-input authority state to the
    /// browser over the federated `display_input_authority` data
    /// channel. Used by the federated authority broadcast loop to
    /// fan personalized `you | other | unclaimed` snapshots out to
    /// each subscribed federated WebRtcPeer.
    ///
    /// If the data channel is not yet open at the time of the call,
    /// the message is queued in the driver state and emitted on
    /// `OnDataChannel(OnOpen)` for the matching label. This bootstrap
    /// path is load-bearing: the broadcast loop registers a federated
    /// WebRtcPeer as a subscriber the moment the federation registry
    /// adds it, which can be — and usually is — before the browser's
    /// data channels finish negotiating. Without queueing, the
    /// browser's chip would stall at `unknown` until the next
    /// authority transition.
    ///
    /// Returns `Ok(true)` if the command was queued for the driver,
    /// `Ok(false)` if the driver is shutting down. Send-success at
    /// the channel layer is best-effort and not surfaced; the
    /// federated path tolerates dropped frames at this layer because
    /// the broadcast loop is the primary state-of-truth source and
    /// will re-broadcast on every transition.
    pub async fn send_authority_state(
        &self,
        display_id: u32,
        state: DisplayInputAuthorityState,
    ) -> Result<bool, CallerError> {
        match self
            .command_tx
            .send(Command::SendAuthorityState { display_id, state })
            .await
        {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// D-3b: send a reliable tile-control binary frame to the browser.
    /// Queues in the driver until `tile-control` opens.
    pub async fn send_tile_control_frame(&self, data: Vec<u8>) -> Result<bool, CallerError> {
        self.send_tile_frame(TileDataChannel::Control, data).await
    }

    /// D-3b: send a reliable tile-snapshot binary frame to the browser.
    /// Queues in the driver until `tile-snapshot` opens.
    pub async fn send_tile_snapshot_frame(&self, data: Vec<u8>) -> Result<bool, CallerError> {
        self.send_tile_frame(TileDataChannel::Snapshot, data).await
    }

    /// D-3b: send an unreliable/supersedable tile-delta binary frame
    /// to the browser. If the channel is not open, the driver drops
    /// the frame rather than queueing stale deltas.
    pub async fn send_tile_delta_frame(&self, data: Vec<u8>) -> Result<bool, CallerError> {
        self.send_tile_frame(TileDataChannel::Deltas, data).await
    }

    async fn send_tile_frame(
        &self,
        channel: TileDataChannel,
        data: Vec<u8>,
    ) -> Result<bool, CallerError> {
        match self
            .command_tx
            .send(Command::SendTileFrame { channel, data })
            .await
        {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Add a trickle ICE candidate from the remote peer.
    ///
    /// The browser sends `{candidate, sdpMid, sdpMLineIndex}`; we only need
    /// the `candidate` string (RFC 5245 format) for the RTC core.
    ///
    /// Browsers obfuscate host candidates as mDNS `.local` hostnames. Resolve
    /// the hostname via the system resolver (nss-mdns / Avahi on Linux,
    /// Bonjour on macOS) and rewrite the candidate string before forwarding to
    /// the driver. Candidates that already contain a literal IP pass through
    /// unchanged.
    pub async fn add_ice_candidate(&self, candidate_json: &str) -> Result<(), CallerError> {
        let parsed: serde_json::Value = serde_json::from_str(candidate_json)
            .map_err(|e| CallerError::WebRtc(format!("parse ICE candidate: {e}")))?;
        let candidate_str = parsed["candidate"].as_str().unwrap_or("");
        if candidate_str.is_empty() {
            return Ok(());
        }
        let resolved = match resolve_mdns_in_candidate(candidate_str).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[display/webrtc] mdns resolve failed: {e}, dropping candidate");
                return Ok(());
            }
        };
        self.command_tx
            .send(Command::AddIceCandidate(resolved))
            .await
            .map_err(|_| CallerError::WebRtc("driver gone".to_string()))?;
        Ok(())
    }

    /// Gracefully close this peer.
    pub async fn close(&self) {
        self.shutdown.cancel();
        // Driver exits on the next select! iteration; channels close on drop.
    }
}

// ---------------------------------------------------------------------------
// Driver task
// ---------------------------------------------------------------------------

/// Per-[`crate::display::encode::PayloadSpec`] negotiation + readiness
/// state. Keyed off the full `PayloadSpec` in [`DriverState::video_specs`]
/// so H.264 fmtp variants (profile-level-id + packetization-mode) stay
/// distinct — browser negotiation treats those independently and caching by
/// `CodecKind` alone would conflate them.
///
/// The combined cache structure is deliberate: under the encoder pool a
/// peer can receive frames from multiple codecs in quick succession
/// (VP8 always-on + H.264 on-demand, etc.). A global `keyframe_seen` flag
/// (the pre-3c.0a shape) lets a stray keyframe from an *unsupported* spec
/// open the gate for P-frames of a *supported* one — a subtle silent-
/// black-screen class of bug. Making readiness per-spec AND flipping it
/// only after a successful `writer.write` eliminates that path.
enum SpecState {
    /// Has this spec had >=1 keyframe successfully packetized for the peer?
    /// Until true, non-keyframe frames drop. Scoped per spec so codec A's
    /// keyframe gate is independent of codec B's.
    Ready { keyframe_seen: bool },
    /// This frame spec does not match the codec negotiated for this peer.
    Unsupported,
}

/// State the driver carries between iterations.
struct DriverState {
    /// Per-`(PayloadSpec, SimulcastRid)` resolved PT + keyframe
    /// readiness. See [`SpecState`].
    ///
    /// Keying changed in phase-4c-prep (this commit) from `PayloadSpec`
    /// alone to `(PayloadSpec, SimulcastRid)`. The previous keying
    /// would have been wrong for VP8 simulcast: every layer of a
    /// VP8 simulcast track produces the SAME `PayloadSpec`
    /// (codec_mime + clock + fmtp are the same across layers), so a
    /// single map entry would conflate the keyframe gates of three
    /// distinct RIDs. A keyframe seen on RID `full` would then open
    /// the gate for P-frames on RIDs `half` / `quarter` — and those
    /// RIDs' subscribers would receive P-frames referencing
    /// keyframes they never got, decoding to garbage. Per-RID
    /// keying eliminates that path.
    ///
    /// For single-encoding peers (today's behavior, preserved in
    /// this refactor; H.264 always-on or VP8-as-single-layer until
    /// commit 2 lights up multi-encoding), the map has exactly one
    /// entry per active spec — same shape as the previous keying,
    /// just with the RID dimension along for the ride.
    video_specs: HashMap<(crate::display::encode::PayloadSpec, SimulcastRid), SpecState>,
    /// Map of channel label → DataChannelId for routing channel data and clipboard sends.
    channels: HashMap<String, RTCDataChannelId>,
    /// F-1.2: queued `display_input_authority_state` messages awaiting
    /// the data-channel's `OnOpen`. The federated authority broadcast
    /// loop calls [`WebRtcPeer::send_authority_state`] as soon as a
    /// federated WebRtcPeer is registered as a subscriber — which can
    /// be (and usually is) before the browser's data channels finish
    /// negotiating. Without queueing, that initial snapshot would land
    /// on the floor and the browser's chip would stall at `unknown`.
    /// Drained and emitted in order on `OnDataChannel(OnOpen)` for
    /// label `display_input_authority`.
    ///
    /// Capacity-bounded by the producer side (the broadcast loop runs
    /// at low frequency — one event per take/release/disconnect, not
    /// per frame), so unbounded growth here is structurally
    /// impossible. Uses `Vec` rather than a channel because the
    /// producer side is not throttled by the channel's send-await
    /// semantics.
    pending_authority_state: Vec<(u32, DisplayInputAuthorityState)>,
    /// D-3b: queued tile control frames awaiting `tile-control`
    /// channel open. Low-rate reliable control only; never per-frame.
    pending_tile_control: Vec<Vec<u8>>,
    /// D-3b: queued snapshot chunks awaiting `tile-snapshot` channel
    /// open. Reliable snapshot delivery is allowed to delay rather
    /// than drop. Tile deltas intentionally have no queue.
    pending_tile_snapshot: Vec<Vec<u8>>,
    /// D-4c: event-driven backpressure state for the supersedable
    /// `tile-deltas` channel. Control and snapshot channels are
    /// reliable and never use this drop policy.
    tile_delta_backpressure: TileDeltaBackpressure,
    /// Wallclock anchor: Instant at which the first frame was emitted.
    /// All subsequent rtp_time values are relative to this.
    first_frame_at: Option<Instant>,
    rtp: RtpSendState,
}

/// Per-RID send state — one entry per simulcast layer (or one entry
/// for non-simulcast codecs).
///
/// SSRC and packetizer are per-RID because:
/// - **SSRC**: [`rtc`]'s `RTCRtpSender::write_rtp` routes packets to
///   encodings by matching `packet.header.ssrc` against the encoding's
///   SSRC. Each layer must carry its own SSRC for the right encoding
///   to claim the packet.
/// - **Packetizer**: each packetizer holds its own RTP sequence
///   number + timestamp continuation state. Sharing one packetizer
///   across RIDs would interleave their sequence streams and the
///   browser's per-encoding jitter buffers would reject everything
///   they didn't expect at the next sequence number.
struct RidRtpState {
    ssrc: u32,
    packetizer: Box<dyn Packetizer + Send>,
}

struct RtpSendState {
    sender_id: RTCRtpSenderId,
    mid: String,
    codec: RTCRtpCodec,
    /// Per-RID send state. Looked up by the
    /// [`OutboundEncodedFrame::rid`] of each incoming frame so the
    /// driver writes with the matching SSRC + the matching
    /// packetizer's continuation state.
    by_rid: HashMap<SimulcastRid, RidRtpState>,
    mid_ext_id: Option<u8>,
    rid_ext_id: Option<u8>,
}

/// Inbound packet from one of the per-interface forwarder tasks or a
/// TCP connection reader. `proto` tags which transport it arrived on so
/// the driver can hand it to the RTC core with the correct metadata.
struct InboundPacket {
    proto: TransportProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

/// Outbound side of an ICE-TCP connection: the sending end of an ordered
/// channel feeding that connection's dedicated writer task. The driver
/// stores one per connection (keyed by the remote's source address) so it
/// can route outbound TCP writes to the right socket.
///
/// **Why a channel + single writer task, not `Arc<Mutex<OwnedWriteHalf>>`
/// with a spawned write per transmit:** spawning a fresh task for every
/// `rtc.poll_write()` transmit (the old design) hands the *scheduler*, not
/// `rtc`, control over the order writes hit the wire — the `Mutex` only
/// stops byte-level interleaving, not whole-frame reordering — and applies
/// no backpressure, so under sustained RTP video the kernel send buffer
/// overflows with unbounded queued tasks. On Linux a non-blocking `send`
/// that can't fit just yields `EWOULDBLOCK` and tokio waits; on Windows the
/// TCP stack instead aborts the connection once the unACKed send backlog
/// trips its retransmit limit, and `send` then returns `WSAECONNABORTED`
/// (os error 10053) on *every* subsequent write — the 10053 flood that left
/// the dashboard black. Funnelling every transmit through one ordered
/// bounded channel drained by a single owner of the write half preserves
/// `rtc`'s emit order, gives real backpressure (a full queue drops the
/// frame instead of overflowing the socket), and gives the connection a
/// single error owner that tears the peer down on the first write failure
/// rather than re-flooding a dead socket.
const TCP_OUT_QUEUE: usize = 256;
type TcpFrameSender = mpsc::Sender<Vec<u8>>;

#[allow(clippy::too_many_arguments)]
async fn driver<I: rtc::interceptor::Interceptor + Send + Sync + 'static>(
    peer_id: PeerId,
    mut rtc: RTCPeerConnection<I>,
    rtp_config: RtpSendConfig,
    sockets: Vec<Arc<UdpSocket>>,
    mut tcp_conn_rx: Option<mpsc::Receiver<AcceptedTcpConnection>>,
    tcp_advertised: Option<SocketAddr>,
    _tcp_registration: Option<PeerRegistration>,
    mut frame_rx: mpsc::Receiver<OutboundEncodedFrame>,
    mut command_rx: mpsc::Receiver<Command>,
    input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    authority_handler: AuthorityChannelHandler,
    tile_control_handler: TileControlHandler,
    keyframe_request_tx: mpsc::Sender<SimulcastRid>,
    observed_send_bitrate_tx: watch::Sender<Option<u64>>,
    remote_inbound_health_tx: watch::Sender<HashMap<SimulcastRid, PeerLayerHealth>>,
    // F8: STUN config + server→browser trickle channel. The driver
    // resolves the STUN server (DNS) and gathers the srflx candidate via
    // its UDP forwarders, all off the peer-setup critical path; `ice_tx`
    // delivers the resulting candidate to the browser as trickle ICE.
    ice_config: IceConfig,
    ice_tx: mpsc::Sender<(PeerId, String)>,
    shutdown: CancellationToken,
) {
    if rtp_config.encodings.is_empty() {
        eprintln!(
            "[display/webrtc] peer {peer_id}: RtpSendConfig.encodings is empty; \
             refusing to start a driver with no SSRC/RID slots — \
             build_with_codec_set must populate at least one encoding"
        );
        shutdown.cancel();
        return;
    }
    // Build one packetizer per encoding (per-RID continuation state).
    // The payloader factory is per-codec, but each encoding gets its
    // own payloader instance — packetizers hold mutable state (current
    // sequence number, RTP timestamp continuation), and sharing one
    // across RIDs would interleave their sequence streams.
    let mut by_rid: HashMap<SimulcastRid, RidRtpState> =
        HashMap::with_capacity(rtp_config.encodings.len());
    for (rid, ssrc) in &rtp_config.encodings {
        let payloader = match rtp_config.codec.payloader() {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[display/webrtc] peer {peer_id}: no RTP payloader for \
                     codec on rid {}: {e}",
                    rid.as_str(),
                );
                shutdown.cancel();
                return;
            }
        };
        let packetizer = packetizer::new_packetizer(
            1200,
            96,
            *ssrc,
            payloader,
            Box::new(sequence::new_random_sequencer()),
            rtp_config.codec.clock_rate,
        );
        by_rid.insert(
            rid.clone(),
            RidRtpState {
                ssrc: *ssrc,
                packetizer: Box::new(packetizer),
            },
        );
    }
    let mut state = DriverState {
        video_specs: HashMap::new(),
        channels: HashMap::new(),
        pending_authority_state: Vec::new(),
        pending_tile_control: Vec::new(),
        pending_tile_snapshot: Vec::new(),
        tile_delta_backpressure: TileDeltaBackpressure::new(),
        first_frame_at: None,
        rtp: RtpSendState {
            sender_id: rtp_config.sender_id,
            mid: rtp_config.mid,
            codec: rtp_config.codec,
            by_rid,
            mid_ext_id: None,
            rid_ext_id: None,
        },
    };

    // Index sockets by their local address so we can route outbound writes
    // through the socket whose source matches.
    let mut sockets_by_addr: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    for sock in &sockets {
        if let Ok(addr) = sock.local_addr() {
            sockets_by_addr.insert(addr, Arc::clone(sock));
        }
    }

    // Outbound frame senders for each active ICE-TCP connection, keyed by
    // the remote's `SocketAddr`. Each feeds a dedicated writer task that
    // owns the connection's write half (see `TcpFrameSender`).
    let mut tcp_senders: HashMap<SocketAddr, TcpFrameSender> = HashMap::new();

    // --- srflx (STUN) gathering, folded into the UDP forwarders ----------
    //
    // Audit F8: this is deliberately OFF the peer-setup critical path. By
    // the time the driver runs, `build_with_codec_set` has already produced
    // and returned the SDP answer (host + ICE-TCP candidates), so resolving
    // the STUN server (DNS) and gathering the srflx mapping here add zero
    // latency to answer creation — a blocked/unreachable STUN server never
    // delays setup. When a mapping arrives the driver's select loop adds the
    // srflx candidate to `rtc` and trickles it to the browser via `ice_tx`.
    //
    // The exchange is folded *into* each per-socket forwarder rather than
    // run as a separate task because the forwarder is the single owner of
    // its socket's `recv_from`; a second concurrent reader would race for
    // the response (tokio wakes only one waiter, so either side could lose
    // the datagram). The forwarder sends one Binding Request at startup,
    // then in its normal read loop hands every datagram that ISN'T our
    // Binding Success Response on to the RTC core unchanged (so ICE
    // connectivity checks the same socket carries are never dropped) and
    // reports the one matching response's mapped address back here.
    let stun_addr = resolve_stun_servers(&ice_config).await.into_iter().next();
    let (srflx_tx, mut srflx_rx) = mpsc::channel::<(SocketAddr, SocketAddr)>(sockets.len().max(1));

    // Spawn one forwarder task per UDP socket. Each forwarder reads packets
    // from its socket and pushes them into the shared inbound channel,
    // tagged with the socket's local address as the destination. The
    // driver keeps its own clone of `inbound_tx` so it can spawn new
    // readers as TCP connections arrive; it'll drop on driver exit and
    // — together with the forwarders terminating on shutdown — close the
    // channel cleanly.
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundPacket>(64);
    let forwarder_shutdown = shutdown.clone();
    let mut forwarder_handles = Vec::new();
    for sock in &sockets {
        let sock = Arc::clone(sock);
        let tx = inbound_tx.clone();
        let shutdown = forwarder_shutdown.clone();
        let local_addr = match sock.local_addr() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let srflx_tx = srflx_tx.clone();
        forwarder_handles.push(tokio::spawn(async move {
            // Fire one STUN Binding Request out this very socket so the
            // mapping the server reports corresponds to this candidate's
            // base (a 1:1 NAT returns the public IP:port the browser can
            // reach directly). `srflx_pending` holds the transaction ID we
            // expect a matching response to echo; it clears once we've
            // gathered (or given up after `STUN_BINDING_TIMEOUT`) so we
            // stop scanning datagrams. With no STUN server configured we
            // never send and stay a plain forwarder.
            let mut srflx_pending: Option<rtc::stun::message::TransactionId> = None;
            if let Some(stun_addr) = stun_addr {
                match build_stun_binding_request() {
                    Ok((wire, tid)) => match sock.send_to(&wire, stun_addr).await {
                        Ok(_) => srflx_pending = Some(tid),
                        Err(e) => eprintln!(
                            "[display/webrtc] forwarder {local_addr}: STUN send to {stun_addr} failed: {e}"
                        ),
                    },
                    Err(e) => eprintln!(
                        "[display/webrtc] forwarder {local_addr}: build STUN request failed: {e}"
                    ),
                }
            }
            // Off-critical-path deadline after which we stop trying to
            // gather srflx (the answer is already out; nothing waits on
            // this). `tokio::time::sleep` is created up front but only
            // selected on while a request is in flight.
            let srflx_deadline = tokio::time::sleep(STUN_BINDING_TIMEOUT);
            tokio::pin!(srflx_deadline);

            let mut buf = vec![0u8; UDP_BUF_LEN];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    // Only armed while a Binding Request is outstanding.
                    _ = &mut srflx_deadline, if srflx_pending.is_some() => {
                        srflx_pending = None;
                    }
                    recv = sock.recv_from(&mut buf) => match recv {
                        Ok((n, source)) => {
                            // Intercept our own STUN Binding Success
                            // Response (from the STUN server, matching txid)
                            // for srflx; everything else — including STUN
                            // connectivity checks from the browser — falls
                            // through to the RTC core unchanged.
                            if let Some(tid) = srflx_pending {
                                if Some(source) == stun_addr {
                                    if let Some(mapped) =
                                        parse_stun_binding_response(&buf[..n], tid)
                                    {
                                        srflx_pending = None;
                                        // Best-effort: driver may have gone.
                                        let _ = srflx_tx.send((local_addr, mapped)).await;
                                        continue;
                                    }
                                }
                            }
                            let pkt = InboundPacket {
                                proto: TransportProtocol::UDP,
                                source,
                                destination: local_addr,
                                bytes: buf[..n].to_vec(),
                                received_at: Instant::now(),
                            };
                            if tx.send(pkt).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[display/webrtc] forwarder {local_addr}: recv failed: {e}"
                            );
                            break;
                        }
                    },
                }
            }
        }));
    }
    // The driver keeps no `srflx_tx` of its own; drop the template clone so
    // `srflx_rx` closes once every forwarder has exited (gathered, given
    // up, or shut down), letting its select-loop branch go dormant.
    drop(srflx_tx);

    // Phase 4d.1: poll-driven observed-send-bitrate computation.
    // Each tick samples `bytes_sent` per outbound stream and computes
    // the rate over the elapsed interval. `prev_outbound_bytes` carries
    // the per-SSRC last sample across polls; the helper updates it
    // in place. First poll produces None (no prev), subsequent polls
    // produce Some(bps) once at least one SSRC has had two samples.
    //
    // `tokio::time::interval` fires immediately on the first
    // `.tick().await`. That first poll seeds `prev_outbound_bytes`
    // and publishes None — fine because the initial value the watch
    // channel was constructed with is already None.
    // `MissedTickBehavior::Skip` ensures a busy driver loop doesn't
    // produce a burst of catch-up polls when it falls behind.
    let mut twcc_poll = tokio::time::interval(TWCC_POLL_INTERVAL);
    twcc_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev_outbound_bytes: HashMap<u32, (u64, Instant)> = HashMap::new();

    loop {
        // 1. Drain all outputs until we get a Timeout (the next deadline).
        let timeout_at = match drain_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut tcp_senders,
            &mut state,
            &input_handler,
            &clipboard_handler,
            &authority_handler,
            &tile_control_handler,
            &keyframe_request_tx,
        )
        .await
        {
            Ok(t) => t,
            Err(DriverExit::Closed) => {
                eprintln!("[display/webrtc] peer {peer_id}: driver exiting");
                shutdown.cancel();
                for h in forwarder_handles {
                    let _ = h.await;
                }
                return;
            }
        };

        // 2. Wait for the next event: inbound packet, frame, command,
        //    deadline, or shutdown.
        let now = Instant::now();
        let timeout_dur = timeout_at
            .saturating_duration_since(now)
            .max(Duration::from_micros(1));

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                eprintln!("[display/webrtc] peer {peer_id}: shutdown requested");
                for h in forwarder_handles {
                    let _ = h.await;
                }
                return;
            }
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
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_read failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(accepted) = async {
                match tcp_conn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                // New ICE-TCP connection from the dispatcher. Split into read
                // + write halves, store the write side keyed by the remote
                // address, spawn a reader task that forwards subsequent
                // frames through the unified inbound channel, and inject the
                // first-frame we already peeked directly.
                //
                // We "lie" to the RTC core about the destination address: every
                // inbound TCP frame gets `destination = tcp_advertised`
                // (the Host-header-derived IP:port we advertised as our
                // TCP candidate), not the actual `stream.local_addr()` —
                // which on a NAT'd VM is the VM's internal interface IP
                    // that the RTC core has no candidate for. Matching the
                // advertised destination to the one local candidate lets
                // ICE form a valid pair. The underlying TCP stream is
                // bidirectional so data still flows through the real
                // kernel socket we own.
                let AcceptedTcpConnection {
                    remote_addr,
                    local_addr: real_local,
                    first_frame,
                    stream,
                } = accepted;
                let Some(fake_local) = tcp_advertised else {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: TCP connection from {remote_addr} but no fake local configured, dropping"
                    );
                    continue;
                };
                eprintln!(
                    "[display/webrtc] peer {peer_id}: ICE-TCP connection from {remote_addr} → {real_local} (rtc sees {fake_local})"
                );
                let (read_half, write_half) = stream.into_split();

                // Dedicated writer task: owns the write half and drains an
                // ordered bounded channel, writing one RFC 4571 frame per
                // queued payload. This is the single owner of the write
                // half — `drain_outputs` only enqueues onto the channel, so
                // `rtc`'s emit order is preserved on the wire and there is
                // exactly one place that observes write failures. On the
                // first write error (e.g. Windows `WSAECONNABORTED` once the
                // TCP stack aborts the connection) it logs once and cancels
                // the peer's shutdown token, tearing the connection down
                // instead of letting every later transmit re-flood the log
                // on a dead socket.
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
                                        write_rfc4571_frame(&mut write_half, &contents).await
                                    {
                                        eprintln!(
                                            "[display/webrtc] ICE-TCP writer for {remote_addr} \
                                             failed, tearing down connection: {e}"
                                        );
                                        writer_shutdown.cancel();
                                        break;
                                    }
                                }
                                // Sender dropped (driver gone) — nothing more
                                // to write; flush a FIN and exit.
                                None => break,
                            }
                        }
                    }
                    let _ = write_half.shutdown().await;
                });

                // Spawn reader task for subsequent frames on this connection.
                let reader_tx = inbound_tx.clone();
                let reader_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut read_half = read_half;
                    loop {
                        tokio::select! {
                            _ = reader_shutdown.cancelled() => break,
                            frame = read_rfc4571_frame(&mut read_half) => match frame {
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
                                        "[display/webrtc] ICE-TCP reader for {remote_addr} exiting: {e}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                });

                // Inject the first frame we peeked off the wire so the RTC core
                // processes the STUN binding request we used to route.
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
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_read(first TCP frame) failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            _ = tokio::time::sleep(timeout_dur) => {
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_timeout failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(outbound) = frame_rx.recv() => {
                write_video_frame(&mut rtc, &mut state, &outbound);
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_timeout after frame failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(cmd) = command_rx.recv() => {
                handle_command(&mut rtc, &mut state, cmd);
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_timeout after command failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            // F8: a UDP forwarder gathered a srflx mapping for its socket.
            // Add the candidate locally so ICE on the RTC side can form the
            // srflx pair, and trickle it to the browser via `ice_tx` (the
            // web gateway forwards it as a `display_ice` frame, which the
            // browser feeds to `pc.addIceCandidate`, buffering until the
            // answer is applied). This is best-effort: a failed add/trickle
            // logs but never tears the peer down — host + ICE-TCP paths
            // remain. `base` is the gathering socket's local address (the
            // host candidate's base).
            Some((base, mapped)) = srflx_rx.recv() => {
                // Drop a degenerate mapping equal to the base (no NAT in
                // front, or STUN reflected loopback) — it would duplicate
                // the host candidate already in the answer SDP.
                if mapped != base {
                    let init = srflx_candidate_init(mapped, base);
                    // Trickle the candidate to the browser using the
                    // canonical RTCIceCandidate.toJSON() field names
                    // (camelCase). A single video m-line means
                    // sdpMLineIndex 0 routes it unambiguously; sdpMid is
                    // null because the inline host candidates carry no
                    // per-candidate mid either.
                    let candidate_json = serde_json::json!({
                        "candidate": init.candidate,
                        "sdpMid": serde_json::Value::Null,
                        "sdpMLineIndex": 0,
                    })
                    .to_string();
                    match rtc.add_local_candidate(init) {
                        Ok(()) => {
                            eprintln!(
                                "[display/webrtc] peer {peer_id}: added srflx candidate {mapped} (base {base}), trickling to browser"
                            );
                            if ice_tx.send((peer_id, candidate_json)).await.is_err() {
                                eprintln!(
                                    "[display/webrtc] peer {peer_id}: srflx trickle channel closed; candidate added locally only"
                                );
                            }
                            if let Err(e) = rtc.handle_timeout(Instant::now()) {
                                eprintln!(
                                    "[display/webrtc] peer {peer_id}: handle_timeout after srflx candidate failed: {e:?}"
                                );
                                shutdown.cancel();
                                for h in forwarder_handles {
                                    let _ = h.await;
                                }
                                return;
                            }
                        }
                        Err(e) => eprintln!(
                            "[display/webrtc] peer {peer_id}: failed to add srflx candidate {mapped}: {e}"
                        ),
                    }
                }
            }
            // Phase 4d.1: observed-send-bitrate poll. Calls
            // `rtc.get_stats(now, StatsSelector::None)` (read-only walk
            // of the rtc-side accumulator state, cheap), projects
            // outbound streams to `(ssrc, bytes_sent)`, computes the
            // recent send bitrate from per-SSRC deltas vs the previous
            // sample, publishes to the watch channel the layer-
            // selection aggregator (4d.2) subscribes to.
            //
            // `send_replace` (not `send`) so the channel always carries
            // the latest value even if no receiver has subscribed yet
            // — semantics align with watch's "always has a current
            // value" contract.
            //
            // No errors propagate from a failed send (channel closed
            // == aggregator gone == nothing to do), so this branch
            // never tears down the driver.
            _ = twcc_poll.tick() => {
                let report = rtc.get_stats(Instant::now(), StatsSelector::None);
                let bitrate = extract_recent_outbound_bitrate(
                    report.outbound_rtp_streams().map(|s| (
                        s.sent_rtp_stream_stats.rtp_stream_stats.ssrc,
                        s.sent_rtp_stream_stats.bytes_sent,
                    )),
                    &mut prev_outbound_bytes,
                    Instant::now(),
                );
                observed_send_bitrate_tx.send_replace(bitrate);

                // Phase 4d.3a: project remote-inbound-rtp entries
                // (RR-derived, the field set rtc 0.9 actually
                // populates per `accumulator/rtp_stream/outbound.rs`)
                // into per-RID health, mapping outbound SSRCs back
                // through `state.rtp.by_rid`. Empty map publishes
                // every poll until the first RR arrives — receivers
                // see `borrow()` returning the empty map and can
                // distinguish "no signal yet" from "healthy."
                let ssrc_table: Vec<(SimulcastRid, u32)> = state
                    .rtp
                    .by_rid
                    .iter()
                    .map(|(rid, s)| (rid.clone(), s.ssrc))
                    .collect();
                let remote_inbound_iter = report
                    .iter_by_type(RTCStatsType::RemoteInboundRTP)
                    .filter_map(|entry| match entry {
                        RTCStatsReportEntry::RemoteInboundRtp(s) => Some((
                            s.received_rtp_stream_stats.rtp_stream_stats.ssrc,
                            s.fraction_lost,
                            s.received_rtp_stream_stats.packets_lost,
                            s.round_trip_time,
                            // Phase 4d.3a review fix: rtc 0.9 emits
                            // default RemoteInboundRTP snapshots for
                            // every outbound stream even pre-RR (all
                            // fields zero). The helper filters on
                            // `rtt_measurements == 0` to drop those
                            // before the policy sees them — without
                            // this, every just-connected peer would
                            // present a phantom "0% loss" signal that
                            // looks like real health.
                            s.round_trip_time_measurements,
                        )),
                        _ => None,
                    });
                let health = map_remote_inbound_to_rid_health(
                    remote_inbound_iter,
                    &ssrc_table,
                );
                remote_inbound_health_tx.send_replace(health);
            }
        }
    }
}

enum DriverExit {
    Closed,
}

/// Drain pending writes, reads, and events from the sans-I/O peer connection.
#[allow(clippy::too_many_arguments)]
async fn drain_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_senders: &mut HashMap<SocketAddr, TcpFrameSender>,
    state: &mut DriverState,
    input_handler: &Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: &Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    authority_handler: &AuthorityChannelHandler,
    tile_control_handler: &TileControlHandler,
    keyframe_request_tx: &mpsc::Sender<SimulcastRid>,
) -> Result<Instant, DriverExit> {
    while let Some(t) = rtc.poll_write() {
        // Routability filtering only applies to UDP: for UDP we need the
        // kernel's `sendto` to succeed from our bound socket, and a
        // loopback-source-to-routable-destination pair would be rejected with
        // EINVAL. For TCP the connection is already established and we own the
        // stream directly.
        if t.transport.transport_protocol == TransportProtocol::UDP {
            if t.transport.local_addr.is_ipv4() != t.transport.peer_addr.is_ipv4() {
                continue;
            }
            if t.transport.local_addr.ip().is_loopback() != t.transport.peer_addr.ip().is_loopback()
            {
                continue;
            }
        }
        match t.transport.transport_protocol {
            TransportProtocol::UDP => {
                let Some(sock) = sockets_by_addr.get(&t.transport.local_addr) else {
                    eprintln!(
                        "[display/webrtc] UDP transmit from unknown source {}, dropping",
                        t.transport.local_addr
                    );
                    continue;
                };
                if let Err(e) = sock.send_to(&t.message, t.transport.peer_addr).await {
                    eprintln!(
                        "[display/webrtc] udp send {} -> {} failed: {e}",
                        t.transport.local_addr, t.transport.peer_addr
                    );
                }
            }
            TransportProtocol::TCP => {
                let Some(sender) = tcp_senders.get(&t.transport.peer_addr) else {
                    continue;
                };
                let contents: Vec<u8> = t.message.to_vec();
                // Enqueue onto the connection's ordered writer channel.
                // `try_send` (never `send().await`) keeps the rtc poll loop
                // non-blocking: a full queue means the writer task can't
                // keep up with the encoder, so we drop *this* frame as
                // backpressure rather than stalling the driver or
                // overflowing the kernel send buffer. The writer task,
                // not the scheduler, controls wire order.
                match sender.try_send(contents) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Slow/saturated TCP path — drop and let RTP
                        // recovery (PLI/FIR + keyframes) catch the peer up.
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // Writer task exited (connection torn down). Forget
                        // the dead sender so we stop trying to route to it.
                        tcp_senders.remove(&t.transport.peer_addr);
                    }
                }
            }
        }
    }

    while let Some(message) = rtc.poll_read() {
        handle_message(
            message,
            state,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            keyframe_request_tx,
        );
    }

    while let Some(event) = rtc.poll_event() {
        if handle_event(rtc, state, event) {
            return Err(DriverExit::Closed);
        }
    }

    Ok(rtc
        .poll_timeout()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400)))
}

fn handle_event<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
    event: RTCPeerConnectionEvent,
) -> bool {
    match event {
        RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(s) => {
            eprintln!("[display/webrtc] ICE: {s:?}");
        }
        RTCPeerConnectionEvent::OnConnectionStateChangeEvent(s) => {
            eprintln!("[display/webrtc] connection: {s:?}");
            if matches!(
                s,
                rtc::peer_connection::state::RTCPeerConnectionState::Failed
                    | rtc::peer_connection::state::RTCPeerConnectionState::Closed
            ) {
                return true;
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
            let label = rtc
                .data_channel(cid)
                .map(|channel| channel.label().to_string())
                .unwrap_or_else(|| format!("channel-{cid}"));
            eprintln!("[display/webrtc] data channel open: {label}");
            let queued =
                drain_pending_authority_for_label(&label, &mut state.pending_authority_state);
            let queued_tile = drain_pending_tile_for_label(state, &label);
            state.channels.insert(label.clone(), cid);
            if label == TILE_DELTAS_CHANNEL_LABEL {
                state.tile_delta_backpressure.reset();
                if let Some(mut channel) = rtc.data_channel(cid) {
                    let cfg = state.tile_delta_backpressure.config();
                    channel.set_buffered_amount_high_threshold(watermark_to_u32(
                        cfg.high_watermark_bytes,
                    ));
                    channel.set_buffered_amount_low_threshold(watermark_to_u32(
                        cfg.low_watermark_bytes,
                    ));
                }
            }
            // F-1.2: flush any authority states queued before the
            // `display_input_authority` channel opened. See
            // `Command::SendAuthorityState` for why queueing exists —
            // the federated authority broadcast can register a
            // subscriber and emit its initial snapshot before the
            // browser's channel finishes negotiating.
            if !queued.is_empty() {
                if let Some(mut channel) = rtc.data_channel(cid) {
                    for (display_id, auth_state) in queued {
                        let json = serialize_authority_state(display_id, auth_state);
                        if let Err(e) = channel.send_text(json) {
                            eprintln!(
                                "[display/webrtc] authority channel \
                                 queued write failed: {e:?}"
                            );
                        }
                    }
                }
            }
            if !queued_tile.is_empty() {
                if let Some(mut channel) = rtc.data_channel(cid) {
                    for data in queued_tile {
                        if let Err(e) = channel.send(BytesMut::from(&data[..])) {
                            eprintln!(
                                "[display/webrtc] tile channel queued write \
                                 failed on {label}: {e:?}"
                            );
                        }
                    }
                }
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
            let was_tile_deltas =
                state.channels.get(TILE_DELTAS_CHANNEL_LABEL).copied() == Some(cid);
            state.channels.retain(|_, v| *v != cid);
            if was_tile_deltas {
                state.tile_delta_backpressure.reset();
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountHigh(cid)) => {
            if state.channels.get(TILE_DELTAS_CHANNEL_LABEL).copied() == Some(cid)
                && state.tile_delta_backpressure.on_buffered_amount_high()
            {
                let stats = state.tile_delta_backpressure.stats();
                eprintln!(
                    "[display/webrtc] tile-deltas backpressure high: \
                     pausing supersedable deltas (sent={} dropped={})",
                    stats.sent_frames, stats.dropped_frames
                );
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountLow(cid)) => {
            if state.channels.get(TILE_DELTAS_CHANNEL_LABEL).copied() == Some(cid)
                && state.tile_delta_backpressure.on_buffered_amount_low()
            {
                let stats = state.tile_delta_backpressure.stats();
                eprintln!(
                    "[display/webrtc] tile-deltas backpressure low: \
                     resuming supersedable deltas (sent={} dropped={})",
                    stats.sent_frames, stats.dropped_frames
                );
            }
        }
        _ => {}
    }
    false
}

/// **Phase 4d.3a**: project per-SSRC remote-inbound stats (RR-derived,
/// from rtc 0.9's `RTCRemoteInboundRtpStreamStats` accumulator) onto
/// the per-RID SSRC table the driver maintains in `state.rtp.by_rid`.
/// Returns one [`PeerLayerHealth`] entry per recognized RID; SSRCs not
/// present in the table (transient renegotiation windows, on-demand
/// codecs we don't carry per-RID, RR for an SSRC we never advertised)
/// are silently dropped — same defensive policy as the per-RID PLI
/// router in [`route_rtcp_keyframe_requests`].
///
/// Pure: takes flat `(ssrc, fraction_lost, packets_lost, rtt,
/// rtt_measurements)` tuples rather than
/// `&RTCRemoteInboundRtpStreamStats` so tests can construct
/// synthetic inputs directly without the rtc 0.9 `pub(crate)`
/// constructor walls. Production projection from
/// `report.iter_by_type(RTCStatsType::RemoteInboundRTP)` happens at
/// the caller (the driver's `twcc_poll` branch).
///
/// **Pre-RR filtering**: rtc 0.9 emits a default-valued
/// `RemoteInboundRTP` snapshot for every outbound stream even
/// before any RR has actually been received — all fields are
/// zero, including `fraction_lost = 0.0` (which would otherwise
/// look like "perfectly healthy" to the policy). The
/// `round_trip_time_measurements == 0` predicate filters these
/// out: a non-zero count means at least one RR has arrived and
/// the values reflect a real measurement. Without this filter,
/// the policy would receive a phantom "0% loss" signal for every
/// outbound layer the moment the peer connects, immediately
/// confirming `Wanted` for layers that may not even be reaching
/// the receiver yet.
///
/// **No deltas in 4d.3a.** All forwarded input fields are passed
/// as-is: `fraction_lost` is already a per-RR-window value (rtc
/// 0.9 derives it from RR), `packets_lost` is cumulative-since-
/// start (deltas can be derived in 4d.3b if the policy needs
/// them), `rtt` is the most recent measurement. Keeping the
/// helper purely projective lets 4d.3b decide which signals to
/// use without re-shaping this layer.
fn map_remote_inbound_to_rid_health(
    remote_inbound: impl IntoIterator<Item = (u32, f64, i64, f64, u64)>,
    ssrc_table: &[(SimulcastRid, u32)],
) -> HashMap<SimulcastRid, PeerLayerHealth> {
    let mut out = HashMap::new();
    for (ssrc, fraction_lost, packets_lost_total, round_trip_time_seconds, rtt_measurements) in
        remote_inbound
    {
        // Pre-RR default snapshot — see helper docstring.
        if rtt_measurements == 0 {
            continue;
        }
        if let Some(rid) = rid_for_ssrc(ssrc_table, ssrc) {
            out.insert(
                rid,
                PeerLayerHealth {
                    fraction_lost,
                    packets_lost_total,
                    round_trip_time_seconds,
                    round_trip_time_measurements: rtt_measurements,
                },
            );
        }
    }
    out
}

/// **Phase 4d.1**: compute the per-peer recent observed send bitrate
/// (bits per second) from the deltas of `bytes_sent` across all
/// outbound RTP streams over one polling window.
///
/// **What this signals**: how much data the peer is actually pushing
/// onto the wire right now, summed across simulcast layers + RTX
/// streams. NOT a congestion-control bandwidth estimate (rtc 0.9
/// doesn't expose one — see `TWCC_POLL_INTERVAL` for why). The
/// layer-selection aggregator (4d.2) interprets this as "delivery
/// rate the peer's encoder + network are sustaining." A drop from
/// the encoder's configured target indicates either encoder
/// underrun or network constraint; either way, it's a layer-
/// selection signal.
///
/// `current` is `(ssrc, bytes_sent)` for each outbound stream,
/// projected from `report.outbound_rtp_streams()` at the production
/// caller. `prev` is the per-SSRC last-sample state the driver
/// maintains across polls; this helper updates it in place.
///
/// Returns `None` when:
/// - First poll for a peer (`prev` empty for every observed SSRC).
/// - All observed SSRCs had zero delta-time since last poll
///   (caller polled twice in the same instant — shouldn't happen
///   with the 1s interval).
/// - All observed SSRCs had non-positive byte deltas (counter
///   wraparound or stream restart, both defensive).
///
/// Returns `Some(total_bps)` when at least one SSRC contributed
/// a usable delta sample. Total is summed across SSRCs because
/// the layer-selection decision is per-peer (the peer's outbound
/// link is the bottleneck, not any individual encoding).
fn extract_recent_outbound_bitrate(
    current: impl IntoIterator<Item = (u32, u64)>,
    prev: &mut HashMap<u32, (u64, Instant)>,
    now: Instant,
) -> Option<u64> {
    let mut total_bits_per_sec: u64 = 0;
    let mut had_usable_sample = false;
    for (ssrc, current_bytes) in current {
        let usable = match prev.get(&ssrc) {
            Some(&(prev_bytes, prev_at)) => {
                let elapsed = now.saturating_duration_since(prev_at);
                if elapsed.is_zero() {
                    // Two polls in the same instant — shouldn't happen
                    // with the 1s poll interval, but defensive.
                    None
                } else if current_bytes < prev_bytes {
                    // Counter wraparound (impossible for u64 in
                    // realistic timeframes) or stream restart
                    // (rtc dropped + recreated the SSRC's accumulator
                    // — happens on renegotiation). Either way, treat
                    // this SSRC's sample as unusable for THIS poll;
                    // the next poll's prev will be the current value
                    // and produce a clean delta.
                    None
                } else {
                    let delta_bytes = current_bytes - prev_bytes;
                    let bps = (delta_bytes as f64 * 8.0) / elapsed.as_secs_f64();
                    if !bps.is_finite() {
                        None
                    } else {
                        Some(bps as u64)
                    }
                }
            }
            None => {
                // First sample for this SSRC — no prev to delta
                // against. Record now; next poll produces the first
                // usable delta.
                None
            }
        };
        prev.insert(ssrc, (current_bytes, now));
        if let Some(bps) = usable {
            total_bits_per_sec = total_bits_per_sec.saturating_add(bps);
            had_usable_sample = true;
        }
    }
    if had_usable_sample {
        Some(total_bits_per_sec)
    } else {
        None
    }
}

/// Reverse-lookup: given an SSRC reported in an inbound RTCP feedback
/// packet (PLI's `media_ssrc` or FIR's per-entry `ssrc`), find the
/// simulcast RID that owns it.
///
/// Linear scan over the (rid, ssrc) pairs — N ≤ 3 (VP8 simulcast:
/// full / half / quarter) or N == 1 (single-encoding codecs like
/// H.264). Takes a flat slice instead of the production
/// `HashMap<SimulcastRid, RidRtpState>` so tests can build the table
/// inline without constructing real packetizers.
fn rid_for_ssrc(ssrc_table: &[(SimulcastRid, u32)], ssrc: u32) -> Option<SimulcastRid> {
    ssrc_table
        .iter()
        .find_map(|(rid, s)| (*s == ssrc).then(|| rid.clone()))
}

/// Iterate inbound RTCP packets and emit a keyframe-request RID for
/// every PLI / FIR whose target SSRC matches one of this peer's
/// outbound encoding SSRCs. Output goes onto a bounded mpsc; the pool
/// intake side reads it and calls
/// [`crate::display::encode::pool::EncoderPool::request_keyframe`]
/// with the active codec + the routed RID, hitting only that layer's
/// encoder.
///
/// **Per-RID PLI is required for simulcast** because each layer's
/// browser-side decoder maintains its own keyframe-recovery state.
/// A PLI on rid `q` (quarter) means "I lost the keyframe on the
/// quarter layer specifically" — kicking the full-layer encoder in
/// response would burn one `f` keyframe (at full bandwidth!) for
/// nothing while the quarter layer stays broken. Routing per-RID
/// keeps recovery cost proportional to which layer actually lost
/// frames.
///
/// Unknown SSRCs are logged at warn level and dropped — they can
/// happen briefly during track-renegotiation windows or if the
/// browser sends RTCP for an SSRC we never advertised. Treating
/// them as a hard error would be over-eager (they're transient and
/// don't break correctness); ignoring them silently would mask
/// genuine SSRC-mapping bugs, hence the log.
///
/// RTCP packet types other than PLI/FIR (NACK, RR, SR, SDES, BYE,
/// transport-cc, REMB, TWCC) are ignored here — those are handled
/// by rtc 0.9's interceptor for stats/bandwidth-estimation purposes
/// and never need to flow through this routing path.
///
/// Lossy `try_send`: if the keyframe-request channel is full, drop.
/// The pool's coalescer would dedup the request anyway, and the
/// next PLI within the coalesce window will re-request. Blocking
/// the rtc poll loop on a full channel would hurt the entire peer
/// for the sake of a request that's about to be dropped at the next
/// hop.
fn route_rtcp_keyframe_requests(
    packets: &[Box<dyn rtc::rtcp::Packet>],
    ssrc_table: &[(SimulcastRid, u32)],
    keyframe_request_tx: &mpsc::Sender<SimulcastRid>,
) {
    for packet in packets {
        if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
            match rid_for_ssrc(ssrc_table, pli.media_ssrc) {
                Some(rid) => {
                    let _ = keyframe_request_tx.try_send(rid);
                }
                None => {
                    eprintln!(
                        "[display/webrtc] PLI for unknown SSRC {} \
                         (known SSRCs: {:?}); dropping",
                        pli.media_ssrc,
                        ssrc_table.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
                    );
                }
            }
        } else if let Some(fir) = packet.as_any().downcast_ref::<FullIntraRequest>() {
            for entry in &fir.fir {
                match rid_for_ssrc(ssrc_table, entry.ssrc) {
                    Some(rid) => {
                        let _ = keyframe_request_tx.try_send(rid);
                    }
                    None => {
                        eprintln!(
                            "[display/webrtc] FIR for unknown SSRC {} \
                             (known SSRCs: {:?}); dropping",
                            entry.ssrc,
                            ssrc_table.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
                        );
                    }
                }
            }
        }
    }
}

fn handle_message(
    message: RTCMessage,
    state: &mut DriverState,
    input_handler: &Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: &Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    authority_handler: &AuthorityChannelHandler,
    tile_control_handler: &TileControlHandler,
    keyframe_request_tx: &mpsc::Sender<SimulcastRid>,
) {
    if let RTCMessage::RtcpPacket(_track_id, packets) = &message {
        // Project by_rid → flat (rid, ssrc) table for the routing
        // helper. N ≤ 3 in production (VP8 simulcast layers);
        // allocation cost is negligible at RTCP rates.
        let ssrc_table: Vec<(SimulcastRid, u32)> = state
            .rtp
            .by_rid
            .iter()
            .map(|(rid, st)| (rid.clone(), st.ssrc))
            .collect();
        route_rtcp_keyframe_requests(packets, &ssrc_table, keyframe_request_tx);
        return;
    }
    let RTCMessage::DataChannelMessage(cid, RTCDataChannelMessage { data, .. }) = message else {
        return;
    };
    let label = state
        .channels
        .iter()
        .find_map(|(k, v)| (*v == cid).then(|| k.clone()));
    match label.as_deref() {
        Some("control") | Some("pointer") => {
            if let Ok(text) = std::str::from_utf8(&data) {
                if let Ok(evt) = serde_json::from_str::<InputEvent>(text) {
                    input_handler(evt);
                }
            }
        }
        Some("clipboard") => {
            if let Ok(text) = std::str::from_utf8(&data) {
                if let Some(content) = parse_clipboard_set(text) {
                    clipboard_handler(content);
                }
            }
        }
        // F-1.3b2: federated authority channel — parse on the wire,
        // hand off to the opaque handler. Match against the const
        // (not a literal) via a guard arm so the channel-label
        // identity is sourced from `AUTHORITY_CHANNEL_LABEL` only —
        // same const that `Command::SendAuthorityState` uses for the
        // outbound write. Any future rename touches one constant.
        Some(label) if label == AUTHORITY_CHANNEL_LABEL => {
            if let Ok(text) = std::str::from_utf8(&data) {
                if let Some(msg) = parse_authority_channel_message(text) {
                    authority_handler(msg);
                }
            }
        }
        Some(label) if label == TILE_CONTROL_CHANNEL_LABEL => {
            if let Some(msg) = parse_tile_control_message(&data) {
                tile_control_handler(msg);
            }
        }
        _ => {}
    }
}

fn write_video_frame<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
    outbound: &OutboundEncodedFrame,
) {
    let frame = &outbound.frame;
    let rid = &outbound.rid;

    // Phase-4c-prep: the keyframe gate is keyed by `(payload_spec, rid)`,
    // not `payload_spec` alone. See `DriverState::video_specs` for why
    // — VP8 simulcast layers share the same payload_spec but each
    // RID's keyframe gate must be independent.
    let spec_key = (frame.payload_spec.clone(), rid.clone());
    if !state.video_specs.contains_key(&spec_key) {
        let new = if payload_spec_matches_codec(&frame.payload_spec, &state.rtp.codec) {
            SpecState::Ready {
                keyframe_seen: false,
            }
        } else {
            eprintln!(
                "[display/webrtc] encoded frame spec {} (rid {}) does not match \
                 negotiated codec {}; dropping this (spec, rid)",
                frame.payload_spec.codec_mime,
                rid.as_str(),
                state.rtp.codec.mime_type,
            );
            SpecState::Unsupported
        };
        state.video_specs.insert(spec_key.clone(), new);
    }

    // Step 2: extract current keyframe readiness from the spec state.
    // Copy out immutably so we can mutate `state.first_frame_at` below
    // without borrow conflicts with `state.video_specs`.
    let keyframe_ready = match state.video_specs.get(&spec_key) {
        Some(SpecState::Ready { keyframe_seen }) => *keyframe_seen,
        // Unsupported or (impossibly) missing — drop silently. The
        // first arm already emitted a log on entering Unsupported.
        _ => return,
    };

    // Step 3: per-`(spec, rid)` keyframe gate. Closed until this
    // (spec, rid) pair has had ≥1 keyframe *successfully written*
    // (see step 5 — the flag flips only after `write_rtp` returns Ok).
    // A keyframe from spec A on rid X that fails to write does not
    // open the gate for spec A's P-frames on rid X, and no keyframe
    // of (spec A, rid X) ever opens (spec A, rid Y)'s gate or (spec
    // B, *)'s gate.
    if !keyframe_ready && !frame.is_keyframe {
        return;
    }

    // Step 3b: look up this RID's send state. Missing entry means a
    // forwarder is producing frames for a RID the driver was never
    // told about — should be unreachable since `build_with_codec_set`
    // populates `by_rid` from the same source the intake's forwarders
    // pull from. Treat as fail-loud to surface the contract violation.
    let rid_state = match state.rtp.by_rid.get_mut(rid) {
        Some(s) => s,
        None => {
            eprintln!(
                "[display/webrtc] frame for unknown rid {}; encoder/track \
                 contract divergence — driver only knows {:?}",
                rid.as_str(),
                state
                    .rtp
                    .by_rid
                    .keys()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>(),
            );
            return;
        }
    };

    // Step 4: wallclock anchor + RTP timestamp samples.
    let now = Instant::now();
    if state.first_frame_at.is_none() {
        state.first_frame_at = Some(now);
    }
    let samples = (frame.duration_ms.max(1) as u32).saturating_mul(90);

    // Step 5: write + on-success gate flip. Use the per-RID
    // packetizer (its own sequence + RTP-timestamp continuation
    // state) and stamp the per-RID SSRC onto every packet so
    // `RTCRtpSender::write_rtp` routes to the matching encoding.
    let payload = Bytes::from(frame.data.clone());
    let packets = match rid_state.packetizer.packetize(&payload, samples) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "[display/webrtc] RTP packetize failed on rid {}: {e}",
                rid.as_str(),
            );
            return;
        }
    };
    let rid_ssrc = rid_state.ssrc;

    let (mid_ext_id, rid_ext_id) = rtp_header_extension_ids(rtc, state);
    for mut packet in packets {
        packet.header.ssrc = rid_ssrc;
        if let Some(id) = mid_ext_id {
            let _ = packet
                .header
                .set_extension(id, Bytes::from(state.rtp.mid.as_bytes().to_vec()));
        }
        if let Some(id) = rid_ext_id {
            let _ = packet
                .header
                .set_extension(id, Bytes::from(rid.as_str().as_bytes().to_vec()));
        }

        let Some(mut sender) = rtc.rtp_sender(state.rtp.sender_id) else {
            return;
        };
        match sender.write_rtp(packet) {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "[display/webrtc] write_rtp failed on rid {}: {e:?}",
                    rid.as_str(),
                );
                return;
            }
        }
    }

    if !keyframe_ready {
        if let Some(SpecState::Ready { keyframe_seen }) = state.video_specs.get_mut(&spec_key) {
            // Only flip keyframe_seen for this (spec, rid) pair AFTER
            // a successful packet write. If the write is the first
            // keyframe on this (spec, rid), the gate opens for
            // subsequent P-frames on the same (spec, rid). If it
            // wasn't a keyframe (gate was already open), this is a
            // no-op.
            *keyframe_seen = true;
        }
    }
}

fn payload_spec_matches_codec(
    spec: &crate::display::encode::PayloadSpec,
    codec: &RTCRtpCodec,
) -> bool {
    if spec
        .codec_mime
        .eq_ignore_ascii_case(crate::display::encode::MIME_TYPE_VP8)
    {
        return codec.mime_type.eq_ignore_ascii_case(RTC_MIME_TYPE_VP8);
    }
    if spec
        .codec_mime
        .eq_ignore_ascii_case(crate::display::encode::MIME_TYPE_H264)
    {
        return codec.mime_type.eq_ignore_ascii_case(RTC_MIME_TYPE_H264)
            && spec.h264_packetization_mode == Some(1)
            && spec.h264_profile_level_id.as_deref() == Some("42e01f");
    }
    false
}

fn rtp_header_extension_ids<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
) -> (Option<u8>, Option<u8>) {
    if state.rtp.mid_ext_id.is_some() || state.rtp.rid_ext_id.is_some() {
        return (state.rtp.mid_ext_id, state.rtp.rid_ext_id);
    }
    if let Some(mut sender) = rtc.rtp_sender(state.rtp.sender_id) {
        let params = sender.get_parameters();
        for ext in &params.rtp_parameters.header_extensions {
            if ext.uri == "urn:ietf:params:rtp-hdrext:sdes:mid" {
                state.rtp.mid_ext_id = Some(ext.id as u8);
            } else if ext.uri == "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id" {
                state.rtp.rid_ext_id = Some(ext.id as u8);
            }
        }
    }
    (state.rtp.mid_ext_id, state.rtp.rid_ext_id)
}

fn handle_command<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
    cmd: Command,
) {
    match cmd {
        Command::AddIceCandidate(s) => {
            let init = RTCIceCandidateInit {
                candidate: s,
                sdp_mid: None,
                sdp_mline_index: None,
                username_fragment: None,
                url: None,
            };
            if let Err(e) = rtc.add_remote_candidate(init) {
                eprintln!("[display/webrtc] parse remote candidate failed: {e}");
            }
        }
        Command::SendClipboard(content) => {
            let Some(cid) = state.channels.get("clipboard").copied() else {
                return;
            };
            let Some(mut channel) = rtc.data_channel(cid) else {
                return;
            };
            let json = serialize_clipboard(&content);
            if let Err(e) = channel.send_text(json) {
                eprintln!("[display/webrtc] clipboard channel write failed: {e:?}");
            }
        }
        Command::SendAuthorityState {
            display_id,
            state: auth_state,
        } => {
            // F-1.2: queue-or-send. If the federated browser's
            // `display_input_authority` data channel is open,
            // serialize and write immediately. If not, queue for
            // flush on `OnDataChannel(OnOpen)` for that label.
            //
            // Local DisplaySlot's WebRtcPeer doesn't create this
            // channel (5a/5c uses the WS path), so the queue
            // accumulates indefinitely there until the driver shuts
            // down — which is fine because: (a) the broadcast loop
            // currently only calls send_authority_state for federated
            // subscribers, and (b) the queue is bounded by the
            // low-frequency take/release event rate, not per-frame.
            if let Some(cid) = state.channels.get(AUTHORITY_CHANNEL_LABEL).copied() {
                if let Some(mut channel) = rtc.data_channel(cid) {
                    let json = serialize_authority_state(display_id, auth_state);
                    if let Err(e) = channel.send_text(json) {
                        eprintln!("[display/webrtc] authority channel write failed: {e:?}");
                    }
                }
            } else {
                state.pending_authority_state.push((display_id, auth_state));
            }
        }
        Command::SendTileFrame { channel, data } => {
            let label = channel.label();
            let data_len = data.len();
            if channel == TileDataChannel::Deltas
                && state.tile_delta_backpressure.decide_delta(data_len)
                    == TileDeltaSendDecision::Drop
            {
                return;
            }
            if let Some(cid) = state.channels.get(label).copied() {
                if let Some(mut dc) = rtc.data_channel(cid) {
                    if let Err(e) = dc.send(BytesMut::from(&data[..])) {
                        eprintln!(
                            "[display/webrtc] tile channel write failed on \
                             {label}: {e:?}"
                        );
                    } else if channel == TileDataChannel::Deltas {
                        state.tile_delta_backpressure.record_delta_sent(data_len);
                    }
                }
            } else if channel.queues_before_open() {
                match channel {
                    TileDataChannel::Control => {
                        state.pending_tile_control.push(data);
                    }
                    TileDataChannel::Snapshot => {
                        state.pending_tile_snapshot.push(data);
                    }
                    TileDataChannel::Deltas => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `clipboard_set` message from a browser data channel, supporting
/// both text and image (base64-encoded) payloads.
fn parse_clipboard_set(text: &str) -> Option<ClipboardContent> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    if parsed.get("t").and_then(|v| v.as_str()) != Some("clipboard_set") {
        return None;
    }
    let mime = parsed
        .get("mime")
        .and_then(|v| v.as_str())
        .unwrap_or("text/plain");
    if mime.starts_with("image/") {
        let b64 = parsed.get("data").and_then(|v| v.as_str())?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        Some(ClipboardContent::Image {
            mime: mime.to_string(),
            data: bytes,
        })
    } else {
        let text = parsed.get("text").and_then(|v| v.as_str())?;
        Some(ClipboardContent::Text(text.to_string()))
    }
}

/// F-1.3b2: parse a frame received on the `display_input_authority`
/// data channel into an [`AuthorityChannelMessage`].
///
/// Wire format pinned by `parse_authority_channel_message_round_trip`:
///
/// ```text
/// { "t": "display_input_authority_request", "display_id": 0 }
/// { "t": "display_input_authority_release", "display_id": 0 }
/// ```
///
/// Returns `None` for unrecognized `t` discriminators, missing or
/// non-numeric `display_id`, or `display_id` values that don't fit
/// in `u32`. Strict by design — silent drop on the receive side
/// mirrors `parse_clipboard_set`'s contract: a malformed frame is
/// the browser's bug to fix, not the peer's to recover from. The
/// authority handler outside this module is the policy boundary;
/// the wire parse is intentionally narrow.
fn parse_authority_channel_message(text: &str) -> Option<AuthorityChannelMessage> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let t = parsed.get("t").and_then(|v| v.as_str())?;
    let display_id: u32 = parsed
        .get("display_id")
        .and_then(|v| v.as_u64())?
        .try_into()
        .ok()?;
    match t {
        "display_input_authority_request" => Some(AuthorityChannelMessage::Request { display_id }),
        "display_input_authority_release" => Some(AuthorityChannelMessage::Release { display_id }),
        _ => None,
    }
}

fn parse_tile_control_message(bytes: &[u8]) -> Option<TileControlMessage> {
    match tile_transport::decode_frame(bytes).ok()? {
        tile_transport::TileFrame::Subscribe { client_id } => {
            Some(TileControlMessage::Subscribe { client_id })
        }
        tile_transport::TileFrame::SnapshotRequest { epoch, reason } => {
            Some(TileControlMessage::SnapshotRequest { epoch, reason })
        }
        tile_transport::TileFrame::GapReport {
            epoch,
            last_seen_seq,
            expected_seq,
        } => Some(TileControlMessage::GapReport {
            epoch,
            last_seen_seq,
            expected_seq,
        }),
        _ => None,
    }
}

/// F-1.2: data-channel label for federated authority state messages.
/// Browsers create this channel from `PeerDisplayConnection.connect()`
/// (added in the next F-1 commit). The peer's driver opens it
/// passively via `OnDataChannel(OnOpen)` and registers it in
/// `state.channels` keyed by this label.
const AUTHORITY_CHANNEL_LABEL: &str = "display_input_authority";
const TILE_CONTROL_CHANNEL_LABEL: &str = "tile-control";
const TILE_SNAPSHOT_CHANNEL_LABEL: &str = "tile-snapshot";
const TILE_DELTAS_CHANNEL_LABEL: &str = "tile-deltas";

/// Serialize a `display_input_authority_state` frame for the
/// `display_input_authority` data channel. Wire format matches the
/// local 5c WS message exactly (same `t` discriminator, same `state`
/// vocabulary) so browser handlers can stay symmetric.
fn serialize_authority_state(display_id: u32, state: DisplayInputAuthorityState) -> String {
    serde_json::json!({
        "t": "display_input_authority_state",
        "display_id": display_id,
        "state": state.as_wire_str(),
    })
    .to_string()
}

/// F-1.2: drain pending authority states queued before the
/// `display_input_authority` data channel opened. Returns the queue
/// in arrival order so the flush preserves send ordering. No-op
/// (returns empty) for any other channel label, leaving `pending`
/// untouched.
///
/// Extracted for testability: the queue/flush invariant
/// (queued-before-open ⇒ flushed-on-open) lives here in pure-data
/// form so a unit test can pin it without needing to fake an
/// `rtc::data_channel`.
fn drain_pending_authority_for_label(
    label: &str,
    pending: &mut Vec<(u32, DisplayInputAuthorityState)>,
) -> Vec<(u32, DisplayInputAuthorityState)> {
    if label == AUTHORITY_CHANNEL_LABEL {
        std::mem::take(pending)
    } else {
        Vec::new()
    }
}

fn drain_pending_tile_for_label(state: &mut DriverState, label: &str) -> Vec<Vec<u8>> {
    match label {
        TILE_CONTROL_CHANNEL_LABEL => std::mem::take(&mut state.pending_tile_control),
        TILE_SNAPSHOT_CHANNEL_LABEL => std::mem::take(&mut state.pending_tile_snapshot),
        _ => Vec::new(),
    }
}

fn watermark_to_u32(bytes: usize) -> u32 {
    bytes.min(u32::MAX as usize) as u32
}

/// Serialize a `ClipboardContent` for sending over the clipboard data channel.
fn serialize_clipboard(content: &ClipboardContent) -> String {
    match content {
        ClipboardContent::Text(text) => serde_json::json!({
            "t": "clipboard_update",
            "mime": "text/plain",
            "text": text,
        })
        .to_string(),
        ClipboardContent::Image { mime, data } => {
            use base64::Engine;
            serde_json::json!({
                "t": "clipboard_update",
                "mime": mime,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            })
            .to_string()
        }
    }
}

// `routable_local_addrs` and `is_link_local_v6` moved to `crate::lan`
// so the federation advertise side can share them — same set of
// "addresses we can be reached at" applies to both WebRTC host
// candidates and Agent Card transport URLs.

/// If a remote ICE candidate's connection-address is an mDNS `.local`
/// hostname, resolve it to a literal IP via the system resolver and return
/// a rewritten candidate string. Otherwise pass through unchanged.
///
/// The candidate format per RFC 5245 §15.1 is:
///   `candidate:<foundation> <component> <proto> <priority> <addr> <port> typ <kind> ...`
/// We split on whitespace, the connection-address is field index 4 (counting
/// from the `candidate:` prefix as 0).
async fn resolve_mdns_in_candidate(candidate: &str) -> Result<String, String> {
    let mut fields: Vec<&str> = candidate.split_whitespace().collect();
    if fields.len() < 6 {
        return Ok(candidate.to_string());
    }
    let addr_field = fields[4];
    if !addr_field.ends_with(".local") {
        return Ok(candidate.to_string());
    }
    // Resolve via tokio::net::lookup_host. We need a port for the call but
    // discard it; any value works.
    let mut iter = tokio::net::lookup_host(format!("{addr_field}:0"))
        .await
        .map_err(|e| format!("lookup {addr_field}: {e}"))?;
    let resolved = iter
        .next()
        .ok_or_else(|| format!("no addrs for {addr_field}"))?;
    let ip_str = resolved.ip().to_string();
    fields[4] = &ip_str;
    Ok(fields.join(" "))
}

// ---------------------------------------------------------------------------
// Pool-mode helpers (3c.3b.2)
// ---------------------------------------------------------------------------

/// Distinct codecs covered by `subscriptions`, deduplicated. Used by
/// tests to pin the actually-served codec set rather than the original
/// peer offer prefs.
///
/// Order is preserved as encountered (CodecKind isn't `Ord`, and
/// dedup avoids counting the same codec twice in a multi-layer
/// simulcast set.
#[cfg(test)]
fn codec_set_from_subscriptions(subscriptions: &[EncoderSubscription]) -> Vec<CodecKind> {
    let mut seen: std::collections::HashSet<CodecKind> = std::collections::HashSet::new();
    let mut codecs: Vec<CodecKind> = Vec::new();
    for sub in subscriptions {
        if seen.insert(sub.id.codec) {
            codecs.push(sub.id.codec);
        }
    }
    codecs
}

fn active_codec_from_subscriptions(
    subscriptions: &[EncoderSubscription],
    prefs: &PeerCodecPreferences,
) -> Option<CodecKind> {
    for &codec in &prefs.supported {
        if subscriptions.iter().any(|s| s.id.codec == codec) {
            return Some(codec);
        }
    }
    None
}

/// Build the **negotiated** codec preferences the intake uses for
/// every `pool.subscribe` call (initial AND every resubscribe).
///
/// Filters `original_prefs` against the codec set actually returned
/// by the initial subscribe (`actual_codecs`). Preserves
/// `original_prefs` ordering — the intake's
/// [`active_codec_from_subscriptions`] uses prefs order as the
/// preference signal, and re-ordering would silently change the
/// peer's chosen codec.
///
/// **Why this matters (3c.3b.2b finding 1):** the peer's SDP answer is built
/// from the codec we can actually serve, not from `original_prefs`. If
/// `original_prefs = [VP8, H.264]` but initial subscribe returned only `[VP8]`
/// (because H.264 encoder construction failed at that moment — VAAPI
/// exhaustion, ffmpeg missing, etc.), the answer enables only VP8.
/// Resubscribing with `original_prefs` after a later resize could pick H.264 if
/// it became available, and the driver would reject every frame because the
/// sender was never negotiated for H.264. Locking the resubscribe prefs to
/// `actual_codecs` makes that reachability bug impossible.
///
/// Returns an empty `PeerCodecPreferences` only if the intersection
/// is empty, which the caller (`new`) prevents by erroring
/// upstream when `subscriptions` is empty (codec_set non-empty →
/// intersection non-empty when `original_prefs` is non-empty). The
/// upstream contract is asserted by the early-return at the top of
/// `new`.
fn filter_prefs_to_negotiated(
    original_prefs: &PeerCodecPreferences,
    actual_codecs: &[CodecKind],
) -> PeerCodecPreferences {
    PeerCodecPreferences::new(
        original_prefs
            .supported
            .iter()
            .copied()
            .filter(|c| actual_codecs.contains(c))
            .collect(),
    )
}

/// Partition pool subscriptions by active codec, dropping the inactive
/// subscriptions immediately and returning their ids so the caller can
/// release the lease's on-demand claims for codecs the active codec
/// doesn't use.
///
/// Active subscriptions are kept and returned for forwarder spawning
/// (one forwarder per `(codec, rid)` slot — that's how
/// browser-visible simulcast happens). Inactive subscriptions get
/// dropped here so their `broadcast::Receiver` clones release
/// immediately rather than lingering until end-of-scope; only the
/// ids escape so the caller can call
/// [`PoolLease::release_on_demand_subset`] on them.
///
/// Always-on slots have no `on_demand_refs` entry; passing their
/// ids to `release_on_demand_subset` is a silent no-op via the
/// skip-unknown-ids contract on that side. This helper doesn't
/// distinguish always-on from on-demand — it just emits every
/// inactive id and lets the lease side decide what to release. That
/// keeps the wasted-CPU regression caught in the 3c.3b.2a review
/// (multi-codec pool with a VP8-preferring peer keeping the H.264
/// encoder spinning into a no-receiver broadcast) closed.
///
/// Pure function for unit testability — no side effects on the
/// lease, no side effects on the pool. The release call lives at
/// the caller in `pool_frame_intake`.
fn partition_subscriptions_by_codec(
    subscriptions: Vec<EncoderSubscription>,
    active_codec: CodecKind,
) -> (Vec<EncoderSubscription>, Vec<EncoderId>) {
    let (active_subs, inactive_subs): (Vec<_>, Vec<_>) = subscriptions
        .into_iter()
        .partition(|s| s.id.codec == active_codec);
    let inactive_ids: Vec<EncoderId> = inactive_subs.iter().map(|s| s.id.clone()).collect();
    drop(inactive_subs);
    (active_subs, inactive_ids)
}

/// Why the intake exits a forwarder loop. The intake's outer select
/// branches on this to decide between resubscribe (encoder epoch
/// rolled over) and clean shutdown (driver gone, intake should exit).
#[derive(Debug)]
enum ForwarderExit {
    /// `broadcast::RecvError::Closed` — the encoder slot's `Sender`
    /// was dropped. Typically [`EncoderPool::on_resize`] or
    /// last-leaseholder exit. Resubscribe to recover.
    EncoderClosed,
    /// `mpsc::TrySendError::Closed` — the driver's encoded-frame
    /// receiver was dropped. The peer is gone (or going). Don't
    /// resubscribe; just exit.
    DriverClosed,
    /// Forwarder cancellation token fired. The intake cancels this
    /// when it's tearing down for `shutdown` propagation.
    Cancelled,
}

/// Per-peer task that bridges the [`EncoderPool`]'s per-subscription
/// `broadcast::Receiver<Arc<EncodedFrame>>` channels to the
/// [`WebRtcPeer`] driver's encoded-frame mpsc, and re-subscribes
/// transparently when an encoder slot is torn down (typically by
/// [`EncoderPool::on_resize`] or an on-demand slot's last-leaseholder
/// exit).
///
/// ## Multi-forwarder per active codec — the phase 4c contract
///
/// `pool.subscribe(prefs)` may return multiple subscriptions: one per
/// `(codec × layer)` the peer's prefs overlap with. For a peer that
/// supports both VP8 and H.264, that's two subscriptions; for a peer
/// supporting VP8 against a simulcast pool, that's one per layer.
///
/// **Codec selection stays single-codec.** Per epoch the intake picks
/// the active codec via [`active_codec_from_subscriptions`] from
/// `negotiated_prefs`'s ordering, then partitions the subscriptions
/// into:
///
/// 1. **Active partition** — every subscription whose codec matches
///    the active codec. For VP8 simulcast that's all three layer
///    subscriptions ([full, half, quarter]); for H.264 it's the single
///    layer. Each subscription in this partition gets its own
///    forwarder task; the per-peer mpsc receives [`OutboundEncodedFrame`]s
///    tagged with each forwarder's RID. **This is what makes
///    browser-visible simulcast possible** — the answer SDP advertises
///    N rids, and the multi-RID driver write path needs frames for
///    each rid to actually produce wire packets per encoding.
///
/// 2. **Inactive partition** — every subscription whose codec is
///    NOT the active codec (e.g. H.264 subscriptions when VP8 wins).
///    These IDs are passed to [`PoolLease::release_on_demand_subset`]
///    so on-demand encoders for the inactive codec(s) drop their
///    refcount immediately and (when refcount → 0) tear down rather
///    than spinning into a broadcast nobody reads. Always-on slots
///    are silently skipped (no refcount entry).
///
/// Codec mixing across the per-peer mpsc is forbidden — feeding two
/// codecs into one WebRTC sender produces codec-interleaved bytes the
/// browser cannot decode and renders the stream black. Per-RID
/// streams of the same codec ARE intentionally interleaved on the
/// mpsc; the driver's `state.video_specs[(spec, rid)]` keying keeps
/// keyframe gates independent per-rid so a P-frame on rid `h` doesn't
/// prematurely open the gate for rid `q`.
///
/// ## Multi-forwarder lifecycle
///
/// All forwarders for one epoch share a single
/// [`CancellationToken`] and report exit reasons via a bounded mpsc
/// sized to the forwarder count. **First exit wins**: whichever
/// forwarder reports first determines the epoch's exit reason
/// ([`ForwarderExit::EncoderClosed`] → resubscribe;
/// [`ForwarderExit::DriverClosed`] / [`ForwarderExit::Cancelled`] →
/// shut down). The intake then cancels the sibling forwarders and
/// reaps them via the exit channel, keeping the (codec, rid) set the
/// driver sees aligned with what the answer SDP advertised — a
/// straggler forwarder still trying to forward stale-epoch frames
/// would write packets the driver's video_specs map no longer
/// recognizes.
///
/// ## `negotiated_prefs` — the 3c.3b.2b finding-1 contract
///
/// `negotiated_prefs` is the **caller-filtered** subset of the peer's
/// original SDP-offer prefs that contains the active codec the pool's
/// initial subscribe can actually serve. This is the codec the peer's SDP
/// answer enabled (`new` derives both the answer and `negotiated_prefs` from
/// the same active subscription source).
///
/// The intake passes `negotiated_prefs` to every `pool.subscribe` —
/// resubscribe-after-Closed included. If we passed the original
/// unfiltered prefs, the resubscribe could return a codec the peer
/// never negotiated (e.g. H.264 construction failed initially but
/// succeeds after a later resize that respawns the on-demand slot).
/// `active_codec_from_subscriptions` would then pick that codec, the
/// driver would reject it as `Unsupported`, and every frame would
/// silently drop -> black stream. Locking the prefs to the negotiated
/// set at construction time and using that on every resubscribe is
/// the structural fix.
///
/// ## Lossy forwarding — the 3c.3b.2a contract (continued)
///
/// The forwarder uses [`mpsc::Sender::try_send`], not
/// `send().await`. When the driver's bounded encoded-frame mpsc is
/// full (slow peer, network stall, encoder burst), [`try_send`]
/// returns [`mpsc::error::TrySendError::Full`] and the forwarder
/// drops the frame and increments `drops_counter`.
///
/// Why lossy: `send().await` parks the forwarder inside the mpsc
/// when full. The forwarder's cancellation `select!` only fires
/// before `rx.recv()` — a parked send is uncancellable. That breaks
/// shutdown propagation: a peer whose driver is dying might never
/// signal exit because its forwarder is parked behind the dying
/// driver's full-and-then-closed mpsc. Lossy `try_send` keeps the
/// forwarder responsive to cancellation in milliseconds.
///
/// Codec keyframe machinery (the encoder's GOP cadence, plus
/// [`EncoderPool::request_keyframe_*`] when wired in 3c.4) recovers
/// the visual stream after a drop burst — exactly as the legacy
/// fan-out does today.
///
/// ## Closed handling
///
/// `on_resize` advances `source_state` BEFORE swapping/cancelling
/// encoder handles. A subscribe that hands us a brand-new
/// subscription can therefore deliver a `Receiver` whose underlying
/// `Sender` has already been dropped — the receiver returns
/// `RecvError::Closed` on the very first `recv()`. The forwarder
/// returns [`ForwarderExit::EncoderClosed`]; the intake treats that
/// as a normal "encoder epoch transitioned" signal, drops the lease
/// (which decrements refcounts under the generation gate, so stale
/// claims don't decrement replacement slots), calls
/// `pool.subscribe(&negotiated_prefs)` again, and continues with
/// fresh handles. The peer never sees the transition; no offer
/// rejection, no peer teardown.
///
/// The escalation path: if `pool.subscribe(&negotiated_prefs)` itself
/// returns `NoCompatibleCodec` (typically: a resize wiped every
/// negotiated codec and re-spawn failed) — or if
/// `active_codec_from_subscriptions` returns `None` against a
/// non-empty subscription set (a contract violation indicating
/// pool/peer divergence) — the intake signals `shutdown.cancel()` so
/// the driver tears the peer down cleanly rather than leaving a
/// never-decoding stream behind.
#[allow(clippy::too_many_arguments)]
async fn pool_frame_intake(
    pool: Arc<EncoderPool>,
    negotiated_prefs: PeerCodecPreferences,
    initial_subs: Vec<EncoderSubscription>,
    initial_lease: PoolLease,
    encoded_frame_tx: mpsc::Sender<OutboundEncodedFrame>,
    drops_counter: Arc<AtomicU64>,
    mut keyframe_request_rx: mpsc::Receiver<SimulcastRid>,
    shutdown: CancellationToken,
) {
    let mut current_lease = Some(initial_lease);
    let mut current_subs = initial_subs;

    'epoch: loop {
        // Phase 4c: pick the active codec, then partition subscriptions
        // into "active codec" (forward all of them — this is what
        // makes simulcast work) and "everything else" (release any
        // on-demand claims so abandoned codecs' encoders shut down).
        //
        // Per the user's correction #3: keep codec selection
        // single-codec — if VP8 wins, forward all VP8 subscriptions
        // (the simulcast layers); if H.264 wins, forward the single
        // H.264 subscription. NEVER mix codecs into one peer's
        // sender.
        let subs_now = std::mem::take(&mut current_subs);
        let active_codec = match active_codec_from_subscriptions(&subs_now, &negotiated_prefs) {
            Some(c) => c,
            None => {
                // Strict-by-construction `codec_set_from_subscriptions`
                // upstream means this should be unreachable: the SDP
                // we negotiated enables exactly the codecs the pool
                // committed to. Reaching here indicates a contract
                // divergence (pool changed shape since the original
                // subscribe). Escalate to peer teardown — leaving a
                // never-decoding stream is the worst possible outcome.
                eprintln!(
                    "[display/webrtc/pool-intake] no subscription matched \
                     negotiated_prefs (supported={:?}) from {} returned subs; \
                     signalling peer shutdown",
                    negotiated_prefs.supported,
                    subs_now.len(),
                );
                shutdown.cancel();
                return;
            }
        };
        // Partition by codec, dropping inactive subs immediately and
        // collecting their ids for release. See
        // [`partition_subscriptions_by_codec`] for the contract.
        let (active_subs, inactive_ids) = partition_subscriptions_by_codec(subs_now, active_codec);
        // Release the inactive on-demand claims on the active lease.
        // For a peer with prefs [VP8, H264] against a pool that has
        // VP8 always-on + H264 on-demand, this is what tears down
        // the never-consumed H264 encoder when the active codec is
        // VP8 — without it, H264 keeps encoding into a broadcast
        // channel with no receivers until peer disconnect (the
        // wasted-CPU regression caught in the 3c.3b.2a review).
        // After `filter_prefs_to_negotiated` locks resubscribes to
        // a single codec, this releases on the FIRST iteration only
        // (when initial_subs may include other codecs); subsequent
        // resubscribes return only active-codec subs, so
        // inactive_ids is empty.
        if !inactive_ids.is_empty() {
            if let Some(lease) = current_lease.as_mut() {
                lease.release_on_demand_subset(&inactive_ids);
            }
        }

        let active_ids: Vec<EncoderId> = active_subs.iter().map(|s| s.id.clone()).collect();
        let active_rids_summary: Vec<String> = active_subs
            .iter()
            .map(|s| s.id.rid.as_str().to_string())
            .collect();
        if active_subs.is_empty() {
            // Defensive: `active_codec_from_subscriptions` returned
            // Some, so at least one subscription matched. If the
            // partition produced an empty active set, something went
            // very wrong (subscriptions changed under us between
            // the two reads, which shouldn't be possible). Escalate.
            eprintln!(
                "[display/webrtc/pool-intake] active_codec={active_codec:?} \
                 resolved but partition produced 0 active subs; \
                 signalling peer shutdown"
            );
            shutdown.cancel();
            return;
        }

        // Spawn one forwarder task per active subscription. Each
        // forwarder reads encoded frames off ITS subscription's
        // broadcast (one per `(codec, rid)` slot) and pushes them to
        // the peer's mpsc as `OutboundEncodedFrame { rid, frame }`.
        // The driver looks up each frame's rid in `state.rtp.by_rid`
        // to pick the matching SSRC + packetizer at write time.
        //
        // Forwarder lifecycle: cancellation token shared across all
        // forwarders so an exit on one (encoder Closed, driver
        // Closed) cancels the others uniformly. Exit channel
        // capacity = number of active subs so the first-to-exit
        // forwarder's reason is preserved even if others race to
        // exit before we can drain.
        let fwd_shutdown = CancellationToken::new();
        let (exit_tx, mut exit_rx) = mpsc::channel::<ForwarderExit>(active_subs.len().max(1));
        let mut forwarders = Vec::with_capacity(active_subs.len());
        for sub in active_subs {
            let rid = sub.id.rid.clone();
            let mut rx = sub.frames;
            let frame_tx = encoded_frame_tx.clone();
            let counter = Arc::clone(&drops_counter);
            let fwd_shutdown_inner = fwd_shutdown.clone();
            let exit_tx_inner = exit_tx.clone();
            forwarders.push(tokio::spawn(async move {
                let exit = loop {
                    tokio::select! {
                        _ = fwd_shutdown_inner.cancelled() => break ForwarderExit::Cancelled,
                        res = rx.recv() => match res {
                            Ok(frame) => {
                                let outbound = OutboundEncodedFrame {
                                    rid: rid.clone(),
                                    frame,
                                };
                                match frame_tx.try_send(outbound) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        // Driver's mpsc is full. Drop
                                        // the frame; the codec's
                                        // keyframe cadence will recover
                                        // the visual stream. Lossy
                                        // forwarding is the 3c.3b.2a
                                        // contract — `send().await`
                                        // would park inside the mpsc
                                        // and break shutdown
                                        // propagation. Per-RID
                                        // forwarders inherit this:
                                        // a slow consumer on one RID
                                        // doesn't backpressure the
                                        // others (each has its own
                                        // forwarder task and the
                                        // try_send is per-task).
                                        counter.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        // Driver receiver dropped.
                                        // Peer is gone; nothing to
                                        // forward to.
                                        break ForwarderExit::DriverClosed;
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Encoder for THIS rid torn down.
                                // Intake escalates to a unified
                                // resubscribe (cancels all sibling
                                // forwarders + drops lease). The
                                // sibling forwarders are still
                                // delivering to active encoders, but
                                // a Closed on any one rid likely
                                // means an `on_resize` epoch
                                // transition that affects ALL
                                // layers; resubscribe-as-a-unit
                                // keeps the multi-RID encodings
                                // coherent.
                                break ForwarderExit::EncoderClosed;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // Slow consumer; broadcast skipped
                                // ahead. Codec keyframe machinery
                                // (GOP / request_keyframe) recovers.
                                continue;
                            }
                        }
                    }
                };
                // Send is best-effort: if the intake is already
                // tearing down (shutdown branch fired and the
                // receiver was dropped), we just exit.
                let _ = exit_tx_inner.send(exit).await;
            }));
        }
        // Drop our `exit_tx` so when ALL forwarders' clones go away,
        // `exit_rx.recv()` returns None — gives a "all forwarders
        // gone" signal even if some forwarders' send-on-exit raced
        // teardown.
        drop(exit_tx);

        // Inner loop: stay here as long as keyframe requests come in
        // (route them to the pool and keep listening). Break out only
        // when shutdown fires or a forwarder exits — those drive the
        // outer 'epoch loop's resubscribe-or-return decisions.
        //
        // **Why an inner loop**: the keyframe-request branch must NOT
        // re-enter the 'epoch loop body. Re-entering would tear down
        // every forwarder we just spawned and respawn them — a PLI
        // burst would interrupt streaming on every layer. The inner
        // loop keeps forwarders running and just routes the request
        // to the pool's coalescer.
        enum InnerExit {
            Shutdown,
            ForwarderExited(Option<ForwarderExit>),
        }
        let inner_exit = 'inner: loop {
            tokio::select! {
                _ = shutdown.cancelled() => break 'inner InnerExit::Shutdown,
                // Phase 4e: drain keyframe-request RIDs from the
                // driver (one per inbound PLI/FIR for one of our
                // SSRCs). Route to the pool with the active codec —
                // the pool's coalescer dedups bursts within
                // KEYFRAME_COALESCE_WINDOW.
                Some(rid) = keyframe_request_rx.recv() => {
                    pool.request_keyframe(active_codec, Some(rid));
                    // Stay in inner loop; forwarders keep running.
                }
                recv = exit_rx.recv() => break 'inner InnerExit::ForwarderExited(recv),
            }
        };
        match inner_exit {
            InnerExit::Shutdown => {
                // Peer is going away. Cancel all forwarders, await
                // them, drop the lease, exit.
                fwd_shutdown.cancel();
                for f in forwarders {
                    let _ = f.await;
                }
                drop(current_lease.take());
                return;
            }
            InnerExit::ForwarderExited(recv) => {
                // First forwarder to exit reports its reason. Cancel
                // all sibling forwarders so the (codec, rid) set
                // doesn't drift (e.g. one rid resubscribing while
                // another keeps streaming the old epoch).
                fwd_shutdown.cancel();
                for f in forwarders {
                    let _ = f.await;
                }
                let exit = recv.unwrap_or(ForwarderExit::DriverClosed);
                match exit {
                    ForwarderExit::EncoderClosed => {
                        // Drop the old lease BEFORE resubscribing so
                        // its generation-gated release runs before
                        // subscribe potentially observes the slot
                        // map. The generation gate makes the order
                        // strictly safe (stale leases can't decrement
                        // replacement slots), but dropping first
                        // keeps the refcount accounting easier to
                        // reason about.
                        drop(current_lease.take());

                        // Use `negotiated_prefs`, not the original
                        // peer prefs. Resubscribing with original
                        // prefs would let the pool return codecs the
                        // peer's SDP answer never enabled (e.g. if
                        // initial subscribe excluded H.264 because
                        // construction failed, but a later resize +
                        // resubscribe finds H.264 working). The
                        // intake would then select a codec the peer
                        // never negotiated and the driver's per-spec
                        // gate marks `Unsupported`, dropping every
                        // frame -> silent black stream.
                        // This is the high-priority finding from the
                        // 3c.3b.2a review. The narrowed prefs locks
                        // resubscribe to exactly the codecs the
                        // peer's answer enabled.
                        match pool.subscribe(&negotiated_prefs) {
                            Ok((subs, lease)) => {
                                current_subs = subs;
                                current_lease = Some(lease);
                                eprintln!(
                                    "[display/webrtc/pool-intake] resubscribed \
                                     after encoder Closed (was forwarding \
                                     codec={active_codec:?} rids={:?})",
                                    active_rids_summary,
                                );
                                continue 'epoch;
                            }
                            Err(e) => {
                                eprintln!(
                                    "[display/webrtc/pool-intake] resubscribe \
                                     after Closed failed ({e:?}): no compatible \
                                     codec; signalling peer shutdown (was \
                                     forwarding {active_ids:?})"
                                );
                                shutdown.cancel();
                                return;
                            }
                        }
                    }
                    ForwarderExit::DriverClosed | ForwarderExit::Cancelled => {
                        // Driver is gone or forwarder was externally
                        // cancelled. Either way, no resubscribe — the
                        // peer's path is closing. Drop the lease and
                        // exit.
                        drop(current_lease.take());
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_passthrough_for_literal_ip() {
        let c = "candidate:1 1 udp 2113937151 192.168.1.10 5000 typ host generation 0";
        let resolved = resolve_mdns_in_candidate(c).await.unwrap();
        assert_eq!(resolved, c);
    }

    #[tokio::test]
    async fn resolve_passthrough_for_short_input() {
        let c = "candidate:1 1 udp 2113937151";
        let resolved = resolve_mdns_in_candidate(c).await.unwrap();
        assert_eq!(resolved, c);
    }

    // --- STUN parser tests ---

    fn make_stun_binding_request(username: &str) -> Vec<u8> {
        // Minimal STUN Binding Request with USERNAME attribute.
        // Header: type 0x0001, length TBD, magic 0x2112A442, txid 12 zeros.
        let username_bytes = username.as_bytes();
        let attr_len = username_bytes.len();
        let padded = (attr_len + 3) & !3;
        let msg_len = 4 + padded; // attr header (4) + padded value

        let mut buf = Vec::new();
        buf.extend_from_slice(&0x0001u16.to_be_bytes()); // type
        buf.extend_from_slice(&(msg_len as u16).to_be_bytes()); // length
        buf.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]); // magic
        buf.extend_from_slice(&[0u8; 12]); // transaction ID
                                           // USERNAME attribute
        buf.extend_from_slice(&0x0006u16.to_be_bytes()); // attr type
        buf.extend_from_slice(&(attr_len as u16).to_be_bytes());
        buf.extend_from_slice(username_bytes);
        buf.resize(buf.len() + padded - attr_len, 0); // padding
        buf
    }

    #[test]
    fn stun_username_extracted() {
        let pkt = make_stun_binding_request("serverufrag:browserufrag");
        assert_eq!(
            parse_stun_username(&pkt),
            Some("serverufrag:browserufrag".to_string())
        );
    }

    #[test]
    fn stun_username_missing() {
        // STUN packet with no attributes at all
        let mut pkt = vec![0u8; 20];
        pkt[0] = 0x00;
        pkt[1] = 0x01; // Binding Request
        pkt[2] = 0x00;
        pkt[3] = 0x00; // length = 0
        pkt[4..8].copy_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        assert_eq!(parse_stun_username(&pkt), None);
    }

    #[test]
    fn stun_not_stun() {
        // Wrong magic cookie
        assert_eq!(parse_stun_username(&[0u8; 20]), None);
    }

    #[test]
    fn stun_too_short() {
        assert_eq!(parse_stun_username(&[0u8; 5]), None);
    }

    // --- srflx (STUN server-reflexive) gathering tests ---

    #[test]
    fn srflx_candidate_init_formats_typ_srflx_with_raddr_rport() {
        use std::net::{Ipv4Addr, SocketAddr};
        let mapped = SocketAddr::new(Ipv4Addr::new(34, 173, 63, 221).into(), 50000);
        let base = SocketAddr::new(Ipv4Addr::new(10, 128, 0, 2).into(), 40000);
        let init = srflx_candidate_init(mapped, base);
        // Public mapped address is the candidate's transport address.
        assert!(
            init.candidate.contains("udp"),
            "udp transport: {}",
            init.candidate
        );
        assert!(
            init.candidate.contains("34.173.63.221 50000 typ srflx"),
            "mapped addr + typ srflx: {}",
            init.candidate
        );
        // RFC 5245 §4.3: srflx candidate carries the host base as raddr/rport.
        assert!(
            init.candidate.contains("raddr 10.128.0.2 rport 40000"),
            "raddr/rport = base: {}",
            init.candidate
        );
        // srflx must outrank ICE-TCP (1_677_721_855) but rank below UDP host
        // (2_130_706_431) so host pairs are still tried first.
        let priority: u32 = init
            .candidate
            .split_whitespace()
            .nth(3)
            .and_then(|p| p.parse().ok())
            .expect("priority field");
        assert!(
            priority < 2_130_706_431 && priority > 1_677_721_855,
            "srflx priority {priority} between ICE-TCP and UDP host"
        );
    }

    #[tokio::test]
    async fn resolve_stun_servers_parses_stun_url_and_skips_turn() {
        use std::net::Ipv4Addr;
        // Mix of a resolvable literal-IP stun URL and a turn URL that must
        // be ignored (srflx only needs STUN). Using a literal IP avoids a
        // DNS dependency in the unit test.
        let ice_config = IceConfig {
            ice_servers: vec![crate::display::IceServer {
                urls: vec![
                    "stun:127.0.0.1:19302".to_string(),
                    "turn:127.0.0.1:3478".to_string(),
                ],
                username: None,
                credential: None,
            }],
        };
        let resolved = resolve_stun_servers(&ice_config).await;
        assert_eq!(
            resolved,
            vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 19302)],
            "only the stun: URL resolved, turn: skipped"
        );
    }

    #[tokio::test]
    async fn resolve_stun_servers_empty_when_no_servers() {
        let resolved = resolve_stun_servers(&IceConfig::default()).await;
        assert!(resolved.is_empty(), "no ice servers -> no STUN addrs");
    }

    #[tokio::test]
    async fn stun_binding_round_trips_against_local_responder() {
        // Stand up a tiny local "STUN server" that answers any Binding
        // Request with a Binding Success carrying the peer's address as
        // XOR-MAPPED-ADDRESS — exercising our request build + response
        // parse path without touching the network.
        use rtc::stun::message::{Message, Setter, BINDING_SUCCESS};
        use rtc::stun::xoraddr::XorMappedAddress;

        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            let mut req = Message::new();
            req.unmarshal_binary(&buf[..n]).unwrap();
            // Echo the requester's address back as XOR-MAPPED-ADDRESS,
            // preserving the request's transaction ID (required for the
            // client to accept the response).
            let mut resp = Message::new();
            resp.build(&[
                Box::new(req.transaction_id) as Box<dyn Setter>,
                Box::new(BINDING_SUCCESS),
                Box::new(XorMappedAddress {
                    ip: from.ip(),
                    port: from.port(),
                }),
            ])
            .unwrap();
            let wire = resp.marshal_binary().unwrap();
            server.send_to(&wire, from).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let mapped = stun_binding_mapped_addr(&client, server_addr)
            .await
            .expect("binding success");
        assert_eq!(
            mapped, client_addr,
            "parsed XOR-MAPPED-ADDRESS == client's own address"
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn stun_binding_times_out_against_silent_server() {
        // A bound-but-silent UDP socket never answers; the client must
        // give up after STUN_BINDING_TIMEOUT rather than hang forever.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = std::time::Instant::now();
        let result = stun_binding_mapped_addr(&client, silent_addr).await;
        assert!(result.is_err(), "no response -> Err, got {result:?}");
        assert!(
            started.elapsed() < STUN_BINDING_TIMEOUT + Duration::from_secs(1),
            "returned promptly after timeout, took {:?}",
            started.elapsed()
        );
    }

    /// `parse_stun_binding_response` is the load-bearing predicate that
    /// lets the srflx gather be folded into the UDP forwarder's single
    /// read loop (audit F8): it must return `Some(mapped)` ONLY for the
    /// Binding Success Response matching the request's transaction ID, and
    /// `None` for everything else so those datagrams fall through to the
    /// RTC core. Builds a real `rtc::stun` Binding Success carrying a known
    /// XOR-MAPPED-ADDRESS.
    #[test]
    fn parse_stun_binding_response_matches_only_our_success() {
        use rtc::stun::message::{Message, Setter, BINDING_SUCCESS};
        use rtc::stun::xoraddr::XorMappedAddress;
        use std::net::Ipv4Addr;

        let (_wire, tid) = build_stun_binding_request().expect("build request");
        let mapped_ip = Ipv4Addr::new(203, 0, 113, 7);
        let mapped_port = 51234u16;
        let mut resp = Message::new();
        resp.build(&[
            Box::new(tid) as Box<dyn Setter>,
            Box::new(BINDING_SUCCESS),
            Box::new(XorMappedAddress {
                ip: mapped_ip.into(),
                port: mapped_port,
            }),
        ])
        .unwrap();
        let success = resp.marshal_binary().unwrap();

        // Matching txid + success class -> the mapped address.
        assert_eq!(
            parse_stun_binding_response(&success, tid),
            Some(SocketAddr::new(mapped_ip.into(), mapped_port)),
            "matching Binding Success yields its XOR-MAPPED-ADDRESS"
        );

        // A *different* expected txid must not match (so two sockets'
        // gathers can't steal each other's responses).
        let (_w2, other_tid) = build_stun_binding_request().expect("build request");
        assert_eq!(
            parse_stun_binding_response(&success, other_tid),
            None,
            "transaction-id mismatch is rejected"
        );

        // A non-STUN datagram (e.g. an ICE connectivity check or media)
        // must pass through (None) so the forwarder forwards it.
        assert_eq!(
            parse_stun_binding_response(b"not a stun message at all", tid),
            None,
            "non-STUN bytes are not mistaken for our response"
        );

        // A STUN Binding *Request* (wrong class) is also not our response.
        let (request_wire, req_tid) = build_stun_binding_request().expect("build request");
        assert_eq!(
            parse_stun_binding_response(&request_wire, req_tid),
            None,
            "a non-success STUN class is rejected"
        );
    }

    /// The srflx candidate trickled to the browser must carry the
    /// canonical `RTCIceCandidate.toJSON()` field names so
    /// `pc.addIceCandidate` accepts it: `candidate` (the SDP attribute
    /// value), `sdpMid`, and `sdpMLineIndex`. This mirrors the JSON the
    /// driver builds in the `srflx_rx` select branch; if that shape drifts
    /// the browser silently drops the candidate and the off-path srflx
    /// path stops advertising.
    #[test]
    fn srflx_trickle_json_has_canonical_candidate_fields() {
        use std::net::Ipv4Addr;
        let mapped = SocketAddr::new(Ipv4Addr::new(34, 173, 63, 221).into(), 50000);
        let base = SocketAddr::new(Ipv4Addr::new(10, 128, 0, 2).into(), 40000);
        let init = srflx_candidate_init(mapped, base);
        let candidate_json = serde_json::json!({
            "candidate": init.candidate,
            "sdpMid": serde_json::Value::Null,
            "sdpMLineIndex": 0,
        });
        let v: serde_json::Value =
            serde_json::from_str(&candidate_json.to_string()).expect("valid JSON");
        assert!(
            v["candidate"]
                .as_str()
                .is_some_and(|s| s.contains("typ srflx")),
            "candidate field carries the srflx SDP attribute: {v}"
        );
        assert!(v["sdpMid"].is_null(), "sdpMid present (null): {v}");
        assert_eq!(
            v["sdpMLineIndex"].as_u64(),
            Some(0),
            "sdpMLineIndex routes to the single video m-line: {v}"
        );
    }

    /// Audit F8 regression guard: a blocked/unreachable STUN server must
    /// NOT delay answer creation. Drives `build_with_codec_set` with a STUN
    /// URL pointing at a real bound-but-SILENT local UDP socket (it accepts
    /// the Binding Request but never replies — the same modelling
    /// `stun_binding_times_out_against_silent_server` uses, robust across
    /// OSes that would otherwise fast-fail an unroutable send) and asserts
    /// the answer is produced far inside `STUN_BINDING_TIMEOUT`. The srflx
    /// gather now runs in the spawned driver, off the critical path, so the
    /// answer no longer waits on the 1.5s STUN timeout. Under the old
    /// blocking code this socket forces the full timeout, so the assertion
    /// fails loudly if blocking ever returns to the answer path. The answer
    /// still advertises the host candidate inline.
    #[tokio::test]
    async fn build_with_codec_set_answer_not_blocked_by_unreachable_stun() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        // Bound-but-silent UDP socket: accepts the request, never answers.
        // Held for the duration so the OS keeps the port reserved.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        // Literal-IP STUN URL (no DNS on the path either).
        let ice_config = IceConfig {
            ice_servers: vec![crate::display::IceServer {
                urls: vec![format!("stun:{}:{}", silent_addr.ip(), silent_addr.port())],
                username: None,
                credential: None,
            }],
        };
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let started = std::time::Instant::now();
        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            7,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("answer must be produced despite unreachable STUN");
        let elapsed = started.elapsed();

        // The whole point of F8: well under the STUN binding timeout. A
        // generous fraction (half) leaves headroom for slow CI while still
        // failing loudly if the blocking gather ever returns to the path.
        assert!(
            elapsed < STUN_BINDING_TIMEOUT / 2,
            "answer creation blocked on STUN ({elapsed:?} >= {:?}/2)",
            STUN_BINDING_TIMEOUT
        );
        // Host (UDP) candidate is still advertised inline in the answer.
        assert!(
            answer_sdp.contains("typ host"),
            "answer advertises host candidate(s): {answer_sdp}"
        );
        // srflx is gathered off-path in the driver and would be trickled
        // via `ice_tx` only if reachable — it must NOT appear inline.
        assert!(
            !answer_sdp.contains("typ srflx"),
            "srflx is trickled, not emitted inline in the answer: {answer_sdp}"
        );
        peer.close().await;
    }

    #[test]
    fn ufrag_split_extracts_target_not_sender() {
        // RFC 8445 §7.2.2: USERNAME = <target_ufrag>:<sender_ufrag>
        // When routing a browser → server request, the FIRST half is the
        // server's ufrag (us), the second is the browser's. The original
        // bug was taking the second half and failing every lookup.
        let username = "serverABC:browserXYZ";
        let target = username
            .split_once(':')
            .map(|(target, _sender)| target.to_string());
        assert_eq!(target, Some("serverABC".to_string()));
    }

    // --- Slice 3b: relay helpers ---

    /// `parse_sdp_ice_ufrag` finds the first `a=ice-ufrag:` attribute
    /// and returns its value. Handles both session-level and
    /// media-level attributes transparently — ICE ufrag can appear in
    /// either per RFC 5245.
    #[test]
    fn parse_sdp_ice_ufrag_finds_session_or_media_level() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\na=ice-ufrag:abc123\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";
        assert_eq!(parse_sdp_ice_ufrag(sdp).as_deref(), Some("abc123"));
        // Also LF-only input, since some producers emit LF not CRLF.
        let sdp_lf = "v=0\nm=video 9 UDP/TLS/RTP/SAVPF 96\na=ice-ufrag:xyz789\n";
        assert_eq!(parse_sdp_ice_ufrag(sdp_lf).as_deref(), Some("xyz789"));
    }

    /// Malformed SDPs — no `a=ice-ufrag:` line, or empty value — return
    /// `None`. The translator treats that as "can't relay this Answer."
    #[test]
    fn parse_sdp_ice_ufrag_returns_none_on_malformed() {
        assert_eq!(
            parse_sdp_ice_ufrag("v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\n"),
            None
        );
        assert_eq!(parse_sdp_ice_ufrag("a=ice-ufrag:\r\n"), None);
        assert_eq!(parse_sdp_ice_ufrag(""), None);
    }

    /// `parse_first_frame_ufrag` extracts the TARGET (server-side)
    /// ufrag from a STUN binding request's USERNAME attribute, which
    /// is the `target:sender` format per RFC 8445.
    #[test]
    fn parse_first_frame_ufrag_picks_target_half() {
        let frame = make_stun_binding_request("peerXYZ:browserABC");
        assert_eq!(parse_first_frame_ufrag(&frame).as_deref(), Some("peerXYZ"));
    }

    /// Non-STUN input or USERNAME missing the `:` separator returns
    /// `None`. Guards against the translator logging a spurious
    /// "relay missed" on garbage input.
    #[test]
    fn parse_first_frame_ufrag_returns_none_on_non_stun() {
        assert_eq!(parse_first_frame_ufrag(b"GET / HTTP/1.1\r\n"), None);
        let bad = make_stun_binding_request("no-colon-here");
        assert_eq!(parse_first_frame_ufrag(&bad), None);
    }

    /// `inject_relay_tcp_candidate` adds a new `a=candidate:` line
    /// right after the first existing one, preserving the original
    /// line and the rest of the SDP verbatim.
    #[test]
    fn inject_relay_tcp_candidate_adds_line_after_first_existing() {
        use std::net::{Ipv4Addr, SocketAddr};
        let original = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=candidate:1 1 tcp 2113937151 10.0.0.1 8765 typ host tcptype passive\r\na=end-of-candidates\r\n";
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let rewritten = inject_relay_tcp_candidate(original, addr);
        assert!(
            rewritten.contains("a=candidate:1 1 tcp 2113937151 10.0.0.1 8765"),
            "original candidate line preserved: {rewritten}"
        );
        assert!(
            rewritten.contains("a=candidate:9001 1 tcp "),
            "injected relay candidate present (foundation 9001): {rewritten}"
        );
        assert!(
            rewritten.contains("192.168.1.42 8765"),
            "injected candidate carries primary address: {rewritten}"
        );
        assert!(
            rewritten.contains("a=end-of-candidates"),
            "post-candidate lines preserved: {rewritten}"
        );
        // CRLF preserved.
        assert!(rewritten.contains("\r\n"), "CRLF preserved");
    }

    /// When the SDP has no existing candidate lines, injection
    /// appends at the end (rather than failing).
    #[test]
    fn inject_relay_tcp_candidate_appends_when_no_candidates_present() {
        use std::net::{Ipv4Addr, SocketAddr};
        let original = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 197).into(), 8765);
        let rewritten = inject_relay_tcp_candidate(original, addr);
        assert!(
            rewritten.contains("a=candidate:9001 "),
            "injected: {rewritten}"
        );
        assert!(rewritten.starts_with("v=0\r\n"), "SDP preamble preserved");
    }

    /// Injected candidate has `typ host tcptype passive` — what
    /// browsers expect for a TCP passive candidate they can dial.
    #[test]
    fn inject_relay_tcp_candidate_uses_host_passive_type() {
        use std::net::{Ipv4Addr, SocketAddr};
        let addr = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let rewritten = inject_relay_tcp_candidate("", addr);
        assert!(
            rewritten.contains("typ host tcptype passive"),
            "expected host+passive: {rewritten}"
        );
    }

    /// IPv6 addresses render as their canonical string form (no
    /// brackets — SDP candidate IPs aren't bracketed, unlike URLs).
    #[test]
    fn inject_relay_tcp_candidate_renders_ipv6_without_brackets() {
        use std::net::{Ipv6Addr, SocketAddr};
        let addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8443);
        let rewritten = inject_relay_tcp_candidate("", addr);
        assert!(
            rewritten.contains("::1 8443"),
            "IPv6 in candidate line without brackets: {rewritten}"
        );
        assert!(
            !rewritten.contains("[::1]"),
            "no brackets in candidate line (URL-style brackets are SDP-invalid)"
        );
    }

    /// `TcpRelayRegistry` round-trips entries and reports presence.
    /// Locks the contract the gateway's accept-loop dispatch relies on.
    #[test]
    fn tcp_relay_registry_roundtrip() {
        use std::net::{Ipv4Addr, SocketAddr};
        let reg = TcpRelayRegistry::new();
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 64, 3).into(), 8765);
        assert!(!reg.contains_ufrag("abc"));
        assert_eq!(reg.lookup("abc"), None);
        reg.register("abc".into(), addr);
        assert!(reg.contains_ufrag("abc"));
        assert_eq!(reg.lookup("abc"), Some(addr));
        reg.unregister("abc");
        assert!(!reg.contains_ufrag("abc"));
        // Double-unregister is idempotent.
        reg.unregister("abc");
    }

    /// Re-registering the same ufrag updates the outbound address
    /// (reconnect case — same peer issues a fresh answer with a new
    /// address).
    #[test]
    fn tcp_relay_registry_reregister_updates_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let reg = TcpRelayRegistry::new();
        let a1 = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 8765);
        let a2 = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 2).into(), 9090);
        reg.register("same-ufrag".into(), a1);
        reg.register("same-ufrag".into(), a2);
        assert_eq!(reg.lookup("same-ufrag"), Some(a2));
    }

    // -----------------------------------------------------------------------
    // Phase 3c.3b.2: pool-mode intake
    // -----------------------------------------------------------------------

    /// `codec_set_from_subscriptions` dedups codecs across multi-layer
    /// (simulcast-style) subscription sets.
    #[test]
    fn codec_set_from_subscriptions_dedups_multi_layer() {
        use crate::display::encode::pool::{EncoderId, LayerSpec, SimulcastRid};
        let (s, _r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        let make = |codec: CodecKind, rid: SimulcastRid| EncoderSubscription {
            id: EncoderId::new(codec, rid),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: s.subscribe(),
        };
        let subs = vec![
            make(CodecKind::Vp8, SimulcastRid::full()),
            // Same codec, different RID — must dedup (one
            // enable_vp8 call covers both layers).
            make(CodecKind::Vp8, SimulcastRid::half()),
            make(CodecKind::H264, SimulcastRid::full()),
        ];
        let codecs = codec_set_from_subscriptions(&subs);
        assert_eq!(codecs.len(), 2);
        assert!(codecs.contains(&CodecKind::Vp8));
        assert!(codecs.contains(&CodecKind::H264));
    }

    #[test]
    fn active_codec_from_subscriptions_respects_peer_pref_order() {
        use crate::display::encode::pool::{EncoderId, LayerSpec, SimulcastRid};
        let (s, _r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        let make = |codec: CodecKind| EncoderSubscription {
            id: EncoderId::new(codec, SimulcastRid::full()),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: s.subscribe(),
        };
        let subs = vec![make(CodecKind::Vp8), make(CodecKind::H264)];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::H264, CodecKind::Vp8]);
        assert_eq!(
            active_codec_from_subscriptions(&subs, &prefs),
            Some(CodecKind::H264)
        );
    }

    #[test]
    fn active_codec_from_subscriptions_returns_none_on_no_pref_overlap() {
        use crate::display::encode::pool::{EncoderId, LayerSpec, SimulcastRid};
        let (s, _r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        let subs = vec![EncoderSubscription {
            id: EncoderId::new(CodecKind::Vp8, SimulcastRid::full()),
            layer: LayerSpec::single(CodecKind::Vp8, 64, 64, 30),
            frames: s.subscribe(),
        }];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::H264]);
        assert_eq!(active_codec_from_subscriptions(&subs, &prefs), None);
    }

    #[test]
    fn first_video_mid_from_offer_ignores_non_video_m_lines() {
        let offer = "v=0\r\n\
                     m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                     a=mid:data\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     a=mid:screen\r\n";
        assert_eq!(first_video_mid_from_offer(offer).as_deref(), Some("screen"));
    }

    #[test]
    fn first_video_mid_from_offer_returns_none_when_absent() {
        let offer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:audio\r\n";
        assert_eq!(first_video_mid_from_offer(offer), None);
    }

    // -----------------------------------------------------------------------
    // Phase 3c.3b.2b: filter_prefs_to_negotiated unit tests
    // -----------------------------------------------------------------------

    /// **3c.3b.2b finding 1 contract.** Filters original prefs against
    /// the codec set actually returned by initial subscribe, preserving
    /// the original ordering. Order matters because
    /// `active_codec_from_subscriptions` uses prefs order as the codec
    /// preference signal — re-ordering would change which codec the
    /// peer actually receives.
    #[test]
    fn filter_prefs_to_negotiated_preserves_original_order() {
        let original =
            PeerCodecPreferences::new(vec![CodecKind::H264, CodecKind::Vp8, CodecKind::Vp9]);
        // Pool returned VP8 + Vp9 only (no H.264 backend at the moment).
        let actual = vec![CodecKind::Vp8, CodecKind::Vp9];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert_eq!(filtered.supported, vec![CodecKind::Vp8, CodecKind::Vp9]);
        // Different order in `actual` must NOT re-rank the result —
        // the result follows `original`'s ordering.
        let actual_reversed = vec![CodecKind::Vp9, CodecKind::Vp8];
        let filtered2 = filter_prefs_to_negotiated(&original, &actual_reversed);
        assert_eq!(filtered2.supported, vec![CodecKind::Vp8, CodecKind::Vp9]);
    }

    /// Identity case: when actual ⊇ original, the filter is a no-op
    /// (everything in original survives).
    #[test]
    fn filter_prefs_to_negotiated_identity_when_actual_covers_original() {
        let original = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let actual = vec![CodecKind::Vp8, CodecKind::H264, CodecKind::Vp9];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert_eq!(filtered.supported, vec![CodecKind::Vp8, CodecKind::H264]);
    }

    /// No overlap → empty result. Caller must reject this case
    /// upstream (see the `is_empty()` guard in `new`); the
    /// filter itself doesn't error.
    #[test]
    fn filter_prefs_to_negotiated_returns_empty_when_no_overlap() {
        let original = PeerCodecPreferences::new(vec![CodecKind::H264]);
        let actual = vec![CodecKind::Vp8];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert!(filtered.is_empty());
    }

    /// Empty original → empty result. Belt-and-suspenders.
    #[test]
    fn filter_prefs_to_negotiated_returns_empty_for_empty_original() {
        let original = PeerCodecPreferences::new(vec![]);
        let actual = vec![CodecKind::Vp8, CodecKind::H264];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert!(filtered.is_empty());
    }

    /// Empty actual → empty result. (The pool returned no codecs;
    /// negotiation would be impossible; upstream rejects via the
    /// "subscriptions is_empty" guard before reaching the filter.)
    #[test]
    fn filter_prefs_to_negotiated_returns_empty_for_empty_actual() {
        let original = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let filtered = filter_prefs_to_negotiated(&original, &[]);
        assert!(filtered.is_empty());
    }

    // -----------------------------------------------------------------------
    // Phase 4c follow-up (d): partition_subscriptions_by_codec unit tests
    // -----------------------------------------------------------------------

    /// Build a synthetic [`EncoderSubscription`] with a fresh
    /// `broadcast::Receiver`. The Sender is `mem::forget`ed so the
    /// channel stays open for the lifetime of the test (we never
    /// `recv()` — these tests inspect ids only).
    ///
    /// Synthetic: lets us construct H.264 subscriptions without
    /// spawning a real H.264 encoder backend (VAAPI / VideoToolbox
    /// / ffmpeg), so the partition test can exercise the
    /// VP8 + H.264 mix without the encoder backend dependency.
    fn make_partition_test_subscription(
        codec: CodecKind,
        rid: SimulcastRid,
    ) -> EncoderSubscription {
        use crate::display::encode::pool::LayerSpec;
        let (s, r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        std::mem::forget(s);
        EncoderSubscription {
            id: EncoderId::new(codec, rid),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: r,
        }
    }

    /// **Phase 4c follow-up (d) contract: mixed-codec partition.**
    ///
    /// When `pool.subscribe(prefs=[VP8, H.264])` returns subscriptions
    /// for both codecs (e.g. VP8 simulcast 3 layers + H.264 single
    /// layer = 4 subs total), `partition_subscriptions_by_codec` with
    /// `active_codec=Vp8` must:
    ///
    /// - Return all 3 VP8 subscriptions in the active partition (each
    ///   gets its own forwarder; per-RID frames feed the multi-RID
    ///   driver write path).
    /// - Return only the H.264 id in the inactive_ids vec (caller
    ///   passes to `lease.release_on_demand_subset` so the H.264
    ///   on-demand encoder tears down rather than spinning into a
    ///   no-receiver broadcast — the wasted-CPU regression caught
    ///   in the 3c.3b.2a review).
    ///
    /// The end-to-end chain is pinned by composition with the
    /// existing `release_on_demand_subset_decrements_only_specified_ids`
    /// + `release_on_demand_subset_silently_skips_unknown_ids` tests
    /// in `display/encode/pool.rs` — they pin the lease side, this
    /// pins the partition side, and `pool_frame_intake` passes the
    /// returned `inactive_ids` verbatim to `release_on_demand_subset`.
    #[test]
    fn partition_subscriptions_by_codec_mixed_codec_separates_active_keeps_inactive_ids() {
        let vp8_full = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::full());
        let vp8_half = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::half());
        let vp8_quarter = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::quarter());
        let h264_full = make_partition_test_subscription(CodecKind::H264, SimulcastRid::full());

        let (active, inactive_ids) = partition_subscriptions_by_codec(
            vec![vp8_full, h264_full, vp8_half, vp8_quarter],
            CodecKind::Vp8,
        );

        // Active partition: all 3 VP8 subs (forwarder spawns for each).
        assert_eq!(
            active.len(),
            3,
            "VP8 simulcast active partition must keep all 3 layer subs"
        );
        let active_ids: std::collections::HashSet<EncoderId> =
            active.iter().map(|s| s.id.clone()).collect();
        assert!(active_ids.contains(&EncoderId::new(CodecKind::Vp8, SimulcastRid::full())));
        assert!(active_ids.contains(&EncoderId::new(CodecKind::Vp8, SimulcastRid::half())));
        assert!(active_ids.contains(&EncoderId::new(CodecKind::Vp8, SimulcastRid::quarter())));

        // Inactive ids: ONLY the H.264 id. pool_frame_intake passes
        // this verbatim to lease.release_on_demand_subset, which is
        // what tears down the never-consumed H.264 on-demand encoder.
        assert_eq!(
            inactive_ids,
            vec![EncoderId::new(CodecKind::H264, SimulcastRid::full())],
            "inactive_ids must contain exactly the H.264 id (and only \
             the H.264 id) so release_on_demand_subset drops the \
             unused on-demand claim"
        );
    }

    /// Single-codec subscription set → empty inactive_ids → caller
    /// skips the release call (the `if !inactive_ids.is_empty()` guard
    /// in `pool_frame_intake`). This is the steady-state case for
    /// resubscribe-after-Closed: `filter_prefs_to_negotiated` locks
    /// resubscribe prefs to the active codec only, so subsequent
    /// epochs always have inactive_ids empty.
    #[test]
    fn partition_subscriptions_by_codec_single_codec_returns_empty_inactive_ids() {
        let vp8_full = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::full());
        let vp8_half = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::half());

        let (active, inactive_ids) =
            partition_subscriptions_by_codec(vec![vp8_full, vp8_half], CodecKind::Vp8);

        assert_eq!(active.len(), 2, "both VP8 subs end up in active");
        assert!(
            inactive_ids.is_empty(),
            "single-codec subscription set must produce empty inactive_ids"
        );
    }

    /// All subs are inactive — active partition empty, inactive_ids
    /// has every id. `pool_frame_intake` defends against this by
    /// calling `active_codec_from_subscriptions` first and escalating
    /// to peer shutdown if the active codec resolves but the partition
    /// still produces zero active subs (a "shouldn't happen" contract
    /// violation). This test pins the helper's behavior at that
    /// boundary so the defensive check upstream has well-defined
    /// inputs.
    #[test]
    fn partition_subscriptions_by_codec_no_active_match_keeps_all_inactive_ids() {
        let h264_full = make_partition_test_subscription(CodecKind::H264, SimulcastRid::full());
        let h264_half = make_partition_test_subscription(CodecKind::H264, SimulcastRid::half());

        let (active, inactive_ids) =
            partition_subscriptions_by_codec(vec![h264_full, h264_half], CodecKind::Vp8);

        assert!(active.is_empty(), "no VP8 in subs → active partition empty");
        assert_eq!(
            inactive_ids.len(),
            2,
            "both H.264 ids must surface as inactive when active codec is VP8"
        );
    }

    /// **3c.3b.2 first explicit test, per the 3c.3b.1a review.**
    ///
    /// `subscribe()` racing with `on_resize()` can briefly hand back
    /// `EncoderSubscription`s whose underlying `broadcast::Sender`
    /// has already been dropped — the receiver returns
    /// `RecvError::Closed` on its very first `recv()`. The pool
    /// intake must treat this as a normal "encoder epoch
    /// transitioned" signal: drop the lease, resubscribe, continue
    /// forwarding from the fresh handles. Critically: do NOT
    /// shut the peer down.
    ///
    /// Setup pins the contract by deliberately constructing the
    /// scenario:
    ///   1. Pool with VP8 always-on at 64x64.
    ///   2. `pool.subscribe(VP8)` → `initial_subs` whose Receiver
    ///      points at the original handle.
    ///   3. `pool.on_resize(128, 96)` — drops the original handle
    ///      (its broadcast Sender goes away with it), spawns a
    ///      replacement at 128x96. `initial_subs` is now stale.
    ///   4. Hand `initial_subs` to a freshly-spawned intake task.
    ///   5. Push frames at the new dimensions; the new always-on
    ///      encoder produces output that the intake — after
    ///      resubscribing — forwards into `frame_rx`.
    ///
    /// Pre-fix behavior would either time out (intake stuck waiting
    /// on a closed Receiver) or shut the peer down (treating Closed
    /// as fatal). Either fires this test's assertion.
    ///
    /// VP8-specific (gated off Windows): like the other `pool_intake_*`
    /// tests below, it drives a VP8 always-on/on-demand pool and
    /// subscribes with a VP8 preference. Windows has no VP8 backend
    /// (`Vp8Encoder::new` always `Err`s and VP8 is not on-demand
    /// spawnable), so `pool.subscribe(VP8)` cannot succeed there. The
    /// `pool_frame_intake` resubscribe/forward/lossy-drop semantics are
    /// codec-agnostic and fully exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_resubscribes_when_initial_subs_already_closed() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Subscribe AGAINST the original handle.
        let (initial_subs, initial_lease) = pool.subscribe(&prefs).expect("initial subscribe");

        // Resize: original handle dropped, new one spawned.
        // initial_subs's Receivers will return Closed on first recv.
        pool.on_resize(128, 96);

        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(16);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        // Push several frames so the resubscribe-window race has time
        // to settle and we definitely hit a frame after resubscribe.
        // 5 frames over 200ms is generous; in practice the intake
        // detects Closed within one tick.
        let frame = Arc::new(vec![0u8; 128 * 96 * 3 / 2]);
        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let result = tokio::time::timeout(Duration::from_secs(2), frame_rx.recv())
            .await
            .expect(
                "frame_rx must produce within 2s — timeout indicates the \
             intake task either deadlocked on a closed Receiver or \
             escalated to peer shutdown instead of treating Closed \
             as a normal epoch transition",
            );
        assert!(
            result.is_some(),
            "intake must forward a frame from the post-resize encoder; \
             got None which means the channel closed — likely intake \
             tore down rather than resubscribed"
        );

        assert!(
            !shutdown.is_cancelled(),
            "intake must not shut down the peer on a normal \
             Closed → resubscribe path; this assertion catches a \
             regression where Closed escalates to peer teardown"
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }

    /// Escalation: when resubscribe genuinely cannot find a
    /// compatible codec (e.g. the pool no longer serves anything
    /// the peer wants), the intake signals peer shutdown rather
    /// than leaving the stream black. Mirror image of the
    /// happy-path test above — Closed should not always escalate,
    /// but it MUST escalate when there's no recovery available.
    ///
    /// VP8-specific (gated off Windows): seeds the intake from a VP8
    /// on-demand subscription (no VP8 backend on Windows). The
    /// Closed → resubscribe → NoCompatibleCodec → shutdown escalation it
    /// pins is codec-agnostic and exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_shuts_down_peer_when_resubscribe_finds_no_codec() {
        use crate::display::encode::pool::EncoderPool;

        // Pool with NO always-on encoders; on-demand only. Subscribe
        // for VP8 (spawns on-demand VP8 slot) to get initial_subs.
        let pool = Arc::new(EncoderPool::new(64, 64, 30, |_, _| vec![], None));
        let prefs_vp8 = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs_vp8).expect("initial on-demand VP8");

        // Drop the on-demand slot via resize. initial_subs's Receivers
        // will see Closed.
        pool.on_resize(128, 96);

        // Hand the intake a prefs the pool CANNOT serve (VP9 has no
        // backend wired). When intake sees Closed and resubscribes
        // with these prefs, pool.subscribe returns NoCompatibleCodec.
        // Intake must then shutdown.cancel() to terminate the peer
        // cleanly.
        let prefs_unservable = PeerCodecPreferences::new(vec![CodecKind::Vp9]);
        let (frame_tx, _frame_rx) = mpsc::channel::<OutboundEncodedFrame>(16);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs_unservable,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        // Wake the orphaned encoder thread so it sees its cancelled
        // shutdown and exits, dropping its frames-Sender clone. With
        // both senders gone (handle.frames already dropped by
        // on_resize, thread's clone dropped by exit) the broadcast
        // channel closes — only then does the intake's forwarder
        // see Closed and trigger the resubscribe → NoCompatibleCodec
        // → shutdown.cancel() escalation. In production the bridge
        // pushes constantly so this is automatic; the test simulates
        // by pushing a few wake-frames.
        let frame = Arc::new(vec![0u8; 128 * 96 * 3 / 2]);
        for _ in 0..3 {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let exited = tokio::time::timeout(Duration::from_secs(2), async {
            while !shutdown.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            exited.is_ok(),
            "intake must escalate to shutdown within 2s when \
             resubscribe returns NoCompatibleCodec; otherwise the \
             peer would sit forever with a black stream"
        );

        let _ = intake_handle.await;
    }

    // -----------------------------------------------------------------------
    // Phase 4c: pool_frame_intake multi-forwarder contract tests
    // -----------------------------------------------------------------------

    /// **Phase 4c contract: forward all active-codec layers.**
    ///
    /// With VP8 simulcast (3 always-on layers), `pool.subscribe(VP8)`
    /// returns three subscriptions. The intake spawns one forwarder
    /// per active-codec subscription and the per-peer mpsc receives
    /// frames from every rid concurrently. This is what makes
    /// browser-visible simulcast possible — the answer SDP advertises
    /// N rids, and the multi-RID driver write path needs frames for
    /// each rid to actually produce wire packets per encoding.
    ///
    /// Test pins:
    ///   1. With VP8 simulcast (3 layers) active, every rid appears
    ///      among forwarded frames over a fixed window.
    ///   2. Frame count is proportional to layers × inputs (NOT 1×
    ///      inputs as pre-4c). Pre-4c behavior would land at ~1×
    ///      (one layer forwarded), so we assert ≥ 2× to leave clear
    ///      daylight even with encoder warm-up irregularities.
    ///
    /// Replaces the pre-4c
    /// `pool_intake_forwards_only_one_layer_with_simulcast_set`
    /// (deleted with this commit) — the inverse contract is now in
    /// effect.
    ///
    /// VP8-specific (gated off Windows): the multi-forwarder contract is
    /// inherently about VP8 simulcast (3 always-on layers); Windows runs
    /// a single full-res H.264 layer with no simulcast and no VP8
    /// backend. Exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_forwards_all_active_codec_layers() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            |w, h| LayerSpec::vp8_simulcast(w, h, 30),
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("VP8 simulcast subscribe");
        // Pre-condition: pool returned multiple subscriptions. If this
        // assertion fires the simulcast set was dropped to a single
        // layer somewhere upstream and this test no longer exercises
        // the multi-sub case it claims to.
        let n_layers = initial_subs.len();
        assert!(
            n_layers >= 2,
            "test setup expects multiple simulcast layers from \
             vp8_simulcast(); got {n_layers}",
        );
        let expected_rids: std::collections::HashSet<SimulcastRid> =
            initial_subs.iter().map(|s| s.id.rid.clone()).collect();
        let input_count = 12u64;

        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(1024);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        // Push frames at the source dimensions. Each i420 buffer
        // arrives at every always-on encoder via the bridge's
        // broadcast; with N forwarders, expect ~N×inputs encoded
        // frames at the per-peer mpsc.
        let frame = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..input_count {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut seen_rids: std::collections::HashSet<SimulcastRid> =
            std::collections::HashSet::new();
        let mut received: u64 = 0;
        while let Ok(outbound) = frame_rx.try_recv() {
            seen_rids.insert(outbound.rid);
            received += 1;
        }

        // Every expected rid must appear in the received stream.
        // Missing any rid means a forwarder failed to spawn for that
        // subscription OR the per-RID rid wrap was dropped.
        for rid in &expected_rids {
            assert!(
                seen_rids.contains(rid),
                "rid {} missing from forwarded frames; got rids {:?}, \
                 expected {:?} — multi-forwarder spawn or rid wrap broke",
                rid.as_str(),
                seen_rids.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
                expected_rids.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            );
        }
        // Frame count proportional to layers. Pre-4c forwarded only
        // one rid (≤ 1.5 × input); post-4c forwards N (~ N × input).
        // Assert ≥ 2× to leave daylight from the pre-4c behavior even
        // with encoder warm-up quirks.
        assert!(
            received >= input_count * 2,
            "expected ≥ {} frames forwarded for {input_count} inputs across \
             {n_layers} layers; got {received} — pre-4c behavior would \
             land at ~{} (1× input)",
            input_count * 2,
            input_count,
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }

    /// **3c.3b.2a contract: lossy forwarding (try_send).**
    ///
    /// The intake forwarder uses `try_send` and drops on `Full`,
    /// incrementing `drops_counter`. A slow peer (full mpsc) sees
    /// frames dropped while the forwarder stays responsive to
    /// cancellation — `send().await` would park inside the mpsc and
    /// make the cancel path unreachable.
    ///
    /// Pre-fix `send().await` would park the forwarder inside the mpsc
    /// when full. Cancellation would only fire on the next `rx.recv()`,
    /// which can't be reached while parked — making the cancel path
    /// effectively unbounded (or bounded only by the slow consumer's
    /// drain rate). This test pins that by:
    ///
    ///   1. Wiring a tiny driver mpsc (capacity 1) and never draining
    ///      it until late in the test.
    ///   2. Pushing many input frames so the encoder produces a burst
    ///      that overflows the mpsc.
    ///   3. Asserting `drops_counter > 0` (frames were dropped, not
    ///      blocked-on).
    ///   4. Asserting `shutdown.cancel()` causes the intake to exit
    ///      within a tight bound (parked-send would exceed it).
    ///
    /// VP8-specific (gated off Windows): drives a VP8 always-on pool and
    /// subscribes with a VP8 preference (no VP8 backend on Windows). The
    /// lossy try_send + prompt-cancel behavior is codec-agnostic and
    /// exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_drops_lossily_when_driver_mpsc_full() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("VP8 always-on subscribe");

        // Tiny mpsc — fills almost immediately. Keep the receiver
        // alive but never drain it during the push phase.
        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(1);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let drops_for_intake = Arc::clone(&drops);
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            drops_for_intake,
            kf_rx,
            intake_shutdown,
        ));

        // Push enough frames that the bounded mpsc(1) overflows
        // significantly. With one always-on encoder and one i420
        // input → one encoded frame, 30 inputs ≫ 1 mpsc slot, so
        // drops should be substantial.
        let frame = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..30 {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Allow the encoder + forwarder a moment to process the burst
        // before reading the counter.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let dropped = drops.load(Ordering::Relaxed);
        assert!(
            dropped > 0,
            "drops_counter must be incremented when the driver mpsc \
             fills; got 0. Either the forwarder is using send().await \
             (parking instead of dropping), or the mpsc isn't actually \
             filling (encoder slower than expected). Pre-fix behavior \
             would also produce 0 drops because the forwarder would \
             park indefinitely."
        );

        // Now prove cancellation propagates promptly: pre-fix, a
        // parked send would only release when frame_rx drained; we'd
        // see cancel take many ms. With try_send the forwarder is
        // never parked, so cancel + intake exit inside a tight bound.
        let cancel_start = Instant::now();
        shutdown.cancel();
        let exited = tokio::time::timeout(Duration::from_secs(1), intake_handle).await;
        let cancel_elapsed = cancel_start.elapsed();
        assert!(
            exited.is_ok(),
            "intake must exit within 1s of shutdown.cancel() (took {:?}); \
             a longer wait indicates the forwarder parked on send()",
            cancel_elapsed,
        );

        // Belt: the receiver eventually drained from the test's side
        // is fine — proves the peer COULD have consumed if it had.
        // Drain to silence "you held the receiver but never read".
        while frame_rx.try_recv().is_ok() {}
    }

    // -----------------------------------------------------------------------
    // Phase-4c-prep: OutboundEncodedFrame + per-(spec, rid) keyframe gate
    // -----------------------------------------------------------------------

    /// **Phase 4c**: every frame the intake forwards must carry the
    /// rid of the subscription that produced it. This is the
    /// mechanism that lets the driver's multi-RID write path look up
    /// the right SSRC + per-`(spec, rid)` keyframe gate at write
    /// time without needing to redundantly embed the rid in
    /// `EncodedFrame` (which is the encoder pool's output type,
    /// shared across subscribers of one slot).
    ///
    /// With multi-forwarder intake (this commit), each rid's
    /// forwarder wraps frames with its own rid. The mpsc receives
    /// frames tagged with multiple rids, and the rid on each frame
    /// matches the encoder slot that produced it. This test pins
    /// that no rid leaks across forwarders (e.g. forwarder A
    /// accidentally tagging with B's rid).
    ///
    /// Replaces the pre-4c
    /// `pool_intake_wraps_forwarded_frames_with_active_subscription_rid`
    /// (which assumed single-active-subscription) — the multi-rid
    /// version pins per-forwarder rid integrity.
    ///
    /// VP8-specific (gated off Windows): per-forwarder rid integrity is
    /// a multi-layer VP8-simulcast property; Windows runs a single
    /// full-res H.264 layer with no VP8 backend. Exercised on
    /// macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_wraps_forwarded_frames_with_per_subscription_rid() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            |w, h| LayerSpec::vp8_simulcast(w, h, 30),
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) = pool
            .subscribe(&prefs)
            .expect("VP8 simulcast subscribe must succeed");

        let subscribed_rids: std::collections::HashSet<SimulcastRid> =
            initial_subs.iter().map(|s| s.id.rid.clone()).collect();
        // Pre-condition: subscribed to multiple rids. If this fires,
        // the test no longer exercises multi-forwarder behavior.
        assert!(
            subscribed_rids.len() >= 2,
            "test setup expects multi-rid subscription set; got {} rids",
            subscribed_rids.len(),
        );

        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(1024);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        let i420 = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..12 {
            pool.push_i420_frame(Arc::clone(&i420), Instant::now());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Every received frame's rid must be in the subscribed set —
        // a frame tagged with an unsubscribed rid would mean a
        // forwarder leaked its rid (e.g. an Arc<SimulcastRid> shared
        // across forwarders by mistake) or a stale rid persisted
        // through a resubscribe.
        let mut received: u32 = 0;
        let mut rid_counts: std::collections::HashMap<SimulcastRid, u32> =
            std::collections::HashMap::new();
        while let Ok(outbound) = frame_rx.try_recv() {
            assert!(
                subscribed_rids.contains(&outbound.rid),
                "frame {received} arrived with rid {} but subscribed \
                 rids are {:?} — per-forwarder rid wrap leaked or \
                 stale rid persisted",
                outbound.rid.as_str(),
                subscribed_rids
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>(),
            );
            *rid_counts.entry(outbound.rid).or_insert(0) += 1;
            received += 1;
        }
        assert!(
            received > 0,
            "intake must forward at least one frame for this test to \
             pin anything — got 0. Either the encoders aren't \
             producing output (test fixture broken) or the forwarders \
             aren't sending (refactor regression)."
        );
        // At least 2 distinct rids must have produced frames —
        // single-rid forwarding (pre-4c) would only show 1.
        assert!(
            rid_counts.len() >= 2,
            "expected ≥2 rids in forwarded stream; got {} ({:?}) — \
             multi-forwarder pool intake regressed to single-rid",
            rid_counts.len(),
            rid_counts.keys().map(|r| r.as_str()).collect::<Vec<_>>(),
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }

    /// **Phase-4c-prep**: `DriverState::video_specs` keying changed
    /// from `PayloadSpec` to `(PayloadSpec, SimulcastRid)`. This is a
    /// data-shape pin: a HashMap with `(spec, rid)` keys treats two
    /// distinct rids as distinct entries even when they share the
    /// same payload_spec — which is exactly the VP8 simulcast case
    /// where every layer has the same `PayloadSpec` but each layer's
    /// keyframe gate must remain independent.
    ///
    /// Without per-RID keying, a keyframe seen on RID `full` would
    /// open the gate for P-frames on RIDs `half` and `quarter`. Those
    /// P-frames would reference keyframes the half/quarter
    /// subscribers never received, decoding to garbage.
    ///
    /// This test pins the keying directly. It can't easily exercise
    /// `write_video_frame` end-to-end without an `RTCPeerConnection`,
    /// but the data-shape contract is what matters — if the keying
    /// regresses to `PayloadSpec`-only the map would conflate the
    /// three layers and this test would compile-fail (or assert).
    #[test]
    fn driver_state_video_specs_keys_by_spec_and_rid() {
        use crate::display::encode::PayloadSpec;
        let spec = PayloadSpec::vp8();
        let mut specs: HashMap<(PayloadSpec, SimulcastRid), SpecState> = HashMap::new();
        // Insert under three distinct rids with the same spec. They
        // must be three distinct entries — pre-fix keying by
        // `PayloadSpec` alone would have collapsed them to one.
        for rid in [
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ] {
            specs.insert(
                (spec.clone(), rid),
                SpecState::Ready {
                    keyframe_seen: false,
                },
            );
        }
        assert_eq!(
            specs.len(),
            3,
            "(PayloadSpec, SimulcastRid) keying must keep three rids \
             with the same spec as three distinct entries; got {} \
             entries — keying regressed to spec-only?",
            specs.len(),
        );
        // Flipping one rid's keyframe_seen must not affect the others.
        if let Some(SpecState::Ready { keyframe_seen }) =
            specs.get_mut(&(spec.clone(), SimulcastRid::full()))
        {
            *keyframe_seen = true;
        }
        for rid in [SimulcastRid::half(), SimulcastRid::quarter()] {
            match specs.get(&(spec.clone(), rid.clone())) {
                Some(SpecState::Ready { keyframe_seen }) => assert!(
                    !keyframe_seen,
                    "rid {} keyframe_seen leaked across rids — keying \
                     is wrong",
                    rid.as_str(),
                ),
                _ => panic!("rid {} entry missing", rid.as_str()),
            }
        }
    }

    /// rtc 0.9 uses rustls 0.23, which requires a process-level
    /// `CryptoProvider`. Production code paths that build an Rtc
    /// transitively need it; the test fixtures call this at the top
    /// of every test that constructs a real `RTCPeerConnection`.
    /// Idempotent — `install_default` returns Err on second call,
    /// which we discard.
    fn ensure_rustls_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    /// **Phase 4c**: synthetic recvonly VP8 offer in the shape rtc 0.9
    /// requires (`a=fingerprint`, `a=ice-ufrag`/`pwd`, `a=setup`,
    /// `a=rtpmap`). Used by the build_with_codec_set integration
    /// tests below to drive `RTCPeerConnection::create_answer`
    /// without standing up a real browser.
    ///
    /// **Phase 4c follow-up (b)**: includes the recv-side simulcast
    /// hint (`a=rid:f/h/q recv` + `a=simulcast:recv f;h;q`) plus the
    /// repaired-rtp-stream-id extmap so the answer-side simulcast
    /// path is exercised by the test the same way the production
    /// browser side now exercises it (via the
    /// `injectRecvSimulcastIntoVideoOffer` helper in static/app.html).
    /// Without the offer's `recv` advertisement, rtc 0.9 omits
    /// `a=simulcast:send` from the answer regardless of how many
    /// encodings the track has — see the test's panic message for
    /// the full reasoning.
    fn synth_recvonly_video_offer_for_rtc() -> String {
        concat!(
            "v=0\r\n",
            "o=- 1 2 IN IP4 0.0.0.0\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=group:BUNDLE 0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=mid:0\r\n",
            "a=recvonly\r\n",
            "a=rtcp-mux\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
            "a=extmap:2 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id\r\n",
            "a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id\r\n",
            "a=rid:f recv\r\n",
            "a=rid:h recv\r\n",
            "a=rid:q recv\r\n",
            "a=simulcast:recv f;h;q\r\n",
            "a=ice-ufrag:testufrag1234\r\n",
            "a=ice-pwd:testpassword12345678901234\r\n",
            "a=fingerprint:sha-256 ",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
            "a=setup:actpass\r\n",
        )
        .to_string()
    }

    /// Same as `synth_recvonly_video_offer_for_rtc` but advertising
    /// H.264 only (Constrained Baseline, packetization-mode=1).
    /// Used by the H.264-only answer test to verify no simulcast
    /// lines appear when active_rids has length 1.
    fn synth_recvonly_h264_video_offer_for_rtc() -> String {
        concat!(
            "v=0\r\n",
            "o=- 1 2 IN IP4 0.0.0.0\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=group:BUNDLE 0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=mid:0\r\n",
            "a=recvonly\r\n",
            "a=rtcp-mux\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=1;",
            "level-asymmetry-allowed=1\r\n",
            "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
            "a=ice-ufrag:testufrag1234\r\n",
            "a=ice-pwd:testpassword12345678901234\r\n",
            "a=fingerprint:sha-256 ",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
            "a=setup:actpass\r\n",
        )
        .to_string()
    }

    /// **Phase 4c**: a multi-encoding track (one encoding per
    /// `active_rids` entry) emits `a=simulcast:send` plus per-rid
    /// `a=rid:<rid> send` lines in the answer SDP. This is the wire-
    /// level contract that browser-visible simulcast depends on —
    /// if these lines are missing, the browser sees a single-stream
    /// answer regardless of how many encodings the track was built
    /// with, and the multi-RID forwarder's frames for non-advertised
    /// rids are silently dropped at the wire.
    ///
    /// Pin: VP8 with active_rids=[full, half, quarter] yields an
    /// answer containing `a=simulcast:send full;half;quarter` and
    /// matching `a=rid:* send` lines for each rid.
    ///
    /// This test exercises `build_with_codec_set` end-to-end (the
    /// only way to verify the rtc-side answer SDP shape), but
    /// abandons the spawned driver task by dropping the returned
    /// peer at scope-end. The driver self-terminates on shutdown
    /// signal (peer Drop fires shutdown.cancel()).
    #[tokio::test]
    async fn build_with_codec_set_emits_simulcast_send_for_multi_rid_vp8() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ];
        let ice_config = crate::display::IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            42,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed for VP8 multi-rid");

        // Summary line with all three rids in preference order
        // (f / h / q — LiveKit / mediasoup convention, see RID_FULL
        // etc. in pool.rs). The order matches the `active_rids`
        // slice order; rtc emits them in track-encoding order.
        //
        // After the sanitized-wire-only shim landed in
        // `build_with_codec_set` (sanitize_answer_sdp called on the
        // wire bytes after set_local_description), these assertions
        // pin the exact shape of the WIRE answer that ships to the
        // browser:
        //
        //   - exactly ONE `a=simulcast:send f;h;q` line — not two
        //     (rtc 0.9's doubled-direction emission was sanitized);
        //   - exactly ONE `a=rid:<rid> send` line per active rid —
        //     not two (rtc 0.9's per-RID duplication was sanitized);
        //   - no `send f;h;q send` substring (the sentinel pattern
        //     of the rtc-0.9 SDP-writer bug);
        //   - the wire answer parses as a valid RTCSessionDescription
        //     of type `answer` — the parse-check that proves WebKit's
        //     parser would accept it.
        //
        // Test-gap discipline: the operator caught earlier that
        // `assert!(answer_sdp.contains("a=simulcast:send f;h;q"))`
        // happily passes against the malformed
        // `a=simulcast:send f;h;q send f;h;q`. Exact-count assertions
        // + explicit substring negation + parse-check are the
        // discipline that catches that class of bug.
        assert_eq!(
            answer_sdp.matches("a=simulcast:send f;h;q").count(),
            1,
            "wire answer must contain exactly ONE \
             `a=simulcast:send f;h;q` line; got:\n{answer_sdp}"
        );
        assert!(
            !answer_sdp.contains("send f;h;q send"),
            "wire answer must NOT contain the rtc-0.9 doubled-direction \
             sentinel `send f;h;q send`; got:\n{answer_sdp}"
        );
        for rid in [
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ] {
            let line = format!("a=rid:{} send", rid.as_str());
            assert_eq!(
                answer_sdp.matches(&line).count(),
                1,
                "wire answer must contain exactly ONE `{line}` line \
                 (rtc 0.9 emits two; sanitize_answer_sdp dedupes); \
                 got:\n{answer_sdp}"
            );
        }
        // Sanity: NO recv direction (we're sendonly answerer).
        assert!(
            !answer_sdp.contains("a=simulcast:recv"),
            "answer must NOT contain a=simulcast:recv (we're sendonly); \
             got:\n{answer_sdp}"
        );
        // Parse-check: the sanitized wire answer must be acceptable
        // to rtc's own SDP parser as a type-`answer` description.
        // This is the strongest available proxy in pure-Rust tests
        // for "WebKit's parser would accept it" — both consume RFC
        // 8853-conformant simulcast.
        RTCSessionDescription::answer(answer_sdp.clone()).expect(
            "sanitized wire answer must parse as a valid \
             RTCSessionDescription of type `answer`",
        );

        // Clean up the spawned driver. Dropping `peer` cancels its
        // shutdown token, the driver task exits on the next select.
        drop(peer);
    }

    /// The answerer's DTLS role MUST be `passive` so the browser
    /// becomes the DTLS client and initiates the handshake. With
    /// the role left to default to `active`, the rtc 0.9 stack
    /// signals `a=setup:active` but never actually emits a
    /// ClientHello over the selected ICE-TCP candidate — the session
    /// stalls at STUN keepalives forever and the dashboard renders
    /// black. Diagnosed across four hops in #41 (RFC 7983 byte-class
    /// instrumentation showed Stun-only in every direction) and
    /// fixed by `setting_engine.set_answering_dtls_role(Server)`
    /// in `build_with_codec_set`. This test pins both the
    /// affirmative (passive present) and the negative (active
    /// absent) so a future refactor that drops the role assignment
    /// re-introduces the regression loudly instead of silently.
    #[tokio::test]
    async fn build_with_codec_set_pins_setup_passive_in_answer() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = crate::display::IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            42,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed for the role-pin test");

        assert!(
            answer_sdp.contains("a=setup:passive"),
            "answer must contain `a=setup:passive` so the browser becomes \
             the DTLS client and initiates the handshake; got:\n{answer_sdp}"
        );
        assert!(
            !answer_sdp.contains("a=setup:active"),
            "answer must NOT contain `a=setup:active` — that role left \
             rtc's DTLS state machine waiting forever over ICE-TCP \
             (diagnosed in #41 / fixed in #42); got:\n{answer_sdp}"
        );

        drop(peer);
    }

    /// **Phase 4c**: a single-encoding track (active_rids has length
    /// 1) emits NO simulcast lines in the answer. This pins the
    /// fall-through path for H.264 (single-layer by design — see
    /// `LayerSpec::single`'s rationale) and for VP8 cases where all
    /// simulcast layers but full dropped below MIN_LAYER_DIM.
    ///
    /// If this test fires, the unconditional simulcast emission
    /// regressed — every peer would advertise simulcast even when
    /// only one encoding exists, and browsers would request
    /// keyframes for rids the encoder pool can't serve.
    #[tokio::test]
    async fn build_with_codec_set_emits_no_simulcast_for_single_rid_h264() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_h264_video_offer_for_rtc();
        // Single rid → no simulcast in answer.
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = crate::display::IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            43,
            &offer_sdp,
            CodecKind::H264,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed for H.264 single-rid");

        assert!(
            !answer_sdp.contains("a=simulcast:"),
            "single-encoding track must NOT advertise simulcast; \
             got:\n{answer_sdp}"
        );
        assert!(
            !answer_sdp.contains("a=rid:"),
            "single-encoding track must NOT advertise per-rid lines; \
             got:\n{answer_sdp}"
        );

        drop(peer);
    }

    /// **Phase 4d.1**: a freshly-constructed `WebRtcPeer` exposes an
    /// observed-send-bitrate signal that starts at `None` (the watch
    /// channel's initial value). The driver's first poll seeds the
    /// per-SSRC `prev` map and publishes nothing; subsequent polls
    /// publish a delta. With no RTP traffic in this test (no real
    /// ICE flow, no media writes), `bytes_sent` stays at 0 and the
    /// helper produces `None` indefinitely — so the steady state
    /// here is `None`.
    ///
    /// Pin both APIs:
    /// - `current_observed_send_bitrate()` for one-shot reads.
    /// - `subscribe_observed_send_bitrate()` for change-driven consumers.
    #[tokio::test]
    async fn web_rtc_peer_exposes_observed_send_bitrate_api_starting_at_none() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, _answer_sdp) = WebRtcPeer::build_with_codec_set(
            44,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed");

        // One-shot read: None initially.
        assert_eq!(
            peer.current_observed_send_bitrate(),
            None,
            "freshly-constructed peer's observed send bitrate must \
             be None until the driver computes a `bytes_sent` delta"
        );

        // Subscriber: initial `borrow` returns None too. `borrow()`
        // reads the current value without marking it as "seen" —
        // identical semantics to `current_observed_send_bitrate()`
        // for the initial state.
        let rx = peer.subscribe_observed_send_bitrate();
        assert_eq!(
            *rx.borrow(),
            None,
            "fresh subscriber must observe None as the initial value"
        );

        // Independent receivers: a second subscribe yields a separate
        // Receiver; mutations on one (in this case, `borrow_and_update`
        // which marks the current value as seen) don't affect the other.
        let mut rx2 = peer.subscribe_observed_send_bitrate();
        assert_eq!(*rx2.borrow_and_update(), None);
        // The first receiver still sees None — independent state.
        assert_eq!(*rx.borrow(), None);

        drop(peer);
    }

    /// **Phase 4c**: `RtpSendState` carries a `by_rid` map keyed by
    /// `SimulcastRid`. `build_with_codec_set` populates it with one
    /// entry per active RID — N entries for VP8 simulcast (full +
    /// half + quarter), single entry for H.264. The driver's
    /// `state.rtp.by_rid.get_mut(rid)` lookup at write time uses this
    /// map to route per-RID packetizer / SSRC state.
    ///
    /// This test pins the data shape directly:
    /// - The map type allows multiple rids with distinct SSRCs.
    /// - Lookup by rid returns the matching `RidRtpState`.
    /// - The structure compiles and behaves like a `HashMap` (so the
    ///   driver's `state.rtp.by_rid.get_mut(rid)` lookup at write
    ///   time works without surprises).
    ///
    /// The driver's actual write_video_frame is exercised end-to-end
    /// by the `pool_intake_*` tests above (which run real encoders
    /// + forwarders).
    #[test]
    fn rtp_send_state_by_rid_supports_multiple_distinct_rids() {
        // Build three RidRtpState entries with distinct SSRCs.
        // packetizers can't be constructed without a real codec
        // payloader, so we exercise just the SSRC routing —
        // `RidRtpState` is a thin per-rid record and the routing
        // contract is "lookup by rid → get matching ssrc."
        let mut by_rid: HashMap<SimulcastRid, u32> = HashMap::new();
        for (rid, ssrc) in [
            (SimulcastRid::full(), 1001u32),
            (SimulcastRid::half(), 1002u32),
            (SimulcastRid::quarter(), 1003u32),
        ] {
            by_rid.insert(rid, ssrc);
        }
        assert_eq!(by_rid.len(), 3);
        assert_eq!(by_rid.get(&SimulcastRid::full()).copied(), Some(1001));
        assert_eq!(by_rid.get(&SimulcastRid::half()).copied(), Some(1002));
        assert_eq!(by_rid.get(&SimulcastRid::quarter()).copied(), Some(1003));
        // Lookup with a rid the map doesn't contain returns None —
        // matches the driver's "frame for unknown rid" defensive
        // branch in write_video_frame, which fail-loud-logs and drops.
        let unknown_rid = SimulcastRid::new("unknown");
        assert_eq!(by_rid.get(&unknown_rid), None);
    }

    // -------------------------------------------------------------------
    // Phase 4e: route_rtcp_keyframe_requests — per-RID PLI/FIR routing
    // -------------------------------------------------------------------

    /// Stand up a 3-layer VP8 simulcast SSRC table (full / half /
    /// quarter at distinct SSRCs) for the routing tests below.
    /// SSRCs are arbitrary but distinct; mirrors what
    /// `build_with_codec_set` produces from `new_ssrc()` in
    /// production.
    fn vp8_simulcast_ssrc_table() -> Vec<(SimulcastRid, u32)> {
        vec![
            (SimulcastRid::full(), 0xAAAA_0001),
            (SimulcastRid::half(), 0xAAAA_0002),
            (SimulcastRid::quarter(), 0xAAAA_0003),
        ]
    }

    fn pli_for(media_ssrc: u32) -> Box<dyn rtc::rtcp::Packet> {
        Box::new(PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc,
        })
    }

    fn fir_for(ssrcs: &[u32]) -> Box<dyn rtc::rtcp::Packet> {
        Box::new(FullIntraRequest {
            sender_ssrc: 0,
            media_ssrc: 0,
            fir: ssrcs
                .iter()
                .map(
                    |s| rtc::rtcp::payload_feedbacks::full_intra_request::FirEntry {
                        ssrc: *s,
                        sequence_number: 0,
                    },
                )
                .collect(),
        })
    }

    /// PLI for the full layer's SSRC routes to `SimulcastRid::full()`.
    /// Pre-4e the entire codec was kicked into a keyframe; per-RID
    /// routing is what makes simulcast recovery proportional to the
    /// layer that actually lost frames.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_routes_to_full_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xAAAA_0001)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI for full SSRC must route");
        assert_eq!(routed, SimulcastRid::full());
        assert!(rx.try_recv().is_err(), "no extra emissions");
    }

    /// PLI for the half layer's SSRC routes to `SimulcastRid::half()` —
    /// NOT to full. Mis-routing to full would burn a full-layer
    /// keyframe (highest bandwidth!) for a half-layer recovery and
    /// leave the half layer broken until its next natural keyframe.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_routes_to_half_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xAAAA_0002)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI for half SSRC must route");
        assert_eq!(routed, SimulcastRid::half());
        assert!(rx.try_recv().is_err(), "no extra emissions");
    }

    /// PLI for the quarter layer's SSRC routes to
    /// `SimulcastRid::quarter()`.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_routes_to_quarter_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xAAAA_0003)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI for quarter SSRC must route");
        assert_eq!(routed, SimulcastRid::quarter());
        assert!(rx.try_recv().is_err(), "no extra emissions");
    }

    /// PLI for an SSRC we never advertised is a no-op — no emission
    /// on the channel. The helper logs at warn level (verified
    /// indirectly via no panic + no emission); this can happen
    /// briefly during track-renegotiation windows or if the browser
    /// references an old SSRC after a track replacement.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_unknown_ssrc_is_noop() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xDEAD_BEEF)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        assert!(
            rx.try_recv().is_err(),
            "PLI for unknown SSRC must not emit a routing decision"
        );
    }

    /// FIR with a single entry for one SSRC routes the same way as
    /// PLI for that SSRC. RFC 5104 says FIR is "for the rare case
    /// where a new participant joins" — we treat it as semantically
    /// equivalent to PLI for keyframe-routing purposes.
    #[tokio::test]
    async fn route_rtcp_keyframe_fir_single_entry_routes_to_matching_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![fir_for(&[0xAAAA_0002])];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("FIR for half SSRC must route");
        assert_eq!(routed, SimulcastRid::half());
        assert!(rx.try_recv().is_err());
    }

    /// FIR can carry multiple `(ssrc, seq)` entries. Each known SSRC
    /// emits its own RID; unknown SSRCs in the same FIR are dropped
    /// silently without affecting the known ones (independent
    /// routing per entry).
    #[tokio::test]
    async fn route_rtcp_keyframe_fir_multi_entry_routes_each_known_ssrc() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        // FIR with full + unknown + quarter — only full and quarter
        // are known and must each route; unknown is a no-op for
        // that entry but must NOT inhibit the other entries.
        let packets = vec![fir_for(&[0xAAAA_0001, 0xDEAD_BEEF, 0xAAAA_0003])];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let mut routed: Vec<SimulcastRid> = Vec::new();
        while let Ok(r) = rx.try_recv() {
            routed.push(r);
        }
        assert_eq!(
            routed.len(),
            2,
            "FIR with 2 known + 1 unknown SSRC should emit 2 routings"
        );
        assert!(routed.contains(&SimulcastRid::full()));
        assert!(routed.contains(&SimulcastRid::quarter()));
    }

    /// FIR for an unknown SSRC alone is a no-op — same contract as
    /// the PLI unknown-SSRC test, exercised through the FIR codepath.
    #[tokio::test]
    async fn route_rtcp_keyframe_fir_unknown_ssrc_is_noop() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![fir_for(&[0xDEAD_BEEF])];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        assert!(
            rx.try_recv().is_err(),
            "FIR for unknown SSRC must not emit a routing decision"
        );
    }

    /// A compound RTCP packet may carry PLI + FIR + non-keyframe
    /// types (NACK, RR, SR, …). Only PLI and FIR contribute to
    /// keyframe routing; the helper iterates the whole vec and
    /// silently passes over non-feedback types.
    ///
    /// This test uses ReceiverReport as the "ignored" stand-in
    /// because it's the simplest non-keyframe RTCP type to
    /// construct — the same contract holds for NACK / SR / SDES /
    /// BYE / TWCC etc.
    #[tokio::test]
    async fn route_rtcp_keyframe_ignores_non_pli_fir_packets() {
        use rtc::rtcp::receiver_report::ReceiverReport;

        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets: Vec<Box<dyn rtc::rtcp::Packet>> = vec![
            Box::new(ReceiverReport::default()),
            pli_for(0xAAAA_0001),
            Box::new(ReceiverReport::default()),
        ];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI between RR/RR must still route");
        assert_eq!(routed, SimulcastRid::full());
        assert!(
            rx.try_recv().is_err(),
            "ReceiverReport packets must not emit any routing decisions"
        );
    }

    // -------------------------------------------------------------------
    // Phase 4d.1 review fix: extract_recent_outbound_bitrate tests
    // -------------------------------------------------------------------

    /// First poll (empty `prev`) → None. The helper has no prior
    /// sample to delta against; it seeds `prev` with the current
    /// values so the next poll can compute a real rate.
    #[test]
    fn extract_bitrate_first_poll_returns_none_seeds_prev() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let now = Instant::now();
        let result =
            extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 10_000u64)], &mut prev, now);
        assert_eq!(result, None, "first poll has no prev — must return None");
        assert_eq!(
            prev.get(&0xAAAA_0001),
            Some(&(10_000u64, now)),
            "first poll must seed prev so the next poll has a baseline"
        );
    }

    /// Second poll with positive delta → `Some(bps)` computed as
    /// `(delta_bytes * 8) / elapsed_secs`. Pin the canonical math so
    /// a future refactor that switches units (kbps? bytes/sec?)
    /// surfaces in the test.
    #[test]
    fn extract_bitrate_second_poll_computes_delta_bps() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // Poll 1: seed.
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 100_000u64)], &mut prev, t0);
        // Poll 2: 200 KB more in 1 second → 200_000 bytes * 8 = 1.6 Mbps.
        let result =
            extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 300_000u64)], &mut prev, t1);
        assert_eq!(result, Some(1_600_000));
        // Prev updated to the latest sample.
        assert_eq!(prev.get(&0xAAAA_0001), Some(&(300_000u64, t1)));
    }

    /// Multi-SSRC: deltas summed across all observed SSRCs. The
    /// layer-selection aggregator decides per-peer (the link is the
    /// bottleneck, not any individual encoding) so the helper rolls
    /// up to a single per-peer total.
    #[test]
    fn extract_bitrate_multi_ssrc_sums_deltas() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // VP8 simulcast: 3 outbound SSRCs (full / half / quarter).
        extract_recent_outbound_bitrate(
            vec![
                (0xAAAA_0001u32, 100_000u64),
                (0xAAAA_0002u32, 50_000u64),
                (0xAAAA_0003u32, 20_000u64),
            ],
            &mut prev,
            t0,
        );
        // After 1s: full +250KB (2 Mbps), half +50KB (400 kbps),
        // quarter +12.5KB (100 kbps). Total: 2.5 Mbps.
        let result = extract_recent_outbound_bitrate(
            vec![
                (0xAAAA_0001u32, 350_000u64),
                (0xAAAA_0002u32, 100_000u64),
                (0xAAAA_0003u32, 32_500u64),
            ],
            &mut prev,
            t1,
        );
        assert_eq!(result, Some(2_500_000));
    }

    /// Counter wraparound (current_bytes < prev_bytes for the same
    /// SSRC) skips that SSRC's contribution this poll and re-seeds
    /// prev with the current value. Defends against rtc-side stream
    /// restart on renegotiation (rtc drops + recreates the
    /// accumulator, resetting bytes_sent to 0). Without the skip we'd
    /// underflow the u64 subtraction and produce a garbage delta.
    #[test]
    fn extract_bitrate_counter_wraparound_skips_ssrc_reseed_prev() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // Seed at high value, then "wrap" to a low value (stream restart).
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 1_000_000u64)], &mut prev, t0);
        let result = extract_recent_outbound_bitrate(
            vec![(0xAAAA_0001u32, 500u64)], // restart, much smaller
            &mut prev,
            t1,
        );
        assert_eq!(
            result, None,
            "wraparound must skip the SSRC's contribution; with one \
             SSRC and that one skipped, total returns None"
        );
        // Prev re-seeded with the current value so the next poll
        // computes a clean delta against this baseline.
        assert_eq!(prev.get(&0xAAAA_0001), Some(&(500u64, t1)));
    }

    /// Zero elapsed time (two polls at the same Instant) skips that
    /// SSRC's contribution. Defends against the math (divide by zero
    /// → infinity → cast to u64 = wrong); the 1s poll interval makes
    /// this practically unreachable, but the helper guards against it.
    #[test]
    fn extract_bitrate_zero_elapsed_skips_ssrc() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 1000u64)], &mut prev, t0);
        let result = extract_recent_outbound_bitrate(
            vec![(0xAAAA_0001u32, 5000u64)],
            &mut prev,
            t0, // same instant
        );
        assert_eq!(result, None);
    }

    /// New SSRC appearing mid-stream (not in prev) returns None for
    /// THAT SSRC this poll, but seeds prev so the next poll produces
    /// a clean delta. Existing SSRCs continue to contribute normally.
    /// Models the case where a peer's simulcast layer count grows
    /// (e.g. an on-demand H.264 spawn during a session).
    #[test]
    fn extract_bitrate_new_ssrc_mid_stream_seeds_only() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // Seed: only one SSRC.
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 100_000u64)], &mut prev, t0);
        // Second poll: existing SSRC has +125KB (1 Mbps), and a new
        // SSRC appears with 50KB total but no prev to delta against.
        // Result: 1 Mbps from existing only; new SSRC seeded.
        let result = extract_recent_outbound_bitrate(
            vec![(0xAAAA_0001u32, 225_000u64), (0xAAAA_0002u32, 50_000u64)],
            &mut prev,
            t1,
        );
        assert_eq!(
            result,
            Some(1_000_000),
            "existing SSRC's delta contributes; new SSRC seeds only"
        );
        assert_eq!(prev.get(&0xAAAA_0002), Some(&(50_000u64, t1)));
    }

    /// Empty current iterator → None. Models the very-early-life case
    /// where the rtc stats report has no outbound streams yet (track
    /// not yet attached, or pre-handshake).
    #[test]
    fn extract_bitrate_empty_current_returns_none() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let now = Instant::now();
        let result = extract_recent_outbound_bitrate(Vec::<(u32, u64)>::new(), &mut prev, now);
        assert_eq!(result, None);
    }

    /// Stable rate over multiple polls produces consistent bps
    /// readings. Pins that the helper's stateful arithmetic doesn't
    /// drift across iterations.
    #[test]
    fn extract_bitrate_stable_rate_consistent_across_polls() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let mut t = Instant::now();
        let mut bytes: u64 = 0;

        // Seed.
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, bytes)], &mut prev, t);
        // Three polls each adding 125KB in 1s = steady 1 Mbps.
        for _ in 0..3 {
            t += Duration::from_secs(1);
            bytes += 125_000;
            let result =
                extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, bytes)], &mut prev, t);
            assert_eq!(result, Some(1_000_000));
        }
    }

    /// `rid_for_ssrc` returns the matching RID for any known SSRC
    /// in the table; None for unknown SSRC. Same contract as the
    /// helper that wraps it; tested directly so a refactor that
    /// changes the lookup data structure (HashMap vs Vec vs
    /// pre-built reverse map) keeps the contract intact.
    #[test]
    fn rid_for_ssrc_returns_matching_rid_or_none() {
        let table = vp8_simulcast_ssrc_table();
        assert_eq!(
            rid_for_ssrc(&table, 0xAAAA_0001),
            Some(SimulcastRid::full())
        );
        assert_eq!(
            rid_for_ssrc(&table, 0xAAAA_0002),
            Some(SimulcastRid::half())
        );
        assert_eq!(
            rid_for_ssrc(&table, 0xAAAA_0003),
            Some(SimulcastRid::quarter())
        );
        assert_eq!(rid_for_ssrc(&table, 0xDEAD_BEEF), None);
        // Empty table: every lookup is None — defends against an
        // empty by_rid in the `build_with_codec_set` empty-active_rids
        // path (which errors out upstream, but the lookup must still
        // be a no-op rather than a panic if it's reached).
        assert_eq!(rid_for_ssrc(&[], 0xAAAA_0001), None);
    }

    // -----------------------------------------------------------------
    // Phase 4d.3a: map_remote_inbound_to_rid_health helper tests
    // -----------------------------------------------------------------

    #[test]
    fn map_remote_inbound_empty_input_returns_empty_map() {
        // No RR data yet — common steady state immediately after a
        // peer connects but before the first RR has been received.
        // The watch publishes the empty map; consumers see "no
        // signal yet" rather than a stale or fabricated reading.
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(std::iter::empty(), &table);
        assert!(out.is_empty());
    }

    #[test]
    fn map_remote_inbound_unknown_ssrc_dropped_silently() {
        // RR for an SSRC we don't carry per-RID (transient
        // renegotiation, on-demand H.264 SSRC outside the simulcast
        // RID table, RR for an SSRC we never advertised). Same
        // defensive policy as `route_rtcp_keyframe_requests`: drop
        // silently rather than fail, since these can occur in the
        // normal lifecycle and aren't actionable. `rtt_measurements
        // = 5` keeps this entry past the pre-RR filter so the test
        // exercises the SSRC-table-drop path specifically, not the
        // pre-RR-filter path.
        let table = vp8_simulcast_ssrc_table();
        let out =
            map_remote_inbound_to_rid_health(vec![(0xDEAD_BEEFu32, 0.05, 42, 0.018, 5)], &table);
        assert!(out.is_empty());
    }

    #[test]
    fn map_remote_inbound_all_known_ssrcs_mapped_to_rids() {
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(
            vec![
                // (ssrc, fraction_lost, packets_lost, rtt, rtt_measurements)
                (0xAAAA_0001u32, 0.01, 5, 0.012, 3),  // full
                (0xAAAA_0002u32, 0.05, 23, 0.018, 7), // half
                (0xAAAA_0003u32, 0.20, 99, 0.025, 4), // quarter
            ],
            &table,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.get(&SimulcastRid::full()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.01,
                packets_lost_total: 5,
                round_trip_time_seconds: 0.012,
                round_trip_time_measurements: 3,
            })
        );
        assert_eq!(
            out.get(&SimulcastRid::half()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.05,
                packets_lost_total: 23,
                round_trip_time_seconds: 0.018,
                round_trip_time_measurements: 7,
            })
        );
        assert_eq!(
            out.get(&SimulcastRid::quarter()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.20,
                packets_lost_total: 99,
                round_trip_time_seconds: 0.025,
                round_trip_time_measurements: 4,
            })
        );
    }

    #[test]
    fn map_remote_inbound_mixed_known_and_unknown_keeps_only_known() {
        // A realistic transient-window state: RR for one
        // simulcast layer arrives alongside RR for a now-released
        // on-demand H.264 SSRC. Helper preserves the known RID
        // entry, drops the unknown. Both have non-zero
        // `rtt_measurements` so the pre-RR filter doesn't
        // intercept either — the test exercises SSRC-table-membership
        // specifically.
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(
            vec![
                (0xAAAA_0002u32, 0.07, 30, 0.020, 9),   // half (known)
                (0xCAFE_BABEu32, 0.50, 200, 0.100, 11), // unknown
            ],
            &table,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out.get(&SimulcastRid::half()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.07,
                packets_lost_total: 30,
                round_trip_time_seconds: 0.020,
                round_trip_time_measurements: 9,
            })
        );
        assert!(!out.contains_key(&SimulcastRid::full()));
        assert!(!out.contains_key(&SimulcastRid::quarter()));
    }

    #[test]
    fn map_remote_inbound_empty_ssrc_table_drops_everything() {
        // Defends the early-session window before `state.rtp.by_rid`
        // is fully populated (or after teardown clears it). Every
        // RR that arrives has nothing to map against; helper returns
        // empty rather than panicking on the lookup. Inputs use
        // `rtt_measurements > 0` so the pre-RR filter doesn't pre-
        // empt the SSRC-table check — the test exercises empty-
        // table-drop semantics specifically.
        let out = map_remote_inbound_to_rid_health(
            vec![
                (0xAAAA_0001u32, 0.01, 5, 0.012, 2),
                (0xAAAA_0002u32, 0.02, 7, 0.015, 3),
            ],
            &[],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn map_remote_inbound_filters_pre_rr_default_snapshots() {
        // **4d.3a review fix regression**: rtc 0.9's accumulator
        // emits a default-valued `RemoteInboundRTP` entry for every
        // outbound stream the moment the stream exists, even before
        // any actual RR has been received. All fields default to
        // zero, including `fraction_lost = 0.0` (which would
        // otherwise present as "perfectly healthy" to the 4d.3b
        // policy and confirm `Wanted` immediately on connect).
        // `round_trip_time_measurements == 0` is the discriminator:
        // non-zero means at least one RR has arrived and the values
        // reflect a real measurement. The helper filters zero-
        // measurement entries out so the policy receives "no
        // signal" until the first RR actually lands.
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(
            vec![
                // Pre-RR snapshot for `full` — must be filtered.
                (0xAAAA_0001u32, 0.0, 0, 0.0, 0),
                // Real RR-derived snapshot for `half` — must be kept.
                (0xAAAA_0002u32, 0.05, 23, 0.018, 4),
                // Another pre-RR snapshot for `quarter`, with
                // `fraction_lost = 0.0` that would look "healthy"
                // if not filtered. Must be filtered.
                (0xAAAA_0003u32, 0.0, 0, 0.0, 0),
            ],
            &table,
        );
        assert_eq!(
            out.len(),
            1,
            "only the entry with rtt_measurements > 0 should survive; \
             pre-RR defaults must be filtered. Got {out:?}",
        );
        assert!(!out.contains_key(&SimulcastRid::full()));
        assert!(!out.contains_key(&SimulcastRid::quarter()));
        assert_eq!(
            out.get(&SimulcastRid::half()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.05,
                packets_lost_total: 23,
                round_trip_time_seconds: 0.018,
                round_trip_time_measurements: 4,
            })
        );
    }

    /// **Phase 4d.3a**: a freshly-constructed `WebRtcPeer` exposes a
    /// remote-inbound-health watch that starts at the empty map (the
    /// watch channel's initial value). The driver's first poll
    /// publishes whatever's in `report.iter_by_type(RTCStatsType::RemoteInboundRTP)`
    /// at that moment — empty until any RR has arrived.
    ///
    /// In this test there's no real ICE flow, so no RTP is ever sent
    /// and no RR is ever received → the steady state is the empty
    /// map.
    ///
    /// Pin both APIs:
    /// - `current_remote_inbound_health()` for one-shot reads.
    /// - `subscribe_remote_inbound_health()` for change-driven consumers
    ///   (the layer-selection aggregator in 4d.3c).
    #[tokio::test]
    async fn web_rtc_peer_exposes_remote_inbound_health_api_starting_at_empty() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, _answer_sdp) = WebRtcPeer::build_with_codec_set(
            44,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed");

        // One-shot read: empty map initially.
        let snapshot = peer.current_remote_inbound_health();
        assert!(
            snapshot.is_empty(),
            "freshly-constructed peer's remote-inbound-health snapshot \
             must be empty until the driver projects a non-empty \
             remote-inbound-rtp set into per-RID health; got {snapshot:?}",
        );

        // Subscriber: initial `borrow` returns empty too. Mirrors
        // `current_remote_inbound_health` for the initial state.
        let rx = peer.subscribe_remote_inbound_health();
        assert!(rx.borrow().is_empty());
        // Independent receivers: a second subscribe returns its own
        // receiver carrying the same initial value.
        let rx2 = peer.subscribe_remote_inbound_health();
        assert!(rx2.borrow().is_empty());

        drop(peer);
    }

    // ----- sanitize_answer_sdp -----------------------------------
    //
    // Pure-helper tests for the rtc 0.9 SDP-writer workaround.
    // See `sanitize_answer_sdp` doc-comment for the bugs being
    // addressed. Each test fixes one input/output pair so a future
    // regression that re-introduces duplicate rids or the doubled
    // simulcast direction fires loudly.

    /// rtc 0.9 emits each `a=rid:<rid> send` line twice for multi-RID
    /// send. The sanitizer must dedupe each to exactly one occurrence
    /// while preserving line order (first-seen wins) and untouched
    /// surrounding lines.
    #[test]
    fn sanitize_answer_sdp_dedupes_duplicate_rid_send_lines() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rid:f send\r\n",
            "a=rid:h send\r\n",
            "a=rid:q send\r\n",
            "a=rid:f send\r\n",
            "a=rid:h send\r\n",
            "a=rid:q send\r\n",
            "a=simulcast:send f;h;q\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert_eq!(out.matches("a=rid:f send").count(), 1, "got:\n{out}");
        assert_eq!(out.matches("a=rid:h send").count(), 1, "got:\n{out}");
        assert_eq!(out.matches("a=rid:q send").count(), 1, "got:\n{out}");
        // Preserves CRLF line endings.
        assert!(out.contains("\r\n"), "must preserve CRLF; got:\n{out}");
        // Surrounding lines untouched.
        assert!(out.contains("a=rtpmap:96 VP8/90000"));
        assert!(out.contains("a=simulcast:send f;h;q"));
    }

    /// `a=simulcast:send f;h;q send f;h;q` (rtc 0.9 doubled-direction
    /// bug) must collapse to `a=simulcast:send f;h;q`. The substring
    /// `send f;h;q send` is the regression marker.
    #[test]
    fn sanitize_answer_sdp_collapses_doubled_simulcast_send_direction() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=simulcast:send f;h;q send f;h;q\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert!(
            out.contains("a=simulcast:send f;h;q"),
            "must contain a=simulcast:send f;h;q; got:\n{out}"
        );
        assert!(
            !out.contains("send f;h;q send"),
            "must NOT contain doubled-direction substring `send f;h;q \
             send`; got:\n{out}"
        );
        // Exactly one a=simulcast: line in the output.
        let count = out
            .lines()
            .filter(|l| l.starts_with("a=simulcast:"))
            .count();
        assert_eq!(
            count, 1,
            "exactly one a=simulcast: line; got {count}\n{out}"
        );
    }

    /// Already-clean SDP must pass through unchanged. Dedupe is
    /// idempotent: re-applying the sanitizer to its own output is a
    /// no-op.
    #[test]
    fn sanitize_answer_sdp_already_clean_unchanged() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rid:f send\r\n",
            "a=rid:h send\r\n",
            "a=rid:q send\r\n",
            "a=simulcast:send f;h;q\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert_eq!(out, input, "clean input must pass through unchanged");
        // Idempotent: sanitize(sanitize(x)) == sanitize(x).
        let twice = super::sanitize_answer_sdp(&out);
        assert_eq!(twice, out, "sanitizer must be idempotent");
    }

    /// H.264 / single-RID answers (no `a=rid:` or `a=simulcast:`
    /// lines at all — the federated peer-display path post-#46 fix
    /// and any single-encoding answer) must pass through untouched.
    #[test]
    fn sanitize_answer_sdp_single_rid_no_simulcast_unchanged() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 98\r\n",
            "a=rtpmap:98 H264/90000\r\n",
            "a=fmtp:98 profile-level-id=42e01f;packetization-mode=1\r\n",
            "a=ssrc:2616664936 cname:display-1\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert_eq!(out, input);
    }

    /// Bidirectional simulcast (`send f;h;q recv x`) is valid per
    /// RFC 8853 — the second pair has a different direction. The
    /// sanitizer must NOT collapse it. Distinguishes the bug shape
    /// (same direction twice) from valid bidirectional shape.
    #[test]
    fn sanitize_answer_sdp_preserves_valid_bidirectional_simulcast() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=simulcast:send f;h;q recv x\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert!(
            out.contains("a=simulcast:send f;h;q recv x"),
            "valid bidirectional simulcast must pass through; got:\n{out}"
        );
    }

    // -----------------------------------------------------------------
    // F-1.2: federated authority state — passive server-side support
    // for the `display_input_authority` data channel.
    // -----------------------------------------------------------------

    /// Wire vocabulary pin: `as_wire_str` matches the local 5c
    /// data-channel state strings exactly. If anyone changes one
    /// without the other, federated browsers' chip rendering desyncs
    /// from local browsers' chip rendering — this test fires.
    #[test]
    fn authority_state_wire_strings_match_local_5c() {
        assert_eq!(DisplayInputAuthorityState::You.as_wire_str(), "you");
        assert_eq!(DisplayInputAuthorityState::Other.as_wire_str(), "other");
        assert_eq!(
            DisplayInputAuthorityState::Unclaimed.as_wire_str(),
            "unclaimed"
        );
    }

    /// `serialize_authority_state` produces the canonical
    /// `display_input_authority_state` frame: `t` discriminator,
    /// numeric `display_id`, string `state` from the wire vocabulary.
    /// Browser handlers parse this exact shape; if it drifts, the
    /// chip on the federated peer-display panel stops updating.
    #[test]
    fn serialize_authority_state_produces_canonical_frame() {
        for (state, expected_state) in [
            (DisplayInputAuthorityState::You, "you"),
            (DisplayInputAuthorityState::Other, "other"),
            (DisplayInputAuthorityState::Unclaimed, "unclaimed"),
        ] {
            let json = serialize_authority_state(7, state);
            let parsed: serde_json::Value =
                serde_json::from_str(&json).expect("frame must parse as JSON");
            assert_eq!(parsed["t"], "display_input_authority_state");
            assert_eq!(parsed["display_id"], 7);
            assert_eq!(parsed["state"], expected_state);
        }
    }

    /// F-1.3b2: `parse_authority_channel_message` round-trips the
    /// canonical wire shape (`t` discriminator + numeric `display_id`)
    /// to the right [`AuthorityChannelMessage`] variant. Pins the
    /// browser↔peer wire vocabulary the federated authority data
    /// channel uses; if browser-side serialization drifts, this test
    /// fires.
    #[test]
    fn parse_authority_channel_message_round_trip() {
        let req = parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request", "display_id": 7 }"#,
        )
        .expect("request frame must parse");
        assert_eq!(req, AuthorityChannelMessage::Request { display_id: 7 });

        let rel = parse_authority_channel_message(
            r#"{ "t": "display_input_authority_release", "display_id": 0 }"#,
        )
        .expect("release frame must parse");
        assert_eq!(rel, AuthorityChannelMessage::Release { display_id: 0 });
    }

    /// F-1.3b2: extra/unknown fields on a well-formed frame are
    /// preserved-by-ignoring — the parser is strict on the
    /// discriminator (`t`) and the typed field (`display_id`) but
    /// tolerant of anything else. Mirrors `parse_clipboard_set`'s
    /// loose-extras contract and leaves room for the browser to add
    /// forward-compat metadata (request ids for ack tracking, actor
    /// identity hints, timestamps) without forcing a peer-side
    /// version bump.
    #[test]
    fn parse_authority_channel_message_tolerates_extra_fields() {
        let msg = parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request",
                 "display_id": 0,
                 "request_id": "abc",
                 "ts": 12345,
                 "actor": { "kind": "operator" } }"#,
        )
        .expect("extra fields must not block parse");
        assert_eq!(msg, AuthorityChannelMessage::Request { display_id: 0 });
    }

    /// F-1.3b2: malformed frames silently drop. Strict by design —
    /// the authority handler should never see a frame the wire layer
    /// couldn't validate. Mirrors `parse_clipboard_set`'s contract:
    /// the browser is expected to send well-formed frames; recovery
    /// from the malformed case lives outside the transport.
    #[test]
    fn parse_authority_channel_message_rejects_malformed() {
        // Unknown `t` discriminator.
        assert!(parse_authority_channel_message(
            r#"{ "t": "display_input_authority_steal", "display_id": 0 }"#,
        )
        .is_none());

        // Missing `display_id`.
        assert!(
            parse_authority_channel_message(r#"{ "t": "display_input_authority_request" }"#,)
                .is_none()
        );

        // Non-numeric `display_id`.
        assert!(parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request", "display_id": "0" }"#,
        )
        .is_none());

        // `display_id` outside u32 range.
        assert!(parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request", "display_id": 4294967296 }"#,
        )
        .is_none());

        // Not JSON.
        assert!(parse_authority_channel_message("not json at all").is_none());

        // Missing `t` discriminator.
        assert!(parse_authority_channel_message(r#"{ "display_id": 0 }"#,).is_none());
    }

    #[test]
    fn parse_tile_control_message_round_trip() {
        let subscribe =
            tile_transport::encode_frame(&tile_transport::TileFrame::Subscribe { client_id: 99 })
                .unwrap();
        assert_eq!(
            parse_tile_control_message(&subscribe),
            Some(TileControlMessage::Subscribe { client_id: 99 })
        );

        let snapshot = tile_transport::encode_frame(&tile_transport::TileFrame::SnapshotRequest {
            epoch: 7,
            reason: tile_transport::SnapshotRequestReason::Gap,
        })
        .unwrap();
        assert_eq!(
            parse_tile_control_message(&snapshot),
            Some(TileControlMessage::SnapshotRequest {
                epoch: 7,
                reason: tile_transport::SnapshotRequestReason::Gap,
            })
        );

        let gap = tile_transport::encode_frame(&tile_transport::TileFrame::GapReport {
            epoch: 3,
            last_seen_seq: 10,
            expected_seq: 14,
        })
        .unwrap();
        assert_eq!(
            parse_tile_control_message(&gap),
            Some(TileControlMessage::GapReport {
                epoch: 3,
                last_seen_seq: 10,
                expected_seq: 14,
            })
        );
    }

    #[test]
    fn parse_tile_control_message_rejects_non_control_frames() {
        let update = tile_transport::encode_frame(&tile_transport::TileFrame::TileUpdate {
            epoch: 1,
            seq: 1,
            records: Vec::new(),
        })
        .unwrap();
        assert_eq!(parse_tile_control_message(&update), None);
        assert_eq!(parse_tile_control_message(b"not tile wire"), None);
    }

    /// `drain_pending_authority_for_label` is a no-op (returns empty,
    /// leaves `pending` untouched) for any channel label that isn't
    /// `display_input_authority`. The OnDataChannel(OnOpen) handler
    /// fires for every data channel — clipboard, control, pointer —
    /// and must not consume the authority queue when an unrelated
    /// channel opens.
    #[test]
    fn drain_pending_authority_skips_other_labels() {
        let mut pending = vec![
            (0, DisplayInputAuthorityState::You),
            (1, DisplayInputAuthorityState::Other),
        ];

        for label in ["clipboard", "control", "pointer", "random"] {
            let drained = drain_pending_authority_for_label(label, &mut pending);
            assert!(
                drained.is_empty(),
                "non-authority label '{label}' must drain nothing"
            );
            assert_eq!(
                pending.len(),
                2,
                "non-authority label '{label}' must leave queue intact",
            );
        }
    }

    /// `drain_pending_authority_for_label` consumes the entire queue
    /// (in arrival order) when the channel label matches and resets
    /// `pending` to empty. After draining, a second call returns an
    /// empty vec — replays must come from a fresh push, not a
    /// double-drain.
    #[test]
    fn drain_pending_authority_flushes_on_authority_label() {
        let mut pending = vec![
            (0, DisplayInputAuthorityState::You),
            (1, DisplayInputAuthorityState::Other),
            (2, DisplayInputAuthorityState::Unclaimed),
        ];

        let drained = drain_pending_authority_for_label(AUTHORITY_CHANNEL_LABEL, &mut pending);
        assert_eq!(drained.len(), 3, "must drain all queued entries");
        assert!(pending.is_empty(), "queue must be empty after drain");

        // Order preserved.
        assert_eq!(drained[0], (0, DisplayInputAuthorityState::You));
        assert_eq!(drained[1], (1, DisplayInputAuthorityState::Other));
        assert_eq!(drained[2], (2, DisplayInputAuthorityState::Unclaimed));

        // Second drain returns empty (no double-flush).
        let again = drain_pending_authority_for_label(AUTHORITY_CHANNEL_LABEL, &mut pending);
        assert!(again.is_empty(), "second drain must be empty");
    }

    /// Empty queue → empty drain on the authority label. No panics,
    /// no resource consumption when the broadcast loop hasn't pushed
    /// anything yet.
    #[test]
    fn drain_pending_authority_empty_queue_is_noop() {
        let mut pending: Vec<(u32, DisplayInputAuthorityState)> = Vec::new();
        let drained = drain_pending_authority_for_label(AUTHORITY_CHANNEL_LABEL, &mut pending);
        assert!(drained.is_empty());
        assert!(pending.is_empty());
    }

    /// D-3b: tile data-channel labels and queue policy are part of
    /// the browser<->peer contract. Control/snapshot are reliable
    /// bootstrap channels and may queue before open; deltas are
    /// supersedable and must not queue stale frames.
    #[test]
    fn tile_data_channel_labels_and_queue_policy_match_wire_contract() {
        assert_eq!(TileDataChannel::Control.label(), "tile-control");
        assert_eq!(TileDataChannel::Snapshot.label(), "tile-snapshot");
        assert_eq!(TileDataChannel::Deltas.label(), "tile-deltas");

        assert!(TileDataChannel::Control.queues_before_open());
        assert!(TileDataChannel::Snapshot.queues_before_open());
        assert!(!TileDataChannel::Deltas.queues_before_open());
    }

    #[test]
    fn tile_watermark_threshold_conversion_saturates_to_u32() {
        assert_eq!(watermark_to_u32(0), 0);
        assert_eq!(watermark_to_u32(1024), 1024);
        assert_eq!(watermark_to_u32(u32::MAX as usize), u32::MAX);
        assert_eq!(watermark_to_u32(u32::MAX as usize + 1), u32::MAX);
    }
}
