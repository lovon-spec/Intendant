use crate::harness::IntendantProcess;
use serde_json::json;
use std::time::Duration;

/// Tier 1: Basic exec — agent runs a command and signals done.
#[tokio::test]
async fn test_basic_exec() {
    let mut proc = IntendantProcess::spawn("echo hello world", "full", &[]);

    // Should see agent_output containing "hello"
    let event = proc
        .wait_for("agent_output", Duration::from_secs(60))
        .await;
    assert!(
        event.is_some(),
        "Expected agent_output event, got: {:?}",
        proc.events
    );
    let event = event.unwrap();
    let stdout = event.data["stdout"].as_str().unwrap_or("");
    assert!(
        stdout.contains("hello"),
        "Expected stdout to contain 'hello', got: {}",
        stdout
    );

    // Should eventually signal done
    let done = proc.wait_for("done", Duration::from_secs(30)).await;
    assert!(
        done.is_some(),
        "Expected done event, got: {:?}",
        proc.events
    );
}

/// Tier 1: Approval flow — approve a command in low autonomy mode.
#[tokio::test]
async fn test_approval_approve() {
    let mut proc = IntendantProcess::spawn("echo approval_test_output", "low", &[]);

    // Should get approval_required
    let event = proc
        .wait_for("approval_required", Duration::from_secs(60))
        .await;
    assert!(
        event.is_some(),
        "Expected approval_required event, got: {:?}",
        proc.events
    );
    let event = event.unwrap();
    let id = event.data["id"].as_u64().unwrap();

    // Approve the command
    proc.send_command(&json!({"action": "approve", "id": id}))
        .await;

    // Should see agent_output
    let output = proc
        .wait_for("agent_output", Duration::from_secs(30))
        .await;
    assert!(
        output.is_some(),
        "Expected agent_output after approval, got: {:?}",
        proc.events
    );
    let output = output.unwrap();
    let stdout = output.data["stdout"].as_str().unwrap_or("");
    assert!(
        stdout.contains("approval_test_output"),
        "Expected stdout to contain 'approval_test_output', got: {}",
        stdout
    );

    // Should complete
    let done = proc.wait_for("done", Duration::from_secs(30)).await;
    assert!(
        done.is_some(),
        "Expected done event after approval, got: {:?}",
        proc.events
    );
}

/// Tier 1: Denial flow — deny a command in low autonomy mode.
#[tokio::test]
async fn test_approval_deny() {
    let mut proc = IntendantProcess::spawn("echo deny_this", "low", &[]);

    // Should get approval_required
    let event = proc
        .wait_for("approval_required", Duration::from_secs(60))
        .await;
    assert!(
        event.is_some(),
        "Expected approval_required event, got: {:?}",
        proc.events
    );
    let event = event.unwrap();
    let id = event.data["id"].as_u64().unwrap();

    // Deny the command
    proc.send_command(&json!({"action": "deny", "id": id}))
        .await;

    // Should signal done (denied)
    let done = proc.wait_for("done", Duration::from_secs(30)).await;
    assert!(
        done.is_some(),
        "Expected done event after denial, got: {:?}",
        proc.events
    );
    let done = done.unwrap();
    let reason = done.data["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("Denied"),
        "Expected denial reason, got: {}",
        reason
    );
}

/// Tier 1: Follow-up — complete one round then send a follow-up task.
#[tokio::test]
async fn test_follow_up() {
    let mut proc = IntendantProcess::spawn("echo round1_marker", "full", &[]);

    // Wait for first round done
    let event = proc
        .wait_for("round_complete", Duration::from_secs(60))
        .await;
    assert!(
        event.is_some(),
        "Expected round_complete event, got: {:?}",
        proc.events
    );

    // Send follow-up
    proc.send_follow_up("echo round2_marker").await;

    // Should see agent_output from the follow-up
    let output = proc
        .wait_for("agent_output", Duration::from_secs(60))
        .await;
    assert!(
        output.is_some(),
        "Expected agent_output for follow-up, got: {:?}",
        proc.events
    );
    let output = output.unwrap();
    let stdout = output.data["stdout"].as_str().unwrap_or("");
    assert!(
        stdout.contains("round2_marker"),
        "Expected stdout to contain 'round2_marker', got: {}",
        stdout
    );
}
