use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use agent_client_protocol_schema::{
    ContentBlock, RequestPermissionOutcome, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, ToolCallStatus,
};

use crate::error::CallerError;

use super::{
    AgentConfig, AgentEvent, AgentThread, ApprovalCategory, ApprovalDecision, ExternalAgent,
    ToolCompletionStatus,
};

// Re-use the same display tools prompt as Codex — the MCP tools are identical.
use super::codex::DISPLAY_TOOLS_PROMPT;

// ---------------------------------------------------------------------------
// JSON-RPC wire types (same framing as Codex — JSONL over stdio)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: u64,
    result: serde_json::Value,
}

#[derive(Deserialize)]
struct JsonRpcMessage {
    id: Option<u64>,
    method: Option<String>,
    params: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// Pending-request bookkeeping
// ---------------------------------------------------------------------------

type RequestResult = Result<serde_json::Value, String>;
type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<RequestResult>>>>;
/// Maps our synthetic approval request_id → (jsonrpc_id, vec of option_ids)
type PendingApprovals = Arc<Mutex<HashMap<String, (u64, Vec<String>)>>>;

// ---------------------------------------------------------------------------
// GeminiAgent
// ---------------------------------------------------------------------------

pub struct GeminiAgent {
    command: String,
    model: Option<String>,
    web_port: Option<u16>,
    prompt_sent: bool,
    config_working_dir: Option<std::path::PathBuf>,
    child: Option<Child>,
    writer: Option<BufWriter<ChildStdin>>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    next_id: AtomicU64,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
}

impl GeminiAgent {
    pub fn new(command: String, model: Option<String>, web_port: Option<u16>) -> Self {
        Self {
            command,
            model,
            web_port,
            prompt_sent: false,
            config_working_dir: None,
            child: None,
            writer: None,
            event_tx: None,
            next_id: AtomicU64::new(1),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            reader_handle: None,
        }
    }

    // -- internal helpers ---------------------------------------------------

    async fn send_request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, CallerError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_requests.lock().await.insert(id, tx);

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&request)?;

        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let result = rx
            .await
            .map_err(|_| CallerError::ExternalAgent("Request channel closed".into()))?;

        result.map_err(CallerError::ExternalAgent)
    }

    async fn send_response(
        &mut self,
        id: u64,
        result: serde_json::Value,
    ) -> Result<(), CallerError> {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        };
        let line = serde_json::to_string(&response)?;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reader task: parse stdout JSONL, dispatch to events
// ---------------------------------------------------------------------------

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_requests: PendingRequests,
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
                    reason: "Agent process closed stdout".into(),
                    exit_code: None,
                });
                break;
            }
            Err(e) => {
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: format!("IO error reading agent stdout: {}", e),
                    exit_code: None,
                });
                break;
            }
        };

        let msg: JsonRpcMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue, // skip non-JSON lines
        };

        // 1. Response to our request (has id + result/error, no method)
        if msg.method.is_none() {
            if let Some(id) = msg.id {
                let mut pending = pending_requests.lock().await;
                if let Some(tx) = pending.remove(&id) {
                    if let Some(err) = msg.error {
                        let _ = tx.send(Err(err.message));
                    } else {
                        let _ = tx.send(Ok(msg.result.unwrap_or(serde_json::Value::Null)));
                    }
                }
            }
            continue;
        }

        let method = msg.method.as_deref().unwrap_or("");
        let params = msg.params.unwrap_or(serde_json::Value::Null);

        // 2. Server-initiated request (has method + id) — permission requests
        if let Some(jsonrpc_id) = msg.id {
            if method == "session/request_permission" {
                if let Ok(req) = serde_json::from_value::<RequestPermissionRequest_>(params) {
                    let request_id =
                        format!("acp-approval-{}", approval_counter.fetch_add(1, Ordering::Relaxed));
                    let option_ids: Vec<String> = req
                        .options
                        .iter()
                        .map(|o| o.option_id.clone())
                        .collect();
                    pending_approvals
                        .lock()
                        .await
                        .insert(request_id.clone(), (jsonrpc_id, option_ids));

                    // Extract command preview from tool_call
                    let command = req.tool_call_title.unwrap_or_else(|| "unknown action".into());
                    let category = if req.tool_call_kind.as_deref() == Some("edit")
                        || req.tool_call_kind.as_deref() == Some("delete")
                    {
                        ApprovalCategory::FileChange
                    } else {
                        ApprovalCategory::CommandExecution
                    };

                    let _ = event_tx.send(AgentEvent::ApprovalRequest {
                        request_id,
                        command,
                        category,
                    });
                }
            }
            // Other server-initiated requests (fs/read_text_file, terminal/create, etc.)
            // are not handled — we don't provide filesystem/terminal services to the agent.
            continue;
        }

        // 3. Notification (has method, no id) — session updates
        if method == "session/update" {
            if let Ok(notif) = serde_json::from_value::<SessionNotification>(params) {
                translate_session_update(&notif.update, &event_tx);
            }
        }
        // Other notifications are silently ignored.
    }
}

/// Lightweight serde struct for permission request params.
/// We use this instead of the schema crate's type to avoid needing the full
/// ToolCallUpdate deserialization (which requires all fields).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestPermissionRequest_ {
    #[allow(dead_code)]
    session_id: String,
    #[serde(default)]
    tool_call: serde_json::Value,
    #[serde(default)]
    options: Vec<PermissionOption_>,
    // Extracted from tool_call for convenience
    #[serde(default)]
    tool_call_title: Option<String>,
    #[serde(default)]
    tool_call_kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOption_ {
    option_id: String,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    kind: String,
}

/// Translate an ACP SessionUpdate into AgentEvent(s).
fn translate_session_update(
    update: &SessionUpdate,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            // ContentChunk has a single `content: ContentBlock` field
            if let ContentBlock::Text(text) = &chunk.content {
                let _ = event_tx.send(AgentEvent::MessageDelta {
                    text: text.text.clone(),
                });
            }
        }
        SessionUpdate::ToolCall(tc) => {
            let tool_name = tc.title.clone();
            let preview = tc
                .raw_input
                .as_ref()
                .map(|v| {
                    if let serde_json::Value::String(s) = v {
                        s.chars().take(200).collect()
                    } else {
                        let s = v.to_string();
                        s.chars().take(200).collect()
                    }
                })
                .unwrap_or_default();

            let _ = event_tx.send(AgentEvent::ToolStarted {
                item_id: tc.tool_call_id.to_string(),
                tool_name,
                preview,
            });
        }
        SessionUpdate::ToolCallUpdate(tcu) => {
            let item_id = tcu.tool_call_id.to_string();
            let fields = &tcu.fields;

            // Emit output delta if there's content
            if let Some(ref content) = fields.content {
                for block in content {
                    // ToolCallContent may contain text; extract what we can
                    let text = serde_json::to_string(block).unwrap_or_default();
                    if !text.is_empty() {
                        let _ = event_tx.send(AgentEvent::ToolOutputDelta {
                            item_id: item_id.clone(),
                            text,
                        });
                    }
                }
            }

            // Emit completion if status is terminal
            if let Some(ref status) = fields.status {
                let completion = match status {
                    ToolCallStatus::Completed => Some(ToolCompletionStatus::Success),
                    ToolCallStatus::Failed => Some(ToolCompletionStatus::Failed {
                        message: fields
                            .raw_output
                            .as_ref()
                            .and_then(|v| v.as_str())
                            .unwrap_or("failed")
                            .to_string(),
                    }),
                    _ => None, // Pending, InProgress — not terminal
                };
                if let Some(status) = completion {
                    let _ = event_tx.send(AgentEvent::ToolCompleted { item_id, status });
                }
            }
        }
        SessionUpdate::AgentThoughtChunk(_) => {
            // Reasoning — skip for now
        }
        _ => {
            // Plan, AvailableCommandsUpdate, CurrentModeUpdate, etc. — not mapped
        }
    }
}

// ---------------------------------------------------------------------------
// ExternalAgent implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl ExternalAgent for GeminiAgent {
    fn name(&self) -> &str {
        "gemini-cli"
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        // Write .gemini/settings.json for MCP-over-HTTP access to Intendant
        let web_port = config.web_port.or(self.web_port);
        if let Some(port) = web_port {
            let gemini_dir = config.working_dir.join(".gemini");
            let _ = std::fs::create_dir_all(&gemini_dir);
            let settings_path = gemini_dir.join("settings.json");
            let backup_path = gemini_dir.join("settings.json.intendant-backup");

            // Backup existing settings if present and not ours
            if settings_path.exists() {
                if let Ok(existing) = std::fs::read_to_string(&settings_path) {
                    if !existing.contains("\"intendant\"") || !existing.contains("Auto-generated") {
                        let _ = std::fs::copy(&settings_path, &backup_path);
                    }
                }
            }

            let mcp_url = format!("http://localhost:{}/mcp", port);
            let settings = serde_json::json!({
                "_comment": "Auto-generated by Intendant for MCP-over-HTTP integration.",
                "mcpServers": {
                    "intendant": {
                        "url": mcp_url
                    }
                }
            });
            let content = serde_json::to_string_pretty(&settings).unwrap_or_default();
            if let Err(e) = std::fs::write(&settings_path, &content) {
                eprintln!(
                    "[gemini] Warning: failed to write {}: {}",
                    settings_path.display(),
                    e
                );
            } else {
                self.config_working_dir = Some(config.working_dir.clone());
            }
        }

        // Spawn the gemini CLI process in ACP mode
        let mut child = Command::new(&self.command)
            .args(["--acp"])
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

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CallerError::ExternalAgent("Failed to capture child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CallerError::ExternalAgent("Failed to capture child stdout".into()))?;

        self.child = Some(child);
        self.writer = Some(BufWriter::new(stdin));

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_tx = Some(event_tx.clone());

        // Spawn reader task
        let pending_requests = Arc::clone(&self.pending_requests);
        let pending_approvals = Arc::clone(&self.pending_approvals);
        let handle = tokio::spawn(reader_task(
            stdout,
            event_tx,
            pending_requests,
            pending_approvals,
        ));
        self.reader_handle = Some(handle);

        // ACP initialize handshake with 10s timeout
        let init_params = serde_json::json!({
            "protocolVersion": "1",
            "clientInfo": {
                "name": "intendant",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "roots": false,
                "terminal": false,
            }
        });

        let init_future = self.send_request("initialize", Some(init_params));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), init_future).await;

        match result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                return Err(CallerError::ExternalAgent(format!(
                    "initialize request failed: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(CallerError::ExternalAgent(
                    "initialize request timed out (10s)".into(),
                ));
            }
        }

        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        let mut params = serde_json::Map::new();
        params.insert(
            "cwd".into(),
            serde_json::Value::String(
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            ),
        );

        let result = self
            .send_request("session/new", Some(serde_json::Value::Object(params)))
            .await?;

        let session_id = result
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CallerError::ExternalAgent(
                    "session/new response missing 'sessionId' field".into(),
                )
            })?
            .to_string();

        Ok(AgentThread {
            thread_id: session_id,
        })
    }

    async fn send_message(
        &mut self,
        thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        let augmented = if self.web_port.is_some() && !self.prompt_sent {
            self.prompt_sent = true;
            format!("{}{}", message, DISPLAY_TOOLS_PROMPT)
        } else {
            message.to_string()
        };

        let params = serde_json::json!({
            "sessionId": thread.thread_id,
            "prompt": [{"type": "text", "text": augmented}],
        });

        // session/prompt is a request — it blocks until the turn completes.
        // The reader task will emit events as they stream in.
        // When the request completes, the agent has finished its turn.
        let result = self.send_request("session/prompt", Some(params)).await?;

        // Extract stop reason and emit TurnCompleted
        let stop_reason = result
            .get("stopReason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");

        if let Some(ref tx) = self.event_tx {
            let _ = tx.send(AgentEvent::TurnCompleted {
                message: Some(format!("Turn completed: {}", stop_reason)),
            });
        }

        Ok(())
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let (jsonrpc_id, option_ids) = {
            let mut pending = self.pending_approvals.lock().await;
            pending.remove(request_id).ok_or_else(|| {
                CallerError::ExternalAgent(format!(
                    "No pending approval for request_id '{}'",
                    request_id
                ))
            })?
        };

        // Map our decision to an ACP option_id.
        // ACP provides specific option_ids — we pick the best match.
        let selected_option = match decision {
            ApprovalDecision::Accept => {
                // Prefer "allow_once" or first allow option
                option_ids
                    .iter()
                    .find(|id| id.contains("allow") && id.contains("once"))
                    .or_else(|| option_ids.iter().find(|id| id.contains("allow")))
                    .cloned()
            }
            ApprovalDecision::AcceptForSession => {
                // Prefer "allow_always"
                option_ids
                    .iter()
                    .find(|id| id.contains("allow") && id.contains("always"))
                    .or_else(|| option_ids.iter().find(|id| id.contains("allow")))
                    .cloned()
            }
            ApprovalDecision::Decline => {
                option_ids
                    .iter()
                    .find(|id| id.contains("reject") && id.contains("once"))
                    .or_else(|| option_ids.iter().find(|id| id.contains("reject")))
                    .cloned()
            }
            ApprovalDecision::Cancel => None,
        };

        let outcome = if let Some(option_id) = selected_option {
            serde_json::to_value(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
            ))
            .unwrap_or_default()
        } else {
            serde_json::to_value(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
            .unwrap_or_default()
        };

        self.send_response(jsonrpc_id, outcome).await
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }

        // Restore .gemini/settings.json from backup
        if let Some(ref wd) = self.config_working_dir.take() {
            let gemini_dir = wd.join(".gemini");
            let settings_path = gemini_dir.join("settings.json");
            let backup_path = gemini_dir.join("settings.json.intendant-backup");
            if backup_path.exists() {
                let _ = std::fs::rename(&backup_path, &settings_path);
            } else if settings_path.exists() {
                let _ = std::fs::remove_file(&settings_path);
            }
        }

        self.writer = None;
        self.event_tx = None;
        self.child = None;

        Ok(())
    }
}

impl Drop for GeminiAgent {
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
    fn gemini_agent_new_defaults() {
        let agent = GeminiAgent::new("gemini".into(), None, None);
        assert_eq!(agent.command, "gemini");
        assert!(agent.model.is_none());
        assert!(agent.web_port.is_none());
        assert!(!agent.prompt_sent);
        assert!(agent.child.is_none());
    }

    #[test]
    fn gemini_agent_new_with_options() {
        let agent = GeminiAgent::new(
            "npx".into(),
            Some("gemini-2.5-pro".into()),
            Some(8765),
        );
        assert_eq!(agent.command, "npx");
        assert_eq!(agent.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(agent.web_port, Some(8765));
    }

    #[test]
    fn translate_agent_message_chunk() {
        use agent_client_protocol_schema::{ContentChunk, TextContent};
        let update = SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("hello world"))),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        translate_session_update(&update, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected MessageDelta, got {:?}", event),
        }
    }

    #[test]
    fn translate_tool_call() {
        let tc = agent_client_protocol_schema::ToolCall::new("call-1", "run_shell_command");
        let update = SessionUpdate::ToolCall(tc);
        let (tx, mut rx) = mpsc::unbounded_channel();
        translate_session_update(&update, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                ..
            } => {
                assert_eq!(item_id, "call-1");
                assert_eq!(tool_name, "run_shell_command");
            }
            _ => panic!("expected ToolStarted"),
        }
    }

    #[test]
    fn translate_tool_call_update_completed() {
        use agent_client_protocol_schema::ToolCallUpdateFields;
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        let tcu = agent_client_protocol_schema::ToolCallUpdate::new("call-1", fields);
        let update = SessionUpdate::ToolCallUpdate(tcu);
        let (tx, mut rx) = mpsc::unbounded_channel();
        translate_session_update(&update, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call-1");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            _ => panic!("expected ToolCompleted"),
        }
    }

    #[test]
    fn translate_tool_call_update_failed() {
        use agent_client_protocol_schema::ToolCallUpdateFields;
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let tcu = agent_client_protocol_schema::ToolCallUpdate::new("call-1", fields);
        let update = SessionUpdate::ToolCallUpdate(tcu);
        let (tx, mut rx) = mpsc::unbounded_channel();
        translate_session_update(&update, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call-1");
                assert!(matches!(status, ToolCompletionStatus::Failed { .. }));
            }
            _ => panic!("expected ToolCompleted"),
        }
    }

    #[test]
    fn approval_response_selected() {
        let response = RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("allow-once"),
        ));
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["outcome"]["outcome"], "selected");
        assert_eq!(json["outcome"]["optionId"], "allow-once");
    }

    #[test]
    fn approval_response_cancelled() {
        let response =
            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["outcome"]["outcome"], "cancelled");
    }
}
