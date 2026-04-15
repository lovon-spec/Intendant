//! Per-peer WebRTC driver built on `str0m`.
//!
//! Architecture: each `WebRtcPeer` owns a tokio task ("driver") that holds an
//! `Rtc` instance and a UDP socket. The driver pumps three things in a single
//! `select!` loop:
//!
//! 1. Inbound UDP datagrams → `rtc.handle_input(Receive)`
//! 2. Encoded video frames from the shared encoder fan-out → `writer.write(...)`
//! 3. Commands from the public `WebRtcPeer` handle (ICE candidates, clipboard
//!    sends, shutdown) → `rtc.add_remote_candidate()` / `rtc.channel().write()`
//!
//! After every input the driver drains `rtc.poll_output()` until it returns
//! `Output::Timeout`, sending any `Transmit` outputs over the UDP socket and
//! dispatching `Event` outputs to the input/clipboard handlers.
//!
//! ## ICE-TCP multiplexing
//!
//! When `IceConfig::tcp_port` is set, the `DisplaySession` creates a shared
//! `TcpDispatcher` holding a single `TcpListener` on that port. Peers register
//! their pre-generated local ufrag with the dispatcher at construction time.
//! The dispatcher accepts incoming connections, parses the first RFC 4571
//! framed STUN binding request, extracts the USERNAME attribute's local-ufrag
//! portion, and routes the connection to the matching peer. Each TCP
//! connection becomes a bidirectional channel with the peer's driver — inbound
//! frames flow into the same packet channel that UDP uses (tagged with
//! `Protocol::Tcp`), outbound `Output::Transmit` with `proto == Tcp` is written
//! to the connection's write-half keyed on the destination address.

use super::clipboard::ClipboardContent;
use super::{EncodedFrame, IceConfig, InputEvent, PeerId};
use crate::error::CallerError;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{MediaAdded, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{DatagramRecv, Protocol, Receive};
use str0m::net::TcpType;
use str0m::{Candidate, Event, IceCreds, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;

/// Bound on the per-peer encoded-frame channel. Frames in excess are dropped
/// with backpressure registered in the display metrics.
const ENCODED_FRAME_CHANNEL: usize = 8;

/// Bound on the per-peer command channel.
const COMMAND_CHANNEL: usize = 32;

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
// `TcpPeerRegistry` is a pure demux registry with no listener of its own. One
// instance is created at web_gateway startup and shared across all display
// sessions. The web_gateway's accept loop (which already peeks every
// incoming TCP connection for HTTP vs. WebSocket) grows a third branch: if
// the first bytes look like an RFC 4571-framed STUN binding request, read
// one full frame, then call `route_accepted` to hand the connection to the
// matching peer. HTTP-on-the-same-port works untouched because the peek is
// non-destructive and STUN traffic is byte-distinguishable from HTTP
// methods (no printable ASCII at offset 0) and TLS handshakes (no 0x16 at
// offset 0).
//
// The same registry can also back a standalone TCP listener on a
// user-configured fixed port (Phase 2 behavior) via `bind_standalone`, for
// deployments where multiplexing on the HTTP port isn't wanted (e.g. HTTPS
// terminated by a proxy that doesn't pass through binary frames).

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
    /// matching). The peer's driver must feed this to `rtc.handle_input`.
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
    /// detects STUN-framed traffic on the HTTP port, and by
    /// `bind_standalone` if a dedicated TCP listener is configured.
    pub async fn route_accepted(
        self: &Arc<Self>,
        stream: TcpStream,
        first_frame: Vec<u8>,
        remote_addr: SocketAddr,
    ) -> Result<(), String> {
        let local_addr = stream
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;

        let username = parse_stun_username(&first_frame).ok_or_else(|| {
            "first frame is not a STUN binding request with USERNAME".to_string()
        })?;

        // USERNAME format for ICE is "remote_ufrag:local_ufrag". The local
        // side is us; that's what we demux on.
        let local_ufrag = username
            .split_once(':')
            .map(|(_, local)| local.to_string())
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

    /// Spawn a standalone TCP listener on the given port that funnels
    /// incoming connections through this registry. Used for the Phase 2
    /// behavior where the user explicitly sets `[webrtc] tcp_port` to pick
    /// a dedicated port separate from HTTP. When multiplexing on the HTTP
    /// port (the default), this helper is not used — the web_gateway's
    /// accept loop calls `route_accepted` directly.
    pub async fn bind_standalone(
        self: &Arc<Self>,
        port: u16,
    ) -> Result<(), CallerError> {
        let listener = TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port,
        ))
        .await
        .map_err(|e| {
            CallerError::WebRtc(format!("bind standalone ICE-TCP listener on :{port}: {e}"))
        })?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| CallerError::WebRtc(format!("listener local_addr: {e}")))?;
        eprintln!("[display/webrtc] ICE-TCP standalone listener bound on {local_addr}");

        let registry = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let registry = Arc::clone(&registry);
                        tokio::spawn(async move {
                            match probe_and_route_tcp_connection(stream, remote_addr, registry)
                                .await
                            {
                                Ok(()) => {}
                                Err(e) => eprintln!(
                                    "[display/webrtc] ICE-TCP standalone probe for {remote_addr} failed: {e}"
                                ),
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("[display/webrtc] ICE-TCP standalone accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
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
        self.registry.registry.lock().unwrap().remove(&self.local_ufrag);
    }
}

/// Read the first RFC 4571 frame from a freshly accepted standalone TCP
/// connection, then hand everything to the registry for ufrag-based
/// routing. Called from the standalone listener spawned by
/// `bind_standalone`; the web_gateway path already has the first frame in
/// hand from its own peek-and-read step and calls `route_accepted` directly.
async fn probe_and_route_tcp_connection(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    registry: Arc<TcpPeerRegistry>,
) -> Result<(), String> {
    let first_frame = read_rfc4571_frame(&mut stream)
        .await
        .map_err(|e| format!("read first frame: {e}"))?;
    registry.route_accepted(stream, first_frame, remote_addr).await
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
pub async fn read_rfc4571_frame_pub(
    stream: &mut TcpStream,
) -> std::io::Result<Vec<u8>> {
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
/// `Rtc` instance and the UDP socket exclusively.
pub struct WebRtcPeer {
    #[allow(dead_code)]
    pub peer_id: PeerId,
    encoded_frame_tx: mpsc::Sender<Arc<EncodedFrame>>,
    command_tx: mpsc::Sender<Command>,
    shutdown: CancellationToken,
}

/// Commands sent from the public `WebRtcPeer` handle to the driver task.
enum Command {
    AddIceCandidate(String),
    SendClipboard(ClipboardContent),
}

impl WebRtcPeer {
    /// Create a new peer from an SDP offer, returning `(Self, answer_sdp)`.
    ///
    /// Steps:
    /// 1. Build an `Rtc` configured for the negotiated `codec_mime`.
    /// 2. Bind a per-peer UDP socket and register it as a host candidate.
    /// 3. Synchronously generate the SDP answer via `accept_offer`.
    /// 4. Spawn the driver task and return.
    ///
    /// `ice_tx` is accepted for API parity with the previous webrtc-rs
    /// implementation but is currently unused: str0m emits its host candidates
    /// inline in the answer SDP, so there is nothing to trickle from the
    /// server side. The browser still trickles its candidates via
    /// `add_ice_candidate`.
    pub async fn new(
        peer_id: PeerId,
        offer_sdp: &str,
        codec_mime: &str,
        _ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<TcpPeerRegistry>>,
        tcp_advertised_addr: Option<SocketAddr>,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        _ice_tx: mpsc::Sender<(PeerId, String)>,
    ) -> Result<(Self, String), CallerError> {
        // --- Pre-generate ICE credentials ----------------------------------
        // We need to know the local ufrag *before* Rtc::build so we can
        // register it with the TCP dispatcher, if present.
        let ice_creds = IceCreds::new();
        let local_ufrag = ice_creds.ufrag.clone();

        // --- Build the Rtc with only the negotiated codec enabled ---------
        // The session-level codec selection has already happened in
        // DisplaySession::handle_offer; we restrict str0m's codec set so
        // negotiation can only resolve to that one codec.
        let mut config = RtcConfig::new()
            .clear_codecs()
            .set_local_ice_credentials(ice_creds);
        config = match codec_mime {
            super::encode::MIME_TYPE_VP8 => config.enable_vp8(true),
            super::encode::MIME_TYPE_H264 => config.enable_h264(true),
            other => {
                return Err(CallerError::WebRtc(format!(
                    "unsupported codec mime: {other}"
                )));
            }
        };
        let mut rtc = config.build(Instant::now());

        // --- Bind one UDP socket per local interface -----------------------
        // str0m's ICE agent matches incoming packets against local candidates
        // by `(local_address, port)`. A single wildcard bind would surface as
        // `0.0.0.0:port` on `socket.local_addr()`, which never matches the
        // concrete-IP candidates we'd advertise — connectivity checks then
        // can't form a valid pair. So we bind a separate socket per
        // interface and emit a host candidate that exactly matches each
        // socket's local address.
        let mut sockets: Vec<Arc<UdpSocket>> = Vec::new();
        let local_addrs = routable_local_addrs();
        for iface_addr in &local_addrs {
            let bind_addr = SocketAddr::new(*iface_addr, 0);
            let socket = match UdpSocket::bind(bind_addr).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "[display/webrtc] skipping UDP bind on {iface_addr}: {e}"
                    );
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
            match Candidate::host(local, "udp") {
                Ok(c) => {
                    rtc.add_local_candidate(c);
                    sockets.push(Arc::new(socket));
                }
                Err(e) => {
                    eprintln!("[display/webrtc] skipping UDP host candidate {local}: {e}");
                }
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound".to_string(),
            ));
        }

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
        // On the server side we "lie" to str0m about the inbound
        // destination: regardless of what `stream.local_addr()` says
        // (typically the VM's internal interface IP behind the NAT), we
        // pass `destination = tcp_advertised_addr` to `handle_input`.
        // str0m matches the lied-about destination to its single local
        // TCP candidate and forms a clean pair; data still flows because
        // the TCP stream is bidirectional and we own the write half
        // directly, no kernel routing involved.
        let mut peer_registration = None;
        let mut tcp_conn_rx: Option<mpsc::Receiver<AcceptedTcpConnection>> = None;
        let mut tcp_fake_local: Option<SocketAddr> = None;
        if let (Some(registry), Some(advertised)) = (
            tcp_peer_registry.as_ref(),
            tcp_advertised_addr.filter(|a| !a.ip().is_loopback() && !a.ip().is_unspecified()),
        ) {
            let (registration, rx) = registry.register(local_ufrag.clone());
            peer_registration = Some(registration);
            tcp_conn_rx = Some(rx);
            tcp_fake_local = Some(advertised);
            // RFC 6544 requires TCP ICE candidates to carry a `tcptype`
            // attribute. `Candidate::host(addr, "tcp")` doesn't set it,
            // and browsers drop TCP candidates that lack it. The builder
            // lets us set `tcptype: passive` — "the remote actively opens
            // the TCP connection to us", the correct role for a
            // server-side host candidate.
            match Candidate::builder()
                .tcp()
                .host(advertised)
                .tcptype(TcpType::Passive)
                .build()
            {
                Ok(c) => {
                    rtc.add_local_candidate(c);
                }
                Err(e) => {
                    eprintln!(
                        "[display/webrtc] failed to add TCP host candidate {advertised}: {e}"
                    );
                }
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
        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| CallerError::WebRtc(format!("parse offer: {e}")))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| CallerError::WebRtc(format!("accept offer: {e}")))?;
        let answer_sdp = answer.to_sdp_string();
        // Dump every a=candidate line from the answer so we can see exactly
        // what str0m emitted — this is the fastest way to diagnose
        // "browser never tries to connect to the TCP candidate" symptoms.
        for line in answer_sdp.lines().filter(|l| l.starts_with("a=candidate:")) {
            eprintln!("[display/webrtc] peer {peer_id}: answer {line}");
        }

        // --- Spawn the driver --------------------------------------------
        let (encoded_frame_tx, encoded_frame_rx) =
            mpsc::channel::<Arc<EncodedFrame>>(ENCODED_FRAME_CHANNEL);
        let (command_tx, command_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL);
        let shutdown = CancellationToken::new();

        tokio::spawn(driver(
            peer_id,
            rtc,
            sockets,
            tcp_conn_rx,
            tcp_fake_local,
            peer_registration,
            encoded_frame_rx,
            command_rx,
            input_handler,
            clipboard_handler,
            shutdown.clone(),
        ));

        Ok((
            Self {
                peer_id,
                encoded_frame_tx,
                command_tx,
                shutdown,
            },
            answer_sdp,
        ))
    }

    /// Returns the sender side of this peer's encoded-frame channel.
    ///
    /// The encoder fan-out task pushes `Arc<EncodedFrame>` via `try_send`;
    /// frames are dropped (with metrics) when the driver is behind.
    pub fn encoded_frame_tx(&self) -> &mpsc::Sender<Arc<EncodedFrame>> {
        &self.encoded_frame_tx
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

    /// Add a trickle ICE candidate from the remote peer.
    ///
    /// The browser sends `{candidate, sdpMid, sdpMLineIndex}`; we only need
    /// the `candidate` string (RFC 5245 format) for str0m.
    ///
    /// Browsers obfuscate host candidates as mDNS `.local` hostnames. str0m's
    /// candidate parser only accepts literal IP addresses, so we resolve the
    /// hostname via the system resolver (nss-mdns / Avahi on Linux, Bonjour
    /// on macOS) and rewrite the candidate string before forwarding to the
    /// driver. Candidates that already contain a literal IP pass through
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

/// State the driver carries between iterations.
struct DriverState {
    /// Mid of the outbound video media. Set on `Event::MediaAdded`.
    video_mid: Option<Mid>,
    /// Negotiated payload type for the video media.
    video_pt: Option<Pt>,
    /// Codec configured at construction (used to filter PT selection).
    video_codec: Codec,
    /// Map of channel label → ChannelId for routing channel data and clipboard sends.
    channels: HashMap<String, ChannelId>,
    /// Wallclock anchor: Instant at which the first frame was emitted.
    /// All subsequent rtp_time values are relative to this.
    first_frame_at: Option<Instant>,
    /// True once we've written at least one keyframe to this peer. Until this
    /// is true we drop all encoded frames — a peer that joins mid-stream
    /// can't decode the first delta frames and would otherwise show a black
    /// or garbage screen until the encoder's next periodic IDR.
    keyframe_seen: bool,
}

/// Inbound packet from one of the per-interface forwarder tasks or a
/// TCP connection reader. `proto` tags which transport it arrived on so
/// the driver can hand it to str0m with the correct metadata.
struct InboundPacket {
    proto: Protocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

/// The outbound write-half of a TCP connection. The driver stores one per
/// connection (keyed by the remote's source address) so it can route
/// `Output::Transmit { proto: Tcp, destination, .. }` to the right socket.
/// Writes are serialized through an inner `tokio::Mutex` so concurrent
/// `write_rfc4571_frame` calls can't interleave frame bytes.
type TcpWriter = Arc<AsyncMutex<tokio::net::tcp::OwnedWriteHalf>>;

#[allow(clippy::too_many_arguments)]
async fn driver(
    peer_id: PeerId,
    mut rtc: Rtc,
    sockets: Vec<Arc<UdpSocket>>,
    mut tcp_conn_rx: Option<mpsc::Receiver<AcceptedTcpConnection>>,
    tcp_fake_local: Option<SocketAddr>,
    _tcp_registration: Option<PeerRegistration>,
    mut frame_rx: mpsc::Receiver<Arc<EncodedFrame>>,
    mut command_rx: mpsc::Receiver<Command>,
    input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    shutdown: CancellationToken,
) {
    let video_codec = first_enabled_video_codec(&rtc).unwrap_or(Codec::Vp8);
    let mut state = DriverState {
        video_mid: None,
        video_pt: None,
        video_codec,
        channels: HashMap::new(),
        first_frame_at: None,
        keyframe_seen: false,
    };

    // Index sockets by their local address so we can route Output::Transmit
    // through the socket whose source matches.
    let mut sockets_by_addr: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    for sock in &sockets {
        if let Ok(addr) = sock.local_addr() {
            sockets_by_addr.insert(addr, Arc::clone(sock));
        }
    }

    // Outbound write halves for each active ICE-TCP connection, keyed by the
    // remote's `SocketAddr` (the transmit.destination str0m emits for TCP).
    let mut tcp_writers: HashMap<SocketAddr, TcpWriter> = HashMap::new();

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
        forwarder_handles.push(tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUF_LEN];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    recv = sock.recv_from(&mut buf) => match recv {
                        Ok((n, source)) => {
                            let pkt = InboundPacket {
                                proto: Protocol::Udp,
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

    loop {
        // 1. Drain all outputs until we get a Timeout (the next deadline).
        let timeout_at = match drain_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut tcp_writers,
            &mut state,
            &input_handler,
            &clipboard_handler,
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
            Err(DriverExit::Error(e)) => {
                eprintln!("[display/webrtc] peer {peer_id}: rtc error {e:?}, exiting");
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
                rtc.disconnect();
                eprintln!("[display/webrtc] peer {peer_id}: shutdown requested");
                for h in forwarder_handles {
                    let _ = h.await;
                }
                return;
            }
            Some(pkt) = inbound_rx.recv() => {
                let contents: DatagramRecv = match pkt.bytes.as_slice().try_into() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let input = Input::Receive(
                    pkt.received_at,
                    Receive {
                        proto: pkt.proto,
                        source: pkt.source,
                        destination: pkt.destination,
                        contents,
                    },
                );
                if let Err(e) = rtc.handle_input(input) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_input(Receive) failed: {e:?}"
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
                // We "lie" to str0m about the destination address: every
                // inbound TCP frame gets `destination = tcp_fake_local`
                // (the 127.0.0.1:<http_port> we advertised as our single
                // TCP candidate), not the actual `stream.local_addr()` —
                // which on a NAT'd VM is the VM's internal interface IP
                // that str0m has no candidate for. Matching the fake
                // destination to the one local candidate we advertised
                // lets ICE form a valid pair. The underlying TCP stream
                // is bidirectional so data still flows through the real
                // kernel socket we own.
                let AcceptedTcpConnection {
                    remote_addr,
                    local_addr: real_local,
                    first_frame,
                    stream,
                } = accepted;
                let Some(fake_local) = tcp_fake_local else {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: TCP connection from {remote_addr} but no fake local configured, dropping"
                    );
                    continue;
                };
                eprintln!(
                    "[display/webrtc] peer {peer_id}: ICE-TCP connection from {remote_addr} → {real_local} (str0m sees {fake_local})"
                );
                let (read_half, write_half) = stream.into_split();
                let writer: TcpWriter = Arc::new(AsyncMutex::new(write_half));
                tcp_writers.insert(remote_addr, Arc::clone(&writer));

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
                                        proto: Protocol::Tcp,
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

                // Inject the first frame we peeked off the wire so str0m
                // processes the STUN binding request we used to route.
                let contents: DatagramRecv = match first_frame.as_slice().try_into() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "[display/webrtc] first ICE-TCP frame from {remote_addr} not a valid datagram: {e:?}"
                        );
                        continue;
                    }
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Tcp,
                        source: remote_addr,
                        destination: fake_local,
                        contents,
                    },
                );
                if let Err(e) = rtc.handle_input(input) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_input(first TCP frame) failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            _ = tokio::time::sleep(timeout_dur) => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_input(Timeout) failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(frame) = frame_rx.recv() => {
                write_video_frame(&mut rtc, &mut state, &frame);
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_input(Timeout after frame) failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(cmd) = command_rx.recv() => {
                handle_command(&mut rtc, &state, cmd);
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_input(Timeout after command) failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
        }
    }
}

enum DriverExit {
    Closed,
    Error(RtcError),
}

/// Drain `rtc.poll_output()` until it yields `Output::Timeout`, sending any
/// `Transmit` outputs over the socket whose local address matches the
/// transmit's `source`, and dispatching `Event` outputs through the handlers
/// and into the driver state.
async fn drain_outputs(
    rtc: &mut Rtc,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_writers: &mut HashMap<SocketAddr, TcpWriter>,
    state: &mut DriverState,
    input_handler: &Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: &Arc<dyn Fn(ClipboardContent) + Send + Sync>,
) -> Result<Instant, DriverExit> {
    loop {
        if !rtc.is_alive() {
            return Err(DriverExit::Closed);
        }
        match rtc.poll_output() {
            Ok(Output::Timeout(t)) => return Ok(t),
            Ok(Output::Transmit(t)) => {
                // Routability filtering only applies to UDP: for UDP we
                // need the kernel's `sendto` to succeed from our bound
                // socket, and a loopback-source→routable-destination (or
                // family mismatch) pair would be rejected with EINVAL
                // and waste syscalls. For TCP the connection is already
                // established and we own the stream directly, so we can
                // write regardless of what the abstract source/destination
                // addresses would imply at a pure routing layer.
                if matches!(t.proto, Protocol::Udp) {
                    if t.source.is_ipv4() != t.destination.is_ipv4() {
                        continue;
                    }
                    if t.source.ip().is_loopback() != t.destination.ip().is_loopback() {
                        continue;
                    }
                }
                match t.proto {
                    Protocol::Udp => {
                        let Some(sock) = sockets_by_addr.get(&t.source) else {
                            eprintln!(
                                "[display/webrtc] UDP transmit from unknown source {}, dropping",
                                t.source
                            );
                            continue;
                        };
                        if let Err(e) = sock.send_to(&t.contents, t.destination).await {
                            eprintln!(
                                "[display/webrtc] udp send {} → {} failed: {e}",
                                t.source, t.destination
                            );
                        }
                    }
                    Protocol::Tcp => {
                        // Look up the established TCP connection by remote
                        // address (str0m's Transmit.destination for TCP is
                        // the peer we received the connection from). If the
                        // writer is gone — connection was closed — drop
                        // the packet silently; str0m will time out the
                        // candidate pair and try another.
                        let Some(writer) = tcp_writers.get(&t.destination).cloned() else {
                            continue;
                        };
                        let contents: Vec<u8> = (*t.contents).to_vec();
                        tokio::spawn(async move {
                            let mut guard = writer.lock().await;
                            if let Err(e) = write_rfc4571_frame(&mut *guard, &contents).await {
                                eprintln!(
                                    "[display/webrtc] tcp write failed: {e}"
                                );
                            }
                        });
                    }
                    _ => {
                        // SslTcp / Tls — we don't advertise these, so str0m
                        // shouldn't ever ask us to send via them.
                        eprintln!(
                            "[display/webrtc] unexpected transmit proto {:?}, dropping",
                            t.proto
                        );
                    }
                }
            }
            Ok(Output::Event(event)) => {
                handle_event(rtc, state, event, input_handler, clipboard_handler);
            }
            Err(e) => return Err(DriverExit::Error(e)),
        }
    }
}

fn handle_event(
    rtc: &mut Rtc,
    state: &mut DriverState,
    event: Event,
    input_handler: &Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: &Arc<dyn Fn(ClipboardContent) + Send + Sync>,
) {
    match event {
        Event::IceConnectionStateChange(s) => {
            eprintln!("[display/webrtc] ICE: {s:?}");
            // Disconnected is recoverable per RFC 8445 — ICE may resume
            // checking candidates and find a new working pair. Stay in the
            // driver loop and let str0m retry. The browser-side WebRTC
            // implementation will close the peer connection cleanly if it
            // gives up first; we'll see that as Closed and exit naturally.
        }
        Event::MediaAdded(MediaAdded { mid, kind, .. }) => {
            if kind == MediaKind::Video {
                state.video_mid = Some(mid);
                if let Some(writer) = rtc.writer(mid) {
                    state.video_pt = writer
                        .payload_params()
                        .find(|p| p.spec().codec == state.video_codec)
                        .map(|p| p.pt());
                }
                eprintln!(
                    "[display/webrtc] video media added: mid={mid:?} pt={:?}",
                    state.video_pt
                );
            }
        }
        Event::ChannelOpen(cid, label) => {
            eprintln!("[display/webrtc] data channel open: {label}");
            state.channels.insert(label, cid);
        }
        Event::ChannelClose(cid) => {
            state.channels.retain(|_, v| *v != cid);
        }
        Event::ChannelData(data) => {
            let label = state
                .channels
                .iter()
                .find_map(|(k, v)| (*v == data.id).then(|| k.clone()));
            match label.as_deref() {
                Some("control") | Some("pointer") => {
                    if let Ok(text) = std::str::from_utf8(&data.data) {
                        if let Ok(evt) = serde_json::from_str::<InputEvent>(text) {
                            input_handler(evt);
                        }
                    }
                }
                Some("clipboard") => {
                    if let Ok(text) = std::str::from_utf8(&data.data) {
                        if let Some(content) = parse_clipboard_set(text) {
                            clipboard_handler(content);
                        }
                    }
                }
                _ => {}
            }
        }
        Event::KeyframeRequest(_) => {
            // Browsers periodically request keyframes via PLI/FIR. Without a
            // back-channel to the encoder we can't force one on demand, but
            // the encoder produces keyframes periodically anyway. Future work
            // could plumb this back to the encoder thread.
        }
        _ => {}
    }
}

fn write_video_frame(rtc: &mut Rtc, state: &mut DriverState, frame: &EncodedFrame) {
    let (Some(mid), Some(pt)) = (state.video_mid, state.video_pt) else {
        return;
    };
    // Wait for a keyframe before forwarding anything: a peer that joins
    // mid-stream can't decode delta frames and would otherwise see black
    // until the encoder's next periodic IDR.
    if !state.keyframe_seen {
        if !frame.is_keyframe {
            return;
        }
        state.keyframe_seen = true;
    }
    let now = Instant::now();
    if state.first_frame_at.is_none() {
        state.first_frame_at = Some(now);
    }
    let anchor = state.first_frame_at.unwrap();
    let elapsed_ms = now.duration_since(anchor).as_millis() as u64;
    let media_time = MediaTime::from_90khz(elapsed_ms.saturating_mul(90));
    let Some(writer) = rtc.writer(mid) else {
        return;
    };
    if let Err(e) = writer.write(pt, now, media_time, frame.data.clone()) {
        eprintln!("[display/webrtc] writer.write failed: {e:?}");
    }
}

fn handle_command(rtc: &mut Rtc, state: &DriverState, cmd: Command) {
    match cmd {
        Command::AddIceCandidate(s) => match Candidate::from_sdp_string(&s) {
            Ok(c) => rtc.add_remote_candidate(c),
            Err(e) => eprintln!("[display/webrtc] parse remote candidate failed: {e}"),
        },
        Command::SendClipboard(content) => {
            let Some(cid) = state.channels.get("clipboard").copied() else {
                return;
            };
            let Some(mut channel) = rtc.channel(cid) else {
                return;
            };
            let json = serialize_clipboard(&content);
            if let Err(e) = channel.write(false, json.as_bytes()) {
                eprintln!("[display/webrtc] clipboard channel write failed: {e:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk the Rtc's enabled codec list and return the first video codec.
/// We configured exactly one codec at construction; this finds it.
fn first_enabled_video_codec(rtc: &Rtc) -> Option<Codec> {
    rtc.codec_config()
        .params()
        .iter()
        .map(|p| p.spec().codec)
        .find(|c| c.is_video())
}

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

/// Enumerate routable local IP addresses to advertise as host candidates.
///
/// Includes loopback so localhost connections work. Excludes link-local v6.
fn routable_local_addrs() -> Vec<IpAddr> {
    let mut out = Vec::new();
    // Always include loopback for same-machine connections.
    out.push(IpAddr::V4(Ipv4Addr::LOCALHOST));

    // Walk interfaces via getifaddrs. We use the libc dep that's already in
    // the tree.
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        unsafe {
            let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
                let mut cur = ifap;
                while !cur.is_null() {
                    let ifa = &*cur;
                    if !ifa.ifa_addr.is_null() {
                        let family = (*ifa.ifa_addr).sa_family as i32;
                        let name = if ifa.ifa_name.is_null() {
                            String::new()
                        } else {
                            CStr::from_ptr(ifa.ifa_name)
                                .to_string_lossy()
                                .into_owned()
                        };
                        if family == libc::AF_INET {
                            let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                            let octets = (*sin).sin_addr.s_addr.to_ne_bytes();
                            let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                out.push(IpAddr::V4(ip));
                            }
                        } else if family == libc::AF_INET6 {
                            let sin6 = ifa.ifa_addr as *const libc::sockaddr_in6;
                            let segs = (*sin6).sin6_addr.s6_addr;
                            let ip = std::net::Ipv6Addr::from(segs);
                            // Skip link-local (fe80::/10) and loopback.
                            if !ip.is_loopback() && !ip.is_unspecified() && !is_link_local_v6(&ip)
                            {
                                out.push(IpAddr::V6(ip));
                            }
                        }
                        let _ = name; // currently unused; kept for future filtering
                    }
                    cur = (*cur).ifa_next;
                }
                libc::freeifaddrs(ifap);
            }
        }
    }
    out
}

#[cfg(unix)]
fn is_link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    let segs = ip.segments();
    (segs[0] & 0xffc0) == 0xfe80
}

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
    let resolved = iter.next().ok_or_else(|| format!("no addrs for {addr_field}"))?;
    let ip_str = resolved.ip().to_string();
    fields[4] = &ip_str;
    Ok(fields.join(" "))
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
}
