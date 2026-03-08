# CLAUDE.md

## Project Overview

This is **Intendant**, a Rust runtime for autonomous AI agents with process lifecycle management. It executes bash commands on behalf of AI agents, tracks process state in memory, and persists structured logs per session.

The project produces two binaries:
- **intendant-runtime** — Command runtime that reads JSON from stdin, executes commands sequentially (blocking until completion), and writes result lines to stdout
- **intendant** — AI integration layer (CLI/TUI/MCP) that drives the runtime via the OpenAI Responses API, Anthropic Messages API, or Gemini API in a loop

## Repository Structure

```
src/
├── main.rs              # intendant-runtime binary entry point (tokio async main)
├── agent.rs             # Core agent implementation
│                        #   - In-memory process state (HashMap<u64, ProcessInfo>)
│                        #   - Blocking command execution (execAsAgent) — returns exit code, stdout/stderr tail
│                        #   - Screenshot capture (captureScreen)
│                        #   - Path inspection (inspectPath)
│                        #   - File editing (editFile) / writing (writeFile)
│                        #   - Web browsing (browse)
│                        #   - Human interaction (askHuman)
│                        #   - PTY sessions (execPty)
│                        #   - Memory storage/recall with tagged knowledge (storeMemory, recallMemory)
│                        #   - Nonce variable replacement
├── models.rs            # Data structures: Command, AgentInput, ProcessInfo, ProcessStatus
├── error.rs             # AgentError enum (Io, Json, Process, InvalidNonce)
├── utils.rs             # get_timestamp()
└── bin/
    └── caller/
        ├── main.rs          # intendant entry point: 3 modes (user/sub-agent/direct), budget-aware loop, TUI init
        ├── provider.rs      # Multi-provider API client (OpenAI Responses API + Anthropic + Gemini), structured output, reasoning controls, streaming, rate-limit retry
        ├── conversation.rs  # Message management with layer protection, drop/summarize, budget tracking, auto-compaction
        ├── agent_runner.rs  # Spawns intendant-runtime subprocess, waits for completion with hard timeout (askHuman-aware), optional Landlock sandboxing
        ├── knowledge.rs     # Tagged knowledge store with pub/sub channels, cursor-based routing
        ├── sub_agent.rs     # Sub-agent spawning, result/progress I/O, role-specific configuration
        ├── worktree.rs      # Git worktree management for isolated implementation agents
        ├── user_mode.rs     # User-mode orchestrator spawning, progress monitoring, input relay
        ├── prompts.rs       # System prompt resolution: compile-time defaults (include_str!) + 3-layer cascade + INTENDANT.md loading
        ├── project.rs       # Project detection (git root), config parsing (intendant.toml + [approval] + [[mcp_servers]] + [sandbox])
        ├── autonomy.rs      # Autonomy levels, action categories, approval rules, command classification
        ├── control.rs       # Unix control socket server (JSON-line protocol at /tmp/intendant-<pid>.sock)
        ├── frontend.rs      # Shared frontend contract for TUI and MCP (UserAction enum, state queries, StatusSnapshot, ModelUsageSnapshot)
        ├── tools.rs         # Native tool definitions (11 tools), provider format conversion, extra tool registration for MCP client
        ├── tool_batch.rs    # Tool call batch assembly/disassembly: separates runtime vs caller-handled vs MCP tool calls, maps results back to per-tool responses
        ├── mcp.rs           # MCP server implementation (rmcp-based, stdio transport, hot-reload)
        ├── mcp_client.rs    # MCP client: connects to external MCP servers, discovers tools, proxies calls
        ├── sandbox.rs       # Landlock filesystem sandboxing (Linux): read/write path policies, process restriction
        ├── vision.rs        # Xvfb display management, x11vnc co-process, per-provider resolution, display :99 preference with orphan reclaim
        ├── web_gateway.rs   # WebSocket gateway: serves web TUI (xterm.js), streams TUI ANSI, bridges EventBus + key/resize input
        ├── session_log.rs   # UUID-based session directories, structured event logging, conversation persistence
        ├── error.rs         # CallerError enum (includes Tui variant)
        └── tui/
            ├── mod.rs       # Tui struct: terminal init/restore, render_frame(), render+event loop
            ├── app.rs       # App state machine, event dispatch, askHuman/approval modes
            ├── event.rs     # AppEvent enum, EventBus (mpsc wrapper), crossterm adapter, askHuman monitor
            ├── web.rs       # WebTui: buffer-backed ratatui backend, ANSI→WebSocket broadcast, web key parsing
            ├── widgets.rs   # StatusBar, LogPanel, ActionPanel, InputPanel, ApprovalPanel, FollowUpPanel, InspectOverlay rendering
            ├── layout.rs    # Panel sizing with constraints, responsive to terminal size
            └── theme.rs     # Color/style constants (Catppuccin Mocha-inspired)
SysPrompt.md                 # Default system prompt (direct mode, text-based JSON extraction)
SysPrompt_tools.md           # Condensed prompt for native tool calling mode
SysPrompt_user.md            # User-facing mode prompt
SysPrompt_orchestrator.md    # Orchestrator agent prompt
SysPrompt_research.md        # Research sub-agent prompt
SysPrompt_implementation.md  # Implementation sub-agent prompt
static/
└── live.html                # Web TUI (xterm.js terminal + optional voice overlay via Gemini Live / OpenAI Realtime)
```

## Build and Run

```bash
cargo build --release     # Produces target/release/intendant-runtime and target/release/intendant
cargo build               # Debug build
cargo check               # Type-check without building
```

Running the runtime:
```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' | ./target/release/intendant-runtime
```

Running the CLI (requires `.env` with API key):
```bash
./target/release/intendant "List the files in /tmp"
./target/release/intendant --no-tui "echo hello"          # Headless (no TUI)
./target/release/intendant --autonomy low "rm /tmp/test"   # Ask before every command
./target/release/intendant --provider anthropic --model claude-sonnet-4-5-20250929 "task"
./target/release/intendant --provider gemini --model gemini-2.5-pro "task"
./target/release/intendant --continue "fix that bug"       # Resume most recent session
./target/release/intendant --resume abc123 "continue"      # Resume specific session by ID
./target/release/intendant --mcp "task"                    # Run as MCP server on stdio
./target/release/intendant --json "echo hello"             # JSONL output to stdout (implies --no-tui)
./target/release/intendant --sandbox "run tests"           # Enable Landlock filesystem sandboxing
./target/release/intendant --web                           # Serve TUI via web (xterm.js + voice) on port 8765
./target/release/intendant --web 9000                      # Web TUI on custom port
./target/release/intendant --direct "complex task"         # Force single-agent mode (skip orchestrator)
./target/release/intendant --control-socket "task"         # Enable Unix control socket
echo "task" | ./target/release/intendant                   # Auto-detects non-TTY, runs headless
```

## Testing

```bash
cargo test                # Run all tests
cargo test -- --list      # List all test names
```

All tests are inline `#[cfg(test)]` modules in the same files as the code they test. Async tests use `#[tokio::test]`. The `tempfile` crate provides isolated temporary directories for tests that touch the filesystem.

Test coverage includes:
- **agent.rs**: Process info operations, blocking command execution, path inspection, nonce reference replacement, process mapping, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters
- **models.rs**: Serialization roundtrips, deserialization of minimal/full commands, repr(C) layout
- **error.rs**: Display formatting, From conversions
- **utils.rs**: Timestamp validity
- **caller/main.rs** (tests across caller modules): JSON extraction, context directives, done signal handling, budget constants, task classification, CLI flags (including --json, --sandbox), EventBus emit, batch assembly, tool name mapping, JSON output format, INTENDANT.md loading, captureScreen command detection
- **caller/conversation.rs**: Message ordering, serialization, drop/summarize turns, message layer protection, budget tracking, save/load JSONL roundtrip
- **caller/knowledge.rs**: Pub/sub lifecycle, subscription/cursor tracking, tag/channel/keyword filtering, old format migration, save/load roundtrip, knowledge routing
- **caller/sub_agent.rs**: Spawn command generation, result/progress I/O, serialization, role roundtrips, directory scanning
- **caller/worktree.rs**: Full lifecycle (create/list/merge/remove), conflict handling
- **caller/user_mode.rs**: Orchestrator spec generation, progress formatting, input relay, prompt resolution
- **caller/project.rs**: Config parsing, project paths, sub-agent directory, approval config parsing, MCP server config, sandbox config
- **caller/prompts.rs**: Compiled-in defaults non-empty, cascade resolution (project root, global config, compiled default), role-specific prompt appending (orchestrator, research, implementation, testing, direct), project override combinations
- **caller/provider.rs**: Provider selection, token usage parsing, context window defaults, Responses API types, structured output, reasoning controls, role mapping, rate-limit retry, API key masking, SSE parsing, streaming events, shared message builders
- **caller/error.rs**: Display formatting, type conversions (including Tui variant)
- **caller/autonomy.rs**: Autonomy levels (display, parse, cycle), action categories, approval rules, needs_approval logic, command classification (exec, destructive, network, file write, askHuman, browse), batch classification
- **caller/control.rs**: Socket path, outbound event serialization (including usage/usage_update), broadcast, server lifecycle
- **caller/tui/app.rs**: App defaults, logging (ring buffer), scrolling, key handling (quit, verbose, help, scroll, approval responses, follow-up input), event dispatch (all AppEvent variants including OrchestratorProgress, ModelResponseDelta, RoundComplete), bottom panel heights, model summary formatting (exec, edit, multiple commands, done signal, askHuman, invalid JSON), streaming buffer accumulation
- **caller/tui/event.rs**: EventBus send/receive/clone, ControlMsg deserialization (all variants), serialization roundtrip, ApprovalResponse variants
- **caller/tui/layout.rs**: Layout calculation (all panel combos, with/without bottom panel, hidden panels, small terminal)
- **caller/tui/widgets.rs**: Log entry formatting (all levels, verbose/non-verbose), string truncation
- **caller/tui/theme.rs**: Budget color thresholds, spinner frames, action style variants, autonomy color variants
- **caller/tui/mod.rs**: TestBackend rendering (default state, log entries, approval panel, help overlay, all phases, verbose modes, small terminal)
- **caller/tui/web.rs**: SharedWriter (write+take, clone shares buffer), web key parsing (enter, ctrl+c, arrows, chars, F-keys, space, escape, modifiers, unknown keys), broadcast_term base64 format
- **caller/agent_runner.rs**: askHuman detection in JSON input, sandboxed execution
- **caller/session_log.rs**: UUID-based session directories, session metadata (write_meta, find_latest_session, find_session_by_id), directory structure creation, JSONL event validity, turn tracking, model response file creation, agent input pretty-printing, agent output file creation (stdout/stderr split), approval log searchability, JSON extraction logging, summary file creation, multi-turn file separation, messages input logging, reasoning content logging (full and summary-only)
- **caller/tools.rs**: Tool definitions, provider format conversion, tool count validation
- **caller/tool_batch.rs** (tests in caller/main.rs): Batch assembly from tool calls (single exec, signal_done, manage_context, mixed tools, unknown tools, duplicate nonce detection, tool name mapping), result routing
- **caller/frontend.rs**: UserAction enum completeness, state query types, log level parsing, StatusSnapshot/ModelUsageSnapshot/UsageSnapshot serialization
- **caller/vision.rs**: Xvfb display configuration per provider, dynamic display allocation with :99 preference and orphan reclaim, x11vnc co-process launch, VNC port tracking, display accessibility probe
- **caller/mcp.rs**: MCP state management, process_action_sync, resource definitions, tool parameter schemas, event-to-state mappings
- **caller/mcp_client.rs**: Tool name parsing (`mcp__<server>_<tool>`), routing validation, connection lifecycle
- **caller/sandbox.rs**: Default config construction, disabled config skip, write path setup
- **caller/conversation.rs** (additional): Auto-compaction threshold, compaction preserves system+tail, too-few-messages guard

## Architecture Details

### Process State

Process state (nonce → PID/status/exit_code mappings) is stored in an in-memory `HashMap<u64, ProcessInfo>` protected by `Arc<RwLock<...>>`. This state is ephemeral — it does not survive binary restarts. Each runtime invocation starts with an empty process map.

### Session Management

Each invocation creates an isolated session with a UUID-based directory at `~/.intendant/logs/<uuid>/`. No global state is used for session tracking. The log directory is passed to the runtime via the `INTENDANT_LOG_DIR` environment variable.

Each session directory contains:
- `session_meta.json` — session metadata (session_id, created_at, project_root, task, status, last_turn)
- `session.jsonl` — structured event log
- `conversation.jsonl` — serialized conversation for resume support
- `human_question` / `human_response` — askHuman IPC files (session-scoped)
- `turns/` — per-turn model responses and agent I/O

Sessions can be resumed with `--continue` (most recent session for the project) or `--resume <id>` (specific session by ID or prefix).

### Execution Model

Commands are processed sequentially. Each command blocks until completion and returns its result directly (exit code, stdout tail, stderr tail for exec commands). The runtime exits after processing all commands.

### Nonce Variables

`$NONCE[id]` in command strings is replaced with the PID of the process launched by that nonce. Handled by regex-based substitution in `replace_nonce_refs()`.

### Intendant Flow

`intendant` operates in three modes based on environment:

**Sub-Agent Mode** (`INTENDANT_ROLE` set): Runs with scoped task, writes progress/results to files, uses role-specific system prompt.

**User Mode** (complex task, no `INTENDANT_ROLE`): Pure subprocess monitor — makes zero model API calls. Spawns orchestrator as a child process, polls its progress file every 500ms, reads its result file on exit. `kill_on_drop(true)` ensures cleanup on TUI quit.

**Direct Mode** (simple task or `--direct` flag, no `INTENDANT_ROLE`): Single-agent execution without orchestrator/sub-agent delegation. Still uses TUI when stdin is a TTY (use `--no-tui` for headless):
1. Selects API provider (OpenAI, Anthropic, or Gemini) from env, configures structured output and reasoning controls
2. Detects project root via git, loads `intendant.toml` config
3. Reads role-appropriate system prompt
4. Injects project knowledge into conversation
5. Budget-aware loop (stops at context exhaustion, `done` signal, or 500-turn safety cap): send to model -> extract JSON -> check done signal -> apply context directives -> inject project context -> pipe to agent -> append budget summary -> feed output back

### TUI Mode

When stdin is a TTY and `--no-tui` is not set, `intendant` launches a ratatui-based terminal UI:
- **Status bar**: Provider, model, turn count, budget percentage, autonomy level
- **Action panel**: Current phase (Thinking/RunningAgent/Orchestrating/WaitingApproval/WaitingHuman/WaitingFollowUp/Idle/Done) with spinner
- **Log panel**: Scrollable chronological log of all events with color-coded levels
- **Approval panel**: Shown when an action needs user approval (y/s/a/n keys)
- **Input panel**: Shown when askHuman is triggered (tui-textarea for response)
- **Follow-up panel**: Shown when agent completes a round and awaits follow-up input
- **Help overlay**: Key bindings reference (? key)

The agent loop runs in a background tokio task and communicates with the TUI via an `EventBus` (unbounded mpsc channel of `AppEvent`). When `bus` is `None` (headless mode), all output goes to stdout/stderr as before.

### Autonomy System

Three-layer autonomy control:

1. **Global level** (`--autonomy` flag, +/- keys in TUI): Low/Medium/High/Full
2. **Category rules** (`[approval]` section in intendant.toml): per-category Auto/Ask/Deny
3. **Per-action approval** (TUI only): approve/skip/approve-all/deny

Commands are classified into categories (FileRead, FileWrite, FileDelete, CommandExec, NetworkRequest, Destructive, HumanInput) by `autonomy::classify_command()`. Shell commands are further classified by inspecting the command string for destructive patterns (rm, kill, sudo), network tools (curl, wget, git), and file writes (redirects, tee, mv, cp). The `sudo` prefix is detected as Destructive and the actual command after `sudo` is also classified.

### Control Socket

A Unix socket server at `/tmp/intendant-<pid>.sock` enables programmatic control. JSON-line protocol supports: status, usage, approve, deny, input, set_autonomy, quit. Outbound events are broadcast to all connected clients. The `status` event includes `session_id` and `task`. The `usage` command returns per-model token usage (`ModelUsageSnapshot` for main and optional presence). A `usage_update` event is broadcast after each agent turn with current token consumption.

### MCP Hot Reload

The `reload` MCP tool rebuilds the binary and replaces the running process via `exec()`. A `ReloadTransport` wrapper injects a synthetic MCP initialization handshake so rmcp's `serve()` works transparently after exec. The `INTENDANT_MCP_RELOAD` env var signals the new process to use `ReloadTransport` instead of plain stdio.

### OpenAI API Features

- **Structured output**: JSON object mode (`text.format`) is enabled by default for capable models (gpt-5+, o3, o4). Controlled via `STRUCTURED_OUTPUT` env var. Eliminates brittle free-text JSON extraction.
- **Reasoning controls**: For reasoning models (gpt-5+, o3, o4), `REASONING_EFFORT` ("low"/"medium"/"high") and `REASONING_SUMMARY` ("auto"/"concise"/"detailed") tune quality/cost tradeoffs.
- **Max output tokens**: Sent as `max_output_tokens` on all OpenAI Responses API requests.
- **Role mapping**: Responses API passes through all non-system roles (user, assistant, developer, tool) instead of filtering to user/assistant only.
- **Done signal**: With structured output enabled, models signal task completion via `{"commands": [], "done": true}` instead of prose responses.

### Streaming Output

All three providers (OpenAI, Anthropic, Gemini) support streaming via `chat_stream()` on the `ChatProvider` trait. The default implementation falls back to non-streaming `chat()` for compatibility. Streaming uses SSE (Server-Sent Events) parsing for all providers:
- **Anthropic**: `stream: true` on Messages API, parses `content_block_delta`, `content_block_start/stop`, `message_delta`
- **OpenAI**: `stream: true` on Responses API, parses `response.output_text.delta`, `response.function_call_arguments.delta`, `response.completed`
- **Gemini**: `streamGenerateContent?alt=sse` endpoint, parses chunked JSON candidates

Text deltas are forwarded to the TUI via `AppEvent::ModelResponseDelta` and accumulated in `App::streaming_buffer`, which is cleared when the full `ModelResponse` arrives.

### Rate-Limit Retry

API requests use `send_with_retry()` with exponential backoff (1s * 2^attempt + jitter, up to 5 retries) for HTTP 429 and 5xx responses. Non-retryable errors (400, 401, etc.) fail immediately. API keys in error messages are masked via `mask_api_keys()`.

### Prompt Caching

- **Anthropic**: Uses `anthropic-beta: prompt-caching-2024-07-31` header with structured system content containing `cache_control: {"type": "ephemeral"}`
- **OpenAI**: Automatic server-side caching for prompts >1024 tokens (no API changes needed)
- **Gemini**: Implicit context caching (no API changes needed)

### INTENDANT.md Project Instructions

Project-level instructions are loaded from a 2-layer cascade:
1. `~/.config/intendant/INTENDANT.md` (global)
2. `<project_root>/INTENDANT.md` (project-local)

Both are loaded and injected as user messages at conversation start (before memory/knowledge injection). Loaded via `prompts::load_project_instructions()`.

### Auto-Compaction

When context usage reaches 90% (`usage_fraction() >= 0.90`), `conversation.auto_compact()` triggers:
- Keeps: system message, first 2 context messages, last 4 messages
- Summarizes: oldest half of remaining middle messages via `summarize_turns()`
- Emits `ContextManagement` event to TUI/MCP

### JSON Output Mode

`--json` flag enables JSONL structured output to stdout (implies `--no-tui`). Each line is a JSON object with `type` and `data` fields. Event types include: `turn_started`, `model_response`, `model_response_delta`, `agent_output`, `done`, `error`, `approval_required`, `budget_warning`.

### MCP Client Support

External MCP servers can be configured in `intendant.toml`:
```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp_servers.env]
SOME_VAR = "value"
```

At startup, `McpClientManager` connects to all configured servers, discovers their tools, and registers them with the `mcp__<server>_<tool>` naming convention. Tool calls with this prefix are routed through the MCP client manager. If a server fails to connect, it is skipped with a warning.

### Landlock Sandboxing

On Linux (kernel 5.13+), `--sandbox` or `[sandbox] enabled = true` in `intendant.toml` enables Landlock filesystem restrictions on the agent runtime process:
- **Read**: `/` (everything)
- **Write**: project root, `/tmp`, log directory, `~/.intendant`
- Extra write paths can be configured via `[sandbox] extra_write_paths`

The sandbox config is passed to the runtime via `INTENDANT_SANDBOX_WRITE_PATHS` environment variable. On kernels without Landlock support, sandboxing is silently skipped.

### Vision / Xvfb

Xvfb is auto-launched lazily on the first turn that contains an `execAsAgent` or `captureScreen` command and no accessible X display exists. The detection flow per turn:
1. Already launched? → skip
2. Batch contains `execAsAgent` or `captureScreen`? No → skip
3. Current `DISPLAY` accessible (via `xdpyinfo`)? Yes → skip (user has working display)
4. Auto-launch Xvfb, store guard, set `DISPLAY`, emit `DisplayReady` event
5. On failure → log warning, let `captureScreen` fail naturally

Display allocation prefers `:99` for a predictable VNC port (5999). If `:99` is locked by a live Xvfb process from a previous session, it is automatically killed and reclaimed (detected via `/proc/<pid>/cmdline`). If `:99` is held by a non-Xvfb process, allocation falls through to `:100+`.

An `x11vnc` server is launched alongside Xvfb as a best-effort co-process (port = `5900 + display_id`). If `x11vnc` is not installed, the display still works normally. The VNC URL is logged to the TUI/stderr on success. Both Xvfb and x11vnc are killed on drop via `XvfbGuard`.

## Code Conventions

- **Rust 2021 edition** with default rustfmt and clippy settings (no .rustfmt.toml or .clippy.toml)
- **Naming**: snake_case for functions/modules, PascalCase for types, SCREAMING_SNAKE_CASE for constants
- **Error handling**: Custom `thiserror`-based enums (`AgentError`, `CallerError`) with `Result<T>` returns
- **Async**: tokio with full features; background tasks via `tokio::spawn`
- **Shared state**: `Arc<RwLock<T>>` for mutable shared state, `mpsc` channels for communication
- **No unsafe code**: The codebase contains no `unsafe` blocks
- **Tests**: Always inline `#[cfg(test)]` modules — no separate test files

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` (full) | Async runtime |
| `serde` + `serde_json` | JSON serialization/deserialization |
| `thiserror` | Error type derivation |
| `chrono` | Timestamp formatting for log directories |
| `env_logger` | Logging |
| `regex` | $NONCE[id] pattern matching, ANSI escape stripping |
| `reqwest` (rustls-tls, stream) | HTTP client for API calls, SSE streaming |
| `html2text` | HTML to plain text conversion for browse |
| `portable-pty` | PTY session management for execPty |
| `dotenvy` | .env file loading |
| `toml` | intendant.toml config parsing |
| `async-trait` | Async trait support for ChatProvider |
| `uuid` (v4) | Session ID generation |
| `dirs` | Platform config directory resolution |
| `rmcp` (server, client, transport-io, transport-child-process) | MCP server and client framework |
| `futures-util` | Stream utilities for SSE response parsing |
| `landlock` (Linux only) | Filesystem sandboxing via Landlock LSM |
| `schemars` | JSON schema derivation for MCP tool parameters |
| `ratatui` | Terminal UI framework |
| `crossterm` | Terminal input/output backend (event-stream feature) |
| `tui-textarea` | Text input widget for askHuman responses |
| `tokio-stream` | Stream utilities for crossterm EventStream |
| `base64` | Encoding screenshot data to base64 for vision API calls |
| `tokio-tungstenite` | WebSocket server/client for live gateway |
| `tempfile` (dev) | Temporary directories in tests |

## Environment Requirements

- **OS**: Linux
- **Permissions**: Runs as unprivileged user with passwordless sudo
- **For intendant**: `.env` file with `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`. Optional: `PROVIDER`, `MODEL_NAME`, `USE_NATIVE_TOOLS`, `STRUCTURED_OUTPUT`, `REASONING_EFFORT`, `REASONING_SUMMARY`, `INTENDANT_LOG_DIR` (set automatically by caller for the runtime)
- **For captureScreen**: ImageMagick `import` command and DISPLAY environment variable (defaults to `:1`)

## CI/CD

No CI/CD is currently configured. Run `cargo test` and `cargo clippy` locally before committing.
