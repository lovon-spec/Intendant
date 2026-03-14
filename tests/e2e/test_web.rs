use crate::harness::{IntendantProcess, WsClient};
use serde_json::json;
use std::time::Duration;

/// Tier 3: WebSocket receives state_snapshot on connect.
#[tokio::test]
async fn test_state_snapshot_on_connect() {
    let _proc = IntendantProcess::spawn_web("echo ws_test", "full", 9876, &[]);

    // Connect WebSocket
    let mut ws = WsClient::connect(9876, Duration::from_secs(10))
        .await
        .expect("Failed to connect WebSocket");

    // First message should be state_snapshot
    let msg = ws.wait_for_type("state_snapshot", Duration::from_secs(5)).await;
    assert!(
        msg.is_some(),
        "Expected state_snapshot on WebSocket connect"
    );
    let msg = msg.unwrap();
    assert!(
        msg.get("state").is_some(),
        "Expected state field in state_snapshot, got: {:?}",
        msg
    );
    let state = &msg["state"];
    assert!(
        state.get("phase").is_some(),
        "Expected phase in state, got: {:?}",
        state
    );

    _proc.kill().await;
}

/// Tier 3: WebSocket tool_request for check_status.
#[tokio::test]
async fn test_tool_request_check_status() {
    let _proc = IntendantProcess::spawn_web("echo tool_request_test", "full", 9877, &[]);

    let mut ws = WsClient::connect(9877, Duration::from_secs(10))
        .await
        .expect("Failed to connect WebSocket");

    // Consume state_snapshot
    ws.wait_for_type("state_snapshot", Duration::from_secs(5))
        .await;

    // Send check_status tool_request
    let result = ws
        .tool_request("check_status", &json!({}), Duration::from_secs(10))
        .await;
    assert!(
        result.is_some(),
        "Expected tool_response for check_status"
    );
    let result = result.unwrap();
    // Should contain some status info
    assert!(
        !result.is_empty(),
        "Expected non-empty check_status result"
    );

    _proc.kill().await;
}

/// Tier 3: WebSocket receives term (ANSI) frames.
#[tokio::test]
async fn test_ansi_term_frames() {
    let _proc = IntendantProcess::spawn_web("echo ansi_test", "full", 9878, &[]);

    let mut ws = WsClient::connect(9878, Duration::from_secs(10))
        .await
        .expect("Failed to connect WebSocket");

    // Collect term frames for a few seconds
    let frames = ws.collect_term_frames(Duration::from_secs(8)).await;

    // Should have received at least one term frame (TUI rendering)
    assert!(
        !frames.is_empty(),
        "Expected at least one term frame from web TUI"
    );

    // Concatenated frames should contain some TUI output
    let all_text: String = frames.join("");
    // The TUI renders status info — it should have some content
    assert!(
        !all_text.is_empty(),
        "Expected non-empty term frame content"
    );

    _proc.kill().await;
}

/// Tier 3: GET /debug returns JSON state.
#[tokio::test]
async fn test_debug_endpoint() {
    let proc = IntendantProcess::spawn_web("echo debug_test", "full", 9879, &[]);

    // Wait for web server to be ready
    tokio::time::sleep(Duration::from_secs(2)).await;

    let snap = proc.debug_snapshot(9879).await;
    assert!(snap.is_some(), "Expected JSON from /debug endpoint");
    let snap = snap.unwrap();

    // Should have agent_state
    assert!(
        snap.get("agent_state").is_some() || snap.get("phase").is_some(),
        "Expected agent state in /debug, got: {:?}",
        snap
    );

    proc.kill().await;
}
