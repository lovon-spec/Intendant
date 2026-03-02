use crate::error::CallerError;
use crate::sandbox::SandboxConfig;
use std::path::PathBuf;
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
    #[cfg(target_os = "linux")]
    if let Ok(raw_paths) = std::env::var("INTENDANT_SANDBOX_WRITE_PATHS") {
        let write_paths: Vec<PathBuf> = raw_paths
            .split(':')
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .collect();
        if !write_paths.is_empty() {
            let sandbox = SandboxConfig {
                read_paths: vec![PathBuf::from("/")],
                write_paths,
                enabled: true,
            };
            return run_agent_inner(json_input, log_dir, Some(&sandbox)).await;
        }
    }
    run_agent_inner(json_input, log_dir, None).await
}

/// Run the agent with optional Landlock sandbox configuration.
#[allow(dead_code)]
pub async fn run_agent_sandboxed(
    json_input: &str,
    log_dir: &std::path::Path,
    sandbox: &crate::sandbox::SandboxConfig,
) -> Result<AgentOutput, CallerError> {
    run_agent_inner(json_input, log_dir, Some(sandbox)).await
}

async fn run_agent_inner(
    json_input: &str,
    log_dir: &std::path::Path,
    sandbox: Option<&crate::sandbox::SandboxConfig>,
) -> Result<AgentOutput, CallerError> {
    let agent_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("intendant-runtime")))
        .unwrap_or_else(|| std::path::PathBuf::from("./target/debug/intendant-runtime"));

    let mut cmd = Command::new(&agent_path);
    cmd.env("INTENDANT_LOG_DIR", log_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // If sandbox config is provided, serialize it as an env var.
    // The runtime will apply Landlock restrictions at startup.
    #[cfg(target_os = "linux")]
    if let Some(sandbox) = sandbox {
        if sandbox.enabled {
            let write_paths: Vec<String> = sandbox
                .write_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            cmd.env("INTENDANT_SANDBOX_WRITE_PATHS", write_paths.join(":"));
        }
    }

    let mut child = cmd.spawn().map_err(|e| {
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
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();

        let read_stdout = async {
            let mut buf = Vec::with_capacity(8192);
            if let Some(ref mut out) = stdout {
                let _ = out
                    .take(MAX_OUTPUT_BYTES as u64)
                    .read_to_end(&mut buf)
                    .await;
            }
            buf
        };
        let read_stderr = async {
            let mut buf = Vec::with_capacity(8192);
            if let Some(ref mut err) = stderr {
                let _ = err
                    .take(MAX_OUTPUT_BYTES as u64)
                    .read_to_end(&mut buf)
                    .await;
            }
            buf
        };

        let (stdout_buf, stderr_buf, _) = tokio::join!(read_stdout, read_stderr, child.wait());
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
