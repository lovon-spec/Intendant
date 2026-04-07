# Computer Use & Live Audio

## Computer Use

Intendant provides a provider-agnostic computer use (CU) abstraction that lets AI models see and interact with graphical desktops.

### Architecture

```
Model (CU actions) → computer_use.rs → DisplayBackend → platform tools
                                            │
                              ┌──────────────┼──────────────┐
                              │              │              │
                           X11           Wayland         macOS
                        (xdotool)       (ydotool)      (cliclick)
                      (ImageMagick)      (grim)      (screencapture)
```

The `DisplayBackend` enum detects the available backend at runtime:

| Backend | Capture | Input | Platform |
|---------|---------|-------|----------|
| X11 | ImageMagick `import` | xdotool | Linux (X11) |
| Wayland | grim | ydotool | Linux (Wayland) |
| macOS | screencapture | cliclick | macOS |

### Display Targets

CU actions operate on a `DisplayTarget`:

- **`Virtual { id }`** — an Xvfb-managed virtual display (`:99`, `:100`, etc.)
- **`UserSession`** — the user's real desktop, requires explicit `DisplayControl` autonomy grant

User session display access uses a session-grant model: approve once via the `d` hotkey in the TUI or the dashboard, revoke anytime. On grant, the display becomes available for CU actions and WebRTC streaming.

### CU-First Routing

When the presence layer submits a task, it can include a `display_target` hint. Display-oriented tasks (clicking buttons, filling forms, navigating apps) are routed to a fast CU model first, with automatic escalation to the heavy coding agent if the task requires code changes.

The routing heuristic:
1. `submit_task` includes `display_target` → route to CU model
2. `submit_task` includes `reference_frame_ids` → route to CU model
3. Otherwise → route to main agent

### Configuration

```toml
[computer_use]
provider = "gemini"        # provider for CU model (optional)
model = "gemini-3-flash"   # model for CU tasks (optional)
backend = "auto"           # "x11", "wayland", "macos", or "auto" (default)
```

## Live Audio

The `spawn_live_audio` tool connects to voice AI APIs (Gemini Live or OpenAI Realtime) and pipes audio through a virtual audio bridge on the host system.

### How It Works

```
AI Voice Model ──WebSocket──▶ Intendant ──audio bridge──▶ Application
     │                            │                          │
     │   structured responses     │   virtual mic/speaker    │
     │   (validated + quarantined)│   (PulseAudio/BlackHole) │
     │◀──────────────────────────│◀─────────────────────────│
```

1. The agent calls `spawn_live_audio` with a provider, playbook, and optional response schema
2. Intendant creates a virtual audio bridge connecting the voice model to the target application
3. The voice model follows the playbook (e.g., conducting a phone call, navigating a voice menu)
4. Model responses are validated against the declared schema
5. The tool returns structured data when the conversation completes

### Security

Live audio sessions are **untrusted**: the voice model has zero tools and zero file access. All safety measures:

- **Schema validation** (`schema_validator.rs`): Responses are checked against the declared schema, with oversized fields truncated
- **Quarantine** (`quarantine.rs`): Unexpected content (tool call attempts, oversized strings, off-schema data) is written to `~/.intendant/quarantine/<session_id>/` and never exposed to the agent
- **Sandbox**: When Landlock is enabled, live audio processes can only write to the session log dir and quarantine dir — no project root, no `/tmp`

### Audio Routing

The virtual audio bridge creates bidirectional audio channels so applications "hear" model audio as microphone input, and the model captures application audio as speaker output.

| Platform | Implementation |
|----------|---------------|
| Linux | PulseAudio null sinks via `pactl`/`parec`/`pacat` |
| macOS | BlackHole 2ch + 16ch with SwitchAudioSource + sox, or Vortex Audio (preferred, shared memory ring buffers) |

Audio routing is optional — browser-based voice interaction (Gemini Live / OpenAI Realtime via the web dashboard) works without it.

### Silence Watchdog

A watchdog monitors live audio sessions for prolonged silence or unresponsive model turns. After 6 consecutive unresponsive turns, a JSON nudge is injected to prompt the model to continue.

### Configuration

```toml
[live_audio]
enabled = true
default_timeout_secs = 300     # session timeout (default: 5 minutes)
gemini_model = "gemini-2.5-flash-native-audio-preview-12-2025"
openai_model = "gpt-4o-realtime-preview"
sample_rate = 24000            # audio sample rate (default: 24000)
```

## Phone Calls

The phone-call skill combines `spawn_live_audio` with SIP telephony to make outbound phone calls:

```
AI Voice Model ──shared memory──▶ Vortex Audio ──▶ pjsua (SIP/SRTP) ──▶ Phone
     │                                                                      │
     │◀─────────────────────────────────────────────────────────────────────│
```

### How It Works

1. The agent invokes the `phone-call` skill with a phone number, playbook, and response schema
2. A SIP call is placed via pjsua with audio routed through Vortex Audio (macOS) or PulseAudio (Linux)
3. The voice model conducts the conversation following the playbook
4. On call completion, structured data is extracted per the response schema
5. User-provided content from the call is flagged as "tainted" in the response

### Playbook Format

The playbook is a natural language script that tells the voice model how to conduct the call:

- Greeting and introduction
- Questions to ask
- How to handle different responses
- When to end the call
- What data to extract

### Response Schema

A typed schema defines what structured data to extract from the call:

```json
{
  "fields": [
    {"name": "confirmed", "type": "boolean", "description": "Whether the appointment was confirmed"},
    {"name": "new_time", "type": "string", "description": "Rescheduled time if changed", "tainted": true}
  ]
}
```

Fields marked `tainted: true` contain user-provided content and are handled with appropriate caution.

## Transcription

Server-side audio transcription via the Whisper API processes browser microphone audio:

1. The web gateway buffers `user_audio` WebSocket messages in ~3-second chunks
2. Chunks are filtered by RMS energy to skip silence
3. Audio is wrapped in WAV format and sent to the transcription API
4. Transcripts are broadcast as `user_transcript` events

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
language = "en"              # ISO-639-1 hint (optional)
# endpoint = "http://..."    # custom endpoint for self-hosted whisper.cpp
```
