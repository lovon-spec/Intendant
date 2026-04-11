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
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, ToolCallContent,
    ToolCallStatus,
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

#[derive(Serialize)]
struct JsonRpcErrorResponse {
    jsonrpc: String,
    id: u64,
    error: JsonRpcErrorObj,
}

#[derive(Serialize)]
struct JsonRpcErrorObj {
    code: i64,
    message: String,
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
/// Shared writer handle so both GeminiAgent and the reader task can write to stdin.
type SharedWriter = Arc<Mutex<BufWriter<ChildStdin>>>;

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
    writer: Option<SharedWriter>,
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

        let shared = self
            .writer
            .as_ref()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        let mut writer = shared.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        drop(writer);

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
        let shared = self
            .writer
            .as_ref()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        let mut writer = shared.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reader task: parse stdout JSONL, dispatch to events
// ---------------------------------------------------------------------------

/// Write a JSON-RPC error response to the shared writer.
async fn send_error_to_writer(writer: &SharedWriter, id: u64, code: i64, message: String) {
    let resp = JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id,
        error: JsonRpcErrorObj { code, message },
    };
    if let Ok(line) = serde_json::to_string(&resp) {
        let mut w = writer.lock().await;
        let _ = w.write_all(line.as_bytes()).await;
        let _ = w.write_all(b"\n").await;
        let _ = w.flush().await;
    }
}

/// Write a JSON-RPC success response to the shared writer.
async fn send_response_to_writer(writer: &SharedWriter, id: u64, result: serde_json::Value) {
    let resp = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result,
    };
    if let Ok(line) = serde_json::to_string(&resp) {
        let mut w = writer.lock().await;
        let _ = w.write_all(line.as_bytes()).await;
        let _ = w.write_all(b"\n").await;
        let _ = w.flush().await;
    }
}

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    writer: SharedWriter,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let approval_counter = AtomicU64::new(1);
    let mut accumulated_message = String::new();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                // Flush accumulated message before termination
                if !accumulated_message.is_empty() {
                    let _ = event_tx.send(AgentEvent::Message {
                        text: std::mem::take(&mut accumulated_message),
                    });
                }
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Agent process closed stdout".into(),
                    exit_code: None,
                });
                break;
            }
            Err(e) => {
                // Flush accumulated message before termination
                if !accumulated_message.is_empty() {
                    let _ = event_tx.send(AgentEvent::Message {
                        text: std::mem::take(&mut accumulated_message),
                    });
                }
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
                // A JSON-RPC response means a turn ended (e.g. session/prompt
                // completed). Flush any accumulated message text.
                if !accumulated_message.is_empty() {
                    let _ = event_tx.send(AgentEvent::Message {
                        text: std::mem::take(&mut accumulated_message),
                    });
                }

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

        // 2. Server-initiated request (has method + id) — requires a response
        if let Some(jsonrpc_id) = msg.id {
            if method == "session/request_permission" {
                match serde_json::from_value::<RequestPermissionRequest_>(params) {
                    Ok(req) => {
                        let request_id = format!(
                            "acp-approval-{}",
                            approval_counter.fetch_add(1, Ordering::Relaxed)
                        );
                        let option_ids: Vec<String> =
                            req.options.iter().map(|o| o.option_id.clone()).collect();
                        pending_approvals
                            .lock()
                            .await
                            .insert(request_id.clone(), (jsonrpc_id, option_ids));

                        // Extract command preview from the nested tool_call object
                        let command = req
                            .tool_call
                            .get("title")
                            .or_else(|| req.tool_call.pointer("/fields/title"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown action")
                            .to_string();

                        let kind_str = req
                            .tool_call
                            .get("kind")
                            .or_else(|| req.tool_call.pointer("/fields/kind"))
                            .and_then(|v| v.as_str());
                        let category = if kind_str == Some("edit") || kind_str == Some("delete") {
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
                    Err(_) => {
                        // Parse failed — send a cancelled response so Gemini doesn't hang.
                        let cancelled = serde_json::to_value(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                        .unwrap_or_default();
                        send_response_to_writer(&writer, jsonrpc_id, cancelled).await;
                    }
                }
            } else {
                // Unhandled server-initiated request (fs/read_text_file,
                // terminal/create, etc.). We don't provide these services,
                // so return a JSON-RPC "method not found" error to unblock Gemini.
                send_error_to_writer(
                    &writer,
                    jsonrpc_id,
                    -32601,
                    format!("Method not supported: {}", method),
                )
                .await;
            }
            continue;
        }

        // 3. Notification (has method, no id) — session updates
        if method == "session/update" {
            if let Ok(notif) = serde_json::from_value::<SessionNotification>(params) {
                // If the update is NOT an AgentMessageChunk and we have
                // accumulated text, flush it as a complete Message first.
                if !matches!(notif.update, SessionUpdate::AgentMessageChunk(_)) {
                    if !accumulated_message.is_empty() {
                        let _ = event_tx.send(AgentEvent::Message {
                            text: std::mem::take(&mut accumulated_message),
                        });
                    }
                }

                let events = translate_session_update(&notif.update);
                for event in events {
                    // Accumulate MessageDelta text for complete Message emission
                    if let AgentEvent::MessageDelta { ref text } = event {
                        accumulated_message.push_str(text);
                    }
                    let _ = event_tx.send(event);
                }
            }
        }
        // Other notifications are silently ignored.
    }
}

/// Lightweight serde struct for permission request params.
/// We only need `sessionId`, `toolCall` (as raw JSON for field extraction),
/// and `options`. The tool call title/kind are extracted from the nested
/// `toolCall` object in the reader task.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestPermissionRequest_ {
    #[allow(dead_code)]
    session_id: String,
    #[serde(default)]
    tool_call: serde_json::Value,
    #[serde(default)]
    options: Vec<PermissionOption_>,
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

/// Extract human-readable text from a `ToolCallContent` block.
/// Format ACP tool content blocks as MCP-style JSON so the WASM Activity tab
/// can extract text and base64 images (for lazy-loaded screenshot rendering).
/// Output format: `{"content":[{"text":"...","type":"text"},{"data":"...","type":"image","mimeType":"image/png"}]}`
fn format_tool_content_blocks(blocks: &[ToolCallContent]) -> String {
    let mut json_blocks = Vec::new();
    let mut has_image = false;

    for block in blocks {
        match block {
            ToolCallContent::Content(c) => match &c.content {
                ContentBlock::Text(t) if !t.text.is_empty() => {
                    json_blocks.push(serde_json::json!({"text": t.text, "type": "text"}));
                }
                ContentBlock::Image(img) if !img.data.is_empty() => {
                    json_blocks.push(serde_json::json!({
                        "data": img.data,
                        "type": "image",
                        "mimeType": img.mime_type,
                    }));
                    has_image = true;
                }
                _ => {}
            },
            ToolCallContent::Diff(d) => {
                let text = format!(
                    "diff {}: {} -> {} bytes",
                    d.path.display(),
                    d.old_text.as_ref().map(|t| t.len()).unwrap_or(0),
                    d.new_text.len()
                );
                json_blocks.push(serde_json::json!({"text": text, "type": "text"}));
            }
            ToolCallContent::Terminal(t) => {
                json_blocks.push(serde_json::json!({"text": format!("[terminal {}]", t.terminal_id), "type": "text"}));
            }
            _ => {}
        }
    }

    if json_blocks.is_empty() {
        return String::new();
    }

    // Only wrap in MCP JSON if there are images (so WASM can extract them).
    // Plain text output stays as plain text for readability.
    if has_image {
        serde_json::json!({"content": json_blocks}).to_string()
    } else {
        // Collect text blocks as plain lines
        json_blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn extract_tool_content_text(block: &ToolCallContent) -> String {
    match block {
        ToolCallContent::Content(c) => match &c.content {
            ContentBlock::Text(t) => t.text.clone(),
            ContentBlock::Image(_) => "[image]".to_string(),
            _ => String::new(),
        },
        ToolCallContent::Diff(d) => {
            let old_len = d.old_text.as_ref().map(|t| t.len()).unwrap_or(0);
            let new_len = d.new_text.len();
            format!("diff {}: {} -> {} bytes", d.path.display(), old_len, new_len)
        }
        ToolCallContent::Terminal(t) => {
            format!("[terminal {}]", t.terminal_id)
        }
        _ => String::new(),
    }
}

/// Extract an error message from a ToolCallUpdate's Failed status.
/// Gemini puts errors in content[0].content.text; fall back to raw_output.
fn extract_failed_message(fields: &agent_client_protocol_schema::ToolCallUpdateFields) -> String {
    // Try content first (Gemini's preferred location for error messages)
    if let Some(ref content) = fields.content {
        for block in content {
            let text = extract_tool_content_text(block);
            if !text.is_empty() {
                return text;
            }
        }
    }
    // Fall back to raw_output
    if let Some(ref raw) = fields.raw_output {
        if let Some(s) = raw.as_str() {
            return s.to_string();
        }
        return raw.to_string();
    }
    "failed".to_string()
}

/// Translate an ACP SessionUpdate into AgentEvent(s).
fn translate_session_update(update: &SessionUpdate) -> Vec<AgentEvent> {
    let mut events = Vec::new();

    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            // ContentChunk has a single `content: ContentBlock` field
            if let ContentBlock::Text(text) = &chunk.content {
                events.push(AgentEvent::MessageDelta {
                    text: text.text.clone(),
                });
            }
        }
        SessionUpdate::ToolCall(tc) => {
            // Use title as primary preview (human-readable, e.g. "Run command: ls -la")
            // Use kind for tool_name; fall back to raw_input formatted for preview
            let tool_name = format!("{:?}", tc.kind).to_lowercase();
            let preview = if !tc.title.is_empty() {
                tc.title.clone()
            } else {
                tc.raw_input
                    .as_ref()
                    .map(|v| {
                        if let serde_json::Value::String(s) = v {
                            s.chars().take(200).collect()
                        } else {
                            let s = v.to_string();
                            s.chars().take(200).collect()
                        }
                    })
                    .unwrap_or_default()
            };

            let item_id = tc.tool_call_id.to_string();
            events.push(AgentEvent::ToolStarted {
                item_id: item_id.clone(),
                tool_name,
                preview,
            });

            // If the tool call already has content and a terminal status (history
            // replay or already-completed), emit content + completion now.
            let is_terminal = matches!(
                tc.status,
                ToolCallStatus::Completed | ToolCallStatus::Failed
            );
            if is_terminal {
                if !tc.content.is_empty() {
                    let output = format_tool_content_blocks(&tc.content);
                    if !output.is_empty() {
                        events.push(AgentEvent::ToolOutputDelta {
                            item_id: item_id.clone(),
                            text: output,
                        });
                    }
                }
                let status = match tc.status {
                    ToolCallStatus::Completed => ToolCompletionStatus::Success,
                    ToolCallStatus::Failed => {
                        let message = tc
                            .raw_output
                            .as_ref()
                            .and_then(|v| v.as_str())
                            .unwrap_or("failed")
                            .to_string();
                        ToolCompletionStatus::Failed { message }
                    }
                    _ => unreachable!(),
                };
                events.push(AgentEvent::ToolCompleted {
                    item_id,
                    status,
                });
            }
        }
        SessionUpdate::ToolCallUpdate(tcu) => {
            let item_id = tcu.tool_call_id.to_string();
            let fields = &tcu.fields;

            // Emit output delta if there's content.
            // Format as MCP-style JSON so the WASM Activity tab can extract
            // text and images (lazy-loaded screenshots) from the same output.
            if let Some(ref content) = fields.content {
                let output = format_tool_content_blocks(content);
                if !output.is_empty() {
                    events.push(AgentEvent::ToolOutputDelta {
                        item_id: item_id.clone(),
                        text: output,
                    });
                }
            }

            // Emit completion if status is terminal
            if let Some(ref status) = fields.status {
                let completion = match status {
                    ToolCallStatus::Completed => Some(ToolCompletionStatus::Success),
                    ToolCallStatus::Failed => Some(ToolCompletionStatus::Failed {
                        message: extract_failed_message(fields),
                    }),
                    _ => None, // Pending, InProgress — not terminal
                };
                if let Some(status) = completion {
                    events.push(AgentEvent::ToolCompleted { item_id, status });
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

    events
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
        let shared_writer: SharedWriter = Arc::new(Mutex::new(BufWriter::new(stdin)));
        self.writer = Some(Arc::clone(&shared_writer));

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_tx = Some(event_tx.clone());

        // Spawn reader task (gets its own Arc to the shared writer for error responses)
        let pending_requests = Arc::clone(&self.pending_requests);
        let pending_approvals = Arc::clone(&self.pending_approvals);
        let handle = tokio::spawn(reader_task(
            stdout,
            shared_writer,
            event_tx,
            pending_requests,
            pending_approvals,
        ));
        self.reader_handle = Some(handle);

        // ACP initialize handshake with 10s timeout
        let init_params = serde_json::json!({
            "protocolVersion": 1,
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
        let cwd = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Build MCP server list — include Intendant's HTTP MCP if web port is set
        let mcp_servers: Vec<serde_json::Value> = if let Some(port) = self.web_port {
            vec![serde_json::json!({
                "type": "http",
                "name": "intendant",
                "url": format!("http://127.0.0.1:{}/mcp", port),
                "headers": [],
            })]
        } else {
            vec![]
        };

        let params = serde_json::json!({
            "cwd": cwd,
            "mcpServers": mcp_servers,
        });

        let result = self
            .send_request("session/new", Some(params))
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

        // session/prompt blocks until the turn completes. We must NOT await it
        // here because the caller's event drain loop needs to run concurrently to
        // handle ApprovalRequest events (permission requests from Gemini). Instead,
        // fire the request and spawn a task that emits TurnCompleted when it resolves.
        let event_tx = self.event_tx.clone();
        let pending = Arc::clone(&self.pending_requests);
        let writer = self.writer.as_ref()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?
            .clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // Register the pending request
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.lock().await.insert(id, tx);

        // Write the request to stdin
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": params,
        });
        let line = serde_json::to_string(&request)
            .map_err(|e| CallerError::ExternalAgent(e.to_string()))?;
        {
            let mut w = writer.lock().await;
            w.write_all(line.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
        }

        // Spawn a task to await the response and emit TurnCompleted
        tokio::spawn(async move {
            let result = rx.await;
            if let Some(ref etx) = event_tx {
                let stop_reason = match &result {
                    Ok(Ok(val)) => val
                        .get("stopReason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("end_turn")
                        .to_string(),
                    Ok(Err(e)) => format!("error: {}", e),
                    Err(_) => "channel closed".to_string(),
                };
                let _ = etx.send(AgentEvent::TurnCompleted {
                    message: Some(format!("Turn completed: {}", stop_reason)),
                });
            }
        });

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
        // ACP agents use different option ID conventions:
        //   Standard ACP: allow_once, allow_always, reject_once, reject_always
        //   Gemini CLI:   proceed_once, proceed_always, proceed_always_server,
        //                 proceed_always_tool, cancel
        // Match permissively to handle both.
        let is_accept = |id: &&String| {
            id.contains("allow") || (id.contains("proceed") && !id.contains("cancel"))
        };
        let is_reject = |id: &&String| {
            id.contains("reject") || id.contains("cancel")
        };

        let selected_option = match decision {
            ApprovalDecision::Accept => {
                option_ids
                    .iter()
                    .find(|id| is_accept(id) && id.contains("once"))
                    .or_else(|| option_ids.iter().find(|id| is_accept(id)))
                    .cloned()
            }
            ApprovalDecision::AcceptForSession => {
                option_ids
                    .iter()
                    .find(|id| is_accept(id) && id.contains("always"))
                    .or_else(|| option_ids.iter().find(|id| is_accept(id)))
                    .cloned()
            }
            ApprovalDecision::Decline => {
                option_ids
                    .iter()
                    .find(|id| is_reject(id) && id.contains("once"))
                    .or_else(|| option_ids.iter().find(|id| is_reject(id)))
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
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected MessageDelta, got {:?}", events[0]),
        }
    }

    #[test]
    fn translate_tool_call() {
        let tc = agent_client_protocol_schema::ToolCall::new("call-1", "Run command: ls -la");
        let update = SessionUpdate::ToolCall(tc);
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "call-1");
                assert_eq!(tool_name, "other"); // default ToolKind
                assert_eq!(preview, "Run command: ls -la");
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
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call-1");
                assert_eq!(*status, ToolCompletionStatus::Success);
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
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call-1");
                assert!(matches!(status, ToolCompletionStatus::Failed { .. }));
            }
            _ => panic!("expected ToolCompleted"),
        }
    }

    #[test]
    fn translate_tool_call_update_text_content_extracts_text() {
        use agent_client_protocol_schema::{ToolCallUpdateFields, TextContent};
        use agent_client_protocol_schema::Content;

        let text_block = ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new("command output here")),
        ));
        let fields = ToolCallUpdateFields::new()
            .content(vec![text_block]);
        let tcu = agent_client_protocol_schema::ToolCallUpdate::new("call-1", fields);
        let update = SessionUpdate::ToolCallUpdate(tcu);
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "call-1");
                assert_eq!(text, "command output here");
            }
            _ => panic!("expected ToolOutputDelta, got {:?}", events[0]),
        }
    }

    #[test]
    fn translate_tool_call_update_failed_extracts_error_from_content() {
        use agent_client_protocol_schema::{ToolCallUpdateFields, TextContent};
        use agent_client_protocol_schema::Content;

        let error_block = ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new("permission denied")),
        ));
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![error_block]);
        let tcu = agent_client_protocol_schema::ToolCallUpdate::new("call-1", fields);
        let update = SessionUpdate::ToolCallUpdate(tcu);
        let events = translate_session_update(&update);

        // Should have ToolOutputDelta + ToolCompleted
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::ToolOutputDelta { text, .. } => {
                assert_eq!(text, "permission denied");
            }
            _ => panic!("expected ToolOutputDelta, got {:?}", events[0]),
        }
        match &events[1] {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call-1");
                match status {
                    ToolCompletionStatus::Failed { message } => {
                        assert_eq!(message, "permission denied");
                    }
                    _ => panic!("expected Failed status"),
                }
            }
            _ => panic!("expected ToolCompleted, got {:?}", events[1]),
        }
    }

    #[test]
    fn translate_tool_call_completed_status_emits_full_lifecycle() {
        use agent_client_protocol_schema::{TextContent, ToolKind};
        use agent_client_protocol_schema::Content;

        let text_block = ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new("file contents")),
        ));
        let tc = agent_client_protocol_schema::ToolCall::new("call-1", "Read file: main.rs")
            .kind(ToolKind::Read)
            .status(ToolCallStatus::Completed)
            .content(vec![text_block]);
        let update = SessionUpdate::ToolCall(tc);
        let events = translate_session_update(&update);

        // Should emit: ToolStarted, ToolOutputDelta, ToolCompleted
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], AgentEvent::ToolStarted { tool_name, .. } if tool_name == "read"));
        assert!(matches!(&events[1], AgentEvent::ToolOutputDelta { text, .. } if text == "file contents"));
        assert!(matches!(&events[2], AgentEvent::ToolCompleted { status: ToolCompletionStatus::Success, .. }));
    }

    #[test]
    fn extract_tool_content_text_diff() {
        use agent_client_protocol_schema::Diff;

        let diff = ToolCallContent::Diff(Diff::new("src/main.rs", "new content")
            .old_text("old content".to_string()));
        let text = extract_tool_content_text(&diff);
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("11")); // old_text length
        assert!(text.contains("11")); // new_text length
    }

    #[test]
    fn extract_tool_content_text_image() {
        use agent_client_protocol_schema::{ImageContent, Content};

        let img = ToolCallContent::Content(Content::new(
            ContentBlock::Image(ImageContent::new("base64data", "image/png")),
        ));
        let text = extract_tool_content_text(&img);
        assert_eq!(text, "[image]");
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

    #[test]
    fn error_response_serialization() {
        let resp = JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: 42,
            error: JsonRpcErrorObj {
                code: -32601,
                message: "Method not supported: fs/read_text_file".into(),
            },
        };
        let json: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
        assert_eq!(json["error"]["code"], -32601);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fs/read_text_file"));
    }

    #[test]
    fn parse_permission_request_extracts_title_from_tool_call() {
        // Simulates the ACP permission request with tool_call containing a nested title
        let params = serde_json::json!({
            "sessionId": "sess-1",
            "toolCall": {
                "toolCallId": "tc-1",
                "title": "Run command: ls -la",
                "kind": "execute",
                "status": "pending"
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject_once", "name": "Reject once", "kind": "reject_once"}
            ]
        });

        let req: RequestPermissionRequest_ = serde_json::from_value(params).unwrap();

        // Title should be extractable from the nested tool_call
        let title = req
            .tool_call
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown action");
        assert_eq!(title, "Run command: ls -la");

        let kind = req
            .tool_call
            .get("kind")
            .and_then(|v| v.as_str());
        assert_eq!(kind, Some("execute"));

        assert_eq!(req.options.len(), 2);
        assert_eq!(req.options[0].option_id, "allow_once");
        assert_eq!(req.options[1].option_id, "reject_once");
    }

    #[test]
    fn parse_permission_request_missing_title_fallback() {
        // When tool_call has no title, we should get the fallback
        let params = serde_json::json!({
            "sessionId": "sess-1",
            "toolCall": {
                "toolCallId": "tc-1",
                "status": "pending"
            },
            "options": []
        });

        let req: RequestPermissionRequest_ = serde_json::from_value(params).unwrap();

        let title = req
            .tool_call
            .get("title")
            .or_else(|| req.tool_call.pointer("/fields/title"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown action");
        assert_eq!(title, "unknown action");
    }
}
