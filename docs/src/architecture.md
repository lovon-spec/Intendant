# Architecture

## Overview

Intendant is a two-binary system: a command **runtime** that executes work, and a **caller** that drives it via AI model APIs.

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
  +--> Presence layer:  conversational mediator between user and agent loop
  +--> Native tool calling (OpenAI/Anthropic/Gemini) with text extraction fallback
  +--> Streaming output:  SSE-based token streaming for all 3 providers
  +--> Ratatui TUI:     status bar, scrollable log, approval panel, askHuman input
  +--> MCP Server:      --mcp flag, stdio transport, full parity with TUI (tools + resources)
  +--> MCP Client:      connects to external MCP servers (configured in intendant.toml)
  +--> Autonomy system: Low/Medium/High/Full + per-category rules from intendant.toml
  +--> Landlock sandbox: filesystem restrictions on agent runtime (Linux)
  +--> Prompt caching:  Anthropic cache_control, OpenAI/Gemini implicit caching
  +--> Auto-compaction: triggers at 90% context usage, preserves system+tail messages
  +--> Control socket:  /tmp/intendant-<pid>.sock (JSON-line protocol)
  +--> Web gateway:     WebSocket server for remote TUI + browser-side voice (Gemini Live / OpenAI Realtime)
  +--> Token budget tracking (context-window-aware loop termination)
  +--> Session resume:  --continue (most recent) or --resume <id> (specific session)
  +--> Git worktree isolation for implementation agents
  +--> Tagged knowledge store with pub/sub channels between agents
```

## Process State

In-memory `HashMap<u64, ProcessInfo>` tracking nonce, PID, status, exit code, and timestamp. Ephemeral — does not survive binary restarts. Each runtime invocation starts with an empty process map.

## Session Directory

Per-session directory at `~/.intendant/logs/<uuid>/` with UUID-based naming. Contains per-nonce stdout/stderr log files, structured session logs (`session.jsonl`), conversation history (`conversation.jsonl`), and askHuman IPC files. The log directory is passed to the runtime via `INTENDANT_LOG_DIR`.

## Execution Model

Commands are processed sequentially. Each command blocks until completion and returns its result directly (exit code, stdout tail, stderr tail). The runtime exits after processing all commands. Daemons backgrounded in bash continue after the tool returns.

## Execution Modes

`intendant` operates in one of three modes, selected automatically based on task complexity and environment:

### Direct Mode

Activated for simple tasks, or forced with `--direct`:
- Single-loop execution with the selected model
- Budget-aware loop: stops at context exhaustion, `done` signal, or 500-turn safety cap
- Used for short tasks that don't need multi-agent orchestration

### User Mode

Activated for complex tasks without `INTENDANT_ROLE`:
- Pure subprocess monitor — makes zero model API calls at Layer 0
- Spawns an orchestrator sub-agent as a child process via `tokio::process::Command`
- Polls the orchestrator's progress file every 500ms, relays status to the TUI or stdout
- Reads the orchestrator's result file on exit; synthesizes a failure if the process crashes
- `kill_on_drop(true)` ensures the orchestrator is terminated if the user quits the TUI

### Sub-Agent Mode

Activated when `INTENDANT_ROLE` env var is set:
- Runs as a child agent with a scoped task
- Writes periodic progress to `INTENDANT_PROGRESS_FILE`
- Writes final results (summary, findings, artifacts, token usage) to `INTENDANT_RESULT_FILE`
- Uses role-specific system prompts (`SysPrompt_research.md`, `SysPrompt_implementation.md`, etc.)

See [Multi-Agent Orchestration](./multi-agent.md) for the full sub-agent architecture.

## How It Works (Direct Mode)

1. Loads `.env` and selects the API provider (OpenAI, Anthropic, or Gemini). OpenAI uses the Responses API (`/v1/responses`), Anthropic uses the Messages API, Gemini uses the `generateContent` endpoint. All providers support streaming via SSE
2. Configures structured output (JSON mode), reasoning controls, native tool calling, prompt caching (Anthropic `cache_control`), and max output tokens based on model capabilities and env vars
3. Detects the project root (via `git rev-parse --show-toplevel`, falls back to cwd)
4. Resolves role-appropriate system prompt via cascade: project root → `~/.config/intendant/` → compiled-in default. When native tools are enabled, uses the condensed `SysPrompt_tools.md` (tool docs live in API tool definitions instead of prose)
5. Injects the project working directory into the conversation so the model knows which project to work in
6. Loads knowledge from `<project>/.intendant/memory.json`, injects into conversation
7. Loads `INTENDANT.md` project instructions (global then project-local), injects into conversation
8. Logs the full messages array to `turn_NNN_messages.json` before each API call
9. Sends the task to the chat API via streaming (`chat_stream()`), with `max_tokens`/`max_output_tokens`, optional `reasoning`, optional JSON format, and native tool definitions when enabled. API requests use exponential backoff retry (up to 5 retries) for rate-limit (429) and server errors (5xx). Text deltas are forwarded to the TUI in real-time
10. Logs reasoning content (both summary and full text) to `turn_NNN_reasoning.txt` when available
11. Processes the model's response via one of two paths:
    - **Native tool call path** (when response contains tool calls): Collects individual tool calls, assembles them into an `AgentInput` batch, pipes to the runtime, maps results back to per-tool-call responses. Handles `manage_context` and `signal_done` tool calls caller-side. Raw API output items (reasoning + function_call) are preserved for verbatim echo-back in subsequent requests, which reasoning models (GPT-5, o3, o4) require
    - **Legacy text extraction path** (fallback): Extracts JSON from the response text (handles structured output, code fences, and bare JSON), checks for explicit `done` signal (`{"done": true}`)
12. Applies context directives (`drop_turns`, `summarize`) to the conversation
13. Injects project context (`memory_file`) into relevant commands
14. Classifies commands by action category (file read/write/delete, exec, network, destructive) and checks autonomy rules
15. If approval is required:
    - TUI mode: emits an approval request and waits for user response
    - Headless mode: denies execution (no implicit auto-approve fallback)
16. Pipes the JSON to the `intendant-runtime` binary and waits for completion with a hard timeout (120s default, 600s for `askHuman`)
17. Feeds the agent output back as the next user message (text path) or as individual tool results (tool call path), appending a token budget summary
18. Repeats until the model signals done, responds with no JSON, or the context budget is exhausted
19. In headless mode, if the model emits `askHuman`, the loop sends a recovery prompt back to the model (continue with explicit assumptions) instead of blocking on human-input timeout

## askHuman Behavior

- In **TUI mode**, `askHuman` opens the input panel and writes your answer to the session-scoped response file.
- Empty submit is rejected in the TUI; you must provide non-empty input or press `Esc` to cancel.
- In **headless mode** (`--no-tui` or non-interactive stdin), `askHuman` cannot be answered interactively. The loop tells the model to continue with explicit assumptions instead of waiting for the runtime timeout.
- Runtime-level timeout for unanswered `askHuman` remains 5 minutes.

## Streaming

All three providers support streaming via `chat_stream()` on the `ChatProvider` trait:

- **Anthropic**: `stream: true` on Messages API, parses `content_block_delta`, `content_block_start/stop`, `message_delta`
- **OpenAI**: `stream: true` on Responses API, parses `response.output_text.delta`, `response.function_call_arguments.delta`, `response.completed`
- **Gemini**: `streamGenerateContent?alt=sse` endpoint, parses chunked JSON candidates

Text deltas are forwarded to the TUI via `AppEvent::ModelResponseDelta` and accumulated in `App::streaming_buffer`, which is cleared when the full `ModelResponse` arrives.

## Rate-Limit Retry

API requests use `send_with_retry()` with exponential backoff (1s × 2^attempt + jitter, up to 5 retries) for HTTP 429 and 5xx responses. Non-retryable errors (400, 401, etc.) fail immediately. API keys in error messages are masked via `mask_api_keys()`.

## Prompt Caching

- **Anthropic**: Uses `anthropic-beta: prompt-caching-2024-07-31` header with structured system content containing `cache_control: {"type": "ephemeral"}`
- **OpenAI**: Automatic server-side caching for prompts >1024 tokens (no API changes needed)
- **Gemini**: Implicit context caching (no API changes needed)

## Auto-Compaction

When context usage reaches 90% (`usage_fraction() >= 0.90`), `conversation.auto_compact()` triggers:
- Keeps: system message, first 2 context messages, last 4 messages
- Summarizes: oldest half of remaining middle messages via `summarize_turns()`
- Emits `ContextManagement` event to TUI/MCP

## Vision / Display Management

Xvfb is auto-launched lazily on the first turn containing an `execAsAgent` or `captureScreen` command when no accessible X display exists (checked via `xdpyinfo`). The detection flow:

1. Already launched? → skip
2. Batch contains `execAsAgent` or `captureScreen`? No → skip
3. Current `DISPLAY` accessible? Yes → skip (user has working display)
4. Auto-launch Xvfb, store guard, set `DISPLAY`, emit `DisplayReady` event
5. On failure → log warning, let `captureScreen` fail naturally

Display allocation prefers `:99` for a predictable VNC port (5999). If `:99` is locked by a live Xvfb from a previous session, it is automatically killed and reclaimed (detected via `/proc/<pid>/cmdline`). If `:99` is held by a non-Xvfb process, allocation falls through to `:100+`.

Per-provider display resolutions:
- **OpenAI**: 1024×768 (3 tiles of 512×512)
- **Anthropic**: 819×1456 (9:16 aspect)
- **Gemini**: 768×1024 (2 tiles of 768×768)

### VNC Remote Observation

An `x11vnc` server is launched alongside Xvfb as a best-effort co-process (port = `5900 + display_id`). If `x11vnc` is not installed, the display works normally. Both Xvfb and x11vnc are killed on drop via `XvfbGuard`.

**On the guest VM** — install `x11vnc`:

```bash
sudo apt-get install -y x11vnc
```

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

## Environment

- **OS:** Linux (Debian 12+)
- **Runtime:** Tokio async (full features)
- **Permissions:** Runs as unprivileged user with passwordless sudo
- **Display:** Auto-managed Xvfb with x11vnc (see above)
- **X11 auth:** At startup the runtime discovers active X displays and merges their xauth cookies into a session-scoped `session.Xauthority` file, passed as `XAUTHORITY` to all spawned commands
