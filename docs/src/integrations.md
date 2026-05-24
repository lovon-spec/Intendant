# Integrations

Intendant integrates with the outside world in several directions: it consumes
**external MCP servers** and **external coding-agent CLIs** as tools, routes
**audio** through platform virtual-audio bridges, transcribes speech, records
displays, and shells out to a set of **system tools**. This chapter covers each,
plus the **control socket** and **web gateway** programmatic surfaces and the
**CI / setup tooling**.

For the MCP *server* (exposing Intendant's own control surface as MCP tools) see
[MCP Server](./mcp-server.md). For the presence layer see
[Presence Layer](./presence.md).

## MCP client

Intendant can connect to external Model Context Protocol servers and surface
their tools to the agent. Servers are declared as `mcp_servers` entries in
`intendant.toml`:

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

Each entry has `name`, `command`, optional `args`, and an optional `env` table
(see [Configuration](./configuration.md#mcp_servers)). At startup each server is
spawned as a child process, its tool list is fetched, and every tool is
registered to the agent under the name `mcp__<server>_<tool>` (e.g.
`mcp__github_list_issues`). When the agent calls one of these, the client routes
the call to the right server by parsing that prefix.

### Trust model

This is a privileged integration with **no sandboxing**:

> An `mcp_servers` entry is spawned with **your full privileges** —
> `Command::new(&config.command).args(&config.args)` in `mcp_client.rs`.
> Intendant performs **no checksum verification, no signature check, and no
> sandboxing** of the server binary. Adding one is equivalent to adding a line
> to your `~/.zshrc` that runs a binary.

The default is `mcp_servers = []`, and `intendant.toml` is git-ignored, so the
repository ships **no** MCP servers. Treat copying an `intendant.toml` between
machines like copying shell rc files: read it before you source it. The same
trust framing applies to the MCP *server* side — see
[MCP Server](./mcp-server.md).

## External coding-agent CLIs

Instead of the native agent loop, Intendant can delegate coding work to an
external CLI agent — **Codex**, **Claude Code**, or **Gemini CLI** — selected
per-invocation with `--agent <codex|claude-code|gemini>` or by default via
`[agent] default_backend`. Each backend's binary path, model, sandbox/approval
policy, and tool restrictions are configured under `[agent.codex]`,
`[agent.claude_code]`, and `[agent.gemini_cli]` (full key reference in
[Configuration](./configuration.md#agent-and-external-backends)).

Notable details:

- **Codex** runs against its app-server (`thread/start`); the `sandbox`,
  `approval_policy`, `reasoning_effort`, `web_search`, `network_access`, and
  `writable_roots` keys map onto Codex's own CLI flags. Approval prompts from
  Codex are surfaced through Intendant's normal approval UI.
- **Gemini CLI** has Intendant's MCP entry merged into
  `$HOME/.gemini/settings.json` so the Gemini session can reach Intendant's
  tools; if you set `allowed_mcp_servers`, include `intendant`.

This is a deep topic with its own chapter — see
[External Agent Orchestration](./external-agent-orchestration.md).

## Audio stack

Live voice and phone calls pipe audio through a **virtual audio bridge** so the
voice model's I/O can be captured and injected independently of the host's real
devices. The backend is chosen per platform (`audio_routing.rs`):

```
                live audio / phone-call session
                              │
        ┌─────────────────────┼─────────────────────┐
     macOS                 Linux                  (host helper)
        │                     │                       │
  Vortex Audio HAL      PulseAudio null sinks    bh-bridge --port
  (preferred): a         (pactl module-null-     (BlackHole fallback
  "Vortex Audio"         sink for mic + speaker,  on macOS): host runs
  device + daemon        default source/sink      a small bridge that
  over a Unix socket     swapped for the session) shuttles BlackHole I/O
```

- **macOS (preferred): Vortex Audio.** A Core Audio HAL plugin shipped in
  `vendor/vortex-guest-tools/` that exposes a "Vortex Audio" device to all apps
  and bridges audio over a Unix socket to a guest daemon — no system audio
  reroute and no reboot. `scripts/setup-macos.sh` installs it (or
  `scripts/update-vortex-pkg.sh` updates the package); see
  [Computer Use & Live Audio](./computer-use-and-audio.md).
- **macOS fallback: BlackHole.** When Vortex is not present, a `BlackHole 2ch` /
  `16ch` virtual device is used, driven by a host-side `bh-bridge` helper.
- **Linux: PulseAudio null sinks.** `pactl` creates null sinks for the mic and
  speaker paths, and the session temporarily swaps the default source/sink,
  restoring them on teardown. (`pulseaudio-utils` provides `pactl`.)
- **Device management (macOS): `SwitchAudioSource`** lists and selects input
  devices; **`sox`** is used for audio format handling.

The phone-call skill places outbound SIP calls via `pjsua`, with the live voice
model conducting the conversation and returning structured data. The live audio
model is untrusted: zero tools, zero file access, and its outputs are
schema-validated and quarantined.

## Transcription (Whisper)

Server-side speech-to-text uses the OpenAI Whisper API (or a compatible
endpoint) when `[transcription]` is enabled or `--transcription` is passed. The
browser streams PCM16 audio; the gateway buffers ~3s chunks (`buffer_secs`,
RMS-filtered to drop silence), wraps each in WAV, and POSTs it to the endpoint.
Transcripts are broadcast as `user_transcript` events and logged. Configure via
`[transcription]` (`provider`, `model`, `language`, `endpoint`) — see
[Configuration](./configuration.md#transcription) and
[Web Dashboard](./web-dashboard.md#server-side-transcription). Point `endpoint`
at a self-hosted whisper.cpp server for a fully local pipeline.

## Recording (ffmpeg)

Display recording is driven by `ffmpeg` and configured under `[recording]`
(`enabled`, `framerate`, `segment_duration_secs`, `quality`,
`max_retention_hours`). Recordings are segmented and served by the dashboard's
`/recordings/*` and `/api/session/{id}/recordings/*` endpoints, with timeline
seeking and 1x/2x/4x playback in the Video tab. If `[recording] enabled = true`
but ffmpeg is not installed, recording is disabled with a logged warning rather
than failing the run. The `--record-display <ID>` flag records an existing X11
display (`:ID`) and is repeatable.

## System tools

Intendant shells out to platform tools for capture, input injection, and search.
Install them via the setup scripts (next section); the agent degrades gracefully
with a clear error when one is missing.

| Purpose | macOS | Linux (X11) | Linux (Wayland) |
|---------|-------|-------------|-----------------|
| Input injection | `cliclick` | `xdotool` | `ydotool` |
| Display capture | ScreenCaptureKit | libxcb + libxcb-shm (XShm) | PipeWire (DMA-BUF) |
| H.264 encode | VideoToolbox | ffmpeg + x264 / VA-API | ffmpeg + x264 / VA-API |
| VP8 encode | libvpx | libvpx | libvpx |
| Image handling | ImageMagick | ImageMagick | ImageMagick |
| Code search (used by external agents) | ripgrep | ripgrep | ripgrep |
| Recording | ffmpeg | ffmpeg | ffmpeg |

X11 displays are auto-launched via Xvfb when the agent first needs one. See
[Display Pipeline](./display-pipeline.md) for the capture/encode pipeline.

## Setup and CI tooling

### Setup scripts (`scripts/`)

| Script | Purpose |
|--------|---------|
| `setup-linux.sh` | Install the Debian/Ubuntu `APT_PACKAGES` set + toolchain build deps; `--check` to report only |
| `setup-macos.sh` | Install macOS deps (cliclick, ffmpeg, sox, SwitchAudioSource, Vortex/BlackHole, wasm-pack) and build; `--check` to report only |
| `setup-windows.ps1` | Windows toolchain + build for `x86_64-pc-windows-msvc` (see [Windows Support](./windows-support.md)) |
| `bundle-macos.sh` | Build and codesign the macOS `.app` (WKWebView wrapper over the `intendant://` scheme) and install to `/Applications` |
| `setup-lan.sh`, `setup-lan-macos.sh`, `setup-lan-guest-macos.sh`, `setup-lan.bat` | Wrappers around the `intendant lan` mTLS reverse-proxy flow |
| `intendant-ctl.sh` | Convenience wrapper over the control socket (`status`, `approve`, `follow`, `start`, …) |

> **When you add a new `-sys` crate dependency, update both
> `scripts/setup-linux.sh` (`APT_PACKAGES`) and `scripts/setup-macos.sh` in the
> same commit** — otherwise fresh-machine setups break later with cryptic
> `pkg-config` errors.

### CI (`.github/workflows/`)

| Workflow | Trigger | What it does |
|----------|---------|--------------|
| `windows.yml` | push/PR to `main` (Rust/Cargo paths) | Cross-platform `cargo check -p intendant` on Windows (`x86_64-pc-windows-msvc`), macOS (`aarch64-apple-darwin`), and Linux (`x86_64-unknown-linux-gnu`) to catch platform-specific build breaks |
| `audit.yml` | push/PR (Cargo paths) + weekly cron (Mon 08:00 UTC) | `cargo audit` against the RustSec advisory DB |
| `docs.yml` | docs changes | Build and deploy this mdBook |

The `tests/e2e/` integration tests make real API calls and are **not** in CI.
Run `cargo test --bins` and `cargo clippy` locally before committing. The TLS
stack is pure-Rust `ring` / `rustls` / `rcgen` (no OpenSSL), which is why the
Windows CI job installs NASM (for `ring`'s assembly) but no `libssl`.

## Control socket

When `--control-socket` is enabled, a Unix domain socket is created at
`/tmp/intendant-<pid>.sock` for programmatic control of a running instance from
external scripts and tools. It is opt-in.

- Outbound: events are broadcast (newline-delimited JSON) to all connected
  clients.
- Inbound: newline-delimited JSON commands for status, approval/denial, human
  input, autonomy change, quit, controller-restart workflow, and (in MCP mode)
  controller-loop intervention.

### Inbound commands (JSON-line)

```json
{"action": "status"}
{"action": "approve", "id": 123}
{"action": "deny", "id": 123}
{"action": "input", "text": "answer to askHuman"}
{"action": "set_autonomy", "level": "high"}
{"action": "schedule_controller_restart", "controller_id":"codex", "north_star_goal":"audit and improve", "restart_after":"turn_end"}
{"action": "controller_turn_complete", "restart_id":"<id>", "turn_complete_token":"<token>", "status":"ok", "handoff_summary":"..."}
{"action": "get_restart_status"}
{"action": "cancel_controller_restart", "restart_id":"<id>"}
{"action": "request_controller_loop_halt", "persistent": true}
{"action": "clear_controller_loop_halt"}
{"action": "intervene_controller_loop", "mode":"stop"}
{"action": "get_controller_loop_status"}
{"action": "query_detail", "scope": "diff"}
{"action": "query_detail", "scope": "file", "target": "src/main.rs"}
{"action": "recall_memory", "keywords": ["auth", "login"], "channel": "project_state"}
{"action": "usage"}
{"action": "quit"}
```

### Outbound events

```json
{"event": "turn_started", "turn": 5, "budget_pct": 12.3}
{"event": "agent_output", "stdout": "...", "stderr": "..."}
{"event": "approval_required", "id": 123, "command": "rm -rf /tmp/test"}
{"event": "ask_human", "question": "Which database?"}
{"event": "task_complete", "reason": "done signal"}
{"event": "status", "turn": 3, "phase": "thinking", "autonomy": "medium", "session_id": "abc-123", "task": "fix tests"}
{"event": "usage", "main": {"provider": "openai", "model": "gpt-5.5", "tokens_used": 12000, "context_window": 128000, "usage_pct": 9.4}}
{"event": "usage_update", "main": {"provider": "openai", "model": "gpt-5.5", "tokens_used": 15000, "context_window": 128000, "usage_pct": 11.7}}
{"event": "display_ready", "display_id": "virtual-99", "width": 1024, "height": 768}
{"event": "command_result", "action": "get_restart_status", "ok": true, "message": "ok", "data": {}}
```

- `status` includes `session_id` and `task`. The `usage` event answers
  `{"action":"usage"}`; `usage_update` is broadcast automatically after each
  turn (with a `presence` field when the presence layer is active).
  `display_ready` fires when a display becomes available for WebRTC streaming.
  `command_result.ok` is `false` when a control action fails (e.g.
  `schedule_controller_restart` with `restart_after="now"` and no executable
  restart action).

### Example

```bash
echo '{"action":"status"}' | socat - UNIX:/tmp/intendant-$(pgrep intendant).sock

# Or the helper script:
./scripts/intendant-ctl.sh status
./scripts/intendant-ctl.sh approve
./scripts/intendant-ctl.sh follow "fix that other bug too"
./scripts/intendant-ctl.sh start "new task description"
```

## Web gateway

The default-on web gateway (`--web`, see [Web Dashboard](./web-dashboard.md))
serves the SPA and bridges WebSocket connections to the EventBus. It is the same
control surface as the control socket, plus terminal I/O, presence, and WebRTC
signaling.

```
Browser ──WebSocket──> Intendant web gateway (default port 8765)
  │                              │
  │  Terminal I/O (ANSI)         │  Events (broadcast to all clients)
  │  Key/resize input            │  Tool responses (per-connection direct channel)
  │  Tool requests               │  State snapshot + log replay (on connect)
  │  presence_connect/disconnect │  Presence welcome (on voice connect)
  │  Voice logs/checkpoints      │  Per-connection TUI frames
  │  Audio for transcription     │  WebRTC signaling (SDP, ICE)
  │  WebRTC signaling            │
  v                              v
App dashboard (WASM)        EventBus + AgentStateSnapshot
  +                              │
Optional: browser-side           │  Dual outbound channels:
live model (Gemini/OpenAI)       │  - broadcast::Receiver (events)
  │                              │  - mpsc::unbounded (direct responses)
  │  (function calls → tool_request)
  v
Intendant agent loop
```

The gateway has three layers:

1. **App dashboard** — the SPA at `/`, state managed by `presence-web` WASM.
   Events are broadcast; late-connecting browsers get a full log replay.
2. **Per-connection TUI rendering** — each connection gets its own `WebTui`
   with independent dimensions; ANSI output goes per-connection on the direct
   channel, not broadcast.
3. **Presence bridge** (optional) — a browser-side live model's tool calls
   become `tool_request` WebSocket messages handled server-side, with
   `tool_response` returned on the per-connection direct channel.

### WebSocket protocol

#### Inbound (browser → server)

| Message | Description |
|---------|-------------|
| `{"t":"key","key":"..."}` | Keyboard input (per-connection WebTui) |
| `{"t":"resize","cols":N,"rows":N}` | Terminal resize (per-connection) |
| `{"t":"presence_connect",...}` | Presence session protocol — replaces server-side presence |
| `{"t":"presence_disconnect"}` | Disconnect presence — resumes server-side presence |
| `{"t":"make_active"}` | Request active voice ownership (handover) |
| `{"t":"voice_log","text":"...","seq":N}` | Voice transcript from the browser presence model |
| `{"t":"presence_checkpoint","summary":"...","last_event_seq":N}` | Context checkpoint |
| `{"t":"voice_diagnostic","kind":"...","detail":"..."}` | Browser voice diagnostics |
| `{"t":"user_audio","data":"<base64>"}` | PCM16 audio for server-side transcription |
| `{"t":"tool_request","id":"...","tool":"...","args":{}}` | Presence tool call |
| `{"t":"async_query","id":"...","tool":"...","args":{}}` | Async query (result returned as text) |
| `{"t":"display_offer","display_id":"...","sdp":"..."}` | WebRTC SDP offer for display streaming |
| `{"t":"display_answer","display_id":"...","sdp":"..."}` | WebRTC SDP answer |
| `{"t":"display_ice","display_id":"...","candidate":"..."}` | WebRTC ICE candidate |
| `{"t":"frame","stream":"...","data":"<base64>"}` | Display/camera frame for the frame registry |
| `{"action":"..."}` | ControlMsg (same as the Unix control socket) |

#### Outbound (server → browser)

| Message | Description |
|---------|-------------|
| `{"t":"term","d":"<base64>"}` | Per-connection TUI ANSI output |
| `{"t":"state_snapshot","state":{},"connection_id":"...","config":{},"session_id":"..."}` | Bootstrap on connect |
| `{"t":"log_replay","entries":[...]}` | Historical session events for late joiners |
| `{"t":"presence_welcome","session_id":"...","state":{},"events":[...],"is_active":bool,"conversation_context":"..."}` | Presence session welcome |
| `{"t":"active_granted","is_active":true,"handover_context":"...","conversation_context":"..."}` | Active ownership granted |
| `{"t":"force_disconnect_voice","reason":"handover"}` | Sent to the old active on handover |
| `{"t":"presence_checkpoint_ack","seq":N}` | Checkpoint acknowledgement |
| `{"t":"tool_response","id":"...","result":"..."}` | Response to a `tool_request` |
| `{"t":"async_query_result","id":"...","tool":"...","result":"..."}` | Response to `async_query` |
| `{"t":"display_answer","display_id":"...","sdp":"..."}` | WebRTC SDP answer |
| `{"t":"display_ice","display_id":"...","candidate":"..."}` | WebRTC ICE candidate |
| `{"event":"..."}` | OutboundEvent broadcast (status, agent_output, approval_required, display_ready, …) |

#### Tool request/response and bootstrap

A browser live model calls presence tools via tagged request/response:

```json
// Browser:
{"t":"tool_request","id":"req-42","tool":"check_status","args":{}}
// Server (direct channel):
{"t":"tool_response","id":"req-42","result":"Phase: Running agent (turn 5). Budget: 23% used."}
```

- **Action tools** (`submit_task`, `approve_action`, `deny_action`,
  `skip_action`, `respond_to_question`, `set_autonomy`, `send_message`) dispatch
  through the EventBus — the same path as TUI key presses and control-socket
  commands.
- **Query tools** (`check_status`, `query_detail`, `recall_memory`) are handled
  asynchronously server-side, reading the shared `AgentStateSnapshot`, project
  files, and knowledge store.
- **Video tools** (`inspect_frame`, `inspect_frames`) examine frames from the
  frame registry.

On connect the server sends, in order: `state_snapshot`, the cached
`usage_update`, the cached `status`, the cached `display_ready`, and finally
`log_replay` — so late-connecting browsers see complete state immediately.

### HTTP endpoints

See [Web Dashboard → HTTP endpoints](./web-dashboard.md#http-endpoints) for the
endpoint table.

### Requirements

- **Microphone access requires a secure context** — `localhost` (e.g.
  `ssh -L 8765:localhost:8765 host`), HTTPS via `--tls`, or the macOS app's
  `intendant://` scheme. See
  [Web Dashboard → Secure context](./web-dashboard.md#secure-context-and-lan-access).
- **API key for voice** — Gemini or OpenAI, used browser-side only. Voice is
  optional.

### Supported tools (browser live model)

| Tool | Type | Description |
|------|------|-------------|
| `submit_task` | Action | Submit a new task to the agent loop |
| `approve_action` | Action | Approve a pending action |
| `deny_action` | Action | Deny a pending action |
| `skip_action` | Action | Skip a pending action |
| `respond_to_question` | Action | Answer an `askHuman` question |
| `set_autonomy` | Action | Change the autonomy level |
| `send_message` | Action | Send a mid-task interjection to the agent |
| `check_status` | Query | Current phase, turn, budget, available displays |
| `query_detail` | Query | git diff, file contents, task results, or log details |
| `recall_memory` | Query | Search the knowledge store by keywords/channel |
| `inspect_frame` | Video | Examine a specific frame from the frame registry |
| `inspect_frames` | Video | Examine multiple frames for visual context |
