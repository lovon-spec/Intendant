# Agent

A Rust runtime that executes commands on behalf of an AI agent, plus an AI caller that drives the agent via the OpenAI API. The runtime manages process lifecycles via shared memory (SHM), streams status updates, and persists logs across binary restarts.

## Architecture

```
stdin (JSON) --> Agent --> spawns bash commands
                  |
                  +--> /dev/shm/agent_processes  (process state, survives restarts)
                  +--> /dev/shm/agent_session     (log directory path, survives restarts)
                  +--> /var/log/agent/<timestamp>/ (stdout/stderr logs per nonce)
                  |
                  +--> StatusMonitor --> stdout (status lines)
```

- **Shared Memory (`/dev/shm/agent_processes`):** Fixed-size array of `ProcessInfo` structs (1024 slots). Each slot stores nonce, PID, status, exit code, and timestamp. Survives binary restarts since it lives on tmpfs.
- **Session File (`/dev/shm/agent_session`):** Stores the log directory path so consecutive runs reuse the same directory.
- **Log Directory (`/var/log/agent/<timestamp>/`):** Per-nonce stdout and stderr log files. Created once per session.
- **Status Monitor:** Background task that polls SHM for status changes and writes update lines to stdout.

## Building

```bash
cargo build --release
```

Two binaries are produced:
- `./target/release/agent` — the command runtime
- `./target/release/caller` — the AI caller

## Usage

The agent reads a single JSON object from stdin and writes status lines to stdout.

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/agent
```

Output:

```
1r0        # nonce 1, running, exit code 0
1c0        # nonce 1, completed, exit code 0
```

Retrieve results in a subsequent run:

```bash
echo '{"commands":[{"function":"fetchStatus","nonce":1,"status_type":"stdout"}]}' \
  | ./target/release/agent
```

Inspect a file path:

```bash
echo '{"commands":[{"function":"inspectPath","nonce":1,"path":"/etc/hosts"}]}' \
  | ./target/release/agent
```

## Protocol

### Functions

| Function | Description | Key Fields |
|----------|-------------|------------|
| `execAsAgent` | Run a bash command in the background | `command`, `display`, `depending_nonce`, `wait`, `expected_status` |
| `captureScreen` | Screenshot a display via ImageMagick | `display` |
| `fetchStatus` | Read process state/logs | `status_type` (`status`, `stdout`, `stderr`, `exit_code`) |
| `inspectPath` | Inspect filesystem path metadata | `path` |

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

## Session Management

State persists across binary restarts via `/dev/shm/`:

- **Process state** is stored in `/dev/shm/agent_processes` — the process map is rebuilt from SHM on each startup.
- **Log directory** is stored in `/dev/shm/agent_session` — subsequent runs reuse the same log directory.

To reset all state (start a fresh session):

```bash
rm -f /dev/shm/agent_processes /dev/shm/agent_session
```

## AI Caller

The caller binary reads a task, sends it to an OpenAI model alongside the system prompt (`SysPrompt.md`), and feeds the model's JSON output to the agent binary in a loop.

### Setup

Create a `.env` file (or export the variables):

```bash
OPENAI_API_KEY=sk-...    # or OPENAI=sk-...
MODEL_NAME=gpt-4o        # optional, defaults to gpt-4o
```

### Running

```bash
# With a task as CLI argument
./target/release/caller "List the files in /tmp"

# Interactive mode (prompts for task on stdin)
./target/release/caller
```

### How it works

1. Loads `.env` and reads `SysPrompt.md` as the system message
2. Sends the task to the OpenAI chat completions API
3. Extracts JSON from the model's response (handles code fences and bare JSON)
4. Pipes the JSON to the agent binary, reads stdout/stderr with idle timeout (3s) and hard timeout (30s)
5. Feeds the agent output back as the next user message
6. Repeats until the model responds with no JSON (task complete) or 50 turns are reached

## Environment

- **OS:** Debian 12+
- **Runtime:** Tokio async
- **Display:** DISPLAY is automatically set to `:1` (configurable via `display` field) for GUI commands
- **Permissions:** Runs as root with full system access
