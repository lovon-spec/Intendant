# MCP Server

The `--mcp` flag launches Intendant as a [Model Context Protocol](https://modelcontextprotocol.io/) server on stdio. This lets external AI agents (Claude Code, Codex, etc.) observe and control Intendant with full parity to the TUI — every action a human can take in the TUI is available as an MCP tool. The server also supports connecting to external MCP servers as a client (see [MCP Client](#mcp-client) below).

## Running

```bash
# Launch as MCP server (stdio transport)
./target/release/intendant --mcp "Deploy the application"

# With provider/model overrides
./target/release/intendant --mcp --provider anthropic --model claude-sonnet-4-5-20250929 "Fix the tests"

# With autonomy preset
./target/release/intendant --mcp --autonomy high "Refactor the auth module"
```

## Client Configuration

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

## Tools

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
| `schedule_controller_restart` | Schedule a controller restart/autonomous re-init workflow | `controller_id`, `north_star_goal`, `reason?`, `restart_after?`, `restart_command?`, `auto_start_task?`, `max_attempts?`, `cooldown_sec?` |
| `controller_turn_complete` | Final handshake from controller; validates token and executes scheduled restart | `restart_id`, `turn_complete_token`, `status?`, `handoff_summary?` |
| `get_restart_status` | Get current controller restart state (or null) | — |
| `cancel_controller_restart` | Cancel scheduled restart | `restart_id?` |
| `request_controller_loop_halt` | Request loop halt | `persistent?` |
| `clear_controller_loop_halt` | Clear loop halt flags so restarts can proceed again | — |
| `intervene_controller_loop` | Request intervention for active loop process | `mode`: `"stop"` or `"abort"` |
| `get_controller_loop_status` | Unified loop health snapshot | — |
| `reload` | Rebuild binary and hot-reload the MCP server via exec() | — |

`schedule_controller_restart`, `controller_turn_complete`, and `cancel_controller_restart` return JSON payloads with an `ok` boolean and status fields. Rejections are returned as JSON (`ok: false`) with an `error` message instead of plain text.

## Hot Reload

The `reload` tool rebuilds the binary from source (`cargo build --release`) and replaces the running MCP server process in-place using `exec()`. The MCP connection survives seamlessly — no Claude Code restart needed.

How it works:
1. `reload` runs `cargo build --release` in the project directory
2. After sending the tool response, the process calls `exec()` to replace itself with the new binary
3. The new process detects `INTENDANT_MCP_RELOAD=1` and uses a `ReloadTransport` that injects a synthetic MCP initialization handshake
4. Claude Code continues using the same connection — the stdio file descriptors survive `exec()`

This is particularly useful during development: edit code, call `reload`, and the MCP server picks up all changes without losing the connection.

## Resources

Resources provide push-based state observation via subscriptions. The server sends `notifications/resources/updated` when state changes, so clients know to re-fetch.

| URI | Description |
|-----|-------------|
| `intendant://status` | Provider, model, turn count, budget %, phase, autonomy, session ID, task |
| `intendant://usage` | Per-model token usage: tokens used, context window, usage % (main + optional presence) |
| `intendant://logs` | Last 100 chronological log entries (same as TUI log panel) |
| `intendant://pending-approval` | Current pending approval request, if any |
| `intendant://pending-input` | Current pending human question, if any |
| `intendant://controller-restart` | Current controller restart workflow state, if any |
| `intendant://controller-loop` | Loop health snapshot (intervention flags, singleton lock owner, active wrapper/codex PIDs, latest run pointers) |

## Controller Restart Workflow

Use this when you want Intendant to trigger a controller re-init cycle safely.

1. Call `schedule_controller_restart` and capture `restart_id` + `turn_complete_token`.
2. Before ending the controlling agent turn, call `controller_turn_complete` with both values.
3. Intendant executes restart actions:
   - spawn `restart_command` (if provided), and/or
   - start a fresh Intendant task using `north_star_goal` (`auto_start_task=false` by default; opt in for E2E testing).
4. Inspect state via `get_restart_status` or `intendant://controller-restart`.

### Notes

- Restart state is persisted to the current session dir as `controller_restart.json`.
- `restart_after` defaults to `"turn_end"`.
- `restart_after` accepts only `"turn_end"` or `"now"`; other values are rejected.
- Restart workflow string inputs are normalized (trimmed) before validation/execution.
- `restart_command`, when provided, must not be empty/whitespace.
- At least one restart action is required at schedule time: set `restart_command` and/or `auto_start_task=true`.
- `max_attempts` must be `>= 1`; `0` is rejected.
- Optional `status`, `handoff_summary`, and cancel `restart_id` guard treat whitespace-only values as unset.
- If `restart_after="now"` and execution fails after passing validation, `schedule_controller_restart` reports `"ok": false` and includes `execution_error`.
- `schedule_controller_restart` always reports `"phase"` from persisted restart state; for `restart_after="now"` this reflects the post-execution phase (`"completed"` or `"failed"`).
- Any restart execution failure (including `auto_start_task` launch errors) updates persisted restart state to `"phase": "failed"` and populates `last_error`.
- `schedule_controller_restart` rejection payloads use `"status": "rejected"` and include `"error"` (plus `"restart_id"`/`"phase"` when a conflicting active restart exists).
- `controller_turn_complete` reports JSON results:
  - success: `"status": "completed"`, `"ok": true`, plus `"execution"` and `"phase"`.
  - rejection/pending: `"ok": false`, with `"status"` (`"rejected"` or `"restart_pending"`) and `"error"`.
- `controller_turn_complete` only accepts restarts in `"awaiting_turn_complete"`; duplicate or late handshakes (for example `"phase": "ready"`) are rejected to prevent duplicate restart execution.
- `cancel_controller_restart` reports JSON results:
  - success: `"status": "cancelled"`, `"ok": true`, plus `"restart_id"` and `"phase": "cancelled"`.
  - rejection: `"status": "rejected"`, `"ok": false`, with `"error"` (and optional `"restart_id"`/`"phase"` context).
- `request_controller_loop_halt`, `clear_controller_loop_halt`, `intervene_controller_loop`, and `get_controller_loop_status` return/emit normalized loop health data (flags, lock owner PID/aliveness, latest run pointers, and active PID counts).
- Control-socket `command_result.data` mirrors structured payloads for restart actions and loop-control actions.
- `get_restart_status` and `intendant://controller-restart` redact `turn_complete_token` as `"[redacted]"`; only `schedule_controller_restart` returns the raw token for the final handshake call.

### Controller Recursion Profile

Recommended for Codex/Claude-style controllers:
- Set `auto_start_task=false` (or omit it, since `false` is the default).
- Use `restart_command` to relaunch the external controller process.
- Treat `start_task` as optional E2E testing only, not the default recursion path.

## Controller Loop Monitoring

Controller loop monitoring files (for `restart_command` scripts):
- Write run artifacts under `.intendant/controller-loop/<run_id>/`.
- Maintain stable pointers:
  - `.intendant/controller-loop/latest` (symlink to current/latest run)
  - `.intendant/controller-loop/latest.pid` (wrapper script PID)
  - `.intendant/controller-loop/latest.status.json` (latest status snapshot)
  - `.intendant/controller-loop/latest.jsonl` (path to latest JSONL output file)
  - `.intendant/controller-loop/active.lock/` (singleton lock: `pid`, `run_id`, `acquired_at`)
- Recommended commands:
  - `tail -f .intendant/controller-loop/latest/codex.jsonl`
  - `watch -n 2 'cat .intendant/controller-loop/latest/heartbeat.txt'`
  - `cat .intendant/controller-loop/latest.status.json`
- Intervention controls:
  - Halt future loop cycles (persistent): `touch .intendant/controller-loop/request_halt`
  - Halt future loop cycles (legacy marker, consumed once): `touch .intendant/controller-loop/request_halt_after_cycle`
  - Graceful stop current run: `touch .intendant/controller-loop/request_stop`
  - Immediate abort current run: `touch .intendant/controller-loop/request_abort`
  - Intervention history: `cat .intendant/controller-loop/latest/intervention.log`
- Per-run PID files:
  - `.intendant/controller-loop/<run_id>/wrapper.pid`
  - `.intendant/controller-loop/<run_id>/codex.pid`

## Typical Agent Workflow

1. Call `get_status` to see the current phase and budget
2. Poll `get_logs` with `since_id` to stream new events
3. When an approval is needed, `get_pending_approval` returns the command preview — call `approve`, `deny`, or `skip`
4. When `askHuman` triggers, `get_pending_input` returns the question — call `respond` with your answer
5. Call `quit` when done

## MCP Client

Intendant can also act as an MCP **client**, connecting to external MCP servers configured in `intendant.toml`. This lets agents use tools from external servers (filesystem, GitHub, databases, etc.) alongside Intendant's native tools.

### Configuration

```toml
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

### How It Works

At startup, `McpClientManager` connects to all configured servers via child process transport, discovers their tools, and registers them with the `mcp__<server>_<tool>` naming convention. For example, a `filesystem` server's `read_file` tool becomes `mcp__filesystem_read_file`.

Tool calls with the `mcp__` prefix are routed through the MCP client manager to the appropriate server. If a server fails to connect at startup, it is skipped with a warning — other servers and native tools continue to work.
