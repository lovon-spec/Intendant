# Web Dashboard

The web dashboard is Intendant's **default frontend**. It is a single-page app
served by the controller's built-in HTTP/WebSocket gateway, running entirely in
the browser with WASM-powered state management (the `presence-web` crate,
Catppuccin Mocha theme, mobile-responsive). The static SPA lives in
`static/app.html`.

## On by default

There is no opt-in: the gateway starts automatically unless you pass `--no-web`,
`--mcp`, or `--json` (those modes own stdio / are headless by contract). The
`--web` flag simply forces it on and optionally sets the port.

```bash
./target/release/intendant "task"          # dashboard comes up; URL is printed
./target/release/intendant --web            # explicitly enable
./target/release/intendant --web 9000       # explicit port
./target/release/intendant --no-web "task"  # disable; use the terminal TUI instead
```

The server binds port **8765** by default, auto-incrementing through 8785 if it
is busy; the chosen port is printed at startup. Open `http://<host>:<port>/` in
a browser.

> **Correction vs. older docs:** `--web` is the default and no longer "implies
> `--mcp`". Earlier docs described `--web` as opt-in and tied to MCP mode —
> neither is true now.

### Headless daemon posture

When the dashboard is on, the terminal **TUI does not own the TTY**. The
controller runs in a headless/daemon posture and tees its stdout/stderr to
`daemon.log` in the session directory (so the dashboard's "Download session
report" can include controller output). With no task argument the agent simply
starts idle and waits for tasks submitted from the dashboard.

If you want the classic in-terminal TUI instead, run with `--no-web` on a real
terminal — then the TUI takes the foreground. See [TUI & Autonomy](./tui.md).

## Tabs

The top tab bar has seven tabs: **Activity**, **Stats**, **Terminal**,
**Video**, **Sessions**, **Debug**, **Settings**. New events arriving while you
are on another tab raise a notification badge.

### Activity

The default tab. Four subtabs:

- **Log** — a scrollable, color-coded event stream of everything in the system,
  grouped by turn with visual separators, with a verbosity selector
  (Normal/Verbose). Event sources are color-coded:
  - **system** — session lifecycle, approvals, context management
  - **worker** — model responses, reasoning summaries, task completion
  - **agent** — command execution output (stdout/stderr, exit codes)
  - **live** — voice transcripts, presence lifecycle, tool requests
  - **server** — presence model internals (thinking, tool calls)

  The Log pane also carries the approval controls (Approve / Skip / Approve All
  / Deny) and a follow-up text input for sending a message after a round
  completes.
- **Context** — the agent's current working context (what it is operating on).
- **Changes** — file changes / diffs produced during the session (with its own
  badge when new changes land).
- **Control** — direct controls for steering the run.

### Stats

Token-consumption and cost tracking:

- Per-model breakdown for the main and presence models (prompt, completion, and
  cached token counts)
- Cost estimates from a built-in pricing table (OpenAI, Anthropic, Gemini)
- All-sessions cumulative usage and disk usage
- Display-transport metrics (frame rate, encode latency, bandwidth per display)

### Terminal

An embedded xterm.js terminal. Two subtabs:

- **TUI** — the server-side ratatui TUI, rendered per-connection. Each browser
  connection gets its own `WebTui` with independent dimensions, so two browsers
  can size the terminal differently. This is the same status bar / log panel /
  action panel / approval-and-input UI as the native terminal.
- **Shell** — an interactive shell session.

### Video

WebRTC display viewers for the agent's graphical displays, with interactive
control (see [Display Pipeline](./display-pipeline.md)):

- **View mode** (default) — watch the agent's display in real time
- **Take Control** — forward mouse and keyboard events to the agent's display
- **Release** — relinquish control, with an optional note
- **Display picker** — choose which monitor to view when several are present
- **Recording replay** — browse and play back recorded sessions with timeline
  seeking and speed control (1x / 2x / 4x)

Displays appear automatically when the agent's first command triggers Xvfb
auto-launch, or when access to the user's real session display is granted.
WebRTC negotiation (SDP offer/answer + ICE candidates) is multiplexed over the
existing dashboard WebSocket.

### Sessions

A browser of past and current sessions. Four subtabs:

- **Recent** — recent sessions with metadata (task, duration, status); click one
  to view its recordings and event log.
- **Deep Search** — search across session history.
- **Worktrees** — the git worktrees in use by sub-agents.
- **New Session** — start a fresh session from the dashboard.

### Debug

A raw view of internal state — the same data as the `GET /debug` endpoint
(agent state, voice connection, active browser), useful when diagnosing the
gateway or presence wiring.

### Settings

The configuration panel for the current session.

## Late-join and session replay

The gateway is built so a browser that connects late sees the full picture
immediately. On WebSocket connect the server sends a sequence of bootstrap
messages:

1. **`state_snapshot`** — the full `AgentStateSnapshot` plus this connection's
   id, the `/config` payload, and the `session_id`
2. **Cached `usage_update`** — latest token usage
3. **Cached `status`** — latest autonomy / session id / task
4. **Cached `display_ready`** — latest display info for WebRTC sessions
5. **`log_replay`** — historical session events parsed from `session.jsonl`

So refreshing the page, or opening a second browser mid-run, replays the
session rather than starting from blank.

## Live voice (optional)

The dashboard supports optional low-latency voice via **Gemini Live** or
**OpenAI Realtime**. Voice is entirely optional — the dashboard is fully usable
without it.

When activated:

- The browser connects **directly** to the model's realtime API for voice I/O.
- The WASM layer (`presence-web`) handles mic capture, resampling, and WebSocket
  streaming.
- The live model receives agent events and narrates progress, and can call
  presence tools (`submit_task`, `approve_action`, `check_status`, …) which are
  routed over the dashboard WebSocket to the server.
- Server-side text presence is automatically paused (the two are mutually
  exclusive).

### Voice setup

1. Enter your provider API key on first visit (Gemini or OpenAI).
2. Keys are stored in browser **localStorage** and are never sent to the
   Intendant server (the server only mints short-lived session tokens via
   `POST /session`).
3. Click the microphone button to connect.

### Active vs. passive browsers

Only one browser can be the **active** voice controller at a time:

- The first browser to connect voice becomes active.
- Additional browsers are passive observers — they receive events and TUI frames
  but do not pause server-side presence.
- A passive browser can request active status, which force-disconnects the
  previous active browser. Handover carries the last checkpoint summary and
  conversation context.

### Session continuity across reconnects

The presence session protocol survives refreshes and dropped connections:

1. On connect the server sends a `presence_welcome` with current state, missed
   events, and conversation context.
2. The browser sends periodic `presence_checkpoint` messages summarizing the
   conversation.
3. On reconnect the server replays events since the last checkpoint.

This keeps the voice model from losing context. The protocol and mutual
exclusion are detailed in [Presence Layer](./presence.md).

## Server-side transcription

Independently of browser-side voice, the server can transcribe microphone audio
via the Whisper API when `[transcription]` is enabled (or `--transcription` is
passed):

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
language = "en"
```

The browser streams PCM16 audio; the server buffers it in ~3s chunks
(`buffer_secs`, RMS-filtered to skip silence) and sends each chunk to the
transcription endpoint. Transcripts are broadcast as `user_transcript` events
and written to the session log. See
[Configuration](./configuration.md#transcription).

## Secure context and LAN access

- **Microphone/camera require a secure context.** Plain `http://<lan-ip>` is not
  a secure context in the browser, so `getUserMedia` is blocked there. Reach the
  dashboard over one of:
  - `http://localhost` (e.g. an SSH tunnel: `ssh -L 8765:localhost:8765 host`),
  - HTTPS via `--tls` / `[server.tls]` (see below), or
  - the macOS app bundle, which serves the page over a custom `intendant://`
    scheme specifically to restore the secure context (see
    [Getting Started](./getting-started.md#macos-app-bundle)).
- **API key for voice:** Gemini or OpenAI, stored browser-side only.

### HTTPS / TLS

```bash
./target/release/intendant --tls                 # auto self-signed cert
./target/release/intendant --tls-cert c.pem --tls-key k.pem   # bring your own
```

`--tls` (or `[server.tls] enabled = true`) makes the gateway serve HTTPS/WSS
directly. The gateway demuxes per connection: a first byte of `0x16` (a TLS
ClientHello) is wrapped in the rustls acceptor, while raw WebRTC ICE-TCP/UDP
media is left untouched. The TLS stack is pure Rust (`rustls` + `rcgen`) and
works on every platform including Windows — no nginx, no OpenSSL. See the
`[server.tls]` keys under
[Configuration → `[server]`](./configuration.md#server-daemon-and-federation).

For mutual-TLS with client certificates (only enrolled devices can connect), use
`intendant lan setup` — see
[Getting Started → LAN access](./getting-started.md#lan-access) and
[Peer Federation](./peer-federation.md). For the daemon posture and remote
control surface, see [Control Plane & Daemon](./control-plane-and-daemon.md).

## HTTP endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /` | The dashboard SPA |
| `GET /config` | Live-model configuration JSON (provider, model, sample rates, git SHA) |
| `GET /debug` | Debug JSON (agent state, voice connection, active browser) |
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime |
| `GET /wasm-web/*` | Compiled WASM + JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for mic capture |
| `GET /api/sessions` | List past sessions |
| `GET /api/session/{id}` | Session detail |
| `GET /api/session/{id}/recordings/*` | Recording segments for a past session |
| `GET /recordings/*` | Current-session recording segments |
| `WS /` or `WS /ws` | Main WebSocket: events, terminal I/O, presence protocol, WebRTC signaling |

The full WebSocket message protocol (inbound key/resize/presence/WebRTC frames,
outbound term/state/log-replay/tool-response frames) and the gateway's internal
layering are documented in [Integrations → Web Gateway](./integrations.md#web-gateway).
