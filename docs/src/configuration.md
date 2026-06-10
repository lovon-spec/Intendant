# Configuration

Intendant is configured through three layers, in increasing specificity:

1. **`intendant.toml`** in the project root — the durable, per-project config
   (structure in `src/bin/caller/project.rs`).
2. **Environment variables** (often via `.env`) — keys, provider/model
   overrides, and a few runtime toggles.
3. **CLI flags** — per-invocation overrides (see
   [Getting Started](./getting-started.md#cli-flag-reference)).

CLI flags win over env vars where they overlap (`--provider` sets `PROVIDER`,
`--model` sets `MODEL_NAME`). `intendant.toml` and `.env` are both git-ignored.

## Environment variables

The controller reads these from the process environment (populated from `.env`;
see [Getting Started](./getting-started.md#api-keys-env) for the search order).

### Keys and provider selection

| Variable | Alias | Default | Description |
|----------|-------|---------|-------------|
| `OPENAI_API_KEY` | `OPENAI` | — | OpenAI key |
| `ANTHROPIC_API_KEY` | `ANTHROPIC` | — | Anthropic key |
| `GEMINI_API_KEY` | `GEMINI` | — | Google AI (Gemini) key |
| `PROVIDER` | — | auto-detect | `openai`, `anthropic`, or `gemini` — which provider to use when multiple keys are set |
| `MODEL_NAME` | — | per-provider | Main model name |

**Auto-detection** (when `PROVIDER` is unset): if an OpenAI key is present it is
used first, then Anthropic, then Gemini. Setting `PROVIDER` explicitly forces
that provider (and errors if its key is missing).

**Per-provider default models** (used when `MODEL_NAME` is unset):

| Provider | Default model |
|----------|---------------|
| OpenAI | `gpt-5.5` |
| Anthropic | `claude-sonnet-4-5-20250929` |
| Gemini | `gemini-2.5-pro` |

### Model and behavior tuning

| Variable | Default | Description |
|----------|---------|-------------|
| `MODEL_CONTEXT_WINDOW` | per-model | Context window in tokens (also settable via `[model] context_window`) |
| `MAX_OUTPUT_TOKENS` | per-model | Max output tokens per API call (also `[model] max_output_tokens`) |
| `USE_NATIVE_TOOLS` | `true` | Use the provider's native tool-calling API; `false` falls back to text-based JSON extraction |
| `STRUCTURED_OUTPUT` | provider-dependent | Enable JSON object mode for deterministic parsing |
| `REASONING_EFFORT` | — | For reasoning models: `low`, `medium`, `high` |
| `REASONING_SUMMARY` | — | Reasoning summary mode: `auto`, `concise`, `detailed` |

`[model] context_window` / `max_output_tokens` from `intendant.toml` are applied
into `MODEL_CONTEXT_WINDOW` / `MAX_OUTPUT_TOKENS` only when those env vars are
not already set, so env/CLI always win.

### Presence and computer-use overrides

| Variable | Default | Description |
|----------|---------|-------------|
| `PRESENCE_PROVIDER` | falls back to `PROVIDER` | Override the presence layer's provider |
| `PRESENCE_MODEL` | falls back to `PRESENCE_PROVIDER`'s default | Override the presence model |
| `CU_PROVIDER` | falls back to `PROVIDER` | Override the computer-use model's provider |
| `CU_MODEL` | — | Override the computer-use model |

These mirror the `[presence]` and `[computer_use]` sections below; the
precedence is **explicit config > env var > auto-detect**.

### Browser workspace overrides

| Variable | Default | Description |
|----------|---------|-------------|
| `INTENDANT_BROWSER_WORKSPACE_EXECUTABLE` | managed browser cache | Explicit Chromium/Chrome-for-Testing executable for CDP browser workspaces |
| `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER` | `false` on macOS, `true` elsewhere | On macOS, explicitly permit CDP workspaces to launch system Chrome/Chromium apps such as `/Applications/Google Chrome.app` |

The default CDP resolver prefers managed Playwright/Puppeteer/Chrome-for-Testing
browser caches and Intendant's own browser cache locations. This avoids
attributing Google Chrome updater/app-bundle activity to Intendant on macOS. Set
the explicit executable variable when a managed browser lives in a custom path,
or choose `provider=system_cdp` for a deliberate one-off system-browser launch.
Run `intendant setup browsers` to download Chrome for Testing into Intendant's
managed cache, or `intendant setup browsers --check` to verify the cache without
network access.

### Sub-agent variables (set automatically)

When the orchestrator spawns sub-agents it sets these in the child environment;
you normally never set them by hand (see
[Multi-Agent Orchestration](./multi-agent.md)):

| Variable | Description |
|----------|-------------|
| `INTENDANT_ROLE` | Sub-agent role (`orchestrator`, `research`, `implementation`, `testing`) |
| `INTENDANT_ID` | Unique sub-agent identifier |
| `INTENDANT_TASK` | Task description |
| `INTENDANT_RESULT_FILE` | Where the sub-agent writes its final result |
| `INTENDANT_PROGRESS_FILE` | Where the sub-agent writes periodic progress |
| `INTENDANT_PARENT_KNOWLEDGE` | Path to the parent's knowledge store for inheritance |
| `INTENDANT_INHERIT_MEMORY` | `1` to inherit project memory |
| `INTENDANT_SANDBOX_WRITE_PATHS` | Landlock write paths (set by the caller when sandboxing) |
| `INTENDANT_MAX_PARALLEL_AGENTS` | Max concurrent sub-agents (from `[orchestrator]`) |
| `INTENDANT_LOG_DIR` | Session log directory (set by the caller for the runtime) |

## `intendant.toml`

Create `intendant.toml` in your project root (the git top-level). Every section
is optional; an absent section uses its defaults. The structure and defaults
below are taken directly from `src/bin/caller/project.rs`.

### `[memory]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable the persistent project memory store (`.intendant/memory.json`) |

### `[model]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `context_window` | int | per-model | Override the model's context window (tokens) |
| `max_output_tokens` | int | per-model | Override max output tokens per call |

### `[orchestrator]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_parallel_agents` | int | unset | Cap on concurrent sub-agents |
| `sub_agent_dir` | string | `.intendant/subagents` | Directory (relative to project root) for sub-agent workspaces |

### `[approval]`

Per-category approval rules. Each value is `auto` (run without asking), `ask`
(prompt the human), or `deny` (refuse). These are layered under the global
`--autonomy` level — see [Autonomy and approval](#autonomy-and-approval).

| Key | Type | Default | Category |
|-----|------|---------|----------|
| `file_read` | rule | `auto` | FileRead |
| `file_write` | rule | `ask` | FileWrite |
| `file_delete` | rule | `ask` | FileDelete |
| `command_exec` | rule | `auto` | CommandExec |
| `network` | rule | `auto` | NetworkRequest |
| `destructive` | rule | `ask` | Destructive |
| `display_control` | rule | `ask` | DisplayControl |

The `HumanInput` and `LiveAudioSpawn` categories always require a human and are
not configurable here.

### `[presence]`

The conversational presence layer that mediates between you and the worker
agent (see [Presence Layer](./presence.md)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable the presence layer |
| `provider` | string | auto-detect | Provider for text-mode presence |
| `model` | string | `gemini-3-flash-preview` | Text-mode presence model |
| `context_window` | int | `1048576` | Context window for text-mode presence |
| `live_provider` | string | auto-detect | Provider for browser-side live (voice) presence |
| `live_model` | string | provider default | Live presence model |
| `live_context_window` | int | `32768` | Context window for live presence |

> The compiled-in default text presence model is `gemini-3-flash-preview`. Text
> presence auto-detection prefers Gemini when `GEMINI_API_KEY` is set.

### `[transcription]`

Server-side speech-to-text via the Whisper API (or a compatible endpoint). See
[Web Dashboard](./web-dashboard.md#server-side-transcription).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` (within a present section) | Enable server-side transcription |
| `provider` | string | `openai` | Transcription provider |
| `model` | string | `whisper-1` | Transcription model |
| `endpoint` | string | OpenAI default | Custom endpoint URL (e.g. self-hosted whisper.cpp) |
| `language` | string | auto-detect | ISO-639-1 language hint |
| `buffer_secs` | float | `3.0` | Audio buffered before each API call (seconds) |

> Note on the `enabled` default: when the entire `[transcription]` section is
> **absent**, the field's struct default applies. When the section is
> **present** but `enabled` is omitted, the bool defaults to `false`. The CLI
> `--transcription` flag and a present `enabled = true` both turn it on.

### `[recording]`

ffmpeg-based recording of agent displays (see
[Integrations](./integrations.md#recording-ffmpeg)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable display recording |
| `framerate` | int | `15` | Capture frames per second |
| `segment_duration_secs` | int | `60` | Length of each recording segment |
| `quality` | string | `medium` | `low` (CRF 35), `medium` (CRF 28), `high` (CRF 20) |
| `max_retention_hours` | int | unset | Auto-delete segments older than this |

### `[computer_use]`

Provider/model used for visual-grounding (computer-use) tasks. See
[Computer Use & Live Audio](./computer-use-and-audio.md).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `provider` | string | auto-detect | CU model provider |
| `model` | string | auto-detect | CU model |
| `backend` | string | `auto` | Input/screenshot backend: `x11`, `wayland`, `macos`, or `auto` |

### `[agent]` and external backends

Routes coding tasks to an external CLI agent instead of the native loop (see
[Integrations](./integrations.md#external-coding-agent-clis)).

`[agent]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `default_backend` | string | unset (use native) | `codex`, `claude-code`, or `gemini` |

`[agent.codex]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `command` | string | `codex` | Path or command name for the Codex binary |
| `model` | string | unset | Model override |
| `approval_policy` | string | `on-request` | `untrusted`, `on-request`, or `never` (UI set; `on-failure` is deprecated upstream) |
| `sandbox` | string | `workspace-write` | `read-only`, `workspace-write`, or `danger-full-access` |
| `reasoning_effort` | string | unset (model default) | `minimal`, `low`, `medium`, `high`, `xhigh` |
| `service_tier` | string | unset (inherit Codex default) | `priority` enables Fast, `flex` requests Flex, `standard` is a sentinel that sends an explicit `serviceTier: null` to opt managed sessions out of Fast |
| `web_search` | bool | `false` | Enable the Responses-API `web_search` tool (`codex --search`) |
| `network_access` | bool | `false` | Allow outbound network in `workspace-write` sandbox (ignored for `read-only` / `danger-full-access`) |
| `writable_roots` | array | `[]` | Extra writable roots, each passed as `--add-dir` (absolute, or resolved against project root) |
| `managed_context` | string | `vanilla` | `vanilla` for upstream/original-fork Codex; `managed` enables proactive Intendant context densification, rewind/backout tools, disables Codex auto-compaction, and requires the patched Codex app-server protocol with lineage prompt-cache-key support |
| `context_archive` | string | `summary` | Context snapshot archive mode ("Context replay" in the UI): `summary` records compact per-request visualization data with temporary provider traces, `exact` persists full provider request payloads for raw replay, `off` disables capture |

Codex `app-server` launches in `managed_context = "managed"` suppress inherited
user-global Codex MCP/plugin/app servers by default and inject Intendant's MCP
endpoint plus the explicit toggles above. Set
`INTENDANT_CODEX_INHERIT_MCP_SERVERS=1` only for a managed launch that should
inherit the user's configured Codex MCP servers and plugins. Vanilla launches
preserve Codex's normal user configuration inheritance.

`[agent.claude_code]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `command` | string | `claude` | Path or command name |
| `model` | string | unset | Model override |
| `permission_mode` | string | `auto` | `default`, `acceptEdits`, `plan`, `auto`, `bypassPermissions` |
| `allowed_tools` | array | `[]` (all) | Restrict the tool set |

`[agent.gemini_cli]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `command` | string | `gemini` | Path or command name |
| `model` | string | unset | Model override |
| `approval_mode` | string | `default` | `default`, `auto_edit`, `yolo`, `plan` (matches `gemini --approval-mode`) |
| `sandbox` | bool | `false` | Pass `--sandbox` when spawning Gemini |
| `extensions` | array | `[]` (all) | Extension names to enable (`--extensions`) |
| `allowed_mcp_servers` | array | `[]` (all) | MCP server allowlist (`--allowed-mcp-server-names`); include `intendant` if you set one |
| `include_directories` | array | `[]` | Extra workspace roots (`--include-directories`, absolute) |
| `debug` | bool | `false` | Open Gemini's DevTools console (`--debug`) |

Unknown or empty values for `approval_policy`, `sandbox`, `reasoning_effort`,
and Gemini's `approval_mode` are normalized to the safe default so a config typo
cannot silently escalate privileges.

### `[live_audio]`

Untrusted voice sub-agent (zero tools, schema-validated) used for phone calls
and live voice (see [Computer Use & Live Audio](./computer-use-and-audio.md)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable live-audio sessions |
| `default_timeout_secs` | int | `300` | Session timeout |
| `gemini_model` | string | unset | Gemini Live model |
| `openai_model` | string | unset | OpenAI Realtime model |
| `sample_rate` | int | `24000` | Audio sample rate (Hz) |

### `[sandbox]`

Landlock filesystem sandboxing for the runtime (Linux). Also enabled by
`--sandbox`.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable Landlock sandboxing |
| `extra_write_paths` | array | `[]` | Extra writable paths beyond project root, `/tmp`, the log dir, and `~/.intendant` |

On kernels without Landlock support, sandboxing is silently skipped.

### `[webrtc]`

ICE servers for the WebRTC display transport (see
[Display Pipeline](./display-pipeline.md)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `ice_servers` | array of tables | `[]` (local-only) | STUN/TURN servers |
| `federation_allow_h264` | bool | `false` | Allow the federated (peer-to-peer) display path to negotiate H.264; default pins VP8 for lossy TURN-relayed paths. The local same-machine path is unaffected |

Each `ice_servers` entry: `urls` (array, required), optional `username`,
optional `credential`.

### `[server]` (daemon and federation)

What this daemon advertises to peers and requires of inbound connections. Most
deployments only ever touch `[server.tls]`.

`[server]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `bind` | string/IP | wildcard dual-stack, then `0.0.0.0` fallback | IP address the dashboard listens on. Use `127.0.0.1` or a specific interface for local/plaintext automation |
| `advertise` | array | `[]` (auto-detect) | WebSocket URLs to advertise in this daemon's Agent Card, preference order. The CLI `--advertise-url` is additive over this |

`[server.tls]` — native TLS-only HTTPS/WSS for the dashboard (pure-Rust
`rustls` + `rcgen`, all platforms; ORed with the `--tls` flag). The dashboard
defaults to mTLS; enable this section when you intentionally want HTTPS/WSS
without browser client-certificate authentication:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Serve the dashboard over HTTPS/WSS without requiring browser client certificates |
| `cert` | string | installed access certs, then auto self-signed | PEM cert (chain) overriding the default cert selection; pair with `key` |
| `key` | string | — | PEM private key (PKCS#8, PKCS#1, or SEC1) matching `cert` |
| `hostname` | string | — | Extra SAN hostname for the self-signed cert (in addition to bind IP + `localhost`) |

When TLS-only mode is enabled and `cert`/`key` are omitted, Intendant first looks
for the installed access server certificate in the per-user platform cert
directory (`server.crt` / `server.key`, normally created by `intendant access
setup`). If that pair is absent, it falls back to an ephemeral self-signed
certificate.

`[server.mtls]` — native client-certificate authentication for the dashboard
(ORed with the `--mtls` flag; this is the default dashboard transport):

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Explicitly require browser/client certificates during the TLS handshake; default behavior already does this unless `--tls` / `[server.tls]` or `--no-tls` is used |
| `ca` | string | installed access CA | PEM CA bundle used to verify client certificates |

Use `[server.tls]` only when the dashboard should be HTTPS/WSS without client
certificate access control. Default mTLS and `[server.mtls]` require a valid
client identity.

Use default mTLS, `[server.tls]`, `--tls`, the macOS app wrapper, or another
trusted HTTPS reverse proxy when a remote browser needs secure-context-gated
features: Station WebGPU, microphone/camera, browser screen capture, or stricter
clipboard APIs. Plain `http://<host-ip>` is not enough for those APIs, and
`--no-tls` on a wildcard listener refuses startup when the host has a public
interface unless `--allow-public-plaintext` is passed. The macOS
app wrapper starts its bundled backend with native mTLS by default and fails
closed with setup guidance when access certs are missing; see
[Web Dashboard: Secure Browser Contexts](./web-dashboard.md#secure-browser-contexts).

Peer access requests use the unauthenticated
`/api/peer-pairing/requests` doorbell endpoint so one daemon can ask another for
pairing approval. It is bounded and rate-limited, and approval still happens
locally before any client certificate is issued. Set
`INTENDANT_PEER_ACCESS_REQUESTS=0` to disable public request creation entirely.

`[server.peer_access_requests]` — public access-request hardening:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Allow unauthenticated callers to create bounded pending peer access requests; `INTENDANT_PEER_ACCESS_REQUESTS=0` still disables this at runtime |
| `body_limit_bytes` | integer | `4096` | Maximum body size for `POST /api/peer-pairing/requests` |
| `ttl_secs` | integer | `600` | Lifetime of a pending request before it expires |
| `max_pending` | integer | `32` | Global cap on simultaneously pending requests |
| `max_pending_per_source` | integer | `5` | Cap on simultaneously pending requests from one source IP/hint |
| `rate_limit_window_secs` | integer | `60` | Sliding-window duration for create-rate limits |
| `max_creates_per_window` | integer | `64` | Global request creations allowed per rate-limit window |
| `max_creates_per_source_per_window` | integer | `8` | Request creations allowed from one source IP/hint per rate-limit window |

`[server.auth]` — advanced compatibility auth for federation peers:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `advertised_transport` | string | `none` | What the Agent Card advertises: `none`, `mutual-tls`, or `pin-self-cert` |
| `bearer_token` | string | none | Legacy/advanced: require `Authorization: Bearer <token>` on inbound HTTP/WS; prefer mTLS/client certificates for normal access |

### `[[peer]]` — federated peers

Each `[[peer]]` block auto-registers a remote daemon at startup. Only `card_url`
is required.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `card_url` | string | (required) | URL of the peer's Agent Card (`.../.well-known/agent-card.json`) |
| `label` | string | from card | Display label override in the dashboard's Daemons panel |
| `bearer_token` | string | none | Legacy/advanced outbound token for peers that still require `[server.auth] bearer_token` |
| `client_cert` | string | installed access client cert when present | Peer-issued client certificate PEM for outbound mTLS; must be paired with `client_key` |
| `client_key` | string | installed access client key when present | Private key PEM for `client_cert`; must be paired with `client_cert` |
| `pinned_fingerprints` | array | `[]` | Operator-pinned SHA-256 cert fingerprints; when set, replaces the card's `auth.transport` claim |
| `browser_tcp_via_url` | string | from primary | Explicit URL the browser uses to reach this peer's HTTP port for WebRTC ICE-TCP |

Manual runtime URL additions from the dashboard live only in the in-memory
registry. Pairing flows are different: `intendant peer join <invite>` and
`intendant peer complete <request-id>` write these fields plus
`pinned_fingerprints` to `intendant.toml`. For independent mTLS daemons,
configure `client_cert` / `client_key` with a client identity issued by the
peer's access CA. The installed local access client cert fallback is only
sufficient when the peer trusts the same issuing CA.

### `mcp_servers`

External MCP servers to connect to as a client (see
[Integrations](./integrations.md#mcp-client) and the trust note below). Each is
an array-of-tables entry.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | (required) | Server name; tools are exposed as `mcp__<name>_<tool>` |
| `command` | string | (required) | Executable to spawn |
| `args` | array | `[]` | Arguments |
| `env` | table | `{}` | Environment for the child process |

> **Trust model:** an `mcp_servers` entry is spawned as a child process with
> your full privileges (`Command::new(command).args(args)`). Intendant performs
> **no** checksum, signature, or sandbox check on it — adding one is equivalent
> to adding a line to your `~/.zshrc` that runs a binary. The default is
> `mcp_servers = []`, and `intendant.toml` is git-ignored, so the repo ships no
> MCP servers. Treat copying an `intendant.toml` between machines like copying
> shell rc files: read it before sourcing. See [MCP Server](./mcp-server.md).

## Worked example

A reasonably full `intendant.toml`:

```toml
[memory]
enabled = true

[model]
context_window = 200000
max_output_tokens = 8192

[orchestrator]
max_parallel_agents = 4
sub_agent_dir = ".intendant/subagents"

[approval]
file_read = "auto"
file_write = "ask"
file_delete = "ask"
command_exec = "auto"
network = "auto"
destructive = "ask"
display_control = "ask"

[presence]
enabled = true
provider = "gemini"
model = "gemini-3-flash-preview"
context_window = 1048576
live_provider = "gemini"
live_model = "gemini-2.5-flash-native-audio-preview-12-2025"
live_context_window = 32768

[transcription]
enabled = false
provider = "openai"
model = "whisper-1"
language = "en"
# endpoint = "http://localhost:8080/v1/audio/transcriptions"

[recording]
enabled = false
framerate = 15
segment_duration_secs = 60
quality = "medium"
# max_retention_hours = 24

[computer_use]
provider = "gemini"
model = "gemini-2.5-flash"
backend = "auto"

[agent]
default_backend = "codex"

[agent.codex]
command = "codex"
model = "gpt-5.5"
approval_policy = "on-request"
sandbox = "workspace-write"
reasoning_effort = "medium"
web_search = false
network_access = false
writable_roots = []

[agent.claude_code]
command = "claude"
permission_mode = "auto"
allowed_tools = []

[agent.gemini_cli]
command = "gemini"
approval_mode = "default"

[live_audio]
enabled = false
default_timeout_secs = 300
gemini_model = "gemini-2.5-flash-native-audio-preview-12-2025"
openai_model = "gpt-4o-realtime-preview"
sample_rate = 24000

[sandbox]
enabled = false
extra_write_paths = ["/var/log"]

[webrtc]
federation_allow_h264 = false

[[webrtc.ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

# [[webrtc.ice_servers]]
# urls = ["turn:turn.example.com:3478"]
# username = "user"
# credential = "pass"

[server]
# bind = "127.0.0.1" # optional; use for local/plaintext automation
advertise = ["wss://192.168.1.42:8765/ws"]

[server.tls]
enabled = false
# cert = "/etc/intendant/server.crt"
# key  = "/etc/intendant/server.key"

[server.auth]
advertised_transport = "none"
# bearer_token = "legacy-advanced-only"

# [[peer]]
# card_url = "https://peer.example.com/.well-known/agent-card.json"
# client_cert = "/etc/intendant/peers/peer-client.crt"
# client_key = "/etc/intendant/peers/peer-client.key"
# bearer_token = "legacy-token-if-the-peer-requires-one"

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

## Autonomy and approval

Approval is decided by a three-layer model (full UI details in
[TUI & Autonomy](./tui.md)):

1. **Global autonomy** — `--autonomy <low|medium|high|full>` (defaults to
   `medium`). `low` asks for everything except file reads; `full` keeps the
   human entirely out of the loop (auto-approve) except for `HumanInput`.
2. **Category rules** — the `[approval]` section above (`auto`/`ask`/`deny`) per
   category.
3. **Per-action approval** — `y` / `s` / `a` / `n` (approve / skip / approve-all
   / deny) prompts in any frontend.

The nine action categories are: `FileRead`, `FileWrite`, `FileDelete`,
`CommandExec`, `NetworkRequest`, `Destructive`, `HumanInput`, `LiveAudioSpawn`,
`DisplayControl`. `DisplayControl` uses a session-grant model — approve once and
subsequent display actions skip the prompt (revocable in-frontend).
`HumanInput` and `LiveAudioSpawn` always require a human regardless of autonomy
level or category rule.

## Skills

Skills are named instruction sets stored as `SKILL.md` files with YAML
frontmatter, discovered from two directories (project-scoped first):

1. `<project-root>/.intendant/skills/<name>/SKILL.md`
2. `~/.intendant/skills/<name>/SKILL.md`

```yaml
---
name: deploy
description: Deploy the application to production
autonomy: high
disable-auto-invocation: true
---

## Steps

1. Run tests
2. Build release binary
3. Deploy to server
```

Frontmatter fields: `name` (required), `description` (required), `autonomy`
(override session autonomy when active), `disable-auto-invocation` (only the
user can trigger it), `disable-model-invocation` (run without LLM calls),
`sandbox` (override the session sandbox setting), `compatibility` (required
system tools), `allowed-tools` (restrict the available tool set). Project skills
take precedence over personal skills of the same name.

## INTENDANT.md project instructions

Place `INTENDANT.md` in your project root or at
`~/.config/intendant/INTENDANT.md` for global instructions. Both are loaded if
present (global first, project-local second) and injected into the conversation
at session start, before knowledge/memory.

## System prompts

System prompts are compiled into the binary, so `intendant` works from any
directory. Two base variants exist:

- `SysPrompt.md` — full prompt with JSON schema and per-function docs (used with
  text-based JSON extraction).
- `SysPrompt_tools.md` — condensed prompt for native tool calling (function docs
  live in the API tool definitions).

The active variant is chosen automatically based on whether the provider has
native tool calling enabled. Prompts resolve via a 3-layer cascade (highest
priority first):

1. **Project root** — `<git-root>/SysPrompt.md` (or `SysPrompt_tools.md`)
2. **Global config** — `~/.config/intendant/SysPrompt.md`
3. **Compiled-in default** — always available, zero-config

Role-specific prompts (`SysPrompt_orchestrator.md`, `SysPrompt_research.md`,
`SysPrompt_implementation.md`) follow the same cascade and append to the base.
The presence layer uses `SysPrompt_presence.md`; the live-audio voice agent uses
`SysPromptLiveAudio.md` with `{PLAYBOOK}` / `{RESPONSE_SCHEMA}` placeholders
substituted at runtime.
