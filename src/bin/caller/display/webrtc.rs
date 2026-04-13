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
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Bound on the per-peer encoded-frame channel. Frames in excess are dropped
/// with backpressure registered in the display metrics.
const ENCODED_FRAME_CHANNEL: usize = 8;

/// Bound on the per-peer command channel.
const COMMAND_CHANNEL: usize = 32;

/// Maximum UDP datagram we'll receive on the per-peer socket.
const UDP_BUF_LEN: usize = 2000;

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
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        _ice_tx: mpsc::Sender<(PeerId, String)>,
    ) -> Result<(Self, String), CallerError> {
        // --- Build the Rtc with only the negotiated codec enabled ---------
        // The session-level codec selection has already happened in
        // DisplaySession::handle_offer; we restrict str0m's codec set so
        // negotiation can only resolve to that one codec.
        let mut config = RtcConfig::new().clear_codecs();
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
        for iface_addr in routable_local_addrs() {
            let bind_addr = SocketAddr::new(iface_addr, 0);
            let socket = match UdpSocket::bind(bind_addr).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "[display/webrtc] skipping bind on {iface_addr}: {e}"
                    );
                    continue;
                }
            };
            let local = match socket.local_addr() {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "[display/webrtc] skipping socket on {iface_addr}: local_addr {e}"
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
                    eprintln!("[display/webrtc] skipping host candidate {local}: {e}");
                }
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound".to_string(),
            ));
        }

        // --- Parse the offer and produce the answer ----------------------
        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| CallerError::WebRtc(format!("parse offer: {e}")))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| CallerError::WebRtc(format!("accept offer: {e}")))?;
        let answer_sdp = answer.to_sdp_string();

        // --- Spawn the driver --------------------------------------------
        let (encoded_frame_tx, encoded_frame_rx) =
            mpsc::channel::<Arc<EncodedFrame>>(ENCODED_FRAME_CHANNEL);
        let (command_tx, command_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL);
        let shutdown = CancellationToken::new();

        tokio::spawn(driver(
            peer_id,
            rtc,
            sockets,
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

/// Inbound packet from one of the per-interface forwarder tasks.
struct InboundPacket {
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

#[allow(clippy::too_many_arguments)]
async fn driver(
    peer_id: PeerId,
    mut rtc: Rtc,
    sockets: Vec<Arc<UdpSocket>>,
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

    // Spawn one forwarder task per socket. Each forwarder reads packets
    // from its socket and pushes them into the shared inbound channel,
    // tagged with the socket's local address as the destination.
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
    drop(inbound_tx); // we keep one in each forwarder; this prevents driver-side deadlock on close

    loop {
        // 1. Drain all outputs until we get a Timeout (the next deadline).
        let timeout_at = match drain_outputs(
            &mut rtc,
            &sockets_by_addr,
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
                        proto: Protocol::Udp,
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
                // Two routability checks the Linux kernel would otherwise
                // reject with EINVAL. str0m doesn't know about local
                // routing constraints — it pairs all candidates of matching
                // address family — so we filter the obviously-impossible
                // pairs ourselves to avoid log spam and wasted syscalls:
                //
                //   1. Address family mismatch (v4 source, v6 destination).
                //   2. Loopback ↔ non-loopback: a 127.0.0.1-bound socket
                //      can't reach a routable IP, and vice versa.
                if t.source.is_ipv4() != t.destination.is_ipv4() {
                    continue;
                }
                if t.source.ip().is_loopback() != t.destination.ip().is_loopback() {
                    continue;
                }
                let Some(sock) = sockets_by_addr.get(&t.source) else {
                    eprintln!(
                        "[display/webrtc] transmit from unknown source {}, dropping",
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
