# CLAUDE.md

## What Intendant Is

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop to work in — shell access, file editing, a graphical display it can see and control, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system.

Two binaries form a security boundary:

- **intendant-runtime** — Sandboxed command executor. Reads JSON commands from stdin, executes them sequentially, writes results to stdout. Runs under Landlock filesystem restrictions. Never holds API keys.
- **intendant** — Controller/caller. Manages the LLM conversation loop, calls model APIs, dispatches tool calls to the runtime subprocess, and runs all user-facing interfaces (CLI, TUI, Web, MCP).

The system is **provider-agnostic** (OpenAI, Anthropic, Gemini), **cross-platform** (macOS, Linux/Debian), and designed around the principle that every capability should be accessible through any interface — TUI, web dashboard, MCP, voice, or programmatic control.

## Vision and Direction

Intendant is evolving toward an **always-on AI steward**: a persistent daemon that manages tasks, sees screens, hears voice, makes phone calls, coordinates sub-agents, and is accessible from any device. The arc is:

1. CLI tool that executes agent commands (done)
2. TUI application with approval gates (done)
3. Web dashboard with real-time streaming (done)
4. Voice-interactive presence layer (done)
5. Full desktop agent with display control and computer use (done)
6. WebRTC display transport with hardware encoding (done)
7. Phone call capability via SIP (done)
8. Persistent daemon with background agents, scheduled tasks, cross-session knowledge (in progress)

## Architecture Pillars

### 1. Agent Execution

The core loop: select provider, load prompts/skills/knowledge, run a budget-aware conversation loop that dispatches tool calls to the runtime subprocess. Stops at context exhaustion, explicit `done` signal, or turn cap.

Three execution modes:
- **Direct** (`--direct`): Single agent loop
- **User**: Spawns an orchestrator that decomposes tasks and delegates to specialized sub-agents (research, implementation, testing) running in isolated git worktrees
- **Sub-Agent** (`INTENDANT_ROLE` set): Scoped child task with role-specific prompt

### 2. Presence Layer

A separate AI (defaulting to a fast model like Gemini Flash) that mediates between the human and the agent system. It observes agent state, narrates events, dispatches tasks, handles approval gates, and maintains conversational continuity. "You ARE Intendant" — the user talks to presence, not directly to the worker agent.

Runs in two modes (mutually exclusive):
- **Server-side text** (`presence.rs`): For TUI and non-voice web
- **Browser-side voice** (`crates/presence-web/`): WASM-powered, connects directly to Gemini Live or OpenAI Realtime APIs from the browser

The presence-core crate compiles to both native Rust and WASM, ensuring identical tool definitions and dispatch logic everywhere.

### 3. Display Pipeline (WebRTC)

Agents can see and interact with graphical displays via a custom WebRTC transport:

```
[CaptureBackend] → encode (VP8/H264) → WebRTC track → browser
  browser input  → WebRTC data channel → input injection
```

Platform capture backends: X11 (XShm), Wayland (PipeWire DMA-BUF zero-copy), macOS (ScreenCaptureKit). Hardware H264 encoding via VideoToolbox (macOS) and VA-API/libx264 (Linux) with VP8 fallback. Bidirectional clipboard sync. Multi-monitor support with stable display identity, enumeration, and per-display metrics.

CU-first routing: display tasks from voice go to a fast computer-use model first, with automatic escalation to the heavy agent for coding tasks.

### 4. Live Audio and Phone Calls

`spawn_live_audio` connects to Gemini Live or OpenAI Realtime APIs via WebSocket, piping audio through a virtual audio bridge (PulseAudio on Linux, BlackHole/Vortex on macOS). Untrusted: zero tools, zero file access. Responses validated against a `ResponseSchema`; unexpected content quarantined.

The phone-call skill makes outbound SIP calls via pjsua with the voice model conducting the conversation, returning structured data.

### 5. Human Oversight

Three-layer autonomy system:
1. **Global level** (`--autonomy` Low/Medium/High/Full)
2. **Category rules** (`[approval]` in intendant.toml — per-category Auto/Ask/Deny)
3. **Per-action approval** (y/s/a/n in any frontend)

Categories: FileRead, FileWrite, FileDelete, CommandExec, NetworkRequest, Destructive, HumanInput, LiveAudioSpawn, DisplayControl.

DisplayControl uses a session-grant model (approve once, revoke anytime via `d` hotkey). Landlock filesystem sandboxing restricts what the runtime can write.

### 6. Frontend Parity

The `UserAction` enum in `frontend.rs` forms a **compile-time contract** — the TUI, web dashboard, MCP server, and control socket all produce the same action variants. Adding a capability to one forces handling in all (no wildcard matches). All frontends are functionally equivalent.

### 7. MCP (Server and Client)

**Server** (`--mcp`): Exposes Intendant's full control surface as MCP tools — approve, deny, start tasks, query status, schedule controller restarts, intervene in loops. Architecturally a peer of the TUI, consuming the same EventBus. Supports hot-reload (rebuild + `exec()`).

**Client**: Connects to external MCP servers configured in `intendant.toml`. Tools registered as `mcp__<server>_<tool>`.

**Trust model for the client**: each MCP server entry is spawned as a child process with the user's full privileges (`Command::new(&config.command).args(&config.args)` in `mcp_client.rs`). Intendant performs **no checksum verification, no signature check, and no sandboxing** of MCP server binaries — adding one is equivalent to adding a line to your `~/.zshrc` that runs a binary. Default is `mcp_servers = []`, and `intendant.toml` is git-ignored, so the repo ships no MCP servers. Treat copying an `intendant.toml` between machines like copying shell rc files: read it before sourcing.

## Repository Layout

```
src/
├── main.rs                  # intendant-runtime entry point (sandboxed executor)
├── agent.rs                 # Runtime functions (exec, edit, browse, screenshot, PTY, memory)
├── models.rs, error.rs      # Shared types
└── bin/caller/
    ├── main.rs              # intendant entry point (controller)
    ├── provider.rs          # Multi-provider LLM abstraction
    ├── event.rs             # EventBus, AppEvent, ControlMsg
    ├── frontend.rs          # UserAction/StateQuery parity contract
    ├── tools.rs             # Core tool definitions
    ├── autonomy.rs          # Autonomy levels, action classification
    ├── presence.rs          # Server-side presence layer
    ├── computer_use.rs      # Cross-platform CU abstraction
    ├── display/             # WebRTC display transport (capture, encode, signaling)
    ├── web_gateway.rs       # HTTP/WebSocket server
    ├── mcp.rs, mcp_client.rs # MCP server and client
    ├── live_audio.rs        # Voice AI sessions
    ├── sub_agent.rs         # Multi-agent orchestration
    ├── worktree.rs          # Git worktree isolation
    ├── sandbox.rs           # Landlock filesystem sandboxing
    ├── recording.rs         # ffmpeg-based display recording
    ├── knowledge.rs         # Tagged knowledge store, pub/sub
    ├── session_log.rs       # Structured session logging
    ├── quarantine.rs        # Untrusted content isolation
    ├── tui/                 # ratatui TUI (widgets, layout, markdown, theme)
    └── ...                  # conversation, prompts, skills, transcription, etc.
crates/
├── presence-core/           # WASM-compatible: types, tools, dispatch, prompt (native + wasm32)
└── presence-web/            # Browser WASM: dashboard state, Gemini Live, OpenAI Realtime
static/
├── app.html                 # Web dashboard SPA (Activity, Stats, Terminal, Video, Sessions, Settings)
├── audio-processor.js       # AudioWorklet for mic capture
└── wasm-web/                # Compiled WASM + JS glue
scripts/                     # setup-linux.sh, setup-macos.sh, setup-lan.sh, bundle-macos.sh, etc.
skills/                      # SKILL.md files (phone-call, tui-e2e, web-e2e, voice-e2e, recording-e2e)
SysPrompt*.md                # System prompts per role (direct, tools, user, orchestrator, research, implementation, presence, live audio)
docs/src/                    # mdBook documentation
tests/e2e/                   # Integration tests (real API calls)
```

## Build and Run

```bash
cargo build --release     # Produces target/release/intendant-runtime and target/release/intendant
cargo build               # Debug build
cargo check               # Type-check only
```

### WASM (presence-web)
```bash
cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
cargo build --release -p intendant   # Re-embed WASM
```
**Important**: `cargo build` alone does NOT rebuild WASM. Any change to `crates/presence-web/` or `crates/presence-core/` requires the wasm-pack step above, then a re-embed build. The `static/wasm-web/` files are pre-compiled artifacts.

### CLI usage (requires `.env` with API key)
```bash
./target/release/intendant "task"                          # Default mode
./target/release/intendant --no-tui "task"                 # Headless
./target/release/intendant --direct "task"                 # Single-agent (skip orchestrator)
./target/release/intendant --json "task"                   # JSONL output (implies --no-tui)
./target/release/intendant --provider anthropic --model claude-sonnet-4-6-20250929 "task"
./target/release/intendant --autonomy low "rm /tmp/test"   # Ask before every command
./target/release/intendant --continue "fix that bug"       # Resume most recent session
./target/release/intendant --resume abc123 "continue"      # Resume by session ID
./target/release/intendant --mcp "task"                    # MCP server on stdio
./target/release/intendant --web                           # Web dashboard on port 8765
./target/release/intendant --sandbox "task"                # Landlock sandboxing (Linux)
./target/release/intendant --control-socket "task"         # Unix control socket
./target/release/intendant --no-presence "task"            # Disable presence layer
echo "task" | ./target/release/intendant                   # Auto-detects non-TTY -> headless
```

### Runtime
```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' | ./target/release/intendant-runtime
```

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys)
cargo test -- --list      # List all tests
```

Unit tests: inline `#[cfg(test)]` modules, `#[tokio::test]` for async, `tempfile` for filesystem isolation.

Integration tests (`tests/e2e/`): spawn real binary, make real API calls (costs tokens, non-deterministic). **Not for CI.**

```bash
cargo build --release
cargo test --test e2e test_basic -- --nocapture           # Tier 1: --json mode, no display
cargo test --test e2e test_control_socket -- --nocapture  # Tier 2: control socket, needs Xvfb
cargo test --test e2e test_web -- --nocapture             # Tier 3: WebSocket, needs Xvfb
cargo test --test e2e test_voice -- --nocapture           # Tier 3: needs Xvfb + Firefox + PulseAudio
```

## Key Design Decisions

- **Two-process security split**: Runtime executes commands under Landlock; controller holds API keys but never runs user-requested commands directly
- **Provider-agnostic with native tool calling**: Not prompt-level abstraction — proper support for each provider's native tool calling, CU, and streaming APIs
- **Quarantine for untrusted voice**: Live audio model outputs are schema-validated and quarantined, never exposed to agents
- **Git worktree isolation**: Sub-agents work in isolated worktrees, enabling parallel development on separate branches
- **Frontend parity via exhaustive enums**: Compile-time guarantee that all interfaces handle the same actions
- **Presence as separate AI**: Not a chat wrapper — a distinct model with its own conversation, tools, and state awareness

## Code Conventions

- Rust 2021 edition, default rustfmt/clippy (no config files)
- snake_case functions/modules, PascalCase types, SCREAMING_SNAKE_CASE constants
- `thiserror`-based error enums (`AgentError`, `CallerError`)
- tokio (full features), `Arc<RwLock/Mutex<T>>` for shared state, `mpsc` for channels
- Pure-safe Rust by default. The Unix (macOS / Linux) code paths contain no
  `unsafe` beyond a handful of well-documented libc existence/identity probes
  in `platform.rs`. The Windows backends are the deliberate exception: capture,
  input injection, and H.264 encoding necessarily go through Win32/COM/Media
  Foundation FFI (`display/windows.rs`, `display/encode/h264_windows.rs`,
  `platform.rs`), which has no safe alternative. Confine that `unsafe` to those
  `#[cfg(windows)]` blocks, keep each block as small as the FFI call it wraps,
  prefer the `windows` crate's RAII interface types (which Release COM refs on
  drop) and small safe wrappers / RAII guards over hand-rolled lifetime
  management, and annotate every `unsafe` block with a `// SAFETY:` comment
  stating the invariant that makes it sound (handle/pointer validity, COM
  refcount/ownership, buffer bounds, thread/apartment affinity). Do not
  introduce `unsafe` on the cross-platform or Unix paths.
- Tests: inline `#[cfg(test)]` modules only
- WASM boundary: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)`
- When adding a new system / `-sys` crate dependency, update **both**
  `scripts/setup-linux.sh` (`APT_PACKAGES`) and `scripts/setup-macos.sh`
  (`check_core` or an appropriate check function) in the same commit.
  Silent missing deps break fresh-machine setups with cryptic `pkg-config`
  errors long after the crate was added.

### Platform Support

Target platforms: macOS, Linux (Debian, X11 and Wayland).

Prefer platform-agnostic code by default. When platform-specific behavior is
unavoidable, use `cfg!(target_os = ...)` runtime checks for small branches or
`#[cfg(target_os = "...")]` compile-time gates for entire implementations.
Collect OS-specific helpers in dedicated modules (e.g. `platform.rs`,
per-platform blocks in `vision.rs`, `audio_routing.rs`, `computer_use.rs`,
`display/`).

Every feature must either work or degrade gracefully with a clear error on all
supported platforms — never panic or silently do nothing.

## Environment Requirements

- **OS**: macOS or Linux (Debian), unprivileged user with passwordless sudo (Linux)
- **API keys**: `.env` with `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`
- **Display capture**: libxcb + libxcb-shm (Linux X11), PipeWire (Linux Wayland), ScreenCaptureKit (macOS)
- **Input injection**: xdotool (Linux X11), ydotool (Linux Wayland), cliclick (macOS)
- **Encoding**: libvpx (VP8), ffmpeg with x264/VA-API (Linux H264), VideoToolbox (macOS H264)
- **Recording**: ffmpeg
- **WASM build**: `wasm-pack` (`cargo install wasm-pack`)
- **Full setup**: `./scripts/setup-linux.sh` (Debian/Ubuntu) or `./scripts/setup-macos.sh` (macOS)

## Multi-Agent Development

Multiple AI agents run concurrently on this machine, each in an isolated git
worktree. The main repo (`/home/user/projects/intendant`) is the shared merge
target — **never build or run intendant from the main worktree**. Always build
and launch from your own worktree's `target/release/intendant`.

Each running intendant instance binds its own web port (printed at startup).
Port discovery is automatic — the dashboard finds all running instances. Note
your port so the user can access your instance. Don't kill intendant processes
you didn't spawn; they belong to other agents.

## CI/CD

None configured. Run `cargo test --bins` and `cargo clippy` locally before committing.
