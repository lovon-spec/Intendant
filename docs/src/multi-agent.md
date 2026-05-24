# Agent Execution & Multi-Agent Orchestration

Intendant can run a task four different ways. The simplest is a single agent loop;
the richest is an orchestrator that decomposes a task and delegates pieces to
specialized child agents, each in its own git worktree. A fourth mode hands the
whole task to a third-party coding CLI (Codex, Claude Code, Gemini) and supervises
it — that path has its own chapter, [External-Agent Orchestration](./external-agent-orchestration.md).

This chapter covers Intendant's *native* execution: the four modes, how one is
chosen, and the orchestrator → sub-agent machinery (worktrees, role prompts,
progress/result IPC, and knowledge routing).

## The Four Execution Modes

| Mode | Selected by | What runs | Source entry point |
|------|-------------|-----------|--------------------|
| **Direct** | `--direct`, or a "simple" task heuristic | One agent loop, no delegation | `run_direct_mode` (`main.rs`) |
| **User** | Default for non-trivial tasks (no `--direct`) | A pure-monitor user layer that spawns an **orchestrator** sub-agent which spawns workers | `run_user_mode` (`main.rs`) |
| **Sub-Agent** | `INTENDANT_ROLE` is set in the environment | A scoped child task with a role-specific prompt, fully autonomous | `run_sub_agent_mode` (`main.rs`) |
| **External-Agent** | `--agent <backend>` or `[agent] default_backend` | A supervised third-party coding CLI wired to Intendant's MCP server | `run_external_agent_mode` (`main.rs`) — see [External-Agent Orchestration](./external-agent-orchestration.md) |

### How the mode is chosen

At startup the caller resolves the mode in this order (see the dispatch around the
end of `src/bin/caller/main.rs`):

```
                       ┌─ INTENDANT_ROLE set? ──► Sub-Agent mode
                       │   (this process IS a child agent)
 intendant <task> ─────┤
                       │   ┌─ --agent / [agent] default_backend? ──► External-Agent mode
                       └───┤
                           │   ┌─ --direct, or is_simple_task(task)? ──► Direct mode
                           └───┤
                               └─ otherwise ──────────────────────────► User mode (orchestration)
```

- **Sub-Agent** is detected first: `sub_agent::detect_sub_agent_mode()` simply
  checks whether `INTENDANT_ROLE` is present. Every orchestrator-spawned child is
  an ordinary `intendant` process with that variable set, so the same binary
  re-enters as a worker.
- **External-Agent** is resolved by `resolve_agent_backend_from_config()`: an
  explicit `--agent` flag wins, otherwise the `[agent] default_backend` TOML key.
  If neither names a backend, native execution is used.
- **Direct vs. User**: with `--direct` the worker runs alone. Without it, the
  `is_simple_task()` heuristic decides — a task of three lines or fewer that
  contains none of the "complex" keywords (`research`, `investigate`, `implement`,
  `build`, `refactor`, `migrate`, `deploy`, `set up`, `analyze`, `compare`,
  `design`, `create a`) is treated as simple and runs Direct; anything else
  triggers User-mode orchestration. The TUI and MCP paths use the same heuristic
  when the caller doesn't force a choice.

> Note: `--direct` only forces *single-agent* execution of the native worker. It
> does **not** disable External-Agent mode — if a backend is configured, the
> supervised CLI still runs.

## User Mode and the Orchestrator

When a complex task is submitted without `--direct`, Intendant enters **User
Mode**. The user layer (`SysPrompt_user.md`) is a clean, conversational interface
that does **not** execute commands itself. Its sole job is to spawn an
orchestrator sub-agent and relay its progress.

The orchestrator (`SysPrompt_orchestrator.md`) is itself a sub-agent — it is
spawned with `INTENDANT_ROLE=orchestrator`. It decomposes the task, spawns
specialized workers, monitors them, routes knowledge between them, and synthesizes
the result.

```
User (TUI / Web / MCP / CLI)
    │
    ▼
[User layer]  — SysPrompt_user.md, monitors only, spawns the orchestrator
    │
    ▼
[Orchestrator]  — SysPrompt_orchestrator.md, INTENDANT_ROLE=orchestrator
    ├──▶ [Research agent]        — INTENDANT_ROLE=research
    ├──▶ [Implementation agent]  — INTENDANT_ROLE=implementation (git worktree)
    └──▶ [Testing agent]         — INTENDANT_ROLE=testing
    │
    ▼
Results synthesized, knowledge consolidated, BRIEF narrated to the user
```

`spawn_orchestrator_spec()` (`user_mode.rs`) builds the orchestrator's
`SubAgentSpec`: id `orchestrator`, working dir = project root, result/progress
files under `<sub_agent_dir>/orchestrator/`, and `inherit_memory = true`.

## Agent Roles

Roles are the `SubAgentRole` enum in `sub_agent.rs`:
`Research`, `Implementation`, `Testing`, `Orchestrator`, `LiveAudio`, and
`Custom(String)`. The string forms (`research`, `implementation`, `testing`,
`orchestrator`, `live_audio`) round-trip through `INTENDANT_ROLE`; any other value
becomes `Custom`.

Prompt resolution (`prompts.rs::resolve_system_prompt[_for_tools]`) always loads
the base prompt first, then **appends** a role-specific prompt for three roles
only:

| Role | Role prompt appended | Focus |
|------|----------------------|-------|
| `orchestrator` | `SysPrompt_orchestrator.md` | Decomposition, delegation, knowledge routing, checkpointing, synthesis |
| `research` | `SysPrompt_research.md` | Reading, browsing, grep/find, synthesizing findings |
| `implementation` | `SysPrompt_implementation.md` | Writing code, builds/tests, committing to a worktree branch |
| `testing` | *(none — base prompt only)* | Validation, test execution, coverage |
| `live_audio` | *(handled separately)* | Voice/phone sessions ([Computer Use & Live Audio](./computer-use-and-audio.md)) |
| `custom` | *(none — base prompt only)* | User-supplied prompt via `INTENDANT_SYSTEM_PROMPT` |

There is intentionally **no `SysPrompt_testing.md`**: the testing role runs on the
unmodified base prompt. When the provider uses native tool calling (the default),
the condensed `SysPrompt_tools.md` is the base instead of `SysPrompt.md` (the
schema-heavy variant); the role addition is identical either way. Prompts also
have `{{PLATFORM}}` / `{{PLATFORM_DETAILS}}` placeholders substituted for the host
OS so the worker knows which tools (xdotool vs. cliclick vs. native Windows, etc.)
are available.

A project may override any prompt by placing a file of the same name at the
project root; `resolve_prompt()` prefers the project copy and falls back to the
binary's embedded default.

## Sub-Agent Spawning

The orchestrator spawns a worker by emitting an `exec_command` (`execAsAgent`)
tool call that runs the **same `intendant` binary** with role environment
variables prefixed. `build_spawn_command()` (`sub_agent.rs`) produces it,
shell-escaping every field:

```
cd <working_dir> && \
  INTENDANT_ROLE=research \
  INTENDANT_ID=research-1 \
  INTENDANT_RESULT_FILE=<.../result.json> \
  INTENDANT_PROGRESS_FILE=<.../progress.json> \
  INTENDANT_INHERIT_MEMORY=1 \
  <intendant_path> '<task>'
```

| Variable | Purpose |
|----------|---------|
| `INTENDANT_ROLE` | Role string; its presence is what triggers Sub-Agent mode |
| `INTENDANT_ID` | Unique id for this agent (defaults to `unnamed`) |
| `INTENDANT_RESULT_FILE` | Path the child writes its final `SubAgentResult` JSON to |
| `INTENDANT_PROGRESS_FILE` | Path the child writes periodic `SubAgentProgress` JSON to |
| `INTENDANT_INHERIT_MEMORY` | Present (`=1`) → child loads the project knowledge store at start |
| `INTENDANT_SYSTEM_PROMPT` | Optional inline prompt for a `Custom` role |

The task itself is passed as the trailing CLI argument, not an env var. A
sub-agent always runs at `AutonomyLevel::Full` with no interactive frontend
(headless) and no MCP client of its own — it is a leaf worker.

## Progress and Result IPC

Parent and child communicate entirely through **files** on disk (no shared
memory, no sockets). The shapes are defined in `sub_agent.rs`.

### Progress (`progress.json`)

The child writes `SubAgentProgress` periodically; the parent polls it.

```json
{
  "id": "research-1",
  "turn": 5,
  "status": "running",
  "last_action": "Running cargo test",
  "question": null
}
```

`format_progress_for_user()` (`user_mode.rs`) renders this for the user layer as
`[Status: turn 5, running] Running cargo test`, truncating `last_action` to 100
chars. If `question` is set, it is appended as a question from the orchestrator —
this is how a blocked child surfaces a clarification request up the chain.

### Result (`result.json`)

On completion the child writes a `SubAgentResult`:

```json
{
  "id": "research-1",
  "status": "Completed",
  "summary": "Found 3 relevant API endpoints…",
  "brief": "Found the API surface; pagination is supported.",
  "findings": ["endpoint /api/users supports pagination", "…"],
  "artifacts": ["docs/api-analysis.md"],
  "usage": { "prompt_tokens": 12000, "completion_tokens": 3000, "total_tokens": 15000 }
}
```

`status` is `Completed` or `Failed(reason)`. `brief` is the one-line spoken
summary the worker emits as its final `BRIEF:` line (narrated by the
[presence layer](./presence.md)). The orchestrator collects results with
`scan_completed_results()`, which writes a `.reported` marker beside each
`result.json` so the same result is surfaced exactly once even across repeated
polls. `format_result_message()` turns a result into the text injected back into
the orchestrator's conversation.

## Git Worktree Isolation

Implementation agents work in isolated git worktrees so parallel workers never
collide in the working tree. The helpers live in `worktree.rs` (with a richer
inventory/bookkeeping layer in `worktree_inventory.rs`):

- **Create** — `worktree::create(root, branch, base)` runs
  `git worktree add -b <branch> <root>/.intendant/worktrees/<branch> <base>`.
  Each worker gets its own branch and a checkout under
  `.intendant/worktrees/<branch>`.
- **Merge** — `worktree::merge(root, wt, target)` runs
  `git merge <branch> --no-edit`. On success it returns `MergeResult::Clean`; on
  failure it runs `git merge --abort` to leave the repo clean and returns
  `MergeResult::Conflict(details)`. Conflicts are never auto-resolved — they are
  reported back so the orchestrator can reassign or escalate.
- **Remove** — `worktree::remove(root, wt)` runs `git worktree remove <path>`
  then `git branch -D <branch>` to clean up.

```
implementation-1 ─► branch impl-1 ─┐
implementation-2 ─► branch impl-2 ─┼─► orchestrator merges each (--no-edit)
                                   │     clean  → keep
                                   └──► conflict → abort + report
```

This lets several implementation agents develop independent slices of a change at
once, then fold them back one branch at a time.

## Knowledge Routing Between Agents

Agents share findings through the **knowledge store** — a tagged, pub/sub-capable
JSON file at `<project>/.intendant/memory.json`, manipulated through the runtime's
`store_memory` / `recall_memory` tools (see
[Runtime Protocol](./runtime-protocol.md#knowledge-system) for the on-disk format
and the legacy↔tagged migration).

- **Publish** — a worker stores a finding on a named *channel* with *tags* and a
  *source* (e.g. channel `findings`, tags `database,config`, source `research-1`).
- **Inherit** — a child spawned with `INTENDANT_INHERIT_MEMORY=1` loads the store
  at session start, so prior findings are already in its context.
- **Route** — the orchestrator forwards relevant findings to the worker that needs
  them; recall filters (channel/tags/source/`since`) let an agent pull just the
  slice it cares about.
- **Cursor tracking** — the tagged store records per-subscription cursors so an
  agent only ever sees entries newer than what it has already consumed.

### Example flow

1. Research agent discovers the DB config → `store_memory` on channel `findings`,
   tag `database`, source `research-1`.
2. Orchestrator routes that finding to the implementation agent.
3. Implementation agent `recall_memory` with `channel=findings, tags=database`
   pulls the config and writes code against it.

## Orchestrator Checkpointing

Long orchestrations outlive their context window. To survive auto-compaction the
orchestrator writes a **project-state checkpoint** after each worker finishes,
using `store_memory` on the `project_state` channel (key `project_state`, tag
`checkpoint`). The checkpoint captures completed tasks, active tasks,
architectural decisions, and discovered constraints.

`write_project_state()` (`sub_agent.rs`) also persists the checkpoint to disk in
the orchestrator's directory as both `project_state.json` (machine-readable) and
`project_state.md` (human-readable). On a context restart the orchestrator's
prompt directs it to `recall_memory` the latest `project_state` first, restoring
awareness of what is done and what remains.

> The orchestrator prompt instructs checkpointing *before context reaches ~60%
> usage*. (Earlier docs cited ~90%; the shipped `SysPrompt_orchestrator.md`
> threshold is 60%.)

## Configuration

Orchestration is tuned under `[orchestrator]` in `intendant.toml`
(`OrchestratorConfig` in `project.rs`):

```toml
[orchestrator]
max_parallel_agents = 4                  # cap on concurrent sub-agents (Option; unset = unbounded)
sub_agent_dir = ".intendant/subagents"   # workspace root for sub-agent result/progress dirs
```

Both keys are optional. When `sub_agent_dir` is unset, `Project::sub_agent_dir()`
defaults to `<project>/.intendant/subagents`. Knowledge sharing can be disabled
entirely with `[memory] enabled = false` (see
[Runtime Protocol](./runtime-protocol.md#knowledge-system)).

To skip orchestration for a single run, pass `--direct`. For the daemon-managed,
multi-session story — running and supervising several agents (native or external)
concurrently from one always-on process — see
[External-Agent Orchestration](./external-agent-orchestration.md) and the
control-plane/daemon chapter ([control plane & daemon](./control-plane-and-daemon.md)).
