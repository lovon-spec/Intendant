# Intendant

A Rust runtime that executes commands on behalf of an AI agent, plus an AI integration layer that drives the runtime via the OpenAI, Anthropic, or Gemini API. The runtime manages process lifecycles via shared memory-style state files, streams status updates, and persists logs across binary restarts. The CLI features native API tool calling (function calling) with automatic fallback to text-based JSON extraction, a ratatui-based TUI for real-time monitoring and control, a configurable autonomy system with per-action approval, and supports hierarchical multi-agent orchestration with token budget awareness, sub-agent spawning, git worktree isolation, and a tagged knowledge system with pub/sub channels.

## Architecture

```
stdin (JSON) --> intendant-runtime --> spawns bash commands
                  |
                  +--> $INTENDANT_SHARED_DIR/intendant_processes (or /dev/shm, or temp dir)
                  +--> $INTENDANT_SHARED_DIR/intendant_session   (or /dev/shm, or temp dir)
                  +--> ~/.intendant/logs/<timestamp>/  (stdout/stderr logs per nonce)
                  |
                  +--> StatusMonitor --> stdout (status lines)

intendant (3 modes) --> detects project root (git) --> loads memory/knowledge
  |
  +--> User Mode:       spawns orchestrator subprocess, monitors progress (no API calls)
  +--> Sub-Agent Mode:  scoped task, writes results/progress, isolated context
  +--> Direct Mode:     single-loop execution for simple tasks
  |
  +--> Native tool calling (OpenAI/Anthropic/Gemini) with text extraction fallback
  +--> Ratatui TUI:     status bar, scrollable log, approval panel, askHuman input
  +--> MCP Server:      --mcp flag, stdio transport, full parity with TUI (tools + resources)
  +--> Autonomy system: Low/Medium/High/Full + per-category rules from intendant.toml
  +--> Optional control socket (--control-socket): /tmp/intendant-<pid>.sock (JSON-line protocol)
  +--> Token budget tracking (context-window-aware loop termination)
  +--> Sub-agent spawning via env vars (INTENDANT_ROLE, INTENDANT_ID, etc.)
  +--> Git worktree isolation for implementation agents
  +--> Tagged knowledge store with pub/sub channels between agents
```

- **Shared Process State (`intendant_processes`):** Fixed-size array of `ProcessInfo` structs (1024 slots). Each slot stores nonce, PID, status, exit code, and timestamp. Path resolves via `INTENDANT_SHARED_DIR`, then `/dev/shm` if available, else OS temp dir.
- **Session File (`intendant_session`):** Stores the log directory path so consecutive runs reuse the same directory. Uses the same shared-dir resolution.
- **Log Directory (`~/.intendant/logs/<timestamp>/`):** Per-nonce stdout and stderr log files, plus structured session logs. Created once per session.
- **Status Monitor:** Background task that polls SHM for status changes and writes update lines to stdout. Status lines are filtered caller-side to only include nonces from the current command batch.

## Building

```bash
cargo build --release
```

Two binaries are produced:
- `./target/release/intendant-runtime` — the command runtime
- `./target/release/intendant` — the AI CLI/TUI

### Installing

```bash
cargo install --path .
```

Both binaries are installed to `~/.cargo/bin/`. The `intendant` binary embeds default system prompts at compile time, so it works immediately from any directory without needing the source tree.

## Usage

The agent reads a single JSON object from stdin and writes status lines to stdout.

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/intendant-runtime
```

Output:

```
1r0        # nonce 1, running, exit code 0
1c0        # nonce 1, completed, exit code 0
```

Retrieve results in a subsequent run (returns JSON with `content`, `total_size`, `offset`, `bytes_read`):

```bash
echo '{"commands":[{"function":"fetchStatus","nonce":1,"status_type":"stdout"}]}' \
  | ./target/release/intendant-runtime
```

Read only the last 1024 bytes of a log:

```bash
echo '{"commands":[{"function":"fetchStatus","nonce":1,"status_type":"stdout","limit":1024}]}' \
  | ./target/release/intendant-runtime
```

Inspect a file path:

```bash
echo '{"commands":[{"function":"inspectPath","nonce":1,"path":"/etc/hosts"}]}' \
  | ./target/release/intendant-runtime
```

Edit a file:

```bash
echo '{"commands":[{"function":"editFile","nonce":1,"file_path":"/tmp/test.txt","operation":"write","content":"hello"}]}' \
  | ./target/release/intendant-runtime
```

Fetch a web page as text:

```bash
echo '{"commands":[{"function":"browse","nonce":1,"url":"https://example.com"}]}' \
  | ./target/release/intendant-runtime
```

Run stateful commands in a persistent PTY:

```bash
echo '{"commands":[{"function":"execPty","nonce":1,"command":"cd /tmp"},{"function":"execPty","nonce":2,"command":"pwd"}]}' \
  | ./target/release/intendant-runtime
```

Store and recall memory (supports tagged knowledge with channels):

```bash
# Basic store
echo '{"commands":[{"function":"storeMemory","nonce":1,"memory_key":"db-config","memory_summary":"PostgreSQL on port 5432","memory_file":"/path/to/.intendant/memory.json"}]}' \
  | ./target/release/intendant-runtime

# Store with tags and channel
echo '{"commands":[{"function":"storeMemory","nonce":1,"memory_key":"db-config","memory_summary":"PostgreSQL on port 5432","memory_tags":"database,config","memory_channel":"findings","memory_source":"research-1","memory_file":"/path/to/.intendant/memory.json"}]}' \
  | ./target/release/intendant-runtime

# Recall with filters
echo '{"commands":[{"function":"recallMemory","nonce":1,"memory_query":"database","memory_tags":"config","memory_channel":"findings","memory_file":"/path/to/.intendant/memory.json"}]}' \
  | ./target/release/intendant-runtime
```

## Protocol

### Functions

| Function | Description | Key Fields |
|----------|-------------|------------|
| `execAsAgent` | Run a bash command in the background | `command`, `display`, `depending_nonce`, `wait`, `expected_status`, `wait_for_port` |
| `captureScreen` | Screenshot a display via ImageMagick | `display` |
| `fetchStatus` | Read process state/logs (JSON with offset/limit) | `status_type` (`status`, `stdout`, `stderr`, `exit_code`), `offset`, `limit` |
| `inspectPath` | Inspect filesystem path metadata | `path` |
| `editFile` | Structured file editing without shell commands | `file_path`, `operation`, `content`, `match_content`, `line_number`, `end_line` |
| `writeFile` | Alias for `editFile` write operation | `file_path`, `content` |
| `browse` | Fetch URL and convert HTML to text | `url` |
| `askHuman` | Ask the operator a question and wait for response | `question` |
| `execPty` | Run command in a persistent PTY session | `command`, `shell_id` |
| `storeMemory` | Store a knowledge entry with optional tags/channel | `memory_key`, `memory_summary`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source` |
| `recallMemory` | Search knowledge by keyword with optional filters | `memory_query`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source`, `memory_since` |

### Status Codes

| Code | Meaning |
|------|---------|
| `r` | Running |
| `c` | Completed |
| `f` | Failed (could not start) |
| `s` | Skipped (dependency not met) |
| `w` | Waiting (on dependency) |

Status lines are formatted as `[nonce][status_char][exit_code]`, e.g. `42c0` means nonce 42 completed with exit code 0.

### Dependencies

Commands can be chained using `depending_nonce`, `wait`, and `expected_status`. When `wait` is `true`, the dependent command blocks until its dependency finishes. When `false`, it is skipped immediately if the dependency is not yet done.

### Nonce Variables

Use `$NONCE[id]` in command strings to reference the PID of a previously launched nonce. For example, `kill -9 $NONCE[10]` kills the process started by nonce 10.

### Context Management

The model can include a `context` field alongside `commands` to manage conversation history:

```json
{
  "commands": [...],
  "context": {
    "drop_turns": [3, 4, 5],
    "summarize": { "turns": [7, 8, 9, 10], "summary": "Set up nginx with reverse proxy" }
  }
}
```

- **`drop_turns`**: Remove messages at given indices (system prompt and last 2 messages are protected).
- **`summarize`**: Replace a range of messages with a single summary.
- Context-only turns (empty commands) are supported for pruning without executing anything.

## Knowledge System

Project knowledge persists tagged entries across sessions in `<project>/.intendant/memory.json`. The system supports both the legacy key-value format and the new tagged knowledge format with automatic migration.

- **`storeMemory`**: Creates or updates an entry with key, summary, tags, channel, and source. Backward-compatible with old format.
- **`recallMemory`**: Searches entries by keyword with optional filters (tags, channel, source, since timestamp).
- Knowledge is loaded and injected into the conversation at session start.
- Supports pub/sub channels for inter-agent knowledge sharing:
  - Agents publish findings to named channels (e.g., `"findings"`, `"decisions"`)
  - The orchestrator routes knowledge between sibling agents via subscriptions
  - Cursor-based tracking ensures agents only see new entries
- Can be disabled in `intendant.toml`:

```toml
[memory]
enabled = false  # default: true
```

## Testing

```bash
cargo test
```

The test suite covers both binaries:

- **Agent binary:** models serialization, status formatting, error types, shared memory operations, nonce replacement, path inspection, status fetching, dependency checking, command processing, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters.
- **Caller binary:** JSON extraction, done signal handling, conversation management with message layer protection and tool call tracking, context directives (drop/summarize), error types, project detection, config parsing with approval rules, memory/knowledge loading and formatting, provider selection with token usage tracking and Responses API support, structured output and reasoning controls, role mapping, native tool definitions (12 tools, provider conversion formats), tool call batch assembly and result routing, Gemini provider request/response format, sub-agent spawning and result parsing, git worktree lifecycle, user mode orchestration, knowledge pub/sub system, prompt resolution cascade (project root, global config, compiled-in defaults, tools-mode variant), TUI rendering (status bar, log panel, action panel, approval panel, help overlay, layout calculations, orchestrator progress), autonomy level resolution and command classification, event bus dispatch, theme color thresholds, control socket serialization, status line filtering, auto-fetch detection, session log file creation, model summary formatting, Xvfb display configuration per provider, and dynamic display allocation.

## Session Logging

Each `intendant` invocation creates a structured session log directory at `~/.intendant/logs/<timestamp>_<pid>/`. The log provides full observability for debugging and post-session analysis.

### Directory Structure

```
~/.intendant/logs/20260219_040037_119700/
├── session.jsonl                    # Structured event log (one JSON per line)
├── summary.json                     # Post-session summary (task, outcome, turns)
├── 1_stdout.log                     # Runtime stdout for nonce 1
├── 1_stderr.log                     # Runtime stderr for nonce 1
└── turns/
    ├── turn_001_messages.json       # Full messages array sent to API
    ├── turn_001_model.txt           # Full model response
    ├── turn_001_reasoning.txt       # Full reasoning content (if available)
    ├── turn_001_agent_in.json       # Commands sent to runtime (pretty-printed)
    ├── turn_001_stdout.txt          # Agent stdout for this turn
    └── turn_001_stderr.txt          # Agent stderr (only if non-empty)
```

### Event Types in session.jsonl

| Event | Description |
|-------|-------------|
| `session_start` | Session initialization |
| `turn_start` | Turn boundary with budget % and remaining tokens |
| `messages_input` | Full API input logged (file reference to messages.json) |
| `model_response` | Model output with token counts (200-char preview, full in file) |
| `reasoning` | Reasoning summary and full content (if available from API) |
| `json_extracted` | Extracted command JSON with function names |
| `agent_input` | Commands sent to runtime |
| `agent_output` | Runtime stdout/stderr |
| `approval` | Approval decisions (category, preview, decision) |
| `session_end` | Summary with outcome and turn count |

### Querying Logs

```bash
# Overview of a session
cat ~/.intendant/logs/<session>/session.jsonl | jq -r '.event'

# See what the model received on turn 5
cat ~/.intendant/logs/<session>/turns/turn_005_messages.json | jq .

# See model reasoning on turn 3
cat ~/.intendant/logs/<session>/turns/turn_003_reasoning.txt

# Find all commands executed
grep '"event":"agent_input"' ~/.intendant/logs/<session>/session.jsonl | jq -r '.message'
```

## Session Management

State persists across binary restarts via the shared state directory:

- **Process state** is stored in `intendant_processes`.
- **Log directory marker** is stored in `intendant_session`.
- Shared path resolution: `INTENDANT_SHARED_DIR` -> `/dev/shm` (if present) -> system temp dir.

To reset all state (start a fresh session):

```bash
rm -f "${INTENDANT_SHARED_DIR:-/dev/shm}/intendant_processes" \
      "${INTENDANT_SHARED_DIR:-/dev/shm}/intendant_session"
```

## AI Caller

The `intendant` binary detects the project, loads memory, sends the task to an AI model, and feeds the model's JSON output to the `intendant-runtime` binary in a loop.

### Setup

Create a `.env` file or export the variables. The caller searches for `.env` in this order:

1. **Current directory** (and parent directories)
2. **Project root** (git root)
3. **Global config** (`~/.config/intendant/.env`)

For global use after `cargo install`, put your keys in `~/.config/intendant/.env`:

```bash
# OpenAI
OPENAI_API_KEY=sk-...

# Or Anthropic
ANTHROPIC_API_KEY=sk-ant-...

# Or Gemini (Google AI)
GEMINI_API_KEY=AI...

# If multiple keys are set, choose one:
PROVIDER=openai          # or "anthropic" or "gemini"

MODEL_NAME=gpt-5.2-codex # optional, provider-specific default used if omitted

# Disable native tool calling (fall back to text-based JSON extraction)
# USE_NATIVE_TOOLS=false
```

### Running

```bash
# With a task as CLI argument (launches TUI)
./target/release/intendant "List the files in /tmp"

# Headless mode (no TUI, plain text output)
./target/release/intendant --no-tui "List the files in /tmp"

# With autonomy level
./target/release/intendant --autonomy low "rm -rf /tmp/test"

# Specify provider and model
./target/release/intendant --provider anthropic --model claude-sonnet-4-5-20250929 "List files"

# Use Gemini provider
./target/release/intendant --provider gemini --model gemini-2.5-pro "List files"

# Interactive mode (prompts for task on stdin)
./target/release/intendant

# Verbose output (show debug-level log entries)
./target/release/intendant --verbose "echo hello"
```

### CLI Flags

| Flag | Description |
|------|-------------|
| `--provider <name>` | Force provider (`openai`, `anthropic`, or `gemini`) |
| `--model <name>` | Override model name |
| `--verbose` | Show debug-level log entries in TUI |
| `--no-tui` | Disable TUI, use plain text output |
| `--autonomy <level>` | Set autonomy level (`low`, `medium`, `high`, `full`) |
| `--log-file <dir>` | Override session log directory |
| `--mcp` | Run as MCP server on stdio (replaces TUI) |
| `--control-socket` | Enable Unix control socket (TUI mode) |
| `--vision` | Launch Xvfb virtual display (auto-allocated `:99+`) sized for the provider's vision model |

The TUI launches only when both stdin and stdout are terminals. When piping input/output or in sub-agent mode, `intendant` falls back to headless mode.

### Execution Modes

`intendant` operates in one of three modes, selected automatically:

**Sub-Agent Mode** (when `INTENDANT_ROLE` env var is set):
- Runs as a child agent with a scoped task
- Writes periodic progress to `INTENDANT_PROGRESS_FILE`
- Writes final results (summary, findings, artifacts, token usage) to `INTENDANT_RESULT_FILE`
- Uses role-specific system prompts (`SysPrompt_research.md`, `SysPrompt_implementation.md`, etc.)

**User Mode** (complex tasks without `INTENDANT_ROLE`):
- Pure subprocess monitor — makes zero model API calls at Layer 0
- Spawns an orchestrator sub-agent as a child process via `tokio::process::Command`
- Polls the orchestrator's progress file every 500ms, relays status to the TUI or stdout
- Reads the orchestrator's result file on exit; synthesizes a failure if the process crashes
- `kill_on_drop(true)` ensures the orchestrator is terminated if the user quits the TUI

**Direct Mode** (simple tasks without `INTENDANT_ROLE`):
- Single-loop execution similar to the original behavior
- Used for short, single-line tasks that don't need orchestration

### `askHuman` Behavior (Important)

- In **TUI mode**, `askHuman` opens the input panel and writes your answer to the shared response file (`intendant_human_response` in the shared state dir).
- Empty submit is rejected in the TUI; you must provide non-empty input or press `Esc` to cancel.
- In **headless mode** (`--no-tui` or non-interactive stdin), `askHuman` cannot be answered interactively. The loop now tells the model to continue with explicit assumptions instead of waiting for the runtime timeout.
- Runtime-level timeout for unanswered `askHuman` remains `5 minutes`.

### How it works

1. Loads `.env` and selects the API provider (OpenAI, Anthropic, or Gemini). OpenAI uses the Responses API (`/v1/responses`), Anthropic uses the Messages API, Gemini uses the `generateContent` endpoint
2. Configures structured output (JSON mode), reasoning controls, native tool calling, and max output tokens based on model capabilities and env vars
3. Detects the project root (via `git rev-parse --show-toplevel`, falls back to cwd)
4. Resolves role-appropriate system prompt via cascade: project root → `~/.config/intendant/` → compiled-in default. When native tools are enabled, uses the condensed `SysPrompt_tools.md` (tool docs live in API tool definitions instead of prose)
5. Injects the project working directory into the conversation so the model knows which project to work in
6. Loads knowledge from `<project>/.intendant/memory.json`, injects into conversation
7. Logs the full messages array to `turn_NNN_messages.json` before each API call
8. Sends the task to the chat API (with `max_tokens`/`max_output_tokens`, optional `reasoning`, optional JSON format, and native tool definitions when enabled)
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
15. Pipes the JSON to the `intendant-runtime` binary, reads stdout/stderr with adaptive timeouts:
    - Default: idle-before-first `2s`, idle-after-first `1s`, hard `30s`
    - `fetchStatus(wait=true)`: idle-before-first `15s`, hard `45s`
    - `askHuman`: idle-before-first `330s`, idle-after-first `1s`, hard `600s`
16. Filters agent status lines to only include nonces from the current command batch (reduces noise)
17. For single standalone `execAsAgent` commands that complete successfully, auto-appends stdout (saves the model a `fetchStatus` round-trip)
18. Feeds the agent output back as the next user message (text path) or as individual tool results (tool call path), appending a token budget summary
19. Repeats until the model signals done, responds with no JSON, or the context budget is exhausted
20. In headless mode, if the model emits `askHuman`, the loop now sends a recovery prompt back to the model (continue with explicit assumptions) instead of blocking on human-input timeout

## Environment

- **OS:** Debian 12+
- **Runtime:** Tokio async
- **Display:** DISPLAY is set from the environment (configurable via `display` field per command, defaults to env `DISPLAY` or `:1`). With `--vision`, Xvfb is auto-allocated on a free display (`:99+`)
- **Permissions:** Runs as unprivileged user with passwordless sudo

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` / `OPENAI` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` / `ANTHROPIC` | — | Anthropic API key |
| `GEMINI_API_KEY` | — | Google AI (Gemini) API key |
| `PROVIDER` | auto-detect | `"openai"`, `"anthropic"`, or `"gemini"` (used when multiple keys are set) |
| `MODEL_NAME` | per-provider default | Model to use (e.g. `gpt-5.2-codex`, `claude-sonnet-4-5-20250929`, `gemini-2.5-pro`) |
| `USE_NATIVE_TOOLS` | `true` | Enable native API tool calling; `false` falls back to text-based JSON extraction |
| `INTENDANT_IDLE_TIMEOUT` | `2` | Seconds to wait for first agent stdout byte before assuming idle (baseline mode) |
| `INTENDANT_HARD_TIMEOUT` | `30` | Maximum seconds to wait for agent output |
| `MODEL_CONTEXT_WINDOW` | per-model default | Context window size in tokens |
| `MAX_OUTPUT_TOKENS` | per-model default | Max output tokens per API call (sent to API) |
| `STRUCTURED_OUTPUT` | `true` for gpt-5+/o3/o4 | Enable JSON object mode for deterministic parsing |
| `REASONING_EFFORT` | — | Reasoning effort for GPT-5/o3/o4 models (`low`, `medium`, `high`) |
| `REASONING_SUMMARY` | — | Reasoning summary mode (`auto`, `concise`, `detailed`) |
| `INTENDANT_ROLE` | — | Sub-agent role (`orchestrator`, `research`, `implementation`, `testing`) |
| `INTENDANT_ID` | — | Unique sub-agent identifier |
| `INTENDANT_RESULT_FILE` | — | Path for sub-agent to write final results |
| `INTENDANT_PROGRESS_FILE` | — | Path for sub-agent to write periodic progress |
| `INTENDANT_TASK` | — | Task description for sub-agent mode |
| `INTENDANT_PARENT_KNOWLEDGE` | — | Path to parent's knowledge store for inheritance |
| `INTENDANT_SHARED_DIR` | auto | Shared state directory for process/session/human I/O files (`/dev/shm` if available, else temp dir) |

Timeouts are automatically extended for slow-start workflows (`askHuman`, `fetchStatus(wait=true)`). Manual override via env vars is still supported.

### Project Configuration

Create `intendant.toml` in the project root:

```toml
[memory]
enabled = true  # default: true

[model]
context_window = 200000       # override per-model default
max_output_tokens = 8192      # override per-model default

[orchestrator]
max_parallel_agents = 4       # max concurrent sub-agents
sub_agent_dir = ".intendant/subagents"  # where sub-agent workspaces are created

[approval]
file_read = "auto"            # auto-approve file reads
file_write = "ask"            # ask before file writes (default)
file_delete = "ask"           # ask before file deletes (default)
command_exec = "auto"         # auto-approve command execution
network = "auto"              # auto-approve network requests
destructive = "ask"           # ask before destructive commands (default)
```

### System Prompts

System prompts are compiled into the binary at build time, so `intendant` works from any directory without needing the source tree. Two base prompt variants exist:

- **`SysPrompt.md`** — Full prompt with JSON schema and per-function documentation (used with text-based JSON extraction)
- **`SysPrompt_tools.md`** — Condensed prompt for native tool calling mode (function docs live in API tool definitions, reducing system prompt tokens)

The active variant is selected automatically based on whether the provider has native tool calling enabled.

Prompts are resolved using a 3-layer cascade (highest priority first):

1. **Project root** — `<git-root>/SysPrompt.md` or `SysPrompt_tools.md` (per-project customization)
2. **Global config** — `~/.config/intendant/SysPrompt.md` or `SysPrompt_tools.md` (user-wide customization)
3. **Compiled-in default** — always available, zero-config

Role-specific prompts (`SysPrompt_orchestrator.md`, `SysPrompt_research.md`, `SysPrompt_implementation.md`) follow the same cascade and are appended to the base prompt.

To customize prompts for a specific project, place your modified `.md` files in the project's git root. For user-wide customization, place them in `~/.config/intendant/`.

## TUI

`intendant` includes a ratatui-based terminal UI that launches automatically when both stdin and stdout are terminals. The TUI provides real-time monitoring and control of the agent loop.

### Layout

```
┌─────────────────────────────────────────────┐
│ StatusBar: provider │ model │ turn │ budget  │  1 line
├─────────────────────────────────────────────┤
│ ActionPanel: phase + spinner + key hints    │  2 lines
├─────────────────────────────────────────────┤
│                                             │
│ LogPanel: scrollable, color-coded entries   │  fills remaining
│                                             │
├─────────────────────────────────────────────┤
│ ApprovalPanel / InputPanel (conditional)    │  3-4 lines
└─────────────────────────────────────────────┘
```

### Key Bindings

| Key | Action |
|-----|--------|
| `q` / `Ctrl-C` | Quit |
| `v` | Toggle verbose mode |
| `?` | Help overlay |
| `+` / `-` | Cycle autonomy level |
| `Up`/`Down`/`PgUp`/`PgDn` | Scroll log |
| `Home` / `End` | Jump to top/bottom of log |
| `1`-`3` | Toggle panels (status, action, log) |
| `y` / `Enter` | Approve pending action |
| `s` | Skip pending action |
| `a` | Auto-approve all remaining |
| `n` | Deny and stop |

## Autonomy System

The autonomy system controls which actions require human approval. It operates on three layers:

**Layer 1 — Global level** (CLI `--autonomy`, toggleable in TUI with `+`/`-`):

| Level | Behavior |
|-------|----------|
| Low | Ask before every command execution |
| Medium | Ask before writes, network, destructive (default) |
| High | Only ask for unavoidable human input |
| Full | Never ask (fully autonomous) |

**Layer 2 — Per-category rules** (from `intendant.toml` `[approval]` section):
Override the global level for specific action categories. Rules: `auto` (always approve), `ask` (require approval), `deny` (always deny).

**Layer 3 — Per-action approval** (TUI panel):
When approval is needed, the agent loop pauses and the TUI shows the command preview. The user can approve, skip, deny, or switch to auto-approve mode.

Action categories are determined by analyzing command JSON: shell commands are classified by inspecting for destructive patterns (`rm`, `kill`, `dd`, `mkfs`), network operations (`curl`, `wget`, `ssh`), file operations, etc.

## Control Socket

When `--control-socket` is enabled in TUI mode, a Unix domain socket is created at `/tmp/intendant-<pid>.sock`.

Current status:
- Outbound event broadcast is implemented.
- Inbound command handling is implemented for status, approval, denial, human input, autonomy change, and quit.
- Socket server is opt-in via `--control-socket`.

### Inbound Commands (JSON-line)

```json
{"action": "status"}
{"action": "approve", "id": 123}
{"action": "deny", "id": 123}
{"action": "input", "text": "answer to askHuman"}
{"action": "set_autonomy", "level": "high"}
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
```

Example usage:
```bash
echo '{"action":"status"}' | socat - UNIX:/tmp/intendant-$(pgrep intendant).sock
```

## MCP Server

The `--mcp` flag launches Intendant as a [Model Context Protocol](https://modelcontextprotocol.io/) server on stdio. This lets external AI agents (Claude Code, Codex, etc.) observe and control Intendant with full parity to the TUI — every action a human can take in the TUI is available as an MCP tool.

### Running

```bash
# Launch as MCP server (stdio transport)
./target/release/intendant --mcp "Deploy the application"

# With provider/model overrides
./target/release/intendant --mcp --provider anthropic --model claude-sonnet-4-5-20250929 "Fix the tests"

# With autonomy preset
./target/release/intendant --mcp --autonomy high "Refactor the auth module"
```

### Client Configuration

Add Intendant to your MCP client's config. For Claude Code (`~/.claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "intendant": {
      "command": "intendant",
      "args": ["--mcp", "Your task description here"]
    }
  }
}
```

### Tools

All tools mirror TUI actions. The server enforces compile-time parity — adding a new user action to the TUI requires implementing it in the MCP server (and vice versa).

| Tool | Description | Parameters |
|------|-------------|------------|
| `get_status` | Current status: provider, model, turn, budget, phase, autonomy, verbosity, tokens | — |
| `get_logs` | Log entries with cursor-based pagination and level filtering | `since_id?`, `level_filter?`, `limit?` |
| `get_pending_approval` | Current pending approval request (or null) | — |
| `get_pending_input` | Current pending human question (or null) | — |
| `approve` | Approve a pending command (TUI: `y`) | `id` |
| `deny` | Deny a pending command and stop (TUI: `n`) | `id` |
| `skip` | Skip a pending command, continue (TUI: `s`) | `id` |
| `approve_all` | Approve and set autonomy to Full (TUI: `a`) | `id` |
| `respond` | Answer an `askHuman` question (TUI: type + Enter) | `text` |
| `set_autonomy` | Set autonomy level (TUI: `+`/`-`) | `level`: `"low"`, `"medium"`, `"high"`, `"full"` |
| `set_verbosity` | Set log verbosity (TUI: `v`) | `level`: `"quiet"`, `"normal"`, `"verbose"`, `"debug"` |
| `quit` | Shut down the agent (TUI: `q`) | — |

### Resources

Resources provide push-based state observation via subscriptions. The server sends `notifications/resources/updated` when state changes, so clients know to re-fetch.

| URI | Description |
|-----|-------------|
| `intendant://status` | Provider, model, turn count, budget %, phase, autonomy level |
| `intendant://logs` | Last 100 chronological log entries (same as TUI log panel) |
| `intendant://pending-approval` | Current pending approval request, if any |
| `intendant://pending-input` | Current pending human question, if any |

### Typical Agent Workflow

1. Call `get_status` to see the current phase and budget
2. Poll `get_logs` with `since_id` to stream new events
3. When an approval is needed, `get_pending_approval` returns the command preview — call `approve`, `deny`, or `skip`
4. When `askHuman` triggers, `get_pending_input` returns the question — call `respond` with your answer
5. Call `quit` when done
