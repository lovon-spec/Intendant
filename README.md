# Intendant

A Rust runtime that executes commands on behalf of an AI agent, plus an AI integration layer that drives the runtime via the OpenAI or Anthropic API. The runtime manages process lifecycles via shared memory (SHM), streams status updates, and persists logs across binary restarts. The CLI features a ratatui-based TUI for real-time monitoring and control, a configurable autonomy system with per-action approval, and supports hierarchical multi-agent orchestration with token budget awareness, sub-agent spawning, git worktree isolation, and a tagged knowledge system with pub/sub channels.

## Architecture

```
stdin (JSON) --> intendant-runtime --> spawns bash commands
                  |
                  +--> /dev/shm/intendant_processes  (process state, survives restarts)
                  +--> /dev/shm/intendant_session     (log directory path, survives restarts)
                  +--> ~/.intendant/logs/<timestamp>/  (stdout/stderr logs per nonce)
                  |
                  +--> StatusMonitor --> stdout (status lines)

intendant (3 modes) --> detects project root (git) --> loads memory/knowledge
  |
  +--> User Mode:       spawns orchestrator, monitors progress, relays to user
  +--> Sub-Agent Mode:  scoped task, writes results/progress, isolated context
  +--> Direct Mode:     single-loop execution for simple tasks
  |
  +--> Ratatui TUI:     status bar, scrollable log, approval panel, askHuman input
  +--> Autonomy system: Low/Medium/High/Full + per-category rules from intendant.toml
  +--> Control socket:  /tmp/intendant-<pid>.sock (JSON-line protocol)
  +--> Token budget tracking (context-window-aware loop termination)
  +--> Sub-agent spawning via env vars (INTENDANT_ROLE, INTENDANT_ID, etc.)
  +--> Git worktree isolation for implementation agents
  +--> Tagged knowledge store with pub/sub channels between agents
```

- **Shared Memory (`/dev/shm/intendant_processes`):** Fixed-size array of `ProcessInfo` structs (1024 slots). Each slot stores nonce, PID, status, exit code, and timestamp. Survives binary restarts since it lives on tmpfs.
- **Session File (`/dev/shm/intendant_session`):** Stores the log directory path so consecutive runs reuse the same directory.
- **Log Directory (`~/.intendant/logs/<timestamp>/`):** Per-nonce stdout and stderr log files. Created once per session.
- **Status Monitor:** Background task that polls SHM for status changes and writes update lines to stdout.

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

The test suite covers both binaries with several hundred unit/integration tests:

- **Agent binary (114 tests):** models serialization, status formatting, error types, shared memory operations, nonce replacement, path inspection, status fetching, dependency checking, command processing, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters.
- **Caller binary (292 tests):** JSON extraction, done signal handling, conversation management with message layer protection, context directives (drop/summarize), error types, project detection, config parsing with approval rules, memory/knowledge loading and formatting, provider selection with token usage tracking and Responses API support, structured output and reasoning controls, role mapping, sub-agent spawning and result parsing, git worktree lifecycle, user mode orchestration, knowledge pub/sub system, prompt resolution cascade (project root, global config, compiled-in defaults), TUI rendering (status bar, log panel, action panel, approval panel, help overlay, layout calculations), autonomy level resolution and command classification, event bus dispatch, theme color thresholds, and control socket serialization.

## Session Management

State persists across binary restarts via `/dev/shm/`:

- **Process state** is stored in `/dev/shm/intendant_processes` — the process map is rebuilt from SHM on each startup.
- **Log directory** is stored in `/dev/shm/intendant_session` — subsequent runs reuse the same log directory.

To reset all state (start a fresh session):

```bash
rm -f /dev/shm/intendant_processes /dev/shm/intendant_session
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

# If both are set, choose one:
PROVIDER=openai          # or "anthropic"

MODEL_NAME=gpt-5.2-codex # optional, provider-specific default used if omitted
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

# Interactive mode (prompts for task on stdin)
./target/release/intendant

# Verbose output (show debug-level log entries)
./target/release/intendant --verbose "echo hello"
```

### CLI Flags

| Flag | Description |
|------|-------------|
| `--provider <name>` | Force provider (`openai` or `anthropic`) |
| `--model <name>` | Override model name |
| `--verbose` | Show debug-level log entries in TUI |
| `--no-tui` | Disable TUI, use plain text output |
| `--autonomy <level>` | Set autonomy level (`low`, `medium`, `high`, `full`) |
| `--log-file <path>` | Write log output to file |
| `--control-socket` | Reserved for headless socket control (currently only TUI mode opens the socket) |

The TUI launches automatically when stdin is a terminal. When piping input or in sub-agent mode, `intendant` falls back to headless mode.

### Execution Modes

`intendant` operates in one of three modes, selected automatically:

**Sub-Agent Mode** (when `INTENDANT_ROLE` env var is set):
- Runs as a child agent with a scoped task
- Writes periodic progress to `INTENDANT_PROGRESS_FILE`
- Writes final results (summary, findings, artifacts, token usage) to `INTENDANT_RESULT_FILE`
- Uses role-specific system prompts (`SysPrompt_research.md`, `SysPrompt_implementation.md`, etc.)

**User Mode** (complex tasks without `INTENDANT_ROLE`):
- Spawns an orchestrator sub-agent to handle the task
- Monitors orchestrator progress, relays status to user
- User conversation is protected from auto-pruning (message layer protection)
- Supports relaying user input to the orchestrator

**Direct Mode** (simple tasks without `INTENDANT_ROLE`):
- Single-loop execution similar to the original behavior
- Used for short, single-line tasks that don't need orchestration

### `askHuman` Behavior (Important)

- In **TUI mode**, `askHuman` opens the input panel and writes your answer to `/dev/shm/intendant_human_response`.
- Empty submit is rejected in the TUI; you must provide non-empty input or press `Esc` to cancel.
- In **headless mode** (`--no-tui` or non-interactive stdin), `askHuman` cannot be answered interactively. The loop now tells the model to continue with explicit assumptions instead of waiting for the runtime timeout.
- Runtime-level timeout for unanswered `askHuman` remains `5 minutes`.

### How it works

1. Loads `.env` and selects the API provider (OpenAI or Anthropic). All OpenAI models use the Responses API (`/v1/responses`)
2. Configures structured output (JSON mode), reasoning controls, and max output tokens based on model capabilities and env vars
3. Detects the project root (via `git rev-parse --show-toplevel`, falls back to cwd)
4. Resolves role-appropriate system prompt via cascade: project root → `~/.config/intendant/` → compiled-in default
5. Loads knowledge from `<project>/.intendant/memory.json`, injects into conversation
6. Sends the task to the chat API (with `max_tokens`/`max_output_tokens`, optional `reasoning`, and optional JSON format)
7. Extracts JSON from the model's response (handles structured output, code fences, and bare JSON)
8. Checks for explicit `done` signal (`{"done": true}`) for task completion in JSON mode
9. Applies context directives (`drop_turns`, `summarize`) to the conversation
10. Injects project context (`memory_file`) into relevant commands
11. Classifies commands by action category (file read/write/delete, exec, network, destructive) and checks autonomy rules
12. If approval is required, emits an approval request to the TUI and waits for user response
13. Pipes the JSON to the `intendant-runtime` binary, reads stdout/stderr with adaptive timeouts:
    - Default: idle-before-first `2s`, idle-after-first `1s`, hard `30s`
    - `fetchStatus(wait=true)`: idle-before-first `15s`, hard `45s`
    - `askHuman`: idle-before-first `330s`, idle-after-first `1s`, hard `600s`
14. Feeds the agent output back as the next user message, appending a token budget summary
15. Repeats until the model signals done, responds with no JSON, or the context budget is exhausted
16. In headless mode, if the model emits `askHuman`, the loop now sends a recovery prompt back to the model (continue with explicit assumptions) instead of blocking on human-input timeout

## Environment

- **OS:** Debian 12+
- **Runtime:** Tokio async
- **Display:** DISPLAY is automatically set to `:1` (configurable via `display` field) for GUI commands
- **Permissions:** Runs as unprivileged user with passwordless sudo

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` / `OPENAI` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` / `ANTHROPIC` | — | Anthropic API key |
| `PROVIDER` | auto-detect | `"openai"` or `"anthropic"` (used when both keys are set) |
| `MODEL_NAME` | `gpt-5.2-codex` / `claude-sonnet-4-5-20250929` | Model to use (default depends on provider) |
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

System prompts (`SysPrompt.md` and role-specific variants) are compiled into the binary at build time, so `intendant` works from any directory without needing the source tree. Prompts are resolved using a 3-layer cascade (highest priority first):

1. **Project root** — `<git-root>/SysPrompt.md` (per-project customization)
2. **Global config** — `~/.config/intendant/SysPrompt.md` (user-wide customization)
3. **Compiled-in default** — always available, zero-config

Role-specific prompts (`SysPrompt_orchestrator.md`, `SysPrompt_research.md`, `SysPrompt_implementation.md`) follow the same cascade and are appended to the base prompt.

To customize prompts for a specific project, place your modified `.md` files in the project's git root. For user-wide customization, place them in `~/.config/intendant/`.

## TUI

`intendant` includes a ratatui-based terminal UI that launches automatically when stdin is a terminal. The TUI provides real-time monitoring and control of the agent loop.

### Layout

```
┌─────────────────────────────────────────────┐
│ StatusBar: provider │ model │ turn │ budget  │  1 line
├─────────────────────────────────────────────┤
│ ActionPanel: phase + spinner + key hints    │  1-3 lines
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

When the TUI is active, a Unix domain socket is created at `/tmp/intendant-<pid>.sock`.

Current status:
- Outbound event broadcast is implemented.
- Inbound command transport is implemented, but command handling is currently minimal (messages are surfaced to the TUI event stream; full remote-control actions are still being expanded).
- `--control-socket` is currently a reserved headless flag.

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
