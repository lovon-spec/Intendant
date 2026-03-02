use crate::error::CallerError;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Maximum bytes to read from agent stdout/stderr (64 MB).
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

pub struct AgentOutput {
    pub stdout: String,
    pub stderr: String,
}

fn has_ask_human(json_input: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_input) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands
                .iter()
                .any(|cmd| cmd.get("function").and_then(|v| v.as_str()) == Some("askHuman"))
        })
        .unwrap_or(false)
}

pub async fn run_agent(
    json_input: &str,
    log_dir: &std::path::Path,
) -> Result<AgentOutput, CallerError> {
    let agent_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("intendant-runtime")))
        .unwrap_or_else(|| std::path::PathBuf::from("./target/debug/intendant-runtime"));

    let mut child = Command::new(&agent_path)
        .env("INTENDANT_LOG_DIR", log_dir)
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

    // Hard timeout: 120s default, 600s for askHuman
    let hard_timeout_secs: u64 = if has_ask_human(json_input) { 600 } else { 120 };
    let hard_timeout = Duration::from_secs(hard_timeout_secs);

    // Read stdout and stderr (bounded), then wait for exit, all under a single hard timeout
    let result = timeout(hard_timeout, async {
        let mut stdout_buf = Vec::with_capacity(8192);
        let mut stderr_buf = Vec::with_capacity(8192);
        if let Some(mut stdout) = child.stdout.take() {
            let _ = (&mut stdout).take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut stdout_buf).await;
        }
        if let Some(mut stderr) = child.stderr.take() {
            let _ = (&mut stderr).take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut stderr_buf).await;
        }
        let _ = child.wait().await;
        (stdout_buf, stderr_buf)
    })
    .await;

    match result {
        Ok((stdout_buf, stderr_buf)) => Ok(AgentOutput {
            stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
            stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        }),
        Err(_) => {
            let _ = child.kill().await;
            Err(CallerError::Agent(format!(
                "Agent timed out after {}s",
                hard_timeout_secs
            )))
        }
    }
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
    fn has_ask_human_false_for_text_only() {
        let json =
            r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo \"askHuman\""}]}"#;
        assert!(!has_ask_human(json));
    }
}
