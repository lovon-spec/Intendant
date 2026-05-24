# Runtime Protocol

`intendant-runtime` is the sandboxed half of the two-process split. It reads a
**single JSON object** from stdin, executes its commands **sequentially**, writes
one result line per command to stdout, and exits. It never holds API keys and
never talks to a model — it is a dumb, auditable command executor that the
controller (`intendant`) drives over pipes.

```
                 stdin: one AgentInput JSON
 intendant ───────────────────────────────────► intendant-runtime
 (controller, holds keys)                        (sandboxed executor)
           ◄───────────────────────────────────
                 stdout: one JSON result line per command
```

The controller side of this boundary is `agent_runner.rs::run_agent()`: it locates
`intendant-runtime` next to its own binary, spawns it with stdin/stdout/stderr
piped, writes the JSON, closes stdin, and reads the bounded output back. The
runtime is short-lived — one invocation per batch of tool calls. PTY sessions are
the one stateful exception (see [`execPty`](#execpty)) and they live only for the
duration of a single runtime process.

## Basic Usage

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/intendant-runtime
```

Each command produces one stdout line wrapped as:

```json
{"type":"result","nonce":1,"data":"<stringified per-command JSON>"}
```

`data` is itself a JSON string — the per-command result (exit code, output tail,
etc.) serialized into a string. The controller parses the outer envelope, matches
on `nonce`, then parses `data`.

More examples:

```bash
# Inspect a path
echo '{"commands":[{"function":"inspectPath","nonce":1,"path":"/etc/hosts"}]}' | ./target/release/intendant-runtime

# Edit a file (structured, no shell)
echo '{"commands":[{"function":"editFile","nonce":1,"file_path":"/tmp/test.txt","operation":"write","content":"hello"}]}' | ./target/release/intendant-runtime

# Fetch a web page as text
echo '{"commands":[{"function":"browse","nonce":1,"url":"https://example.com"}]}' | ./target/release/intendant-runtime

# Stateful commands in one persistent PTY (same process only)
echo '{"commands":[{"function":"execPty","nonce":1,"command":"cd /tmp"},{"function":"execPty","nonce":2,"command":"pwd"}]}' | ./target/release/intendant-runtime
```

## The `AgentInput` Shape

The entire stdin payload deserializes into `AgentInput` (`src/models.rs`), which
has exactly one field:

```jsonc
{
  "commands": [ /* array of Command objects, executed in order */ ]
}
```

There is no top-level `context` field at the runtime layer — conversation/context
management is handled entirely on the controller side (see
[Caller-Handled Functions](#caller-handled-functions)). stdin is bounded to 64 MB
(`MAX_INPUT_BYTES`); a parse failure prints the offending input to stderr and
exits non-zero.

### The `Command` object

Every command shares one flat struct (`models::Command`). Only `function` and
`nonce` are always required; the rest are `Option`al and interpreted per
function:

| Field | Type | Used by |
|-------|------|---------|
| `function` | string | all — selects the handler |
| `nonce` | u64 | all — correlation id, echoed in the result |
| `command` | string | `execAsAgent`, `execPty` |
| `display` | i32 | `execAsAgent`, `captureScreen` |
| `timeout_ms` | u64 | `execAsAgent` (default 120000) |
| `wait_for_port` | u16 | `execAsAgent` (0 = no wait) |
| `path` | string | `inspectPath` |
| `file_path` | string | `editFile` / `writeFile` |
| `operation` | string | `editFile` (`write`/`append`/`replace`/`insert_at`/`replace_lines`) |
| `content` | string | `editFile` write/append/replace/insert/replace_lines |
| `match_content` | string | `editFile` `replace` |
| `line_number` | usize | `editFile` `insert_at` / `replace_lines` |
| `end_line` | usize | `editFile` `replace_lines` |
| `url` | string | `browse` |
| `question` | string | `askHuman` |
| `shell_id` | string | `execPty` (defaults to `default`) |
| `memory_key`, `memory_summary`, `memory_query`, `memory_file` | string | `storeMemory` / `recallMemory` |
| `memory_tags`, `memory_channel`, `memory_source` | string | tagged knowledge |
| `memory_since` | u64 | `recallMemory` time filter |

## Sequential, Blocking Execution

`Agent::process_input()` (`src/agent.rs`) iterates `commands` **in order**, and
each command **blocks until it finishes** before the next starts. There is no
concurrency within a batch. `execAsAgent` waits for the child process to exit (or
its timeout); `askHuman` polls indefinitely for a human response. This determinism
is deliberate — the controller can reason about ordering and the human-oversight
layer can gate each action.

A handler error for the filesystem/knowledge functions is captured and returned as
`data: "Error: <message>"` for that nonce rather than aborting the whole batch.
An **unknown `function`**, however, aborts the run with a hard error.

## Functions

### Runtime Functions

These are the ~10 functions `intendant-runtime` actually implements:

| Function | Description | Key fields |
|----------|-------------|------------|
| `execAsAgent` | Run a command via the platform shell (`bash -c` on Unix, `cmd /C` on Windows); blocks until exit; returns pid, exit code, and 10 KB tails of stdout/stderr | `command`, `display`, `timeout_ms`, `wait_for_port` |
| `captureScreen` | Screenshot a display (macOS `screencapture`; X11 `import`) to a PNG in the log dir | `display` |
| `inspectPath` | Filesystem metadata (type, size, mtime; plus mode/uid/gid on Unix) | `path` |
| `editFile` | Structured file editing without a shell | `file_path`, `operation`, `content`, `match_content`, `line_number`, `end_line` |
| `writeFile` | Back-compat alias — rewritten to `editFile` with `operation:"write"` if unset | `file_path`, `content` |
| `browse` | HTTP GET, HTML→text via `html2text` (50 KB cap, 15 s timeout, ≤5 redirects) | `url` |
| `askHuman` | Write a question to the log dir and **poll indefinitely** for a response file | `question` |
| `execPty` | Run a command in a persistent PTY session for the life of this process | `command`, `shell_id` |
| `storeMemory` | Store/update a knowledge entry (legacy or tagged format) | `memory_key`, `memory_summary`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source` |
| `recallMemory` | Keyword search with optional filters | `memory_query`, `memory_file`, `memory_tags`, `memory_channel`, `memory_source`, `memory_since` |

Path-taking functions (`inspectPath`, `editFile`) run through `validate_path()`,
which blocks `..` traversal and a fixed set of sensitive locations
(`/etc/shadow`, `/etc/gshadow`, `/proc`, `/sys`, `/dev`, and any `.ssh` / `.gnupg`
component), checking both the raw and canonicalized forms.

### `editFile` Operations

| Operation | Description | Required fields |
|-----------|-------------|-----------------|
| `write` | Create or overwrite the file (creates parent dirs) | `file_path`, `content` |
| `append` | Append to the end of the file | `file_path`, `content` |
| `replace` | Replace **every** occurrence of `match_content`; reports `replacements` count (fails gracefully if not found) | `file_path`, `match_content`, `content` |
| `insert_at` | Insert `content` as a line at `line_number` (clamped to file length) | `file_path`, `line_number`, `content` |
| `replace_lines` | Replace lines `[line_number, end_line)` with `content` | `file_path`, `line_number`, `end_line`, `content` |

`insert_at` and `replace_lines` preserve a trailing newline when the original had
one. `replace_lines` errors if `end_line < line_number`.

### `execAsAgent` details

- **Shell**: `crate::utils::agent_shell_command()` picks `bash -c <cmd>` on Unix
  and `cmd.exe /C <cmd>` on Windows; the whole command is one argument so the
  shell does word-splitting. Exit semantics are identical across platforms.
- **stdout/stderr** are streamed to `<log_dir>/<nonce>_stdout.log` /
  `_stderr.log`; the result carries only the **last 10 KB** of each
  (`LOG_TAIL_BYTES`).
- **Keys are stripped**: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`
  (and the bare `OPENAI`/`ANTHROPIC`/`GEMINI` names) are removed from the child's
  environment, so commands the agent runs can never read provider credentials.
- **Display gating**: `DISPLAY` is set to the chosen display. Access to the user's
  session display (`:0` or below) is refused unless
  `INTENDANT_USER_DISPLAY_GRANTED` is set; otherwise a virtual display is used.
- **Exit codes**: real exit code on completion; `-3` on timeout (process killed),
  `-2` on `wait_for_port` timeout, `-1` on spawn/wait failure.

### `execPty`

Lazily opens a PTY (`bash --norc --noprofile` on Unix; PowerShell with a cmd.exe
fallback on Windows) keyed by `shell_id`, so state (cwd, shell vars) persists
**across commands within the same runtime invocation**. Output is bracketed with
`__PTY_START_<nonce>__` / `__PTY_END_<nonce>__` markers, drained by a background
reader thread, ANSI-stripped, and trimmed of the echoed command and prompt lines.
A 30 s per-command deadline guarantees a quiet shell can't wedge the loop. (The
reader thread also answers ConPTY's startup cursor-position query so Windows
shells don't hang at launch.)

### `askHuman`

Writes the question to `<log_dir>/human_question`, echoes it to stderr, and polls
every 500 ms for `<log_dir>/human_response`, **with no timeout** — it waits as long
as the human is away. The controller correspondingly drops its hard timeout for any
batch containing an `askHuman` (`agent_runner.rs::has_ask_human`). Both files are
removed once a response arrives.

### Caller-Handled Functions

These tool names appear in the controller's tool catalog but are **intercepted by
the controller and never sent to the runtime**. If they ever reached
`process_input()` they would hit the unknown-function error.

| Function | Handled by | Description |
|----------|-----------|-------------|
| `manage_context` | controller loop | Apply context directives (drop/summarize turns) to the conversation |
| `signal_done` | controller loop | Signal task completion to the agent loop |
| `invoke_skill` | controller loop | Run a packaged skill |
| `spawn_live_audio` | controller loop | Start a voice/phone session (untrusted; see [Computer Use & Live Audio](./computer-use-and-audio.md)) |
| `mcp__<server>_<tool>` | MCP client | Tools registered from external MCP servers ([MCP Server](./mcp-server.md)) |

### Native Tool Names

When the provider uses native tool calling (the default), the model sees
snake_case tool names that map onto the runtime functions:

| Native name | Runtime function |
|-------------|------------------|
| `exec_command` | `execAsAgent` |
| `capture_screen` | `captureScreen` |
| `inspect_path` | `inspectPath` |
| `edit_file` | `editFile` |
| `browse_url` | `browse` |
| `ask_human` | `askHuman` |
| `exec_pty` | `execPty` |
| `store_memory` | `storeMemory` |
| `recall_memory` | `recallMemory` |
| `manage_context`, `signal_done`, `invoke_skill`, `spawn_live_audio` | *(caller-handled)* |

## Nonce Variables

Inside `command` strings, `$NONCE[id]` is substituted with the PID of the process
launched by command `id` earlier in the same batch. For example
`kill -9 $NONCE[10]` kills the process started by nonce 10. This is a regex
substitution in `replace_nonce_refs()`, resolved against the runtime's per-process
PID table.

## Logging Directories

The runtime resolves its working/log directory in `resolve_log_dir()`:

1. `INTENDANT_LOG_DIR` if set by the controller (created if missing) — the normal
   case; the controller passes the session log dir here.
2. Otherwise a fresh timestamped dir under `$HOME/.intendant/logs/<YYYYMMDD_HHMMSS>`.

This directory holds per-command stdout/stderr logs, screenshots
(`screenshot_<nonce>.png`), the `askHuman` question/response files, and (on
Linux/X11) the merged `session.Xauthority` cookie file.

## Filesystem Sandboxing (Landlock)

On Linux the runtime applies a Landlock ruleset **before running any command**,
driven entirely by the `INTENDANT_SANDBOX_WRITE_PATHS` environment variable
(`apply_sandbox_from_env` in `src/main.rs`):

- The value is a `:`-separated list of writable paths.
- Empty/unset → no sandbox is applied.
- When set, the whole filesystem is granted **read** access and only the listed
  paths (that exist) get **write** access (ABI v5).
- If the kernel doesn't enforce Landlock, a warning is printed and execution
  continues.

The controller (`agent_runner.rs`) is what populates this variable from its
`SandboxConfig` when `--sandbox` is in effect. On non-Linux platforms the sandbox
call is a no-op (the OS-level confinement differs — see
[Architecture](./architecture.md)). This is the runtime's primary write-boundary;
combined with the key-stripping and path validation above, it bounds what an agent
command can touch even though it runs with the user's privileges.

## Knowledge System

Project knowledge persists as tagged entries across sessions in
`<project>/.intendant/memory.json`. The runtime supports both the **legacy
key-value format** (entries as an object) and the **tagged format** (entries as an
array with `tags`/`channel`/`source`/cursors), auto-detecting which is on disk and
migrating on write when knowledge fields are supplied.

- **`storeMemory`** creates or updates an entry by `(key, source)`. Tags come from
  a comma-separated `memory_tags`; `memory_channel` defaults to `default`;
  `memory_source` defaults to `agent`. Supplying any tag/channel/source field on a
  new file selects the tagged format.
- **`recallMemory`** keyword-searches `key`+`summary`, ranks by match count, and
  applies optional filters: `memory_tags` (any-match), `memory_channel`,
  `memory_source`, and `memory_since` (Unix-seconds lower bound). A filter-only
  query with no keywords returns all matching entries.

These pub/sub channels and cursors are what the orchestrator uses to route
findings between sub-agents — see
[Multi-Agent Orchestration](./multi-agent.md#knowledge-routing-between-agents).
Knowledge can be disabled entirely:

```toml
[memory]
enabled = false   # default: true
```

## JSON Output Mode (controller, not runtime)

`--json` is a **controller** flag (it implies `--no-tui`), not part of the runtime
protocol — but it is the closest thing to a machine-readable interface for the
whole system, so it is documented here. Each stdout line is a JSON object with
`type` and `data`. Event types include `turn_started`, `model_response`,
`model_response_delta`, `agent_output`, `done`, `error`, `approval_required`,
`human_question`, `budget_warning`, `round_complete`, and `context_management`.

In `--json` mode the controller's stdin accepts both plain-text follow-ups and
`ControlMsg` JSON (the same vocabulary as the Unix control socket):

```json
{"action":"approve","id":123}
{"action":"deny","id":123}
{"action":"skip","id":123}
{"action":"approve_all","id":123}
{"action":"input","text":"answer to askHuman"}
{"action":"follow_up","text":"continue with this"}
```

Lines that don't start with `{` or don't parse as a `ControlMsg` are treated as
follow-up text, making `--json` fully interactive — approvals, `askHuman`, and
multi-round conversations all work without a TUI or socket.
