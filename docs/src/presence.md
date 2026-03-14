# Presence Layer

The presence layer is the conversational interface between the user and the agent system. It mediates all interaction: the user talks to presence, presence delegates work via `submit_task`, and narrates progress as events stream back from the agent loop.

## Architecture

Only one presence model is active at a time — either server-side text presence OR browser-side live presence (Gemini Live / OpenAI Realtime). Never both simultaneously.

```
User input ──▶ [Presence Layer] ──▶ submit_task ──▶ Agent Loop
                     │                                  │
                     │◀── events (phase, approval, etc) ◀┘
                     │
                     ▼
              Narration to user (TUI / Web)
```

## Server-Side Text Presence

The default mode. `PresenceLayer` wraps a small/fast text model (e.g., `gemini-2.5-flash`) and maintains its own `Conversation` separate from the agent's.

### Behavior

- Processes user input via `process_user_input()` — decides whether to handle directly or delegate to the agent loop
- Narrates agent events via `handle_event()` — translates phase changes, approvals, completions into conversational updates
- Handles status queries, memory recall, and autonomy changes directly without involving the agent loop
- Uses its own system prompt (`SysPrompt_presence.md`) — standalone, not appended to the base agent prompt
- Follow-up input in the TUI is routed through the presence layer when active

### Configuration

```toml
[presence]
enabled = true                # default: true
provider = "gemini"           # provider for the presence model (optional)
model = "gemini-2.5-flash"    # model for the presence layer (optional)
context_window = 32768        # context window for presence conversation (default: 32768)
```

Or via environment variables:
- `PRESENCE_PROVIDER` — override provider (fallback: `PROVIDER`)
- `PRESENCE_MODEL` — override model

Disable with `--no-presence` flag or `[presence] enabled = false` in `intendant.toml`.

## Browser-Side Live Presence

When `--web` is used and a browser connects a live model (Gemini Live / OpenAI Realtime), the browser sends a `live_connected` message over WebSocket. This pauses server-side presence — `PresenceLayer::handle_event()` returns `Ok(None)` while paused. The browser's live model takes over as the conversational front-end, using the same 9 tools via the WebSocket tool request/response protocol.

When the browser's live model disconnects (page close, error), a `live_disconnected` message is sent and server-side presence resumes automatically.

### Configuration

```toml
[presence]
audio_model = "gemini-2.5-flash-live"  # model for browser-side live presence (optional)
```

Voice requires an API key (Gemini or OpenAI), stored in browser localStorage. The key is used browser-side only — it is never sent to the Intendant server.

## Presence Tools

The presence layer has 9 tools, defined in the `presence-core` workspace crate:

### Action Tools

| Tool | Description |
|------|-------------|
| `submit_task` | Submit a new task to the agent loop |
| `approve_action` | Approve a pending action |
| `deny_action` | Deny a pending action |
| `skip_action` | Skip a pending action |
| `respond_to_question` | Answer an `askHuman` question |
| `set_autonomy` | Change autonomy level |

Action tools dispatch via the EventBus as `ControlMsg` — the same path as TUI key presses and control socket commands.

### Query Tools

| Tool | Description |
|------|-------------|
| `check_status` | Read current `AgentStateSnapshot` (phase, turn, budget, pending approval/question) |
| `query_detail` | Get git diff, file contents, or log details from the project |
| `recall_memory` | Search the knowledge store by keywords, with optional channel/tag filters; falls back to session log |

Query tools are handled synchronously server-side. They are shared between `PresenceLayer` and the web gateway via standalone functions in `presence.rs`.

## Event Filtering

Not all agent events are worth narrating. The presence layer classifies events as:

**Push-worthy** (trigger narration):
- `TaskSubmitted`, `TaskComplete`
- `ApprovalRequired`, `HumanQuestion`
- `PhaseChanged` (debounced to avoid rapid phase flip noise)
- `ContextManagement`

**Pull-only** (available on request via `check_status`):
- Status snapshots, log entries, token usage updates

## Mutual Exclusion

The presence layer enforces mutual exclusion between server-side and browser-side presence:

1. Browser connects live model → sends `{"t":"live_connected"}`
2. Web gateway emits `AppEvent::LiveConnected` → sets shared `AtomicBool` pause flag
3. Server-side `PresenceLayer::handle_event()` returns `Ok(None)` while paused
4. Browser live model handles all presence duties (narration, tool calls, user interaction)
5. Browser disconnects → sends `{"t":"live_disconnected"}`
6. Web gateway emits `AppEvent::LiveDisconnected` → clears pause flag
7. Server-side presence resumes

## presence-core Crate

The `crates/presence-core/` workspace crate contains the WASM-compatible core logic:

- **Types**: `PresenceConfig`, `TaskEnvelope`, `PresenceEvent`, `AgentStateSnapshot`, constants
- **Dispatch**: `PresenceAction` enum, `dispatch_tool_call()` — pure logic dispatch
- **Tools**: 9 presence tool definitions (provider-agnostic `ToolDefinition` format)
- **Format**: `format_event()`, `truncate()` (unicode-safe)
- **Prompt**: `DEFAULT_PRESENCE_PROMPT` via `include_str!`

Minimal dependencies (serde + serde_json only, no tokio/reqwest). Compiles to both native and `wasm32-unknown-unknown`. The main crate re-exports its types and converts `ToolDefinition` to the provider-specific format.

## Tool Dispatch Flow

Tool dispatch uses `presence_core::dispatch_tool_call()` which returns a `PresenceAction` enum:

```
Tool call arrives (from text model or browser live model)
    │
    ▼
dispatch_tool_call() → PresenceAction
    │
    ├── TextResult(text) → return immediately
    ├── SubmitTask(envelope) → send to EventBus
    ├── Approve/Deny/Skip → send ControlMsg to EventBus
    ├── SetAutonomy(level) → send ControlMsg to EventBus
    └── NeedsIO(query) → platform layer handles:
         ├── check_status → read AgentStateSnapshot
         ├── query_detail → read files, git diff
         └── recall_memory → search knowledge store + session log
```

Pure-logic tools return `TextResult`/`SubmitTask`/`Approve`/etc. I/O-dependent tools return `NeedsIO` for the platform layer to handle, keeping `presence-core` free of I/O dependencies.
