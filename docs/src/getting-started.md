# Getting Started

## Building

```bash
cargo build --release
```

Two binaries are produced:
- `./target/release/intendant-runtime` — the command runtime
- `./target/release/intendant` — the AI CLI/TUI

### Installing

```bash
cargo install --path .
```

Both binaries are installed to `~/.cargo/bin/`. The `intendant` binary embeds default system prompts at compile time, so it works immediately from any directory without needing the source tree.

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

# Web TUI (remote terminal + optional voice, default port 8765)
./target/release/intendant --web

# Web TUI on custom port
./target/release/intendant --web 9000
```

## Testing

```bash
cargo test
```

The test suite covers both binaries. See [Session Logging](./session-logging.md) for the full test coverage summary.
