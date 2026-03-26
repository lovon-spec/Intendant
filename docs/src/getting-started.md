# Getting Started

## Building

```bash
cargo build --release
```

Two binaries are produced:
- `./target/release/intendant-runtime` — the command runtime
- `./target/release/intendant` — the AI CLI/TUI/Web

### Installing

```bash
cargo install --path .
```

Both binaries are installed to `~/.cargo/bin/`. The `intendant` binary embeds default system prompts and web assets (HTML, WASM) at compile time, so it works immediately from any directory without needing the source tree.

### Prerequisites

- **Rust** toolchain (stable)
- **wasm-pack** — `cargo install wasm-pack` (auto-rebuilds WASM on source changes)
- **ffmpeg** — required for display recording (`brew install ffmpeg` / `apt install ffmpeg`)
- **macOS**: `./scripts/setup-macos.sh` installs all platform dependencies
- **Linux**: `sudo apt install imagemagick xdotool xvfb x11vnc ffmpeg`

### WASM auto-rebuild

The `build.rs` script automatically rebuilds WASM when `crates/presence-web/` or `crates/presence-core/` source files change. This requires `wasm-pack` to be installed. If not installed, `cargo build` prints a warning and skips the WASM rebuild.

To rebuild manually:

```bash
cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
cargo build --release -p intendant   # Re-embed WASM
```

## Setup

Create a `.env` file or export the variables. The caller searches for `.env` in this order:

1. **Current directory** (and parent directories)
2. **Project root** (git root)
3. **Global config** (`~/.config/intendant/.env`)

For global use after `cargo install`, put your keys in `~/.config/intendant/.env`:

```bash
# OpenAI
OPENAI_API_KEY=sk-...

# Or Anthropic
ANTHROPIC_API_KEY=sk-ant-...

# Or Gemini (Google AI)
GEMINI_API_KEY=AI...

# If multiple keys are set, choose one:
PROVIDER=openai          # or "anthropic" or "gemini"

MODEL_NAME=gpt-5.2-codex # optional, provider-specific default used if omitted

# Disable native tool calling (fall back to text-based JSON extraction)
# USE_NATIVE_TOOLS=false
```

## Running

```bash
# With a task as CLI argument (launches TUI)
./target/release/intendant "List the files in /tmp"

# Headless mode (no TUI, plain text output)
./target/release/intendant --no-tui "List the files in /tmp"

# With autonomy level
./target/release/intendant --autonomy low "rm -rf /tmp/test"

# Specify provider and model
./target/release/intendant --provider anthropic --model claude-sonnet-4-5-20250929 "List files"

# Use Gemini provider
./target/release/intendant --provider gemini --model gemini-2.5-pro "List files"

# Interactive mode (prompts for task on stdin)
./target/release/intendant

# Verbose output (show debug-level log entries)
./target/release/intendant --verbose "echo hello"

# JSONL structured output (implies --no-tui)
./target/release/intendant --json "echo hello"

# Resume most recent session for this project
./target/release/intendant --continue "fix that bug"

# Resume specific session by ID or prefix
./target/release/intendant --resume abc123 "continue"

# Force single-agent mode (skip orchestrator)
./target/release/intendant --direct "simple task"

# Web dashboard (Activity + Usage + Terminal + Displays, default port 8765)
./target/release/intendant --web

# Web dashboard on custom port
./target/release/intendant --web 9000

# Enable filesystem sandboxing (Landlock, Linux 5.13+)
./target/release/intendant --sandbox "run tests"

# Run as MCP server (stdio transport)
./target/release/intendant --mcp "Deploy the application"

# Enable Unix control socket
./target/release/intendant --control-socket "task"

# Disable the presence layer
./target/release/intendant --no-presence "task"

# Pipe input (auto-detects non-TTY, runs headless)
echo "task" | ./target/release/intendant
```

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

The test suite covers both binaries with inline `#[cfg(test)]` modules. See [Session Logging](./session-logging.md) for the full test coverage summary.

Integration tests in `tests/e2e/` spawn a real binary and make real API calls — see [Architecture](./architecture.md) for details.
