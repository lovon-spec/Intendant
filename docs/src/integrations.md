# Integrations

## Control Socket

When `--control-socket` is enabled, a Unix domain socket is created at `/tmp/intendant-<pid>.sock`.

- Outbound event broadcast is implemented.
- Inbound command handling is implemented for status, approval, denial, human input, autonomy change, quit, controller-restart workflow commands, and controller-loop intervention commands (in MCP mode).
- Socket server is opt-in via `--control-socket`.

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
{"action": "quit"}
```

### Outbound Events (streamed to connected clients)

```json
{"event": "turn_started", "turn": 5, "budget_pct": 12.3}
{"event": "agent_output", "stdout": "...", "stderr": "..."}
{"event": "approval_required", "id": 123, "command": "rm -rf /tmp/test"}
{"event": "ask_human", "question": "Which database?"}
{"event": "task_complete", "reason": "done signal"}
{"event": "status", "turn": 3, "phase": "thinking", "autonomy": "medium"}
{"event": "command_result", "action": "get_restart_status", "ok": true, "message": "ok", "data": {...}}
```

`command_result.ok` is `false` when a control action fails (for example, `schedule_controller_restart` with `restart_after="now"` and no executable restart action configured).

### Example Usage

```bash
echo '{"action":"status"}' | socat - UNIX:/tmp/intendant-$(pgrep intendant).sock
```

## Voice Gateway

The `--voice-gateway` flag starts a WebSocket server that enables voice control of Intendant from a phone browser via the Gemini Live API.

### How It Works

```
Phone browser ──WebSocket──> Intendant gateway (port 8765)
     │                              │
     │  (audio)                     │ (ControlMsg JSON, same as control socket)
     v                              v
Gemini Live API            EventBus / broadcast channel
     │                              │
     │  (function calls)            │ (OutboundEvent JSON)
     v                              v
  JS bridge ──────────────> Intendant agent loop
```

The phone browser connects directly to the Gemini Live API for low-latency voice I/O, and to the Intendant gateway for control messages. A JS bridge in the browser translates Gemini function calls (`submit_task`, `approve_action`, `check_status`, etc.) into Intendant `ControlMsg` JSON and injects Intendant events back into the Gemini session for voice narration.

### Running

```bash
# With MCP mode
./target/release/intendant --mcp --voice-gateway

# With TUI
./target/release/intendant --voice-gateway "Fix the login bug"

# Custom port
./target/release/intendant --voice-gateway 9000 --mcp
```

Open `http://<host>:8765/` on your phone. On first visit, enter your [Google AI Studio API key](https://aistudio.google.com/apikey) (stored in browser localStorage, never sent to Intendant).

### Requirements

- **Microphone access requires a secure context**: Use `localhost` (via SSH tunnel: `ssh -L 8765:localhost:8765 host`), or set browser flags for insecure origins.
- **Gemini API key**: Free tier from Google AI Studio. The key is used browser-side only.

### Voice Identity

The voice gateway speaks in first person as Intendant — "I'm running your tests now" rather than "The agent is running tests." Event narration from the agent loop is rewritten into first-person system messages before being injected into the Gemini session.

### Supported Voice Commands

| Voice command | Maps to |
|---|---|
| "List files in /tmp" | `submit_task({description: "list files in /tmp"})` |
| "What's the status?" | `check_status()` |
| "Approve that" | `approve_action({id: N})` |
| "No, skip it" | `skip_action({id: N})` |
| "Set autonomy to full" | `set_autonomy({level: "full"})` |
| "The answer is PostgreSQL" | `respond_to_question({text: "PostgreSQL"})` |
| "Show me the diff" | `query_detail({scope: "diff"})` |
| "What do you know about auth?" | `recall_memory({keywords: ["auth"]})` |
