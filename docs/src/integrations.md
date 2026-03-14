# Integrations

This chapter covers the control socket (Unix domain socket) and web gateway (WebSocket) integration points. For the MCP server interface, see [MCP Server](./mcp-server.md). For the presence layer that mediates user interaction, see [Presence Layer](./presence.md).

## Control Socket

When `--control-socket` is enabled, a Unix domain socket is created at `/tmp/intendant-<pid>.sock`. This enables programmatic control of a running Intendant instance from external scripts and tools.

- Outbound event broadcast to all connected clients
- Inbound command handling for status, approval, denial, human input, autonomy change, quit, controller-restart workflow commands, and controller-loop intervention commands (in MCP mode)
- Socket server is opt-in via `--control-socket`

### Inbound Commands (JSON-line)

```json
{"action": "status"}
{"action": "approve", "id": 123}
{"action": "deny", "id": 123}
{"action": "input", "text": "answer to askHuman"}
{"action": "set_autonomy", "level": "high"}
{"action": "schedule_controller_restart", "controller_id":"codex", "north_star_goal":"audit and improve", "restart_after":"turn_end"}
{"action": "controller_turn_complete", "restart_id":"<id>", "turn_complete_token":"<token>", "status":"ok", "handoff_summary":"..."}
{"action": "get_restart_status"}
{"action": "cancel_controller_restart", "restart_id":"<id>"}
{"action": "request_controller_loop_halt", "persistent": true}
{"action": "clear_controller_loop_halt"}
{"action": "intervene_controller_loop", "mode":"stop"}
{"action": "get_controller_loop_status"}
{"action": "query_detail", "scope": "diff"}
{"action": "query_detail", "scope": "file", "target": "src/main.rs"}
{"action": "recall_memory", "keywords": ["auth", "login"], "channel": "project_state"}
{"action": "usage"}
{"action": "quit"}
```

### Outbound Events (streamed to connected clients)

```json
{"event": "turn_started", "turn": 5, "budget_pct": 12.3}
{"event": "agent_output", "stdout": "...", "stderr": "..."}
{"event": "approval_required", "id": 123, "command": "rm -rf /tmp/test"}
{"event": "ask_human", "question": "Which database?"}
{"event": "task_complete", "reason": "done signal"}
{"event": "status", "turn": 3, "phase": "thinking", "autonomy": "medium", "session_id": "abc-123", "task": "fix tests"}
{"event": "usage", "main": {"provider": "openai", "model": "gpt-5", "tokens_used": 12000, "context_window": 128000, "usage_pct": 9.4}}
{"event": "usage_update", "main": {"provider": "openai", "model": "gpt-5", "tokens_used": 15000, "context_window": 128000, "usage_pct": 11.7}}
{"event": "command_result", "action": "get_restart_status", "ok": true, "message": "ok", "data": {...}}
```

- The `status` event now includes `session_id` and `task` fields.
- The `usage` event is a response to `{"action": "usage"}`, returning per-model token usage.
- The `usage_update` event is broadcast automatically after each agent turn, providing streaming token consumption updates. The `presence` field is included when the presence layer is active.

`command_result.ok` is `false` when a control action fails (for example, `schedule_controller_restart` with `restart_after="now"` and no executable restart action configured).

### Example Usage

```bash
echo '{"action":"status"}' | socat - UNIX:/tmp/intendant-$(pgrep intendant).sock
```

## Web Gateway

The `--web` flag starts a WebSocket server that serves a remote TUI (xterm.js) and optionally enables browser-side live model interaction (voice/text) via the Gemini Live API or OpenAI Realtime API. `--web` implies `--mcp`, so no initial task is required — the agent starts idle and accepts tasks dynamically.

See [TUI & Autonomy — Web TUI](./tui.md#web-tui) for setup instructions and [Presence Layer](./presence.md) for details on the mutual exclusion between server-side and browser-side presence.

### How It Works

```
Browser ──WebSocket──> Intendant web gateway (port 8765)
  │                              │
  │  Terminal I/O (ANSI)         │  Events (broadcast to all clients)
  │  Key/resize input            │  Tool responses (per-connection direct channel)
  │  Tool requests               │  State snapshot (on connect)
  │  live_connected/disconnected │
  v                              v
xterm.js terminal           EventBus + AgentStateSnapshot
  +                              │
Optional: browser-side           │  Dual outbound channels:
live model (Gemini/OpenAI)       │  - broadcast::Receiver (events)
  │                              │  - mpsc::unbounded (direct responses)
  │  (function calls → tool_request)
  v
Intendant agent loop
```

The web gateway has two layers:

1. **Remote TUI** — The TUI renders to a buffer-backed ratatui backend (`WebTui`). ANSI output is broadcast to all connected WebSocket clients as base64-encoded `{"t":"term","d":"..."}` messages. Key presses and terminal resizes from the browser are sent back and injected into the TUI event loop.

2. **Presence bridge** (optional) — When a browser connects a live model (Gemini Live / OpenAI Realtime), the model uses 9 presence tools that map to `tool_request` WebSocket messages. The gateway handles these server-side and returns `tool_response` messages on a per-connection direct channel.

### WebSocket Protocol

#### Inbound Messages (browser → server)

| Message | Description |
|---------|-------------|
| `{"t":"key","key":"..."}` | Keyboard input (crossterm key format) |
| `{"t":"resize","cols":N,"rows":N}` | Terminal resize |
| `{"t":"live_connected"}` | Browser live model connected — pauses server-side presence |
| `{"t":"live_disconnected"}` | Browser live model disconnected — resumes server-side presence |
| `{"t":"tool_request","id":"...","tool":"...","args":{}}` | Presence tool call from browser live model |

#### Outbound Messages (server → browser)

| Message | Description |
|---------|-------------|
| `{"t":"term","d":"<base64>"}` | TUI ANSI output (broadcast) |
| `{"t":"state_snapshot","state":{...}}` | Full `AgentStateSnapshot` on connect (bootstrap) |
| `{"t":"event","event":{...}}` | Agent event from EventBus (broadcast) |
| `{"t":"tool_response","id":"...","result":"..."}` | Response to a tool_request (direct, per-connection) |

#### Tool Request/Response Protocol

The browser live model calls presence tools via tagged request/response messages:

```json
// Browser sends:
{"t":"tool_request","id":"req-42","tool":"check_status","args":{}}

// Server responds (on direct channel):
{"t":"tool_response","id":"req-42","result":"Phase: Running agent (turn 5). Budget: 23% used."}
```

**Action tools** (`submit_task`, `approve_action`, `deny_action`, `skip_action`, `respond_to_question`, `set_autonomy`) are dispatched via the EventBus — the same path as TUI key presses and control socket commands.

**Query tools** (`check_status`, `query_detail`, `recall_memory`) are handled synchronously server-side via `presence::handle_tool_query()`, which reads from the shared `AgentStateSnapshot`, project files, and knowledge store.

#### State Bootstrap

On WebSocket connect, if a `WebQueryCtx` is available (presence layer active), the server sends a `state_snapshot` message containing the full `AgentStateSnapshot`. This lets the browser reconstruct agent state without replaying the event history:

```json
{
  "t": "state_snapshot",
  "state": {
    "phase": "thinking",
    "turn": 3,
    "budget_pct": 8.5,
    "autonomy": "medium",
    "last_command": "cargo test",
    "pending_approval": null,
    "pending_question": null
  }
}
```

Subsequent state changes arrive as incremental `event` messages.

### Mutual Exclusion

Only one presence model runs at a time — server-side text or browser-side live:

1. Browser connects live model → sends `{"t":"live_connected"}`
2. Web gateway emits `AppEvent::LiveConnected` → sets shared `AtomicBool` pause flag
3. Server-side `PresenceLayer::handle_event()` returns `Ok(None)` while paused
4. Browser live model handles all presence duties (narration, tool calls, user interaction)
5. Browser disconnects → sends `{"t":"live_disconnected"}`
6. Web gateway emits `AppEvent::LiveDisconnected` → clears pause flag
7. Server-side presence resumes

### Running

```bash
# Start idle, waiting for tasks via web UI (default port 8765)
./target/release/intendant --web

# Custom port
./target/release/intendant --web 9000
```

Open `http://<host>:8765/` on your phone or browser. The xterm.js terminal shows the full TUI remotely. To enable voice, enter your API key on first visit (Gemini or OpenAI; stored in browser localStorage, never sent to Intendant).

### HTTP Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /` | Serves `static/live.html` (xterm.js terminal + optional voice overlay) |
| `GET /config` | Returns configured live model provider/model as JSON |
| `GET /ws` | WebSocket upgrade endpoint |

### Requirements

- **Microphone access requires a secure context**: Use `localhost` (via SSH tunnel: `ssh -L 8765:localhost:8765 host`), or set browser flags for insecure origins.
- **API key for voice**: Gemini (free tier from Google AI Studio) or OpenAI. The key is used browser-side only. Voice is optional — the remote TUI works without it.

### Supported Tools (Browser Live Model)

| Tool | Type | Description |
|------|------|-------------|
| `submit_task` | Action | Submit a new task to the agent loop |
| `approve_action` | Action | Approve a pending action |
| `deny_action` | Action | Deny a pending action |
| `skip_action` | Action | Skip a pending action |
| `respond_to_question` | Action | Answer an `askHuman` question |
| `set_autonomy` | Action | Change autonomy level |
| `check_status` | Query | Get current agent phase, turn, budget |
| `query_detail` | Query | Get git diff, file contents, or log details |
| `recall_memory` | Query | Search the knowledge store by keywords/channel |
