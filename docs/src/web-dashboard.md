# Web Dashboard

The `--web` flag starts a web server that serves a modern dashboard for monitoring and interacting with Intendant remotely. The dashboard runs entirely in the browser with WASM-powered state management (Catppuccin Mocha theme, mobile-responsive).

## Running

```bash
# Default port 8765
./target/release/intendant --web

# Custom port
./target/release/intendant --web 9000
```

Open `http://<host>:8765/` in a browser. The `--web` flag implies `--mcp`, so no initial task is required — the agent starts idle and accepts tasks dynamically.

## Dashboard Tabs

### Activity

A scrollable, color-coded event log showing everything happening in the system:

- **system** — session lifecycle, approvals, context management
- **worker** — model responses, reasoning summaries, task completion
- **agent** — command execution output (stdout/stderr, exit codes)
- **live** — voice transcripts, presence lifecycle, tool requests
- **server** — presence model internals (thinking, tool calls)

Events are grouped by turn with visual separators. New events while viewing other tabs trigger a notification badge. Late-connecting browsers receive a full replay of historical events from `session.jsonl`. Includes approval buttons (Approve/Skip/Approve All/Deny) and a follow-up text input for sending messages after rounds complete.

### Stats

Token consumption tracking:

- Per-model breakdown for main and presence models
- Prompt, completion, and cached token counts
- Cost estimates using a built-in pricing table (OpenAI, Anthropic, Gemini)
- All-sessions cumulative usage
- Disk usage
- Display transport metrics (frame rate, encode latency, bandwidth per display)

### Terminal

An embedded xterm.js terminal connected to the server-side ratatui TUI. Each browser connection gets its own independent terminal rendering with separate dimensions. This shows the same interface as the native terminal TUI — status bar, log panel, action panel, approval/input panels.

### Video

WebRTC display viewers for agent displays with interactive control:

- **View mode** (default) — watch the agent's graphical display in real-time
- **Take Control** — forward mouse and keyboard events to the agent's display
- **Release** — release control with an optional note
- **Display picker** — select which monitor to view when multiple displays are available
- **Recording replay** — browse and play back recorded display sessions with timeline seeking and speed control (1x/2x/4x)

Displays appear automatically when the agent's first command triggers Xvfb auto-launch or when user session display access is granted. WebRTC negotiation happens transparently — the browser and server exchange SDP offers/answers and ICE candidates over the existing WebSocket connection.

### Sessions

Session browser showing past sessions with metadata (task, duration, status). Click a session to view its recordings and event log.

### Settings

Configuration panel for the current session.

## Live Voice

The dashboard supports optional live voice interaction via Gemini Live or OpenAI Realtime. When activated:

- The browser connects directly to the model's realtime API for low-latency voice I/O
- The WASM layer (`presence-web` crate) handles audio capture, resampling, and WebSocket streaming
- The live model receives agent events and narrates progress
- Tool calls from the live model (`submit_task`, `approve_action`, `check_status`, etc.) are routed through the WebSocket to the server
- Server-side presence is automatically paused (mutual exclusion)

### Setup

1. Enter your API key on first visit (Gemini or OpenAI)
2. Keys are stored in browser localStorage — never sent to the Intendant server
3. Click the microphone button to connect

### Active/Passive Browsers

Only one browser can be "active" (controlling the voice model) at a time:

- First browser to connect voice becomes active
- Additional browsers are passive observers (receive events and TUI frames, but don't pause server-side presence)
- A passive browser can request active status via the UI, which force-disconnects the previous active browser
- Active handover includes the last checkpoint summary and conversation context

### Session Continuity

The presence session protocol maintains context across reconnects:

1. On connect, the server sends a `presence_welcome` with current state, missed events, and conversation context
2. The browser sends periodic `presence_checkpoint` messages with a summary of the conversation
3. On reconnect, the server replays events since the last checkpoint
4. This prevents the voice model from losing context when the browser refreshes or the connection drops

## Server-Side Transcription

When `[transcription]` is enabled in `intendant.toml`, the browser sends microphone audio to the server for transcription via the Whisper API:

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
language = "en"
```

Audio is buffered in ~3s chunks, filtered by RMS energy to skip silence, and sent to the transcription endpoint. Transcripts are broadcast as `user_transcript` events and logged to the session.

## Configuration

The web gateway configuration is controlled by `[presence]` settings in `intendant.toml`:

```toml
[presence]
live_provider = "gemini"                                    # voice model provider
live_model = "gemini-2.5-flash-native-audio-preview-12-2025"  # voice model
```

Or via environment variables:
- `GEMINI_API_KEY` / `OPENAI_API_KEY` — for ephemeral token minting (POST /session)

The `/config` endpoint returns the configured provider, model, and sample rates as JSON.

## HTTP Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /` | Web app dashboard |
| `GET /config` | Live model configuration JSON |
| `GET /debug` | Debug JSON (agent state, voice connection, active browser) |
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime |
| `GET /wasm-web/*` | WASM and JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for microphone capture |
| `GET /api/sessions` | List past sessions |
| `GET /api/session/{id}` | Session detail |
| `GET /api/session/{id}/recordings/*` | Session recording segments |
| `GET /recordings/*` | Current session recording segments |
| `WS /` or `WS /ws` | Main WebSocket (events, terminal I/O, presence protocol, WebRTC signaling) |

## Requirements

- **Microphone access requires a secure context**: Use `localhost` (via SSH tunnel: `ssh -L 8765:localhost:8765 host`), or set browser flags for insecure origins
- **API key for voice**: Gemini or OpenAI (stored browser-side only). Voice is optional — the dashboard works without it
- **WASM**: The dashboard uses a compiled WASM module (`presence-web` crate). Rebuild with `wasm-pack build --target web` from `crates/presence-web/` if you modify the Rust code
