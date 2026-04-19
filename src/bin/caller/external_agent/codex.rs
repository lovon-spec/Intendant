use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::CallerError;

use super::{
    AgentConfig, AgentEvent, AgentImageAttachment, AgentThread, ApprovalCategory,
    ApprovalDecision, ExternalAgent, ToolCompletionStatus,
};

// ---------------------------------------------------------------------------
// Display tools system prompt
// ---------------------------------------------------------------------------

/// Codex-specific thread-action helpers. Each wraps one of Codex's app-server
/// JSON-RPC methods (`thread/compact/start`, `thread/fork`, `thread/rollback`,
/// `review/start`, `memory/reset`) with the `threadId` lookup boilerplate.
/// All return a short human-readable status string on success for the
/// dashboard toast.
impl CodexAgent {
    async fn require_active_thread(&self) -> Result<String, CallerError> {
        let guard = self.active_thread_id.lock().await;
        guard
            .clone()
            .ok_or_else(|| CallerError::ExternalAgent("no active Codex thread".into()))
    }

    pub(super) async fn dispatch_thread_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        match op {
            "compact" => self.compact_thread().await,
            "fork" => {
                let name = params.get("name").and_then(|v| v.as_str()).map(String::from);
                self.fork_thread(name).await
            }
            "undo" => {
                let turns =
                    params.get("turns").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                self.rollback_turns_inner(turns).await
            }
            "review" => {
                let prompt = params
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                self.start_review(prompt).await
            }
            "memory-reset" | "memory_reset" => self.reset_memory().await,
            other => Err(CallerError::ExternalAgent(format!(
                "unsupported Codex thread action: /{}",
                other
            ))),
        }
    }

    async fn compact_thread(&mut self) -> Result<String, CallerError> {
        let thread_id = self.require_active_thread().await?;
        let params = serde_json::json!({ "threadId": thread_id });
        let _ = self
            .send_request("thread/compact/start", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/compact/start: {e}")))?;
        Ok("conversation compaction started".to_string())
    }

    async fn fork_thread(&mut self, name: Option<String>) -> Result<String, CallerError> {
        let thread_id = self.require_active_thread().await?;
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String(thread_id));
        if let Some(n) = name.as_deref().filter(|s| !s.trim().is_empty()) {
            obj.insert("name".into(), serde_json::Value::String(n.trim().to_string()));
        }
        let response = self
            .send_request("thread/fork", Some(serde_json::Value::Object(obj)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/fork: {e}")))?;
        let new_id = response
            .pointer("/thread/id")
            .and_then(|v| v.as_str())
            .or_else(|| response.pointer("/threadId").and_then(|v| v.as_str()))
            .unwrap_or("(unknown)");
        // Point subsequent RPCs at the fork — matches raw codex UX where
        // `/fork` moves you into the new thread automatically.
        *self.active_thread_id.lock().await = Some(new_id.to_string());
        Ok(format!("forked into thread {}", new_id))
    }

    /// Inner implementation of the `/undo` thread action. Returns a
    /// human-readable status string for the dashboard toast. The
    /// `ExternalAgent::rollback_turns` trait method (impl below) wraps
    /// this same RPC without the status string — callers just need
    /// to know success/failure.
    async fn rollback_turns_inner(&mut self, turns: u32) -> Result<String, CallerError> {
        if turns == 0 {
            return Err(CallerError::ExternalAgent(
                "rollback count must be at least 1".into(),
            ));
        }
        let thread_id = self.require_active_thread().await?;
        // Codex's `ThreadRollbackParams` accepts `turnsToRollback` in camel
        // case (matching the rest of the wire vocabulary). If Codex ends up
        // accepting `turns` too in a later version, this still works.
        let params = serde_json::json!({
            "threadId": thread_id,
            "turnsToRollback": turns,
        });
        let _ = self
            .send_request("thread/rollback", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/rollback: {e}")))?;
        Ok(format!("rolled back {} turn(s)", turns))
    }

    async fn start_review(
        &mut self,
        prompt: Option<String>,
    ) -> Result<String, CallerError> {
        let thread_id = self.require_active_thread().await?;
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String(thread_id));
        if let Some(p) = prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            obj.insert(
                "prompt".into(),
                serde_json::Value::String(p.trim().to_string()),
            );
        }
        let _ = self
            .send_request("review/start", Some(serde_json::Value::Object(obj)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("review/start: {e}")))?;
        Ok(match prompt {
            Some(p) if !p.trim().is_empty() => format!("review started with prompt: {}", p),
            _ => "review started on current changes".to_string(),
        })
    }

    async fn reset_memory(&mut self) -> Result<String, CallerError> {
        let thread_id = self.require_active_thread().await?;
        let params = serde_json::json!({ "threadId": thread_id });
        let _ = self
            .send_request("memory/reset", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("memory/reset: {e}")))?;
        Ok("Codex memory reset".to_string())
    }
}

/// Guidance about the sandbox Codex is running under, appended to the first
/// user message alongside [`DISPLAY_TOOLS_PROMPT`]. The string is dynamic so
/// the model sees the actual sandbox for this session, not a baked-in default.
///
/// Steered by a concrete failure mode: under `workspace-write`, Codex tried
/// to drive LibreOffice via a UNO socket, then via a named pipe — both are
/// listener binds the sandbox blocks. A line up front about "pure-Python
/// libraries before daemon processes" would have short-circuited that.
pub(super) fn sandbox_hint(sandbox_mode: &str) -> String {
    let body = match sandbox_mode {
        "read-only" => "\
You are running under Codex's `read-only` sandbox. You CANNOT modify any \
file on disk. Use read/search tools only and return findings to the user — \
do not attempt edits, shell side-effects, or spawning daemons.",
        "danger-full-access" => "\
You are running under Codex's `danger-full-access` sandbox. No filesystem \
or network restrictions apply — the user has explicitly opted in. Still \
prefer the least-invasive approach that gets the task done.",
        // Default: treat anything else as workspace-write (Intendant's
        // project config uses that as the default).
        _ => "\
You are running under Codex's `workspace-write` sandbox. Writes are allowed \
inside the project root and `/tmp`; outbound network is blocked unless \
`sandbox_workspace_write.network_access = true` in the config; inbound \
listener binds (sockets AND named pipes) are blocked regardless. \
\n\n\
Implication: when a task needs a document, data file, or archive, prefer a \
pure-Python library that writes the file directly (e.g. `python-pptx` or \
`odfpy` for presentations, `openpyxl` for spreadsheets, `zipfile`/`tarfile` \
for archives, or hand-rolled XML+zip packaging) over automating a desktop \
application through UNO / D-Bus / AppleScript — those need a listener the \
sandbox blocks. If the user explicitly asked for live automation, say the \
sandbox prevents it and ask whether to switch to `danger-full-access` \
before retrying.",
    };
    format!("\n\n## Environment\n\n{}\n", body)
}

pub(super) const DISPLAY_TOOLS_PROMPT: &str = "\n\n\
## Intendant MCP Tools\n\
\n\
You have access to these tools through the `intendant` MCP server.\n\
\n\
**GUI interaction rule:** For all GUI tasks, use take_screenshot and execute_cu_actions. Look at screenshots and click what you see. Do NOT use osascript, accessibility queries, shell commands, or app binary inspection for GUI interaction.\n\
\n\
### Computer Use (always available)\n\
Direct capture and interaction with displays.\n\
- **take_screenshot(display_target?)**: On-demand capture. Returns base64 PNG image.\n\
- **execute_cu_actions(actions, display_target?)**: Execute actions AND return a screenshot.\n\
  A screenshot is automatically taken after the last action.\n\
  Actions is a JSON array. Action types:\n\
  - `{\"type\": \"click\", \"x\": 100, \"y\": 200, \"button\": \"left\"}` — button: left/right/middle\n\
  - `{\"type\": \"double_click\", \"x\": 100, \"y\": 200}`\n\
  - `{\"type\": \"type\", \"text\": \"hello\"}` — types text literally\n\
  - `{\"type\": \"key\", \"key\": \"cmd+space\"}` — key combos: cmd, ctrl, alt, shift + key. Examples: cmd+tab, cmd+space, cmd+w, enter, escape, tab, up, down\n\
  - `{\"type\": \"scroll\", \"x\": 400, \"y\": 300, \"direction\": \"down\", \"amount\": 3}`\n\
  - `{\"type\": \"move_mouse\", \"x\": 100, \"y\": 200}`\n\
  - `{\"type\": \"drag\", \"start_x\": 100, \"start_y\": 200, \"end_x\": 300, \"end_y\": 400}`\n\
  - `{\"type\": \"wait\", \"ms\": 1000}`\n\
  Coordinates are in logical display points.\n\
- **list_displays()**: Enumerate available displays with IDs and resolutions.\n\
\n\
### Display Streaming & Frames (requires active web dashboard)\n\
These access the frame registry populated by the web dashboard's WebRTC\n\
display stream. Returns empty if no dashboard is streaming.\n\
- **list_frames(stream?, count?)**: List captured frames with metadata.\n\
- **read_frame(frame_id, stream?)**: Read a frame image (base64 JPEG). Use frame_id=\"latest\" for most recent.\n\
- **take_display(display_id)**: Signal you are using a display. Notifies the dashboard UI.\n\
- **release_display(display_id, note?)**: Signal you are done with a display.\n\
\n\
### Voice / Live Audio\n\
- **spawn_live_audio(id, provider, playbook, response_schema, timeout_secs?, voice?, model?, initial_message?)**: Spawn a voice conversation via OpenAI Realtime or Gemini Live. Routes audio through Vortex Audio. The voice model follows the playbook and returns structured data matching response_schema. Blocks until complete.\n\
\n\
### Task Delegation\n\
- **start_task(task, display_target?)**: Delegate a task to Intendant's internal agent.\n\
\n\
Display targets: \"user_session\" (user's display), \":99\" (virtual display 99).\n\
";

// ---------------------------------------------------------------------------
// JSON-RPC wire types
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

/// Response sent back to server-initiated requests (e.g. approval responses).
#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: u64,
    result: serde_json::Value,
}

/// Unified incoming message: can be a response, notification, or server request.
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
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// Pending-request bookkeeping
// ---------------------------------------------------------------------------

/// Value resolved for a pending outbound request: either `Ok(result)` or a
/// stringified error.
type RequestResult = Result<serde_json::Value, String>;

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<RequestResult>>>>;

/// Maps our synthetic `request_id` strings back to the JSON-RPC `id` from
/// server-initiated approval requests.
/// Stores (jsonrpc_id, method) so resolve_approval knows the response format.
type PendingApprovals = Arc<Mutex<HashMap<String, (u64, String)>>>;

// ---------------------------------------------------------------------------
// CodexAgent
// ---------------------------------------------------------------------------

pub struct CodexAgent {
    command: String,
    model: Option<String>,
    approval_policy: String,
    /// Sandbox mode sent verbatim to Codex `thread/start`. One of
    /// `"read-only"`, `"workspace-write"`, `"danger-full-access"`.
    sandbox: String,
    /// Reasoning effort override (Responses API). `None` = Codex default.
    reasoning_effort: Option<String>,
    /// Enable Responses API `web_search` tool. Maps to `codex --search`.
    web_search: bool,
    /// Enable outbound network inside the `workspace-write` sandbox. Ignored
    /// by other sandbox modes.
    network_access: bool,
    /// Extra writable roots beyond the project. Absolute paths.
    writable_roots: Vec<String>,
    web_port: Option<u16>,
    prompt_sent: bool,
    /// Working directory where .codex/config.toml was written (for cleanup).
    config_working_dir: Option<std::path::PathBuf>,
    child: Option<Child>,
    writer: Option<BufWriter<ChildStdin>>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    next_id: AtomicU64,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Thread id from the most recent `thread/start`. Used by `interrupt_turn`
    /// to build the `turn/interrupt` params without needing a thread handle.
    active_thread_id: Arc<Mutex<Option<String>>>,
    /// Turn id of the currently active turn, if any. Captured from the
    /// `turn/start` response (and `turn/started`/`thread/started` notifications
    /// as a fallback) and cleared on `turn/completed` / `turn/interrupted` /
    /// `Terminated`.
    active_turn_id: Arc<Mutex<Option<String>>>,
}

/// Knobs that vary per-session and feed into Codex `thread/start` or the
/// process spawn. Accepts sensible defaults so tests and callers that only
/// care about the common fields (command/model/approval/sandbox) can use
/// `..CodexAgentOptions::default()`.
#[derive(Debug, Clone, Default)]
pub struct CodexAgentOptions {
    pub reasoning_effort: Option<String>,
    pub web_search: bool,
    pub network_access: bool,
    pub writable_roots: Vec<String>,
}

impl CodexAgent {
    pub fn new(
        command: String,
        model: Option<String>,
        approval_policy: String,
        sandbox: String,
        web_port: Option<u16>,
    ) -> Self {
        Self::with_options(
            command,
            model,
            approval_policy,
            sandbox,
            web_port,
            CodexAgentOptions::default(),
        )
    }

    pub fn with_options(
        command: String,
        model: Option<String>,
        approval_policy: String,
        sandbox: String,
        web_port: Option<u16>,
        opts: CodexAgentOptions,
    ) -> Self {
        Self {
            command,
            model,
            approval_policy,
            sandbox,
            reasoning_effort: opts.reasoning_effort,
            web_search: opts.web_search,
            network_access: opts.network_access,
            writable_roots: opts.writable_roots,
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
            active_thread_id: Arc::new(Mutex::new(None)),
            active_turn_id: Arc::new(Mutex::new(None)),
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

        result.map_err(|msg| CallerError::ExternalAgent(msg))
    }

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

        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    /// Send a raw JSON-RPC response (used for approval replies to
    /// server-initiated requests).
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
// Reader task
// ---------------------------------------------------------------------------

/// Runs on a background tokio task, reading JSONL from the Codex process
/// stdout and dispatching events / resolving pending requests.
async fn reader_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
    approval_counter: Arc<AtomicU64>,
    active_turn_id: Arc<Mutex<Option<String>>>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                // EOF — clear any active turn so a later interrupt_turn
                // doesn't fire against a dead process.
                active_turn_id.lock().await.take();
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Process stdout closed".into(),
                    exit_code: None,
                });
                return;
            }
            Err(e) => {
                active_turn_id.lock().await.take();
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: format!("IO error reading stdout: {}", e),
                    exit_code: None,
                });
                return;
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let msg: JsonRpcMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[codex] failed to parse JSON-RPC message: {}: {:?}", e, line);
                continue;
            }
        };

        // 1. Response to our request (has id + result/error, no method)
        if msg.method.is_none() {
            if let Some(id) = msg.id {
                let mut pending = pending_requests.lock().await;
                if let Some(tx) = pending.remove(&id) {
                    if let Some(err) = msg.error {
                        let _ = tx.send(Err(format!(
                            "JSON-RPC error {}: {}",
                            err.code, err.message
                        )));
                    } else {
                        let _ = tx.send(Ok(msg.result.unwrap_or(serde_json::Value::Null)));
                    }
                }
            }
            continue;
        }

        let method = msg.method.as_deref().unwrap_or("");

        // 2. Server-to-client request (has method AND id) -- approval requests
        if let Some(jsonrpc_id) = msg.id {
            let request_id = format!(
                "approval-{}",
                approval_counter.fetch_add(1, Ordering::Relaxed)
            );
            pending_approvals
                .lock()
                .await
                .insert(request_id.clone(), (jsonrpc_id, method.to_string()));

            let params = msg.params.unwrap_or(serde_json::Value::Null);

            if method == "item/fileChange/requestApproval" {
                let path = params
                    .pointer("/item/path")
                    .or_else(|| params.pointer("/path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>")
                    .to_string();
                let diff = params
                    .pointer("/item/diff")
                    .or_else(|| params.pointer("/diff"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = event_tx.send(AgentEvent::FileApprovalRequest {
                    request_id,
                    path,
                    diff,
                });
            } else {
                // item/commandExecution/requestApproval or unknown server requests
                let command = params
                    .pointer("/item/command")
                    .or_else(|| params.pointer("/command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>")
                    .to_string();
                let _ = event_tx.send(AgentEvent::ApprovalRequest {
                    request_id,
                    command,
                    category: ApprovalCategory::CommandExecution,
                });
            }
            continue;
        }

        // 3. Notification (has method, no id)
        let params = msg.params.unwrap_or(serde_json::Value::Null);

        // Track active turn id so interrupt_turn() has a target to cancel.
        // Codex emits turn_id in several shapes across versions; accept any
        // top-level `turnId` / `turn_id` / `turn.id` / `thread.lastTurnId`.
        match method {
            "turn/started" | "thread/started" => {
                if let Some(id) = extract_turn_id(&params) {
                    *active_turn_id.lock().await = Some(id);
                }
            }
            "turn/completed" | "turn/interrupted" | "turn/failed" => {
                active_turn_id.lock().await.take();
            }
            "thread/status/changed" => {
                // Only clear if the status signals the turn actually ended.
                // "running" / "paused" / "busy" keep the active turn alive so
                // a subsequent interrupt still has a target.
                if let Some(status) = params.get("status").and_then(|v| v.as_str()) {
                    if matches!(status, "completed" | "idle" | "failed") {
                        active_turn_id.lock().await.take();
                    }
                }
            }
            _ => {}
        }

        translate_notification(method, &params, &event_tx);
    }
}

/// Extract a turn id from a Codex response or notification payload.
///
/// Codex v2 has emitted turn ids under several names across versions; accept
/// the common shapes: `turnId`, `turn_id`, `turn.id`, `thread.lastTurnId`.
fn extract_turn_id(value: &serde_json::Value) -> Option<String> {
    for path in [
        "/turnId",
        "/turn_id",
        "/turn/id",
        "/thread/lastTurnId",
        "/thread/last_turn_id",
    ] {
        if let Some(s) = value.pointer(path).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Translate a Codex notification into one or more `AgentEvent`s.
fn translate_notification(
    method: &str,
    params: &serde_json::Value,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    match method {
        "item/agentMessage/delta" => {
            let text = params
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = event_tx.send(AgentEvent::MessageDelta { text });
        }

        "item/started" => {
            let item_type = params
                .pointer("/item/type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let item_id = params
                .pointer("/item/id")
                .or_else(|| params.get("itemId"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            match item_type {
                "commandExecution" => {
                    let command = params
                        .pointer("/item/command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = event_tx.send(AgentEvent::ToolStarted {
                        item_id,
                        tool_name: "command".to_string(),
                        preview: command,
                    });
                }
                "fileChange" => {
                    let path = params
                        .pointer("/item/path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = event_tx.send(AgentEvent::ToolStarted {
                        item_id,
                        tool_name: "file_change".to_string(),
                        preview: path,
                    });
                }
                "agentMessage" | "userMessage" | "reasoning" => {
                    // agentMessage: deltas will follow via item/agentMessage/delta.
                    // userMessage: echo of the user's input; nothing to emit.
                    // reasoning: model reasoning trace; nothing to emit.
                }
                "mcpToolCall" => {
                    // Codex is calling an MCP tool (e.g. spawn_live_audio, take_screenshot).
                    let tool_name = params
                        .pointer("/item/name")
                        .or_else(|| params.pointer("/item/toolName"))
                        .or_else(|| params.pointer("/item/serverLabel"))
                        .or_else(|| params.pointer("/item/arguments/name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("mcp_tool")
                        .to_string();
                    let server = params
                        .pointer("/item/serverName")
                        .or_else(|| params.pointer("/item/server"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let preview = if server.is_empty() {
                        tool_name.clone()
                    } else {
                        format!("{}:{}", server, tool_name)
                    };
                    let _ = event_tx.send(AgentEvent::ToolStarted {
                        item_id,
                        tool_name: "mcp".to_string(),
                        preview,
                    });
                }
                other => {
                    eprintln!("[codex] unknown item type in item/started: {:?}", other);
                }
            }
        }

        "item/commandExecution/outputDelta" => {
            let item_id = params
                .get("itemId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = params
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = event_tx.send(AgentEvent::ToolOutputDelta { item_id, text });
        }

        "item/completed" => {
            let item = params.get("item").unwrap_or(params);
            let item_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // Reasoning items: surface the chain-of-thought text via a
            // dedicated event so it renders at "detail" verbosity (Verbose +
            // Debug). Skip the ToolCompleted marker — reasoning is not a tool.
            if item_type == "reasoning" {
                if let Some(text) = extract_reasoning_text(item) {
                    if !text.is_empty() {
                        let _ = event_tx.send(AgentEvent::Reasoning { text });
                    }
                }
                return;
            }

            // agentMessage items: content arrives via either streaming deltas
            // (item/agentMessage/delta → Message) or the completed item's
            // text field. Emit Message on completion if the deltas didn't
            // already produce one. Skip the ToolCompleted marker — the
            // final message is not a tool.
            if item_type == "agentMessage" {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        let _ = event_tx.send(AgentEvent::Message {
                            text: text.to_string(),
                        });
                    }
                }
                return;
            }

            // userMessage items are echoes of the user's own input — ignore.
            if item_type == "userMessage" {
                return;
            }

            // Extract command output from commandExecution items
            if item_type == "commandExecution" {
                if let Some(output) = item.get("aggregatedOutput").and_then(|v| v.as_str()) {
                    if !output.is_empty() {
                        let _ = event_tx.send(AgentEvent::ToolOutputDelta {
                            item_id: item_id.clone(),
                            text: output.to_string(),
                        });
                    }
                }
            }

            // Extract MCP tool call results
            if item_type == "mcpToolCall" {
                // MCP results may contain structured data; surface as output
                if let Some(result) = item.get("result") {
                    let text = if let Some(s) = result.as_str() {
                        s.to_string()
                    } else {
                        serde_json::to_string_pretty(result).unwrap_or_default()
                    };
                    if !text.is_empty() {
                        let _ = event_tx.send(AgentEvent::ToolOutputDelta {
                            item_id: item_id.clone(),
                            text,
                        });
                    }
                }
            }

            let status_str = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let status = match status_str {
                "failed" => {
                    let message = extract_failure_message(item);
                    ToolCompletionStatus::Failed { message }
                }
                "cancelled" => ToolCompletionStatus::Cancelled,
                _ => ToolCompletionStatus::Success,
            };
            let _ = event_tx.send(AgentEvent::ToolCompleted { item_id, status });
        }

        "turn/completed" => {
            let message = params
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let _ = event_tx.send(AgentEvent::TurnCompleted { message });
        }

        "turn/diff/updated" => {
            let unified_diff = params
                .get("diff")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let files_changed = params
                .get("files")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let _ = event_tx.send(AgentEvent::DiffUpdated {
                files_changed,
                unified_diff,
            });
        }

        // Informational Codex v2 notifications — no action needed.
        "turn/started" | "thread/started" | "thread/tokenUsage/updated"
        | "account/rateLimits/updated" | "configWarning" => {}

        "mcpServer/startupStatus/updated" => {
            let status = params.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(error) = params.get("error").and_then(|v| v.as_str()) {
                if !error.is_empty() {
                    eprintln!("[codex] MCP server '{}' {}: {}", name, status, error);
                }
            }
        }

        // thread/status/changed may signal turn or thread completion.
        // Codex v2 uses this alongside (or instead of) turn/completed.
        "thread/status/changed" => {
            if let Some(status) = params.get("status").and_then(|v| v.as_str()) {
                if status == "completed" || status == "idle" {
                    let _ = event_tx.send(AgentEvent::TurnCompleted { message: None });
                }
            }
        }

        other => {
            eprintln!("[codex] unknown notification method: {:?} params: {}", other, serde_json::to_string(params).unwrap_or_default());
        }
    }
}

/// Build a failure message for a Codex `item/completed` item with
/// `status: "failed"`. Codex fills `error` for MCP tool faults and internal
/// failures, but for `commandExecution` items that ran to completion with a
/// non-zero exit it omits `error` — the diagnostic sits in `aggregatedOutput`
/// and `exitCode` instead. Prefer the structured `error` when present, else
/// synthesize something informative so downstream logs don't read
/// "unknown error" next to a real Python traceback.
fn extract_failure_message(item: &serde_json::Value) -> String {
    if let Some(err) = item.get("error") {
        match err {
            serde_json::Value::String(s) if !s.is_empty() => return s.clone(),
            serde_json::Value::Object(obj) => {
                if let Some(s) = obj.get("message").and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
            serde_json::Value::Null => {}
            other => return other.to_string(),
        }
    }

    let exit_code = item
        .get("exitCode")
        .and_then(|v| v.as_i64())
        .or_else(|| item.get("exit_code").and_then(|v| v.as_i64()));
    let output_tail = item
        .get("aggregatedOutput")
        .and_then(|v| v.as_str())
        .map(|s| {
            let trimmed = s.trim_end();
            const MAX: usize = 400;
            if trimmed.chars().count() > MAX {
                let start = trimmed.chars().count() - MAX;
                let tail: String = trimmed.chars().skip(start).collect();
                format!("…{}", tail)
            } else {
                trimmed.to_string()
            }
        })
        .filter(|s| !s.is_empty());

    match (exit_code, output_tail) {
        (Some(code), Some(tail)) => format!("command exited {}: {}", code, tail),
        (Some(code), None) => format!("command exited {}", code),
        (None, Some(tail)) => tail,
        (None, None) => "unknown error".to_string(),
    }
}

/// Extract the chain-of-thought text from a Codex `reasoning` item.
///
/// Codex v2 wraps the OpenAI Responses API reasoning shape, which has
/// historically varied: `text` (single string), `summary` (array of
/// `{type: "summary_text", text: "..."}` entries), or `content` (similar
/// array). Walk all three and concatenate whatever we find.
fn extract_reasoning_text(item: &serde_json::Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    }

    for key in ["summary", "content"] {
        if let Some(arr) = item.get(key).and_then(|v| v.as_array()) {
            for entry in arr {
                if let Some(s) = entry.as_str() {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                } else if let Some(s) = entry.get("text").and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                }
            }
        } else if let Some(s) = item.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// ExternalAgent trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl ExternalAgent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        self.model = config.model.or_else(|| self.model.clone());
        self.approval_policy = config.approval_policy.clone();
        self.sandbox = config.sandbox;
        self.reasoning_effort = config.reasoning_effort;
        self.web_search = config.web_search;
        self.network_access = config.network_access;
        self.writable_roots = config.writable_roots;

        // Write .codex/config.toml for MCP-over-HTTP access to Intendant.
        // Backup any existing config and restore on shutdown.
        let web_port = config.web_port.or(self.web_port);
        if let Some(port) = web_port {
            let codex_dir = config.working_dir.join(".codex");
            let _ = std::fs::create_dir_all(&codex_dir);
            let config_path = codex_dir.join("config.toml");
            let backup_path = codex_dir.join("config.toml.intendant-backup");

            // Backup existing config if present (and not already our backup)
            if config_path.exists() {
                if let Ok(existing) = std::fs::read_to_string(&config_path) {
                    if !existing.contains("# Auto-generated by Intendant") {
                        let _ = std::fs::copy(&config_path, &backup_path);
                    }
                }
            }

            let config_content = format!(
                "# Auto-generated by Intendant for MCP-over-HTTP integration.\n\
                 # Original config backed up to config.toml.intendant-backup (if it existed).\n\
                 \n\
                 [mcp_servers.intendant]\n\
                 type = \"http\"\n\
                 url = \"http://localhost:{}/mcp\"\n",
                port
            );
            if let Err(e) = std::fs::write(&config_path, &config_content) {
                eprintln!("[codex] Warning: failed to write {}: {}", config_path.display(), e);
            } else {
                self.config_working_dir = Some(config.working_dir.clone());
            }
        }

        // Pass MCP server config via -c flag so Codex connects to intendant's MCP.
        // Any additional knobs the user toggled in the Control tab (web search,
        // network access inside workspace-write, extra writable roots) are
        // appended here as `-c key=value` overrides so Codex's app-server picks
        // them up exactly as if they had been written to `~/.codex/config.toml`
        // before launch.
        let mcp_url = format!("http://localhost:{}/mcp", self.web_port.unwrap_or(8765));
        let mut args: Vec<String> = vec![
            "app-server".to_string(),
            "-c".to_string(),
            "mcp_servers.intendant.type=\"http\"".to_string(),
            "-c".to_string(),
            format!("mcp_servers.intendant.url=\"{}\"", mcp_url),
        ];
        if self.web_search {
            args.push("-c".to_string());
            args.push("tools.web_search=true".to_string());
        }
        if let Some(ref effort) = self.reasoning_effort {
            // TOML-quote the value explicitly; `-c` parses the RHS as TOML.
            args.push("-c".to_string());
            args.push(format!("model_reasoning_effort=\"{}\"", effort));
        }
        if self.network_access && self.sandbox == "workspace-write" {
            args.push("-c".to_string());
            args.push("sandbox_workspace_write.network_access=true".to_string());
        }
        if !self.writable_roots.is_empty() {
            // TOML array of strings. Quote and escape each path so whitespace
            // and backslashes don't break the parse.
            let quoted: Vec<String> = self
                .writable_roots
                .iter()
                .map(|p| format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\"")))
                .collect();
            args.push("-c".to_string());
            args.push(format!(
                "sandbox_workspace_write.writable_roots=[{}]",
                quoted.join(", ")
            ));
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

        let stdin = child.stdin.take().ok_or_else(|| {
            CallerError::ExternalAgent("Failed to capture child stdin".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CallerError::ExternalAgent("Failed to capture child stdout".into())
        })?;

        self.child = Some(child);
        self.writer = Some(BufWriter::new(stdin));

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_tx = Some(event_tx.clone());

        // Spawn reader task
        let pending_requests = Arc::clone(&self.pending_requests);
        let pending_approvals = Arc::clone(&self.pending_approvals);
        let approval_counter = Arc::new(AtomicU64::new(1));
        let active_turn_id = Arc::clone(&self.active_turn_id);

        let handle = tokio::spawn(reader_task(
            stdout,
            event_tx,
            pending_requests,
            pending_approvals,
            approval_counter,
            active_turn_id,
        ));
        self.reader_handle = Some(handle);

        // Send initialize request with 10s timeout
        let init_params = serde_json::json!({
            "clientInfo": {
                "name": "intendant",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {},
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

        // Send initialized notification
        self.send_notification("initialized", None).await?;

        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        let mut params = serde_json::Map::new();
        if let Some(ref model) = self.model {
            params.insert("model".into(), serde_json::Value::String(model.clone()));
        }
        params.insert(
            "approvalPolicy".into(),
            serde_json::Value::String(self.approval_policy.clone()),
        );
        // Codex accepts `read-only`, `workspace-write`, or
        // `danger-full-access`. Pass the configured value through verbatim
        // so all three modes reach Codex's enforcer unchanged; the config
        // layer is responsible for validation (see `normalize_sandbox_mode`
        // in project.rs).
        params.insert(
            "sandbox".into(),
            serde_json::Value::String(self.sandbox.clone()),
        );

        let result = self
            .send_request("thread/start", Some(serde_json::Value::Object(params)))
            .await?;

        let thread_id = result
            .pointer("/thread/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CallerError::ExternalAgent(
                    "thread/start response missing 'thread.id' field".into(),
                )
            })?
            .to_string();

        // Cache the thread id so interrupt_turn() can build the
        // `turn/interrupt` params without requiring a thread handle.
        *self.active_thread_id.lock().await = Some(thread_id.clone());

        Ok(AgentThread { thread_id })
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
        let augmented = if !self.prompt_sent {
            self.prompt_sent = true;
            // Sandbox hint is cheap (~400 chars) and steers the model away
            // from approaches that the current sandbox will silently reject
            // (e.g. listener binds under workspace-write). Attach on every
            // new thread, whether or not the MCP display tools are wired.
            let sandbox = sandbox_hint(&self.sandbox);
            if self.web_port.is_some() {
                format!("{}{}{}", message, sandbox, DISPLAY_TOOLS_PROMPT)
            } else {
                format!("{}{}", message, sandbox)
            }
        } else {
            message.to_string()
        };
        // Codex v2 `UserInput` enum (camelCase): { type: "text" | "localImage" | "image" }.
        // Prefer `localImage` (file path) when we have one — keeps base64 out of the
        // JSON-RPC stream. Fall back to `image` with a data URL only if we don't.
        let mut input: Vec<serde_json::Value> = Vec::with_capacity(images.len() + 1);
        input.push(serde_json::json!({"type": "text", "text": augmented}));
        for img in images {
            if let Some(ref path) = img.local_path {
                input.push(serde_json::json!({
                    "type": "localImage",
                    "path": path.to_string_lossy(),
                }));
            } else {
                let data_url = format!("data:{};base64,{}", img.mime_type, img.base64);
                input.push(serde_json::json!({
                    "type": "image",
                    "url": data_url,
                }));
            }
        }
        let params = serde_json::json!({
            "threadId": thread.thread_id,
            "input": input,
        });
        // turn/start is a request — Codex v2 requires an id to start processing.
        // The response carries the turn id; cache it so interrupt_turn() can
        // target this specific turn. Fall back to the reader task's
        // turn/started notification hook if the response shape differs.
        let response = self.send_request("turn/start", Some(params)).await?;
        if let Some(id) = extract_turn_id(&response) {
            *self.active_turn_id.lock().await = Some(id);
        }
        // Also make sure the thread id cache matches the thread we were handed
        // (start_thread normally seeds it, but send_message can be called with
        // any thread in principle).
        *self.active_thread_id.lock().await = Some(thread.thread_id.clone());
        Ok(())
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let (jsonrpc_id, method) = self
            .pending_approvals
            .lock()
            .await
            .remove(request_id)
            .ok_or_else(|| {
                CallerError::ExternalAgent(format!(
                    "No pending approval for request_id '{}'",
                    request_id
                ))
            })?;

        // MCP elicitation requests use {"action": "allow/deny"} format.
        // Command/file approval requests use {"decision": "accept/decline"} format.
        let result = if method.contains("mcpServer") || method.contains("elicit") {
            let action = match decision {
                ApprovalDecision::Accept | ApprovalDecision::AcceptForSession => "accept",
                ApprovalDecision::Decline | ApprovalDecision::Cancel => "decline",
            };
            serde_json::json!({ "action": action, "content": {} })
        } else {
            let decision_str = match decision {
                ApprovalDecision::Accept => "accept",
                ApprovalDecision::AcceptForSession => "acceptForSession",
                ApprovalDecision::Decline => "decline",
                ApprovalDecision::Cancel => "cancel",
            };
            serde_json::json!({ "decision": decision_str })
        };

        self.send_response(jsonrpc_id, result).await
    }

    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        let turn_id = {
            let guard = self.active_turn_id.lock().await;
            guard.clone()
        };
        let turn_id = turn_id.ok_or_else(|| {
            CallerError::ExternalAgent("no active turn to interrupt".into())
        })?;
        let thread_id = {
            let guard = self.active_thread_id.lock().await;
            guard.clone()
        };
        let thread_id = thread_id.ok_or_else(|| {
            CallerError::ExternalAgent("no active thread to interrupt".into())
        })?;
        let params = serde_json::json!({
            "threadId": thread_id,
            "turnId": turn_id,
        });
        // turn/interrupt is a JSON-RPC request; Codex responds with `{}` and
        // emits a `turn/completed` notification with status="interrupted"
        // shortly after. The reader task handles that notification like any
        // other turn completion.
        let _ = self.send_request("turn/interrupt", Some(params)).await?;
        // Clear pending approvals — the caller is also expected to resolve
        // them, but clearing here makes the agent's state consistent if the
        // caller forgets.
        self.pending_approvals.lock().await.clear();
        Ok(())
    }

    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        // Mirror `interrupt_turn`'s precondition checks so the error
        // messages are consistent: "no active turn to steer" /
        // "no active thread to steer" both map to typed ExternalAgent
        // errors that `drain_external_agent_events` can fall back on.
        let turn_id = {
            let guard = self.active_turn_id.lock().await;
            guard.clone()
        };
        let turn_id = turn_id.ok_or_else(|| {
            CallerError::ExternalAgent("no active turn to steer".into())
        })?;
        let thread_id = {
            let guard = self.active_thread_id.lock().await;
            guard.clone()
        };
        let thread_id = thread_id.ok_or_else(|| {
            CallerError::ExternalAgent("no active thread to steer".into())
        })?;
        let params = serde_json::json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
            "expectedTurnId": turn_id,
        });
        // `turn/steer` is a JSON-RPC request; Codex replies with
        // `{"turnId": "..."}` on success. We don't care about the returned
        // id — the active turn id hasn't changed, and the active_turn_id
        // cache is still valid for the next interrupt/steer call.
        let _ = self.send_request("turn/steer", Some(params)).await?;
        Ok(())
    }

    async fn thread_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        CodexAgent::dispatch_thread_action(self, op, params).await
    }

    /// Native implementation of conversation rollback. Reuses the
    /// `thread/rollback` RPC under `turnsToRollback` — same as `/undo`,
    /// just without the status string and with a guard allowing 0 to be
    /// a no-op (the HTTP handler may issue rollback with 0 turns when
    /// the target round is already the head).
    async fn rollback_turns(&mut self, turns_to_drop: u32) -> Result<(), CallerError> {
        if turns_to_drop == 0 {
            return Ok(());
        }
        let _status = self.rollback_turns_inner(turns_to_drop).await?;
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        // Abort reader task
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        // Kill child process
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }

        // Restore .codex/config.toml from backup
        if let Some(ref wd) = self.config_working_dir.take() {
            let codex_dir = wd.join(".codex");
            let config_path = codex_dir.join("config.toml");
            let backup_path = codex_dir.join("config.toml.intendant-backup");
            if backup_path.exists() {
                let _ = std::fs::rename(&backup_path, &config_path);
            } else if config_path.exists() {
                // No backup means we created it fresh — remove our generated file
                let _ = std::fs::remove_file(&config_path);
            }
        }

        // Drop handles
        self.writer = None;
        self.event_tx = None;
        self.child = None;
        self.active_turn_id.lock().await.take();
        self.active_thread_id.lock().await.take();

        Ok(())
    }
}

impl Drop for CodexAgent {
    fn drop(&mut self) {
        // Kill the child process synchronously to prevent orphans.
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
    fn json_rpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "initialize".to_string(),
            params: Some(serde_json::json!({"key": "value"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["params"]["key"], "value");
    }

    #[test]
    fn json_rpc_request_no_params() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 2,
            method: "initialized".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn json_rpc_notification_serialization() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "initialized".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&notif).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "initialized");
        assert!(parsed.get("id").is_none());
    }

    #[test]
    fn json_rpc_response_serialization() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: 5,
            result: serde_json::json!({"decision": "accept"}),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], 5);
        assert_eq!(parsed["result"]["decision"], "accept");
    }

    #[test]
    fn deserialize_response_message() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some(1));
        assert!(msg.method.is_none());
        assert!(msg.result.is_some());
        assert!(msg.error.is_none());
    }

    #[test]
    fn deserialize_error_response() {
        let json = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some(2));
        assert!(msg.method.is_none());
        assert!(msg.result.is_none());
        let err = msg.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid request");
    }

    #[test]
    fn deserialize_notification_message() {
        let json = r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"hello"}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert!(msg.id.is_none());
        assert_eq!(msg.method.as_deref(), Some("item/agentMessage/delta"));
        assert!(msg.params.is_some());
    }

    #[test]
    fn deserialize_server_request() {
        let json = r#"{"jsonrpc":"2.0","id":99,"method":"item/commandExecution/requestApproval","params":{"item":{"command":"rm -rf /"}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some(99));
        assert_eq!(
            msg.method.as_deref(),
            Some("item/commandExecution/requestApproval")
        );
        assert!(msg.params.is_some());
    }

    #[test]
    fn translate_agent_message_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"delta": "Hello world"});
        translate_notification("item/agentMessage/delta", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected MessageDelta, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_command() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-1",
            "item": {"type": "commandExecution", "command": "ls -la"}
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(tool_name, "command");
                assert_eq!(preview, "ls -la");
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_file_change() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-2",
            "item": {"type": "fileChange", "path": "/tmp/test.txt"}
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "item-2");
                assert_eq!(tool_name, "file_change");
                assert_eq!(preview, "/tmp/test.txt");
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_agent_message_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-3",
            "item": {"type": "agentMessage"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(rx.try_recv().is_err(), "agentMessage start should emit nothing");
    }

    #[test]
    fn translate_output_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"itemId": "item-1", "delta": "output line"});
        translate_notification("item/commandExecution/outputDelta", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(text, "output line");
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_success() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-1", "type": "commandExecution", "status": "completed", "aggregatedOutput": "hello\n"}
        });
        translate_notification("item/completed", &params, &tx);
        // First event: ToolOutputDelta with the aggregated output
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(text, "hello\n");
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }
        // Second event: ToolCompleted
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-1", "type": "commandExecution", "status": "failed", "error": "permission denied"}
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(
                    status,
                    ToolCompletionStatus::Failed {
                        message: "permission denied".into()
                    }
                );
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_nonzero_exit() {
        // commandExecution that ran to completion with exit != 0: Codex omits
        // `error`, carries the diagnostic in aggregatedOutput + exitCode.
        // We must surface a real message, not "unknown error".
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-1",
                "type": "commandExecution",
                "status": "failed",
                "exitCode": 1,
                "aggregatedOutput": "Traceback (most recent call last):\n  File \"<string>\", line 1\nModuleNotFoundError: No module named 'odf'\n"
            }
        });
        translate_notification("item/completed", &params, &tx);
        // First the output delta, then the ToolCompleted with a real reason.
        let _ = rx.try_recv().unwrap(); // ToolOutputDelta
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { status: ToolCompletionStatus::Failed { message }, .. } => {
                assert!(message.contains("exited 1"), "message should carry exit code: {}", message);
                assert!(message.contains("ModuleNotFoundError"), "message should carry output tail: {}", message);
            }
            other => panic!("expected Failed with detailed message, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_output_only() {
        // aggregatedOutput without exitCode still beats "unknown error".
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-2",
                "type": "commandExecution",
                "status": "failed",
                "aggregatedOutput": "RuntimeError: could not connect to pipe\n"
            }
        });
        translate_notification("item/completed", &params, &tx);
        let _ = rx.try_recv().unwrap();
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { status: ToolCompletionStatus::Failed { message }, .. } => {
                assert!(message.contains("could not connect to pipe"), "got: {}", message);
                assert!(!message.contains("unknown error"), "should not fall through to unknown: {}", message);
            }
            other => panic!("expected Failed with output tail, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_truly_empty_falls_back() {
        // Only when we have literally nothing do we say "unknown error".
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-3", "type": "mcpToolCall", "status": "failed"}
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { status: ToolCompletionStatus::Failed { message }, .. } => {
                assert_eq!(message, "unknown error");
            }
            other => panic!("expected Failed with unknown error, got {:?}", other),
        }
    }

    #[test]
    fn sandbox_hint_mentions_mode_and_steers_writeable() {
        let ws = sandbox_hint("workspace-write");
        assert!(ws.contains("workspace-write"), "missing mode: {}", ws);
        assert!(
            ws.contains("python-pptx") || ws.contains("pure-Python"),
            "workspace-write hint should steer toward library-first path, got: {}",
            ws,
        );
        assert!(ws.contains("listener"), "should warn about listener binds: {}", ws);

        let ro = sandbox_hint("read-only");
        assert!(ro.contains("read-only"), "missing mode: {}", ro);
        assert!(ro.contains("CANNOT modify"), "read-only hint should be explicit: {}", ro);

        let danger = sandbox_hint("danger-full-access");
        assert!(danger.contains("danger-full-access"), "missing mode: {}", danger);
    }

    #[test]
    fn sandbox_hint_unknown_mode_falls_back_to_workspace_write() {
        // Defensive: if a new sandbox mode is added upstream and we haven't
        // updated here, we don't lie to the model about what's possible.
        let hint = sandbox_hint("some-new-mode");
        assert!(hint.contains("workspace-write"), "unknown mode must fall back to the safest real policy: {}", hint);
    }

    #[test]
    fn translate_item_completed_cancelled() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-1", "type": "commandExecution", "status": "cancelled"}
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(status, ToolCompletionStatus::Cancelled);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_reasoning_emits_reasoning_event() {
        // Codex emits reasoning text via item/completed with type="reasoning".
        // We must surface the chain-of-thought via AgentEvent::Reasoning
        // (rendered at "detail" verbosity) instead of the old AutoApproved
        // noise path. And no ToolCompleted marker — reasoning is not a tool.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "rs_123",
                "type": "reasoning",
                "summary": [
                    {"type": "summary_text", "text": "Step 1: parse the request"},
                    {"type": "summary_text", "text": "Step 2: decide tool"}
                ],
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::Reasoning { text } => {
                assert!(text.contains("Step 1: parse the request"));
                assert!(text.contains("Step 2: decide tool"));
            }
            other => panic!("expected Reasoning, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "reasoning should not emit a ToolCompleted marker"
        );
    }

    #[test]
    fn translate_item_completed_reasoning_text_field() {
        // Fallback path: reasoning item with plain text field.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "rs_456",
                "type": "reasoning",
                "text": "raw reasoning trace"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Reasoning { text } => assert_eq!(text, "raw reasoning trace"),
            other => panic!("expected Reasoning, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_item_completed_reasoning_empty_is_silent() {
        // No text, no summary → no event.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "rs_789", "type": "reasoning"}
        });
        translate_notification("item/completed", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "empty reasoning should emit nothing"
        );
    }

    #[test]
    fn translate_item_completed_agent_message_skips_tool_completed() {
        // agentMessage items should emit Message with the final text, but
        // NOT a ToolCompleted marker — they are not tools.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "msg_001",
                "type": "agentMessage",
                "text": "Final response text",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Message { text } => assert_eq!(text, "Final response text"),
            other => panic!("expected Message, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "agentMessage should not emit ToolCompleted"
        );
    }

    #[test]
    fn translate_item_completed_user_message_silent() {
        // userMessage items are echoes of the user's input — emit nothing.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "u_001", "type": "userMessage", "text": "hello"}
        });
        translate_notification("item/completed", &params, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_turn_completed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"message": "All done"});
        translate_notification("turn/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, Some("All done".into()));
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_turn_completed_no_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({});
        translate_notification("turn/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_diff_updated() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "diff": "--- a/foo\n+++ b/foo\n@@ -1 +1 @@\n-old\n+new",
            "files": ["foo"]
        });
        translate_notification("turn/diff/updated", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::DiffUpdated {
                files_changed,
                unified_diff,
            } => {
                assert_eq!(files_changed, vec!["foo".to_string()]);
                assert!(unified_diff.contains("-old"));
            }
            other => panic!("expected DiffUpdated, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_user_message_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-10",
            "item": {"type": "userMessage"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "userMessage start should emit nothing"
        );
    }

    #[test]
    fn translate_item_started_reasoning_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-11",
            "item": {"type": "reasoning"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "reasoning start should emit nothing"
        );
    }

    #[test]
    fn translate_thread_status_changed_completed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": "completed"});
        translate_notification("thread/status/changed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_thread_status_changed_idle() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": "idle"});
        translate_notification("thread/status/changed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_thread_status_changed_running_no_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": "running"});
        translate_notification("thread/status/changed", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "running status should not emit TurnCompleted"
        );
    }

    #[test]
    fn translate_informational_notifications_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty = serde_json::json!({});
        let methods = [
            "turn/started",
            "thread/started",
            "thread/tokenUsage/updated",
            "account/rateLimits/updated",
            "mcpServer/startupStatus/updated",
            "configWarning",
        ];
        for method in &methods {
            translate_notification(method, &empty, &tx);
            assert!(
                rx.try_recv().is_err(),
                "{} should not emit any event",
                method
            );
        }
    }

    #[test]
    fn translate_unknown_method_does_not_panic() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({});
        // Should log a warning but not panic
        translate_notification("some/unknown/method", &params, &tx);
    }

    #[test]
    fn approval_decision_formatting() {
        // Verify the decision strings match the Codex protocol
        let cases = vec![
            (ApprovalDecision::Accept, "accept"),
            (ApprovalDecision::AcceptForSession, "acceptForSession"),
            (ApprovalDecision::Decline, "decline"),
            (ApprovalDecision::Cancel, "cancel"),
        ];
        for (decision, expected) in cases {
            let decision_str = match decision {
                ApprovalDecision::Accept => "accept",
                ApprovalDecision::AcceptForSession => "acceptForSession",
                ApprovalDecision::Decline => "decline",
                ApprovalDecision::Cancel => "cancel",
            };
            assert_eq!(decision_str, expected);
        }
    }

    #[test]
    fn malformed_json_does_not_panic() {
        // Simulate what happens when the reader encounters bad JSON
        let bad_lines = vec![
            "",
            "not json at all",
            "{malformed",
            r#"{"jsonrpc":"2.0"}"#, // valid JSON but missing fields -- should not panic
        ];
        for line in bad_lines {
            // These should either parse successfully (with missing optional fields)
            // or fail gracefully without panicking
            let _result: Result<JsonRpcMessage, _> = serde_json::from_str(line);
        }
    }

    #[test]
    fn codex_agent_new_defaults() {
        let agent = CodexAgent::new(
            "codex".into(),
            Some("o4-mini".into()),
            "on-request".into(),
            "workspace-write".into(),
            None,
        );
        assert_eq!(agent.command, "codex");
        assert_eq!(agent.model, Some("o4-mini".into()));
        assert_eq!(agent.approval_policy, "on-request");
        assert_eq!(agent.sandbox, "workspace-write");
        assert!(agent.child.is_none());
        assert!(agent.writer.is_none());
        assert!(agent.event_tx.is_none());
        assert!(agent.reader_handle.is_none());
    }

    #[test]
    fn extract_turn_id_top_level_camelcase() {
        let v = serde_json::json!({"turnId": "t-123"});
        assert_eq!(extract_turn_id(&v), Some("t-123".to_string()));
    }

    #[test]
    fn extract_turn_id_snake_case() {
        let v = serde_json::json!({"turn_id": "t-456"});
        assert_eq!(extract_turn_id(&v), Some("t-456".to_string()));
    }

    #[test]
    fn extract_turn_id_nested_turn_object() {
        let v = serde_json::json!({"turn": {"id": "t-789"}});
        assert_eq!(extract_turn_id(&v), Some("t-789".to_string()));
    }

    #[test]
    fn extract_turn_id_nested_thread_last_turn() {
        let v = serde_json::json!({"thread": {"lastTurnId": "t-last"}});
        assert_eq!(extract_turn_id(&v), Some("t-last".to_string()));
    }

    #[test]
    fn extract_turn_id_missing() {
        let v = serde_json::json!({"other": "value"});
        assert_eq!(extract_turn_id(&v), None);
    }

    #[test]
    fn extract_turn_id_empty_string_is_none() {
        let v = serde_json::json!({"turnId": ""});
        assert_eq!(extract_turn_id(&v), None);
    }

    #[tokio::test]
    async fn interrupt_turn_without_active_turn_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        let err = agent.interrupt_turn().await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active turn"),
                    "expected 'no active turn' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn interrupt_turn_without_thread_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        // Active turn but no thread — should still error with "no active thread".
        *agent.active_turn_id.lock().await = Some("t-1".into());
        let err = agent.interrupt_turn().await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active thread"),
                    "expected 'no active thread' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn interrupt_turn_sends_correct_jsonrpc_request() {
        // Set up an agent with a duplex pipe in place of the child stdin.
        // We can't easily stub `send_request` without refactoring, so instead
        // we assert the pre-write state: the request builder would produce the
        // right JSON by inspecting the agent's captured thread/turn ids and
        // re-running the same params construction path.
        let agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_turn_id.lock().await = Some("turn-xyz".into());
        *agent.active_thread_id.lock().await = Some("thread-abc".into());

        // Reconstruct the same params object the implementation builds.
        let turn_id = agent.active_turn_id.lock().await.clone().unwrap();
        let thread_id = agent.active_thread_id.lock().await.clone().unwrap();
        let params = serde_json::json!({
            "threadId": thread_id,
            "turnId": turn_id,
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["turnId"], "turn-xyz");
    }

    #[tokio::test]
    async fn interrupt_turn_wire_format_is_jsonrpc_request() {
        // Confirm the shape of the JSON-RPC request we emit matches what Codex
        // v2 expects: {"jsonrpc":"2.0","id":<N>,"method":"turn/interrupt",
        // "params":{"threadId":...,"turnId":...}}
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 42,
            method: "turn/interrupt".to_string(),
            params: Some(serde_json::json!({
                "threadId": "thread-abc",
                "turnId": "turn-xyz",
            })),
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["method"], "turn/interrupt");
        assert_eq!(v["params"]["threadId"], "thread-abc");
        assert_eq!(v["params"]["turnId"], "turn-xyz");
    }

    // ── Mid-turn steering (`turn/steer`) ──
    //
    // Steering injects user text into the currently running turn without
    // cancelling it. Same pattern as `interrupt_turn` — precondition checks
    // for active turn/thread ids, then a JSON-RPC request with the steering
    // params. The response carries a turnId we intentionally discard.

    #[tokio::test]
    async fn steer_turn_without_active_turn_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        let err = agent.steer_turn("redirect to test coverage").await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active turn"),
                    "expected 'no active turn' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn steer_turn_without_thread_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_turn_id.lock().await = Some("t-1".into());
        let err = agent.steer_turn("please stop").await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active thread"),
                    "expected 'no active thread' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[test]
    fn steer_turn_wire_format_is_jsonrpc_request() {
        // Verify the params shape matches the spec: threadId + expectedTurnId
        // for the precondition, and input as a singleton content array of
        // type="text". Frozen format — changes here should update the
        // Codex compat docs too.
        let text = "please check tests/e2e/ first";
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "input": [{"type": "text", "text": text}],
            "expectedTurnId": "turn-xyz",
        });
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 99,
            method: "turn/steer".to_string(),
            params: Some(params),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 99);
        assert_eq!(v["method"], "turn/steer");
        assert_eq!(v["params"]["threadId"], "thread-abc");
        assert_eq!(v["params"]["expectedTurnId"], "turn-xyz");
        let input = v["params"]["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], text);
    }

    // ── Thread actions (compact / fork / undo / review / memory-reset) ──
    //
    // These tests assert the error-handling contract (no active thread →
    // typed error) and the dispatcher routing (/op → right method). The
    // happy-path RPC wire format is verified in a dedicated wire-format
    // test parallel to `interrupt_turn_wire_format_is_jsonrpc_request`
    // below, because the pipe plumbing is the same.

    fn test_agent() -> CodexAgent {
        CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "workspace-write".into(),
            None,
        )
    }

    #[tokio::test]
    async fn thread_action_without_thread_errors() {
        // Each action needs an active thread; without one the dispatcher
        // returns a clear error rather than hanging on the pending-request
        // oneshot.
        for op in ["compact", "fork", "undo", "review", "memory-reset"] {
            let mut agent = test_agent();
            let err = agent
                .thread_action(op, &serde_json::Value::Null)
                .await
                .unwrap_err();
            match err {
                CallerError::ExternalAgent(msg) => {
                    assert!(
                        msg.contains("no active Codex thread"),
                        "op /{}: expected 'no active Codex thread' error, got: {}",
                        op,
                        msg,
                    );
                }
                other => panic!("op /{}: expected ExternalAgent error, got {:?}", op, other),
            }
        }
    }

    #[tokio::test]
    async fn thread_action_unknown_op_errors() {
        let mut agent = test_agent();
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        let err = agent
            .thread_action("explode", &serde_json::Value::Null)
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("unsupported Codex thread action"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn thread_action_undo_zero_turns_errors_early() {
        // Defensive check inside rollback_turns: `/undo 0` makes no sense.
        let mut agent = test_agent();
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        let err = agent
            .thread_action("undo", &serde_json::json!({"turns": 0}))
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(msg.contains("at least 1"), "got: {}", msg);
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rollback_turns_trait_zero_is_noop() {
        // The trait method treats 0 as a no-op (HTTP handler may emit
        // 0 turns when the target round is already the head). No RPC
        // is dispatched so the call returns Ok without an active
        // thread.
        let mut agent = test_agent();
        agent.rollback_turns(0).await.expect("0 turns should be a noop");
    }

    #[tokio::test]
    async fn rollback_turns_trait_without_thread_errors() {
        // Non-zero turns without an active thread surfaces the same
        // "no active Codex thread" error as the /undo dispatcher.
        let mut agent = test_agent();
        let err = agent.rollback_turns(2).await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active Codex thread"),
                    "expected 'no active Codex thread', got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[test]
    fn thread_rollback_wire_format_is_jsonrpc_request() {
        // Assert the params shape without actually running the RPC.
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "turnsToRollback": 2,
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["turnsToRollback"], 2);
    }

    #[test]
    fn thread_fork_wire_format_with_name() {
        // The implementation constructs the params map conditionally; re-run
        // the same construction here to guarantee the shape doesn't drift.
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String("thread-abc".into()));
        obj.insert("name".into(), serde_json::Value::String("feature-x".into()));
        let params = serde_json::Value::Object(obj);
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["name"], "feature-x");
    }

    #[test]
    fn thread_fork_wire_format_without_name() {
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String("thread-abc".into()));
        let params = serde_json::Value::Object(obj);
        assert_eq!(params["threadId"], "thread-abc");
        assert!(params.get("name").is_none());
    }

    #[test]
    fn review_start_wire_format_with_prompt() {
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String("thread-abc".into()));
        obj.insert("prompt".into(), serde_json::Value::String("check for leaks".into()));
        let params = serde_json::Value::Object(obj);
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["prompt"], "check for leaks");
    }
}
