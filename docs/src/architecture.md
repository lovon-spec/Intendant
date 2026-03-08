# Architecture

```
stdin (JSON) --> intendant-runtime --> executes commands sequentially (blocking)
                  |
                  +--> in-memory process state (HashMap<nonce, ProcessInfo>)
                  +--> $INTENDANT_LOG_DIR/  (stdout/stderr logs per nonce)
                  |
                  +--> stdout (result lines with exit code, stdout/stderr tail)

intendant (3 modes) --> detects project root (git) --> loads memory/knowledge
  |
  +--> User Mode:       spawns orchestrator subprocess, monitors progress (no API calls)
  +--> Sub-Agent Mode:  scoped task, writes results/progress, isolated context
  +--> Direct Mode:     single-loop execution for simple tasks
  |
  +--> Native tool calling (OpenAI/Anthropic/Gemini) with text extraction fallback
  +--> Streaming output:  SSE-based token streaming for all 3 providers
  +--> Ratatui TUI:     status bar, scrollable log, approval panel, askHuman input
  +--> MCP Server:      --mcp flag, stdio transport, full parity with TUI (tools + resources)
  +--> MCP Client:      connects to external MCP servers (configured in intendant.toml)
  +--> Autonomy system: Low/Medium/High/Full + per-category rules from intendant.toml
  +--> Landlock sandbox: filesystem restrictions on agent runtime (Linux)
  +--> Prompt caching:  Anthropic cache_control, OpenAI/Gemini implicit caching
  +--> Auto-compaction: triggers at 90% context usage, preserves system+tail messages
  +--> Optional control socket (--control-socket): /tmp/intendant-<pid>.sock (JSON-line protocol)
  |       status (with session_id, task), usage, usage_update events
  +--> Web gateway (--web): WebSocket server for remote TUI + browser-side live model (voice/text)
  +--> Token budget tracking (context-window-aware loop termination)
  +--> Sub-agent spawning via env vars (INTENDANT_ROLE, INTENDANT_ID, etc.)
  +--> Git worktree isolation for implementation agents
  +--> Tagged knowledge store with pub/sub channels between agents
  +--> Presence layer: conversational mediator between user and agent loop
```

- **Process State:** In-memory `HashMap<u64, ProcessInfo>` tracking nonce, PID, status, exit code, and timestamp. Ephemeral — does not survive binary restarts.
- **Session Directory (`~/.intendant/logs/<uuid>/`):** Per-session directory with UUID-based naming. Contains per-nonce stdout/stderr log files, structured session logs, conversation history, and askHuman IPC files. The log directory is passed to the runtime via `INTENDANT_LOG_DIR`.
- **Execution Model:** Commands are processed sequentially. Each command blocks until completion and returns its result directly (exit code, stdout tail, stderr tail). The runtime exits after processing all commands.

## Execution Modes

`intendant` operates in one of three modes, selected automatically:

### Sub-Agent Mode

Activated when `INTENDANT_ROLE` env var is set:
- Runs as a child agent with a scoped task
- Writes periodic progress to `INTENDANT_PROGRESS_FILE`
- Writes final results (summary, findings, artifacts, token usage) to `INTENDANT_RESULT_FILE`
- Uses role-specific system prompts (`SysPrompt_research.md`, `SysPrompt_implementation.md`, etc.)

### User Mode

Activated for complex tasks without `INTENDANT_ROLE`:
- Pure subprocess monitor — makes zero model API calls at Layer 0
- Spawns an orchestrator sub-agent as a child process via `tokio::process::Command`
- Polls the orchestrator's progress file every 500ms, relays status to the TUI or stdout
- Reads the orchestrator's result file on exit; synthesizes a failure if the process crashes
- `kill_on_drop(true)` ensures the orchestrator is terminated if the user quits the TUI

### Direct Mode

Activated for simple tasks without `INTENDANT_ROLE`:
- Single-loop execution similar to the original behavior
- Used for short, single-line tasks that don't need orchestration

### Presence Mode

When the presence layer is enabled (default, disable with `--no-presence` or `[presence] enabled = false`):
- A small/fast model (configurable via `[presence]` in `intendant.toml`) acts as the conversational front-end
- The user talks to the presence layer, which delegates coding tasks to the agent loop via `submit_task`
- The presence layer narrates agent events (phase changes, approvals, completions) in first person
- Handles status queries, memory recall, and autonomy changes directly without involving the agent loop
- Uses its own system prompt (`SysPrompt_presence.md`) — standalone, not appended to the base prompt
- Follow-up input in the TUI is routed through the presence layer when active

Presence can run in two modes, but only one is active at a time (mutual exclusion):

#### Server-Side Text Presence

The default mode. A text model (e.g. `gemini-2.5-flash`) runs server-side inside `PresenceLayer`, processing events and generating narration text displayed in the TUI.

#### Browser-Side Live Presence

When `--web` is used and a browser connects a live model (Gemini Live / OpenAI Realtime), the browser sends a `live_connected` message over WebSocket. This sets a shared `AtomicBool` flag that pauses server-side presence — `PresenceLayer::handle_event()` returns `Ok(None)` while paused. The browser's live model takes over as the conversational front-end, using the same 9 tools via the WebSocket tool request/response protocol.

When the browser's live model disconnects (page close, error), a `live_disconnected` message is sent and server-side presence resumes automatically.

Both modes share the same tool implementations via standalone query functions (`query_detail()`, `recall_memory()`, `handle_tool_query()`) in `presence.rs`.

### Orchestrator Checkpointing

The orchestrator writes project state checkpoints after each sub-agent completes, using `storeMemory` with a `project_state` channel. Checkpoints capture completed/active tasks, architectural decisions, and constraints. This preserves essential context across auto-compaction boundaries — when context is compacted at ~60% usage, the orchestrator can recover state via `recallMemory`.

Checkpoints are also written to disk as both `project_state.json` (machine-readable) and `project_state.md` (human-readable) in the sub-agent directory.

## How It Works (Direct Mode)

1. Loads `.env` and selects the API provider (OpenAI, Anthropic, or Gemini). OpenAI uses the Responses API (`/v1/responses`), Anthropic uses the Messages API, Gemini uses the `generateContent` endpoint. All providers support streaming via SSE
2. Configures structured output (JSON mode), reasoning controls, native tool calling, prompt caching (Anthropic `cache_control`), and max output tokens based on model capabilities and env vars
3. Detects the project root (via `git rev-parse --show-toplevel`, falls back to cwd)
4. Resolves role-appropriate system prompt via cascade: project root → `~/.config/intendant/` → compiled-in default. When native tools are enabled, uses the condensed `SysPrompt_tools.md` (tool docs live in API tool definitions instead of prose)
5. Injects the project working directory into the conversation so the model knows which project to work in
6. Loads knowledge from `<project>/.intendant/memory.json`, injects into conversation
7. Logs the full messages array to `turn_NNN_messages.json` before each API call
8. Sends the task to the chat API via streaming (`chat_stream()`), with `max_tokens`/`max_output_tokens`, optional `reasoning`, optional JSON format, and native tool definitions when enabled. API requests use exponential backoff retry (up to 5 retries) for rate-limit (429) and server errors (5xx). Text deltas are forwarded to the TUI in real-time
9. Logs reasoning content (both summary and full text) to `turn_NNN_reasoning.txt` when available
10. Processes the model's response via one of two paths:
    - **Native tool call path** (when response contains tool calls): Collects individual tool calls, assembles them into an `AgentInput` batch, pipes to the runtime, maps results back to per-tool-call responses. Handles `manage_context` and `signal_done` tool calls caller-side. Raw API output items (reasoning + function_call) are preserved for verbatim echo-back in subsequent requests, which reasoning models (GPT-5, o3, o4) require
    - **Legacy text extraction path** (fallback): Extracts JSON from the response text (handles structured output, code fences, and bare JSON), checks for explicit `done` signal (`{"done": true}`)
11. Applies context directives (`drop_turns`, `summarize`) to the conversation
12. Injects project context (`memory_file`) into relevant commands
13. Classifies commands by action category (file read/write/delete, exec, network, destructive) and checks autonomy rules
14. If approval is required:
    - TUI mode: emits an approval request and waits for user response
    - Headless mode: denies execution (no implicit auto-approve fallback)
15. Pipes the JSON to the `intendant-runtime` binary and waits for completion with a hard timeout (120s default, 600s for `askHuman`)
16. Feeds the agent output back as the next user message (text path) or as individual tool results (tool call path), appending a token budget summary
17. Repeats until the model signals done, responds with no JSON, or the context budget is exhausted
18. In headless mode, if the model emits `askHuman`, the loop now sends a recovery prompt back to the model (continue with explicit assumptions) instead of blocking on human-input timeout

## askHuman Behavior

- In **TUI mode**, `askHuman` opens the input panel and writes your answer to the session-scoped response file.
- Empty submit is rejected in the TUI; you must provide non-empty input or press `Esc` to cancel.
- In **headless mode** (`--no-tui` or non-interactive stdin), `askHuman` cannot be answered interactively. The loop now tells the model to continue with explicit assumptions instead of waiting for the runtime timeout.
- Runtime-level timeout for unanswered `askHuman` remains `5 minutes`.

## Environment

- **OS:** Debian 12+
- **Runtime:** Tokio async
- **Display:** Xvfb is auto-launched lazily on the first turn containing an `execAsAgent` or `captureScreen` command when no accessible X display exists (checked via `xdpyinfo`). Display allocation prefers `:99` for a predictable VNC port (5999). An `x11vnc` server is co-launched alongside Xvfb for remote VNC observation (port = `5900 + display_id`); if `x11vnc` is not installed, the display works normally. Orphaned Xvfb processes from crashed sessions are detected via `/proc/<pid>/cmdline` and automatically reclaimed. The guard is kept for the session lifetime so subsequent calls reuse the same display. At startup the runtime discovers active X displays and merges their xauth cookies (from `~/.Xauthority` and `/var/run/lightdm/root/:N` via `sudo -n`) into a session-scoped `session.Xauthority` file, which is passed as `XAUTHORITY` to all spawned commands.
- **Permissions:** Runs as unprivileged user with passwordless sudo

## VNC Remote Observation

When running intendant in a VM (or any headless machine), you can watch the agent's GUI activity in real time via VNC.

**On the guest VM** — install `x11vnc`:

```bash
sudo apt-get install -y x11vnc
```

That's it. When intendant auto-launches a virtual display, it automatically starts an `x11vnc` server alongside it. The default display is `:99` with VNC on port **5999**.

**From your host machine** — connect with any VNC client:

```bash
# Direct connection (VM on local network)
vncviewer <vm-ip>:5999

# Over SSH tunnel (recommended for remote VMs)
ssh -L 5999:localhost:5999 user@vm-host
vncviewer localhost:5999
```

If display `:99` was already taken, intendant falls through to `:100+` and the VNC port shifts accordingly (`6000`, `6001`, ...). Check the TUI log panel or stderr for the actual port:

```
22:29:12  VNC server available at vnc://localhost:5999
```
