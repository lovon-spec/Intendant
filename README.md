# Intendant

A Rust runtime for autonomous AI agents with process lifecycle management. Intendant executes commands on behalf of AI agents, tracks process state in memory, and persists structured session logs. It supports OpenAI, Anthropic, and Gemini APIs with native tool calling, streaming, a ratatui TUI, a web dashboard with live voice interaction, configurable autonomy and approval gates, MCP server/client, multi-agent orchestration, a conversational presence layer, and session resume.

## Architecture

```
                          ┌────────────────────────────────────┐
                          │        intendant (caller)          │
                          │                                    │
  Web Dashboard ◄─────────┤  presence ─── agent loop ───┐     │
  TUI / MCP     ◄─────────┤     │            │          │     │
                          │     │      ┌─────┴──────┐   │     │
                          │     │      │ sub-agents  │   │     │
                          │     │      └────────────┘   │     │
                          └─────┼────────────────────────┼─────┘
                                │                        │
                                v                        v
                          Model APIs              intendant-runtime
                     (OpenAI/Anthropic/           (sequential command
                      Gemini + streaming)          execution, stdin/stdout)
```

**Web dashboard** (`--web`) serves a 4-tab app at `/` with activity log, usage tracking with cost estimates, embedded terminal, and remote display viewing via noVNC. Optional live voice interaction via Gemini Live or OpenAI Realtime.

**Presence layer** mediates between the user and agent loop — handles conversation, dispatches tasks, narrates events. Runs as server-side text or browser-side voice, with mutual exclusion and session continuity across reconnects.

Three execution modes: *direct* (single agent), *user* (orchestrator + sub-agents), *sub-agent* (scoped child task).

## Dependencies

- **Rust** toolchain (stable)
- **wasm-pack** — `cargo install wasm-pack` (auto-rebuilds WASM when presence-web source changes)
- **ffmpeg** — required for display recording (`brew install ffmpeg` / `apt install ffmpeg`)
- **macOS**: `./scripts/setup-macos.sh` installs all dependencies (cliclick, ffmpeg, wasm-pack, etc.)
- **Linux**: ImageMagick (`import`), xdotool, Xvfb, x11vnc, ffmpeg — `sudo apt install imagemagick xdotool xvfb x11vnc ffmpeg`

## Quick Start

```bash
# Build
cargo build --release

# Install (optional)
cargo install --path .

# Set up API keys (~/.config/intendant/.env for global use)
echo 'OPENAI_API_KEY=sk-...' > .env

# Run with TUI
./target/release/intendant "List the files in /tmp"

# Headless mode
./target/release/intendant --no-tui "echo hello"

# Choose provider/model
./target/release/intendant --provider anthropic --model claude-sonnet-4-5-20250929 "Fix the tests"

# Web dashboard (Activity + Usage + Terminal + Displays, default port 8765)
./target/release/intendant --web

# Run as MCP server
./target/release/intendant --mcp "Deploy the application"

# JSONL structured output
./target/release/intendant --json "echo hello"

# Resume most recent session
./target/release/intendant --continue "fix that bug"

# Force single-agent mode (skip orchestrator)
./target/release/intendant --direct "simple task"

# Enable filesystem sandboxing (Landlock, Linux 5.13+)
./target/release/intendant --sandbox "run tests"
```

## Web Dashboard

The `--web` flag starts a web server (default port 8765) serving a modern dashboard at `/`:

- **Activity** — Live event log from agent loop, presence, and voice model with color-coded entries and turn separators
- **Usage** — Token consumption for main and presence models with cost estimates (built-in pricing for OpenAI, Anthropic, Gemini)
- **Terminal** — Embedded xterm.js connected to the server-side ratatui TUI
- **Displays** — noVNC viewers for each Xvfb display created by the agent

Optional **live voice** via Gemini Live or OpenAI Realtime — the browser connects directly to the model's realtime API with presence tools for approving actions, submitting tasks, and querying status by voice.

Late-connecting browsers receive the full session log replay and cached state, so you can open the dashboard at any point and see everything that's happened.

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

## Documentation

**[Read the full documentation](https://lovon-spec.github.io/intendant/)** — covers architecture, configuration, runtime protocol, TUI & autonomy, multi-agent orchestration, the presence layer, web gateway, MCP server, integrations, and session logging.

Or build locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
