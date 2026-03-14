# Intendant

A Rust runtime for autonomous AI agents with process lifecycle management. Intendant executes commands on behalf of AI agents, tracks process state in memory, and persists structured session logs. It supports OpenAI, Anthropic, and Gemini APIs with native tool calling, streaming, a ratatui TUI, configurable autonomy and approval gates, MCP server/client, multi-agent orchestration, a conversational presence layer, browser-based voice interaction, and session resume.

## Architecture

```
                          ┌─────────────────────────────┐
                          │     intendant (caller)       │
                          │                             │
  TUI / MCP / Web ◄─────┤  presence ── agent loop ──┐ │
                          │     │           │         │ │
                          │     │      ┌────┴────┐    │ │
                          │     │      │ sub-agents│   │ │
                          │     │      └─────────┘    │ │
                          └─────┼─────────────────────┼─┘
                                │                     │
                                v                     v
                          Model APIs           intendant-runtime
                     (OpenAI/Anthropic/        (sequential command
                      Gemini + streaming)       execution, stdin/stdout)
```

**Presence layer** mediates between the user and agent loop — handles conversation, dispatches tasks, narrates events. Runs as server-side text or browser-side voice (Gemini Live / OpenAI Realtime), with mutual exclusion.

Three execution modes: *direct* (single agent), *user* (orchestrator + sub-agents), *sub-agent* (scoped child task).

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

# Run as MCP server
./target/release/intendant --mcp "Deploy the application"

# Web gateway (remote TUI + optional voice from browser)
./target/release/intendant --web

# JSONL structured output
./target/release/intendant --json "echo hello"

# Resume most recent session
./target/release/intendant --continue "fix that bug"

# Force single-agent mode (skip orchestrator)
./target/release/intendant --direct "simple task"

# Enable filesystem sandboxing (Landlock, Linux 5.13+)
./target/release/intendant --sandbox "run tests"
```

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

## Documentation

**[Read the full documentation](https://lovon-spec.github.io/intendant/)** — covers architecture, configuration, runtime protocol, TUI & autonomy, multi-agent orchestration, the presence layer, MCP server, integrations, and session logging.

Or build locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
