# CLAUDE.md

## Project Overview

This is **Agent**, a Rust runtime for autonomous AI agents with process lifecycle management. It executes bash commands on behalf of AI agents, tracks process state via shared memory, streams status updates, and persists logs across binary restarts.

The project produces two binaries:
- **agent** — Command runtime that reads JSON from stdin, spawns bash commands, and writes status lines to stdout
- **caller** — AI integration layer that drives the agent via the OpenAI chat completions API in a loop

## Repository Structure

```
src/
├── main.rs              # Agent binary entry point (tokio async main)
├── agent.rs             # Core agent implementation (~1200 lines)
│                        #   - Shared memory management
│                        #   - Command execution (execAsAgent)
│                        #   - Screenshot capture (captureScreen)
│                        #   - Status fetching (fetchStatus)
│                        #   - Path inspection (inspectPath)
│                        #   - Dependency checking and nonce replacement
├── models.rs            # Data structures: Command, AgentInput, ProcessInfo, ProcessStatus, StatusUpdate
├── error.rs             # AgentError enum (Io, Json, Process, InvalidNonce)
├── utils.rs             # get_timestamp(), format_status_output()
├── status_monitor.rs    # Background task polling shared memory every 100ms
└── bin/
    └── caller/
        ├── main.rs          # Caller entry point, JSON extraction, main loop (max 50 turns)
        ├── openai.rs        # OpenAI chat API client
        ├── conversation.rs  # Message management (system/user/assistant roles)
        ├── agent_runner.rs  # Spawns agent subprocess, manages I/O with timeouts
        └── error.rs         # CallerError enum
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

Running the caller (requires `.env` with `OPENAI_API_KEY`):
```bash
./target/release/caller "List the files in /tmp"
```

## Testing

```bash
cargo test                # Run all 73 tests
cargo test -- --list      # List all test names
```

All tests are inline `#[cfg(test)]` modules in the same files as the code they test. Async tests use `#[tokio::test]`. The `tempfile` crate provides isolated temporary directories for tests that touch the filesystem or shared memory.

Test coverage includes:
- **agent.rs**: Process info operations, dependency checking, command execution, status fetching, path inspection, nonce reference replacement, process mapping
- **models.rs**: Serialization roundtrips, deserialization of minimal/full commands, repr(C) layout
- **error.rs**: Display formatting, From conversions
- **utils.rs**: Timestamp validity, status output formatting
- **caller/main.rs**: JSON extraction from code fences and bare text
- **caller/conversation.rs**: Message ordering, serialization
- **caller/error.rs**: Display formatting, type conversions

## Architecture Details

### Shared Memory

Process state lives in `/dev/shm/agent_processes` — a fixed-size array of 1024 `ProcessInfo` slots (repr(C) structs). Each slot holds: nonce (u64), PID (i32), status (u8), exit code (i32), timestamp (u64). This survives binary restarts since `/dev/shm` is tmpfs.

The process map (`HashMap<u64, usize>`) is rebuilt from shared memory on every startup by scanning all 1024 slots for non-zero nonces.

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
| `reqwest` (rustls-tls) | HTTP client for OpenAI API |
| `dotenvy` | .env file loading |
| `tempfile` (dev) | Temporary directories in tests |

## Environment Requirements

- **OS**: Linux (requires `/dev/shm` for shared memory)
- **Permissions**: Root access expected
- **For caller**: `.env` file with `OPENAI_API_KEY` (or `OPENAI`), optional `MODEL_NAME` (defaults to gpt-4o)
- **For captureScreen**: ImageMagick `import` command and DISPLAY environment variable (defaults to `:1`)

## CI/CD

No CI/CD is currently configured. Run `cargo test` and `cargo clippy` locally before committing.
