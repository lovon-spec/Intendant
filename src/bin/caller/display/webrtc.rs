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
//! uses (tagged with `Protocol::Tcp`), and outbound `Output::Transmit`
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
use super::{EncodedFrame, IceConfig, InputEvent, PeerId};
use crate::error::CallerError;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::{CodecSpec, FormatParams, PayloadParams};
use str0m::media::{Frequency, MediaAdded, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{DatagramRecv, Protocol, Receive};
use str0m::net::TcpType;
use str0m::{Candidate, Event, IceCreds, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex};
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

        let username = parse_stun_username(&first_frame).ok_or_else(|| {
            "first frame is not a STUN binding request with USERNAME".to_string()
        })?;

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
        self.registry.registry.lock().unwrap().remove(&self.local_ufrag);
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
        let username = parse_stun_username(&first_frame).ok_or_else(|| {
            "first frame is not a STUN binding request with USERNAME".to_string()
        })?;
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
/// distinct from str0m's typical values to avoid collision; `priority`
/// is set lower than a typical host-TCP-passive candidate so ICE
/// prefers the peer's direct candidate when reachable and only falls
/// back to the relay when direct fails.
///
/// IPv6 addresses are emitted in canonical form (str0m accepts the
/// same); IPv4 addresses as dotted-quad. `component_id` is always 1
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
    // candidate (str0m picks type_pref 126 for those). local_pref
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
    // Foundation 9001 is arbitrary; picked to not collide with str0m's
    // typical sequential foundations (1, 2, ...). Same foundation for
    // every injected candidate is fine per RFC 5245 since foundations
    // only need to be unique-per-stream within a single side's set.
    let candidate_line =
        format!("a=candidate:9001 {component_id} tcp {priority} {ip} {port} typ host tcptype passive generation 0");

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
        if !inserted && line.trim_end_matches(|c| c == '\r' || c == '\n').starts_with("a=candidate:") {
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
    /// `true` when this peer was constructed via [`Self::new_pool_mode`]
    /// (i.e. it gets frames via a per-peer `pool_frame_intake` task,
    /// not via the legacy single-encoder fan-out at
    /// `display/mod.rs:1118`). The fan-out checks this and skips
    /// pool-mode peers — without that skip, pool-mode peers would
    /// receive frames from BOTH the legacy encoder AND the pool's
    /// intake, producing duplicate RTP samples and corrupted decode.
    ///
    /// Phase 3c.4 deletes the legacy fan-out entirely; this field
    /// then becomes vestigial and gets removed alongside the legacy
    /// path.
    pool_mode: bool,
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
    /// 1. Build an [`Rtc`] with each codec in `codec_set` enabled — str0m
    ///    then negotiates from the browser's offer against those codecs.
    /// 2. Bind a per-peer UDP socket and register it as a host candidate.
    /// 3. Synchronously generate the SDP answer via `accept_offer`.
    /// 4. Spawn the driver task and return.
    ///
    /// ## `codec_set` contract
    ///
    /// Each peer gets its own [`Rtc`] (str0m does not support per-peer
    /// codec selection inside one `Rtc`), so codec enablement is a
    /// per-peer decision. The caller passes the codecs this peer should
    /// be allowed to negotiate — typically the intersection of "what the
    /// encoder pipeline can currently produce" with "what the peer's
    /// offer advertised." An empty set is rejected up front; unknown
    /// codecs return an explicit error so the failure point is not
    /// buried inside str0m's accept_offer.
    ///
    /// Empty / no-overlap cases are surfaced to `handle_offer` as
    /// [`CallerError::WebRtc`] errors rather than producing a silent
    /// broken stream — matches the "no compatible codec, clean reject"
    /// contract from the multi-viewer redesign.
    ///
    /// `ice_tx` is accepted for API parity with the previous webrtc-rs
    /// implementation but is currently unused: str0m emits its host candidates
    /// inline in the answer SDP, so there is nothing to trickle from the
    /// server side. The browser still trickles its candidates via
    /// `add_ice_candidate`.
    pub async fn new(
        peer_id: PeerId,
        offer_sdp: &str,
        codec_set: &[CodecKind],
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

        // --- Build the Rtc with the per-peer codec set enabled ------------
        // Previously the session locked one codec at the first peer's offer
        // and every subsequent peer was pre-restricted to it. Now each peer's
        // Rtc enables the codec set its handler chose based on this peer's
        // own offer, so str0m's accept_offer handles SDP negotiation cleanly
        // against the browser's actual preferences.
        if codec_set.is_empty() {
            return Err(CallerError::WebRtc(
                "peer codec set is empty — caller must ensure at least one \
                 encoder codec overlaps with the peer's offer before \
                 constructing WebRtcPeer"
                    .to_string(),
            ));
        }
        let mut config = RtcConfig::new()
            .clear_codecs()
            .set_local_ice_credentials(ice_creds);
        for codec in codec_set {
            config = match codec {
                CodecKind::Vp8 => config.enable_vp8(true),
                CodecKind::H264 => config.enable_h264(true),
                // VP9 / AV1 wiring to str0m lands with phase 3 of the
                // encoder-pool redesign. Reject explicitly so a stray
                // caller doesn't get silent codec loss.
                CodecKind::Vp9 | CodecKind::Av1 => {
                    return Err(CallerError::WebRtc(format!(
                        "codec {} not yet wired to str0m enable API",
                        codec
                    )));
                }
            };
        }
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
        // WebRTC needs loopback so a browser on the same machine can
        // pair against the daemon's host candidates.
        let local_addrs = crate::lan::routable_local_addrs(true);
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
            tcp_advertised,
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
                // Legacy single-encoder fan-out path. The fan-out at
                // `display/mod.rs:1118` will push frames into this
                // peer's `encoded_frame_tx`. `new_pool_mode` flips
                // this to `true` after delegating to `Self::new`.
                pool_mode: false,
            },
            answer_sdp,
        ))
    }

    /// Returns whether this peer was constructed via [`Self::new_pool_mode`].
    /// The legacy fan-out checks this and skips pool-mode peers so
    /// they don't receive frames from both the legacy encoder AND
    /// their per-peer pool intake task.
    pub fn is_pool_mode(&self) -> bool {
        self.pool_mode
    }

    /// Returns the sender side of this peer's encoded-frame channel.
    ///
    /// The encoder fan-out task pushes `Arc<EncodedFrame>` via `try_send`;
    /// frames are dropped (with metrics) when the driver is behind.
    pub fn encoded_frame_tx(&self) -> &mpsc::Sender<Arc<EncodedFrame>> {
        &self.encoded_frame_tx
    }

    /// Pool-mode constructor: builds the same str0m peer as
    /// [`Self::new`] but feeds frames from the shared
    /// [`EncoderPool`] rather than the legacy single-encoder fan-out.
    /// Used by the env-gated `INTENDANT_DISPLAY_POOL` path
    /// (3c.3b.3 onward); legacy callers continue to use [`Self::new`]
    /// until the pre-pool pipeline is deleted in 3c.4.
    ///
    /// `codec_set` is derived from `subscriptions` rather than from
    /// the original peer offer prefs — this is the contract that
    /// makes the 3c.3b.1a partial-result discussion safe in the
    /// other direction: the SDP we negotiate enables exactly the
    /// codecs the pool can serve, so the peer can never select a
    /// codec we'll silently drop frames for. Empty subscriptions
    /// upstream means the offer handler should reject before
    /// reaching here; we forward the empty case as a clean
    /// `WebRtc("empty subscription set")` rather than silently
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
    /// (peer is slow). Mirrors the legacy single-encoder fan-out's
    /// `peer_drops` counter at `display/mod.rs:1120`. Callers should
    /// share this counter with their metrics aggregation so the
    /// `peer_drops` field on `DisplayMetricsSnapshot` continues to
    /// reflect total drops across pre-pool and pool paths during the
    /// 3c.3b.3 → 3c.4 cutover. Tests can pass a fresh
    /// `Arc::new(AtomicU64::new(0))` and inspect it directly.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_pool_mode(
        peer_id: PeerId,
        offer_sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<TcpPeerRegistry>>,
        tcp_advertised_addr: Option<SocketAddr>,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        pool: Arc<EncoderPool>,
        subscriptions: Vec<EncoderSubscription>,
        lease: PoolLease,
        prefs: PeerCodecPreferences,
        drops_counter: Arc<AtomicU64>,
    ) -> Result<(Self, String), CallerError> {
        if subscriptions.is_empty() {
            return Err(CallerError::WebRtc(
                "new_pool_mode: empty subscription set — offer handler must \
                 reject before reaching here"
                    .to_string(),
            ));
        }
        let codec_set = codec_set_from_subscriptions(&subscriptions);
        // Filter the peer's original prefs against codec_set BEFORE
        // building the answer — both must agree exactly. The answer
        // enables `codec_set` (via `enable_*()` calls in `Self::new`),
        // and the intake uses `negotiated_prefs` for every subsequent
        // `pool.subscribe`. They derive from the same source so they
        // can't drift.
        let negotiated_prefs = filter_prefs_to_negotiated(&prefs, &codec_set);
        // Defensive — should be unreachable: subscriptions is non-empty
        // (early return above), codec_set is non-empty (one entry per
        // unique codec in subs), and codec_set ⊆ original prefs (the
        // pool only returns subs for codecs the prefs include). So the
        // intersection is non-empty whenever original prefs is
        // non-empty. If it's empty here, something upstream is producing
        // subs for a codec the prefs doesn't include — fail loud.
        if negotiated_prefs.is_empty() {
            return Err(CallerError::WebRtc(
                "new_pool_mode: filter_prefs_to_negotiated produced empty set; \
                 pool returned subscriptions for codecs not in peer prefs"
                    .to_string(),
            ));
        }
        let (mut peer, answer_sdp) = Self::new(
            peer_id,
            offer_sdp,
            &codec_set,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            input_handler,
            clipboard_handler,
            ice_tx,
        )
        .await?;
        // Mark this peer as pool-mode so the legacy single-encoder
        // fan-out at `display/mod.rs:1118` skips it. Without this,
        // a pool-mode peer would receive frames from BOTH the legacy
        // encoder (via `encoded_frame_tx` fed by the fan-out) AND
        // the pool's per-peer intake task — duplicate RTP samples,
        // corrupted decode, black/garbage stream.
        peer.pool_mode = true;

        // Spawn the intake task. It clones the encoded_frame_tx and
        // shutdown so it can push frames into the existing driver and
        // exit when the peer is torn down. The task owns the lease
        // and resubscribes as needed (see `pool_frame_intake` for
        // the Closed-handling contract).
        let intake_tx = peer.encoded_frame_tx.clone();
        let intake_shutdown = peer.shutdown.clone();
        tokio::spawn(pool_frame_intake(
            pool,
            negotiated_prefs,
            subscriptions,
            lease,
            intake_tx,
            drops_counter,
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

/// Per-[`crate::display::encode::PayloadSpec`] negotiation + readiness
/// state. Keyed off the full `PayloadSpec` in [`DriverState::video_specs`]
/// so H.264 fmtp variants (profile-level-id + packetization-mode) stay
/// distinct — str0m negotiates those independently and caching by
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
    /// `Writer::match_params` resolved this spec to a peer-negotiated PT.
    /// Use `pt` for `writer.write`; `keyframe_seen` flips to `true` only
    /// after the first **successful** write on this spec (see
    /// `write_video_frame`), so a keyframe from this spec that matches
    /// but fails to write doesn't leave the gate open.
    Resolved {
        pt: Pt,
        /// Has this spec had ≥1 keyframe successfully written to the peer?
        /// Until true, non-keyframe frames drop. Scoped per spec so codec
        /// A's keyframe gate is independent of codec B's.
        keyframe_seen: bool,
    },
    /// `Writer::match_params` returned `None` for this spec. str0m's
    /// negotiation is fixed post-answer, so every future frame carrying
    /// this spec drops without re-asking. A single log line is emitted
    /// on the transition into this state; subsequent frames are silent.
    Unsupported,
}

/// State the driver carries between iterations.
struct DriverState {
    /// Mid of the outbound video media. Set on `Event::MediaAdded`.
    video_mid: Option<Mid>,
    /// Per-`PayloadSpec` resolved PT + keyframe readiness. See [`SpecState`].
    /// Replaces the earlier split `video_pt_cache` + global `keyframe_seen`
    /// (findings #2 in 3c.0a review).
    video_specs: HashMap<crate::display::encode::PayloadSpec, SpecState>,
    /// Map of channel label → ChannelId for routing channel data and clipboard sends.
    channels: HashMap<String, ChannelId>,
    /// Wallclock anchor: Instant at which the first frame was emitted.
    /// All subsequent rtp_time values are relative to this.
    first_frame_at: Option<Instant>,
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
    tcp_advertised: Option<SocketAddr>,
    _tcp_registration: Option<PeerRegistration>,
    mut frame_rx: mpsc::Receiver<Arc<EncodedFrame>>,
    mut command_rx: mpsc::Receiver<Command>,
    input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    shutdown: CancellationToken,
) {
    let mut state = DriverState {
        video_mid: None,
        video_specs: HashMap::new(),
        channels: HashMap::new(),
        first_frame_at: None,
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
                // inbound TCP frame gets `destination = tcp_advertised`
                // (the Host-header-derived IP:port we advertised as our
                // TCP candidate), not the actual `stream.local_addr()` —
                // which on a NAT'd VM is the VM's internal interface IP
                // that str0m has no candidate for. Matching the
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
                // No longer pre-resolves a PT at MediaAdded time: PT
                // resolution has moved to the per-frame path via the
                // `video_pt_cache` + `writer.match_params(...)`. A
                // peer may receive frames from multiple codecs
                // (VP8 always-on + H.264 on-demand + …) and each
                // `PayloadSpec` gets its own cached PT on first hit.
                eprintln!("[display/webrtc] video media added: mid={mid:?}");
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
    let Some(mid) = state.video_mid else {
        return;
    };
    let Some(writer) = rtc.writer(mid) else {
        return;
    };

    // Step 1: resolve the spec's PT via match_params if we haven't yet.
    // Inserts a `SpecState::Resolved { pt, keyframe_seen: false }` on
    // success or `SpecState::Unsupported` on None, so the lookup below
    // always finds an entry. A spec ending in `Unsupported` stays that
    // way — str0m's negotiation is fixed post-answer, so re-matching
    // won't start working.
    if !state.video_specs.contains_key(&frame.payload_spec) {
        let resolved = writer.match_params(payload_spec_to_str0m(&frame.payload_spec));
        let new = match resolved {
            Some(pt) => SpecState::Resolved {
                pt,
                keyframe_seen: false,
            },
            None => {
                eprintln!(
                    "[display/webrtc] no match_params for {} — peer did not \
                     negotiate this codec profile, frames on this spec will \
                     be dropped",
                    frame.payload_spec.codec_mime
                );
                SpecState::Unsupported
            }
        };
        state.video_specs.insert(frame.payload_spec.clone(), new);
    }

    // Step 2: extract pt + current keyframe readiness from the spec state.
    // Copy out immutably so we can mutate `state.first_frame_at` below
    // without borrow conflicts with `state.video_specs`.
    let (pt, keyframe_ready) = match state.video_specs.get(&frame.payload_spec) {
        Some(SpecState::Resolved { pt, keyframe_seen }) => (*pt, *keyframe_seen),
        // Unsupported or (impossibly) missing — drop silently. The
        // first arm already emitted a log on entering Unsupported.
        _ => return,
    };

    // Step 3: per-spec keyframe gate. Closed until this spec has had
    // ≥1 keyframe *successfully written* (see step 5 — the flag flips
    // only after `writer.write` returns Ok). A keyframe from codec A
    // that match_params resolved but that then fails to write does not
    // open the gate for codec A's P-frames, and no keyframe of codec A
    // ever opens codec B's gate.
    if !keyframe_ready && !frame.is_keyframe {
        return;
    }

    // Step 4: wallclock anchor + media time.
    let now = Instant::now();
    if state.first_frame_at.is_none() {
        state.first_frame_at = Some(now);
    }
    let anchor = state.first_frame_at.unwrap();
    let elapsed_ms = now.duration_since(anchor).as_millis() as u64;
    let media_time = MediaTime::from_90khz(elapsed_ms.saturating_mul(90));

    // Step 5: write + on-success gate flip.
    match writer.write(pt, now, media_time, frame.data.clone()) {
        Ok(()) => {
            // Only flip keyframe_seen for this spec AFTER a successful
            // write (findings #2 in 3c.0a review). If the write is the
            // first keyframe on this spec, the gate opens for subsequent
            // P-frames. If it wasn't a keyframe (gate was already open),
            // this is a no-op.
            if !keyframe_ready {
                if let Some(SpecState::Resolved { keyframe_seen, .. }) =
                    state.video_specs.get_mut(&frame.payload_spec)
                {
                    *keyframe_seen = true;
                }
            }
        }
        Err(e) => eprintln!("[display/webrtc] writer.write failed: {e:?}"),
    }
}

/// Construct a str0m [`PayloadParams`] from our abstract
/// [`crate::display::encode::PayloadSpec`] suitable for
/// `Writer::match_params`. The incoming `pt` field is a placeholder
/// (match_params uses the spec for comparison, not the incoming PT),
/// so we hand it any valid video PT — 96 is the convention for VP8.
fn payload_spec_to_str0m(spec: &crate::display::encode::PayloadSpec) -> PayloadParams {
    use str0m::format::Codec as StrCodec;
    let codec = match spec.codec_mime {
        crate::display::encode::MIME_TYPE_VP8 => StrCodec::Vp8,
        crate::display::encode::MIME_TYPE_H264 => StrCodec::H264,
        "video/VP9" => StrCodec::Vp9,
        "video/AV1" => StrCodec::Av1,
        _ => StrCodec::Unknown,
    };
    let mut format = FormatParams::default();
    // H.264 fmtp fields — populated only when both sides carry the
    // relevant values. `profile_level_id` on the str0m side is a u32
    // (6 hex digits packed into the low 24 bits); ours is a string
    // ("42e01f" etc) for readability. Parse on the fly.
    if let Some(plid_str) = spec.h264_profile_level_id.as_deref() {
        if let Ok(plid_u32) = u32::from_str_radix(plid_str, 16) {
            format.profile_level_id = Some(plid_u32);
        }
    }
    if let Some(pm) = spec.h264_packetization_mode {
        format.packetization_mode = Some(pm as u8);
    }
    let clock_rate = Frequency::NINETY_KHZ;
    let codec_spec = CodecSpec {
        codec,
        clock_rate,
        channels: None,
        format,
    };
    // The `pt` we pass here is a placeholder: `Writer::match_params`
    // scores incoming `PayloadParams` against the peer's negotiated
    // set by codec/format/etc., not by PT. Pick 96 (conventional
    // first dynamic PT) so the return value is a valid PayloadParams
    // even though the PT itself is unused downstream.
    PayloadParams::new(Pt::new_with_value(96), None, codec_spec)
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

// `first_enabled_video_codec` was removed as part of the 3c.0 contract
// — codec selection happens per-frame via the `video_pt_cache` +
// `writer.match_params(...)` path, not pre-locked at driver init.
// The peer's Rtc can have multiple video codecs enabled (VP8 + H.264
// + …) and each frame resolves to its own PT.

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
    let resolved = iter.next().ok_or_else(|| format!("no addrs for {addr_field}"))?;
    let ip_str = resolved.ip().to_string();
    fields[4] = &ip_str;
    Ok(fields.join(" "))
}

// ---------------------------------------------------------------------------
// Pool-mode helpers (3c.3b.2)
// ---------------------------------------------------------------------------

/// Distinct codecs covered by `subscriptions`, deduplicated. Used by
/// [`WebRtcPeer::new_pool_mode`] to drive the str0m enable_*() calls
/// from the actually-served set rather than from the original peer
/// offer prefs — the SDP we negotiate is exactly what the pool
/// commits to producing, so there's no path where the peer picks an
/// unsupported codec and gets a black stream.
///
/// Order is preserved as encountered (CodecKind isn't `Ord`, and
/// str0m's `enable_*()` calls are commutative — order has no
/// observable effect on the negotiated answer). Dedup avoids
/// calling `enable_vp8(true)` twice on a multi-layer simulcast set.
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

/// Build the **negotiated** codec preferences the intake uses for
/// every `pool.subscribe` call (initial AND every resubscribe).
///
/// Filters `original_prefs` against the codec set actually returned
/// by the initial subscribe (`actual_codecs`). Preserves
/// `original_prefs` ordering — the intake's
/// [`select_active_subscription`] uses prefs order as the
/// preference signal, and re-ordering would silently change the
/// peer's chosen codec.
///
/// **Why this matters (3c.3b.2b finding 1):** the peer's SDP answer
/// is built from `actual_codecs` (via `codec_set_from_subscriptions`),
/// not from `original_prefs`. If `original_prefs = [VP8, H.264]` but
/// initial subscribe returned only `[VP8]` (because H.264 encoder
/// construction failed at that moment — VAAPI exhaustion, ffmpeg
/// missing, etc.), the answer enables only VP8. Resubscribing with
/// `original_prefs` after a later resize could pick H.264 if it
/// became available — and the peer's WebRTC sender would then
/// `match_params` against a PT cache that has no H.264 entry, mark
/// the spec `Unsupported` per the 3c.0a per-spec gate, and silently
/// drop every frame. Locking the resubscribe prefs to
/// `actual_codecs` makes that reachability bug impossible.
///
/// Returns an empty `PeerCodecPreferences` only if the intersection
/// is empty, which the caller (`new_pool_mode`) prevents by erroring
/// upstream when `subscriptions` is empty (codec_set non-empty →
/// intersection non-empty when `original_prefs` is non-empty). The
/// upstream contract is asserted by the early-return at the top of
/// `new_pool_mode`.
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

/// Pick the single subscription the intake should forward from, given
/// the peer's prefs and the set the pool returned.
///
/// Selection rule, in priority order:
///   1. **First codec in `prefs.supported`** that has any matching
///      subscription. Prefs order encodes the peer's codec preference
///      from its SDP offer (`codec_preferences_from_offer`), so this
///      respects the peer's own ranking.
///   2. **Within that codec**, prefer the [`SimulcastRid::full`] layer.
///      Pre-phase-4 the pool only publishes the full layer per codec;
///      phase 4 will replace this with TWCC-driven layer choice. Until
///      then, full is the right baseline — peers that need lower
///      bitrate hit the encoder's existing rate-control rather than
///      a separate layer.
///   3. **Fall back to any RID for the chosen codec** if full isn't
///      present. Defensive: today this never fires (pre-phase-4 always
///      publishes full), but pinning the contract here makes
///      multi-layer add-on work less surprising.
///
/// Returns `Some` with the chosen subscription removed from
/// `subscriptions` (via `swap_remove`, so order of remaining elements
/// is not preserved — they're about to be dropped anyway). Returns
/// `None` only when no codec in `prefs.supported` has any subscription
/// in the input set, which the caller treats as escalation: with the
/// strict `codec_set_from_subscriptions` contract from 3c.3b.2 the SDP
/// we negotiate enables exactly the codecs the pool committed to, so
/// reaching `None` indicates a pool/peer state divergence the intake
/// can't recover from silently.
///
/// The remaining (unused) subscriptions in `subscriptions` are the
/// caller's to drop. Dropping them releases their `broadcast::Receiver`
/// clones, which in turn lets the corresponding encoder slots see
/// reduced receiver pressure (no functional effect — the slots stay
/// alive via the lease's refcount — but it keeps the channel state
/// minimal and avoids confusing `receiver_count()` accounting in
/// later debug sessions).
fn select_active_subscription(
    subscriptions: &mut Vec<EncoderSubscription>,
    prefs: &PeerCodecPreferences,
) -> Option<EncoderSubscription> {
    let full_rid = SimulcastRid::full();
    for &codec in &prefs.supported {
        // Pass 1: codec + full RID (the canonical pre-phase-4 case).
        if let Some(idx) = subscriptions
            .iter()
            .position(|s| s.id.codec == codec && s.id.rid == full_rid)
        {
            return Some(subscriptions.swap_remove(idx));
        }
        // Pass 2: codec, any RID (defensive — pre-phase-4 unreachable).
        if let Some(idx) = subscriptions.iter().position(|s| s.id.codec == codec) {
            return Some(subscriptions.swap_remove(idx));
        }
    }
    None
}

/// Per-peer task that bridges the [`EncoderPool`]'s per-subscription
/// `broadcast::Receiver<Arc<EncodedFrame>>` channels to the
/// [`WebRtcPeer`] driver's encoded-frame mpsc, and re-subscribes
/// transparently when an encoder slot is torn down (typically by
/// [`EncoderPool::on_resize`] or an on-demand slot's last-leaseholder
/// exit).
///
/// ## Single active subscription per epoch — the 3c.3b.2a contract
///
/// `pool.subscribe(prefs)` may return multiple subscriptions: one per
/// `(codec × layer)` the peer's prefs overlap with. For a peer that
/// supports both VP8 and H.264, that's two subscriptions; for a peer
/// supporting VP8 against a simulcast pool, that's one per layer.
///
/// The intake forwards from **exactly one** of those subscriptions
/// per epoch. Forwarding from all of them into a single per-peer
/// encoded-frame `mpsc` would feed multiple codec streams (or
/// multiple simulcast layers of the same codec) into one WebRTC
/// sender — at best doubling bandwidth, at worst producing
/// codec-interleaved bytes the browser cannot decode and rendering
/// the stream black.
///
/// The active subscription is picked via [`select_active_subscription`]
/// from `negotiated_prefs`'s codec ordering. The unused subscriptions
/// are dropped explicitly so their `broadcast::Receiver` clones release
/// immediately rather than lingering until end-of-scope.
///
/// Dropping a subscription **does not** decrement the encoder's
/// refcount — refcounts live on the [`PoolLease`]. So we additionally
/// call [`PoolLease::release_on_demand_subset`] with the inactive
/// subs' ids: on-demand encoders the peer's active codec doesn't use
/// drop their refcount immediately, and (when the refcount hits zero)
/// the encoder is torn down. Always-on slots have no refcount entry;
/// passing their ids is a silent no-op. The 3c.3b.2a review caught
/// this as a wasted-CPU regression (multi-codec pool with a
/// VP8-preferring peer would keep the H.264 encoder spinning into
/// no-op broadcast until peer disconnect); 3c.3b.2b closed it.
///
/// ## `negotiated_prefs` — the 3c.3b.2b finding-1 contract
///
/// `negotiated_prefs` is the **caller-filtered** subset of the peer's
/// original SDP-offer prefs that intersects the codecs the pool's
/// initial subscribe actually returned. This is the codec set the
/// peer's SDP answer enabled (`new_pool_mode` derives both the answer's
/// `enable_*()` calls AND `negotiated_prefs` from the same
/// `codec_set_from_subscriptions(initial_subs)` source).
///
/// The intake passes `negotiated_prefs` to every `pool.subscribe` —
/// resubscribe-after-Closed included. If we passed the original
/// unfiltered prefs, the resubscribe could return a codec the peer
/// never negotiated (e.g. H.264 construction failed initially but
/// succeeds after a later resize that respawns the on-demand slot).
/// `select_active_subscription` would then pick that codec, the
/// driver would call `match_params` against the negotiated PT cache,
/// the codec wouldn't match, the per-spec gate would mark it
/// `Unsupported`, and every frame would silently drop → black stream.
/// Locking the prefs to the negotiated set at construction time and
/// using that on every resubscribe is the structural fix.
///
/// ## Lossy forwarding — the 3c.3b.2a contract (continued)
///
/// The forwarder uses [`mpsc::Sender::try_send`], not
/// `send().await`. When the driver's bounded encoded-frame mpsc is
/// full (slow peer, network stall, encoder burst), [`try_send`]
/// returns [`mpsc::error::TrySendError::Full`] and the forwarder
/// drops the frame and increments `drops_counter`. Mirrors the
/// legacy single-encoder fan-out at `display/mod.rs:1120` exactly.
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
/// `select_active_subscription` returns `None` against a non-empty
/// subscription set (a contract violation indicating pool/peer
/// divergence) — the intake signals `shutdown.cancel()` so the driver
/// tears the peer down cleanly rather than leaving a never-decoding
/// stream behind.
async fn pool_frame_intake(
    pool: Arc<EncoderPool>,
    negotiated_prefs: PeerCodecPreferences,
    initial_subs: Vec<EncoderSubscription>,
    initial_lease: PoolLease,
    encoded_frame_tx: mpsc::Sender<Arc<EncodedFrame>>,
    drops_counter: Arc<AtomicU64>,
    shutdown: CancellationToken,
) {
    let mut current_lease = Some(initial_lease);
    let mut current_subs = initial_subs;

    'epoch: loop {
        // Pick exactly one subscription. Drop the rest explicitly so
        // their broadcast::Receiver clones release at this scope's
        // statement boundary rather than at end-of-function.
        let active = match select_active_subscription(&mut current_subs, &negotiated_prefs) {
            Some(s) => s,
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
                    current_subs.len(),
                );
                shutdown.cancel();
                return;
            }
        };
        // Collect the inactive subs' ids BEFORE dropping the subs so
        // we can release their on-demand claims (3c.3b.2b finding 2).
        // Always-on slots have no on_demand_refs entry; passing their
        // ids is a silent no-op via `release_on_demand_subset`'s
        // skip-unknown-ids contract. So we don't have to distinguish
        // always-on from on-demand here — just pass everything.
        let inactive_ids: Vec<EncoderId> =
            current_subs.iter().map(|s| s.id.clone()).collect();
        // Make the drop point obvious. Future maintainers reading the
        // function should not have to wonder when the unused subs go
        // away — it's right here, between selection and forwarder
        // spawn. `current_subs` is moved-from after this and is only
        // re-initialized on the resubscribe path
        // (`current_subs = subs` in the EncoderClosed branch below)
        // before the next read at the top of the loop, so no
        // `= Vec::new()` placeholder is needed here.
        drop(current_subs);
        // Release the inactive on-demand claims on the active lease.
        // For a peer with prefs [VP8, H264] against a pool that has
        // VP8 always-on + H264 on-demand, this is what tears down the
        // never-consumed H264 encoder when the active codec is VP8 —
        // without it, H264 keeps encoding into a broadcast channel
        // with no receivers until peer disconnect (the wasted-CPU
        // regression caught in the 3c.3b.2a review).
        if !inactive_ids.is_empty() {
            if let Some(lease) = current_lease.as_mut() {
                lease.release_on_demand_subset(&inactive_ids);
            }
        }

        let active_id = active.id.clone();
        let mut rx = active.frames;
        let frame_tx = encoded_frame_tx.clone();
        let counter = Arc::clone(&drops_counter);
        // Forwarder cancellation. With a single forwarder per epoch
        // this is overkill (we could just await the JoinHandle), but
        // it gives the intake a clean way to interrupt a forwarder
        // that's parked inside `rx.recv()` waiting for the next frame
        // — `shutdown` propagation should not have to wait for the
        // encoder to publish another frame to wake the recv.
        let fwd_shutdown = CancellationToken::new();
        let fwd_shutdown_inner = fwd_shutdown.clone();
        // Exit channel so the intake's outer `select!` can receive
        // the forwarder's exit reason without consuming the
        // `JoinHandle` (the JoinHandle would otherwise be moved by
        // both arms of the select). Capacity 1 is enough — the
        // forwarder sends exactly once on exit.
        let (exit_tx, mut exit_rx) = mpsc::channel::<ForwarderExit>(1);
        let forwarder = tokio::spawn(async move {
            let exit = loop {
                tokio::select! {
                    _ = fwd_shutdown_inner.cancelled() => break ForwarderExit::Cancelled,
                    res = rx.recv() => match res {
                        Ok(frame) => {
                            match frame_tx.try_send(frame) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    // Driver's mpsc is full. Drop the
                                    // frame; the codec's keyframe
                                    // cadence will recover the visual
                                    // stream. Mirrors legacy fan-out
                                    // semantics (display/mod.rs:1120).
                                    counter.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    // Driver receiver dropped. Peer
                                    // is gone; nothing to forward to.
                                    break ForwarderExit::DriverClosed;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Encoder torn down. Intake will
                            // resubscribe (or escalate to peer
                            // shutdown if that fails).
                            break ForwarderExit::EncoderClosed;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Slow consumer; broadcast skipped
                            // ahead. Codec keyframe machinery
                            // (request_keyframe / GOP) recovers.
                            continue;
                        }
                    }
                }
            };
            // Send is best-effort: if the intake is already
            // tearing down (shutdown branch fired and the receiver
            // was dropped), we just exit.
            let _ = exit_tx.send(exit).await;
        });

        tokio::select! {
            _ = shutdown.cancelled() => {
                // Peer is going away. Cancel forwarder, await it,
                // drop the lease, exit.
                fwd_shutdown.cancel();
                let _ = forwarder.await;
                drop(current_lease.take());
                return;
            }
            recv = exit_rx.recv() => {
                // Forwarder reported its exit via the exit channel
                // (and is on the way out — its task body already ran
                // past the loop). `fwd_shutdown.cancel()` here is a
                // defensive no-op; await the JoinHandle so the task's
                // resources are reaped before we move on.
                fwd_shutdown.cancel();
                let _ = forwarder.await;
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
                        // intake would then `match_params` against a
                        // codec the peer never negotiated and the
                        // driver's per-spec gate marks `Unsupported`,
                        // dropping every frame → silent black stream.
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
                                     after encoder Closed (active was {:?})",
                                    active_id,
                                );
                                continue 'epoch;
                            }
                            Err(e) => {
                                eprintln!(
                                    "[display/webrtc/pool-intake] resubscribe \
                                     after Closed failed ({e:?}): no compatible \
                                     codec; signalling peer shutdown"
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
        assert_eq!(parse_sdp_ice_ufrag("v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\n"), None);
        assert_eq!(parse_sdp_ice_ufrag("a=ice-ufrag:\r\n"), None);
        assert_eq!(parse_sdp_ice_ufrag(""), None);
    }

    /// `parse_first_frame_ufrag` extracts the TARGET (server-side)
    /// ufrag from a STUN binding request's USERNAME attribute, which
    /// is the `target:sender` format per RFC 8445.
    #[test]
    fn parse_first_frame_ufrag_picks_target_half() {
        let frame = make_stun_binding_request("peerXYZ:browserABC");
        assert_eq!(
            parse_first_frame_ufrag(&frame).as_deref(),
            Some("peerXYZ")
        );
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
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 197).into(), 8765);
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
            rewritten.contains("192.168.1.197 8765"),
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
    /// (simulcast-style) subscription sets. The downstream str0m
    /// `enable_*(true)` calls would tolerate duplicates, but dedup
    /// keeps the SDP shape clean and avoids subtle effects in
    /// future negotiation logic.
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

    // -----------------------------------------------------------------------
    // Phase 3c.3b.2b: filter_prefs_to_negotiated unit tests
    // -----------------------------------------------------------------------

    /// **3c.3b.2b finding 1 contract.** Filters original prefs against
    /// the codec set actually returned by initial subscribe, preserving
    /// the original ordering. Order matters because
    /// `select_active_subscription` uses prefs order as the codec
    /// preference signal — re-ordering would change which codec the
    /// peer actually receives.
    #[test]
    fn filter_prefs_to_negotiated_preserves_original_order() {
        let original = PeerCodecPreferences::new(vec![
            CodecKind::H264,
            CodecKind::Vp8,
            CodecKind::Vp9,
        ]);
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
    /// upstream (see the `is_empty()` guard in `new_pool_mode`); the
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
    #[tokio::test]
    async fn pool_intake_resubscribes_when_initial_subs_already_closed() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            vec![LayerSpec::single(CodecKind::Vp8, 64, 64, 30)],
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Subscribe AGAINST the original handle.
        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("initial subscribe");

        // Resize: original handle dropped, new one spawned.
        // initial_subs's Receivers will return Closed on first recv.
        pool.on_resize(128, 96);

        let (frame_tx, mut frame_rx) =
            mpsc::channel::<Arc<EncodedFrame>>(16);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
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

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            frame_rx.recv(),
        )
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
    #[tokio::test]
    async fn pool_intake_shuts_down_peer_when_resubscribe_finds_no_codec() {
        use crate::display::encode::pool::EncoderPool;

        // Pool with NO always-on encoders; on-demand only. Subscribe
        // for VP8 (spawns on-demand VP8 slot) to get initial_subs.
        let pool = Arc::new(EncoderPool::new(64, 64, 30, vec![], None));
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
        let (frame_tx, _frame_rx) =
            mpsc::channel::<Arc<EncodedFrame>>(16);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs_unservable,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
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

        let exited = tokio::time::timeout(
            Duration::from_secs(2),
            async {
                while !shutdown.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            },
        )
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
    // Phase 3c.3b.2a: select_active_subscription unit tests + intake contract
    // -----------------------------------------------------------------------

    /// Build an `EncoderSubscription` whose `frames` is a fresh
    /// `broadcast::Receiver`. The Sender is dropped at end-of-scope of
    /// the test; that's fine for `select_active_subscription` tests
    /// which never `recv()` from the receivers — they only inspect IDs.
    fn make_test_subscription(codec: CodecKind, rid: SimulcastRid) -> EncoderSubscription {
        use crate::display::encode::pool::LayerSpec;
        let (s, r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        // Keep the sender alive at least until the receiver is taken.
        // Forgetting the sender keeps the channel open; for unit tests
        // that only read `id` and `layer` from the subscription this is
        // strictly correct (we never call `recv()`). The sender leak is
        // bounded by test lifetime.
        std::mem::forget(s);
        EncoderSubscription {
            id: EncoderId::new(codec, rid),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: r,
        }
    }

    /// First-codec-in-prefs wins. With VP8 and H.264 both available,
    /// prefs `[VP8, H264]` picks VP8 — and with VP8 also at full RID,
    /// the full layer wins over no other layer.
    #[test]
    fn select_active_subscription_picks_first_pref_codec() {
        let mut subs = vec![
            make_test_subscription(CodecKind::H264, SimulcastRid::full()),
            make_test_subscription(CodecKind::Vp8, SimulcastRid::full()),
        ];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let active = select_active_subscription(&mut subs, &prefs)
            .expect("pool returned both codecs; one must be active");
        assert_eq!(active.id.codec, CodecKind::Vp8);
        assert_eq!(active.id.rid, SimulcastRid::full());
        // The unused subscription remains in `subs` for the caller to drop.
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id.codec, CodecKind::H264);
    }

    /// Within a chosen codec, full RID beats half/quarter. With VP8
    /// simulcast the pool returns three subscriptions; we always pick
    /// full pre-phase-4. Phase 4 will replace this rule with
    /// TWCC-driven layer selection.
    #[test]
    fn select_active_subscription_prefers_full_rid_within_codec() {
        let mut subs = vec![
            make_test_subscription(CodecKind::Vp8, SimulcastRid::quarter()),
            make_test_subscription(CodecKind::Vp8, SimulcastRid::half()),
            make_test_subscription(CodecKind::Vp8, SimulcastRid::full()),
        ];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let active = select_active_subscription(&mut subs, &prefs)
            .expect("VP8 in prefs and three VP8 subs; one must be active");
        assert_eq!(active.id.rid, SimulcastRid::full());
        assert_eq!(subs.len(), 2);
    }

    /// Defensive RID fallback: if the chosen codec has no full-RID
    /// subscription (pre-phase-4 unreachable; phase-4-onward could
    /// happen if TWCC pinned a non-full layer at construction time),
    /// pick any RID for that codec rather than dropping to a
    /// less-preferred codec. Pinning the contract here so a future
    /// refactor doesn't accidentally fall through to the next codec.
    #[test]
    fn select_active_subscription_falls_back_to_any_rid_for_chosen_codec() {
        let mut subs = vec![
            make_test_subscription(CodecKind::H264, SimulcastRid::full()),
            make_test_subscription(CodecKind::Vp8, SimulcastRid::half()),
        ];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let active = select_active_subscription(&mut subs, &prefs)
            .expect("VP8 sub at half RID is still a VP8 match");
        assert_eq!(active.id.codec, CodecKind::Vp8);
        assert_eq!(active.id.rid, SimulcastRid::half());
    }

    /// No codec match → `None`. The intake escalates to peer
    /// shutdown rather than silently picking nothing.
    #[test]
    fn select_active_subscription_returns_none_when_no_codec_matches() {
        let mut subs = vec![
            make_test_subscription(CodecKind::Vp8, SimulcastRid::full()),
            make_test_subscription(CodecKind::H264, SimulcastRid::full()),
        ];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp9]);
        let active = select_active_subscription(&mut subs, &prefs);
        assert!(
            active.is_none(),
            "VP9-only prefs against VP8+H.264 subs must return None — \
             escalation path lives in pool_frame_intake"
        );
        // All subs left intact for the caller (not that the caller
        // does anything with them; they'll be dropped on the
        // escalation path).
        assert_eq!(subs.len(), 2);
    }

    /// Empty prefs → `None`. Belt-and-suspenders: prefs always
    /// non-empty in production (codec_preferences_from_offer rejects
    /// empty offers), but the function should still behave sanely.
    #[test]
    fn select_active_subscription_returns_none_for_empty_prefs() {
        let mut subs = vec![
            make_test_subscription(CodecKind::Vp8, SimulcastRid::full()),
        ];
        let prefs = PeerCodecPreferences::new(vec![]);
        assert!(select_active_subscription(&mut subs, &prefs).is_none());
    }

    /// **3c.3b.2a contract: single active subscription per epoch.**
    ///
    /// With VP8 simulcast (3 always-on layers), `pool.subscribe(VP8)`
    /// returns three subscriptions. Pre-3c.3b.2a the intake spawned
    /// one forwarder per subscription, so each i420 frame produced
    /// three encoded frames at the peer's mpsc — at best 3× bandwidth,
    /// at worst codec/layer-interleaved bytes the browser cannot
    /// decode (silent black stream).
    ///
    /// Post-3c.3b.2a: the intake picks the full RID layer and drops
    /// the other two subscriptions. This test pins that by counting
    /// frames received over a fixed window: with one active layer,
    /// the count is roughly equal to input frame count (one encoded
    /// frame per i420 input, modulo encoder packetization). With
    /// three active layers, the count would be ~3× larger.
    ///
    /// We assert "received ≤ 1.5 × input" as the contract; the
    /// hysteresis tolerance is for keyframe-vs-delta duplication
    /// quirks of the encoder. The pre-fix behavior would produce ~3×.
    #[tokio::test]
    async fn pool_intake_forwards_only_one_layer_with_simulcast_set() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            LayerSpec::vp8_simulcast(64, 64, 30),
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("VP8 simulcast subscribe");
        // Pre-condition: pool returned multiple subscriptions. If this
        // assertion fires the simulcast set was dropped to a single
        // layer somewhere upstream and this test no longer exercises
        // the multi-sub case it claims to.
        assert!(
            initial_subs.len() >= 2,
            "test setup expects multiple simulcast layers from \
             vp8_simulcast(); got {}",
            initial_subs.len(),
        );
        let input_count = 12u64;

        let (frame_tx, mut frame_rx) =
            mpsc::channel::<Arc<EncodedFrame>>(256);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            intake_shutdown,
        ));

        // Push frames at the source dimensions. Each i420 buffer
        // arrives at every always-on encoder via the bridge's
        // broadcast; pre-fix behavior would have THREE encoders
        // producing one encoded frame each per input.
        let frame = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..input_count {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        // Drain a short window past the last push so any in-flight
        // encoded frames land.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut received: u64 = 0;
        while frame_rx.try_recv().is_ok() {
            received += 1;
        }

        // The lower bound (>0) catches "intake dropped everything"
        // regressions. The upper bound is the actual contract: with
        // one active layer, count tracks input. Pre-fix would deliver
        // ~3 × input, so 1.5 × leaves clear daylight even with
        // encoder warm-up keyframe-burst quirks.
        assert!(
            received > 0,
            "intake must forward SOMETHING from the active layer — \
             received 0 frames means the subscription was dropped \
             entirely, not just shrunk to one"
        );
        let max_expected = (input_count * 3) / 2; // 1.5x tolerance
        assert!(
            received <= max_expected,
            "intake forwarded {received} frames for {input_count} i420 \
             inputs (max acceptable = {max_expected}); pre-3c.3b.2a \
             behavior would land at ~{} (3× input). Multi-layer fan-out \
             regression?",
            input_count * 3,
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }

    /// **3c.3b.2a contract: lossy forwarding (try_send) parity with
    /// legacy fan-out.**
    ///
    /// The legacy fan-out at `display/mod.rs:1120` uses `try_send` and
    /// drops on `Full`, incrementing `peer_drops`. The intake must
    /// match: a slow peer (full mpsc) sees frames dropped, the
    /// `drops_counter` reflects them, and the forwarder stays
    /// responsive to cancellation.
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
    #[tokio::test]
    async fn pool_intake_drops_lossily_when_driver_mpsc_full() {
        use crate::display::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            vec![LayerSpec::single(CodecKind::Vp8, 64, 64, 30)],
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("VP8 always-on subscribe");

        // Tiny mpsc — fills almost immediately. Keep the receiver
        // alive but never drain it during the push phase.
        let (frame_tx, mut frame_rx) =
            mpsc::channel::<Arc<EncodedFrame>>(1);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let drops_for_intake = Arc::clone(&drops);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            drops_for_intake,
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
        let exited = tokio::time::timeout(
            Duration::from_secs(1),
            intake_handle,
        )
        .await;
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
}
