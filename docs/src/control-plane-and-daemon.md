# Control Plane & Persistent Daemon

This chapter covers the machinery that turned Intendant's controller from a
single agent loop into a multi-session orchestration host: the single-writer
**control plane**, the long-lived **session supervisor**, the **task
dispatcher**, the **file-watcher** that powers rewind/redo, the **headless
daemon** that an idle `--web` launch becomes, and **cost accounting**. It closes
with an explicit note on what is *not* built yet.

For the system-wide picture and the EventBus that ties these together, start
with [Architecture](./architecture.md).

## Why a Single-Writer Control Plane

Intendant has four frontends (TUI, web dashboard, MCP server, control socket)
and they can all be live at once against the same daemon. If each frontend wrote
shared mutable state directly — "the dashboard sets autonomy to High, the TUI
sets it to Low" — the truth would depend on event ordering and which render loop
happened to run. Worse, some state (the active external-agent backend, Codex
sandbox/model config) also has to persist to `intendant.toml`, and you do not
want three frontends racing to rewrite the same file.

The fix is a hard rule, stated at the top of `control_plane.rs`:

> Frontends remain display-only — they render state changes but never write to
> shared state from `ControlMsg` handlers.

Frontends *emit intents* (`ControlMsg`, defined in `event.rs`) onto the
EventBus. Exactly one subscriber — the control plane — interprets the
state-mutating ones and applies them. Everyone else (including the frontend that
sent the intent) learns the result by observing the *broadcast* event the
control plane emits afterward (`AutonomyChanged`, `ExternalAgentChanged`,
`CodexConfigChanged`, …).

```
  TUI ─┐
  Web ─┤  emit ControlMsg          ┌──────────────┐  write    ┌──────────────┐
  MCP ─┼───────────────▶ EventBus ─┤ Control Plane │──────────▶│ shared state │
 Sock ─┘                  (bus)    │ (sole writer) │           │  + intendant │
        ◀───── observe ────────────┤               │◀──────────│    .toml     │
         AutonomyChanged etc.      └──────────────┘            └──────────────┘
```

### What the control plane owns

`ControlPlaneState` (in `control_plane.rs`) holds the shared, mutable runtime
state:

| Field | Type | Notes |
|-------|------|-------|
| `autonomy` | `SharedAutonomy` | Global autonomy level + the user-display grant flag |
| `external_agent` | `Arc<RwLock<Option<AgentBackend>>>` | Active backend: Codex / Claude Code / Gemini CLI, or `None` (internal) |
| `codex_config` | `SharedCodexConfig` | Runtime Codex config (command, sandbox, approval policy, model, reasoning effort, web search, network access, writable roots) |
| `gemini_config` | `SharedGeminiConfig` | Runtime Gemini config (model, approval mode, sandbox, extensions, allowed MCP servers, include dirs, debug) |
| `project_root` | `Option<PathBuf>` | When set, state changes also persist to `intendant.toml` |

It is spawned once, early in `main.rs`, from its own bus subscription:

```rust
let _control_plane_handle = control_plane::spawn(
    bus.subscribe(),
    control_plane::ControlPlaneState { autonomy, external_agent, codex_config, gemini_config, bus, project_root },
);
```

### The "applies on the NEXT task" rule

A subtlety worth internalizing: most Codex and Gemini settings **latch at
process spawn**. Codex locks its sandbox / approval / model / tool configuration
at `thread/start`; Gemini latches `--model` and friends when its process is
launched. So when a frontend flips, say, the Codex sandbox mode, the control
plane updates the shared config and persists it, but an already-running Codex
thread keeps its old values for the rest of its life. The change takes effect on
the **next** task — the daemon loop re-reads the shared config at the start of
each task and, for changes that cannot be applied mid-session, tears down the
persistent agent so the next launch picks them up. Each `ControlMsg::SetCodex*`
/ `SetGemini*` variant documents this in its doc comment.

Two exceptions apply *immediately* rather than next-task, because the backend
accepts them as live RPCs: `CodexThreadAction` (the `/new`, `/compact`, `/fast`, `/fork`,
`/undo`, `/review`, … slash-command surface) and `GeminiThreadAction` (`/new`).
The control plane does not own the persistent agent, so it merely *re-broadcasts*
these as `CodexThreadActionRequested` / `GeminiThreadActionRequested` for the
daemon-side watcher that does own it.

## Session Supervisor

`session_supervisor.rs` is the long-lived owner of every session launched from
the control plane at runtime. Where the control plane owns *settings*, the
supervisor owns *sessions* — their lifecycle, their per-session resources, and
the graph of how they relate.

It subscribes to the bus and handles the session-lifecycle `ControlMsg`s:

| ControlMsg | Behavior |
|------------|----------|
| `CreateSession` | Explicitly create a new managed session and submit its first task (the forward-compatible primitive for parallel sessions). A task of exactly `/fast` is special-cased into a new idle Codex session with the fast service tier enabled. |
| `StartTask { session_id: None }` | Start a new managed session (legacy clients). A task of exactly `/fast` follows the same idle fast Codex bootstrap path as `CreateSession`. |
| `StartTask { session_id: Some(id) }` | Route the text as a follow-up turn into the named session |
| `ResumeSession` | Re-attach an existing session by source (`intendant`/`codex`/`claude-code`/`gemini`) + id, optionally with a prompt |
| `FollowUp` | Route text to a session's follow-up channel (target id, or the active session) |
| `EditUserMessage` | Rewind a session to a user turn and submit replacement text (rollback-capable backends only) |
| `Interrupt` / `Steer` | Mid-turn control of a running session. If a steer body is a supported Codex slash command, the supervisor converts it to a Codex thread action instead of injecting it as model text. |
| `Approve`/`Deny`/`Skip`/`ApproveAll` | Resolve a pending approval against the right session's `ApprovalRegistry` |
| `RenameSession` | Rename via the cross-backend naming abstraction (see [Session Logging](./session-logging.md)) |

### Per-session state

Internally the supervisor keeps a `SupervisorState`:

```
SupervisorState {
    sessions:         HashMap<String, ManagedSession>,   // canonical id → session
    session_aliases:  HashMap<String, String>,           // alias id → canonical id
    related_sessions: HashMap<String, RelatedSession>,   // child id → {parent, relationship}
    active_session_id: Option<String>,                   // the "current" session for un-targeted commands
}
```

Each `ManagedSession` carries its `session_id`, `source` (`intendant` or the
external backend's short name), optional display `name`, `phase`, `project_root`,
`session_dir`, a `follow_up_tx` channel, and its own `ApprovalRegistry`.

When `start_new_session` runs, it:

1. resolves a fresh session log directory (`SessionLog::resolve_path(None)` →
   `~/.intendant/logs/<uuid>/`) and opens the `SessionLog`;
2. resolves the project root (per-session override or the daemon's default) and
   loads the `Project`;
3. writes `session_meta.json` and activates the shared active-session handle;
4. resolves the backend (one-shot `agent` override → configured default →
   internal) and applies the runtime Codex/Gemini config;
5. resolves any dashboard attachments (frames/uploads) into agent content;
6. spawns the agent session loop and emits `SessionStarted`.

### The session graph

The supervisor tracks *relationships* between sessions, not just a flat list. A
child session is linked to a parent with a relationship of `side` or `subagent`
(`apply_related_session`); the alias map lets a follow-up addressed to a child id
resolve to its canonical parent session, and removing a parent prunes its
children. This is what lets the dashboard render a tree of related sessions (a
Codex `/fork` or `/side` thread, an orchestrator's sub-agents) rather than a
disconnected pile. Identity and relationship updates also arrive over the bus as
`SessionIdentity` and `SessionRelationship` events, which the supervisor folds
into the same maps.

`active_session_id` is the fallback target: an un-targeted `FollowUp` or
`Interrupt` resolves to it, which is how single-session frontends keep working
unchanged while multi-session clients address sessions explicitly by id.

## Task Dispatch

`task_dispatch.rs` decides *which channel* a task goes to. It used to live in the
TUI's `handle_control_command`; pulling it out is part of making the TUI
display-only. The `Dispatcher` owns up to three senders — `presence_tx`,
`task_tx`, `follow_up_tx` — and routes a `StartTask`/`FollowUp` like this:

1. If the task is **not** direct and `presence_tx` exists → send the text to the
   [presence layer](./presence.md), which decides whether to forward it as a
   real task (via its own `submit_task` tool) or answer in-line.
2. Else if `task_tx` exists → wrap in a `TaskEnvelope` and send (preserving
   attachments / frame refs / display target). `direct` (and legacy
   `orchestrate == Some(false)`) forces this path.
3. Else if `follow_up_tx` exists → send a follow-up message (metadata dropped;
   non-presence mode has no CU-first routing anyway).
4. Else → warn and drop.

A task carrying metadata (attachments, reference frames, a display target) is
always forced onto `task_tx` even when non-direct, because presence's text
channel cannot carry that metadata.

The dispatcher and the session supervisor coexist: the dispatcher serves the
legacy single-session loop and routes into channels it already owns, while
`CreateSession` and targeted multi-session commands are left to the supervisor.
A targeted command for a session the dispatcher does not own is simply ignored
by it, so the supervisor picks it up.

## File Watcher, Snapshots, and Rewind

`file_watcher.rs` is a live filesystem watcher rooted at the project directory.
It works for **all** agent types — internal, Codex, Claude Code, Gemini CLI —
because it watches the filesystem directly rather than diffing git, so an
external CLI's edits show up the same as Intendant's own. It does two jobs:

1. **Live change events.** It emits `AppEvent::FileChanged { Created / Modified /
   Deleted }` so the dashboard's activity view can show per-file diffs as the
   agent works.
2. **Per-round content-addressed history** for rewind / redo / branching.

On every `AppEvent::RoundComplete`, the watcher records a `HistoryRound`
capturing the *full* project state as a `path → sha256` map, plus the subset of
paths that changed. Content blobs are stored once in a content-addressed
`objects/` directory, so identical content across rounds costs no extra disk.
Each round also records `turn_count` and `native_message_count`, which
conversation rollback uses to truncate the native conversation correctly.

The snapshot store lives **inside the session log dir**:

```
~/.intendant/logs/<uuid>/file_snapshots/
├── baseline/            # initial text-file snapshot at session start
├── objects/             # content-addressed blobs (sha256-named)
├── rounds/              # per-round artifacts
└── history.json         # current_head_id, rounds[], abandoned_branches[], next_id
```

The public API on `FileWatcher`:

- **`rollback(target_round_id)`** — restores every tracked path to that round's
  recorded state, moves `current_head_id` back **without** truncating history
  (so redo stays available), and emits `AppEvent::RolledBack { from_id, to_id,
  files_reverted }`. A *new* action after a rollback branches off the abandoned
  path and stores it in `abandoned_branches` for later pruning.
- **`redo()`** — moves `current_head_id` forward along the linear path, restoring
  file state, emitting `AppEvent::Redone { to_id }`.
- **`prune_abandoned()`** — drops abandoned branches and garbage-collects
  orphaned blobs, emitting `AppEvent::HistoryPruned { branches_removed,
  bytes_freed }`.

A soft byte cap bounds total snapshot size; once exceeded, pruning kicks in. The
dashboard exposes this as the rewind/redo UI; on session resume or controller
restart, `history.json` is reloaded so history survives the restart.

## Headless Daemon Mode (idle `--web`)

A bare `--web` launch with no task is special. `should_start_idle_web_daemon`
returns true when `--web` is set, it is not an MCP-stdio run, and no task was
supplied:

```rust
fn should_start_idle_web_daemon(use_web: bool, flags: &CliFlags) -> bool {
    use_web && !flags.mcp && flags.task.as_ref().map(|t| t.trim().is_empty()).unwrap_or(true)
}
```

When that holds, the controller does **not** start a terminal TUI. Instead it
runs `run_daemon_loop` (`main.rs`), which simply constructs and runs the session
supervisor:

```
run_daemon_loop(DaemonConfig { bus, project_root, autonomy,
                               shared_external_agent, shared_codex_config,
                               shared_gemini_config, frame_registry,
                               web_port, flags_direct, shared_session })
    └─▶ SessionSupervisor::new(..).run()   // owns every launch
```

The daemon then sits idle waiting for `CreateSession` / `StartTask` /
`ResumeSession` / follow-up intents arriving over the dashboard WebSocket (or the
control socket). This is the persistent-daemon mode: one long-lived controller,
many sessions over its lifetime, driven entirely from frontends. Pass `--no-web`
to keep the interactive terminal TUI instead.

Ctrl+C in this mode is handled by the global signal handler installed in `main`
(it marks the session interrupted and exits 130); the daemon loop deliberately
does not also listen for it, to avoid racing two handlers.

> **Controller stdout/stderr tee.** Whenever the controller does *not* own a real
> interactive TTY (i.e. web/headless/MCP), `daemon_log_tee::install` tees its
> stderr and stdout into `~/.intendant/logs/<uuid>/daemon.log` (with per-line
> timestamps) while still mirroring to the original terminal. This is Unix-only;
> on Windows `install` is a no-op. It is what lets the dashboard's "Download
> session report" bundle carry controller-side `eprintln!`/panic/tracing output
> alongside `session.jsonl`. The tee is *skipped* under the real TUI because
> ratatui writes escape sequences to stdout and cannot tolerate a pipe.

## Cost Accounting

`app_state_pricing.rs` provides server-side per-model USD cost estimation,
mirroring the pricing table in `presence-web/app_state.rs` so the native daemon
and the browser agree. Each entry gives per-token prices for input, cache-write,
cached, and output tokens; `estimate_session_cost(...)` combines those with a
session's token usage, and `estimate_live_usage_cost(...)` covers live-audio
usage. The dashboard surfaces these as the running session cost.

## What Is Not Built Yet

Be precise about this: **Intendant has no recurring / scheduled-task facility.**
There is no cron, no "run every N minutes", no calendar of future tasks.

The *only* scheduling primitive is the one-shot `ScheduleControllerRestart`
(`event.rs` / `mcp.rs`): it schedules a single controller restart after an
optional delay, carrying a north-star goal and handoff summary across the
restart boundary. It is a continuity mechanism for long-running work, not a task
scheduler. Treat any documentation or assumption that implies recurring jobs as
incorrect.

## Where to Go Next

- [Architecture](./architecture.md) — the EventBus, four execution modes, and
  the corrected frontend-parity model.
- [Session Logging](./session-logging.md) — the on-disk layout these components
  read and write, and the cross-backend session naming the supervisor uses.
- [Web Dashboard](./web-dashboard.md) — the primary consumer of these events
  (activity diffs, rewind UI, session graph, cost).
- [Multi-Agent Orchestration](./multi-agent.md) — User mode, sub-agents, and
  external-agent supervision that the supervisor launches.
