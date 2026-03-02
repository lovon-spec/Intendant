use crate::tui::event::{AppEvent, ControlMsg, EventBus};
use serde::Serialize;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

/// Events sent to connected control socket clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum OutboundEvent {
    TurnStarted {
        turn: usize,
        budget_pct: f64,
    },
    AgentOutput {
        stdout: String,
        stderr: String,
    },
    ApprovalRequired {
        id: u64,
        command: String,
    },
    AskHuman {
        question: String,
    },
    TaskComplete {
        reason: String,
    },
    Status {
        turn: usize,
        phase: String,
        autonomy: String,
    },
    CommandResult {
        action: String,
        ok: bool,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
}

/// Get the socket path for this process.
pub fn socket_path() -> PathBuf {
    PathBuf::from(format!("/tmp/intendant-{}.sock", std::process::id()))
}

/// Spawn the Unix control socket server.
/// Returns a broadcast sender for pushing events to connected clients.
pub fn spawn_control_server(
    bus: EventBus,
) -> (tokio::task::JoinHandle<()>, broadcast::Sender<String>) {
    let (outbound_tx, _) = broadcast::channel::<String>(256);
    let outbound_tx_clone = outbound_tx.clone();

    let path = socket_path();
    // Clean up stale socket
    let _ = std::fs::remove_file(&path);

    let handle = tokio::spawn(async move {
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Control socket bind failed: {}", e);
                return;
            }
        };
        #[cfg(target_os = "linux")]
        {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    #[cfg(target_os = "linux")]
                    {
                        let current_uid = std::fs::metadata("/proc/self").ok().map(|m| m.uid());
                        if let (Some(uid), Ok(cred)) = (current_uid, stream.peer_cred()) {
                            if cred.uid() != uid {
                                continue;
                            }
                        }
                    }

                    let bus = bus.clone();
                    let mut outbound_rx = outbound_tx_clone.subscribe();

                    tokio::spawn(async move {
                        let (reader, mut writer) = stream.into_split();
                        let mut reader = BufReader::new(reader);

                        // Read inbound commands in one task
                        let bus_inbound = bus.clone();
                        let inbound = tokio::spawn(async move {
                            let mut line = String::new();
                            loop {
                                line.clear();
                                match reader.read_line(&mut line).await {
                                    Ok(0) => break, // EOF
                                    Ok(_) => {
                                        let trimmed = line.trim();
                                        if !trimmed.is_empty() {
                                            if let Ok(msg) =
                                                serde_json::from_str::<ControlMsg>(trimmed)
                                            {
                                                bus_inbound.send(AppEvent::ControlCommand(msg));
                                            }
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        });

                        // Write outbound events in another task
                        let outbound = tokio::spawn(async move {
                            loop {
                                match outbound_rx.recv().await {
                                    Ok(line) => {
                                        let mut data = line.into_bytes();
                                        data.push(b'\n');
                                        if writer.write_all(&data).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                }
                            }
                        });

                        let _ = tokio::join!(inbound, outbound);
                    });
                }
                Err(_) => break,
            }
        }
    });

    (handle, outbound_tx)
}

/// Clean up the socket file.
pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}

/// Broadcast an outbound event to all connected clients.
#[allow(dead_code)]
pub fn broadcast_event(tx: &broadcast::Sender<String>, event: &OutboundEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        let _ = tx.send(json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_contains_pid() {
        let path = socket_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.starts_with("/tmp/intendant-"));
        assert!(path_str.ends_with(".sock"));
        assert!(path_str.contains(&std::process::id().to_string()));
    }

    #[test]
    fn outbound_event_turn_started_serialize() {
        let event = OutboundEvent::TurnStarted {
            turn: 5,
            budget_pct: 12.3,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"turn_started\""));
        assert!(json.contains("\"turn\":5"));
    }

    #[test]
    fn outbound_event_agent_output_serialize() {
        let event = OutboundEvent::AgentOutput {
            stdout: "hello".to_string(),
            stderr: "".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"agent_output\""));
    }

    #[test]
    fn outbound_event_approval_required_serialize() {
        let event = OutboundEvent::ApprovalRequired {
            id: 42,
            command: "rm -rf /tmp".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"approval_required\""));
        assert!(json.contains("\"id\":42"));
    }

    #[test]
    fn outbound_event_ask_human_serialize() {
        let event = OutboundEvent::AskHuman {
            question: "Which database?".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"ask_human\""));
    }

    #[test]
    fn outbound_event_task_complete_serialize() {
        let event = OutboundEvent::TaskComplete {
            reason: "done signal".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_complete\""));
    }

    #[test]
    fn outbound_event_status_serialize() {
        let event = OutboundEvent::Status {
            turn: 3,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"status\""));
        assert!(json.contains("\"turn\":3"));
    }

    #[test]
    fn outbound_event_command_result_serialize() {
        let event = OutboundEvent::CommandResult {
            action: "get_restart_status".to_string(),
            ok: true,
            message: "ok".to_string(),
            data: Some(serde_json::json!({"phase":"ready"})),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"command_result\""));
        assert!(json.contains("\"action\":\"get_restart_status\""));
    }

    #[test]
    fn broadcast_event_to_sender() {
        let (tx, mut rx) = broadcast::channel::<String>(16);
        let event = OutboundEvent::TurnStarted {
            turn: 1,
            budget_pct: 5.0,
        };
        broadcast_event(&tx, &event);
        let received = rx.try_recv().unwrap();
        assert!(received.contains("turn_started"));
    }

    #[tokio::test]
    async fn control_server_lifecycle() {
        let (bus, _rx) = EventBus::new();
        let (handle, _tx) = spawn_control_server(bus);

        // In restricted sandboxes Unix socket bind can be blocked.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let path = socket_path();
        if !path.exists() {
            handle.abort();
            cleanup();
            return;
        }

        // Cleanup
        handle.abort();
        cleanup();
        assert!(!path.exists());
    }
}
