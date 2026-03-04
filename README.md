# Intendant

A Rust runtime for autonomous AI agents with process lifecycle management. Intendant executes commands on behalf of AI agents, tracks process state in memory, and persists structured session logs. The CLI supports OpenAI, Anthropic, and Gemini APIs with native tool calling, a ratatui TUI, configurable autonomy, MCP server/client, and multi-agent orchestration.

## Architecture

```
stdin (JSON) --> intendant-runtime --> executes commands sequentially (blocking)
                  |
                  +--> in-memory process state (HashMap<nonce, ProcessInfo>)
                  +--> $INTENDANT_LOG_DIR/  (stdout/stderr logs per nonce)
                  |
                  +--> stdout (result lines with exit code, stdout/stderr tail)

intendant (3 modes) --> detects project root (git) --> loads memory/knowledge
  |
  +--> User Mode:       spawns orchestrator subprocess, monitors progress (no API calls)
  +--> Sub-Agent Mode:  scoped task, writes results/progress, isolated context
  +--> Direct Mode:     single-loop execution for simple tasks
  |
  +--> Native tool calling (OpenAI/Anthropic/Gemini) with text extraction fallback
  +--> Streaming output:  SSE-based token streaming for all 3 providers
  +--> Ratatui TUI:     status bar, scrollable log, approval panel, askHuman input
  +--> MCP Server:      --mcp flag, stdio transport, full parity with TUI (tools + resources)
  +--> MCP Client:      connects to external MCP servers (configured in intendant.toml)
  +--> Autonomy system: Low/Medium/High/Full + per-category rules from intendant.toml
  +--> Landlock sandbox: filesystem restrictions on agent runtime (Linux)
  +--> Prompt caching:  Anthropic cache_control, OpenAI/Gemini implicit caching
  +--> Auto-compaction: triggers at 90% context usage, preserves system+tail messages
  +--> Optional control socket (--control-socket): /tmp/intendant-<pid>.sock (JSON-line protocol)
  +--> Voice gateway (--voice-gateway): WebSocket + Gemini Live API for phone-based voice control
  +--> Token budget tracking (context-window-aware loop termination)
  +--> Sub-agent spawning via env vars (INTENDANT_ROLE, INTENDANT_ID, etc.)
  +--> Git worktree isolation for implementation agents
  +--> Tagged knowledge store with pub/sub channels between agents
  +--> Presence layer: conversational mediator between user and agent loop
```

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
```

## Testing

```bash
cargo test
```

## Documentation

See the [full documentation](docs/src/SUMMARY.md) for detailed guides on configuration, the runtime protocol, TUI & autonomy, MCP server, integrations, and session logging.

Build the docs locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
