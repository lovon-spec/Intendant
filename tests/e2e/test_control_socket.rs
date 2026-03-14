use crate::harness::{ControlSocketClient, IntendantProcess};
use serde_json::json;
use std::time::Duration;

/// Tier 2: Query status via control socket.
#[tokio::test]
async fn test_status_query() {
    let proc = IntendantProcess::spawn_tui("echo control_socket_test", "full", &[]);
    let pid = proc.pid();

    // Connect to the control socket (retries until available)
    let mut sock = ControlSocketClient::connect(pid, Duration::from_secs(10))
        .await
        .expect("Failed to connect to control socket");

    // Send status query
    sock.send(&json!({"action": "status"})).await;

    // Should get a status event back
    let resp = sock.recv(Duration::from_secs(5)).await;
    assert!(resp.is_some(), "Expected status response");
    let resp = resp.unwrap();
    assert_eq!(
        resp.get("event").and_then(|v| v.as_str()),
        Some("status"),
        "Expected status event, got: {:?}",
        resp
    );
    assert!(
        resp.get("session_id").is_some(),
        "Expected session_id in status, got: {:?}",
        resp
    );

    // Kill the process
    proc.kill().await;
}

/// Tier 2: Query usage via control socket.
#[tokio::test]
async fn test_usage_query() {
    let proc = IntendantProcess::spawn_tui("echo usage_test", "full", &[]);
    let pid = proc.pid();

    let mut sock = ControlSocketClient::connect(pid, Duration::from_secs(10))
        .await
        .expect("Failed to connect to control socket");

    // Wait a moment for the agent to start
    tokio::time::sleep(Duration::from_secs(2)).await;

    sock.send(&json!({"action": "usage"})).await;

    let resp = sock.recv(Duration::from_secs(5)).await;
    assert!(resp.is_some(), "Expected usage response");
    let resp = resp.unwrap();
    assert_eq!(
        resp.get("event").and_then(|v| v.as_str()),
        Some("usage"),
        "Expected usage event, got: {:?}",
        resp
    );

    proc.kill().await;
}

/// Tier 2: Set autonomy level via control socket.
#[tokio::test]
async fn test_autonomy_change() {
    let proc = IntendantProcess::spawn_tui("echo autonomy_test", "low", &[]);
    let pid = proc.pid();

    let mut sock = ControlSocketClient::connect(pid, Duration::from_secs(10))
        .await
        .expect("Failed to connect to control socket");

    // Change autonomy to full
    sock.send(&json!({"action": "set_autonomy", "level": "full"}))
        .await;

    // Verify via status query
    tokio::time::sleep(Duration::from_millis(500)).await;
    sock.send(&json!({"action": "status"})).await;

    let resp = sock.recv(Duration::from_secs(5)).await;
    assert!(resp.is_some(), "Expected status response");
    let resp = resp.unwrap();
    // The autonomy field should reflect "Full"
    if let Some(autonomy) = resp.get("autonomy").and_then(|v| v.as_str()) {
        assert_eq!(autonomy, "Full", "Expected Full autonomy, got: {}", autonomy);
    }

    proc.kill().await;
}

/// Tier 2: Approve via control socket.
#[tokio::test]
async fn test_approve_via_socket() {
    let proc = IntendantProcess::spawn_tui("echo socket_approve_test", "low", &[]);
    let pid = proc.pid();

    let mut sock = ControlSocketClient::connect(pid, Duration::from_secs(10))
        .await
        .expect("Failed to connect to control socket");

    // Wait for approval_required event on the socket
    let event = sock
        .wait_for_event("approval_required", Duration::from_secs(60))
        .await;
    assert!(
        event.is_some(),
        "Expected approval_required event on control socket"
    );
    let event = event.unwrap();
    let id = event
        .get("id")
        .and_then(|v| v.as_u64())
        .expect("Expected id in approval_required");

    // Approve it
    sock.send(&json!({"action": "approve", "id": id})).await;

    // Should see task_complete or agent output eventually
    let completion = sock
        .wait_for_event("task_complete", Duration::from_secs(30))
        .await;
    // If the task completes, we get task_complete. If not, the agent may keep going.
    // Either way, the approve should have been processed.
    assert!(
        completion.is_some(),
        "Expected completion event after approval"
    );

    proc.kill().await;
}
