# CLAUDE.md

## Project Overview

**Intendant** — Rust runtime for autonomous AI agents with process lifecycle management. Two binaries:
- **intendant-runtime** — Reads JSON from stdin, executes commands sequentially (blocking), writes results to stdout
- **intendant** — AI integration layer (CLI/TUI/Web/MCP) driving the runtime via OpenAI Responses API, Anthropic Messages API, or Gemini API

## Repository Structure

```
src/
├── main.rs              # intendant-runtime entry point
├── agent.rs             # Runtime functions: execAsAgent, captureScreen, inspectPath, editFile, writeFile, browse, askHuman, execPty, storeMemory, recallMemory, nonce replacement
├── models.rs            # Command, AgentInput, ProcessInfo, ProcessStatus
├── error.rs             # AgentError enum
├── utils.rs             # get_timestamp()
└── bin/caller/
    ├── main.rs          # intendant entry: 3 modes (user/sub-agent/direct), budget loop, TUI init
    ├── event.rs         # EventBus, AppEvent, ControlMsg, ApprovalRegistry, ContextInjectionQueue
    ├── types.rs         # Phase, LogLevel, Verbosity, OutboundEvent, format_model_summary()
    ├── provider.rs      # Multi-provider API client, structured output, reasoning, streaming, retry
    ├── conversation.rs  # Message management, layer protection, budget tracking, auto-compaction
    ├── agent_runner.rs  # Spawns runtime subprocess, hard timeout, optional Landlock sandboxing
    ├── knowledge.rs     # Tagged knowledge store, pub/sub, cursor-based routing
    ├── sub_agent.rs     # Sub-agent spawning, result/progress I/O, role-specific config
    ├── worktree.rs      # Git worktree management for isolated agents
    ├── user_mode.rs     # User-mode orchestrator spawning, progress monitoring
    ├── prompts.rs       # System prompt resolution: include_str! + 3-layer cascade + INTENDANT.md
    ├── project.rs       # Git root detection, intendant.toml parsing
    ├── autonomy.rs      # Autonomy levels, action categories, approval rules, command classification
    ├── control.rs       # Unix control socket (/tmp/intendant-<pid>.sock), JSON-line protocol
    ├── frontend.rs      # Shared frontend contract (UserAction, StatusSnapshot, ModelUsageSnapshot)
    ├── tools.rs         # 13 native tool definitions, provider format conversion, MCP tool registration
    ├── tool_batch.rs    # Tool call batch assembly: runtime vs caller-handled vs MCP routing
    ├── computer_use.rs  # CU abstraction: DisplayBackend (X11/Wayland/macOS), DisplayTarget, CuAction executor
    ├── platform.rs      # Cross-platform helpers: process_alive(), process_cmdline(), current_uid()
    ├── presence.rs      # PresenceLayer, tool dispatch, query functions, event filtering, session protocol
    ├── mcp.rs           # MCP server (rmcp, stdio transport, hot-reload via exec()), 22 control tools
    ├── mcp_client.rs    # MCP client: connects to external servers, discovers/proxies tools
    ├── sandbox.rs       # Landlock filesystem sandboxing (Linux 5.13+, no-op on macOS)
    ├── vision.rs        # Display management: Xvfb/x11vnc (Linux), native display (macOS), orphan reclaim
    ├── audio_routing.rs # Virtual audio bridge: PulseAudio (Linux), BlackHole (macOS)
    ├── recording.rs     # Segmented video recording: x11grab (Linux), avfoundation (macOS)
    ├── debug.rs         # Debug screen: Xvfb+Firefox (Linux), native browser (macOS)
    ├── live_audio.rs    # Live audio sessions: Gemini Live / OpenAI Realtime via virtual audio bridge
    ├── live_audio_types.rs # LiveAudioSpec, LiveAudioResult, provider enums
    ├── frames.rs        # FrameRegistry: video frame capture, inspection for presence tools
    ├── quarantine.rs    # Untrusted live audio content quarantine and review
    ├── schema_validator.rs # Tool call response schema validation
    ├── app_state_pricing.rs # Session cost estimation for web dashboard
    ├── skills.rs        # SKILL.md discovery (YAML frontmatter), catalog formatting
    ├── transcription.rs # Whisper API transcription, WAV encoding, silence detection
    ├── web_gateway.rs   # HTTP/WebSocket server, presence protocol, VNC proxy, session replay
    ├── session_log.rs   # UUID session dirs, event logging, conversation persistence
    ├── error.rs         # CallerError enum
    └── tui/
        ├── mod.rs       # Terminal init/restore, render loop
        ├── app.rs       # App state machine, event dispatch, approval/input modes
        ├── event.rs     # crossterm adapter, askHuman file monitor
        ├── web.rs       # WebTui: per-connection buffer backend, ANSI→WebSocket, key parsing
        ├── widgets.rs   # StatusBar, LogPanel, ActionPanel, InputPanel, ApprovalPanel, etc.
        ├── layout.rs    # Panel sizing, responsive constraints
        ├── markdown.rs  # Markdown-to-ratatui renderer
        └── theme.rs     # Catppuccin Mocha color scheme
crates/
├── presence-core/       # WASM-compatible crate: types, tools (12), dispatch, format, prompt, session protocol
└── presence-web/        # Browser WASM: app dashboard state, Gemini Live, OpenAI Realtime, WebSocket transport
static/
├── app.html             # 4-tab web dashboard (Activity, Usage, Terminal, Displays)
├── audio-processor.js   # AudioWorklet for mic capture (PCM16)
└── wasm-web/            # Compiled WASM + JS glue
scripts/                 # Setup scripts: setup-macos.sh, setup-lan.sh, setup-lan-macos.sh, setup-lan-guest-macos.sh
SysPrompt*.md            # System prompts (direct, tools, user, orchestrator, research, implementation, presence, live audio)
docs/src/                # mdBook documentation
tests/e2e/               # Integration tests (real API calls, 3 tiers)
skills/                  # SKILL.md files (tui-e2e, web-e2e, voice-e2e, recording-e2e)
```

## Build and Run

```bash
cargo build --release     # Produces target/release/intendant-runtime and target/release/intendant
cargo build               # Debug build
cargo check               # Type-check only
```

### WASM crate (presence-web)
```bash
cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
cargo build --release -p intendant   # Re-embed WASM
```
**Important**: `cargo build` alone does NOT rebuild WASM. Any change to `crates/presence-web/` or `crates/presence-core/` requires the wasm-pack step above, then re-embed. The `static/wasm-web/` files are pre-compiled artifacts.

### CLI usage (requires `.env` with API key)
```bash
./target/release/intendant "task"                          # Default mode
./target/release/intendant --no-tui "task"                 # Headless
./target/release/intendant --direct "task"                 # Single-agent (skip orchestrator)
./target/release/intendant --json "task"                   # JSONL output (implies --no-tui)
./target/release/intendant --provider anthropic --model claude-sonnet-4-6-20250929 "task"
./target/release/intendant --autonomy low "rm /tmp/test"   # Ask before every command
./target/release/intendant --continue "fix that bug"       # Resume most recent session
./target/release/intendant --resume abc123 "continue"      # Resume by session ID
./target/release/intendant --mcp "task"                    # MCP server on stdio
./target/release/intendant --web                           # Web dashboard on port 8765
./target/release/intendant --sandbox "task"                # Landlock sandboxing (Linux)
./target/release/intendant --control-socket "task"         # Unix control socket
./target/release/intendant --no-presence "task"            # Disable presence layer
echo "task" | ./target/release/intendant                   # Auto-detects non-TTY → headless
```

### Runtime
```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' | ./target/release/intendant-runtime
```

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys)
cargo test -- --list      # List all tests
```

Unit tests: inline `#[cfg(test)]` modules, `#[tokio::test]` for async, `tempfile` for filesystem isolation.

Integration tests (`tests/e2e/`): spawn real binary, make real API calls (costs tokens, non-deterministic). **Not for CI.**

```bash
cargo build --release
cargo test --test e2e test_basic -- --nocapture           # Tier 1: --json mode, no display
cargo test --test e2e test_control_socket -- --nocapture  # Tier 2: control socket, needs Xvfb
cargo test --test e2e test_web -- --nocapture             # Tier 3: WebSocket, needs Xvfb
cargo test --test e2e test_voice -- --nocapture           # Tier 3: needs Xvfb + Firefox + PulseAudio
```

Tier 2/3 use Xvfb on `:50` with x11vnc on port 5950 for VNC monitoring.

## Architecture

### Execution Modes

`intendant` operates in three modes:

1. **Sub-Agent Mode** (`INTENDANT_ROLE` set): Scoped task, role-specific prompt, writes progress/results to files
2. **User Mode** (complex task): Pure subprocess monitor — zero API calls, spawns orchestrator child, polls progress
3. **Direct Mode** (`--direct` or simple task): Single-agent loop — select provider → load config → inject prompts/skills/knowledge → budget-aware loop (stops at context exhaustion, `done` signal, or 500-turn cap)

### Process State & Sessions

Runtime process state: in-memory `HashMap<u64, ProcessInfo>` behind `Arc<RwLock<...>>`, ephemeral per invocation.

Sessions: UUID-based dirs at `~/.intendant/logs/<uuid>/` containing `session_meta.json`, `session.jsonl`, `conversation.jsonl`, `human_question`/`human_response` IPC files, `turns/` dir. Resume via `--continue` or `--resume <id>`.

### Nonce Variables

`$NONCE[id]` in commands → replaced with PID of process launched by that nonce via `replace_nonce_refs()`.

### Autonomy System

Three layers: global level (`--autonomy` Low/Medium/High/Full, +/- in TUI) → category rules (`[approval]` in intendant.toml, per-category Auto/Ask/Deny) → per-action approval (y/s/a/n in TUI).

Categories: FileRead, FileWrite, FileDelete, CommandExec, NetworkRequest, Destructive, HumanInput, LiveAudioSpawn, DisplayControl. Shell commands classified by inspecting for destructive patterns, network tools, file writes. `sudo` detected as Destructive. `DisplayControl` uses session-grant model (approve once, revoke anytime via `d` hotkey).

### Provider Features

**All providers**: streaming via `chat_stream()` (SSE parsing), rate-limit retry (exponential backoff, 5 retries for 429/5xx), prompt caching, computer use action parsing.

**OpenAI**: structured output (JSON object mode for gpt-5+/o3/o4, `STRUCTURED_OUTPUT` env), reasoning controls (`REASONING_EFFORT`, `REASONING_SUMMARY`), done signal via `{"commands":[],"done":true}`.

**Anthropic**: `prompt-caching-2024-07-31` beta header, `cache_control` on system content.

**Gemini**: `streamGenerateContent?alt=sse`, implicit caching.

### Computer Use

Provider-agnostic CU abstraction (`computer_use.rs`). `DisplayBackend` detects X11/Wayland/macOS; `DisplayTarget` distinguishes `Virtual { id }` from `UserSession`. CU-first routing sends display tasks to a fast model before escalating to the heavy agent.

On Linux: xdotool (input), ImageMagick `import` (screenshots), Xvfb (virtual display).
On macOS: cliclick (input), `screencapture` (screenshots), native display.

### Display Management

**Linux**: Xvfb auto-launched lazily on first `execAsAgent`/`captureScreen` when no accessible display exists. Prefers `:99` (VNC port 5999), reclaims orphaned Xvfb processes. x11vnc co-process launched if available. Both killed on drop via `XvfbGuard`.

**macOS**: Native display always accessible, no virtual display needed. `is_display_accessible()` returns true, `find_free_display()` returns 0 as sentinel.

### Audio Routing

Virtual audio bridge for `spawn_live_audio` (voice calls through third-party apps). Linux: PulseAudio null sinks (`pactl`/`parec`/`pacat`). macOS: BlackHole 2ch + 16ch with SwitchAudioSource + sox. Optional — browser-based voice (Gemini Live / OpenAI Realtime via WebRTC) works without it.

### Live Audio

`spawn_live_audio` tool connects to Gemini Live or OpenAI Realtime APIs via WebSocket, pipes audio through the virtual audio bridge. Untrusted: zero tools, zero file access. Responses validated against a `ResponseSchema`; unexpected content quarantined (`quarantine.rs`). Whisper transcription runs in parallel for app-side audio.

### Recording

Segmented MP4 recording via ffmpeg. Linux: `x11grab`. macOS: `avfoundation`. Frame-fed mode (`image2pipe`) for browser camera streams. `RecordingRegistry` tracks active streams, auto-starts on `DisplayReady` events, stops agent streams on task completion.

### TUI

ratatui-based UI with panels: StatusBar, ActionPanel (phase + spinner), LogPanel (scrollable, markdown), ApprovalPanel, InputPanel (tui-textarea), FollowUpPanel, HelpOverlay. Agent loop communicates via `EventBus` (`mpsc` channel of `AppEvent`).

### Control Socket & JSON Mode

Control socket at `/tmp/intendant-<pid>.sock`: JSON-line protocol. Broadcasts events to all clients. Peer UID verification on Unix.

`ControlMsg` variants: Approve, Deny, Skip, ApproveAll, Input, SetAutonomy, Quit, StartTask (with context_hints, reference_frame_ids, display_target), FollowUp, TakeDisplay, InvokeSkill, debug screen commands.

`--json` mode: JSONL to stdout, stdin accepts plain text or `ControlMsg` JSON. Fully interactive without TUI.

### Auto-Compaction

At 90% context usage: keeps system + first 2 context + last 4 messages, summarizes oldest half of middle via `summarize_turns()`.

### MCP

**Server**: rmcp-based, stdio transport. 22 tools: agent control (approve, deny, skip, respond, quit, set_autonomy, set_verbosity), task management (start_task, get_status, get_logs, get_pending_approval, get_pending_input), controller restart management (schedule/cancel/get_restart_status, controller_turn_complete), loop intervention (request/clear halt, intervene, get_loop_status), and `reload` (rebuilds binary, replaces process via `exec()`).

**Client**: configured via `[[mcp_servers]]` in intendant.toml. Tools registered as `mcp__<server>_<tool>`.

### Sandboxing

Landlock (Linux 5.13+): read `/` everywhere, write limited to project root + `/tmp` + log dir + `~/.intendant`. Extra write paths via `[sandbox] extra_write_paths`. Passed to runtime via `INTENDANT_SANDBOX_WRITE_PATHS`. Silently skipped on non-Linux or without kernel support.

### Display Pipeline

VNC is the foundation of the entire display pipeline. All displays — virtual and user session — go through the same lifecycle:

1. VNC server runs on the display (x11vnc on Linux, Screen Sharing on macOS)
2. `DisplayReady { display_id, vnc_port, width, height }` event fires
3. Recording listener starts ffmpeg (x11grab / avfoundation) → segmented MP4
4. Web dashboard shows noVNC viewer in the Displays tab
5. "Stream" button captures frames from the **noVNC canvas** at 1 Hz on two parallel paths:
   - 768×768 JPEG → sent **directly from browser WASM to live model** (Gemini Live / OpenAI Realtime). Server never sees these.
   - HQ JPEG → sent to server → frame registry (`<session>/frames/`) + recording pipeline
6. `sent_to_live` flag in `FrameMeta` tracks what the live model already saw

**User session display** (`DisplayTarget::UserSession`): Opt-in via `DisplayControl` autonomy category (session-grant model, `d` hotkey). On grant: x11vnc launched on `:0` (Linux) or macOS Screen Sharing prompted on port 5900. `DisplayReady` emitted — same pipeline as virtual displays. Display state is pull-based in `AgentStateSnapshot.available_displays` (no proactive inference on grant/revoke).

**CU-first routing**: `auto_attach_display_frames()` grabs the latest HQ frame per `display_*` stream from the registry. `submit_task` accepts `display_target` for explicit routing (`"user_session"`, `":99"`). Fallback: `resolve_cu_display_target()` heuristic.

### Skills

`SKILL.md` files with YAML frontmatter (`name`, `description`, `autonomy`, `disable-auto-invocation`, `sandbox`). Discovered from `<project_root>/.intendant/skills/` and `~/.intendant/skills/` (project takes precedence). Invoked via `invoke_skill` tool or control socket/TUI/presence.

### INTENDANT.md

Project instructions loaded from 2-layer cascade: `~/.config/intendant/INTENDANT.md` (global) → `<project_root>/INTENDANT.md` (local). Injected as user messages at conversation start.

### Transcription

Whisper API via `[transcription]` in intendant.toml. Web gateway buffers `user_audio` WebSocket messages in ~3s chunks, filters by RMS energy, wraps in WAV, sends to API. Custom endpoints supported for self-hosted whisper.cpp.

### Presence Layer

Conversational interface between user and agent system. Only one active at a time: server-side text OR browser-side live (Gemini Live / OpenAI Realtime).

**Server-side** (`presence.rs`): `PresenceLayer` wraps a fast model (e.g., gemini-3.0-flash). 12 tools from `presence-core`: action tools (submit_task, approve/deny/skip_action, respond_to_question, set_autonomy, send_message), query tools (check_status, query_detail, recall_memory), and video tools (inspect_frame, inspect_frames). Dispatch via `dispatch_tool_call()` → `PresenceAction` enum. Display state is pull-based via `check_status` → `available_displays` field (no proactive inference on display events).

**Browser-side** (`crates/presence-web/`): `presence_connect` pauses server-side presence, sends `presence_welcome` with state/replay/context. Tool calls via `tool_request`/`tool_response` WebSocket protocol. `presence_disconnect` resumes server-side.

**Active/passive browsers**: one active (controls voice), others passive. `make_active` → handover with `force_disconnect_voice` + `active_granted`.

**Session protocol**: `PresenceSession` tracks event windows/checkpoints. `presence_checkpoint` enables continuity across reconnects.

**presence-core**: WASM-compatible, no tokio/reqwest. Compiles to native + wasm32-unknown-unknown.

### Web Gateway

`--web` (default 8765). HTTP: `/` (app.html), `/config`, `/debug`, `POST /session` (ephemeral tokens), `/wasm-web/*`, `/audio-processor.js`, `/api/sessions`, `/api/session/{id}`, `/api/session/{id}/recordings/*`, `/recordings/*`. WebSocket: `/` or `/ws` (events + terminal + presence), `/vnc` (VNC proxy).

Per-connection `WebTui` instances with independent terminal dimensions. Caches latest `usage_update`, `status`, `display_ready` for late-connecting browsers.

**Web App Dashboard**: 4-tab SPA (Activity, Usage, Terminal, Displays). WASM-driven state via `AppState` returning `Vec<UiCommand>`. Catppuccin Mocha theme, mobile-responsive.

## Code Conventions

- Rust 2021 edition, default rustfmt/clippy (no config files)
- snake_case functions/modules, PascalCase types, SCREAMING_SNAKE_CASE constants
- `thiserror`-based error enums (`AgentError`, `CallerError`)
- tokio (full features), `Arc<RwLock/Mutex<T>>` for shared state, `mpsc` for channels
- No `unsafe` code
- Tests: inline `#[cfg(test)]` modules only
- WASM boundary: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)`

### Platform Support

Target platforms: macOS, Linux (Debian, X11 and Wayland).

Prefer platform-agnostic code by default. When platform-specific behavior is
unavoidable, use `cfg!(target_os = ...)` runtime checks for small branches or
`#[cfg(target_os = "...")]` compile-time gates for entire implementations.
Collect OS-specific helpers in dedicated modules (e.g. `platform.rs`,
per-platform blocks in `vision.rs`, `audio_routing.rs`, `computer_use.rs`).

Every feature must either work or degrade gracefully with a clear error on all
supported platforms — never panic or silently do nothing.

## Environment Requirements

- **OS**: macOS or Linux (Debian), unprivileged user with passwordless sudo (Linux)
- **API keys**: `.env` with `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`. Optional: `PROVIDER`, `MODEL_NAME`, `USE_NATIVE_TOOLS`, `STRUCTURED_OUTPUT`, `REASONING_EFFORT`, `REASONING_SUMMARY`
- **captureScreen**: ImageMagick `import` + DISPLAY (Linux), `screencapture` (macOS)
- **Computer use**: xdotool (Linux), cliclick (macOS)
- **Recording**: ffmpeg (`brew install ffmpeg` / `apt install ffmpeg`)
- **WASM build**: `wasm-pack` (`cargo install wasm-pack`)
- **macOS setup**: `./scripts/setup-macos.sh` installs all dependencies

## CI/CD

None configured. Run `cargo test --bins` and `cargo clippy` locally before committing.
