# E2E Testing Skill

Run screenshot-free end-to-end tests for intendant. All assertions use structured JSON from programmatic APIs — no vision model needed.

## Prerequisites

```bash
cargo build --release
```

## Three Tiers

### Tier 1 — JSON mode (no display needed)

Tests: basic exec, approval approve/deny, follow-up rounds.

Uses `--json --direct` mode: reads JSONL events from stdout, sends commands on stdin.

```bash
# Run all Tier 1 tests
cargo test --test e2e test_basic -- --nocapture

# Run a specific test
cargo test --test e2e test_basic_exec -- --nocapture
cargo test --test e2e test_approval_approve -- --nocapture
cargo test --test e2e test_approval_deny -- --nocapture
cargo test --test e2e test_follow_up -- --nocapture
```

**Requires**: API key in `.env` (any provider: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`).

**How it works**:
- Spawns `intendant --json --direct --autonomy <level> "task"`
- Reads JSONL lines from stdout: `{"type":"agent_output","data":{"stdout":"...","stderr":"..."}}`
- Sends JSON commands on stdin: `{"action":"approve","id":1}`, `{"action":"deny","id":1}`
- Sends plain text on stdin for follow-up messages
- Asserts on event types and data fields

**JSON stdin commands** (same as control socket ControlMsg):
- `{"action":"approve","id":N}` — approve pending command
- `{"action":"deny","id":N}` — deny pending command
- `{"action":"skip","id":N}` — skip pending command
- `{"action":"approve_all","id":N}` — approve and set autonomy to Full
- `{"action":"input","text":"..."}` — respond to askHuman
- `{"action":"follow_up","text":"..."}` — send follow-up task (or just type plain text)

**JSON stdout event types**:
- `turn_started` — new turn beginning
- `model_response` — full model response with usage
- `model_response_delta` — streaming text delta
- `agent_output` — command execution result (stdout, stderr)
- `approval_required` — command needs approval (id, command_preview, category)
- `human_question` — askHuman question (question text)
- `done` — task complete (message or reason)
- `round_complete` — round finished (round number, turns)
- `budget_warning` / `budget_exhausted` — context budget alerts
- `context_management` — auto-compaction triggered

### Tier 2 — Control socket (needs display for TUI)

Tests: status query, usage query, autonomy change, approve via socket.

```bash
# Set up display first
export DISPLAY=:50
Xvfb :50 -screen 0 1280x720x24 &

# Run Tier 2 tests
cargo test --test e2e test_control_socket -- --nocapture
```

**How it works**:
- Spawns `intendant --control-socket --direct --autonomy <level> "task"` (TUI mode)
- Connects to Unix socket at `/tmp/intendant-<pid>.sock`
- Sends JSON commands, reads JSON events
- Same ControlMsg format as stdin commands

### Tier 3 — Web gateway (needs display + browser for voice)

Tests: WebSocket state_snapshot, tool_request, ANSI frames, /debug endpoint, voice pipeline.

```bash
# Set up display and audio
export DISPLAY=:50
Xvfb :50 -screen 0 1280x720x24 &
pulseaudio --start --exit-idle-time=-1

# Run web tests (no browser needed)
cargo test --test e2e test_web -- --nocapture

# Run voice tests (needs Firefox + espeak-ng + PulseAudio)
cargo test --test e2e test_voice -- --nocapture
```

**Voice test requirements**:
- PulseAudio with virtual mic (auto-created by test)
- espeak-ng and ffmpeg for text-to-speech
- Firefox with remote debugging for browser automation
- `GEMINI_API_KEY` for Gemini Live voice model

**How it works**:
- Spawns `intendant --json --direct --web <port> --autonomy <level> "task"`
- WebSocket connects to `ws://127.0.0.1:<port>/ws`
- First message: `{"t":"state_snapshot","state":{...}}`
- Tool requests: `{"t":"tool_request","id":"...","tool":"check_status","args":{}}`
- Tool responses: `{"t":"tool_response","id":"...","result":"..."}`
- `/debug` endpoint returns agent state JSON (no screenshots needed)

## Interpreting Failures

1. **Timeout waiting for event**: The model may be slow or not producing expected output.
   Check stderr of the intendant process for API errors. Increase timeout if network is slow.

2. **approval_required not received**: Verify `--autonomy low` is set. In `full` mode,
   commands auto-approve.

3. **Connection refused**: Build hasn't completed, or binary not found.
   Run `cargo build --release` first.

4. **Voice tests skipped**: Missing `GEMINI_API_KEY`, Firefox, or PulseAudio.
   Voice tests gracefully skip if infrastructure is unavailable.

5. **Debug state**: Use `curl -s http://localhost:<port>/debug | python3 -m json.tool`
   to inspect live state during debugging.

## Test File Map

| File | Tier | Tests |
|------|------|-------|
| `tests/e2e/harness.rs` | — | IntendantProcess, ControlSocketClient, WsClient, voice helpers |
| `tests/e2e/test_basic.rs` | 1 | basic_exec, approval_approve, approval_deny, follow_up |
| `tests/e2e/test_control_socket.rs` | 2 | status_query, usage_query, autonomy_change, approve_via_socket |
| `tests/e2e/test_web.rs` | 3 | state_snapshot_on_connect, tool_request_check_status, ansi_term_frames, debug_endpoint |
| `tests/e2e/test_voice.rs` | 3 | voice_connection, voice_submit_and_approve |
