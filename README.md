# Intendant

A Rust runtime that executes commands on behalf of an AI agent, plus an AI integration layer that drives the runtime via the OpenAI, Anthropic, or Gemini API. The runtime manages process lifecycles with in-memory state, executes commands sequentially (blocking until completion), and persists structured session logs. The CLI features native API tool calling (function calling) with automatic fallback to text-based JSON extraction, streaming token output with real-time deltas, a ratatui-based TUI for real-time monitoring and control, a configurable autonomy system with per-action approval, MCP client support for connecting to external tool servers, Landlock filesystem sandboxing, prompt caching (Anthropic), auto-compaction at 90% context usage, JSONL structured output mode, INTENDANT.md project instructions, and supports hierarchical multi-agent orchestration with token budget awareness, sub-agent spawning, git worktree isolation, and a tagged knowledge system with pub/sub channels.

## Architecture

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
  +--> Token budget tracking (context-window-aware loop termination)
  +--> Sub-agent spawning via env vars (INTENDANT_ROLE, INTENDANT_ID, etc.)
  +--> Git worktree isolation for implementation agents
  +--> Tagged knowledge store with pub/sub channels between agents
```

- **Process State:** In-memory `HashMap<u64, ProcessInfo>` tracking nonce, PID, status, exit code, and timestamp. Ephemeral — does not survive binary restarts.
- **Session Directory (`~/.intendant/logs/<uuid>/`):** Per-session directory with UUID-based naming. Contains per-nonce stdout/stderr log files, structured session logs, conversation history, and askHuman IPC files. The log directory is passed to the runtime via `INTENDANT_LOG_DIR`.
- **Execution Model:** Commands are processed sequentially. Each command blocks until completion and returns its result directly (exit code, stdout tail, stderr tail). The runtime exits after processing all commands.

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

The agent reads a single JSON object from stdin, executes commands sequentially, and writes result lines to stdout.

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/intendant-runtime
```

Output is a JSON result line containing the nonce, exit code, stdout tail (last 10KB), and stderr tail.

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
| `execAsAgent` | Run a bash command (blocks until exit, returns exit code + stdout/stderr tail) | `command`, `display`, `wait_for_port` |
| `captureScreen` | Screenshot a display via ImageMagick | `display` |
| `inspectPath` | Inspect filesystem path metadata | `path` |
| `editFile` | Structured file editing without shell commands | `file_path`, `operation`, `content`, `match_content`, `line_number`, `end_line` |
| `writeFile` | Alias for `editFile` write operation | `file_path`, `content` |
| `browse` | Fetch URL and convert HTML to text | `url` |
| `askHuman` | Ask the operator a question and wait for response | `question`, `timeout_ms` |
| `execPty` | Run command in a persistent PTY session | `command`, `shell_id` |
| `storeMemory` | Store a knowledge entry with optional tags/channel | `memory_key`, `memory_summary`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source` |
| `recallMemory` | Search knowledge by keyword with optional filters | `memory_query`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source`, `memory_since` |

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

- **Agent binary:** models serialization, error types, process state operations, nonce replacement, path inspection, blocking command execution, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters.
- **Caller binary:** JSON extraction, done signal handling, conversation management with message layer protection, tool call tracking, and auto-compaction, context directives (drop/summarize), error types, project detection, config parsing with approval rules and MCP server config and sandbox config, provider selection with token usage tracking, Responses API support, rate-limit retry with exponential backoff, API key masking, SSE streaming and event parsing, shared message builders, structured output and reasoning controls, role mapping, native tool definitions (11+ tools including MCP client tools, provider conversion formats), tool call batch assembly and result routing (including MCP tool routing), Gemini provider request/response format, sub-agent spawning and result parsing, git worktree lifecycle, user mode orchestration, knowledge pub/sub system, prompt resolution cascade (project root, global config, compiled-in defaults, tools-mode variant) with INTENDANT.md loading, TUI rendering (status bar, log panel, action panel, approval panel, help overlay, layout calculations, orchestrator progress, streaming buffer), autonomy level resolution and command classification, event bus dispatch, theme color thresholds, control socket serialization, session log file creation, model summary formatting, Xvfb display configuration per provider, dynamic display allocation, MCP client tool name parsing and routing, Landlock sandbox config construction, and JSON structured output mode.

## Session Logging

Each `intendant` invocation creates a structured session log directory at `~/.intendant/logs/<uuid>/`. The log provides full observability for debugging and post-session analysis.

### Directory Structure

```
~/.intendant/logs/<uuid>/
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

Each invocation creates an isolated session with a UUID-based directory at `~/.intendant/logs/<uuid>/`. No global state files are used.

- **Process state** is in-memory (ephemeral, per-invocation).
- **Session logs** persist in the session directory.
- **Conversation history** is saved to `conversation.jsonl` for resume support.

Sessions can be resumed:
```bash
./target/release/intendant --continue "fix that bug"     # Resume most recent session for this project
./target/release/intendant --resume abc123 "continue"     # Resume specific session by ID or prefix
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
| `--control-socket` | Enable Unix control socket (TUI and MCP modes) |
| `--vision` | Launch Xvfb virtual display (auto-allocated `:99+`) sized for the provider's vision model |
| `--json` | JSONL structured output to stdout (implies `--no-tui`) |
| `--sandbox` | Enable Landlock filesystem sandboxing (Linux kernel 5.13+) |

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

- In **TUI mode**, `askHuman` opens the input panel and writes your answer to the session-scoped response file.
- Empty submit is rejected in the TUI; you must provide non-empty input or press `Esc` to cancel.
- In **headless mode** (`--no-tui` or non-interactive stdin), `askHuman` cannot be answered interactively. The loop now tells the model to continue with explicit assumptions instead of waiting for the runtime timeout.
- Runtime-level timeout for unanswered `askHuman` remains `5 minutes`.

### How it works

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

## Environment

- **OS:** Debian 12+
- **Runtime:** Tokio async
- **Display:** DISPLAY is set from the environment (configurable via `display` field per command, defaults to env `DISPLAY` or first discovered display from `/tmp/.X*-lock`, then `:1`). With `--vision`, Xvfb is auto-allocated on a free display (`:99+`). At startup the runtime discovers active X displays and merges their xauth cookies (from `~/.Xauthority` and `/var/run/lightdm/root/:N` via `sudo -n`) into a session-scoped `session.Xauthority` file, which is passed as `XAUTHORITY` to all spawned commands.
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
| `INTENDANT_LOG_DIR` | auto | Session log directory (set automatically by caller for the runtime) |

The agent runner hard timeout is 120s default, automatically extended to 600s when `askHuman` is present in the command batch.

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

[sandbox]
enabled = false               # enable Landlock filesystem sandboxing (default: false)
extra_write_paths = ["/var/log"]  # additional writable paths beyond project root, /tmp, log dir

# External MCP servers to connect to as a client
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

When sandboxing is enabled (via `--sandbox` or `[sandbox].enabled = true`), runtime command execution is restricted to read-only filesystem access plus writes to project root, `/tmp`, session log directory, `~/.intendant`, and `extra_write_paths`.

### INTENDANT.md Project Instructions

Place an `INTENDANT.md` file in your project root or at `~/.config/intendant/INTENDANT.md` for global instructions. These are injected into the conversation at session start, before knowledge/memory. Both files are loaded if present (global first, project-local second).

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

Action categories are determined by analyzing command JSON: shell commands are classified by inspecting for destructive patterns (`rm`, `kill`, `dd`, `mkfs`, `sudo`), network operations (`curl`, `wget`, `ssh`), file operations, etc.

## Control Socket

When `--control-socket` is enabled, a Unix domain socket is created at `/tmp/intendant-<pid>.sock`.

Current status:
- Outbound event broadcast is implemented.
- Inbound command handling is implemented for status, approval, denial, human input, autonomy change, quit, and controller-restart workflow commands (in MCP mode).
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
| `start_task` | Start a new agent task | `task` |
| `schedule_controller_restart` | Schedule a controller restart/autonomous re-init workflow | `controller_id`, `north_star_goal`, `reason?`, `restart_after?` (`"turn_end"` or `"now"`), `restart_command?` (non-empty), `auto_start_task?` (default `false`), `max_attempts?`, `cooldown_sec?`; requires at least one restart action (`restart_command` and/or `auto_start_task=true`) |
| `controller_turn_complete` | Final handshake from controller; validates token and executes scheduled restart | `restart_id`, `turn_complete_token`, `status?`, `handoff_summary?` |
| `get_restart_status` | Get current controller restart state (or null) | — |
| `cancel_controller_restart` | Cancel scheduled restart | `restart_id?` |
| `reload` | Rebuild binary and hot-reload the MCP server via exec() | — |

`schedule_controller_restart`, `controller_turn_complete`, and `cancel_controller_restart` return JSON payloads with an `ok` boolean and status fields. Rejections are returned as JSON (`ok: false`) with an `error` message instead of plain text.

### Hot Reload

The `reload` tool rebuilds the binary from source (`cargo build --release`) and replaces the running MCP server process in-place using `exec()`. The MCP connection survives seamlessly — no Claude Code restart needed.

How it works:
1. `reload` runs `cargo build --release` in the project directory
2. After sending the tool response, the process calls `exec()` to replace itself with the new binary
3. The new process detects `INTENDANT_MCP_RELOAD=1` and uses a `ReloadTransport` that injects a synthetic MCP initialization handshake
4. Claude Code continues using the same connection — the stdio file descriptors survive `exec()`

This is particularly useful during development: edit code, call `reload`, and the MCP server picks up all changes without losing the connection.

### Resources

Resources provide push-based state observation via subscriptions. The server sends `notifications/resources/updated` when state changes, so clients know to re-fetch.

| URI | Description |
|-----|-------------|
| `intendant://status` | Provider, model, turn count, budget %, phase, autonomy level |
| `intendant://logs` | Last 100 chronological log entries (same as TUI log panel) |
| `intendant://pending-approval` | Current pending approval request, if any |
| `intendant://pending-input` | Current pending human question, if any |
| `intendant://controller-restart` | Current controller restart workflow state, if any |

### Controller Restart Workflow

Use this when you want Intendant to trigger a controller re-init cycle safely.

1. Call `schedule_controller_restart` and capture `restart_id` + `turn_complete_token`.
2. Before ending the controlling agent turn, call `controller_turn_complete` with both values.
3. Intendant executes restart actions:
   - spawn `restart_command` (if provided), and/or
   - start a fresh Intendant task using `north_star_goal` (`auto_start_task=false` by default; opt in for E2E testing).
4. Inspect state via `get_restart_status` or `intendant://controller-restart`.

Notes:
- Restart state is persisted to the current session dir as `controller_restart.json`.
- `restart_after` defaults to `"turn_end"`.
- `restart_after` accepts only `"turn_end"` or `"now"`; other values are rejected.
- Restart workflow string inputs are normalized (trimmed) before validation/execution.
- `restart_command`, when provided, must not be empty/whitespace.
- At least one restart action is required at schedule time: set `restart_command` and/or `auto_start_task=true`.
- `max_attempts` must be `>= 1`; `0` is rejected.
- Optional `status`, `handoff_summary`, and cancel `restart_id` guard treat whitespace-only values as unset.
- If `restart_after="now"` and execution fails after passing validation, `schedule_controller_restart` reports `"ok": false` and includes `execution_error`.
- `schedule_controller_restart` rejection payloads use `"status": "rejected"` and include `"error"` (plus `"restart_id"`/`"phase"` when a conflicting active restart exists).
- `controller_turn_complete` reports JSON results:
  - success: `"status": "completed"`, `"ok": true`, plus `"execution"` and `"phase"`.
  - rejection/pending: `"ok": false`, with `"status"` (`"rejected"` or `"restart_pending"`) and `"error"`.
- `cancel_controller_restart` reports JSON results:
  - success: `"status": "cancelled"`, `"ok": true`, plus `"restart_id"` and `"phase": "cancelled"`.
  - rejection: `"status": "rejected"`, `"ok": false`, with `"error"` (and optional `"restart_id"`/`"phase"` context).

Controller recursion profile (recommended for Codex/Claude-style controllers):
- Set `auto_start_task=false` (or omit it, since `false` is the default).
- Use `restart_command` to relaunch the external controller process.
- Treat `start_task` as optional E2E testing only, not the default recursion path.

Controller loop monitoring files (for `restart_command` scripts):
- Write run artifacts under `.intendant/controller-loop/<run_id>/`.
- Maintain stable pointers:
  - `.intendant/controller-loop/latest` (symlink to current/latest run)
  - `.intendant/controller-loop/latest.pid` (wrapper script PID)
  - `.intendant/controller-loop/latest.status.json` (latest status snapshot)
  - `.intendant/controller-loop/latest.jsonl` (path to latest JSONL output file)
- Recommended commands:
  - `tail -f .intendant/controller-loop/latest/codex.jsonl`
  - `watch -n 2 'cat .intendant/controller-loop/latest/heartbeat.txt'`
  - `cat .intendant/controller-loop/latest.status.json`
- Intervention controls:
  - Graceful stop current run: `touch .intendant/controller-loop/request_stop`
  - Immediate abort current run: `touch .intendant/controller-loop/request_abort`
  - Intervention history: `cat .intendant/controller-loop/latest/intervention.log`

### Typical Agent Workflow

1. Call `get_status` to see the current phase and budget
2. Poll `get_logs` with `since_id` to stream new events
3. When an approval is needed, `get_pending_approval` returns the command preview — call `approve`, `deny`, or `skip`
4. When `askHuman` triggers, `get_pending_input` returns the question — call `respond` with your answer
5. Call `quit` when done
