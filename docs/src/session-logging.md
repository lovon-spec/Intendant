# Session Logging

## Overview

Every `intendant` invocation gets a structured session log directory. It is the
single source of truth for what happened in a session: a line-per-event JSONL
stream, full per-turn artifacts, the agent's stdout/stderr, file-history
snapshots, and (in headless/web/MCP runs) the controller's own console output.
It serves three audiences: a human debugging after the fact, the dashboard
replaying a session into the browser, and the resume path rehydrating a
conversation to continue work.

The implementation lives in `session_log.rs` (`SessionLog`, ~5k lines). Sessions
are fully isolated — there is no global state file; each session is one
self-contained directory.

## On-Disk Layout

By default each session is a UUID-named directory under `~/.intendant/logs/`
(verified in `SessionLog::resolve_path`). `--log-file <DIR>` overrides the
directory outright (used to pin a session to a known path). The controller hands
the chosen directory to the runtime subprocess via the `INTENDANT_LOG_DIR`
environment variable, so per-command stdout/stderr land in the same place.

```
~/.intendant/logs/<uuid>/
├── session_meta.json        # id, created_at, project_root, name, task, status, last_turn, role, rounds
├── session.jsonl            # structured event log — one JSON object per line (the spine)
├── transcript.jsonl         # simplified {ts, role, text, tools_called?} — rebuilt at session end
├── conversation.jsonl       # serialized native Conversation, for --continue / --resume
├── session_summary.json     # accumulated stats (duration, voice, CU tasks, tokens, errors)
├── daemon.log               # controller stdout/stderr tee (web/headless/MCP only; Unix only)
├── human_question           # askHuman IPC: question file (session-scoped)
├── human_response           # askHuman IPC: response file (session-scoped)
├── <nonce>_stdout.log       # runtime stdout for command nonce N  (e.g. 1_stdout.log)
├── <nonce>_stderr.log       # runtime stderr for command nonce N
├── frames/                  # display & camera frame captures
│   ├── frames.jsonl         #   frame manifest (id, stream, timestamp, sent_to_live)
│   └── *.jpg                #   HQ JPEG frames
├── file_snapshots/          # file-watcher rewind/redo history (see Control Plane & Daemon)
│   ├── baseline/            #   initial text-file snapshot
│   ├── objects/             #   content-addressed blobs (sha256-named)
│   ├── rounds/              #   per-round artifacts
│   └── history.json         #   rounds[], abandoned_branches[], current_head_id, next_id
└── turns/
    ├── turn_001_messages.json    # full messages array sent to the API
    ├── turn_001_model.txt        # full model response text
    ├── turn_001_reasoning.txt    # full reasoning content (when the provider returns it)
    ├── turn_001_agent_in.json    # commands sent to the runtime (pretty-printed)
    ├── turn_001_stdout.txt       # agent stdout for this turn
    ├── turn_001_stderr.txt       # agent stderr for this turn (only when non-empty)
    └── turn_001_context_<id>.json # context snapshot, when a context directive fires
```

Turn files are named `turn_{NNN}_{suffix}` with `NNN` zero-padded to three digits
(`write_turn_file` / `append_turn_file`). Per-nonce runtime logs are named
`{nonce}_stdout.log` / `{nonce}_stderr.log` (`agent.rs`).

### `session_meta.json`

```json
{
  "session_id": "a1b2c3d4-...",
  "created_at": "2026-05-24T10:30:00",
  "project_root": "/home/user/myproject",
  "name": "Fix auth bug",
  "task": "Fix the authentication bug",
  "status": "running",
  "last_turn": 5,
  "role": null,
  "rounds": 2
}
```

`name` is an optional user-facing label (see [Session naming](#session-naming-and-aliasing-across-backends));
`role` is set for sub-agent sessions (`orchestrator`, `research`,
`implementation`, `testing`) and is how the resume scan skips them. This file
drives `--continue` (most-recent session for the project) and `--resume <id>`
(by full id or prefix).

## The `session.jsonl` Event Stream

`session.jsonl` is the spine: one `LogEvent` JSON object per line. Each event
carries a timestamp, an optional turn number, the event name, an optional level,
an optional human message, optional structured `data`, and optional `file` /
`file2` references pointing at the full-content turn files (so the line stays
small and the bulk lives in `turns/`).

```rust
struct LogEvent {
    ts: String, turn: Option<usize>, event: String,
    level: Option<String>, message: Option<String>,
    data: Option<serde_json::Value>,
    file: Option<String>, file2: Option<String>,
}
```

The event vocabulary is broad and grows with the system. Grouped by area
(verified against `session_log.rs`):

| Area | Events |
|------|--------|
| Lifecycle | `session_start`, `session_started`, `agent_started`, `turn_start`, `round_complete`, `task_complete`, `done_signal`, `safety_cap_reached`, `session_end`, `session_ended` |
| Model I/O | `messages_input`, `model_response`, `reasoning`, `json_extracted` |
| Runtime | `agent_input`, `agent_output` |
| Approvals | `approval`, `approval_resolved`, `auto_approved`, `human_question`, `human_response_sent` |
| Context | `context_snapshot`, `snapshot_created`, `conversation_rolled_back`, `rolled_back`, `redone`, `history_pruned` |
| Sessions/graph | `session_identity`, `session_relationship`, `session_attached`, `session_capabilities`, `sub_agent_result`, `orchestrator_progress`, `presence_checkpoint` |
| Computer use | `cu_task_start`, `cu_turn`, `cu_task_complete`, `cu_task_error` |
| Display | `display_ready`, `display_taken`, `display_released`, `display_resize`, `debug_screen_ready`, `debug_screen_torn_down` |
| Voice/live | `live_audio_started`, `live_audio_progress`, `live_audio_completed`, `live_usage_update`, `presence_connected`, `presence_disconnected`, `presence_log`, `presence_usage_update` |
| Recording | `recording_started`, `recording_stopped`, `recording_error` |
| Generic | `info`, `debug`, `error`, `tool_request`, `tool_response` |

Each is written by a typed method on `SessionLog` (e.g. `turn_start`,
`model_response`, `agent_input`, `agent_output`, `approval`, `json_extracted`,
`reasoning_content`), not by hand-formatting JSON.

### Querying

```bash
S=~/.intendant/logs/<uuid>

# Event overview
jq -r '.event' "$S/session.jsonl"

# What the model received on turn 5
jq . "$S/turns/turn_005_messages.json"

# Model reasoning on turn 3
cat "$S/turns/turn_003_reasoning.txt"

# Every batch of commands sent to the runtime
grep '"event":"agent_input"' "$S/session.jsonl" | jq -r '.message'

# Approvals and how they resolved
grep -E '"event":"(approval|approval_resolved|auto_approved)"' "$S/session.jsonl" | jq .

# All sessions, newest first
ls -lt ~/.intendant/logs/

# Sessions for one project
grep -l '"project_root":"/home/user/myproject"' ~/.intendant/logs/*/session_meta.json
```

## `transcript.jsonl` and `session_summary.json`

`transcript.jsonl` is a simplified, human-skimmable conversation log
(`{ts, role, text, tools_called?}` per line). It is appended live and then fully
**rebuilt from `session.jsonl` at session end** (`rebuild_transcript`) so it is
complete and consistent even if the live append missed anything (notably voice
tokens, which are buffered into whole utterances before being emitted).

`session_summary.json` is written at session end with accumulated statistics:
duration, voice provider/model and connection/reconnect counts, model-turn
count, computer-use task summaries, frames sent, errors, and total tokens.

## Resume and Rehydration

The native conversation is serialized to `conversation.jsonl` so a session can
be continued:

```bash
# Resume the most recent session for this project
./target/release/intendant --continue "fix that bug"

# Resume a specific session by id or prefix
./target/release/intendant --resume abc123 "continue"
```

On resume, `Conversation::load_from_file(conversation.jsonl, context_window)`
rehydrates the message history, the new task is appended as a
`[Session resumed] Continue with: …` continuation message (with any attachments
folded in), and the loop continues from the rehydrated turn. `session_meta.json`
is updated with the new task.

`conversation.jsonl` is specific to Intendant's **internal** agent. External
backends (Codex / Claude Code / Gemini) own their own conversation history; the
session supervisor resumes those through each backend's native resume token (see
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md) →
`ResumeSession`), keyed by the session `source`.

## Multi-Session and the Session Graph

A persistent daemon (an idle `--web` launch) runs many sessions over its
lifetime, each its own `~/.intendant/logs/<uuid>/` directory. The
`session_supervisor` (see
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md)) creates these
directories on `CreateSession`/`StartTask`, tracks which is active, and records
parent/child relationships (`side`, `subagent`). Those relationships are also
logged into the streams as `session_identity` and `session_relationship` events,
which lets a consumer reconstruct the session tree purely from the logs.

## Session Naming and Aliasing Across Backends

Sessions can be renamed for display, and the same abstraction works whether the
session is an Intendant session or an external backend's. This lives in
`session_names.rs`.

- **Source normalization.** Free-form source strings collapse to a canonical set:
  `intendant`, `codex`, `claude-code`, `gemini` (so `"claude code"`, `"cc"`,
  `"gemini cli"`, etc. all map correctly).
- **Intendant sessions** store the name directly in their own
  `session_meta.json` (`write_intendant_session_name`), located by id or prefix
  under `~/.intendant/logs/`.
- **External backends** get an **overlay**: a single
  `~/.intendant/session_names.json`, keyed `source → { session_id → name }`. When
  the dashboard lists sessions, `apply_session_name_overlays` merges these names
  onto the listed sessions (matching on `session_id` or `resume_id`), without
  touching the backend's own files.
- Names are normalized (whitespace-collapsed, truncated at 180 chars) on both
  write and read.

`ControlMsg::RenameSession` carries `session_id`, optional `backend_session_id`,
optional `source`, and the new `name`; the supervisor dispatches it through this
abstraction. A backend with native rename support can map it to its own protocol;
otherwise the overlay is used.

## The `daemon.log` Controller Tee

When the controller does **not** own a real interactive TTY — i.e. web, headless,
or MCP runs — `daemon_log_tee::install` redirects the controller's own stderr and
stdout into `~/.intendant/logs/<uuid>/daemon.log`, prefixing each line with a
wallclock timestamp, while still mirroring everything to the original terminal.
This captures controller-side `eprintln!`, panics, and tracing that would
otherwise never reach `session.jsonl` (which only records *agent* events). The
dashboard's "Download session report" zip includes `daemon.log` so a tester's
bundle is temporally analyzable by a developer.

This is **Unix-only**: on Windows `install` is a no-op. It is deliberately
**skipped under the interactive TUI**, because ratatui writes escape sequences to
stdout and routing stdout through a pipe would corrupt the display.

## How the Dashboard Consumes the Logs

A browser that connects late does not miss history. The web gateway reads
`session.jsonl` and converts it to a stream of outbound events for the WASM
client (`replay_jsonl_to_outbound_entries` in `web_gateway.rs`):

- The first replayed entry is a `replay_start` marker carrying the
  provider/model/autonomy values scanned from the log (`scan_replay_status`), so
  the dashboard seeds its status bar correctly before any live event arrives.
- Each subsequent line is converted to an `OutboundEvent`-shaped object with its
  original `ts` preserved, so replay reproduces the exact event sequence the
  Activity tab would have shown live.
- External-agent activity replay intentionally includes only user/assistant
  messages (not every internal event).

Live events then continue to stream over the same WebSocket. See
[Web Dashboard](./web-dashboard.md) for the tab structure and
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md) for the event
producers (session supervisor, file watcher) behind the stream.

## Test Coverage

Session logging is exercised by inline `#[cfg(test)]` tests in `session_log.rs`
and `session_names.rs`: turn-file creation and pretty-printing, separate
stdout/stderr files, skipping empty stderr, `json_extracted` function extraction,
reasoning-file writes, span-based chunk reads that avoid re-reading whole turn
files, intendant-meta renames, and external-source overlay application.

Integration tests in `tests/e2e/` spawn the real binary and exercise the full
stack (see [Architecture](./architecture.md)):

- **Tier 1 (JSON mode)** — full-stack exec, approve/deny via stdin, multi-round
  follow-up. No display.
- **Tier 2 (control socket)** — status/usage queries, autonomy change, approve
  via the Unix control socket. Needs Xvfb.
- **Tier 3 (web/voice/WebRTC)** — WebSocket `state_snapshot`,
  `tool_request`/`tool_response`, ANSI terminal frames, WebRTC signaling, and the
  `/debug` endpoint. Voice tests need Firefox, PulseAudio, and espeak-ng.
