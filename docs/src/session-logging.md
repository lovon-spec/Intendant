# Session Logging

## Overview

Each `intendant` invocation creates a structured session log directory at `~/.intendant/logs/<uuid>/`. The log provides full observability for debugging and post-session analysis. No global state files are used — each session is fully isolated.

## Directory Structure

```
~/.intendant/logs/<uuid>/
├── session_meta.json               # Session metadata (id, created_at, project_root, task, status, last_turn)
├── session.jsonl                    # Structured event log (one JSON per line)
├── conversation.jsonl               # Serialized conversation for session resume
├── summary.json                     # Post-session summary (task, outcome, turns)
├── human_question                   # askHuman IPC: question file (session-scoped)
├── human_response                   # askHuman IPC: response file (session-scoped)
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

## Session Metadata

`session_meta.json` contains:

```json
{
  "session_id": "a1b2c3d4-...",
  "created_at": "2025-01-15T10:30:00Z",
  "project_root": "/home/user/myproject",
  "task": "Fix the authentication bug",
  "role": null,
  "status": "running",
  "last_turn": 5
}
```

This file is used by `--continue` (find most recent session for the project) and `--resume <id>` (find session by ID or prefix).

## Event Types in session.jsonl

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
| `context_management` | Auto-compaction or manual context directive |
| `session_end` | Summary with outcome and turn count |

## Querying Logs

```bash
# Overview of a session
cat ~/.intendant/logs/<session>/session.jsonl | jq -r '.event'

# See what the model received on turn 5
cat ~/.intendant/logs/<session>/turns/turn_005_messages.json | jq .

# See model reasoning on turn 3
cat ~/.intendant/logs/<session>/turns/turn_003_reasoning.txt

# Find all commands executed
grep '"event":"agent_input"' ~/.intendant/logs/<session>/session.jsonl | jq -r '.message'

# List all sessions
ls -lt ~/.intendant/logs/

# Find sessions for a specific project
grep -l '"project_root":"/home/user/myproject"' ~/.intendant/logs/*/session_meta.json
```

## Session Resume

Conversation history is saved to `conversation.jsonl` after each turn, enabling session resume:

```bash
# Resume most recent session for this project
./target/release/intendant --continue "fix that bug"

# Resume specific session by ID or prefix
./target/release/intendant --resume abc123 "continue"
```

When resuming, the conversation is loaded from `conversation.jsonl` and the agent continues from where it left off. Session metadata is updated with the new task.

## Test Coverage

The test suite covers both binaries with inline `#[cfg(test)]` modules:

- **Agent binary:** models serialization, error types, process state operations, nonce replacement, path inspection, blocking command execution, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters.
- **Caller binary:** JSON extraction, done signal handling, conversation management with message layer protection, tool call tracking, and auto-compaction, context directives (drop/summarize), error types, project detection, config parsing with approval rules and MCP server config and sandbox config, provider selection with token usage tracking, Responses API support, rate-limit retry with exponential backoff, API key masking, SSE streaming and event parsing, shared message builders, structured output and reasoning controls, role mapping, native tool definitions (11+ tools including MCP client tools, provider conversion formats), tool call batch assembly and result routing (including MCP tool routing), Gemini provider request/response format, sub-agent spawning and result parsing, git worktree lifecycle, user mode orchestration, knowledge pub/sub system, prompt resolution cascade (project root, global config, compiled-in defaults, tools-mode variant) with INTENDANT.md loading, TUI rendering (status bar, log panel, action panel, approval panel, help overlay, layout calculations, orchestrator progress, streaming buffer), autonomy level resolution and command classification, event bus dispatch, theme color thresholds, control socket serialization, session log file creation, model summary formatting, Xvfb display configuration per provider, dynamic display allocation, MCP client tool name parsing and routing, Landlock sandbox config construction, JSON structured output mode, web gateway (WebSocket lifecycle, tool request/response, broadcast, state bootstrap, live connect/disconnect), and presence event filtering.

Integration tests in `tests/e2e/` spawn a real binary and exercise the full stack (see [Architecture](./architecture.md)):

- **Tier 1 (JSON mode)**: Full-stack exec, approval approve/deny via stdin, multi-round follow-up. No display required.
- **Tier 2 (Control socket)**: Status/usage queries, autonomy change, approve via Unix control socket. Requires Xvfb.
- **Tier 3 (Web/Voice)**: WebSocket state_snapshot, tool_request/response, ANSI term frames, /debug endpoint. Voice tests require Firefox, PulseAudio, and espeak-ng.
