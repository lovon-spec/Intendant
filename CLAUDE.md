# CLAUDE.md

> **Living document — last verified 2026-05-24 against `main` @ `58c264d`.**
> This is a *tight orientation* for working in the repo. The deep reference lives in
> the mdBook under `docs/src/` (mapped below). **Both this file and those docs lag the
> code** — Intendant moves fast (~500 commits/month) and the docs are *not* updated on
> every change. When this file, the docs, and the source disagree, **trust the source**,
> then fix the doc. See what changed since this was written with
> `git log --oneline 58c264d..HEAD`. (`AGENTS.md` is a gitignored symlink to this file.)

## What Intendant Is

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop — shell, file editing, a graphical display it can see and control, voice, and phone calls — under layered human oversight. Beyond running its own agent loop, it **supervises external coding agents** (Codex, Gemini CLI, Claude Code) as managed backends and **federates with peer machines**. Provider-agnostic (OpenAI, Anthropic, Gemini); cross-platform (macOS, Linux, Windows — all first-class); every capability reachable from any interface (CLI, TUI, web dashboard, MCP, voice).

## The Two Binaries (security boundary)

- **intendant-runtime** (`src/main.rs`, `src/agent.rs`) — sandboxed executor. Reads one JSON `AgentInput` from stdin, runs commands sequentially, writes JSONL results. Landlock-restricted. **Never holds API keys.**
- **intendant** (`src/bin/caller/main.rs`) — controller. Drives the LLM loop, calls model APIs, dispatches tool calls to the runtime subprocess, supervises external agents, and runs every frontend.

A compromised model conversation can't reach API keys; the runtime can't exfiltrate through model APIs. This split is the load-bearing security decision — preserve it.

## Architecture at a Glance

The controller runs a budget-aware loop in one of **four execution modes**: Direct (`--direct`), User (an orchestrator decomposes work to sub-agents in isolated git worktrees), Sub-Agent (`INTENDANT_ROLE`), and External-Agent (`--agent`, supervising a third-party coding CLI). A separate **presence** AI mediates between the user and the worker. A single-writer **control plane** owns shared state — frontends are display-only, emitting intents (`ControlMsg`) rather than mutating state. A persistent **daemon** owns long-lived sessions; the web dashboard is the default frontend (`--web` is on by default).

Read the relevant chapter before changing a subsystem:

| Area | Chapter |
|---|---|
| Whole-system overview, the agent loop, streaming, caching | `docs/src/architecture.md` |
| Native multi-agent orchestration (modes, sub-agents, worktrees) | `docs/src/multi-agent.md` |
| Supervising Codex / Gemini CLI / Claude Code | `docs/src/external-agent-orchestration.md` |
| Control plane, persistent daemon, session lifecycle | `docs/src/control-plane-and-daemon.md` |
| Runtime stdin/stdout JSON protocol | `docs/src/runtime-protocol.md` |
| WebRTC display (shared encoder pool, tile streaming) | `docs/src/display-pipeline.md` |
| Peer federation, cross-machine display, LAN/mTLS | `docs/src/peer-federation.md` |
| Computer use, live audio, phone/voice-call skills | `docs/src/computer-use-and-audio.md` |
| Presence layer (server text + browser voice) | `docs/src/presence.md` |
| TUI + the autonomy/approval model | `docs/src/tui.md` |
| Web dashboard (tabs, sessions, live voice) | `docs/src/web-dashboard.md` |
| MCP server + client (trust model) | `docs/src/mcp-server.md` |
| Full `intendant.toml` + env reference | `docs/src/configuration.md` |
| Session logging, replay, rehydration | `docs/src/session-logging.md` |
| Windows backends and gotchas | `docs/src/windows-support.md` |

## Build, Run, Test

```bash
cargo build --release     # → target/release/{intendant-runtime, intendant}
cargo build               # debug
cargo check               # type-check only
cargo test --bins         # unit tests (fast, no API keys)
cargo clippy              # lint
```

**WASM** (`crates/presence-web`): `build.rs` auto-detects stale WASM and rebuilds it via `wasm-pack`, then re-embeds, on a normal `cargo build` (wasm-pack must be installed). Manual fallback only if that fails:
`cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web`.

Common invocations (full flag reference in `docs/src/getting-started.md`):

```bash
./target/release/intendant "task"                  # web dashboard ON by default (port 8765)
./target/release/intendant --no-web --no-tui "task"  # headless
./target/release/intendant --direct "task"         # single agent (skip orchestrator)
./target/release/intendant --agent codex "task"    # supervise an external coding CLI
./target/release/intendant --mcp "task"            # MCP server on stdio
./target/release/intendant --continue "..."        # resume most recent session
```

Requires an API key in `.env` (searched: cwd + parents → project root → `~/.config/intendant/.env`). `.env` and `intendant.toml` are git-ignored.

**Tests:** unit tests are inline `#[cfg(test)]` modules. `tests/e2e/main.rs` is an empty stub; end-to-end scenarios now live as SKILL.md files under `tests/skills/` and are **not** in CI (they make real API calls / need a display). Run `cargo test --bins` and `cargo clippy` locally before committing.

## Repository Layout

```
src/
├── main.rs, agent.rs           # intendant-runtime (sandboxed executor)
├── models.rs, error.rs, utils.rs
└── bin/caller/                 # the intendant controller:
    ├── main.rs                 # entry: CLI parsing, agent + daemon loops
    ├── control_plane.rs, event.rs, frontend.rs   # single-writer state; EventBus; UserAction/ControlMsg
    ├── session_supervisor.rs, task_dispatch.rs, file_watcher.rs   # daemon: sessions, dispatch, rewind snapshots
    ├── provider.rs, conversation.rs, tools.rs, prompts.rs, skills.rs, autonomy.rs, approval.rs
    ├── sub_agent.rs, worktree.rs, worktree_inventory.rs, user_mode.rs, agent_runner.rs   # native multi-agent
    ├── external_agent/         # supervise Codex / Claude Code / Gemini CLI
    ├── peer/, lan/, web_tls.rs # peer federation; mTLS LAN proxy; native HTTPS/WSS
    ├── display/                # WebRTC: encode/{pool,vp8,h264_*}, tile/, capture/, webrtc, {x11,wayland,macos,windows}
    ├── computer_use.rs, vision.rs, recording.rs, frames.rs
    ├── presence.rs, live_audio.rs, audio_routing.rs, transcription.rs, quarantine.rs, schema_validator.rs
    ├── web_gateway.rs, mcp.rs, mcp_client.rs, control.rs
    ├── session_log.rs, session_names.rs, knowledge.rs, project.rs, app_state_pricing.rs
    ├── sandbox.rs, platform.rs, daemon_log_tee.rs, diagnostics.rs, …
    └── tui/                    # ratatui TUI (display-only client of the control plane)
crates/{presence-core, presence-web}   # WASM-shared presence types/tools/dispatch + browser WASM client
static/         # app.html dashboard SPA + compiled wasm-web/
macos-app/      # native macOS WKWebView wrapper (built by scripts/bundle-macos.sh)
vendor/         # vortex-guest-tools (macOS Vortex Audio HAL plugin)
scripts/        # setup-{linux,macos,windows}, setup-lan*, bundle-macos, …
skills/         # phone-call, voice-call-app, wayland-portal-e2e
docs/src/       # this project's mdBook — the deep reference (see the table above)
SysPrompt*.md   # per-role system prompts (base, tools, user, orchestrator, research, implementation, presence, live audio)
```

## Code Conventions

- Rust 2021 edition, default rustfmt/clippy (no config files)
- snake_case functions/modules, PascalCase types, SCREAMING_SNAKE_CASE constants
- `thiserror`-based error enums (`AgentError`, `CallerError`)
- tokio (full features), `Arc<RwLock/Mutex<T>>` for shared state, `mpsc` for channels
- TLS/cert code is **pure-Rust `ring`/`rcgen`/`rustls`** (`web_tls.rs`, `lan/certs.rs`) — no OpenSSL; prefer that path when touching crypto/cert code
- Tests live in inline `#[cfg(test)]` modules only
- WASM boundary: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)`
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
- When adding a new system / `-sys` crate dependency, update **both**
  `scripts/setup-linux.sh` (`APT_PACKAGES`) and `scripts/setup-macos.sh`
  (`check_core` or an appropriate check function) in the same commit. Silent
  missing deps break fresh-machine setups with cryptic `pkg-config` errors long
  after the crate was added.

## Platform Support

macOS, Linux (Debian, X11 and Wayland), and Windows (`x86_64-pc-windows-msvc`) are all
first-class targets. **OS-specific `std` APIs must be `#[cfg]`-guarded** — don't use
`std::os::unix::*` / `std::os::windows::*` items unconditionally; wrap the platform call
in a `#[cfg(unix)]`/`#[cfg(windows)]`-paired helper in `platform.rs` (the existing
convention) with a portable fallback, and route callers through it. Prefer
platform-agnostic code; when unavoidable, use `cfg!(target_os = ...)` for small branches
or `#[cfg(target_os = "...")]` for whole implementations, collected in dedicated modules
(`platform.rs`, per-platform blocks in `display/`, `vision.rs`, `audio_routing.rs`,
`computer_use.rs`). Every feature must work or degrade gracefully with a clear error on
all supported platforms — never panic or silently do nothing. See `docs/src/windows-support.md`.

## Multi-Agent Development

Multiple AI agents run concurrently on this machine, each in an isolated git worktree.
**The main worktree (the repo root) is the shared merge target — never build or run
intendant from it.** Always build and launch from your own worktree's
`target/release/intendant`. Each running instance binds its own web port (printed at
startup) and the dashboard auto-discovers running instances; note your port so the user
can reach your instance. Don't kill intendant processes you didn't spawn — they belong to
other agents.

## CI/CD

GitHub Actions on push / PR to `main`:
- **`windows.yml`** — cross-platform `cargo check -p intendant` on Windows + macOS + Linux (catches platform-specific build breaks; excludes the WASM `presence-web` crate).
- **`audit.yml`** — `cargo audit` on push/PR plus a weekly cron (Mondays 08:00 UTC).
- **`docs.yml`** — mdBook (`docs/`) deploy to GitHub Pages.

The `tests/skills/` end-to-end scenarios are not in CI (real API calls / need a display). Run `cargo test --bins` and `cargo clippy` locally before committing.
