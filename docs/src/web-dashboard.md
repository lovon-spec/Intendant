# Web Dashboard

The `--web` flag starts a web server that serves a modern dashboard for monitoring and interacting with Intendant remotely. The dashboard runs entirely in the browser with WASM-powered state management.

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

Events are grouped by turn with visual separators. New events while viewing other tabs trigger a notification badge. Late-connecting browsers receive a full replay of historical events from `session.jsonl`.

### Usage

Token consumption for the main model and presence model:

- Prompt, completion, and cached token breakdowns
- Cost estimates using a built-in pricing table (OpenAI, Anthropic, Gemini models)
- Usage history over time
- Updated after each agent turn via `usage_update` events

### Terminal

An embedded xterm.js terminal connected to the server-side ratatui TUI. Each browser connection gets its own independent terminal rendering with separate dimensions. This shows the same interface as the native terminal TUI — status bar, log panel, action panel, approval/input panels.

Key presses and terminal resizes in the browser are sent to the server and rendered independently per connection.

### Displays

Remote viewing of Xvfb displays created by the agent. When the agent runs graphical applications (via `execAsAgent` with a DISPLAY), the display appears here as a noVNC viewer.

Displays are created lazily — the tab populates automatically when the agent's first command triggers Xvfb auto-launch. Each display shows the VNC port for direct connection too.

## Live Voice

The dashboard supports optional live voice interaction via Gemini Live or OpenAI Realtime. When activated:

- The browser connects directly to the model's realtime API for low-latency voice I/O
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
| `GET /` | Web app dashboard (4-tab UI) |
| `GET /live` | Legacy xterm.js terminal + voice overlay |
| `GET /config` | Live model configuration JSON |
| `GET /debug` | Debug JSON (agent state, voice connection, active browser) |
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime |
| `GET /wasm-web/*` | WASM and JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for microphone capture |
| `WS /` | Main WebSocket (events, terminal I/O, presence protocol) |
| `WS /vnc` | WebSocket-to-TCP VNC proxy (for noVNC) |

## Requirements

- **Microphone access requires a secure context**: Use `localhost` (via SSH tunnel: `ssh -L 8765:localhost:8765 host`), or set browser flags for insecure origins
- **API key for voice**: Gemini or OpenAI (stored browser-side only). Voice is optional — the dashboard works without it
- **WASM**: The dashboard uses a compiled WASM module (`presence-web` crate). Rebuild with `wasm-pack build --target web` from `crates/presence-web/` if you modify the Rust code
