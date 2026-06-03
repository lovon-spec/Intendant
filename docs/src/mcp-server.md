# MCP Server

The `--mcp` flag runs Intendant as a [Model Context Protocol](https://modelcontextprotocol.io/)
server over stdio JSON-RPC (`src/bin/caller/mcp.rs`). It lets an external agent
(Claude Code, Codex, etc.) observe and control Intendant: every action a human
can take in the TUI is exposed as an MCP tool, plus display/CU/frame tools, live
audio, and a controller-orchestration surface.

Architecturally the MCP server is a **peer of the TUI**: it subscribes to the
same `EventBus` and produces the same `UserAction` variants, processed by the
single shared `process_action` handler. Adding a `UserAction` variant forces both
the TUI key handler and the MCP tool handler to implement it (exhaustive `match`,
no wildcards).

> **Parity scope (corrected):** the `UserAction` compile-time contract
> (`frontend.rs`) binds **the TUI and the MCP server only** — its module doc says
> exactly that. The web dashboard and the Unix control socket do *not* go through
> `UserAction`; they dispatch [`ControlMsg`](./integrations.md) values that the
> centralized `control_plane.rs` applies (see [TUI & Autonomy](./tui.md) for why
> frontends are display-only). So "all frontends share one action enum" is *not*
> accurate — there are two dispatch contracts: `UserAction` (TUI/MCP) and
> `ControlMsg` (web/control-socket). `--mcp` is its own run mode and is **not**
> implied by `--web`.

## Running

```bash
# MCP server on stdio
./target/release/intendant --mcp "Deploy the application"

# With provider/model overrides
./target/release/intendant --mcp --provider anthropic --model claude-sonnet-4-6-20250929 "Fix the tests"

# With an autonomy preset
./target/release/intendant --mcp --autonomy high "Refactor the auth module"
```

In MCP mode, stdin/stdout are reserved for JSON-RPC, so the initial task is taken
from the command line (or the server starts idle and accepts `start_task`).

### Client Configuration

Add Intendant to your MCP client config (Claude Code
`~/.claude/claude_desktop_config.json`):

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

The full MCP tool surface (dispatched in `call_tool_by_name`) is broad. For
model clients that front-load tool schemas into every request, prefer the
HTTP transport's `tool_profile=core` query parameter and the `intendant ctl`
CLI for lazy discovery. `tool_profile=core` advertises only status, shared-view
collaboration, and managed-context rewind/backout tools when managed context is
enabled; omitting `tool_profile` keeps the historical full tool list.
With the patched managed Codex binary, `rewind_backout mode="fork"` creates a
new Codex thread while inheriting the lineage prompt-cache key from the saved
rollout; same-thread `restore` remains available when the current thread should
be rewritten in place.

The CLI mirrors the broad surface without loading every schema into model
context:

```bash
"${INTENDANT:-intendant}" ctl --help
"${INTENDANT:-intendant}" ctl tools list
"${INTENDANT:-intendant}" ctl tools schema take_screenshot
"${INTENDANT:-intendant}" ctl display screenshot --target user_session --output screen.png
```

Full MCP tool groups:

### Status & logs (observation)

| Tool                   | Description | Params |
|------------------------|-------------|--------|
| `get_status`           | Provider, model, turn, budget %, phase, autonomy, verbosity, tokens. | — |
| `get_logs`             | Log entries, cursor-paginated, level-filterable. | `since_id?`, `level_filter?`, `limit?` |
| `get_pending_approval` | The current pending approval request (or null). | — |
| `get_pending_input`    | The current pending `askHuman` question (or null). | — |

### Interactive actions (mirror TUI keys → `UserAction`)

| Tool            | Description | Params |
|-----------------|-------------|--------|
| `approve`       | Approve a pending command (TUI `y`). | `id` |
| `deny`          | Deny and stop (TUI `n`). | `id` |
| `skip`          | Skip, continue (TUI `s`). | `id` |
| `approve_all`   | Approve and set autonomy to Full (TUI `a`). | `id` |
| `respond`       | Answer an `askHuman` question (TUI type + Enter). | `text` |
| `set_autonomy`  | Set autonomy (TUI `+`/`-`). | `level`: `low`/`medium`/`high`/`full` |
| `set_verbosity` | Set log verbosity (TUI `v`). | `level`: `quiet`/`normal`/`verbose`/`debug` |
| `start_task`    | Start a new agent task (also used as follow-up when waiting). | `task` |
| `quit`          | Shut down the agent (TUI `q`). | — |

### Display, computer use & frames

| Tool                 | Description | Params |
|----------------------|-------------|--------|
| `list_displays`      | Enumerate displays with their session state. | — |
| `take_display`       | Take control of a display. | `display_id` |
| `release_display`    | Release control of a display. | `display_id`, `note?` |
| `take_screenshot`    | Capture a screenshot (returns image content). | display params |
| `execute_cu_actions` | Run a batch of [computer-use](./computer-use-and-audio.md) actions. | CU action params |
| `list_frames`        | List captured video frames. | filter params |
| `read_frame`         | Read a specific frame. | `frame_id` |

### Browser workspaces

Browser workspaces are addressable browser-control surfaces for agent/human
collaboration and headed UI testing. The first executable backend launches a
managed local Chromium-family browser with an isolated profile and Chrome
DevTools Protocol metadata. On macOS, Intendant does not launch the user's
installed `/Applications/Google Chrome.app` by default; use `provider=system_cdp`
or `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1` to opt into system
Chrome/Chromium, and use `INTENDANT_BROWSER_WORKSPACE_EXECUTABLE` for an
explicit browser binary. Run `intendant setup browsers` to install Chrome for
Testing into Intendant's managed cache. The wire contract already carries
`provider` and `peer_id` fields so Playwright/Agent Browser adapters and
federated peer-hosted browsers can slot in later. Each workspace has a lease,
so concurrent agents must explicitly acquire it and use `force` to take over an
active holder.

| Tool                          | Description | Params |
|-------------------------------|-------------|--------|
| `browser_workspace_providers` | Report available workspace providers. | — |
| `list_browser_workspaces`     | List active browser workspaces and leases. | — |
| `create_browser_workspace`    | Launch/register a workspace. | `url?`, `label?`, `provider?`, `peer_id?`, `owner_session_id?`, `profile_dir?` |
| `acquire_browser_workspace`   | Acquire a workspace lease. | `workspace_id`, `holder_id`, `holder_kind?`, `note?`, `force?` |
| `release_browser_workspace`   | Release a workspace lease. | `workspace_id`, `holder_id?`, `note?` |
| `close_browser_workspace`     | Close a workspace and terminate its local browser process when owned here. | `workspace_id`, `reason?` |

### Live audio

| Tool               | Description | Params |
|--------------------|-------------|--------|
| `spawn_live_audio` | Spawn an untrusted [live-audio](./computer-use-and-audio.md#live-audio) voice session. | `id`, `provider`, `playbook`, `response_schema`, … |

### Controller Orchestration

| Tool                            | Description | Params |
|---------------------------------|-------------|--------|
| `schedule_controller_restart`   | Schedule a controller restart / autonomous re-init workflow. | `controller_id`, `north_star_goal`, `reason?`, `restart_after?`, `restart_command?`, `auto_start_task?`, `max_attempts?`, `cooldown_sec?` |
| `controller_turn_complete`      | Final handshake; validates token and executes the scheduled restart. | `restart_id`, `turn_complete_token`, `status?`, `handoff_summary?` |
| `get_restart_status`            | Current restart state (or null). | — |
| `cancel_controller_restart`     | Cancel a scheduled restart. | `restart_id?` |
| `request_controller_loop_halt`  | Request loop halt. | `persistent?` |
| `clear_controller_loop_halt`    | Clear loop-halt flags so restarts can resume. | — |
| `intervene_controller_loop`     | Intervene in the active loop process. | `mode`: `stop`/`abort` |
| `get_controller_loop_status`    | Unified loop-health snapshot. | — |

`schedule_controller_restart`, `controller_turn_complete`, and
`cancel_controller_restart` return JSON payloads with an `ok` boolean and status
fields; rejections come back as JSON (`ok: false`) with an `error` message rather
than plain text.

## Resources

Resources provide push-based observation via subscriptions. The server emits
`notifications/resources/updated` when state changes so clients re-fetch.

| URI                              | Description |
|----------------------------------|-------------|
| `intendant://status`             | Provider, model, turn, budget %, phase, autonomy, session ID, task. |
| `intendant://usage`              | Per-model token usage (main + optional presence). |
| `intendant://logs`               | Last 100 chronological log entries (same as the TUI log panel). |
| `intendant://pending-approval`   | The current pending approval, if any. |
| `intendant://pending-input`      | The current pending `askHuman` question, if any. |
| `intendant://controller-restart` | Current controller-restart workflow state, if any. |
| `intendant://controller-loop`    | Loop-health snapshot (intervention flags, singleton lock owner, active wrapper/codex PIDs, latest run pointers). |

## Controller Restart Workflow

Use this when you want Intendant to trigger a controller re-init cycle safely
(e.g. an external Codex/Claude controller relaunching itself).

1. Call `schedule_controller_restart`; capture `restart_id` + `turn_complete_token`.
2. Before ending the controlling agent's turn, call `controller_turn_complete`
   with both values.
3. Intendant executes the restart actions:
   - spawn `restart_command` (if provided), and/or
   - start a fresh Intendant task from `north_star_goal`
     (`auto_start_task=false` by default; opt in only for E2E testing).
4. Inspect via `get_restart_status` or `intendant://controller-restart`.

### Notes & guarantees

- Restart state persists to the session dir as `controller_restart.json`.
- `restart_after` defaults to `"turn_end"`; only `"turn_end"` or `"now"` are
  accepted (others rejected). String inputs are trimmed before validation.
- `restart_command`, when provided, must be non-empty/non-whitespace.
- At least one restart action is required: `restart_command` and/or
  `auto_start_task=true`.
- `max_attempts` must be `>= 1` (`0` rejected). Optional `status`,
  `handoff_summary`, and the cancel `restart_id` guard treat whitespace-only as
  unset.
- If `restart_after="now"` and execution fails after validation,
  `schedule_controller_restart` reports `"ok": false` with `execution_error`, and
  the persisted phase becomes `"failed"` with `last_error` populated.
- `controller_turn_complete` only accepts restarts in
  `"awaiting_turn_complete"`; duplicate/late handshakes (e.g. `"phase": "ready"`)
  are rejected to prevent double execution.
- `get_restart_status` and `intendant://controller-restart` redact
  `turn_complete_token` as `"[redacted]"`; only `schedule_controller_restart`
  returns the raw token (for the final handshake).
- `request_controller_loop_halt`, `clear_controller_loop_halt`,
  `intervene_controller_loop`, and `get_controller_loop_status` return/emit
  normalized loop-health data (flags, lock owner PID + liveness, latest run
  pointers, active PID counts). The control socket's `command_result.data`
  mirrors the same structured payloads.

### Controller recursion profile

Recommended for Codex/Claude-style controllers:

- Set `auto_start_task=false` (or omit it — `false` is the default).
- Use `restart_command` to relaunch the external controller process.
- Treat `start_task` as optional E2E testing, not the default recursion path.

## Controller Loop Monitoring

For `restart_command` wrapper scripts, loop artifacts live under
`.intendant/controller-loop/`:

- Stable pointers: `latest` (symlink), `latest.pid`, `latest.status.json`,
  `latest.jsonl`, and the singleton `active.lock/` (`pid`, `run_id`,
  `acquired_at`).
- Inspect: `tail -f .intendant/controller-loop/latest/codex.jsonl`,
  `cat .intendant/controller-loop/latest.status.json`.
- Intervention markers: `touch .intendant/controller-loop/request_halt`
  (persistent), `request_halt_after_cycle` (one-shot legacy), `request_stop`
  (graceful), `request_abort` (immediate). History:
  `.intendant/controller-loop/latest/intervention.log`.
- Per-run PIDs: `.intendant/controller-loop/<run_id>/wrapper.pid` and
  `codex.pid`.

## Typical Agent Workflow

1. `get_status` for the current phase and budget.
2. Poll `get_logs` with `since_id` to stream new events (or subscribe to
   `intendant://logs`).
3. On an approval, `get_pending_approval` gives the command preview → `approve`,
   `deny`, or `skip`.
4. On an `askHuman`, `get_pending_input` gives the question → `respond`.
5. `quit` when done.

## MCP Client

Intendant can also be an MCP **client**, connecting to external MCP servers
configured in `intendant.toml` so the agent can use their tools alongside
Intendant's native ones (`mcp_client.rs`).

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

At startup, `McpClientManager::connect_all()` spawns each server, discovers its
tools, and registers them as `mcp__<server>_<tool>` (e.g. a `filesystem` server's
`read_file` → `mcp__filesystem_read_file`). Tool calls with the `mcp__` prefix
are routed to the right server. If a server fails to connect, it is skipped with
a warning; other servers and native tools keep working.

### Trust model — read this before adding a server

Each `[[mcp_servers]]` entry is launched as a **child process with the user's
full privileges**:

```rust
let mut cmd = Command::new(&config.command);
cmd.args(&config.args);
let transport = TokioChildProcess::new(cmd)?;   // mcp_client.rs
```

Intendant performs **no checksum verification, no signature check, and no
sandboxing** of MCP server binaries. Adding an MCP server is equivalent to adding
a line to your `~/.zshrc` that runs a binary.

Mitigating defaults: `mcp_servers = []` by default, and `intendant.toml` is
**git-ignored**, so the repo ships no MCP servers. Treat copying an
`intendant.toml` between machines like copying shell rc files — read it before
you source it.

## See Also

- [TUI & Autonomy](./tui.md) — the other half of the `UserAction` contract, and
  the autonomy model that gates approvals.
- [Integrations](./integrations.md) — `ControlMsg`, the control socket, and the
  web gateway WebSocket protocol.
