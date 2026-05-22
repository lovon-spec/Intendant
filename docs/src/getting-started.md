# Getting Started

## Building

```bash
cargo build --release
```

Two binaries are produced:
- `./target/release/intendant-runtime` — the sandboxed command runtime
- `./target/release/intendant` — the AI controller (CLI/TUI/Web/MCP)

### Installing

```bash
cargo install --path .
```

Both binaries are installed to `~/.cargo/bin/`. The `intendant` binary embeds default system prompts and web assets (HTML, WASM) at compile time, so it works immediately from any directory without needing the source tree.

### Prerequisites

- **Rust** toolchain (stable)
- **wasm-pack** — `cargo install wasm-pack`
- **ffmpeg** — display recording and H264 encoding
- **macOS**: `./scripts/setup-macos.sh` installs all platform dependencies (cliclick, ffmpeg, Vortex Audio, wasm-pack, app bundle)
- **Linux**: `./scripts/setup-linux.sh` installs all platform dependencies (libvpx, libxcb, xdotool, PipeWire, ffmpeg, PulseAudio, Xvfb)

Manual Linux install if not using the setup script:

```bash
sudo apt install build-essential binutils pkg-config libclang-dev \
  libvpx-dev libpipewire-0.3-dev libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev \
  xdotool x11-utils imagemagick ffmpeg xvfb pulseaudio-utils xdg-utils
```

### WASM

The `build.rs` script automatically rebuilds WASM when `crates/presence-web/` or `crates/presence-core/` source files change. This requires `wasm-pack` to be installed. If not installed, `cargo build` prints a warning and skips the WASM rebuild.

To rebuild manually:

```bash
cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
cargo build --release -p intendant   # Re-embed WASM
```

**Important**: `cargo build` alone does NOT rebuild WASM. Any change to `crates/presence-web/` or `crates/presence-core/` requires the wasm-pack step above, then a re-embed build. The `static/wasm-web/` files are pre-compiled artifacts checked into the repo.

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

MODEL_NAME=gpt-5         # optional, provider-specific default used if omitted
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
./target/release/intendant --provider anthropic --model claude-sonnet-4-6-20250929 "List files"

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

# Web dashboard (default port 8765)
./target/release/intendant --web

# Web dashboard on custom port
./target/release/intendant --web 9000

# Enable Landlock filesystem sandboxing (Linux 5.13+)
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

## LAN Access

For accessing the web dashboard from phones, tablets, or other devices on your network:

```bash
# Linux/macOS: sets up mTLS nginx reverse proxy with client certificates
./scripts/setup-lan.sh
```

This generates CA, server, and client certificates. The client certificate is exported as `client.p12` (a password-protected PKCS#12 bundle) for installation on iOS/Android/desktop browsers.

### Apple device requirement (`client.p12` import)

The `client.p12` bundle is packaged with **modern** PKCS#12 encryption — PBES2
(PBKDF2-HMAC-SHA256 + AES-256-CBC) with a SHA-256 MAC. This is the algorithm set
Apple's current importer (`SecPKCS12Import`, used by Keychain Access and the iOS
profile installer) accepts.

> **Supported environment:** importing `client.p12` on an Apple device requires
> **macOS 15 (Sequoia) or later**, or **iOS / iPadOS 18 or later**.

Older Apple releases predate this importer and only accept the legacy
RC2-40 / 3DES + SHA-1 packaging, which Intendant intentionally no longer
produces (dropping it is what let the cert subsystem become pure-Rust, with no
OpenSSL dependency). Android, and desktop browsers such as Chrome and Firefox,
import the modern bundle without a version floor. If you must serve a pre-15
macOS or pre-18 iOS client, convert the bundle to the legacy format yourself
with `openssl pkcs12 -legacy` on a machine that has OpenSSL.

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

The test suite covers both binaries with inline `#[cfg(test)]` modules. See [Session Logging](./session-logging.md) for the full test coverage summary.

Integration tests in `tests/e2e/` spawn a real binary and make real API calls — see [Architecture](./architecture.md) for details.
