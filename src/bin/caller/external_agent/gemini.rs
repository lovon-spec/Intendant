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
    AgentConfig, AgentEvent, AgentImageAttachment, AgentThread, ApprovalCategory,
    ApprovalDecision, ExternalAgent, ToolCompletionStatus,
};

// Re-use the same display tools prompt as Codex — the MCP tools are identical.
use super::codex::DISPLAY_TOOLS_PROMPT;

/// Gemini CU uses a 0-1000 normalized coordinate grid. Tell the model to pass
/// coordinate_space so we denormalize before executing clicks.
const GEMINI_CU_ADDENDUM: &str = "\n\n\
### Coordinate Space\n\
When calling `execute_cu_actions`, ALWAYS pass `\"coordinate_space\": \"normalized_1000\"` \
as a parameter. Your click/scroll/move coordinates use a 0-1000 normalized grid \
and need to be converted to display pixels. Without this parameter, coordinates \
will be interpreted as raw pixel values and clicks will miss their targets.\n";

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
struct JsonRpcNotification {
    jsonrpc: String,
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
    /// Approval mode to pass as `--approval-mode`. One of `default`,
    /// `auto_edit`, `yolo`, `plan` (see `project::GEMINI_APPROVAL_MODES`).
    approval_mode: String,
    /// Whether to pass `--sandbox` to Gemini.
    sandbox: bool,
    /// Extension names to enable via `--extensions`. Empty = use all.
    extensions: Vec<String>,
    /// MCP server name allowlist via `--allowed-mcp-server-names`. Empty = all.
    allowed_mcp_servers: Vec<String>,
    /// Extra workspace dirs via `--include-directories`.
    include_directories: Vec<String>,
    /// Whether to pass `--debug` (Gemini's DevTools console). Off by default.
    debug: bool,
    web_port: Option<u16>,
    prompt_sent: bool,
    config_working_dir: Option<std::path::PathBuf>,
    /// Tracks whether we've merged our entry into `$HOME/.gemini/settings.json`
    /// and what the prior `mcpServers.intendant` value was, if any.
    /// `None` = not modified; `Some(None)` = inserted new; `Some(Some(v))` = overwrote v.
    prior_home_intendant_mcp: Option<Option<serde_json::Value>>,
    child: Option<Child>,
    writer: Option<SharedWriter>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    next_id: AtomicU64,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Active ACP session id captured from the most recent `session/new`
    /// response. Used by `interrupt_turn` to build the `session/cancel`
    /// notification without needing a thread handle.
    session_id: Option<String>,
}

fn home_gemini_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("settings.json"))
}

fn read_settings_json(path: &std::path::Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_settings_json(
    path: &std::path::Path,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    std::fs::write(path, content)
}

/// Undo a prior merge of `mcpServers.intendant` into `$HOME/.gemini/settings.json`.
fn restore_home_gemini_settings(prior: &mut Option<Option<serde_json::Value>>) {
    let Some(prior_val) = prior.take() else {
        return;
    };
    let Some(path) = home_gemini_settings_path() else {
        return;
    };
    if !path.exists() {
        return;
    }
    let mut settings = read_settings_json(&path);
    let Some(obj) = settings.as_object_mut() else {
        return;
    };
    if let Some(mcp_val) = obj.get_mut("mcpServers") {
        if let Some(mcp_obj) = mcp_val.as_object_mut() {
            match prior_val {
                Some(v) => {
                    mcp_obj.insert("intendant".to_string(), v);
                }
                None => {
                    mcp_obj.remove("intendant");
                }
            }
            if mcp_obj.is_empty() {
                obj.remove("mcpServers");
            }
        }
    }
    let _ = write_settings_json(&path, &settings);
}

/// Per-session Gemini CLI configuration. Mirrors the fields we pass as
/// command-line args when spawning the agent process (everything except
/// `command` and `web_port`, which are lifecycle concerns, not knobs).
#[derive(Debug, Clone)]
pub struct GeminiLaunchConfig {
    pub model: Option<String>,
    pub approval_mode: String,
    pub sandbox: bool,
    pub extensions: Vec<String>,
    pub allowed_mcp_servers: Vec<String>,
    pub include_directories: Vec<String>,
    pub debug: bool,
}

impl Default for GeminiLaunchConfig {
    fn default() -> Self {
        Self {
            model: None,
            approval_mode: "default".into(),
            sandbox: false,
            extensions: Vec::new(),
            allowed_mcp_servers: Vec::new(),
            include_directories: Vec::new(),
            debug: false,
        }
    }
}

impl GeminiAgent {
    pub fn new(
        command: String,
        launch: GeminiLaunchConfig,
        web_port: Option<u16>,
    ) -> Self {
        Self {
            command,
            model: launch.model,
            approval_mode: launch.approval_mode,
            sandbox: launch.sandbox,
            extensions: launch.extensions,
            allowed_mcp_servers: launch.allowed_mcp_servers,
            include_directories: launch.include_directories,
            debug: launch.debug,
            web_port,
            prompt_sent: false,
            config_working_dir: None,
            prior_home_intendant_mcp: None,
            child: None,
            writer: None,
            event_tx: None,
            next_id: AtomicU64::new(1),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            reader_handle: None,
            session_id: None,
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

    /// Send a JSON-RPC notification (no response expected). Used for ACP
    /// methods like `session/cancel` that are fire-and-forget.
    async fn send_notification(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), CallerError> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&notification)?;
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
            // Use title for the tool name (e.g. "mcp_intendant_execute_cu_actions")
            // Use raw_input for the preview (actual arguments, not model reasoning)
            let tool_name = if !tc.title.is_empty() {
                tc.title.clone()
            } else {
                format!("{:?}", tc.kind).to_lowercase()
            };
            let preview = tc
                .raw_input
                .as_ref()
                .map(|v| {
                    let s = if let serde_json::Value::String(s) = v {
                        s.clone()
                    } else {
                        v.to_string()
                    };
                    s.chars().take(200).collect::<String>()
                })
                .unwrap_or_default();

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
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                if !text.text.is_empty() {
                    events.push(AgentEvent::Reasoning {
                        text: text.text.clone(),
                    });
                }
            }
        }
        SessionUpdate::Plan(plan) => {
            let entries: Vec<(String, String, String)> = plan
                .entries
                .iter()
                .map(|e| {
                    let priority = format!("{:?}", e.priority).to_lowercase();
                    let status = format!("{:?}", e.status).to_lowercase();
                    (e.content.clone(), priority, status)
                })
                .collect();
            if !entries.is_empty() {
                events.push(AgentEvent::PlanUpdate { entries });
            }
        }
        SessionUpdate::AvailableCommandsUpdate(update) => {
            eprintln!(
                "[gemini] available commands: {}",
                update
                    .available_commands
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        SessionUpdate::CurrentModeUpdate(update) => {
            eprintln!("[gemini] mode changed: {}", update.current_mode_id.0);
        }
        _ => {
            // ConfigOptionUpdate, SessionInfoUpdate, UserMessageChunk — not mapped
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
        // Merge an `mcpServers.intendant` entry into `$HOME/.gemini/settings.json`
        // for MCP-over-HTTP access. We use the user's home dir rather than a
        // project-local `.gemini/` so we don't shadow the real config directory
        // (which holds OAuth credentials and user settings).
        let web_port = config.web_port.or(self.web_port);
        if let Some(port) = web_port {
            if let Some(settings_path) = home_gemini_settings_path() {
                let mut settings = read_settings_json(&settings_path);
                if !settings.is_object() {
                    settings = serde_json::json!({});
                }
                let obj = settings.as_object_mut().expect("settings is object");
                let mcp_entry = obj
                    .entry("mcpServers".to_string())
                    .or_insert_with(|| serde_json::json!({}));
                if !mcp_entry.is_object() {
                    *mcp_entry = serde_json::json!({});
                }
                let mcp_obj = mcp_entry.as_object_mut().expect("mcpServers is object");
                let prior = mcp_obj.get("intendant").cloned();
                let mcp_url = format!("http://localhost:{}/mcp", port);
                mcp_obj.insert(
                    "intendant".to_string(),
                    serde_json::json!({ "url": mcp_url }),
                );

                if let Err(e) = write_settings_json(&settings_path, &settings) {
                    eprintln!(
                        "[gemini] Warning: failed to write {}: {}",
                        settings_path.display(),
                        e
                    );
                } else {
                    self.prior_home_intendant_mcp = Some(prior);
                }
            }
        }
        self.config_working_dir = Some(config.working_dir.clone());

        // Spawn the gemini CLI process in ACP mode, plus every config
        // knob the user has flipped. Gemini latches these at process start
        // (no equivalent of `thread/start` to re-set them later), so the
        // daemon loop handles reactive config changes by tearing the whole
        // agent down and calling `initialize()` again with fresh args.
        let mut args: Vec<String> = vec!["--acp".into()];
        if let Some(ref m) = self.model {
            args.push("--model".into());
            args.push(m.clone());
        }
        // `default` is the Gemini CLI default; passing the flag explicitly
        // makes no difference but bloats `ps`. Skip it.
        if self.approval_mode != "default" {
            args.push("--approval-mode".into());
            args.push(self.approval_mode.clone());
        }
        if self.sandbox {
            args.push("--sandbox".into());
        }
        if !self.extensions.is_empty() {
            args.push("--extensions".into());
            args.push(self.extensions.join(","));
        }
        if !self.allowed_mcp_servers.is_empty() {
            args.push("--allowed-mcp-server-names".into());
            args.push(self.allowed_mcp_servers.join(","));
        }
        if !self.include_directories.is_empty() {
            args.push("--include-directories".into());
            args.push(self.include_directories.join(","));
        }
        if self.debug {
            args.push("--debug".into());
        }
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
        let cwd = self
            .config_working_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

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

        let mut params = serde_json::json!({
            "cwd": cwd,
            "mcpServers": mcp_servers,
        });
        if let Some(ref model) = self.model {
            params["model"] = serde_json::Value::String(model.clone());
        }

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

        // Cache for interrupt_turn — the trait method doesn't take a thread.
        self.session_id = Some(session_id.clone());

        Ok(AgentThread {
            thread_id: session_id,
        })
    }

    async fn send_message(
        &mut self,
        thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        self.send_message_with_images(thread, message, &[]).await
    }

    async fn send_message_with_images(
        &mut self,
        thread: &AgentThread,
        message: &str,
        images: &[AgentImageAttachment],
    ) -> Result<(), CallerError> {
        let augmented = if self.web_port.is_some() && !self.prompt_sent {
            self.prompt_sent = true;
            format!("{}{}{}", message, DISPLAY_TOOLS_PROMPT, GEMINI_CU_ADDENDUM)
        } else {
            message.to_string()
        };

        // ACP `session/prompt` accepts an array of `ContentBlock`s.
        // We use snake_case discriminator (per ACP spec): `text` and `image`.
        let mut prompt: Vec<serde_json::Value> = Vec::with_capacity(images.len() + 1);
        prompt.push(serde_json::json!({"type": "text", "text": augmented}));
        for img in images {
            prompt.push(serde_json::json!({
                "type": "image",
                "data": img.base64,
                "mimeType": img.mime_type,
            }));
        }
        let params = serde_json::json!({
            "sessionId": thread.thread_id,
            "prompt": prompt,
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

    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        let session_id = self
            .session_id
            .as_ref()
            .ok_or_else(|| CallerError::ExternalAgent("no active session".into()))?
            .clone();
        let params = serde_json::json!({ "sessionId": session_id });
        // ACP `session/cancel` is a notification (no response). The agent
        // MUST respond to the pending `session/prompt` request with
        // `StopReason::Cancelled` after receiving it — the existing
        // pending-request path surfaces that as TurnCompleted regardless of
        // the stop reason string, so no special-case handling is needed.
        self.send_notification("session/cancel", Some(params)).await?;
        // Clear pending approval mappings — any outstanding approval
        // requests will be abandoned by the agent anyway.
        self.pending_approvals.lock().await.clear();
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }

        // Undo the `mcpServers.intendant` merge in $HOME/.gemini/settings.json
        restore_home_gemini_settings(&mut self.prior_home_intendant_mcp);
        self.config_working_dir = None;

        self.writer = None;
        self.event_tx = None;
        self.child = None;
        self.session_id = None;

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
        // Undo the $HOME/.gemini/settings.json merge if shutdown() wasn't called.
        restore_home_gemini_settings(&mut self.prior_home_intendant_mcp);
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
        let agent = GeminiAgent::new("gemini".into(), GeminiLaunchConfig::default(), None);
        assert_eq!(agent.command, "gemini");
        assert!(agent.model.is_none());
        assert_eq!(agent.approval_mode, "default");
        assert!(!agent.sandbox);
        assert!(agent.extensions.is_empty());
        assert!(agent.allowed_mcp_servers.is_empty());
        assert!(agent.include_directories.is_empty());
        assert!(!agent.debug);
        assert!(agent.web_port.is_none());
        assert!(!agent.prompt_sent);
        assert!(agent.child.is_none());
    }

    #[tokio::test]
    async fn rollback_turns_default_returns_not_supported() {
        // Gemini inherits the default `rollback_turns` from the trait,
        // which returns "not supported" — the outer loop keys on this
        // typed error to fall back to a full session reset.
        let mut agent = GeminiAgent::new(
            "gemini".into(),
            GeminiLaunchConfig::default(),
            None,
        );
        let err = agent.rollback_turns(1).await.unwrap_err();
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
    fn gemini_agent_new_with_options() {
        let launch = GeminiLaunchConfig {
            model: Some("gemini-2.5-pro".into()),
            approval_mode: "yolo".into(),
            sandbox: true,
            extensions: vec!["ext1".into(), "ext2".into()],
            allowed_mcp_servers: vec!["intendant".into()],
            include_directories: vec!["/tmp/workspace".into()],
            debug: true,
        };
        let agent = GeminiAgent::new("npx".into(), launch, Some(8765));
        assert_eq!(agent.command, "npx");
        assert_eq!(agent.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(agent.approval_mode, "yolo");
        assert!(agent.sandbox);
        assert_eq!(agent.extensions, vec!["ext1".to_string(), "ext2".to_string()]);
        assert_eq!(agent.allowed_mcp_servers, vec!["intendant".to_string()]);
        assert_eq!(agent.include_directories, vec!["/tmp/workspace".to_string()]);
        assert!(agent.debug);
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
    fn translate_agent_thought_chunk() {
        use agent_client_protocol_schema::{ContentChunk, TextContent};
        let update = SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("Step 1: analyze the page layout"))),
        );
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Reasoning { text } => assert_eq!(text, "Step 1: analyze the page layout"),
            _ => panic!("expected Reasoning, got {:?}", events[0]),
        }
    }

    #[test]
    fn translate_plan_update() {
        use agent_client_protocol_schema::{Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus};
        let plan = Plan::new(vec![
            PlanEntry::new("Search for AI agents", PlanEntryPriority::High, PlanEntryStatus::Completed),
            PlanEntry::new("Summarize findings", PlanEntryPriority::Medium, PlanEntryStatus::InProgress),
            PlanEntry::new("Create presentation", PlanEntryPriority::Medium, PlanEntryStatus::Pending),
        ]);
        let update = SessionUpdate::Plan(plan);
        let events = translate_session_update(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::PlanUpdate { entries } => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].0, "Search for AI agents");
                assert_eq!(entries[0].2, "completed");
                assert_eq!(entries[1].2, "inprogress");
                assert_eq!(entries[2].2, "pending");
            }
            _ => panic!("expected PlanUpdate, got {:?}", events[0]),
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
                assert_eq!(tool_name, "Run command: ls -la"); // title becomes tool_name
                assert!(preview.is_empty()); // no raw_input on default ToolCall
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
        assert!(matches!(&events[0], AgentEvent::ToolStarted { tool_name, .. } if tool_name == "Read file: main.rs"));
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

    #[tokio::test]
    async fn interrupt_turn_without_session_errors() {
        let mut agent = GeminiAgent::new("gemini".into(), GeminiLaunchConfig::default(), None);
        let err = agent.interrupt_turn().await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active session"),
                    "expected 'no active session' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[test]
    fn interrupt_turn_wire_format_is_notification() {
        // ACP `session/cancel` must be a notification (no `id` field) and
        // carry `{"sessionId": ...}` as its params.
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "session/cancel".to_string(),
            params: Some(serde_json::json!({"sessionId": "sess-xyz"})),
        };
        let json = serde_json::to_string(&notif).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "session/cancel");
        assert!(v.get("id").is_none(), "notification must not have an id");
        assert_eq!(v["params"]["sessionId"], "sess-xyz");
    }
}
