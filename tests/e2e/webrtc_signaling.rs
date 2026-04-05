//! WebRTC display signaling E2E test.
//!
//! Spawns `intendant --web --json --no-presence` on a random port, connects
//! via WebSocket, grants user display access, and verifies the SDP offer/answer
//! exchange using a Rust-side RTCPeerConnection.
//!
//! On headless environments (no display backend), the test skips gracefully
//! when `display_ready` is not received within 5 seconds.

use futures_util::{SinkExt, StreamExt};
use std::net::TcpListener;
use std::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

/// Find an available TCP port by binding to port 0 and reading the assigned port.
fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to ephemeral port");
    listener.local_addr().unwrap().port()
}

/// RAII guard that kills the subprocess on drop, ensuring cleanup on both
/// success and panic (test failure).
struct ProcessGuard(Option<Child>);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.0 {
            let pid = child.id();
            // Send SIGTERM via the kill command for clean shutdown
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
            // Give it a moment, then force kill
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Build the intendant binary path.  Prefers release, falls back to debug.
fn intendant_binary() -> String {
    let release = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/release/intendant");
    if release.exists() {
        return release.to_string_lossy().into_owned();
    }
    let debug = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug/intendant");
    if debug.exists() {
        return debug.to_string_lossy().into_owned();
    }
    // Fall back to hoping it's on PATH
    "intendant".to_string()
}

/// Spawn `intendant --web <port> --json --no-presence` and return a process guard.
fn spawn_intendant(port: u16) -> ProcessGuard {
    let bin = intendant_binary();
    let child = Command::new(&bin)
        .args([
            "--web",
            &port.to_string(),
            "--json",
            "--no-presence",
            "--direct",
            "sleep 120", // dummy task so the agent stays alive
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {}: {}", bin, e));
    ProcessGuard(Some(child))
}

/// Connect a WebSocket client to the intendant web gateway, retrying briefly
/// while the server starts up.
async fn connect_ws(
    port: u16,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = format!("ws://127.0.0.1:{}", port);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => return ws,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => panic!("WebSocket connect to {} failed: {}", url, e),
        }
    }
}

/// Read the next text message from the WebSocket, with a timeout.
async fn read_ws_text(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    timeout: std::time::Duration,
) -> Option<String> {
    match tokio::time::timeout(timeout, async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(text) = msg {
                return Some(text.to_string());
            }
        }
        None
    })
    .await
    {
        Ok(result) => result,
        Err(_) => None, // timeout
    }
}

/// Create a client-side RTCPeerConnection with a recvonly video transceiver.
async fn create_client_peer_connection(
) -> (std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>, String) {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .expect("register codecs");

    let registry = Registry::new();
    let registry =
        register_default_interceptors(registry, &mut media_engine).expect("register interceptors");

    let mut setting_engine = webrtc::api::setting_engine::SettingEngine::default();
    setting_engine.set_include_loopback_candidate(true);
    setting_engine.set_network_types(vec![
        webrtc::ice::network_type::NetworkType::Udp4,
        webrtc::ice::network_type::NetworkType::Udp6,
    ]);

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();

    let config = RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    };

    let pc = std::sync::Arc::new(
        api.new_peer_connection(config)
            .await
            .expect("new peer connection"),
    );

    // Add a recvonly video transceiver
    pc.add_transceiver_from_kind(
        webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await
    .expect("add recvonly transceiver");

    // Create offer
    let offer = pc.create_offer(None).await.expect("create offer");
    pc.set_local_description(offer)
        .await
        .expect("set local description");

    // Wait for ICE gathering to complete so the offer includes candidates
    let (gather_tx, gather_rx) = tokio::sync::oneshot::channel::<()>();
    let gather_tx = std::sync::Mutex::new(Some(gather_tx));
    pc.on_ice_gathering_state_change(Box::new(move |state| {
        if state
            == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete
        {
            if let Some(tx) = gather_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
        Box::pin(async {})
    }));

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), gather_rx).await;

    let offer_sdp = pc
        .local_description()
        .await
        .map(|d| d.sdp)
        .unwrap_or_default();

    (pc, offer_sdp)
}

#[tokio::test]
async fn test_webrtc_signaling() {
    // ------------------------------------------------------------------
    // Setup: spawn intendant, connect WebSocket
    // ------------------------------------------------------------------
    let port = find_free_port();
    let _guard = spawn_intendant(port);

    // Give the process a moment to initialize
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let ws = connect_ws(port).await;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Drain bootstrap messages (state_snapshot, cached events)
    let drain_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            ws_rx.next(),
        )
        .await
        {
            Ok(Some(Ok(Message::Text(_)))) => continue,
            _ => break,
        }
    }

    // ------------------------------------------------------------------
    // Step 1: Grant user display access
    // ------------------------------------------------------------------
    let grant_msg = serde_json::json!({"action": "grant_user_display"});
    ws_tx
        .send(Message::Text(grant_msg.to_string().into()))
        .await
        .expect("send grant_user_display");

    // ------------------------------------------------------------------
    // Step 2: Wait for display_ready (skip on headless)
    // ------------------------------------------------------------------
    let mut display_id: Option<u32> = None;
    let mut display_width: Option<u32> = None;
    let mut display_height: Option<u32> = None;

    let display_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < display_deadline {
        let text = match read_ws_text(
            &mut ws_rx,
            display_deadline.duration_since(tokio::time::Instant::now()),
        )
        .await
        {
            Some(t) => t,
            None => break,
        };

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if json.get("event").and_then(|v| v.as_str()) == Some("display_ready") {
                display_id = json["display_id"].as_u64().map(|n| n as u32);
                display_width = json["width"].as_u64().map(|n| n as u32);
                display_height = json["height"].as_u64().map(|n| n as u32);
                break;
            }
        }
    }

    // On headless environments, display_ready will never arrive.
    // Skip the WebRTC portion gracefully.
    let did = match display_id {
        Some(id) => {
            assert!(display_width.is_some(), "display_ready missing width");
            assert!(display_height.is_some(), "display_ready missing height");
            eprintln!(
                "[test] display_ready: id={} {}x{}",
                id,
                display_width.unwrap(),
                display_height.unwrap()
            );
            id
        }
        None => {
            eprintln!(
                "[test] No display_ready within 5s -- headless environment, skipping WebRTC portion"
            );
            return;
        }
    };

    // ------------------------------------------------------------------
    // Step 3-4: Create client RTCPeerConnection with recvonly video, SDP offer
    // ------------------------------------------------------------------
    let (client_pc, offer_sdp) = create_client_peer_connection().await;
    assert!(
        offer_sdp.contains("m=video"),
        "Client offer SDP should contain m=video"
    );

    // ------------------------------------------------------------------
    // Step 5: Send display_offer
    // ------------------------------------------------------------------
    let offer_msg = serde_json::json!({
        "t": "display_offer",
        "display_id": did,
        "sdp": offer_sdp,
    });
    ws_tx
        .send(Message::Text(offer_msg.to_string().into()))
        .await
        .expect("send display_offer");

    // ------------------------------------------------------------------
    // Step 6: Wait for display_answer
    // ------------------------------------------------------------------
    let mut answer_sdp: Option<String> = None;
    let answer_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < answer_deadline {
        let text = match read_ws_text(
            &mut ws_rx,
            answer_deadline.duration_since(tokio::time::Instant::now()),
        )
        .await
        {
            Some(t) => t,
            None => break,
        };

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            match json.get("t").and_then(|v| v.as_str()) {
                Some("display_answer") => {
                    let sdp = json["sdp"]
                        .as_str()
                        .expect("display_answer missing sdp")
                        .to_string();
                    let ans_display_id =
                        json["display_id"].as_u64().map(|n| n as u32);
                    assert_eq!(
                        ans_display_id,
                        Some(did),
                        "display_answer display_id mismatch"
                    );
                    answer_sdp = Some(sdp);
                    break;
                }
                Some("display_ice") => {
                    // Server ICE candidates may arrive before the answer
                    // if gathering is fast.  We process them after setting
                    // the remote description.
                    continue;
                }
                _ => continue,
            }
        }
    }

    let answer_sdp =
        answer_sdp.expect("Should have received display_answer within 10s");

    // ------------------------------------------------------------------
    // Step 7: Verify answer SDP
    // ------------------------------------------------------------------
    assert!(
        answer_sdp.contains("m=video"),
        "Answer SDP should contain m=video section"
    );
    // The server waits for ICE gathering to complete, so candidates should
    // be inlined in the SDP.  On some systems the host candidate is
    // 127.0.0.1 (loopback), on others it's a real interface address.
    let has_candidate = answer_sdp.lines().any(|l| l.starts_with("a=candidate"));
    assert!(
        has_candidate,
        "Answer SDP should contain at least one a=candidate line"
    );

    // ------------------------------------------------------------------
    // Step 8: Set remote description on client
    // ------------------------------------------------------------------
    let answer =
        RTCSessionDescription::answer(answer_sdp.clone()).expect("parse answer SDP");
    client_pc
        .set_remote_description(answer)
        .await
        .expect("set remote description (answer)");

    // ------------------------------------------------------------------
    // Step 9: Collect and add server ICE candidates
    // ------------------------------------------------------------------
    // Read any pending display_ice messages and add them.
    let ice_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < ice_deadline {
        let text = match read_ws_text(
            &mut ws_rx,
            ice_deadline.duration_since(tokio::time::Instant::now()),
        )
        .await
        {
            Some(t) => t,
            None => break,
        };

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if json.get("t").and_then(|v| v.as_str()) == Some("display_ice") {
                if let Some(candidate_obj) = json.get("candidate") {
                    let candidate_str = candidate_obj["candidate"]
                        .as_str()
                        .unwrap_or("");
                    let sdp_mid = candidate_obj["sdpMid"]
                        .as_str()
                        .map(String::from);
                    let sdp_mline_index = candidate_obj["sdpMLineIndex"]
                        .as_u64()
                        .map(|n| n as u16);

                    let init = RTCIceCandidateInit {
                        candidate: candidate_str.to_string(),
                        sdp_mid,
                        sdp_mline_index,
                        username_fragment: None,
                    };
                    let _ = client_pc.add_ice_candidate(init).await;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Step 10: Wait for ICE connection (best-effort)
    // ------------------------------------------------------------------
    // ICE may or may not reach Connected depending on the environment.
    // We wait up to 10s; reaching at least Checking confirms the
    // signaling path works.
    let (conn_tx, conn_rx) = tokio::sync::oneshot::channel::<RTCPeerConnectionState>();
    let conn_tx = std::sync::Mutex::new(Some(conn_tx));
    client_pc.on_peer_connection_state_change(Box::new(move |state| {
        eprintln!("[test] client peer connection state: {}", state);
        if state == RTCPeerConnectionState::Connected
            || state == RTCPeerConnectionState::Failed
        {
            if let Some(tx) = conn_tx.lock().unwrap().take() {
                let _ = tx.send(state);
            }
        }
        Box::pin(async {})
    }));

    match tokio::time::timeout(std::time::Duration::from_secs(10), conn_rx).await {
        Ok(Ok(RTCPeerConnectionState::Connected)) => {
            eprintln!("[test] ICE connection established successfully");
        }
        Ok(Ok(state)) => {
            eprintln!(
                "[test] ICE reached state {} (not Connected, but signaling worked)",
                state
            );
        }
        Ok(Err(_)) => {
            eprintln!("[test] ICE state channel closed (peer connection dropped)");
        }
        Err(_) => {
            // Timeout is acceptable -- the signaling path was verified
            // by the answer SDP containing valid candidates.
            eprintln!("[test] ICE connection did not complete within 10s (acceptable in some environments)");
        }
    }

    // ------------------------------------------------------------------
    // Cleanup: close the peer connection and WebSocket
    // ------------------------------------------------------------------
    let _ = client_pc.close().await;
    let _ = ws_tx.close().await;
    // ProcessGuard::drop kills the subprocess
}
