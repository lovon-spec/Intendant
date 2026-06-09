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
is busy; the chosen port is printed at startup. With the default mTLS transport,
open `https://<host>:<port>/` in a browser after running
`intendant access setup` and enrolling that browser/device. Use
`--bind 127.0.0.1` when starting plaintext local/debug dashboards with
`--no-tls`.

> **Correction vs. older docs:** `--web` is the default and no longer "implies
> `--mcp`". Earlier docs described `--web` as opt-in and tied to MCP mode —
> neither is true now.

## Secure Browser Contexts

The dashboard shell, Activity log, Sessions, Settings, and basic display viewing
can run over ordinary HTTP. Some browser capabilities are different: browsers
expose them only to a **secure context**.

Use a secure dashboard context when you need:

- **Station WebGPU rendering** (`navigator.gpu`) — otherwise Station falls back
  to its canvas-2D WASM renderer.
- **Microphone and camera** (`navigator.mediaDevices`, `getUserMedia`) for live
  voice, browser-side audio/video capture, or camera recording.
- **Screen/window capture from the browser** (`getDisplayMedia`) when a browser
  is the capture source.
- **Privileged browser APIs** such as the async clipboard in stricter browsers.

Practical rules:

- `https://` with a trusted certificate is the normal secure context for remote
  browsers.
- `http://localhost` and `http://127.0.0.1` are treated as secure by most
  desktop browsers, but not by every embedding. In particular, the macOS
  `WKWebView` wrapper uses the custom `intendant://` scheme because
  `http://localhost` there does not expose media devices.
- `http://<host-ip>` is not a secure context. Use default native mTLS,
  `--tls` with a trusted certificate, the macOS app wrapper, or another trusted
  HTTPS reverse proxy. The macOS app wrapper starts its bundled backend with
  mTLS by default and fails closed with setup guidance when access certs are
  missing.
- Clicking through a self-signed certificate warning is not a reliable substitute
  for installing/trusting the certificate; browsers may still withhold secure
  APIs.

### Headless daemon posture

When the dashboard is on, the terminal **TUI does not own the TTY**. The
controller runs in a headless/daemon posture and tees its stdout/stderr to
`daemon.log` in the session directory (so the dashboard's "Download session
report" can include controller output). With no task argument the agent simply
starts idle and waits for tasks submitted from the dashboard.

If you want the classic in-terminal TUI instead, run with `--no-web` on a real
terminal — then the TUI takes the foreground. See [TUI & Autonomy](./tui.md).

## Tabs

The top tab bar has eight tabs: **Activity**, **Stats**, **Terminal**,
**Video**, **Station**, **Sessions**, **Debug**, **Settings**. New events
arriving while you are on another tab raise a notification badge.

### Activity

The default tab. Five subtabs:

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
- **Managed** — managed-context anchors, rewind records, and recovery actions for
  managed Codex sessions.
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

### Station

An immersive WASM-rendered control center for the same operational surfaces as
the rest of the dashboard — Activity, Context, Managed context, Changes,
Sessions, Peers/displays, and Control. The `station-web` crate draws the whole
scene into a single canvas: WebGPU when the browser exposes it (a secure
context is required — see Secure Browser Contexts above), with a canvas-2D
WASM fallback used automatically when WebGPU is unavailable or forced with
`?station_gpu=canvas`. The renderer runs on `requestAnimationFrame` and
re-renders only when state or view input changes, so an idle Station stays
cheap.

There is no DOM dock: the rendered scene is the UI. An invisible hotspot
overlay mirrors the scene's interactive elements so they stay reachable from
the keyboard. Station actions dispatch through the same control plane as the
classic tabs, so anything triggered from Station behaves exactly like its
canonical dashboard equivalent. View settings shape the scene: layout
(`orbital` / `constellation`), mood (`calm` / `cockpit`), and fov, motion, ar,
and density tuning.

### Sessions

A browser of past and current sessions. Four subtabs:

- **Recent** — recent sessions with metadata (task, duration, status); click one
  to view its recordings and event log.
- **Deep Search** — search across session history.
- **Worktrees** — the git worktrees in use by sub-agents.
- **New Session** — start a fresh session from the dashboard.
  External Codex sessions can choose both the binary path and the
  `managed_context` mode (`vanilla` or `managed`) for that session.

External-agent session cards and Activity windows also expose **Launch config**
for per-session binary and managed-context settings. Use **Save** to update the
next attach/resume, or **Save & restart** to apply the new binary/mode
immediately to that external backend. These settings are stored with the
Intendant wrapper session and, for canonical backend session IDs, in an
external-session overlay. They are used on the next attach/resume so a daemon
restart or page refresh does not fall back to the current global Settings pane.
The separate **Restart with saved config** action is a power-user shortcut for
reapplying settings that were already persisted elsewhere.
The Managed activity view exposes rewind anchors, saved records, restore, and
fork/backout actions. With the patched managed Codex binary, fork/backout starts
a new Codex thread while inheriting the saved rollout's lineage prompt-cache key;
there is no separate cache-reset opt-in in the dashboard.
Editable user-message entries still perform an in-place rollback when the
message is active in the current thread. Superseded user-message entries in a
managed Codex session show the same edit control as a historical branch action:
submitting the text creates a child thread from the newest saved pre-rewind
rollout containing the clicked message, rolls that child back to the selected
turn, and sends the replacement there. The edit chip labels this as branching so
the active compacted session is not mistaken for the target of the mutation.

### Debug

A raw view of internal state — the same data as the `GET /debug` endpoint
(agent state, voice connection, active browser), useful when diagnosing the
gateway or presence wiring. It also includes a browser-workspace panel for
manual smoke testing of local CDP-backed browser workspaces and their leases.
CDP workspaces prefer managed Chromium/Chrome-for-Testing executables; on macOS
system Chrome/Chromium apps require choosing `system_cdp` or setting
`INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1`. Run
`intendant setup browsers` to install or repair the managed browser cache.

### Settings

The configuration panel for the current session. **Network → Daemons** is the
dashboard entry point for peer relationships:

- **Create Invite / Join Invite** handles the direct secret handoff flow.
- **Request Access** lets this daemon ask another daemon for a peer-scoped mTLS
  identity without receiving a private-key-bearing invite.
- **Incoming Requests** shows doorbell requests from other daemons and lets the
  local operator approve or deny them.

The panel keeps the older manual runtime URL add path for already-enrolled
peers, tunnels, and local/debug daemons.

## Late-join and session replay

The gateway is built so a browser that connects late sees the full picture
immediately. On WebSocket connect the server sends a sequence of bootstrap
messages:

1. **`state_snapshot`** — the full `AgentStateSnapshot` plus this connection's
   id, the `/config` payload, and the `session_id`
2. **Cached `usage_update`** — latest token usage
3. **Cached `status`** — latest autonomy / session id / task
4. **Cached `display_ready`** — latest display info for WebRTC sessions
5. **`browser_workspace_snapshot`** — active browser workspaces and lease state
6. **`log_replay`** — historical session events parsed from `session.jsonl`

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

## Secure Browser Contexts

- **Microphone/camera require a secure context.** Plain `http://<host-ip>` is not
  a secure context in the browser, so `getUserMedia` is blocked there. Reach the
  dashboard over one of:
  - `http://localhost` (e.g. an SSH tunnel: `ssh -L 8765:localhost:8765 host`),
  - HTTPS/mTLS via the default dashboard transport, `--tls`, or `[server.tls]`
    (see below), or
  - the macOS app bundle, which serves the page over a custom `intendant://`
    scheme specifically to restore the secure context (see
    [Getting Started](./getting-started.md#macos-app-bundle)). The bundle starts
    its daemon with native mTLS by default so remote browsers get a safe context
    over `https://` and must present an enrolled client identity.
- **API key for voice:** Gemini or OpenAI, stored browser-side only.

### HTTPS / TLS

```bash
./target/release/intendant                       # default: mTLS, requires access certs
./target/release/intendant --tls                 # TLS-only; installed access certs when present, else self-signed
./target/release/intendant --no-tls --bind 127.0.0.1 # explicit local plaintext/debug escape
./target/release/intendant --tls-cert c.pem --tls-key k.pem   # bring your own
```

By default, the gateway serves HTTPS/WSS with browser client certificates
required. `--tls` (or `[server.tls] enabled = true`) makes the gateway serve
HTTPS/WSS without requiring client certificates. With no explicit cert/key
override, TLS-only uses installed access server certs when present and falls back
to an auto self-signed certificate. Plain HTTP via `--no-tls` is intended for
local/programmatic debugging; wildcard plaintext refuses startup when the host
has a public interface unless `--allow-public-plaintext` is passed.
The gateway demuxes per connection: a first byte of `0x16` (a TLS ClientHello)
is wrapped in the rustls acceptor, while raw WebRTC ICE-TCP/UDP media is left
untouched. The TLS stack is pure Rust (`rustls` + `rcgen`) and works on every
platform including Windows — no nginx, no OpenSSL. See the
`[server.tls]` keys under
[Configuration → `[server]`](./configuration.md#server-daemon-and-federation).

For explicit mutual-TLS with client certificates (only enrolled devices can
connect), use native `--mtls` / `[server.mtls]`; this is also the default when no
transport flag is supplied. Use `intendant access setup` to generate the
per-user access CA/server/client certs and run strict enrollment. See
[Getting Started → Dashboard access over TLS](./getting-started.md#dashboard-access-over-tls) and
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
