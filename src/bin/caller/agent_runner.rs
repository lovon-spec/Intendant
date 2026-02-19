use crate::error::CallerError;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub struct AgentOutput {
    pub stdout: String,
    pub stderr: String,
}

fn has_ask_human(json_input: &str) -> bool {
    json_input.contains("\"askHuman\"")
}

fn has_waiting_fetch_status(json_input: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_input) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands.iter().any(|cmd| {
                cmd.get("function").and_then(|v| v.as_str()) == Some("fetchStatus")
                    && cmd.get("wait").and_then(|v| v.as_bool()) == Some(true)
            })
        })
        .unwrap_or(false)
}

pub async fn run_agent(json_input: &str) -> Result<AgentOutput, CallerError> {
    let agent_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("intendant-runtime")))
        .unwrap_or_else(|| std::path::PathBuf::from("./target/debug/intendant-runtime"));

    let mut child = Command::new(&agent_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            CallerError::Agent(format!("Failed to spawn agent at {:?}: {}", agent_path, e))
        })?;

    // Write JSON to stdin and close it
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json_input.as_bytes()).await?;
        // stdin dropped here, closing the pipe
    }

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    // Read stdout with idle timeout and hard timeout (configurable via env vars)
    // When askHuman is present, extend timeouts to allow human response time
    if let Some(mut stdout) = child.stdout.take() {
        let ask_human = has_ask_human(json_input);
        let waiting_fetch = has_waiting_fetch_status(json_input);
        let default_idle_before_first = if ask_human {
            330
        } else if waiting_fetch {
            15
        } else {
            2
        };
        // askHuman may need a long wait *before* first output, but once the first byte arrives
        // we should drain quickly and return control to the loop.
        let default_idle_after_first = 1;
        let default_hard = if ask_human {
            600
        } else if waiting_fetch {
            45
        } else {
            30
        };

        let idle_timeout_before_first = Duration::from_secs(
            std::env::var("INTENDANT_IDLE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default_idle_before_first),
        );
        let idle_timeout_after_first = Duration::from_secs(default_idle_after_first);
        let hard_timeout = Duration::from_secs(
            std::env::var("INTENDANT_HARD_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default_hard),
        );

        let timed_out = timeout(hard_timeout, async {
            let mut temp = [0u8; 4096];
            let mut saw_output = false;
            loop {
                let idle_timeout = if saw_output {
                    idle_timeout_after_first
                } else {
                    idle_timeout_before_first
                };
                match timeout(idle_timeout, stdout.read(&mut temp)).await {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => {
                        saw_output = true;
                        stdout_buf.push_str(&String::from_utf8_lossy(&temp[..n]));
                    }
                    Ok(Err(_)) => break, // Read error
                    Err(_) => {
                        // Wait longer for first byte on delayed fetchStatus results.
                        if saw_output {
                            break;
                        }
                    }
                }
            }
        })
        .await
        .is_err();
        if timed_out {
            stderr_buf.push_str("[caller] hard timeout while reading runtime stdout\n");
        }
    }

    // Read any remaining stderr
    if let Some(mut stderr) = child.stderr.take() {
        let mut temp = Vec::new();
        let _ = timeout(Duration::from_secs(1), stderr.read_to_end(&mut temp)).await;
        stderr_buf = String::from_utf8_lossy(&temp).to_string();
    }

    // Kill the runtime only if still running.
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(2), child.wait()).await;
    }

    Ok(AgentOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_ask_human_detects_function() {
        let json = r#"{"commands":[{"function":"askHuman","nonce":1,"question":"Which DB?"}]}"#;
        assert!(has_ask_human(json));
    }

    #[test]
    fn has_ask_human_false_for_other() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        assert!(!has_ask_human(json));
    }

    #[test]
    fn has_ask_human_mixed_commands() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"},{"function":"askHuman","nonce":2,"question":"ok?"}]}"#;
        assert!(has_ask_human(json));
    }

    #[test]
    fn has_waiting_fetch_status_detects_wait_true() {
        let json = r#"{"commands":[{"function":"fetchStatus","nonce":2,"depending_nonce":1,"wait":true,"status_type":"stdout"}]}"#;
        assert!(has_waiting_fetch_status(json));
    }

    #[test]
    fn has_waiting_fetch_status_false_without_wait() {
        let json = r#"{"commands":[{"function":"fetchStatus","nonce":2,"depending_nonce":1,"status_type":"stdout"}]}"#;
        assert!(!has_waiting_fetch_status(json));
    }
}
