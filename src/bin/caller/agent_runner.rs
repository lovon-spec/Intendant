use crate::error::CallerError;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub struct AgentOutput {
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_agent(json_input: &str) -> Result<AgentOutput, CallerError> {
    let agent_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("agent")))
        .unwrap_or_else(|| std::path::PathBuf::from("./target/debug/agent"));

    let mut child = Command::new(&agent_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CallerError::Agent(format!("Failed to spawn agent at {:?}: {}", agent_path, e)))?;

    // Write JSON to stdin and close it
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json_input.as_bytes()).await?;
        // stdin dropped here, closing the pipe
    }

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    // Read stdout with idle timeout and hard timeout (configurable via env vars)
    if let Some(mut stdout) = child.stdout.take() {
        let idle_timeout = Duration::from_secs(
            std::env::var("AGENT_IDLE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
        );
        let hard_timeout = Duration::from_secs(
            std::env::var("AGENT_HARD_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        );

        let _ = timeout(hard_timeout, async {
            let mut temp = [0u8; 4096];
            loop {
                match timeout(idle_timeout, stdout.read(&mut temp)).await {
                    Ok(Ok(0)) => break,     // EOF
                    Ok(Ok(n)) => {
                        stdout_buf.push_str(&String::from_utf8_lossy(&temp[..n]));
                    }
                    Ok(Err(_)) => break,    // Read error
                    Err(_) => break,        // Idle timeout
                }
            }
        })
        .await;
    }

    // Read any remaining stderr
    if let Some(mut stderr) = child.stderr.take() {
        let mut temp = Vec::new();
        let _ = timeout(Duration::from_secs(1), stderr.read_to_end(&mut temp)).await;
        stderr_buf = String::from_utf8_lossy(&temp).to_string();
    }

    // Kill the agent process (it runs a status monitor loop that won't exit on its own)
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;

    Ok(AgentOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}
