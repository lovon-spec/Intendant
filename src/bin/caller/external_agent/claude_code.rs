use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};

use crate::error::CallerError;

use super::{
    AgentConfig, AgentEvent, AgentThread, ApprovalCategory, ApprovalDecision, ExternalAgent,
    ToolCompletionStatus,
};

use super::codex::DISPLAY_TOOLS_PROMPT;

// ---------------------------------------------------------------------------
// Claude Code JSONL protocol types (stdin/stdout)
// ---------------------------------------------------------------------------

/// Incoming message from Claude Code stdout (JSONL).
#[derive(Deserialize)]
struct CcMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    subtype: Option<String>,
    /// For assistant messages: the API message object.
    #[serde(default)]
    message: Option<serde_json::Value>,
    /// For stream_event: the streaming delta event.
    #[serde(default)]
    event: Option<serde_json::Value>,
    /// For result: the final text result.
    #[serde(default)]
    result: Option<String>,
    /// For result: the session ID.
    #[serde(default)]
    session_id: Option<String>,
    /// For control_request: the request ID.
    #[serde(default)]
    request_id: Option<String>,
    /// For control_request: the request payload.
    #[serde(default)]
    request: Option<serde_json::Value>,
}

/// User message written to Claude Code stdin (JSONL).
#[derive(Serialize)]
struct CcUserMessage {
    #[serde(rename = "type")]
    msg_type: String,
    message: CcMessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    parent_tool_use_id: Option<String>,
}

#[derive(Serialize)]
struct CcMessageContent {
    role: String,
    content: Vec<CcContentBlock>,
}

#[derive(Serialize)]
struct CcContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
}

/// Control response written to stdin for permission requests.
#[derive(Serialize)]
struct CcControlResponse {
    #[serde(rename = "type")]
    msg_type: String,
    response: CcControlResponseInner,
}

#[derive(Serialize)]
struct CcControlResponseInner {
    subtype: String,
    request_id: String,
    response: CcPermissionDecision,
}

#[derive(Serialize)]
struct CcPermissionDecision {
    behavior: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared writer
// ---------------------------------------------------------------------------

type SharedWriter = Arc<Mutex<BufWriter<ChildStdin>>>;

/// Pending approval requests: our request_id → CC's request_id
type PendingApprovals = Arc<Mutex<HashMap<String, String>>>;

// ---------------------------------------------------------------------------
// ClaudeCodeAgent
// ---------------------------------------------------------------------------

pub struct ClaudeCodeAgent {
    command: String,
    model: Option<String>,
    permission_mode: String,
    allowed_tools: Vec<String>,
    web_port: Option<u16>,
    prompt_sent: bool,
    working_dir: Option<std::path::PathBuf>,
    child: Option<Child>,
    writer: Option<SharedWriter>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Session ID from the first result message, used for multi-turn.
    session_id: Option<String>,
}

impl ClaudeCodeAgent {
    pub fn new(
        command: String,
        model: Option<String>,
        permission_mode: String,
        allowed_tools: Vec<String>,
        web_port: Option<u16>,
    ) -> Self {
        Self {
            command,
            model,
            permission_mode,
            allowed_tools,
            web_port,
            prompt_sent: false,
            working_dir: None,
            child: None,
            writer: None,
            event_tx: None,
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            reader_handle: None,
            session_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Reader task: parse stdout JSONL
// ---------------------------------------------------------------------------

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    writer: SharedWriter,
    pending_approvals: PendingApprovals,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let approval_counter = AtomicU64::new(1);

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Claude Code process closed stdout".into(),
                    exit_code: None,
                });
                break;
            }
            Err(e) => {
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: format!("IO error reading Claude Code stdout: {}", e),
                    exit_code: None,
                });
                break;
            }
        };

        let msg: CcMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        match msg.msg_type.as_str() {
            "assistant" => {
                // Full assistant turn — extract text and tool_use blocks
                if let Some(ref message) = msg.message {
                    if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match block_type {
                                "text" => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        let _ = event_tx.send(AgentEvent::Message {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                                "tool_use" => {
                                    let tool_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let tool_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                                    let input = block.get("input").cloned().unwrap_or_default();
                                    let preview: String = if let serde_json::Value::String(s) = &input {
                                        s.chars().take(200).collect()
                                    } else {
                                        let s = input.to_string();
                                        s.chars().take(200).collect()
                                    };
                                    let _ = event_tx.send(AgentEvent::ToolStarted {
                                        item_id: tool_id,
                                        tool_name,
                                        preview,
                                    });
                                }
                                "tool_result" => {
                                    let tool_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                                    let content_text = block.get("content")
                                        .and_then(|c| {
                                            if let serde_json::Value::String(s) = c { Some(s.clone()) }
                                            else if let Some(arr) = c.as_array() {
                                                Some(arr.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
                                            } else { None }
                                        })
                                        .unwrap_or_default();

                                    if !content_text.is_empty() {
                                        let _ = event_tx.send(AgentEvent::ToolOutputDelta {
                                            item_id: tool_id.clone(),
                                            text: content_text,
                                        });
                                    }

                                    let status = if is_error {
                                        ToolCompletionStatus::Failed { message: "tool error".into() }
                                    } else {
                                        ToolCompletionStatus::Success
                                    };
                                    let _ = event_tx.send(AgentEvent::ToolCompleted {
                                        item_id: tool_id,
                                        status,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            "stream_event" => {
                // Streaming delta — extract text deltas
                if let Some(ref event) = msg.event {
                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if event_type == "content_block_delta" {
                        if let Some(delta) = event.get("delta") {
                            let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if delta_type == "text_delta" {
                                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                    let _ = event_tx.send(AgentEvent::MessageDelta {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }

            "result" => {
                let _ = event_tx.send(AgentEvent::TurnCompleted {
                    message: msg.result.clone(),
                });
            }

            "control_request" => {
                if let Some(ref request) = msg.request {
                    let subtype = request.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                    let cc_request_id = msg.request_id.clone().unwrap_or_default();

                    if subtype == "can_use_tool" {
                        let tool_name = request.get("tool_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let input = request.get("input").cloned().unwrap_or_default();
                        let preview: String = if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
                            cmd.chars().take(200).collect()
                        } else {
                            let s = input.to_string();
                            s.chars().take(200).collect()
                        };

                        let our_id = format!("cc-approval-{}", approval_counter.fetch_add(1, Ordering::Relaxed));

                        // Determine category from tool name
                        let category = if tool_name == "Edit" || tool_name == "Write" || tool_name == "NotebookEdit" {
                            ApprovalCategory::FileChange
                        } else {
                            ApprovalCategory::CommandExecution
                        };

                        pending_approvals.lock().await.insert(our_id.clone(), cc_request_id);

                        let _ = event_tx.send(AgentEvent::ApprovalRequest {
                            request_id: our_id,
                            command: format!("{}: {}", tool_name, preview),
                            category,
                        });
                    } else {
                        // Unknown control request — auto-allow to avoid hanging
                        let response = CcControlResponse {
                            msg_type: "control_response".into(),
                            response: CcControlResponseInner {
                                subtype: "success".into(),
                                request_id: cc_request_id,
                                response: CcPermissionDecision {
                                    behavior: "allow".into(),
                                    message: None,
                                },
                            },
                        };
                        if let Ok(line) = serde_json::to_string(&response) {
                            let mut w = writer.lock().await;
                            let _ = w.write_all(line.as_bytes()).await;
                            let _ = w.write_all(b"\n").await;
                            let _ = w.flush().await;
                        }
                    }
                }
            }

            "system" => {
                // Status updates — log but don't emit events
            }

            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ExternalAgent implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl ExternalAgent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        "claude-code"
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        self.working_dir = Some(config.working_dir.clone());

        // Build command args
        let mut args = vec![
            "-p".to_string(),
            "--output-format".into(), "stream-json".into(),
            "--input-format".into(), "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
            "--permission-prompt-tool".into(), "stdio".into(),
        ];

        if let Some(ref model) = self.model.as_ref().or(config.model.as_ref()) {
            args.push("--model".into());
            args.push(model.to_string());
        }

        if !self.permission_mode.is_empty() && self.permission_mode != "default" {
            args.push("--permission-mode".into());
            args.push(self.permission_mode.clone());
        }

        if !self.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.push(self.allowed_tools.join(","));
        }

        // MCP config for Intendant display/CU tools
        let web_port = config.web_port.or(self.web_port);
        if let Some(port) = web_port {
            let mcp_config = serde_json::json!({
                "mcpServers": {
                    "intendant": {
                        "type": "http",
                        "url": format!("http://localhost:{}/mcp", port)
                    }
                }
            });
            args.push("--mcp-config".into());
            args.push(mcp_config.to_string());
        }

        // Spawn the process
        let mut child = Command::new(&self.command)
            .args(&args)
            .current_dir(&config.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| {
                CallerError::ExternalAgent(format!(
                    "Failed to spawn '{}': {}",
                    self.command, e
                ))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            CallerError::ExternalAgent("Failed to capture child stdin".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CallerError::ExternalAgent("Failed to capture child stdout".into())
        })?;

        self.child = Some(child);
        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        self.writer = Some(writer.clone());

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_tx = Some(event_tx.clone());

        let pending_approvals = Arc::clone(&self.pending_approvals);
        let handle = tokio::spawn(reader_task(stdout, event_tx, writer, pending_approvals));
        self.reader_handle = Some(handle);

        // No handshake needed — Claude Code starts immediately.
        // The first user message triggers the agent loop.

        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        // Claude Code doesn't have an explicit thread/session creation step.
        // The session is implicit — created when the first message is sent.
        // We use a placeholder ID; the real session_id comes from the first result.
        Ok(AgentThread {
            thread_id: "claude-code-session".into(),
        })
    }

    async fn send_message(
        &mut self,
        _thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        let augmented = if self.web_port.is_some() && !self.prompt_sent {
            self.prompt_sent = true;
            format!("{}{}", message, DISPLAY_TOOLS_PROMPT)
        } else {
            message.to_string()
        };

        let user_msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content: vec![CcContentBlock {
                    block_type: "text".into(),
                    text: augmented,
                }],
            },
            session_id: self.session_id.clone(),
            parent_tool_use_id: None,
        };

        let line = serde_json::to_string(&user_msg)?;
        let writer = self.writer.as_ref().ok_or_else(|| {
            CallerError::ExternalAgent("Not initialized".into())
        })?;

        let mut w = writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;

        // send_message is non-blocking. The reader task will emit events
        // (MessageDelta, ToolStarted, etc.) as they arrive, and TurnCompleted
        // when a "result" message appears. No deadlock risk because CC's
        // approval flow uses the same stdout stream (control_request), not
        // a blocking request/response pair.

        Ok(())
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let cc_request_id = {
            let mut pending = self.pending_approvals.lock().await;
            pending.remove(request_id).ok_or_else(|| {
                CallerError::ExternalAgent(format!(
                    "No pending approval for request_id '{}'",
                    request_id
                ))
            })?
        };

        let behavior = match decision {
            ApprovalDecision::Accept | ApprovalDecision::AcceptForSession => "allow",
            ApprovalDecision::Decline | ApprovalDecision::Cancel => "deny",
        };

        let response = CcControlResponse {
            msg_type: "control_response".into(),
            response: CcControlResponseInner {
                subtype: "success".into(),
                request_id: cc_request_id,
                response: CcPermissionDecision {
                    behavior: behavior.into(),
                    message: None,
                },
            },
        };

        let line = serde_json::to_string(&response)?;
        let writer = self.writer.as_ref().ok_or_else(|| {
            CallerError::ExternalAgent("Not initialized".into())
        })?;

        let mut w = writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;

        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }

        self.writer = None;
        self.event_tx = None;
        self.child = None;

        Ok(())
    }
}

impl Drop for ClaudeCodeAgent {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_agent_defaults() {
        let agent = ClaudeCodeAgent::new(
            "claude".into(),
            None,
            "auto".into(),
            vec![],
            None,
        );
        assert_eq!(agent.command, "claude");
        assert!(agent.model.is_none());
        assert_eq!(agent.permission_mode, "auto");
        assert!(agent.allowed_tools.is_empty());
        assert!(agent.web_port.is_none());
        assert!(!agent.prompt_sent);
    }

    #[tokio::test]
    async fn rollback_turns_default_returns_not_supported() {
        // Claude Code inherits the default `rollback_turns` from the
        // trait, which returns the "not supported" typed error the
        // outer loop keys on to fall back to a session reset.
        let mut agent = ClaudeCodeAgent::new(
            "claude".into(),
            None,
            "auto".into(),
            vec![],
            None,
        );
        let err = agent.rollback_turns(3).await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("not supported"),
                    "expected 'not supported' in default error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[test]
    fn claude_code_agent_with_options() {
        let agent = ClaudeCodeAgent::new(
            "/usr/local/bin/claude".into(),
            Some("claude-sonnet-4-6".into()),
            "acceptEdits".into(),
            vec!["Read".into(), "Edit".into(), "Bash".into()],
            Some(8765),
        );
        assert_eq!(agent.command, "/usr/local/bin/claude");
        assert_eq!(agent.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(agent.permission_mode, "acceptEdits");
        assert_eq!(agent.allowed_tools, vec!["Read", "Edit", "Bash"]);
        assert_eq!(agent.web_port, Some(8765));
    }

    #[test]
    fn user_message_serialization() {
        let msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content: vec![CcContentBlock {
                    block_type: "text".into(),
                    text: "fix the bug".into(),
                }],
            },
            session_id: None,
            parent_tool_use_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["message"]["role"], "user");
        assert_eq!(json["message"]["content"][0]["type"], "text");
        assert_eq!(json["message"]["content"][0]["text"], "fix the bug");
    }

    #[test]
    fn control_response_serialization() {
        let resp = CcControlResponse {
            msg_type: "control_response".into(),
            response: CcControlResponseInner {
                subtype: "success".into(),
                request_id: "req-123".into(),
                response: CcPermissionDecision {
                    behavior: "allow".into(),
                    message: None,
                },
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], "control_response");
        assert_eq!(json["response"]["subtype"], "success");
        assert_eq!(json["response"]["request_id"], "req-123");
        assert_eq!(json["response"]["response"]["behavior"], "allow");
    }

    #[test]
    fn control_response_deny() {
        let resp = CcControlResponse {
            msg_type: "control_response".into(),
            response: CcControlResponseInner {
                subtype: "success".into(),
                request_id: "req-456".into(),
                response: CcPermissionDecision {
                    behavior: "deny".into(),
                    message: Some("Not allowed".into()),
                },
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["response"]["response"]["behavior"], "deny");
        assert_eq!(json["response"]["response"]["message"], "Not allowed");
    }

    #[test]
    fn parse_assistant_message() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"session_id":"sess-1"}"#;
        let msg: CcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "assistant");
        let content = msg.message.unwrap()["content"].as_array().unwrap().clone();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["name"], "Bash");
    }

    #[test]
    fn parse_stream_event() {
        let json = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}}"#;
        let msg: CcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "stream_event");
        let event = msg.event.unwrap();
        let delta_text = event["delta"]["text"].as_str().unwrap();
        assert_eq!(delta_text, "hello");
    }

    #[test]
    fn parse_result() {
        let json = r#"{"type":"result","subtype":"success","result":"Done","session_id":"sess-1","duration_ms":5000}"#;
        let msg: CcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "result");
        assert_eq!(msg.result.as_deref(), Some("Done"));
        assert_eq!(msg.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn parse_control_request() {
        let json = r#"{"type":"control_request","request_id":"cr-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"}}}"#;
        let msg: CcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "control_request");
        assert_eq!(msg.request_id.as_deref(), Some("cr-1"));
        let req = msg.request.unwrap();
        assert_eq!(req["subtype"], "can_use_tool");
        assert_eq!(req["tool_name"], "Bash");
        assert_eq!(req["input"]["command"], "rm -rf /");
    }

    #[tokio::test]
    async fn interrupt_turn_returns_default_unsupported_error() {
        // Claude Code's JSONL protocol has no documented mid-turn cancel
        // today. We keep the trait default so the caller sees a typed error
        // and can log/escalate without triggering unsafe shutdowns.
        use crate::external_agent::ExternalAgent;
        let mut agent = ClaudeCodeAgent::new(
            "claude".into(),
            None,
            "default".into(),
            vec![],
            None,
        );
        let err = agent.interrupt_turn().await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("interruption not supported"),
                    "expected 'interruption not supported' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }
}
