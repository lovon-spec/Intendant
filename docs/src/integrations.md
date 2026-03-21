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

The `--web` flag starts a web server that serves the app dashboard and bridges WebSocket connections to the EventBus. `--web` implies `--mcp`, so no initial task is required — the agent starts idle and accepts tasks dynamically.

See [Web Dashboard](./web-dashboard.md) for the full dashboard documentation and [Presence Layer](./presence.md) for details on the presence session protocol and mutual exclusion.

### How It Works

```
Browser ──WebSocket──> Intendant web gateway (port 8765)
  │                              │
  │  Terminal I/O (ANSI)         │  Events (broadcast to all clients)
  │  Key/resize input            │  Tool responses (per-connection direct channel)
  │  Tool requests               │  State snapshot + log replay (on connect)
  │  presence_connect/disconnect │  Presence welcome (on voice connect)
  │  Voice logs/checkpoints      │  Per-connection TUI frames
  │  Audio for transcription     │
  v                              v
App dashboard (WASM)        EventBus + AgentStateSnapshot
  +                              │
Optional: browser-side           │  Dual outbound channels:
live model (Gemini/OpenAI)       │  - broadcast::Receiver (events)
  │                              │  - mpsc::unbounded (direct responses)
  │  (function calls → tool_request)
  v
Intendant agent loop
```

The web gateway has three layers:

1. **App dashboard** — The primary web interface at `/` with 4 tabs (Activity, Usage, Terminal, Displays). State management is handled by `presence-web` WASM. Events are broadcast and late-connecting browsers get a full log replay.

2. **Per-connection TUI rendering** — Each WebSocket connection gets its own `WebTui` instance with independent terminal dimensions. ANSI output is sent per-connection via the direct channel, not broadcast.

3. **Presence bridge** (optional) — When a browser connects a live model (Gemini Live / OpenAI Realtime), the model uses 9 presence tools that map to `tool_request` WebSocket messages. The gateway handles these server-side and returns `tool_response` messages on the per-connection direct channel.

### WebSocket Protocol

#### Inbound Messages (browser → server)

| Message | Description |
|---------|-------------|
| `{"t":"key","key":"..."}` | Keyboard input (routed to per-connection WebTui) |
| `{"t":"resize","cols":N,"rows":N}` | Terminal resize (per-connection) |
| `{"t":"presence_connect",...}` | Presence session protocol — replaces server-side presence |
| `{"t":"presence_disconnect"}` | Disconnect presence — resumes server-side presence |
| `{"t":"make_active"}` | Request active voice ownership (handover) |
| `{"t":"voice_log","text":"...","seq":N}` | Voice transcript from browser presence model |
| `{"t":"presence_checkpoint","summary":"...","last_event_seq":N}` | Context checkpoint |
| `{"t":"voice_diagnostic","kind":"...","detail":"..."}` | Browser voice diagnostics |
| `{"t":"user_audio","data":"<base64>"}` | PCM16 audio for server-side transcription |
| `{"t":"tool_request","id":"...","tool":"...","args":{}}` | Presence tool call |
| `{"t":"async_query","id":"...","tool":"...","args":{}}` | Async query (result as text, not tool response) |
| `{"action":"..."}` | ControlMsg (same as Unix control socket) |
| `{"t":"live_connected"}` / `{"t":"live_disconnected"}` | Legacy (still accepted) |

#### Outbound Messages (server → browser)

| Message | Description |
|---------|-------------|
| `{"t":"term","d":"<base64>"}` | Per-connection TUI ANSI output |
| `{"t":"state_snapshot","state":{...},"connection_id":"...","config":{...},"session_id":"..."}` | Bootstrap on connect |
| `{"t":"log_replay","entries":[...]}` | Historical session events for late-connecting browsers |
| `{"t":"presence_welcome","session_id":"...","state":{...},"events":[...],"is_active":bool,"conversation_context":"..."}` | Presence session welcome |
| `{"t":"active_granted","is_active":true,"handover_context":"...","conversation_context":"..."}` | Active ownership granted |
| `{"t":"force_disconnect_voice","reason":"handover"}` | Sent to old active on handover |
| `{"t":"presence_checkpoint_ack","seq":N}` | Checkpoint acknowledgement |
| `{"t":"tool_response","id":"...","result":"..."}` | Response to a tool_request |
| `{"t":"async_query_result","id":"...","tool":"...","result":"..."}` | Response to async_query |
| `{"event":"..."}` | OutboundEvent broadcast (status, agent_output, approval_required, etc.) |

#### Tool Request/Response Protocol

The browser live model calls presence tools via tagged request/response messages:

```json
// Browser sends:
{"t":"tool_request","id":"req-42","tool":"check_status","args":{}}

// Server responds (on direct channel):
{"t":"tool_response","id":"req-42","result":"Phase: Running agent (turn 5). Budget: 23% used."}
```

**Action tools** (`submit_task`, `approve_action`, `deny_action`, `skip_action`, `respond_to_question`, `set_autonomy`) are dispatched via the EventBus — the same path as TUI key presses and control socket commands.

**Query tools** (`check_status`, `query_detail`, `recall_memory`) are handled asynchronously server-side via `presence::handle_tool_query()`, which reads from the shared `AgentStateSnapshot`, project files, and knowledge store.

#### State Bootstrap

On WebSocket connect, the server sends multiple bootstrap messages:

1. **`state_snapshot`** — Full `AgentStateSnapshot` with `connection_id`, config, and `session_id`
2. **Cached `usage_update`** — Latest token usage data
3. **Cached `status`** — Latest status (autonomy, session_id, task)
4. **Cached `display_ready`** — Latest display info for VNC slots
5. **`log_replay`** — Historical session events parsed from `session.jsonl`

This ensures late-connecting browsers see the complete state immediately.

### HTTP Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /` | App dashboard (4-tab UI: Activity, Usage, Terminal, Displays) |
| `GET /live` | Legacy xterm.js terminal + voice overlay |
| `GET /config` | Live model configuration JSON |
| `GET /debug` | Debug JSON (agent state, voice connection, active browser) |
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime |
| `GET /wasm-web/*` | WASM and JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for microphone capture |
| `WS /` | Main WebSocket (events, terminal I/O, presence protocol) |
| `WS /vnc` | WebSocket-to-TCP VNC proxy for noVNC display viewing |

### Requirements

- **Microphone access requires a secure context**: Use `localhost` (via SSH tunnel: `ssh -L 8765:localhost:8765 host`), or set browser flags for insecure origins.
- **API key for voice**: Gemini or OpenAI. The key is used browser-side only. Voice is optional — the dashboard works without it.

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
