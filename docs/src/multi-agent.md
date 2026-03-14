# Multi-Agent Orchestration

Intendant supports multi-agent orchestration where a parent orchestrator decomposes complex tasks into sub-tasks and delegates them to specialized child agents. Each agent runs as a separate `intendant` process with its own context window, system prompt, and session log.

## How It Works

```
User (TUI / MCP / Web)
    │
    ▼
[User Mode] — pure subprocess monitor, zero API calls
    │
    ▼
[Orchestrator Sub-Agent] — decomposes task, coordinates
    ├──▶ [Research Agent]       — investigation, file reading, browsing
    ├──▶ [Implementation Agent] — code writing, builds, tests (git worktree)
    └──▶ [Testing Agent]        — validation, test execution
    │
    ▼
Results merged, knowledge consolidated
```

When a complex task is submitted (and `--direct` is not set), intendant enters **User Mode**: it spawns an orchestrator sub-agent and monitors its progress without making any model API calls itself. The orchestrator then spawns specialized sub-agents as needed.

## Agent Roles

Each sub-agent role has a dedicated system prompt that is appended to the base prompt:

| Role | Prompt | Focus |
|------|--------|-------|
| `orchestrator` | `SysPrompt_orchestrator.md` | Task decomposition, sub-agent management, coordination, checkpointing |
| `research` | `SysPrompt_research.md` | Investigation, file reading, browsing, synthesizing findings |
| `implementation` | `SysPrompt_implementation.md` | Code writing, builds, testing, git worktree isolation |
| `testing` | `SysPrompt_testing.md` | Validation, test execution, coverage |

## Sub-Agent Spawning

Sub-agents are spawned via `tokio::process::Command` with environment variables that configure their behavior:

| Variable | Purpose |
|----------|---------|
| `INTENDANT_ROLE` | Agent role (triggers sub-agent mode) |
| `INTENDANT_ID` | Unique identifier for this agent |
| `INTENDANT_TASK` | Task description |
| `INTENDANT_RESULT_FILE` | Path to write final results |
| `INTENDANT_PROGRESS_FILE` | Path to write periodic progress |
| `INTENDANT_PARENT_KNOWLEDGE` | Path to parent's knowledge store |
| `INTENDANT_INHERIT_MEMORY` | `1` to inherit project memory |

## Progress and Results

### Progress Polling

The parent agent polls each sub-agent's progress file every 500ms. Progress is a JSON file with:

```json
{
  "turn": 5,
  "status": "running",
  "last_action": "Running cargo test",
  "question": null
}
```

Progress updates are relayed to the TUI or stdout as `OrchestratorProgress` events.

### Result Files

When a sub-agent completes, it writes a result JSON file:

```json
{
  "id": "research-1",
  "status": "Completed",
  "summary": "Found 3 relevant API endpoints...",
  "findings": ["endpoint /api/users supports pagination", "..."],
  "artifacts": ["docs/api-analysis.md"],
  "usage": { "tokens_used": 15000, "context_window": 128000 }
}
```

The orchestrator reads result files to synthesize final outcomes and route knowledge between agents.

## Git Worktree Isolation

Implementation agents can work in isolated git worktrees to avoid conflicts with the main working tree:

- **Create**: `worktree.rs` creates a new worktree branch for the agent
- **Merge**: On successful completion, the orchestrator merges the worktree branch back
- **Conflict handling**: If merge conflicts arise, the orchestrator is prompted to resolve them
- **Cleanup**: Worktrees are removed after merge or on failure

This allows multiple implementation agents to work on different parts of the codebase simultaneously without stepping on each other.

## Knowledge Routing

The knowledge system supports inter-agent communication via pub/sub channels:

- **Publishing**: Agents store findings with tagged channels (e.g., `"findings"`, `"decisions"`, `"project_state"`)
- **Subscribing**: The orchestrator sets up subscriptions between agents so they receive relevant knowledge
- **Cursor tracking**: Each subscription tracks which entries have been consumed, ensuring agents only see new knowledge
- **Inheritance**: Sub-agents can inherit the parent's knowledge store via `INTENDANT_INHERIT_MEMORY`

### Example Flow

1. Research agent discovers database configuration → publishes to `"findings"` channel with tag `"database"`
2. Orchestrator routes `"findings"` to implementation agent
3. Implementation agent receives the database config via `recallMemory` with channel filter
4. Implementation agent writes code using discovered config

## Orchestrator Checkpointing

The orchestrator writes project state checkpoints after each sub-agent completes, using `storeMemory` with a `project_state` channel. Checkpoints capture:

- Completed and active tasks
- Architectural decisions made so far
- Constraints and dependencies discovered

This preserves essential context across auto-compaction boundaries — when context is compacted at ~90% usage, the orchestrator can recover state via `recallMemory`.

Checkpoints are also written to disk as both `project_state.json` (machine-readable) and `project_state.md` (human-readable) in the sub-agent directory.

## Configuration

Orchestration behavior can be tuned in `intendant.toml`:

```toml
[orchestrator]
max_parallel_agents = 4                    # max concurrent sub-agents
sub_agent_dir = ".intendant/subagents"     # workspace directory for sub-agents
```

To force single-agent mode and skip orchestration entirely, use the `--direct` flag.
