# CLAUDE.md

## Project Overview

This is **Intendant**, a Rust runtime for autonomous AI agents with process lifecycle management. It executes bash commands on behalf of AI agents, tracks process state via shared memory, and persists logs across binary restarts.

The project produces two binaries:
- **intendant-runtime** — Command runtime that reads JSON from stdin, executes commands sequentially (blocking until completion), and writes result lines to stdout
- **intendant** — AI integration layer (CLI/TUI) that drives the runtime via the OpenAI Responses API or Anthropic Messages API in a loop

## Repository Structure

```
src/
├── main.rs              # intendant-runtime binary entry point (tokio async main)
├── agent.rs             # Core agent implementation
│                        #   - Shared memory management
│                        #   - Blocking command execution (execAsAgent) — returns exit code, stdout/stderr tail
│                        #   - Screenshot capture (captureScreen)
│                        #   - Path inspection (inspectPath)
│                        #   - File editing (editFile)
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
        ├── provider.rs      # Multi-provider API client (OpenAI Responses API + Anthropic), structured output, reasoning controls
        ├── conversation.rs  # Message management with layer protection, drop/summarize, budget tracking
        ├── agent_runner.rs  # Spawns intendant-runtime subprocess, waits for completion with hard timeout (askHuman-aware)
        ├── knowledge.rs     # Tagged knowledge store with pub/sub channels, cursor-based routing
        ├── memory.rs        # Backward-compatible memory wrapper delegating to knowledge.rs
        ├── sub_agent.rs     # Sub-agent spawning, result/progress I/O, role-specific configuration
        ├── worktree.rs      # Git worktree management for isolated implementation agents
        ├── user_mode.rs     # User-mode orchestrator spawning, progress monitoring, input relay
        ├── prompts.rs       # System prompt resolution: compile-time defaults (include_str!) + 3-layer cascade
        ├── project.rs       # Project detection (git root), config parsing (intendant.toml + [approval])
        ├── autonomy.rs      # Autonomy levels, action categories, approval rules, command classification
        ├── control.rs       # Unix control socket server (JSON-line protocol at /tmp/intendant-<pid>.sock)
        ├── error.rs         # CallerError enum (includes Tui variant)
        └── tui/
            ├── mod.rs       # Tui struct: terminal init/restore, panic hook, render+event loop
            ├── app.rs       # App state machine, event dispatch, askHuman/approval modes
            ├── event.rs     # AppEvent enum, EventBus (mpsc wrapper), crossterm adapter, askHuman monitor
            ├── widgets.rs   # StatusBar, LogPanel, ActionPanel, InputPanel, ApprovalPanel rendering
            ├── layout.rs    # Panel sizing with constraints, responsive to terminal size
            └── theme.rs     # Color/style constants (Catppuccin Mocha-inspired)
SysPrompt.md                 # Default system prompt (direct mode)
SysPrompt_user.md            # User-facing mode prompt
SysPrompt_orchestrator.md    # Orchestrator agent prompt
SysPrompt_research.md        # Research sub-agent prompt
SysPrompt_implementation.md  # Implementation sub-agent prompt
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
./target/release/intendant --continue "fix that bug"       # Resume most recent session
./target/release/intendant --resume abc123 "continue"      # Resume specific session by ID
echo "task" | ./target/release/intendant                   # Auto-detects non-TTY, runs headless
```

## Testing

```bash
cargo test                # Run all tests
cargo test -- --list      # List all test names
```

All tests are inline `#[cfg(test)]` modules in the same files as the code they test. Async tests use `#[tokio::test]`. The `tempfile` crate provides isolated temporary directories for tests that touch the filesystem or shared memory.

Test coverage includes:
- **agent.rs**: Process info operations, blocking command execution, path inspection, nonce reference replacement, process mapping, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters
- **models.rs**: Serialization roundtrips, deserialization of minimal/full commands, repr(C) layout
- **error.rs**: Display formatting, From conversions
- **utils.rs**: Timestamp validity
- **caller/main.rs** (tests across caller modules): JSON extraction, context directives, done signal handling, budget constants, task classification, CLI flags, EventBus emit, batch assembly, tool name mapping
- **caller/conversation.rs**: Message ordering, serialization, drop/summarize turns, message layer protection, budget tracking, save/load JSONL roundtrip
- **caller/knowledge.rs**: Pub/sub lifecycle, subscription/cursor tracking, tag/channel/keyword filtering, old format migration, save/load roundtrip, knowledge routing
- **caller/sub_agent.rs**: Spawn command generation, result/progress I/O, serialization, role roundtrips, directory scanning
- **caller/worktree.rs**: Full lifecycle (create/list/merge/remove), conflict handling
- **caller/user_mode.rs**: Orchestrator spec generation, progress formatting, input relay, prompt resolution
- **caller/project.rs**: Config parsing, project paths, sub-agent directory, approval config parsing
- **caller/memory.rs**: Memory/knowledge loading, formatting, format migration
- **caller/prompts.rs**: Compiled-in defaults non-empty, cascade resolution (project root, global config, compiled default), role-specific prompt appending (orchestrator, research, implementation, testing, direct), project override combinations
- **caller/provider.rs**: Provider selection, token usage parsing, context window defaults, Responses API types, structured output, reasoning controls, role mapping
- **caller/error.rs**: Display formatting, type conversions (including Tui variant)
- **caller/autonomy.rs**: Autonomy levels (display, parse, cycle), action categories, approval rules, needs_approval logic, command classification (exec, destructive, network, file write, askHuman, browse), batch classification
- **caller/control.rs**: Socket path, outbound event serialization, broadcast, server lifecycle
- **caller/tui/app.rs**: App defaults, logging (ring buffer), scrolling, key handling (quit, verbose, help, scroll, approval responses), event dispatch (all AppEvent variants including OrchestratorProgress), bottom panel heights, model summary formatting (exec, edit, multiple commands, done signal, askHuman, invalid JSON)
- **caller/tui/event.rs**: EventBus send/receive/clone, ControlMsg deserialization (all variants), serialization roundtrip, ApprovalResponse variants
- **caller/tui/layout.rs**: Layout calculation (all panel combos, with/without bottom panel, hidden panels, small terminal)
- **caller/tui/widgets.rs**: Log entry formatting (all levels, verbose/non-verbose), string truncation
- **caller/tui/theme.rs**: Budget color thresholds, spinner frames, action style variants, autonomy color variants
- **caller/tui/mod.rs**: TestBackend rendering (default state, log entries, approval panel, help overlay, all phases, verbose modes, small terminal)
- **caller/agent_runner.rs**: askHuman detection in JSON input
- **caller/session_log.rs**: UUID-based session directories, session metadata (write_meta, find_latest_session, find_session_by_id), directory structure creation, JSONL event validity, turn tracking, model response file creation, agent input pretty-printing, agent output file creation (stdout/stderr split), approval log searchability, JSON extraction logging, summary file creation, multi-turn file separation, messages input logging, reasoning content logging (full and summary-only)

## Architecture Details

### Shared Memory

Process state lives in `/dev/shm/intendant_processes` — a fixed-size array of 1024 `ProcessInfo` slots (repr(C) structs). Each slot holds: nonce (u64), PID (i32), status (u8), exit code (i32), timestamp (u64). This survives binary restarts since `/dev/shm` is tmpfs.

The process map (`HashMap<u64, usize>`) is rebuilt from shared memory on every startup by scanning all 1024 slots for non-zero nonces.

### Session Management

Each invocation creates an isolated session with a UUID-based directory at `~/.intendant/logs/<uuid>/`. No global state is used for session tracking. The log directory is passed to the runtime via the `INTENDANT_LOG_DIR` environment variable.

Each session directory contains:
- `session_meta.json` — session metadata (session_id, created_at, project_root, task, status, last_turn)
- `session.jsonl` — structured event log
- `conversation.jsonl` — serialized conversation for resume support
- `human_question` / `human_response` — askHuman IPC files (session-scoped)
- `turns/` — per-turn model responses and agent I/O

Sessions can be resumed with `--continue` (most recent session for the project) or `--resume <id>` (specific session by ID or prefix). To reset shared memory: `rm -f /dev/shm/intendant_processes`.

### Execution Model

Commands are processed sequentially. Each command blocks until completion and returns its result directly (exit code, stdout tail, stderr tail for exec commands). The runtime exits after processing all commands.

### Nonce Variables

`$NONCE[id]` in command strings is replaced with the PID of the process launched by that nonce. Handled by regex-based substitution in `replace_nonce_refs()`.

### Intendant Flow

`intendant` operates in three modes based on environment:

**Sub-Agent Mode** (`INTENDANT_ROLE` set): Runs with scoped task, writes progress/results to files, uses role-specific system prompt.

**User Mode** (complex task, no `INTENDANT_ROLE`): Pure subprocess monitor — makes zero model API calls. Spawns orchestrator as a child process, polls its progress file every 500ms, reads its result file on exit. `kill_on_drop(true)` ensures cleanup on TUI quit.

**Direct Mode** (simple task, no `INTENDANT_ROLE`): Single-loop execution:
1. Selects API provider (OpenAI or Anthropic) from env, configures structured output and reasoning controls
2. Detects project root via git, loads `intendant.toml` config
3. Reads role-appropriate system prompt
4. Injects project knowledge into conversation
5. Budget-aware loop (stops at context exhaustion, `done` signal, or 500-turn safety cap): send to model -> extract JSON -> check done signal -> apply context directives -> inject project context -> pipe to agent -> append budget summary -> feed output back

### TUI Mode

When stdin is a TTY and `--no-tui` is not set, `intendant` launches a ratatui-based terminal UI:
- **Status bar**: Provider, model, turn count, budget percentage, autonomy level
- **Action panel**: Current phase (Thinking/RunningAgent/Orchestrating/WaitingApproval/WaitingHuman/Done) with spinner
- **Log panel**: Scrollable chronological log of all events with color-coded levels
- **Approval panel**: Shown when an action needs user approval (y/s/a/n keys)
- **Input panel**: Shown when askHuman is triggered (tui-textarea for response)
- **Help overlay**: Key bindings reference (? key)

The agent loop runs in a background tokio task and communicates with the TUI via an `EventBus` (unbounded mpsc channel of `AppEvent`). When `bus` is `None` (headless mode), all output goes to stdout/stderr as before.

### Autonomy System

Three-layer autonomy control:

1. **Global level** (`--autonomy` flag, +/- keys in TUI): Low/Medium/High/Full
2. **Category rules** (`[approval]` section in intendant.toml): per-category Auto/Ask/Deny
3. **Per-action approval** (TUI only): approve/skip/approve-all/deny

Commands are classified into categories (FileRead, FileWrite, FileDelete, CommandExec, NetworkRequest, Destructive, HumanInput) by `autonomy::classify_command()`. Shell commands are further classified by inspecting the command string for destructive patterns (rm, kill), network tools (curl, wget, git), and file writes (redirects, tee, mv, cp).

### Control Socket

A Unix socket server at `/tmp/intendant-<pid>.sock` enables programmatic control. JSON-line protocol supports: status, approve, deny, input, set_autonomy, quit. Outbound events are broadcast to all connected clients.

### MCP Hot Reload

The `reload` MCP tool rebuilds the binary and replaces the running process via `exec()`. A `ReloadTransport` wrapper injects a synthetic MCP initialization handshake so rmcp's `serve()` works transparently after exec. The `INTENDANT_MCP_RELOAD` env var signals the new process to use `ReloadTransport` instead of plain stdio.

### OpenAI API Features

- **Structured output**: JSON object mode (`text.format`) is enabled by default for capable models (gpt-5+, o3, o4). Controlled via `STRUCTURED_OUTPUT` env var. Eliminates brittle free-text JSON extraction.
- **Reasoning controls**: For reasoning models (gpt-5+, o3, o4), `REASONING_EFFORT` ("low"/"medium"/"high") and `REASONING_SUMMARY` ("auto"/"concise"/"detailed") tune quality/cost tradeoffs.
- **Max output tokens**: Sent as `max_output_tokens` on all OpenAI Responses API requests.
- **Role mapping**: Responses API passes through all non-system roles (user, assistant, developer, tool) instead of filtering to user/assistant only.
- **Done signal**: With structured output enabled, models signal task completion via `{"commands": [], "done": true}` instead of prose responses.

## Code Conventions

- **Rust 2021 edition** with default rustfmt and clippy settings (no .rustfmt.toml or .clippy.toml)
- **Naming**: snake_case for functions/modules, PascalCase for types, SCREAMING_SNAKE_CASE for constants
- **Error handling**: Custom `thiserror`-based enums (`AgentError`, `CallerError`) with `Result<T>` returns
- **Async**: tokio with full features; background tasks via `tokio::spawn`
- **Shared state**: `Arc<RwLock<T>>` for mutable shared state, `mpsc` channels for communication
- **Unsafe code**: Used sparingly for memory-mapped file pointer operations (reading/writing `ProcessInfo` structs to shared memory)
- **Tests**: Always inline `#[cfg(test)]` modules — no separate test files

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` (full) | Async runtime |
| `serde` + `serde_json` | JSON serialization/deserialization |
| `thiserror` | Error type derivation |
| `memmap2` | Memory-mapped files for shared memory |
| `chrono` | Timestamp formatting for log directories |
| `env_logger` | Logging |
| `regex` | $NONCE[id] pattern matching |
| `reqwest` (rustls-tls) | HTTP client for API calls |
| `html2text` | HTML to plain text conversion for browse |
| `portable-pty` | PTY session management for execPty |
| `dotenvy` | .env file loading |
| `toml` | intendant.toml config parsing |
| `async-trait` | Async trait support for ChatProvider |
| `ratatui` | Terminal UI framework |
| `crossterm` | Terminal input/output backend (event-stream feature) |
| `tui-textarea` | Text input widget for askHuman responses |
| `tokio-stream` | Stream utilities for crossterm EventStream |
| `tempfile` (dev) | Temporary directories in tests |

## Environment Requirements

- **OS**: Linux (requires `/dev/shm` for shared memory)
- **Permissions**: Runs as unprivileged user with passwordless sudo
- **For intendant**: `.env` file with `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, optional `PROVIDER`, `MODEL_NAME`, `STRUCTURED_OUTPUT`, `REASONING_EFFORT`, `REASONING_SUMMARY`, `INTENDANT_LOG_DIR` (set automatically by caller for the runtime)
- **For captureScreen**: ImageMagick `import` command and DISPLAY environment variable (defaults to `:1`)

## CI/CD

No CI/CD is currently configured. Run `cargo test` and `cargo clippy` locally before committing.
