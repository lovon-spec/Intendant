# Presence Layer

The presence layer is the conversational front-end of Intendant. The user talks
to *presence*, not directly to the worker agent. Presence speaks as Intendant in
the first person ("I'm working on that now", not "the agent is working on
that"), decides whether to answer directly or delegate work, dispatches tasks to
the agent loop, narrates progress as events stream back, and mediates approvals
and questions on the user's behalf.

## Why a Separate AI

Presence is a distinct model with its own conversation, system prompt, tool set,
and token budget — **not** a chat wrapper around the worker agent. Two reasons:

1. **Latency and cost.** Presence runs a small, fast model (a Gemini Flash by
   default). It can answer "what's the status?" or narrate a phase change
   instantly without burning the heavy coding model's budget or context.
2. **Separation of concerns.** The worker agent focuses on the task; presence
   focuses on the human relationship — interjections, approvals, recall, and
   keeping a continuous conversational thread across many tasks.

Presence dispatches work to the agent loop the same way a human frontend does:
by emitting a [`ControlMsg`](./integrations.md) onto the EventBus (see
[Tool Dispatch](#tool-dispatch) below). It never executes commands itself.

## Two Modes

Presence runs in one of two modes:

- **Server-side text presence** (`src/bin/caller/presence.rs`) — the default.
  A native Rust `PresenceLayer` driving a text model. Used by the TUI and by the
  web dashboard when no browser voice session is connected.
- **Browser-side live presence** (`crates/presence-web/`, WASM) — when a browser
  connects a live voice/realtime model (Gemini Live or OpenAI Realtime), that
  model becomes the conversational front-end directly from the browser.

These are **not strictly mutually exclusive**. Server-side narration is
*ref-count paused* while one or more browsers hold an active voice session, and
resumes automatically when they disconnect. The same `presence-core` logic
(tool definitions, dispatch, prompt, event formatting) backs both modes, so the
behavior is identical whichever model is driving.

```
                        ┌──────────────────────────────────┐
   user (text/voice) ──▶│            Presence               │
                        │  (text model OR browser voice)    │
                        └──────────────┬───────────────────┘
                                       │ submit_task / approve / …
                                       │ → ControlMsg on EventBus
                                       ▼
                              ┌──────────────────┐
                              │   Agent loop /    │
                              │ session supervisor│
                              └────────┬─────────┘
                                       │ filtered PresenceEvents
                                       ▼
                          narration back to the user
```

## Server-Side Text Presence

`PresenceLayer` (`src/bin/caller/presence.rs`) wraps a `ChatProvider` and keeps
its own `Conversation`, separate from the agent's. Its responsibilities:

- **`process_user_input()`** — runs the model on a user message; the model
  decides whether to respond directly or call a tool (e.g. `submit_task`).
- **`handle_event()`** — receives a filtered `PresenceEvent` from the agent loop
  and lets the model narrate it. Phase-change narrations are debounced
  (`NARRATION_DEBOUNCE_MS` = 500 ms) so rapid phase flips don't spam the user.
- **`run()`** — the loop: `select!` over user input and incoming events.

Each turn runs `run_model_loop()`, which calls the model, dispatches any tool
calls through `presence-core`, feeds results back, and repeats up to 10 tool
rounds before returning text. Token usage is accumulated and emitted as
`PresenceUsageUpdate` events so the dashboard can show presence's own cost.

### Model and Configuration

The text model is chosen by `provider::select_presence_provider()`. Default
selection (no config, no env): auto-detect, **preferring Gemini** when a key is
present. Per-provider defaults:

| Provider  | Default model                | Constant / literal                       |
|-----------|------------------------------|------------------------------------------|
| gemini    | `gemini-3-flash-preview`     | `presence_core::DEFAULT_TEXT_MODEL`      |
| anthropic | `claude-haiku-4-5-20251001`  | literal in `select_presence_provider`    |
| openai    | `gpt-4.1-mini`               | literal in `select_presence_provider`    |

> **Note:** `DEFAULT_TEXT_PROVIDER` is `"gemini"`. The default text model is
> `gemini-3-flash-preview` — earlier docs that said `gemini-3.0-flash` or
> `gemini-2.5-flash` were stale (the `gemini-2.5-flash` string still appears in
> a doc-comment on `PresenceConfig::model` in `crates/presence-core/src/types.rs`
> but is not the value actually used).

```toml
[presence]
enabled = true                # default: true (disable with --no-presence)
provider = "gemini"           # optional; default auto-detect (prefers gemini)
model = "gemini-3-flash-preview"   # optional; default per table above
context_window = 1048576      # default: 1_048_576
```

Environment overrides (take precedence over auto-detect, below explicit config):
`PRESENCE_PROVIDER` and `PRESENCE_MODEL`. API keys are read from
`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` (or the bare
`OPENAI` / `ANTHROPIC` / `GEMINI` variants).

Disable presence with the `--no-presence` flag or `[presence] enabled = false`.
If no API key is available for presence, Intendant degrades gracefully: it runs
without narration, and dashboard chat / tasks dispatch directly to the worker.

## Browser-Side Live Presence

When the dashboard is open with voice enabled, the browser connects directly to
Gemini Live or OpenAI Realtime (the API key lives in browser `localStorage` and
is **never sent to the Intendant server**). The browser's voice model becomes
presence, calling the exact same tools over the WebSocket tool request/response
protocol.

The default live models (browser-side, in `crates/presence-web/src/lib.rs`):

| Provider | Default live model                                  |
|----------|-----------------------------------------------------|
| gemini   | `gemini-2.5-flash-native-audio-preview-12-2025`     |
| openai   | `gpt-4o-realtime-preview`                            |

```toml
[presence]
live_provider = "gemini"
live_model = "gemini-2.5-flash-native-audio-preview-12-2025"
live_context_window = 32768   # default: 32_768
```

### Ref-Counted Pause (not mutual exclusion)

The server-side `PresenceLayer` holds an `Arc<AtomicUsize>` *pause counter*
(`PresenceLayer::paused`, exposed via `paused_flag()` / `is_paused()`). When a
browser starts an active voice session, the counter is incremented; when it
ends, it is decremented. While the counter is `> 0`:

- `process_user_input()` returns immediately with an empty string (input is
  dropped — the browser voice model is handling the user).
- `handle_event()` returns `Ok(None)` (no server narration).

This is a *ref count*, not a boolean, precisely because multiple browsers may be
connected. The browser voice model dispatches tasks directly via the same task
channel, so nothing is lost — the server is simply quiet. Earlier docs called
this "mutual exclusion / never both simultaneously"; the accurate description is
a ref-counted pause.

The wire handshake (handled in `web_gateway.rs`):

```
1. Browser connects voice  → {"t":"presence_connect"}
2. Gateway emits AppEvent::PresenceConnected → pause counter incremented
3. Server replies          → {"t":"presence_welcome", state, events, summary}
4. Browser voice model is now presence (narration, tools, user interaction)
5. Browser disconnects      → {"t":"presence_disconnect"}
6. Gateway emits AppEvent::PresenceDisconnected → pause counter decremented
```

### Active / Passive Multi-Browser

Only one browser connection can be **active** (controlling the voice model) at a
time; others are passive observers:

- **Active browser** — increments the pause counter, receives tool responses,
  drives the voice session.
- **Passive browsers** — receive TUI frames and events but do not pause the
  server presence.
- **Handover** — a passive browser sends `{"t":"make_active"}` to take over; the
  gateway force-disconnects the previous active browser and grants the new one
  active status with handover context. (Voice reconnect after handover does not
  double-count the pause — the gateway tracks who is already active.)

### Session Continuity

The presence session protocol (`PresenceSession`, `PresenceEventWindow` in
`presence-core`) maintains voice context across reconnects:

1. The server keeps a bounded ring of sequenced events
   (`PresenceEventWindow`, default capacity 200).
2. Browsers periodically send `{"t":"presence_checkpoint"}` with a conversation
   summary and `last_event_seq`.
3. On reconnect, `presence_welcome` carries every event since `last_event_seq`
   plus the last checkpoint summary, so the voice model resumes mid-thread.

## Presence Tools

Presence has **12 tools**, defined once in `crates/presence-core/src/tools.rs`
(`presence_tools()`) and shared by both modes. The main crate re-exports them
and converts the provider-agnostic `ToolDefinition` to the provider-specific
format.

### Action tools (mutate state via `ControlMsg`)

| Tool                  | Effect |
|-----------------------|--------|
| `submit_task`         | Submit a task to the agent loop. Optional `force_direct`, `context_hints`, `reference_frame_ids`, `display_target`. |
| `approve_action`      | Approve a pending action by `id`. |
| `deny_action`         | Deny a pending action by `id` (stops the command). |
| `skip_action`         | Skip a pending action by `id` (continue with the next). |
| `respond_to_question` | Answer an `askHuman` question (`text`). |
| `set_autonomy`        | Change the autonomy level (`low`/`medium`/`high`/`full`). |
| `send_message`        | Mid-task interjection injected into the running worker's conversation at its next turn. Optional `frame_ids` to attach HQ images. |

### Query tools (read-only, server I/O)

| Tool            | Effect |
|-----------------|--------|
| `check_status`  | Read the current `AgentStateSnapshot` — phase, turn, budget, last command/output, workers, pending approval, and **`available_displays`**. |
| `query_detail`  | Detailed lookups by `scope`: `current_turn`, `last_output`, `worker`, `diff`, `logs`, `file` (needs `target`), `task_result`. |
| `recall_memory` | Search the tagged knowledge store (by `keywords`, optional `tags`/`channel`); falls back to the session log. |

### Video / frame tools (read-only, server I/O)

| Tool             | Effect |
|------------------|--------|
| `inspect_frame`  | Fetch the HQ version of a frame (`frame_id`, or latest if omitted); the image is injected into context after the response. |
| `inspect_frames` | Search past frames by time range / stream / description; returns metadata only (no images). |

Display state is **pull-based**: presence learns about displays via
`check_status` → `available_displays`, not from proactive display-event
narration. Frame IDs surfaced by `inspect_frames`/`inspect_frame` can be passed
back into `submit_task` via `reference_frame_ids` to give the worker (and the
[CU runner](./computer-use-and-audio.md)) visual context about what the user was
looking at.

## Tool Dispatch

Dispatch is a two-stage design that keeps `presence-core` free of I/O so it can
compile to WASM. `presence_core::dispatch_tool_call(name, args, state)` returns
a pure `PresenceAction` enum:

```
tool call (text model OR browser voice model)
        │
        ▼
 dispatch_tool_call()  →  PresenceAction
        │
        ├── TextResult(text)        — resolved locally, returned immediately
        │       └── check_status is computed purely from the state snapshot
        ├── SubmitTask(TaskEnvelope)
        ├── Approve { id } / Deny { id } / Skip { id }
        ├── Respond { text }
        ├── SetAutonomy { level }
        └── NeedsIO { tool_name, args }  — platform must do I/O:
                 query_detail · recall_memory · send_message
                 · inspect_frame · inspect_frames
```

On the native side, `presence::action_to_control_msg()` turns the
state-mutating variants into `ControlMsg`s on the EventBus — the **same path** a
TUI keypress or control-socket command takes:

| `PresenceAction`     | `ControlMsg`         |
|----------------------|----------------------|
| `SubmitTask`         | `StartTask { … }`    |
| `Approve`            | `Approve { id }`     |
| `Deny`               | `Deny { id }`        |
| `Skip`               | `Skip { id }`        |
| `Respond`            | `Input { text }`     |
| `SetAutonomy`        | `SetAutonomy { level }` |

`TextResult` and `NeedsIO` have no `ControlMsg` — `TextResult` is returned to the
model directly, and `NeedsIO` is executed by the platform's I/O handler
(`handle_tool_query()` in `presence.rs`, or a server round-trip in the browser).
Note that `send_message` is a `NeedsIO` tool (it injects into the worker's
context-injection queue), not a direct `ControlMsg`.

`submit_task` is guarded: if the agent is **busy** (not idle / not in a
follow-up-ready state), a new `submit_task` is rejected to stop the presence
model from hallucinating an unrelated task over active work. Idle, completed,
and follow-up-waiting states accept it — which is exactly how follow-up dispatch
works.

## Event Filtering

`filter_event()` (`presence.rs`) decides which `AppEvent`s become a narratable
`PresenceEvent`. Roughly:

**Narrated:** `TaskSubmitted`/task start, `TaskComplete`, `ApprovalRequired`,
`HumanQuestion`, `PhaseChanged` (debounced), `RoundComplete`, budget warnings,
errors, display-grant/-revoke. `PresenceConnected`/`PresenceDisconnected` are
*not* narrated.

**Not narrated (pull-only):** raw agent output, status snapshots, token-usage
updates, and display readiness — these are available on demand via
`check_status` / `query_detail`.

## The `presence-core` Crate

`crates/presence-core/` is the WASM-compatible core. Minimal deps (serde +
serde_json + wasm-bindgen — no tokio/reqwest). Compiles to both native and
`wasm32-unknown-unknown`. Modules:

- **types.rs** — `PresenceConfig`, `TaskEnvelope`, `PresenceEvent`,
  `AgentStateSnapshot`, `PendingApprovalSnapshot`, `PresenceUsage`, the session
  protocol types (`PresenceConnect`, `PresenceWelcome`, `SequencedPresenceEvent`,
  `PresenceCheckpoint`, `PresenceEventWindow`), frame types (`FrameMeta`,
  `VideoState`), and constants (`DEFAULT_TEXT_MODEL`, `DEFAULT_TEXT_PROVIDER`,
  `NARRATION_DEBOUNCE_MS`, `PRESENCE_TURN_OFFSET`). `AgentStateSnapshot::update_from_server_event()`
  is the shared state machine that both native and WASM use to fold server
  events into the snapshot.
- **dispatch.rs** — `PresenceAction` enum + `dispatch_tool_call()` (pure logic)
  and `action_confirmation()`.
- **tools.rs** — the 12 `ToolDefinition`s.
- **format.rs** — `format_event()`, `format_agent_output()`, unicode-safe
  `truncate()`.
- **prompt.rs** — `DEFAULT_PRESENCE_PROMPT` via `include_str!` of
  `crates/presence-core/prompts/SysPrompt_presence.md`.
- **wasm.rs** (`#[cfg(target_arch = "wasm32")]`) — `WasmPresence` object plus
  `get_presence_tools()`, `get_presence_prompt()`, `wasm_truncate()`. Data
  crosses the WASM boundary via `serde-wasm-bindgen`.

The WASM boundary guarantee: because the *same* `dispatch_tool_call`, tool
definitions, prompt, and event formatter are compiled into both the native
binary and the browser bundle, presence behaves identically whether the text
model or the browser voice model is driving.

## The `presence-web` Crate

`crates/presence-web/` is the browser-side WASM layer. Modules (declared in
`crates/presence-web/src/lib.rs`):

- **lib.rs** — the WASM entry point and `#[wasm_bindgen]` surface: voice
  connect/disconnect, voice-provider selection and the default-model fallback
  table, and the WASM↔DOM bridge.
- **app_state.rs** — pure-Rust dashboard state: event routing, log filtering,
  usage tracking, and cost calculation (includes a per-model pricing table for
  OpenAI / Anthropic / Gemini). Methods return `Vec<UiCommand>` for a thin JS
  layer to apply to the DOM.
- **server.rs** — the WebSocket connection to the Intendant server and message
  routing.
- **gemini.rs** — Gemini Live (BidiGenerateContent), dual-mode auth (API key +
  ephemeral token).
- **openai.rs** — OpenAI Realtime integration.
- **callbacks.rs** — JS callback management for voice/tool events.

Build (does **not** happen during `cargo build`):

```bash
cd crates/presence-web
wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
cargo build --release -p intendant   # re-embed the compiled WASM
```

The `static/wasm-web/` files are pre-compiled artifacts; any change to
`presence-web/` or `presence-core/` requires the `wasm-pack` step plus a
re-embed build.

## See Also

- [TUI & Autonomy](./tui.md) — how presence narration and approvals surface in
  the terminal UI.
- [Web Dashboard](./web-dashboard.md) — the dashboard host for browser voice.
- [Computer Use & Live Audio](./computer-use-and-audio.md) — where
  `display_target` / `reference_frame_ids` from `submit_task` route.
