# Getting Started

This chapter takes you from a clean checkout to a running agent: prerequisites,
per-OS setup, building, API keys, your first run, and the full CLI flag
reference.

## Prerequisites

Intendant is a Rust workspace. At minimum you need:

- **Rust** toolchain (stable) — `rustup` recommended
- **wasm-pack** — `cargo install wasm-pack` (the dashboard's browser code is a
  WASM crate; the build auto-rebuilds it, see [WASM](#wasm-builds-automatically))
- **ffmpeg** — display recording and software H.264 encoding
- An **API key** for at least one provider (OpenAI, Anthropic, or Gemini)

Platform-specific runtime dependencies (display capture, input injection, audio
routing) are best installed with the setup script for your OS.

### Per-OS setup scripts

The scripts in `scripts/` install everything a fresh machine needs and verify
what is already present. Each accepts `--check` to report status without
changing anything.

```bash
# macOS — installs cliclick, ffmpeg, sox, SwitchAudioSource, wasm-pack,
# Vortex Audio HAL plugin (or BlackHole fallback), and builds the app bundle.
./scripts/setup-macos.sh
./scripts/setup-macos.sh --check     # report only

# Linux (Debian/Ubuntu) — installs the APT_PACKAGES set: libvpx, libxcb +
# libxcb-shm + libxcb-randr, libpipewire, xdotool, imagemagick, ffmpeg,
# pulseaudio-utils, ripgrep, Xvfb, and toolchain build deps.
./scripts/setup-linux.sh
./scripts/setup-linux.sh --check

# Windows (Server 2022 / 11), PowerShell — see ./windows-support.md
./scripts/setup-windows.ps1
```

Manual Linux install if you would rather not run the script:

```bash
sudo apt install build-essential binutils pkg-config libclang-dev \
  libvpx-dev libpipewire-0.3-dev libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev \
  xdotool x11-utils imagemagick ffmpeg xvfb pulseaudio-utils ripgrep xdg-utils \
  ca-certificates libnss3 libatk-bridge2.0-0 libgtk-3-0 libxcomposite1 \
  libxdamage1 libxrandr2 libxss1 libgbm1 libdrm2 libxkbcommon0 libcups2
```

See [Integrations](./integrations.md) for what each tool is used for, and
[Windows Support](./windows-support.md) for the Windows toolchain in detail.

## Building

```bash
cargo build --release     # optimized
cargo build               # debug
cargo check               # type-check only (fast)
./target/release/intendant setup browsers  # managed Chrome for Testing for browser workspaces
```

A release build produces two binaries:

- `target/release/intendant-runtime` — the sandboxed command executor. Reads
  JSON commands on stdin, runs them, writes JSON results to stdout. Never holds
  API keys.
- `target/release/intendant` — the controller. Manages the LLM conversation,
  calls model APIs, dispatches tool calls to the runtime subprocess, and hosts
  every frontend (web dashboard, TUI, MCP, control socket).

The two-binary split is the security boundary; see [Architecture](./architecture.md).

### Installing

```bash
cargo install --path .
```

Both binaries land in `~/.cargo/bin/`. The `intendant` binary embeds the default
system prompts and the web assets (HTML, JS, compiled WASM) at compile time, so
it runs from any directory without the source tree.

### WASM builds automatically

The dashboard's browser-side state machine and voice clients live in the
`crates/presence-web` (and shared `crates/presence-core`) WASM crate. **A normal
`cargo build` rebuilds the WASM for you**: `build.rs` compares the timestamps of
`crates/presence-web/src` and `crates/presence-core/src` against the compiled
`static/wasm-web/presence_web_bg.wasm`, and when the sources are newer it runs
`wasm-pack` into a separate target dir and re-embeds the result.

This requires `wasm-pack` to be installed. If it is missing, `cargo build`
prints a `cargo:warning` and skips the WASM step (the previously-compiled
artifact in `static/wasm-web/` is used as-is).

The manual two-step is now only a **fallback** (e.g. if the auto-detect ever
misfires):

```bash
cd crates/presence-web && \
  wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
cargo build --release -p intendant   # re-embed
```

> The earlier guidance that "`cargo build` alone does NOT rebuild WASM" is no
> longer true — `build.rs` handles it.

### macOS app bundle

`scripts/bundle-macos.sh` compiles a small Swift wrapper
(`macos-app/main.swift`) with `swiftc`, bundles it with the release `intendant`
binary, codesigns it (a persistent self-signed identity, ad-hoc fallback), and
installs to `/Applications/Intendant.app`.

The wrapper hosts a `WKWebView` that loads the dashboard over a custom
`intendant://` URL scheme. This is deliberate: `WKWebView` does not treat
`http://localhost` as a secure context, so `navigator.mediaDevices` (microphone
and camera) would be unavailable. Serving from a registered custom scheme
restores the secure context the live-voice and camera features need.

When the generated access cert set is readable in
`~/.intendant/access-certs`, the wrapper automatically starts the bundled daemon
with native `--mtls`. The in-app `intendant://` bridge then speaks HTTPS/WSS to
the local backend, pins the generated server certificate, and presents the
generated `client.p12` for its own local bridge. Remote browsers use
`https://<mac-ip>:<port>` and must have an enrolled client identity. If no
complete access cert set is installed, the bundled daemon fails closed with setup
guidance rather than serving plain HTTP. Explicit launch flags still win:
`open -a Intendant --args --tls` forces TLS-only, and `--no-tls --bind 127.0.0.1`
is the explicit local plaintext/debug escape.

The same secure-context requirement applies to remote browsers using Station's
WebGPU renderer, microphone/camera features, browser screen capture, and other
privileged browser APIs; see
[Web Dashboard: Secure Browser Contexts](./web-dashboard.md#secure-browser-contexts).

```bash
./scripts/bundle-macos.sh           # release build + install to /Applications
./scripts/bundle-macos.sh debug     # debug build + install
INSTALL_APP=0 ./scripts/bundle-macos.sh   # build the bundle without installing
```

## API keys (.env)

On startup the controller loads environment variables from `.env`, searching in
this order (later files do not override variables already set):

1. **Current directory and its parents** (`dotenvy::dotenv()`)
2. **Project root** — the git top-level, `<project-root>/.env`
3. **Global config** — `~/.config/intendant/.env`

`.env` and `intendant.toml` are git-ignored, so secrets never land in the repo.
For use-anywhere-after-`cargo install`, put your keys in
`~/.config/intendant/.env`:

```bash
# Provide at least one of these:
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
GEMINI_API_KEY=AI...

# When more than one key is present, pick the main provider explicitly:
PROVIDER=openai            # "openai" | "anthropic" | "gemini"
MODEL_NAME=gpt-5.5         # optional; a provider default is used if omitted
```

`OPENAI`, `ANTHROPIC`, and `GEMINI` are accepted as aliases for the
corresponding `*_API_KEY` variables. Provider auto-detection (when `PROVIDER` is
unset) prefers **OpenAI** when an OpenAI key is present, then Anthropic, then
Gemini. See [Configuration](./configuration.md) for the full environment
reference and per-provider default models.

## Your first run

```bash
# A one-off task. By default the web dashboard comes up (see below) and the
# controller prints the dashboard URL; open it in a browser to watch/steer.
./target/release/intendant "List the files in /tmp"

# Pipe a task in (non-TTY stdin auto-selects headless mode):
echo "summarize README.md" | ./target/release/intendant

# Interactive: with no task argument, you are prompted for one.
./target/release/intendant
```

### Frontend selection

There is no single "TUI vs web" switch — the controller picks a frontend from
the flags and whether it owns a real terminal:

- **Web dashboard is on by default.** It runs unless you pass `--no-web`,
  `--mcp`, or `--json`. The server binds port **8765**, auto-incrementing
  through 8785 if that port is taken; the chosen port is printed at startup.
- The terminal **TUI owns the TTY only when the web gateway is off** (`--no-web`)
  **and** `--no-tui` is not set **and** both stdin and stdout are real
  terminals. With the dashboard on (the default), the process runs in a
  headless/daemon posture and the dashboard's embedded terminal tab renders the
  same TUI.
- `--mcp` turns the process into an MCP server on stdio (no dashboard, no TUI).
- `--json` emits JSONL events to stdout and implies `--no-tui`.

So a plain `intendant "task"` on a desktop gives you a dashboard URL; if you
want the classic in-terminal TUI, run `intendant --no-web "task"`.

### Resume and continue

```bash
./target/release/intendant --continue "fix that bug"   # most recent session
./target/release/intendant -c "fix that bug"            # short form
./target/release/intendant --resume abc123 "continue"   # by session id / prefix / path
./target/release/intendant -r abc123 "continue"
./target/release/intendant --resume                     # no id given → acts like --continue
```

### Launching specific frontends

```bash
# Web dashboard explicitly (and/or pick a port)
./target/release/intendant --web
./target/release/intendant --web 9000

# Default dashboard: mTLS with installed access certs
./target/release/intendant

# TLS-only: HTTPS/WSS without requiring browser client certificates
./target/release/intendant --tls

# Explicit local/plaintext debug escape
./target/release/intendant --no-tls --bind 127.0.0.1

# Classic terminal TUI (dashboard off)
./target/release/intendant --no-web "task"

# MCP server on stdio
./target/release/intendant --mcp "Deploy the application"

# Headless JSONL stream
./target/release/intendant --json "echo hello"

# macOS app bundle (after scripts/bundle-macos.sh)
open -a Intendant

# The bundle starts the backend with mTLS by default.
# Forward --tls only when you intentionally want TLS without client cert auth,
# or --no-tls --bind 127.0.0.1 only for explicit plaintext/debug use.
open -a Intendant --args --tls
```

## CLI flag reference

The authoritative source is the argument parser in `src/bin/caller/main.rs`
(`print_help` and the parse loop). Flags that take a value error out if the
value is missing.

| Flag | Argument | Description |
|------|----------|-------------|
| `--provider` | `<name>` | Force provider: `openai`, `anthropic`, or `gemini` (sets `PROVIDER`) |
| `--model` | `<name>` | Override the model (sets `MODEL_NAME`) |
| `--autonomy` | `<level>` | Autonomy level: `low`, `medium`, `high`, `full` (loose parse; unknown → `medium`) |
| `--log-file` | `<dir>` | Override the session log directory (default `~/.intendant/logs/<timestamp>/`) |
| `--continue`, `-c` | — | Resume the most recent session for this project |
| `--resume`, `-r` | `[id]` | Resume a session by id, prefix, or path; with no id behaves like `--continue` |
| `--no-tui` | — | Disable the terminal TUI; run headless |
| `--mcp` | — | Run as an MCP server on stdio (disables dashboard/TUI) |
| `--verbose`, `-v` | — | Show debug-level log entries |
| `--control-socket` | — | Enable the Unix control socket at `/tmp/intendant-<pid>.sock` |
| `--json` | — | Emit JSONL events to stdout (implies `--no-tui`; disables dashboard) |
| `--sandbox` | — | Enable Landlock filesystem sandboxing for the runtime (Linux 5.13+) |
| `--direct` | — | Force single-agent mode (skip the orchestrator / sub-agent delegation) |
| `--no-presence` | — | Disable the presence layer (talk to the worker agent directly) |
| `--web` | `[port]` | Start the web dashboard. **On by default**; optional numeric port (default 8765) |
| `--no-web` | — | Disable the web dashboard; use the terminal TUI when interactive |
| `--bind` | `<addr>` | IP address for the web dashboard listener. Use `127.0.0.1` for local/plaintext automation |
| `--no-tls` | — | Serve the dashboard over plain HTTP. Explicit local/debug escape; wildcard bind refuses startup when a public interface exists |
| `--allow-public-plaintext` | — | Override the `--no-tls` public-interface guard for intentional public plaintext exposure |
| `--tls` | — | Serve HTTPS/WSS without requiring browser client certificates; uses installed access certs when present, otherwise falls back to an auto self-signed cert |
| `--tls-cert` | `<path>` | PEM cert (chain) overriding the default cert selection; implies `--tls` (pair with `--tls-key`) |
| `--tls-key` | `<path>` | PEM private key matching `--tls-cert`; implies `--tls` |
| `--mtls` | — | Require browser/client certificates signed by the Intendant access CA. This is the default dashboard transport |
| `--mtls-ca` | `<path>` | PEM CA bundle for `--mtls` client certificate verification |
| `--transcription` | — | Enable server-side speech transcription (overrides `[transcription] enabled`) |
| `--record-display` | `<id>` | Record an existing X11 display, e.g. `50` for `:50` (repeatable) |
| `--agent` | `<backend>` | Use an external coding-agent backend: `codex` or `claude-code` |
| `--advertise-url` | `<url>` | WebSocket URL to advertise to federation peers in this daemon's Agent Card (repeatable, preference order; overrides `[server.advertise]`) |
| `--help`, `-h` | — | Print help and exit |

> **Correction vs. older docs:** `--web` is **on by default** and no longer
> "implies `--mcp`". The dashboard runs unless disabled by `--no-web`, `--mcp`,
> or `--json`. Earlier documentation treated `--web` as opt-in — that is no
> longer accurate.

A non-flag token (one that does not start with `-`) is collected into the task
string; an unknown flag is an error.

## Dashboard access over TLS

The dashboard defaults to native mTLS. Run `intendant access setup` first so the
daemon has `server.crt`, `server.key`, and `ca.crt`, and so dashboard consumer
devices can enroll a client identity.

### 1. Built-in mTLS / TLS (any platform, pure-Rust)

```bash
./target/release/intendant        # default: mTLS
./target/release/intendant --tls  # TLS-only
```

With no transport flag, the gateway serves HTTPS/WSS with client-certificate
authentication required. `--tls` (or `[server.tls] enabled = true` in
`intendant.toml`) serves HTTPS/WSS without requiring browser client
certificates. With no explicit cert override, TLS-only first uses the installed
Intendant access server certificate (`server.crt` / `server.key`) when present,
then falls back to minting a self-signed certificate at startup (SAN = bind IP +
`localhost`, plus an optional configured hostname). The TLS stack is pure Rust
(`rustls` + `rcgen`) — no OpenSSL, no nginx — and works on Windows too. See
[Configuration](./configuration.md) for `[server.tls]` and `--tls-cert` /
`--tls-key`.

`intendant access setup` stores the access CA, server cert, and client identity in
the current user's native cert store (`~/.intendant/access-certs` on Unix-like
hosts). Run setup as the same user that launches the daemon; the native gateway
reads `server.crt` / `server.key` directly from that store.

To be explicit about the default, pass `--mtls`:

```bash
./target/release/intendant --mtls
```

`--mtls` verifies browser/client certificates against the installed Intendant
access CA (`ca.crt`) unless `--mtls-ca` or `[server.mtls] ca` overrides it.
Clients without the installed client identity cannot complete the TLS handshake.
If access certs are missing, startup fails closed with setup guidance. Use
`--no-tls` only when you intentionally want plain HTTP for local/programmatic
debugging. Prefer `--no-tls --bind 127.0.0.1`; wildcard plaintext refuses
startup when the host has a public interface unless `--allow-public-plaintext`
is passed.

### 2. Native access cert enrollment (`intendant access setup`)

For mutual-TLS with client certificates (so only enrolled devices can connect),
the `intendant access` subcommand creates a per-user access certificate authority,
server cert, client identity, and strict enrollment page for installing those
certs on browsers and mobile devices:

```bash
intendant access setup            # generate CA/server/client certs + start enrollment
intendant access recert           # regenerate the server cert after access addresses change
intendant access list             # show current setup state
intendant access serve-certs      # run strict HTTPS client-cert enrollment
intendant access remove           # remove the per-user access cert store
```

Useful flags: `--port <N>` (native dashboard HTTPS port to advertise, default
8765),
`--cert-port <N>` (HTTPS enrollment server, default 9999), repeatable
`--ip <IP>`, repeatable `--host <DNS>`, `--name <label>`, `--force`, and
`--no-serve-certs`. Removed LAN/proxy flags are rejected; certificate setup no
longer configures an upstream proxy.

`--name` is the daemon display label used by the Agent Card and dashboard
targets. If omitted, setup now uses the system hostname when available and falls
back to the primary IP only as a last resort. Existing IP-based labels continue
to work, but the dashboard prefers a human label or hostname over a stale IP
when both are present.

Client certificate enrollment is deliberately strict. The temporary enrollment
server is HTTPS, using the same access server certificate as the dashboard. Before
the CLI reveals the one-time enrollment secret, the operator must copy the
server certificate SHA-256 fingerprint observed in the browser certificate UI
and paste it into the `intendant access` terminal. Treat any enrollment web content
as untrusted until that terminal check succeeds: stop at the browser certificate
warning/details UI, do not continue to page content, enter secrets, click
downloads, or install anything first. If the browser cannot expose certificate
details before loading the page, inspect the server certificate with a
client-side certificate tool instead. Only after the pasted fingerprint matches
the local `server.crt` can the browser redeem the secret and download enrollment
artifacts. Apple clients get a single `intendant.mobileconfig` profile; other
clients can download `ca.crt` plus `client.p12` (or the same identity as
`client.pfx`). This avoids the unsafe pattern where a MITM page can serve
active content or steal a secret from an unauthenticated download page.

The enrollment page uses the browser's User-Agent only to prioritize the
instructions and download buttons for that device. The authorization decision is
always the strict terminal-paired session cookie.

Use this HTTPS/mTLS path, native `--tls` with a trusted certificate, or the macOS
app wrapper for dashboard features that require a secure browser context:
Station WebGPU, microphone/camera, browser screen capture, and stricter
clipboard APIs. Plain `http://<host-ip>:8765` is available only when launched
with `--no-tls`; it is intended for local/programmatic debugging, is guarded on
public wildcard binds, and does not enable those browser-gated features.

The client certificate is exported as `client.p12`, a password-protected
PKCS#12 bundle for installation on iOS / Android / desktop browsers. The
password is shown only on the unlocked enrollment page. The Apple
`intendant.mobileconfig` profile embeds that same client identity, the access CA,
and the PKCS#12 password, so it is served only after strict pairing succeeds.
On macOS, install downloaded profiles from **System Settings → General → Device
Management**; if that pane is hidden, search System Settings for "Profiles".
If Safari still shows **Not Secure**, open Keychain Access and set the
Intendant CA to **Always Trust**; installing the profile is not enough unless
the CA is trusted for websites.
If macOS reports that the profile certificate could not be verified, install
`ca.crt` and `client.p12` manually from the same unlocked page, or regenerate
access cert material with `intendant access setup --force` so the Apple profile
uses an Apple-compatible client identity bundle and certificate payloads.

#### Apple device requirement for `client.p12`

`client.p12` is packaged in Apple auto-detect-compatible PKCS#12 form: PBES1
3DES-CBC with a SHA-1 MAC. This is intentionally less modern than PBES2/AES
because macOS profile installation and `security import` can reject PBES2/AES
bundles as a password/MAC authentication error unless the caller explicitly
forces PKCS#12 format. New access cert material still uses RSA-2048 certificates
with SHA-256 signatures, matching Apple's documented certificate
configuration-profile payload compatibility.

Android and desktop Chrome/Firefox also import the Apple-compatible bundle.

## Testing

```bash
cargo test --bins         # unit tests (fast, no API keys)
cargo test -- --list      # list all test names
```

Unit tests are inline `#[cfg(test)]` modules in both binaries. Integration
tests under `tests/e2e/` spawn a real binary and make real API calls (they cost
tokens and are non-deterministic) — they are **not** part of CI. See
[Architecture](./architecture.md) for the tiered e2e suite and
[Session Logging](./session-logging.md) for the test-coverage summary.

## Runtime (standalone)

The runtime executes a JSON command batch on stdin and writes results to
stdout:

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/intendant-runtime
```

See [Runtime Protocol](./runtime-protocol.md) for the full command schema.
