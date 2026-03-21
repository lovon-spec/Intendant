# CLAUDE.md

## Project Overview

This is **Intendant**, a Rust runtime for autonomous AI agents with process lifecycle management. It executes bash commands on behalf of AI agents, tracks process state in memory, and persists structured logs per session.

The project produces two binaries:
- **intendant-runtime** — Command runtime that reads JSON from stdin, executes commands sequentially (blocking until completion), and writes result lines to stdout
- **intendant** — AI integration layer (CLI/TUI/Web/MCP) that drives the runtime via the OpenAI Responses API, Anthropic Messages API, or Gemini API in a loop

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
        ├── event.rs         # Shared event infrastructure: EventBus, AppEvent, ControlMsg, ApprovalRegistry, ContextInjectionQueue (extracted from tui/)
        ├── types.rs         # Shared types: Phase, LogLevel, Verbosity, OutboundEvent, format_model_summary() (extracted from tui/)
        ├── provider.rs      # Multi-provider API client (OpenAI Responses API + Anthropic + Gemini), structured output, reasoning controls, streaming, rate-limit retry
        ├── conversation.rs  # Message management with layer protection, drop/summarize, budget tracking, auto-compaction
        ├── agent_runner.rs  # Spawns intendant-runtime subprocess, waits for completion with hard timeout (askHuman-aware), optional Landlock sandboxing
        ├── knowledge.rs     # Tagged knowledge store with pub/sub channels, cursor-based routing
        ├── sub_agent.rs     # Sub-agent spawning, result/progress I/O, role-specific configuration
        ├── worktree.rs      # Git worktree management for isolated implementation agents
        ├── user_mode.rs     # User-mode orchestrator spawning, progress monitoring, input relay
        ├── prompts.rs       # System prompt resolution: compile-time defaults (include_str!) + 3-layer cascade + INTENDANT.md loading
        ├── project.rs       # Project detection (git root), config parsing (intendant.toml + [approval] + [[mcp_servers]] + [sandbox] + [transcription])
        ├── autonomy.rs      # Autonomy levels, action categories, approval rules, command classification
        ├── control.rs       # Unix control socket server (JSON-line protocol at /tmp/intendant-<pid>.sock)
        ├── frontend.rs      # Shared frontend contract for TUI and MCP (UserAction enum, state queries, StatusSnapshot, ModelUsageSnapshot)
        ├── tools.rs         # Native tool definitions (12 tools incl. invoke_skill), provider format conversion, extra tool registration for MCP client
        ├── tool_batch.rs    # Tool call batch assembly/disassembly: separates runtime vs caller-handled vs MCP tool calls, maps results back to per-tool responses
        ├── presence.rs      # Presence layer: server-side PresenceLayer, tool dispatch, standalone query functions, event filtering, agent state tracking, presence session protocol, conversation context builder
        ├── mcp.rs           # MCP server implementation (rmcp-based, stdio transport, hot-reload)
        ├── mcp_client.rs    # MCP client: connects to external MCP servers, discovers tools, proxies calls
        ├── sandbox.rs       # Landlock filesystem sandboxing (Linux): read/write path policies, process restriction
        ├── vision.rs        # Xvfb display management, x11vnc co-process, per-provider resolution, display :99 preference with orphan reclaim
        ├── skills.rs        # Skill discovery (SKILL.md with YAML frontmatter), parsing, catalog formatting, project + personal dirs
        ├── transcription.rs # Audio transcription via Whisper API (configurable endpoint/model), WAV encoding, silence detection
        ├── web_gateway.rs   # Web gateway: serves app dashboard + legacy live page, WebSocket bridge, presence session protocol, VNC proxy, session log replay, ephemeral token minting
        ├── session_log.rs   # UUID-based session directories, structured event logging, conversation persistence
        ├── error.rs         # CallerError enum (includes Tui variant)
        └── tui/
            ├── mod.rs       # Tui struct: terminal init/restore, render_frame(), render+event loop
            ├── app.rs       # App state machine, event dispatch, askHuman/approval modes, presence pause/resume
            ├── event.rs     # crossterm adapter, askHuman file monitor (AppEvent/EventBus moved to caller/event.rs)
            ├── web.rs       # WebTui: per-connection buffer-backed ratatui backend, ANSI→WebSocket, web key parsing, WebTuiCommand
            ├── widgets.rs   # StatusBar, LogPanel, ActionPanel, InputPanel, ApprovalPanel, FollowUpPanel, InspectOverlay rendering
            ├── layout.rs    # Panel sizing with constraints, responsive to terminal size
            ├── markdown.rs  # Lightweight markdown-to-ratatui renderer (headers, bold, italic, code, lists, rules)
            └── theme.rs     # Color/style constants (Catppuccin Mocha-inspired)
crates/
├── presence-core/           # WASM-compatible workspace crate for presence logic
│   ├── Cargo.toml           # Minimal deps: serde + serde_json + wasm-bindgen (no tokio/reqwest)
│   ├── src/
│   │   ├── lib.rs           # Re-exports all modules
│   │   ├── types.rs         # PresenceConfig, TaskEnvelope, PresenceEvent, AgentStateSnapshot, PresenceSession, PresenceCheckpoint, PresenceConnect, PresenceWelcome, constants
│   │   ├── dispatch.rs      # PresenceAction enum, dispatch_tool_call() — pure logic dispatch
│   │   ├── format.rs        # format_event(), truncate() (unicode-safe)
│   │   ├── tools.rs         # 9 presence tool definitions (provider-agnostic)
│   │   ├── prompt.rs        # DEFAULT_PRESENCE_PROMPT via include_str!
│   │   └── wasm.rs          # WASM exports: WasmPresence object, get_presence_tools(), get_presence_prompt(), format helpers
│   └── prompts/
│       └── SysPrompt_presence.md  # Presence system prompt
└── presence-web/            # WASM crate for browser-side presence and app dashboard
    ├── Cargo.toml           # Deps: presence-core, wasm-bindgen, serde-wasm-bindgen, js-sys, web-sys
    ├── src/
    │   ├── lib.rs           # Entry point: PresenceClient, LiveUsage, to_js() helper
    │   ├── callbacks.rs     # JS callback management for voice/tool events
    │   ├── server.rs        # WebSocket connection to Intendant server, message routing
    │   ├── gemini.rs        # Gemini Live API integration (BidiGenerateContent), dual-mode auth (API key + ephemeral token)
    │   ├── openai.rs        # OpenAI Realtime API integration
    │   ├── app_state.rs     # App dashboard state: UiCommand enum, per-model pricing tables, cost calculation, event routing, log filtering
    │   └── app_web.rs       # Browser-side app dashboard: WASM↔DOM bridge, tab management, WebSocket event dispatch
static/
├── app.html                 # Web app dashboard: 4-tab UI (Activity, Usage, Terminal, Displays) with WASM-driven state, Catppuccin theme
├── live.html                # Legacy web TUI: xterm.js terminal + live model presence (Gemini Live / OpenAI Realtime)
├── audio-processor.js       # AudioWorklet processor for microphone capture (PCM16 output)
└── wasm-web/
    ├── presence_web.js      # Generated wasm-bindgen JS glue
    └── presence_web_bg.wasm # Compiled WASM binary (presence-web crate)
SysPrompt.md                 # Default system prompt (direct mode, text-based JSON extraction)
SysPrompt_tools.md           # Condensed prompt for native tool calling mode
SysPrompt_user.md            # User-facing mode prompt
SysPrompt_orchestrator.md    # Orchestrator agent prompt
SysPrompt_research.md        # Research sub-agent prompt
SysPrompt_implementation.md  # Implementation sub-agent prompt
SysPrompt_presence.md        # Presence layer system prompt (top-level copy)
docs/
├── book.toml                # mdBook configuration
└── src/
    ├── SUMMARY.md           # Book table of contents
    ├── getting-started.md   # Build, setup, running, testing
    ├── architecture.md      # Two-binary system, execution modes, streaming, caching
    ├── configuration.md     # CLI flags, env vars, intendant.toml, system prompts
    ├── runtime-protocol.md  # Runtime functions, nonce variables, context management
    ├── tui.md               # TUI layout, key bindings, autonomy system, web TUI
    ├── multi-agent.md       # Orchestration, sub-agents, worktrees, knowledge routing
    ├── presence.md          # Presence layer, tools, mutual exclusion, session protocol
    ├── mcp-server.md        # MCP server tools, hot reload, resources, controller restart
    ├── integrations.md      # Control socket, web gateway WebSocket protocol
    └── session-logging.md   # Session directories, event types, resume, test coverage
tests/
└── e2e/
    ├── main.rs              # Integration test entry point
    ├── harness.rs           # IntendantProcess, ControlSocketClient, WsClient, voice helpers
    ├── test_basic.rs        # Tier 1: exec, approval, follow-up (--json mode, no display)
    ├── test_control_socket.rs # Tier 2: status, usage, autonomy (control socket, needs display)
    ├── test_web.rs          # Tier 3: WebSocket state_snapshot, tool_request, ANSI frames
    └── test_voice.rs        # Tier 3: voice connection, submit+approve (needs browser + audio)
skills/
├── tui-e2e/SKILL.md         # Interactive TUI testing guide (screenshot-based)
├── web-e2e/SKILL.md         # Interactive web/voice testing guide
└── voice-e2e/SKILL.md       # Full audio pipeline testing guide
scripts/
└── ff-eval.py               # Firefox JS eval via remote debugger (raw socket)
```

## Build and Run

```bash
cargo build --release     # Produces target/release/intendant-runtime and target/release/intendant
cargo build               # Debug build
cargo check               # Type-check without building
```

### Building the WASM crate (presence-web)

```bash
# From crates/presence-web/
wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web

# Then rebuild intendant to re-embed the WASM
cargo build --release -p intendant
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
./target/release/intendant --web                           # Web app dashboard on port 8765
./target/release/intendant --web 9000                      # Web app on custom port
./target/release/intendant --direct "complex task"         # Force single-agent mode (skip orchestrator)
./target/release/intendant --control-socket "task"         # Enable Unix control socket
./target/release/intendant --no-presence "task"            # Disable presence layer
echo "task" | ./target/release/intendant                   # Auto-detects non-TTY, runs headless
```

## Testing

```bash
cargo test --bins         # Run unit tests only (fast, no API keys needed)
cargo test -- --list      # List all test names
```

### Unit Tests

All unit tests are inline `#[cfg(test)]` modules in the same files as the code they test. Async tests use `#[tokio::test]`. The `tempfile` crate provides isolated temporary directories for tests that touch the filesystem. These tests are deterministic, fast, and require no external services.

### Integration Tests (`tests/e2e/`)

Integration tests in `tests/e2e/` spawn a real intendant binary and exercise the full stack. **Every test makes real API calls** to an LLM provider, so they cost tokens and are non-deterministic. They are NOT suitable for CI/CD — run them manually or on a schedule.

```bash
cargo build --release                                    # Build binary first
cargo test --test e2e test_basic -- --nocapture           # Tier 1: no display needed
cargo test --test e2e test_control_socket -- --nocapture  # Tier 2: needs Xvfb
cargo test --test e2e test_web -- --nocapture             # Tier 3: needs Xvfb
cargo test --test e2e test_voice -- --nocapture           # Tier 3: needs Xvfb + Firefox + PulseAudio
```

**Tier 1 (JSON mode)**: Spawns `intendant --json --direct`, reads JSONL events from stdout, sends JSON commands (`{"action":"approve","id":N}`) and follow-up text on stdin. No display required.

**Tier 2 (Control socket)**: Spawns `intendant --control-socket --direct` in TUI mode, connects to `/tmp/intendant-<pid>.sock`. Requires `DISPLAY` for TUI rendering.

**Tier 3 (Web/Voice)**: Spawns `intendant --json --direct --web <port>`, connects via WebSocket and HTTP `/debug`. Voice tests additionally require Firefox, PulseAudio, and espeak-ng.

VNC monitoring: Tier 2/3 tests use Xvfb on `:50` with x11vnc on port 5950. Connect with any VNC viewer to watch tests run graphically.

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
4. Discovers skills from project and personal directories, injects catalog into conversation
5. Injects project knowledge into conversation
6. Budget-aware loop (stops at context exhaustion, `done` signal, or 500-turn safety cap): send to model -> extract JSON -> check done signal -> apply context directives -> inject project context -> pipe to agent -> append budget summary -> feed output back

### TUI Mode

When stdin is a TTY and `--no-tui` is not set, `intendant` launches a ratatui-based terminal UI:
- **Status bar**: Provider, model, turn count, budget percentage, autonomy level
- **Action panel**: Current phase (Thinking/RunningAgent/Orchestrating/WaitingApproval/WaitingHuman/WaitingFollowUp/Idle/Done) with spinner
- **Log panel**: Scrollable chronological log of all events with color-coded levels, markdown rendering for model responses
- **Approval panel**: Shown when an action needs user approval (y/s/a/n keys)
- **Input panel**: Shown when askHuman is triggered (tui-textarea for response)
- **Follow-up panel**: Shown when agent completes a round and awaits follow-up input
- **Help overlay**: Key bindings reference (? key)

The agent loop runs in a background tokio task and communicates with the TUI via an `EventBus` (unbounded mpsc channel of `AppEvent`). When `bus` is `None` (headless mode), all output goes to stdout/stderr as before.

### Module Extraction (event.rs and types.rs)

`AppEvent`, `EventBus`, `ControlMsg`, and `ApprovalRegistry` were extracted from `tui/event.rs` into `caller/event.rs` so that non-TUI modules (MCP, control socket, web gateway, presence) have zero `use crate::tui::` imports for shared types. Similarly, `Phase`, `LogLevel`, `Verbosity`, and `OutboundEvent` were extracted into `caller/types.rs`.

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

`--json` flag enables JSONL structured output to stdout (implies `--no-tui`). Each line is a JSON object with `type` and `data` fields. Event types include: `turn_started`, `model_response`, `model_response_delta`, `agent_output`, `done`, `error`, `approval_required`, `human_question`, `budget_warning`, `round_complete`, `context_management`.

In JSON mode, stdin accepts both plain text (follow-up messages) and JSON commands using the same `ControlMsg` format as the Unix control socket:
- `{"action":"approve","id":N}` — approve pending command
- `{"action":"deny","id":N}` — deny pending command
- `{"action":"skip","id":N}` — skip pending command
- `{"action":"approve_all","id":N}` — approve and set autonomy to Full
- `{"action":"input","text":"..."}` — respond to askHuman
- `{"action":"follow_up","text":"..."}` — send follow-up task

Lines not starting with `{` or not parseable as `ControlMsg` are treated as follow-up text. This makes `--json` mode fully interactive: approval flows, askHuman, and multi-round conversations all work without a TUI or control socket.

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

### Skills System

Skills are named instruction sets stored as `SKILL.md` files with YAML frontmatter. They are discovered from two locations (project-scoped first):

1. `<project_root>/.intendant/skills/<name>/SKILL.md`
2. `~/.intendant/skills/<name>/SKILL.md`

Frontmatter fields: `name`, `description`, `autonomy` (optional override), `disable-auto-invocation` (boolean), `sandbox` (optional override). Project skills take precedence over personal skills with the same name. The model can invoke skills via the `invoke_skill` tool, or the user can trigger them via the control socket / TUI / presence layer. Available skills are formatted into a catalog and injected into the conversation.

### Transcription

Server-side audio transcription via the Whisper API (or compatible endpoints). Configured in `intendant.toml`:

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
language = "en"           # ISO-639-1 hint (optional, auto-detect if omitted)
# endpoint = "http://..."  # Custom endpoint for self-hosted whisper.cpp
```

When enabled, the web gateway accepts `user_audio` WebSocket messages containing base64-encoded PCM16 audio. Audio is buffered in ~3s chunks, filtered by RMS energy (to avoid Whisper hallucinating on silence), wrapped in WAV, and sent to the transcription API. Results are emitted as `AppEvent::UserTranscript` and broadcast as `OutboundEvent::UserTranscript`.

### Presence Layer

The presence layer is the conversational interface between the user and the agent system. It mediates all interaction: the user talks to presence, presence delegates work via `submit_task`, and narrates progress as events stream back from the agent loop.

**Architecture**: Only one presence model is active at a time — either server-side text presence OR browser-side live presence (Gemini Live / OpenAI Realtime). Never both simultaneously.

**Server-side presence** (`presence.rs`): `PresenceLayer` wraps a small/fast text model (e.g., gemini-2.0-flash). It maintains its own `Conversation`, processes user input via `process_user_input()`, narrates agent events via `handle_event()`, and dispatches tasks via `TaskEnvelope` on a channel. The presence layer has 9 tools defined in `presence-core`:
- **Action tools**: `submit_task`, `approve_action`, `deny_action`, `skip_action`, `respond_to_question`, `set_autonomy` — dispatch via EventBus as ControlMsg
- **Query tools**: `check_status` (reads AgentStateSnapshot), `query_detail` (git diff, logs, files), `recall_memory` (knowledge store + session log fallback)

Tool dispatch uses `presence_core::dispatch_tool_call()` which returns a `PresenceAction` enum. Pure-logic tools return `TextResult`/`SubmitTask`/`Approve`/etc. I/O tools return `NeedsIO` for the platform layer to handle. The standalone functions `query_detail()`, `recall_memory()`, and `handle_tool_query()` are shared between `PresenceLayer` and the web gateway.

**Browser-side live presence** (`crates/presence-web/`): When the user connects a live model (Gemini Live / OpenAI Realtime) from the browser, it sends `{"t":"presence_connect"}` over WebSocket. The server pauses `PresenceLayer` and sends a `presence_welcome` message with session state, event replay window, and conversation context. Tool calls from the live model go through the `tool_request`/`tool_response` WebSocket protocol. When the live model disconnects, `{"t":"presence_disconnect"}` resumes server-side presence.

**Active/passive browsers**: Only one browser connection can be "active" (controlling the voice model) at a time. Other connections are passive observers that receive TUI frames and events but don't pause server-side presence. A browser can request active status via `{"t":"make_active"}`, which force-disconnects the previous active browser and sends an `active_granted` message with handover context.

**Presence session protocol**: The server maintains a `PresenceSession` that tracks event windows and checkpoints. Browsers can send `presence_checkpoint` messages with summary text and `last_event_seq`, enabling session continuity across reconnects. The `presence_welcome` message includes missed events since `last_event_seq`, conversation context from recent transcripts, and the last checkpoint summary.

**presence-core** (`crates/presence-core/`): WASM-compatible workspace crate containing types, tool definitions, dispatch logic, event formatting, session protocol types, and the presence system prompt. No tokio/reqwest dependencies. Compiles to both native and `wasm32-unknown-unknown`. The main crate re-exports its types and converts `ToolDefinition` to the provider-specific format.

**presence-web** (`crates/presence-web/`): Browser-side WASM crate that wraps `presence-core` with WebSocket transport. Contains:
- `app_state.rs` — Pure-Rust app state for the web dashboard. All event routing, log filtering, usage tracking, cost calculation, and status bar state. Methods return `Vec<UiCommand>` which the thin JS layer executes as DOM updates. Includes a per-model pricing table covering OpenAI, Anthropic, and Gemini models.
- `app_web.rs` — Browser-side entry point for the app dashboard. WASM↔DOM bridge, tab management, WebSocket event dispatch.
- `server.rs` — WebSocket connection to the Intendant server, message routing.
- `gemini.rs` — Gemini Live API integration (BidiGenerateContent), dual-mode auth (API key + ephemeral token).
- `openai.rs` — OpenAI Realtime API integration.
- `callbacks.rs` — JS callback management for voice/tool events.

### Web Gateway

`--web` (default port 8765) serves the web app dashboard and bridges WebSocket connections to the EventBus. The gateway serves two web UIs and handles both the presence session protocol and per-connection terminal rendering.

**HTTP Endpoints**:
- `GET /` — serves `app.html` (the 4-tab web dashboard: Activity, Usage, Terminal, Displays)
- `GET /live` — serves `live.html` (legacy xterm.js terminal + voice overlay)
- `GET /config` — returns `WebGatewayConfig` JSON (provider, model, sample rates, transcription flag)
- `GET /debug` — returns debug JSON (agent state, voice connection, active connection ID)
- `POST /session` — mints ephemeral session tokens for Gemini Live or OpenAI Realtime
- `GET /wasm-web/*` — serves compiled WASM and JS glue with content-hash cache-busting
- `GET /audio-processor.js` — serves AudioWorklet processor

**WebSocket endpoints**:
- `/` or `/ws` — main WebSocket for events, terminal I/O, and presence protocol
- `/vnc` — WebSocket-to-TCP VNC proxy (bridges noVNC to local x11vnc)

**Inbound messages** (browser → server):
- `{"t":"key",...}` → per-connection terminal key input via WebTuiCommand
- `{"t":"resize","cols":N,"rows":N}` → per-connection terminal resize
- `{"t":"presence_connect",...}` → presence session protocol (replaces legacy `live_connected`)
- `{"t":"presence_disconnect"}` → disconnect presence (replaces legacy `live_disconnected`)
- `{"t":"make_active"}` → request active voice ownership (handover from another browser)
- `{"t":"voice_log",...}` → voice transcript from browser presence model
- `{"t":"presence_checkpoint",...}` → context checkpoint from browser presence model
- `{"t":"voice_diagnostic",...}` → diagnostic from browser voice/presence layer
- `{"t":"user_audio",...}` → base64 PCM16 audio for server-side transcription
- `{"t":"tool_request","id":"...","tool":"...","args":{}}` → presence tool dispatch + per-connection response
- `{"t":"async_query","id":"...","tool":"...","args":{}}` → async query (result injected as text, not tool response)
- `{"action":"..."}` → `ControlMsg` (same as Unix control socket)
- Legacy: `{"t":"live_connected"}` / `{"t":"live_disconnected"}` still accepted

**Outbound messages** (server → browser):
- `{"t":"term","d":"..."}` — per-connection base64-encoded TUI ANSI frames
- `{"t":"state_snapshot","state":{...},"connection_id":"...","config":{...},"session_id":"..."}` — bootstrap on connect
- `{"t":"log_replay","entries":[...]}` — historical session log entries for late-connecting browsers
- `{"t":"presence_welcome","session_id":"...","state":{...},"events":[...],"is_active":bool,"conversation_context":"..."}` — presence session welcome
- `{"t":"active_granted","is_active":true,"handover_context":"...","conversation_context":"..."}` — active ownership granted
- `{"t":"force_disconnect_voice","reason":"handover"}` — sent to old active browser on handover
- `{"t":"presence_checkpoint_ack","seq":N}` — checkpoint acknowledgement
- `{"t":"tool_response","id":"...","result":"..."}` — response to a tool_request (per-connection)
- `{"t":"async_query_result","id":"...","tool":"...","result":"..."}` — response to an async_query
- `{"event":"..."}` — `OutboundEvent` broadcast: status, agent_output, approval_required, task_complete, usage_update, log_entry, etc.

The gateway caches the latest `usage_update`, `status`, and `display_ready` events so late-connecting browsers receive them immediately without triggering ControlMsg queries.

**Per-connection TUI rendering**: Each WebSocket connection gets its own `WebTui` instance (buffer-backed ratatui backend) with independent terminal dimensions. Key events and resizes are routed to the correct connection via `WebTuiCommand` (not broadcast). ANSI frames are sent per-connection via the direct channel.

### Web App Dashboard (app.html)

The primary web interface, served at `/` (root). A 4-tab single-page app with WASM-driven state management:

- **Activity tab**: Scrollable activity log with color-coded entries (system, worker, agent, live, server), turn separators, collapsible items, and notification badges. All events from the agent loop, presence layer, and live model are routed here.
- **Usage tab**: Token usage for main model and presence model, with cost calculations using a built-in pricing table. Displays prompt/completion/cached token breakdowns.
- **Terminal tab**: Embedded xterm.js terminal connected to the server-side TUI (same rendering as native terminal).
- **Displays tab**: noVNC display slots for each Xvfb display created by the agent, enabling remote viewing of graphical applications.

The dashboard uses the Catppuccin Mocha color scheme, is mobile-responsive, and maintains connection state via a dot indicator. Event routing from WebSocket messages to DOM updates goes through `presence-web`'s `AppState` (pure Rust/WASM), which returns `Vec<UiCommand>` that the thin JS layer applies to the DOM.

## Code Conventions

- **Rust 2021 edition** with default rustfmt and clippy settings (no .rustfmt.toml or .clippy.toml)
- **Naming**: snake_case for functions/modules, PascalCase for types, SCREAMING_SNAKE_CASE for constants
- **Error handling**: Custom `thiserror`-based enums (`AgentError`, `CallerError`) with `Result<T>` returns
- **Async**: tokio with full features; background tasks via `tokio::spawn`
- **Shared state**: `Arc<RwLock<T>>` or `Arc<Mutex<T>>` for mutable shared state, `mpsc` channels for communication
- **No unsafe code**: The codebase contains no `unsafe` blocks
- **Tests**: Always inline `#[cfg(test)]` modules — no separate test files
- **WASM boundary**: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)` for any `serde_json::Value` passed to JS (avoids ES6 Map vs Object issues)

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` (full) | Async runtime |
| `serde` + `serde_json` | JSON serialization/deserialization |
| `thiserror` | Error type derivation |
| `chrono` | Timestamp formatting for log directories |
| `env_logger` | Logging |
| `regex` | $NONCE[id] pattern matching, ANSI escape stripping |
| `reqwest` (rustls-tls, stream, multipart) | HTTP client for API calls, SSE streaming, Whisper API |
| `html2text` | HTML to plain text conversion for browse |
| `portable-pty` | PTY session management for execPty |
| `dotenvy` | .env file loading |
| `toml` | intendant.toml config parsing |
| `async-trait` | Async trait support for ChatProvider, Transcriber |
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
| `base64` | Encoding screenshot data and audio to base64 |
| `tokio-tungstenite` | WebSocket server for web gateway and VNC proxy |
| `presence-core` (workspace) | WASM-compatible presence logic (types, tools, dispatch, format, prompt, session protocol) |
| `tempfile` (dev) | Temporary directories in tests |

## Environment Requirements

- **OS**: Linux
- **Permissions**: Runs as unprivileged user with passwordless sudo
- **For intendant**: `.env` file with `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`. Optional: `PROVIDER`, `MODEL_NAME`, `USE_NATIVE_TOOLS`, `STRUCTURED_OUTPUT`, `REASONING_EFFORT`, `REASONING_SUMMARY`, `INTENDANT_LOG_DIR` (set automatically by caller for the runtime)
- **For captureScreen**: ImageMagick `import` command and DISPLAY environment variable (defaults to `:1`)
- **For WASM build**: `wasm-pack` (install via `cargo install wasm-pack`)

## CI/CD

No CI/CD is currently configured. Run `cargo test --bins` and `cargo clippy` locally before committing. Unit tests (`cargo test --bins`) are fast and deterministic — safe for CI. Integration tests (`cargo test --test e2e`) make real API calls to LLM providers — run them manually, not in CI.
