# External-Agent Orchestration

Intendant can hand a whole task to a third-party coding CLI — **OpenAI Codex**,
**Claude Code**, or **Gemini CLI** — and supervise it as a subordinate worker. The
external tool does the actual coding; Intendant wraps it in its own oversight,
display, and computer-use surface by pointing the tool's MCP client at Intendant's
own [MCP server](./mcp-server.md).

This is the fourth execution mode (alongside Direct, User, and Sub-Agent — see
[Agent Execution & Multi-Agent Orchestration](./multi-agent.md)). It is selected
by `--agent <backend>` or the `[agent] default_backend` config key.

## Why

These CLIs are excellent autonomous coders but live in their own terminals, with
their own approval prompts, no shared display, and no voice/phone reach. Wrapping
one in Intendant gives you:

- **One oversight surface.** The supervised agent's command/file approval requests
  are lifted into Intendant's frontends (TUI, web dashboard, MCP, `--json`) and the
  same autonomy policy that governs the native agent.
- **Display & computer use.** Intendant injects an `intendant` MCP server into the
  external tool's config, so the coding agent can call Intendant's MCP tools —
  screenshots, computer use, etc. — over MCP-over-HTTP against the running gateway.
- **Presence & multi-session.** The supervised session is just another session on
  the [EventBus](./architecture.md); the [presence layer](./presence.md) narrates
  it and the daemon can run several alongside native agents
  (see [control plane & daemon](./control-plane-and-daemon.md)).

Crucially, external-agent control does **not** flow through the `UserAction` enum
that unifies the native frontends. It rides `ControlMsg` (inbound) and `AppEvent`
(outbound) on the EventBus (`event.rs`), because the verbs are backend-shaped
(steer a turn, fork a thread, roll back) rather than the native action set.

## The `ExternalAgent` Trait

Every backend implements one async trait, `ExternalAgent`
(`src/bin/caller/external_agent/mod.rs`). The controller supervises through this
contract and never touches a backend's wire protocol directly:

```rust
#[async_trait]
pub trait ExternalAgent: Send + Sync {
    fn name(&self) -> &str;

    // Lifecycle
    async fn initialize(&mut self, config: AgentConfig)
        -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError>;
    async fn start_thread(&mut self) -> Result<AgentThread, CallerError>;
    async fn shutdown(&mut self) -> Result<(), CallerError>;

    // Turns
    async fn send_message(&mut self, thread: &AgentThread, message: &str) -> Result<(), CallerError>;
    async fn send_message_with_images(/* … */) -> Result<(), CallerError>;          // default: text-only
    async fn send_message_with_attachments(/* … */) -> Result<(), CallerError>;     // default: stage files + prelude

    // Oversight
    async fn resolve_approval(&mut self, request_id: &str, decision: ApprovalDecision)
        -> Result<(), CallerError>;
    async fn interrupt_turn(&mut self) -> Result<(), CallerError>;                  // default: unsupported error
    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError>;          // default: unsupported error

    // Rich thread control (Codex)
    async fn thread_action(&mut self, op: &str, params: &Value) -> Result<String, CallerError>; // default: unsupported
    async fn rollback_turns(&mut self, turns_to_drop: u32) -> Result<(), CallerError>;           // default: unsupported
    async fn rollback_thread_turns(&mut self, thread_id: &str, n: u32) -> Result<(), CallerError>;
    async fn activate_thread(&mut self, thread_id: &str) -> Result<(), CallerError>; // local adapter state only
    fn supports_user_message_rewind(&self) -> bool;                                  // default: false

    // Exact provider request payload (if the backend exposes one)
    async fn context_snapshot(&mut self) -> Result<Option<AgentContextSnapshot>, CallerError>;
}
```

`initialize()` spawns the backend process and returns a channel of normalized
**`AgentEvent`**s; everything the backend emits (deltas, messages, reasoning, plan
updates, tool start/output/complete, approval requests, diffs, usage, termination)
is translated into that enum so the controller's display and oversight code is
backend-agnostic. `AgentEvent::Scoped { thread_id, turn_id, .. }` wraps inner
events when a backend (Codex) multiplexes several threads through one process.

`AgentConfig` carries the working dir, model, approval policy, the
**`web_port`** (used to generate the MCP-over-HTTP config), an optional
`resume_session` id, and the Codex-only knobs (`sandbox`, `reasoning_effort`,
`web_search`, `network_access`, `writable_roots`). Backends that don't model a
field ignore it.

The three backend identities are the `AgentBackend` enum (`Codex`, `ClaudeCode`,
`GeminiCli`). `from_str_loose()` accepts the canonical short forms plus older
Display forms (`codex`, `claude-code`/`claude_code`/`cc`, `gemini`/`gemini-cli`,
case-insensitive); `as_short_str()` emits the canonical wire form that matches the
dashboard dropdown's `<option value>`.

## Per-Backend Reference

`create_external_agent()` (`main.rs`) constructs the right adapter from
`[agent.<backend>]` config, then `run_external_agent_mode()` drives the supervise
loop.

| | **Codex** (reference impl) | **Claude Code** | **Gemini CLI** |
|---|---|---|---|
| Module | `external_agent/codex.rs` (~200 KB) | `external_agent/claude_code.rs` | `external_agent/gemini.rs` |
| Spawn command | `codex app-server` | `claude -p --output-format stream-json --input-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio` | `gemini --acp` |
| Wire protocol | JSON-RPC over JSONL (`app-server`) | stream-json over stdio | ACP (Agent Client Protocol) |
| MCP injection | Writes `.codex/config.toml` **and** passes `-c mcp_servers.intendant.{type,url}` flags | Inline `--mcp-config '{…}'` JSON string | Merges `mcpServers.intendant` into `$HOME/.gemini/settings.json` |
| Multi-thread | Yes — many threads per process | No | No |
| Native thread id | Yes | No (`claude-code-session` placeholder until a real id appears) | Yes |
| Mid-turn steer | Yes (`turn/steer`) | No → queue + next-turn fallback | No → queue + next-turn fallback |
| Rollback turns | Yes (`thread/rollback`) | No → session reset | No → session reset |
| Fork / side threads / review / goals / compact / fast / memory-reset | Yes (`thread_action`) | No | No |

All three spawn through `crate::platform::spawn_command(&cfg.command)` with the
working dir set to the project root, stdin/stdout piped, and stderr inherited.

### Codex (the reference backend)

Codex is the most fully wired backend; the others fall back to defaults for
features they lack.

- **MCP injection — belt and suspenders.** On `initialize()`, Codex writes
  `<workspace>/.codex/config.toml`:

  ```toml
  # Auto-generated by Intendant for MCP-over-HTTP integration.
  # Original config backed up to config.toml.intendant-backup (if it existed).

  [mcp_servers.intendant]
  type = "http"
  url = "http://localhost:<web_port>/mcp?managed_context=vanilla&tool_profile=core"
  ```

  Any pre-existing `config.toml` that isn't already ours is copied to
  `config.toml.intendant-backup` first. On `shutdown()` the backup is **renamed
  back**; if there was no backup (we created it fresh) the generated file is
  removed. In addition, the same MCP entry is passed on the command line as
  `-c mcp_servers.intendant.type="http" -c mcp_servers.intendant.url="…"`, plus the
  user's toggles as further `-c` overrides:
  `tools.web_search=true`, `model_reasoning_effort="…"`,
  `sandbox_workspace_write.network_access=true` (only in `workspace-write`), and
  `sandbox_workspace_write.writable_roots=[…]`.

  Codex uses `tool_profile=core` by default to avoid MCP tool-schema bloat. The
  core profile keeps a small bootstrap surface (`get_status`, shared-view tools,
  and managed-context tools when enabled). Broad or rare Intendant operations
  should be discovered lazily through `intendant ctl --help`,
  `intendant ctl tools list`, and focused subcommand help. Supervised Codex
  sessions receive `INTENDANT=/absolute/path/to/intendant`, `INTENDANT_MCP_URL`,
  `INTENDANT_SESSION_ID`, and `INTENDANT_MANAGED_CONTEXT`, so agent shells can
  run `"$INTENDANT" ctl ...` without relying on user PATH setup.

  For dashboard/browser validation against an already-running Intendant web port,
  managed agents should use the repository helper instead of generating ad-hoc
  Chromium/CDP scripts:

  ```bash
  node scripts/validate-dashboard.cjs --port <web_port> --selector '<css>'
  node scripts/validate-dashboard.cjs --url http://127.0.0.1:<web_port>/app \
    --wait-for-function '() => Boolean(window.someReadyFlag)'
  ```

  The helper launches a fresh isolated headless Chromium, waits for CDP
  readiness, supports selector/function waits, falls back when Node has no
  WebSocket module, and prints compact PASS/FAIL output with bounded log
  excerpts on failure. It does not default to port 8765; pass `--port`/`--url` or
  let it derive the port from `INTENDANT_MCP_URL`. Managed agents should keep
  validation bounded: one primary smoke, at most one diagnostic retry such as
  `--diagnostics --json`, then either a targeted fix or a clear
  partial-validation conclusion with the helper reason/logs/diagnostics.

  `[agent.codex] managed_context = "vanilla"` is the default and is safe for
  upstream Codex or the original Codex fork. Set it to `"managed"` only when
  launching the Intendant-aware Codex fork; that mode advertises
  `rewind_context` / `rewind_backout`, suppresses Codex auto-compaction, and
  uses same-thread rollback/restore to keep the active thread informationally
  dense. Rewinds are not just emergency context-limit recovery: they should also
  happen after noisy tool output, failed exploration, or a long research branch
  whose useful result can be crystallized into a compact primer. Model-driven
  rewinds must first call `list_rewind_anchors`; when the compact row is
  ambiguous, `inspect_rewind_anchor` returns a small before/after window for the
  candidate. `rewind_context` still validates the exact `item_id` against the
  current rollout before mutating the Codex thread.
  When backend-reported pressure is at or above the rewind-only threshold,
  `list_rewind_anchors` defaults to recovery candidates: anchors whose nearest
  following backend token report is below that threshold. Passing
  `include_non_recovery=true` is an audit escape hatch, not the normal recovery
  path. A successful `rewind_context` only proves the lineage mutation was
  applied; Intendant and Codex keep normal tools hidden until a later backend
  token report confirms the active thread is below the rewind-only limit.

  Managed Codex relies on the minimal lineage patch separating Codex's thread id
  from the Responses `prompt_cache_key`. Same-thread restore keeps the active
  thread id. Fork/backout creates a new Codex thread id but inherits the rollout's
  lineage prompt-cache key, so branch recovery is git-style without deliberately
  resetting cache routing. The old `allow_cache_reset` flag is accepted only for
  compatibility with older clients; it is not required for managed forks.
  Dashboard edits of a user message that is still active use the normal precise
  Codex rollback path. If the clicked message has been overwritten by a managed
  rewind, Intendant treats the edit as a branch operation: it finds the newest
  saved pre-rewind rollout that still contains that exact message text, forks that
  rollout, rolls the child back to just before the selected user turn, and starts
  the child with the replacement message. The original compacted thread is not
  mutated silently.

- **Rich `thread_action` ops** (`codex.rs`): `compact`, `fast`, `fork`,
  `side`/`btw` (open a side conversation) and `side-close`, `review`,
  `goal`/`goal-set`/`goal-clear`/`goal-pause`/`goal-resume`/`goal-complete`, and
  `memory-reset`. Side threads can be steered and rolled back independently of the
  parent (`rollback_thread_turns`, `activate_thread`). Codex also reports native
  **sub-agent** activity (`AgentEvent::SubAgentToolCall`) and per-fork token
  accounting.

  `/fast` is also a session-bootstrap command: when a new-session request contains
  exactly `/fast`, the supervisor starts a new idle Codex session and passes
  `serviceTier: "priority"` on `thread/start`. Existing targeted `/fast` commands
  remain live thread actions and toggle the service tier for future turns in that
  Codex session; if typed while the prompt is in steer mode, the supervisor still
  converts it to the thread action rather than sending `/fast` as model text.

- **Diff handling.** Codex's `turn/diff/updated` sometimes carries paths only
  inside the diff body; `parse_diff_file_paths()` recovers them from the unified
  diff when the explicit `files_changed` list is empty.

### Claude Code

Spawned in non-interactive stream-json mode with `--permission-prompt-tool stdio`,
so permission prompts come back over the JSON stream and become
`AgentEvent::ApprovalRequest` / `FileApprovalRequest`. The Intendant MCP server is
passed **inline** as a JSON string to `--mcp-config` (not a file path).
`--permission-mode` and `--allowedTools` are added from config when set;
`--resume <id>` resumes a prior session. Claude Code doesn't surface a real session
id at thread start, so Intendant keeps its own log id canonical until a usable
native id appears.

### Gemini CLI

Spawned with `--acp` and driven over the Agent Client Protocol. Intendant merges an
`mcpServers.intendant` entry into **`$HOME/.gemini/settings.json`** — deliberately
the home settings file, not a project-local `.gemini/`, so it doesn't shadow the
real config directory holding OAuth credentials. The prior value of
`mcpServers.intendant` (present or absent) is remembered and restored on
`shutdown()` (and on `Drop` as a safety net). Config knobs map to flags:
`--model`, `--approval-mode`, `--sandbox`, `--extensions`,
`--allowed-mcp-server-names`, `--include-directories`, `--debug`. If you set
`allowed_mcp_servers`, you must include `intendant` or the injected tools won't be
reachable.

## Approval Routing

When a supervised agent asks to run a command or change a file, the backend emits
`AgentEvent::ApprovalRequest` / `FileApprovalRequest`. `drain_external_agent_events()`
(`main.rs`) routes the decision through **the same autonomy policy and approval
registry as the native agent**:

```
External agent ─► AgentEvent::ApprovalRequest { request_id, command, category }
                       │
       map category ──►  CommandExecution → CommandExec
                         FileChange       → FileWrite
                       │
   autonomy.external_approval_decision(category)
        ├── AutoApprove ─► resolve_approval(Accept)            + AppEvent::AutoApproved
        ├── Reject ──────► resolve_approval(Decline)           + AppEvent::ApprovalResolved("deny")
        ├── headless &&  ─► resolve_approval(Decline)          (no interactive frontend → auto-deny)
        │   no json &&
        │   no web_port
        └── otherwise ───► AppEvent::ApprovalRequired { id, command_preview, category }
                              └─ await decision via ApprovalRegistry / JsonApprovalSlot
                                 approve      → Accept
                                 approve_all  → AcceptForSession
                                 deny         → Decline
                                 skip         → Cancel
                                 channel drop → Decline (fail safe)
                              └─ AppEvent::ApprovalResolved + resolve_approval(decision)
```

Because the request becomes an ordinary `AppEvent::ApprovalRequired`, every
frontend that already renders native approvals — the TUI gate, the web dashboard,
the MCP `approve`/`deny` tools, and `--json` stdin — handles external-agent
approvals identically. `ApprovalDecision` (re-exported from `crate::approval`) is
the shared decision vocabulary; `AcceptForSession` is how "approve all" sticks for
the rest of the session. Note that `--web` providing a `web_port` is what keeps an
otherwise-headless run from auto-denying: it signals that an interactive frontend
exists.

## Configuration

External-agent settings live under `[agent]` in `intendant.toml`
(`ExternalAgentConfig` in `project.rs`). `default_backend` selects the mode; the
per-backend subtables tune each tool. All keys have defaults, so a bare `[agent]`
with just `default_backend` works.

```toml
[agent]
# Which backend to use when --agent is not passed. Omit/empty = native agent.
# Accepts: "codex", "claude-code", "gemini".
default_backend = "codex"

[agent.codex]
command          = "codex"            # binary on PATH or absolute path
model            = "gpt-5-codex"      # optional; omit to use Codex's default
approval_policy  = "on-request"       # untrusted | on-request | never
sandbox          = "workspace-write"  # read-only | workspace-write | danger-full-access
reasoning_effort = "medium"           # ""(default) | minimal | low | medium | high | xhigh
web_search       = false              # enable the Responses web_search tool
network_access   = false              # outbound net inside workspace-write only
writable_roots   = []                 # extra writable dirs (absolute), each → -c writable_roots
managed_context = "vanilla"          # vanilla | managed

[agent.claude_code]
command         = "claude"
model           = "claude-sonnet-4-6-20250929"   # optional
permission_mode = "auto"              # default | acceptEdits | plan | auto | bypassPermissions
allowed_tools   = []                  # e.g. ["Read", "Edit", "Bash"]; empty = all

[agent.gemini_cli]
command              = "gemini"
model                = "gemini-2.5-pro"  # optional
approval_mode        = "default"      # default | auto_edit | yolo | plan
sandbox              = false          # pass --sandbox
extensions           = []             # --extensions; empty = all installed
allowed_mcp_servers  = []             # --allowed-mcp-server-names; if set, include "intendant"
include_directories  = []             # --include-directories (absolute)
debug                = false          # --debug (Gemini DevTools console)
```

Values are normalized at dispatch (`normalize_sandbox_mode`,
`normalize_approval_policy`, `normalize_reasoning_effort`,
`normalize_gemini_approval_mode`): unknown or empty values fall back to the safe
default rather than silently escalating privileges (e.g. a typo'd Codex sandbox
becomes `workspace-write`, not `danger-full-access`; a bad Gemini approval mode
becomes `default`, not `yolo`).

### Selecting the backend with `--agent`

```bash
intendant --agent codex "refactor the auth module"
intendant --agent claude-code "add tests for the parser"
intendant --agent gemini --web "investigate the flaky CI job"
```

`--agent <name>` parses via `AgentBackend::from_str_loose` and overrides
`default_backend` for that run; an unknown name is a hard config error.
`resolve_agent_backend_from_config()` applies the precedence: explicit flag → MCP
shared state (when driven over MCP) → config default → native.

## Gotchas and Caveats

- **Config-file mutation.** Codex (`.codex/config.toml`) and Gemini
  (`$HOME/.gemini/settings.json`) have their config **mutated in place** and
  restored on clean shutdown. Codex backs up a non-Intendant config and renames it
  back; Gemini remembers and restores the prior `mcpServers.intendant` value (with a
  `Drop` safety net). A hard kill that bypasses `shutdown()` can leave the
  Intendant-generated entry behind — check for `config.toml.intendant-backup` /
  a stray `mcpServers.intendant` if a session crashed.
- **Settings latch at thread/process start.** Codex latches sandbox, approval
  policy, model, reasoning effort, tool set, and writable roots at `thread/start`;
  Gemini latches its flags at process spawn (no equivalent of `thread/start`).
  Changing these mid-session requires a teardown + respawn. The daemon's
  `codex_runtime_config_equal` / `gemini_runtime_config_equal` checks detect drift
  across tasks and force a rebuild when any latched field changes.
- **Per-session launch config beats global defaults.** Dashboard-created and
  dashboard-configured external sessions persist their binary command and, for
  Codex, `managed_context` mode. Resume/attach first applies explicit dashboard
  overrides, then the persisted per-session config, then the global Settings
  pane. This keeps old sessions from silently adopting a new global Codex binary
  or managed-context mode after a daemon restart.
- **Managed historical edits are branches.** Once a managed rewind has replaced
  old rollout context with a dense primer, the old user-turn number may no longer
  exist in the active Codex thread. Editing or jumping to that overwritten message
  must fork from the closest saved pre-rewind rollout containing the clicked
  message, then roll the fork back to the selected turn. Do not send stale visible
  turn numbers directly to the compacted active thread.
- **Load-bearing fallback error strings.** Several trait methods return a *typed
  error* by default (`steer_turn`, `rollback_turns`, `interrupt_turn`,
  `thread_action`). `drain_external_agent_events` distinguishes "feature
  unsupported by this backend" from "feature attempted but failed" partly by these
  error messages — e.g. mid-turn steering on Claude Code / Gemini returns the
  unsupported error, and the caller falls back to **queueing** the text onto the
  context-injection queue for delivery at the next turn. Don't reword those
  strings without checking the drain logic.
- **`--direct` does not bypass external mode.** It only forces single-agent
  execution of the *native* worker. If a backend is configured, the supervised CLI
  still runs.
- **MCP reachability needs the gateway.** The injected `intendant` MCP server is
  MCP-over-HTTP at `http://localhost:<web_port>/mcp`. The external tool can only
  reach Intendant's display/CU tools while the gateway is up; without a resolved
  `web_port`, the MCP entry still points at the default port but nothing answers.
- **The external tool brings its own keys.** Intendant supervises the process but
  the coding CLI authenticates to its own provider with its own credentials —
  Intendant's `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` are for the
  native agent and presence layer, not the supervised tool.

## See Also

- [Agent Execution & Multi-Agent Orchestration](./multi-agent.md) — the four modes
  and native sub-agent orchestration.
- [MCP Server](./mcp-server.md) — the control surface the external tool's MCP
  client connects back to.
- [Control plane & daemon](./control-plane-and-daemon.md) — running and supervising
  multiple sessions (native and external) from one daemon.
- [Configuration](./configuration.md) — the full `intendant.toml` reference.
