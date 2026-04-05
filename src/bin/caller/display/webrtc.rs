use super::{EncodedFrame, IceConfig, InputEvent, PeerId};
use crate::error::CallerError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

/// A single WebRTC peer connection with a VP8 video track.
///
/// Each peer has its own bounded channel for receiving encoded frames from the
/// shared encoder, and its own sender task that independently packetizes frames
/// into RTP samples. This ensures per-peer RTP timing/sequence state.
pub struct WebRtcPeer {
    pub peer_id: PeerId,
    peer_connection: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    encoded_frame_tx: mpsc::Sender<Arc<EncodedFrame>>,
    sender_handle: Mutex<Option<JoinHandle<()>>>,
    shutdown: CancellationToken,
}

impl WebRtcPeer {
    /// Create a new peer from an SDP offer, returning `(Self, answer_sdp)`.
    ///
    /// `ice_tx` receives trickle ICE candidates as JSON strings, tagged with the
    /// peer ID so the signaling layer can route them to the correct browser.
    ///
    /// `input_handler` is invoked for each `InputEvent` received on the peer's
    /// data channels.
    pub async fn new(
        peer_id: PeerId,
        offer_sdp: &str,
        ice_config: &IceConfig,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        ice_tx: mpsc::Sender<(PeerId, String)>,
    ) -> Result<(Self, String), CallerError> {
        // --- Media engine ---
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs().map_err(|e| {
            CallerError::WebRtc(format!("register codecs: {e}"))
        })?;

        // --- Interceptors ---
        let registry = Registry::new();
        let registry =
            register_default_interceptors(registry, &mut media_engine).map_err(|e| {
                CallerError::WebRtc(format!("register interceptors: {e}"))
            })?;

        // --- API ---
        let mut setting_engine = webrtc::api::setting_engine::SettingEngine::default();
        // Include loopback candidates so localhost connections work (browser
        // and server on the same machine connect via 127.0.0.1).
        setting_engine.set_include_loopback_candidate(true);

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        // --- ICE configuration ---
        let ice_servers: Vec<RTCIceServer> = ice_config
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
                ..Default::default()
            })
            .collect();

        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        // --- Peer connection ---
        let peer_connection = Arc::new(
            api.new_peer_connection(config)
                .await
                .map_err(|e| CallerError::WebRtc(format!("new peer connection: {e}")))?,
        );

        // --- Video track ---
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                ..Default::default()
            },
            "video".to_string(),
            "intendant-display".to_string(),
        ));

        peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| CallerError::WebRtc(format!("add track: {e}")))?;

        // --- Data channels (browser creates them; we handle on_data_channel) ---
        let handler_control = Arc::clone(&input_handler);
        let handler_pointer = Arc::clone(&input_handler);

        peer_connection.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            match label.as_str() {
                "control" => {
                    let handler = Arc::clone(&handler_control);
                    Box::pin(async move {
                        dc.on_message(Box::new(move |msg: DataChannelMessage| {
                            if let Ok(text) = std::str::from_utf8(&msg.data) {
                                if let Ok(evt) = serde_json::from_str::<InputEvent>(text) {
                                    handler(evt);
                                }
                            }
                            Box::pin(async {})
                        }));
                    })
                }
                "pointer" => {
                    let handler = Arc::clone(&handler_pointer);
                    Box::pin(async move {
                        dc.on_message(Box::new(move |msg: DataChannelMessage| {
                            if let Ok(text) = std::str::from_utf8(&msg.data) {
                                if let Ok(evt) = serde_json::from_str::<InputEvent>(text) {
                                    handler(evt);
                                }
                            }
                            Box::pin(async {})
                        }));
                    })
                }
                _ => Box::pin(async {}),
            }
        }));

        // --- Trickle ICE ---
        let ice_peer_id = peer_id;
        peer_connection.on_ice_candidate(Box::new(move |candidate| {
            let tx = ice_tx.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    if let Ok(json) = c.to_json() {
                        if let Ok(s) = serde_json::to_string(&json) {
                            let _ = tx.send((ice_peer_id, s)).await;
                        }
                    }
                }
            })
        }));

        // --- Connection state logging ---
        peer_connection.on_peer_connection_state_change(Box::new(move |state| {
            eprintln!("[display/webrtc] peer connection: {}", state);
            Box::pin(async {})
        }));
        peer_connection.on_ice_connection_state_change(Box::new(move |state| {
            eprintln!("[display/webrtc] ICE: {}", state);
            Box::pin(async {})
        }));

        // --- Set remote description (offer) ---
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| CallerError::WebRtc(format!("parse offer: {e}")))?;
        peer_connection
            .set_remote_description(offer)
            .await
            .map_err(|e| CallerError::WebRtc(format!("set remote description: {e}")))?;

        // --- Create answer ---
        let answer = peer_connection
            .create_answer(None)
            .await
            .map_err(|e| CallerError::WebRtc(format!("create answer: {e}")))?;

        let answer_sdp = answer.sdp.clone();

        peer_connection
            .set_local_description(answer)
            .await
            .map_err(|e| CallerError::WebRtc(format!("set local description: {e}")))?;

        // --- Per-peer encoded frame channel (bounded, backpressure via drop) ---
        let (encoded_frame_tx, mut encoded_frame_rx) = mpsc::channel::<Arc<EncodedFrame>>(8);

        // --- Sender task: read encoded frames, write RTP samples ---
        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let track_clone = Arc::clone(&video_track);

        let sender_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_clone.cancelled() => break,
                    frame = encoded_frame_rx.recv() => {
                        let Some(frame) = frame else { break };
                        let sample = Sample {
                            data: frame.data.clone().into(),
                            duration: Duration::from_millis(frame.duration_ms),
                            ..Default::default()
                        };
                        // Best-effort write; if the track is closed we just stop.
                        if track_clone.write_sample(&sample).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                peer_id,
                peer_connection,
                video_track,
                encoded_frame_tx,
                sender_handle: Mutex::new(Some(sender_handle)),
                shutdown,
            },
            answer_sdp,
        ))
    }

    /// Returns a reference to the sender side of this peer's encoded frame
    /// channel. The encoder fans out `Arc<EncodedFrame>` via `try_send()`;
    /// if the channel is full the frame is dropped for this peer.
    pub fn encoded_frame_tx(&self) -> &mpsc::Sender<Arc<EncodedFrame>> {
        &self.encoded_frame_tx
    }

    /// Add a trickle ICE candidate from the remote peer.
    ///
    /// Parses the JSON-encoded candidate (as sent by the browser's
    /// `RTCPeerConnection.onicecandidate`) and adds it to the underlying
    /// peer connection.
    pub async fn add_ice_candidate(&self, candidate_json: &str) -> Result<(), CallerError> {
        let parsed: serde_json::Value = serde_json::from_str(candidate_json)
            .map_err(|e| CallerError::WebRtc(format!("parse ICE candidate: {e}")))?;

        let candidate_str = parsed["candidate"].as_str().unwrap_or("");
        let sdp_mid = parsed["sdpMid"].as_str().map(String::from);
        let sdp_mline_index = parsed["sdpMLineIndex"].as_u64().map(|n| n as u16);

        let candidate = RTCIceCandidateInit {
            candidate: candidate_str.to_string(),
            sdp_mid,
            sdp_mline_index,
            username_fragment: None,
        };

        self.peer_connection
            .add_ice_candidate(candidate)
            .await
            .map_err(|e| CallerError::WebRtc(format!("add ICE candidate: {e}")))?;

        Ok(())
    }

    /// Gracefully close this peer: cancel the sender task and close the
    /// underlying peer connection.
    pub async fn close(&self) {
        self.shutdown.cancel();
        if let Some(handle) = self.sender_handle.lock().await.take() {
            let _ = handle.await;
        }
        let _ = self.peer_connection.close().await;
    }
}
