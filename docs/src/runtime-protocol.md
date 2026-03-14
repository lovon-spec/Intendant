# Runtime Protocol

The `intendant-runtime` binary reads a single JSON object from stdin, executes commands sequentially, and writes result lines to stdout.

## Basic Usage

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

## Functions

### Runtime Functions

| Function | Description | Key Fields |
|----------|-------------|------------|
| `execAsAgent` | Run a bash command (blocks until exit, returns exit code + stdout/stderr tail) | `command`, `display`, `wait_for_port` |
| `captureScreen` | Screenshot a display via ImageMagick | `display` |
| `inspectPath` | Inspect filesystem path metadata (type, size, perms, timestamps) | `path` |
| `editFile` | Structured file editing without shell commands | `file_path`, `operation`, `content`, `match_content`, `line_number`, `end_line` |
| `writeFile` | Alias for `editFile` with `operation: "write"` (backward compatibility) | `file_path`, `content` |
| `browse` | Fetch URL and convert HTML to plain text (50KB max) | `url` |
| `askHuman` | Ask the operator a question and wait for response (5-minute timeout) | `question`, `timeout_ms` |
| `execPty` | Run command in a persistent PTY session (`bash --norc --noprofile`) | `command`, `shell_id` |
| `storeMemory` | Store a knowledge entry with optional tags/channel | `memory_key`, `memory_summary`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source` |
| `recallMemory` | Search knowledge by keyword with optional filters | `memory_query`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source`, `memory_since` |

### Caller-Handled Functions

These are intercepted by the caller and never reach the runtime:

| Function | Description |
|----------|-------------|
| `manage_context` | Apply context directives (drop/summarize turns) to the conversation |
| `signal_done` | Signal task completion to the caller loop |

### Native Tool Names

When using native tool calling (the default), tool names use snake_case:

| Native Name | Runtime Function |
|------------|-----------------|
| `exec_command` | `execAsAgent` |
| `capture_screen` | `captureScreen` |
| `inspect_path` | `inspectPath` |
| `edit_file` | `editFile` |
| `browse_url` | `browse` |
| `ask_human` | `askHuman` |
| `exec_pty` | `execPty` |
| `store_memory` | `storeMemory` |
| `recall_memory` | `recallMemory` |
| `manage_context` | (caller-handled) |
| `signal_done` | (caller-handled) |

### editFile Operations

The `editFile` function supports 5 operations:

| Operation | Description | Required Fields |
|-----------|-------------|-----------------|
| `write` | Write content to file (creates or overwrites) | `file_path`, `content` |
| `append` | Append content to end of file | `file_path`, `content` |
| `replace` | Replace matching text with new content | `file_path`, `match_content`, `content` |
| `insert_at` | Insert content at a specific line number | `file_path`, `line_number`, `content` |
| `replace_lines` | Replace a range of lines | `file_path`, `line_number`, `end_line`, `content` |

## Nonce Variables

Use `$NONCE[id]` in command strings to reference the PID of a previously launched nonce. For example, `kill -9 $NONCE[10]` kills the process started by nonce 10. Handled by regex-based substitution in `replace_nonce_refs()`.

## Context Management

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
- **`recallMemory`**: Searches entries by keyword with optional filters (tags, channel, source, since timestamp). Results are ranked by relevance (key/summary match).
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

## JSON Output Mode

`--json` enables JSONL structured output to stdout (implies `--no-tui`). Each line is a JSON object with `type` and `data` fields. Event types include: `turn_started`, `model_response`, `model_response_delta`, `agent_output`, `done`, `error`, `approval_required`, `human_question`, `budget_warning`, `round_complete`, `context_management`.

In JSON mode, stdin accepts both plain text (follow-up messages) and JSON commands using the same `ControlMsg` format as the Unix control socket:

```json
{"action":"approve","id":123}
{"action":"deny","id":123}
{"action":"skip","id":123}
{"action":"approve_all","id":123}
{"action":"input","text":"answer to askHuman"}
{"action":"follow_up","text":"continue with this"}
```

Lines not starting with `{` or not parseable as `ControlMsg` are treated as follow-up text. This makes `--json` mode fully interactive: approval flows, askHuman, and multi-round conversations all work without a TUI or control socket.
