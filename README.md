<p align="center">
  <img src="static/icon-128.png" width="96" alt="Intendant" />
</p>

# Intendant

An autonomous AI agent operating environment written in Rust. Intendant gives AI agents a full desktop to work in — shell access, file editing, a graphical display they can see and control via computer use, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system. Provider-agnostic (OpenAI, Anthropic, Gemini), cross-platform (macOS, Linux), accessible through CLI, TUI, web dashboard, MCP, or voice.

## Architecture

```
                          ┌──────────────────────────────────────────┐
                          │           intendant (controller)         │
                          │                                          │
  Web Dashboard ◄─────────┤  presence ─── agent loop ────┐           │
  TUI / MCP     ◄─────────┤     │            │           │           │
  Voice         ◄─────────┤     │      ┌─────┴──────┐    │           │
                          │     │      │ sub-agents │    │           │
                          │     │      └────────────┘    │           │
                          └─────┼────────────────────────┼───────────┘
                                │                        │
                    ┌───────────┤                        │
                    │           │                        │
                    v           v                        v
              Voice APIs   Model APIs              intendant-runtime
           (Gemini Live,  (OpenAI/Anthropic/       (sandboxed command
            OAI Realtime)  Gemini + streaming)      execution, Landlock)
```

**Presence layer** — a separate AI that mediates between user and agent. Handles conversation, dispatches tasks, narrates events, manages approval gates. Runs as server-side text or browser-side voice (Gemini Live / OpenAI Realtime via WASM).

**WebRTC display pipeline** — agents see and interact with graphical displays through a custom WebRTC transport with hardware H264 encoding (VideoToolbox on macOS, VA-API/x264 on Linux), VP8 fallback, bidirectional clipboard sync, and multi-monitor support.

**Phone calls** — outbound SIP calls via pjsua with a voice model conducting the conversation, returning structured data.

Three execution modes: *direct* (single agent), *user* (orchestrator + sub-agents in git worktrees), *sub-agent* (scoped child task).

## Dependencies

- **Rust** toolchain (stable)
- **wasm-pack** — `cargo install wasm-pack`
- **ffmpeg** — display recording and H264 encoding
- **macOS**: `./scripts/setup-macos.sh` installs everything (cliclick, ffmpeg, Vortex Audio, wasm-pack, app bundle)
- **Linux**: `./scripts/setup-linux.sh` installs everything (libvpx, libxcb, xdotool, PipeWire, ffmpeg, PulseAudio, Xvfb)

## Quick Start

```bash
# Build
cargo build --release

# Set up API keys (~/.config/intendant/.env for global use)
echo 'OPENAI_API_KEY=sk-...' > .env

# Run with TUI
./target/release/intendant "List the files in /tmp"

# Headless mode
./target/release/intendant --no-tui "echo hello"

# Choose provider/model
./target/release/intendant --provider anthropic --model claude-sonnet-4-6-20250929 "Fix the tests"

# Web dashboard (default port 8765)
./target/release/intendant --web

# Run as MCP server (for Claude Code, etc.)
./target/release/intendant --mcp "Deploy the application"

# JSONL structured output
./target/release/intendant --json "echo hello"

# Resume most recent session
./target/release/intendant --continue "fix that bug"

# Force single-agent mode
./target/release/intendant --direct "simple task"

# Enable Landlock sandboxing (Linux)
./target/release/intendant --sandbox "run tests"
```

## Web Dashboard

The `--web` flag starts a web server (default port 8765) with a multi-tab dashboard:

- **Activity** — Live event log with color-coded entries, approval buttons, follow-up input
- **Stats** — Token usage per model with cost estimates, disk usage
- **Terminal** — Embedded xterm.js connected to the server-side TUI
- **Video** — WebRTC display viewers with remote control, recording replay, annotations
- **Sessions** — Session browser with recording playback

Optional **live voice** via Gemini Live or OpenAI Realtime — the browser connects directly to the model's realtime API through WASM with presence tools for approving actions, submitting tasks, and querying status by voice.

Late-connecting browsers receive the full session replay and cached state.

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

## Documentation

**[Read the full documentation](https://lovon-spec.github.io/intendant/)** — covers architecture, configuration, runtime protocol, display pipeline, computer use, live audio, TUI & autonomy, multi-agent orchestration, the presence layer, web gateway, MCP, integrations, and session logging.

Or build locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
