# Session Logging

## Overview

Each `intendant` invocation creates a structured session log directory at `~/.intendant/logs/<uuid>/`. The log provides full observability for debugging and post-session analysis.

## Directory Structure

```
~/.intendant/logs/<uuid>/
├── session.jsonl                    # Structured event log (one JSON per line)
├── summary.json                     # Post-session summary (task, outcome, turns)
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
```

## Session Management

Each invocation creates an isolated session with a UUID-based directory at `~/.intendant/logs/<uuid>/`. No global state files are used.

- **Process state** is in-memory (ephemeral, per-invocation).
- **Session logs** persist in the session directory.
- **Conversation history** is saved to `conversation.jsonl` for resume support.

Sessions can be resumed:
```bash
./target/release/intendant --continue "fix that bug"     # Resume most recent session for this project
./target/release/intendant --resume abc123 "continue"     # Resume specific session by ID or prefix
```

## Test Coverage

The test suite covers both binaries:

- **Agent binary:** models serialization, error types, process state operations, nonce replacement, path inspection, blocking command execution, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters.
- **Caller binary:** JSON extraction, done signal handling, conversation management with message layer protection, tool call tracking, and auto-compaction, context directives (drop/summarize), error types, project detection, config parsing with approval rules and MCP server config and sandbox config, provider selection with token usage tracking, Responses API support, rate-limit retry with exponential backoff, API key masking, SSE streaming and event parsing, shared message builders, structured output and reasoning controls, role mapping, native tool definitions (11+ tools including MCP client tools, provider conversion formats), tool call batch assembly and result routing (including MCP tool routing), Gemini provider request/response format, sub-agent spawning and result parsing, git worktree lifecycle, user mode orchestration, knowledge pub/sub system, prompt resolution cascade (project root, global config, compiled-in defaults, tools-mode variant) with INTENDANT.md loading, TUI rendering (status bar, log panel, action panel, approval panel, help overlay, layout calculations, orchestrator progress, streaming buffer), autonomy level resolution and command classification, event bus dispatch, theme color thresholds, control socket serialization, session log file creation, model summary formatting, Xvfb display configuration per provider, dynamic display allocation, MCP client tool name parsing and routing, Landlock sandbox config construction, and JSON structured output mode.
