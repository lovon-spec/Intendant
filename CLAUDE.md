# CLAUDE.md

## Project Overview

This is **Agent**, a Rust runtime for autonomous AI agents with process lifecycle management. It executes bash commands on behalf of AI agents, tracks process state via shared memory, streams status updates, and persists logs across binary restarts.

The project produces two binaries:
- **agent** — Command runtime that reads JSON from stdin, spawns bash commands, and writes status lines to stdout
- **caller** — AI integration layer that drives the agent via the OpenAI or Anthropic chat completions API in a loop

## Repository Structure

```
src/
├── main.rs              # Agent binary entry point (tokio async main)
├── agent.rs             # Core agent implementation (~3000 lines)
│                        #   - Shared memory management
│                        #   - Command execution (execAsAgent)
│                        #   - Screenshot capture (captureScreen)
│                        #   - Status fetching (fetchStatus) with log tail
│                        #   - Path inspection (inspectPath)
│                        #   - File editing (editFile)
│                        #   - Web browsing (browse)
│                        #   - Human interaction (askHuman)
│                        #   - PTY sessions (execPty)
│                        #   - Memory storage/recall with tagged knowledge (storeMemory, recallMemory)
│                        #   - Dependency checking and nonce replacement
├── models.rs            # Data structures: Command, AgentInput, ProcessInfo, ProcessStatus, StatusUpdate
├── error.rs             # AgentError enum (Io, Json, Process, InvalidNonce)
├── utils.rs             # get_timestamp(), format_status_output()
├── status_monitor.rs    # Background task polling shared memory every 100ms
└── bin/
    └── caller/
        ├── main.rs          # Caller entry point: 3 modes (user/sub-agent/direct), budget-aware loop
        ├── provider.rs      # Multi-provider API client with token usage tracking, ChatProvider trait
        ├── conversation.rs  # Message management with layer protection, drop/summarize, budget tracking
        ├── agent_runner.rs  # Spawns agent subprocess, manages I/O with timeouts
        ├── knowledge.rs     # Tagged knowledge store with pub/sub channels, cursor-based routing
        ├── memory.rs        # Backward-compatible memory wrapper delegating to knowledge.rs
        ├── sub_agent.rs     # Sub-agent spawning, result/progress I/O, role-specific configuration
        ├── worktree.rs      # Git worktree management for isolated implementation agents
        ├── user_mode.rs     # User-mode orchestrator spawning, progress monitoring, input relay
        ├── project.rs       # Project detection (git root), config parsing (agent.toml)
        └── error.rs         # CallerError enum
SysPrompt.md                 # Default system prompt (direct mode)
SysPrompt_user.md            # User-facing mode prompt
SysPrompt_orchestrator.md    # Orchestrator agent prompt
SysPrompt_research.md        # Research sub-agent prompt
SysPrompt_implementation.md  # Implementation sub-agent prompt
```

## Build and Run

```bash
cargo build --release     # Produces target/release/agent and target/release/caller
cargo build               # Debug build
cargo check               # Type-check without building
```

Running the agent:
```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' | ./target/release/agent
```

Running the caller (requires `.env` with API key):
```bash
./target/release/caller "List the files in /tmp"
```

## Testing

```bash
cargo test                # Run all 267 tests
cargo test -- --list      # List all test names
```

All tests are inline `#[cfg(test)]` modules in the same files as the code they test. Async tests use `#[tokio::test]`. The `tempfile` crate provides isolated temporary directories for tests that touch the filesystem or shared memory.

Test coverage includes:
- **agent.rs** (114 tests): Process info operations, dependency checking, command execution, status fetching with log tail, path inspection, nonce reference replacement, process mapping, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage/recall with tags and filters, synchronous command shared memory registration, cross-command-type dependency chaining
- **models.rs**: Serialization roundtrips, deserialization of minimal/full commands, repr(C) layout
- **error.rs**: Display formatting, From conversions
- **utils.rs**: Timestamp validity, status output formatting
- **caller/main.rs** (153 tests total across caller modules): JSON extraction, context directives, budget constants, task classification
- **caller/conversation.rs**: Message ordering, serialization, drop/summarize turns, message layer protection, budget tracking
- **caller/knowledge.rs**: Pub/sub lifecycle, subscription/cursor tracking, tag/channel/keyword filtering, old format migration, save/load roundtrip, knowledge routing
- **caller/sub_agent.rs**: Spawn command generation, result/progress I/O, serialization, role roundtrips, directory scanning
- **caller/worktree.rs**: Full lifecycle (create/list/merge/remove), conflict handling
- **caller/user_mode.rs**: Orchestrator spec generation, progress formatting, input relay, prompt resolution
- **caller/project.rs**: Config parsing, project paths, sub-agent directory
- **caller/memory.rs**: Memory/knowledge loading, formatting, format migration
- **caller/provider.rs**: Provider selection, token usage parsing, context window defaults
- **caller/error.rs**: Display formatting, type conversions

## Architecture Details

### Shared Memory

Process state lives in `/dev/shm/agent_processes` — a fixed-size array of 1024 `ProcessInfo` slots (repr(C) structs). Each slot holds: nonce (u64), PID (i32), status (u8), exit code (i32), timestamp (u64). This survives binary restarts since `/dev/shm` is tmpfs.

The process map (`HashMap<u64, usize>`) is rebuilt from shared memory on every startup by scanning all 1024 slots for non-zero nonces.

All command nonces (both async and synchronous) are pre-registered in shared memory with `Waiting` status before execution begins. Synchronous commands update their status to `Completed`/`Failed` after execution. This enables dependency chaining across command types (e.g., `editFile` -> `execAsAgent` via `depending_nonce`).

### Session Persistence

`/dev/shm/agent_session` stores the log directory path. Consecutive runs reuse the same log directory (`/var/log/agent/<timestamp>/`). To reset: `rm -f /dev/shm/agent_processes /dev/shm/agent_session`.

### Status Protocol

Status lines are formatted as `[nonce][status_char][exit_code]`:
- `r` = Running, `c` = Completed, `f` = Failed, `s` = Skipped, `w` = Waiting
- Example: `42c0` means nonce 42 completed with exit code 0

### Command Dependencies

Commands chain via `depending_nonce`, `wait`, and `expected_status`. When `wait` is true, execution blocks until the dependency finishes. When false, the command is skipped if the dependency hasn't completed yet.

### Nonce Variables

`$NONCE[id]` in command strings is replaced with the PID of the process launched by that nonce. Handled by regex-based substitution in `replace_nonce_refs()`.

### Caller Flow

The caller operates in three modes based on environment:

**Sub-Agent Mode** (`AGENT_ROLE` set): Runs with scoped task, writes progress/results to files, uses role-specific system prompt.

**User Mode** (complex task, no `AGENT_ROLE`): Spawns orchestrator sub-agent, monitors progress, relays to user. User-layer messages are protected from auto-pruning.

**Direct Mode** (simple task, no `AGENT_ROLE`): Single-loop execution:
1. Selects API provider (OpenAI or Anthropic) from env
2. Detects project root via git, loads `agent.toml` config
3. Reads role-appropriate system prompt
4. Injects project knowledge into conversation
5. Budget-aware loop (stops at context exhaustion or 500-turn safety cap): send to model -> extract JSON -> apply context directives -> inject project context -> pipe to agent -> append budget summary -> feed output back

## Code Conventions

- **Rust 2021 edition** with default rustfmt and clippy settings (no .rustfmt.toml or .clippy.toml)
- **Naming**: snake_case for functions/modules, PascalCase for types, SCREAMING_SNAKE_CASE for constants
- **Error handling**: Custom `thiserror`-based enums (`AgentError`, `CallerError`) with `Result<T>` returns
- **Async**: tokio with full features; background tasks via `tokio::spawn`
- **Shared state**: `Arc<RwLock<T>>` for mutable shared state, `mpsc` channels for communication
- **Unsafe code**: Used sparingly for memory-mapped file pointer operations (reading/writing `ProcessInfo` structs to shared memory)
- **Tests**: Always inline `#[cfg(test)]` modules — no separate test files

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` (full) | Async runtime |
| `serde` + `serde_json` | JSON serialization/deserialization |
| `thiserror` | Error type derivation |
| `memmap2` | Memory-mapped files for shared memory |
| `chrono` | Timestamp formatting for log directories |
| `env_logger` | Logging |
| `regex` | $NONCE[id] pattern matching |
| `reqwest` (rustls-tls) | HTTP client for API calls |
| `html2text` | HTML to plain text conversion for browse |
| `portable-pty` | PTY session management for execPty |
| `dotenvy` | .env file loading |
| `toml` | agent.toml config parsing |
| `async-trait` | Async trait support for ChatProvider |
| `tempfile` (dev) | Temporary directories in tests |

## Environment Requirements

- **OS**: Linux (requires `/dev/shm` for shared memory)
- **Permissions**: Runs as unprivileged user with passwordless sudo
- **For caller**: `.env` file with `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, optional `PROVIDER` and `MODEL_NAME`
- **For captureScreen**: ImageMagick `import` command and DISPLAY environment variable (defaults to `:1`)

## CI/CD

No CI/CD is currently configured. Run `cargo test` and `cargo clippy` locally before committing.
