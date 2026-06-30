# Web Dashboard

The web dashboard is Intendant's **default frontend**. It is a single-page app
served by the controller's built-in HTTP/WebSocket gateway, running entirely in
the browser with WASM-powered state management (the `presence-web` crate,
Catppuccin Mocha theme, mobile-responsive). The static SPA lives in
`static/app.html`.

## On by default

There is no opt-in: the gateway starts automatically unless you pass `--no-web`,
`--mcp`, or `--json` (those modes own stdio / are headless by contract). The
`--web` flag simply forces it on and optionally sets the port.

```bash
./target/release/intendant "task"          # dashboard comes up; URL is printed
./target/release/intendant --web            # explicitly enable
./target/release/intendant --web 9000       # explicit port
./target/release/intendant --no-web "task"  # disable; use the terminal TUI instead
```

The server binds port **8765** by default, auto-incrementing through 8785 if it
is busy; the chosen port is printed at startup. With the default mTLS transport,
open `https://<host>:<port>/` in a browser after running
`intendant access setup` and enrolling that browser/device. Use
`--bind 127.0.0.1` when starting plaintext local/debug dashboards with
`--no-tls`.

> **Correction vs. older docs:** `--web` is the default and no longer "implies
> `--mcp`". Earlier docs described `--web` as opt-in and tied to MCP mode —
> neither is true now.

## Secure Browser Contexts

The dashboard shell, Activity log, Sessions, Settings, and basic display viewing
can run over ordinary HTTP. Some browser capabilities are different: browsers
expose them only to a **secure context**.

Use a secure dashboard context when you need:

- **Station WebGPU rendering** (`navigator.gpu`) — otherwise Station falls back
  to its canvas-2D WASM renderer.
- **Microphone and camera** (`navigator.mediaDevices`, `getUserMedia`) for live
  voice, browser-side audio/video capture, or camera recording.
- **Screen/window capture from the browser** (`getDisplayMedia`) when a browser
  is the capture source.
- **Privileged browser APIs** such as the async clipboard in stricter browsers.

Practical rules:

- `https://` with a trusted certificate is the normal secure context for remote
  browsers.
- `http://localhost` and `http://127.0.0.1` are treated as secure by most
  desktop browsers, but not by every embedding. In particular, the macOS
  `WKWebView` wrapper uses the custom `intendant://` scheme because
  `http://localhost` there does not expose media devices.
- `http://<host-ip>` is not a secure context. Use default native mTLS,
  `--tls` with a trusted certificate, the macOS app wrapper, or another trusted
  HTTPS reverse proxy. The macOS app wrapper starts its bundled backend with
  mTLS by default and fails closed with setup guidance when access certs are
  missing.
- Clicking through a self-signed certificate warning is not a reliable substitute
  for installing/trusting the certificate; browsers may still withhold secure
  APIs.

### Headless daemon posture

When the dashboard is on, the terminal **TUI does not own the TTY**. The
controller runs in a headless/daemon posture and tees its stdout/stderr to
`daemon.log` in the session directory (so the dashboard's "Download session
report" can include controller output). With no task argument the agent simply
starts idle and waits for tasks submitted from the dashboard.

If you want the classic in-terminal TUI instead, run with `--no-web` on a real
terminal — then the TUI takes the foreground. See [TUI & Autonomy](./tui.md).

## Tabs

The top tab bar has ten tabs: **Activity**, **Stats**, **Terminal**,
**Video**, **Station**, **Sessions**, **Files**, **Access**, **Debug**, and
**Settings**. New events arriving while you are on another tab raise a
notification badge.

### Activity

The default tab. Five subtabs:

- **Log** — a scrollable, color-coded event stream of everything in the system,
  grouped by turn with visual separators, with a verbosity selector
  (Normal/Verbose). Event sources are color-coded:
  - **system** — session lifecycle, approvals, context management
  - **worker** — model responses, reasoning summaries, task completion
  - **agent** — command execution output (stdout/stderr, exit codes)
  - **live** — voice transcripts, presence lifecycle, tool requests
  - **server** — presence model internals (thinking, tool calls)

  The Log pane also carries the approval controls (Approve / Skip / Approve All
  / Deny) and a follow-up text input for sending a message after a round
  completes.
- **Context** — the agent's current working context (what it is operating on).
- **Managed** — operator console for managed-Codex context maintenance (see
  below).
- **Changes** — file changes / diffs produced during the session (with its own
  badge when new changes land).
- **Control** — direct controls for steering the run.

#### Managed (Activity → Managed)

The manual counterpart to the model-driven managed-context tools. A session
picker lists Codex-like sessions — live windows plus historical sessions from
the session store — sorted prompt-target first, then managed-mode, live, and
most recently updated (labels show name, short id, source, and `via <id>` when
the Codex thread is reached through an Intendant wrapper session). **Use
target** snaps back to the current prompt target.

For a live session the pane calls the per-session MCP `get_status` and renders
a density card: managed/vanilla mode, pressure status, effective and hard-limit
token usage with a colored pressure bar, the soft rewind-at threshold, and
whether rewind-only gating is active. When the verified dashboard-control
tunnel is connected, these dashboard-originated MCP `tools/call` requests use
`api_mcp_tool_call`; otherwise they fall back to `/mcp?session_id=...`.
Historical sessions show `historical` status — records and anchors stay
readable, but live actions are disabled.
Alerts flag non-Codex selections, sessions without managed mode, an
insufficient last rewind, and a configured Codex command that doesn't look like
the patched managed build.

- **Rewind** — manual `rewind_context` dispatch with an exact item anchor
  (`call_id` or response item id) plus anchor side (`before`/`after`), a
  required reason and carry-forward primer, and optional preserve / discard /
  artifacts / next-steps lists (one entry per line). **Inspect anchor** runs
  `inspect_rewind_anchor` to show a small window around the candidate before
  committing.
- **Recent anchors** — harvested from the live activity log and the
  `/api/managed-context/anchors` history, each with a one-click **Use** that
  fills the anchor field (switching the picker to the anchor's session if
  needed).
- **Records / Backout** — the session's rewind records from
  `/api/managed-context/records`; clicking one shows its JSON and fills the
  backout form, which runs `rewind_backout` in `inspect`, `restore`, `fork`,
  or `backout` mode with an optional fork name.
- **Lineage and fission** — the ledger card. Lineage groups come from the live
  `get_status` payload; fission groups come from
  `GET /api/managed-context/fission`, the merged ledger + extension view that
  works for historical sessions too (live-status `fission_ledger` groups are
  only a fallback when the endpoint has nothing yet). A fission group row
  shows the group id, its anchoring tool (`fission_spawn`, or
  `fission_spawn:head` when the spawn fell back to the catalog head), the
  spawn anchor item id, the canonical session (`--` when unclaimed), and — for
  severed groups — a **detached** chip carrying the detach time and reason.
  Each branch row carries a status chip colored by the ledger's canonical
  status vocabulary (`running` / `blocked` / `completed` / `failed` /
  `detached` / `cancelled`; legacy raw values fold the same way the ledger
  normalizes them), a **canonical** chip on the claimed branch, an
  **imported** chip once the branch result was imported, a changed-file
  count, the branch charter (objective, write scope, worktree path), and its
  latest summary. (For the fission model itself — charters, worktrees,
  detach-on-rewind — see
  [External-Agent Orchestration](./external-agent-orchestration.md).)
- **Per-branch fission actions** — **Wait** / **Import** / **Cancel** /
  **Detach** run `fission_control` against the selected session. Wait uses a
  60 s window, and a `still_running` result is surfaced as an info toast, not
  an error; import, cancel, and detach ask for confirmation first, with
  cancel and detach styled as destructive. **Claim** calls
  `claim_fission_canonical`, passing the group's current canonical id as the
  compare-and-swap guard when one exists.
- **Spawn fission branches** — the spawn form above the ledger list: one to
  four branch rows (objective required; optional comma-separated write scope
  and display name; **Add branch** adds rows, each row has a remove control,
  and the last row is always kept) plus a tri-state worktree select —
  `default` omits `use_worktree` so write-scoped branches in a git project
  get isolated worktrees, while `on`/`off` force it either way — submitted as
  a single `fission_spawn` call for the selected session.
- **Copy status JSON** copies the raw status payload.

Rewind, backout, inspect, and fission spawn stay disabled unless the selected
session is live and effectively managed. The pane refreshes when the Managed
subtab is opened and re-schedules itself (only while the subtab is active)
after each pane action, thread-action result, session start, and usage update.

### Stats

Token-consumption and cost tracking:

- Per-model breakdown for the main and presence models (prompt, completion, and
  cached token counts)
- Cost estimates from a built-in pricing table (OpenAI, Anthropic, Gemini)
- All-sessions cumulative usage and disk usage
- Display-transport metrics (frame rate, encode latency, bandwidth per display)

### Terminal

An embedded xterm.js terminal. Two subtabs:

- **TUI** — the server-side ratatui TUI, rendered per-connection. Each browser
  connection gets its own `WebTui` with independent dimensions, so two browsers
  can size the terminal differently. This is the same status bar / log panel /
  action panel / approval-and-input UI as the native terminal.
- **Shell** — an interactive shell session.

### Video

WebRTC display viewers for the agent's graphical displays, with interactive
control (see [Display Pipeline](./display-pipeline.md)):

- **View mode** (default) — watch the agent's display in real time
- **Take Control** — forward mouse and keyboard events to the agent's display
- **Release** — relinquish control, with an optional note
- **Display picker** — choose which monitor to view when several are present
- **Recording replay** — browse and play back recorded sessions with timeline
  seeking and speed control (1x / 2x / 4x)

Displays appear automatically when the agent's first command triggers Xvfb
auto-launch, or when access to the user's real session display is granted.
WebRTC negotiation (SDP offer/answer + ICE candidates) is multiplexed over the
existing dashboard WebSocket. When the verified dashboard-control DataChannel is
connected, local display input authority requests and keyboard/mouse input can
use that daemon-scoped control tunnel; video media still flows through the
per-display WebRTC session.

### Station

An immersive WASM-rendered control center for the same operational surfaces as
the rest of the dashboard — Activity, Context, Managed context, Changes,
Sessions, Peers/displays, and Control. The `station-web` crate draws the whole
scene into a single canvas: WebGPU when the browser exposes it (a secure
context is required — see Secure Browser Contexts above), with a canvas-2D
WASM fallback used automatically when WebGPU is unavailable or forced with
`?station_gpu=canvas`. The renderer runs on `requestAnimationFrame` and
re-renders only when state or view input changes, so an idle Station stays
cheap.

There is no DOM dock: the rendered scene is the UI. An invisible hotspot
overlay mirrors the scene's interactive elements so they stay reachable from
the keyboard. Station actions dispatch through the same control plane as the
classic tabs, so anything triggered from Station behaves exactly like its
canonical dashboard equivalent. View settings shape the scene: layout
(`orbital` / `constellation`), mood (`calm` / `cockpit`), and fov, motion, ar,
and density tuning.

### Sessions

A browser of past and current sessions. Four subtabs:

- **Recent** — recent sessions with metadata (task, duration, status); click one
  to view its recordings and event log. Child sub-agent sessions are hidden by
  default; enable **Show subagents** to include them. Fork and side sessions
  stay visible with lineage chips that point back to their parent session.
- **Deep Search** — search across session history.
- **Worktrees** — the git worktrees in use by sub-agents.
- **New Session** — start a fresh session from the dashboard.
  External Codex sessions can choose both the binary path and the
  `managed_context` mode (`vanilla` or `managed`) for that session.

External-agent session cards and Activity windows also expose **Launch config**
for per-session binary and managed-context settings. Use **Save** to update the
next attach/resume, or **Save & restart** to apply the new binary/mode
immediately to that external backend. These settings are stored with the
Intendant wrapper session and, for canonical backend session IDs, in an
external-session overlay. They are used on the next attach/resume so a daemon
restart or page refresh does not fall back to the current global Settings pane.
The separate **Restart with saved config** action is a power-user shortcut for
reapplying settings that were already persisted elsewhere.
The Managed activity view exposes rewind anchors, saved records, restore, and
fork/backout actions. With the patched managed Codex binary, fork/backout starts
a new Codex thread while inheriting the saved rollout's lineage prompt-cache key;
there is no separate cache-reset opt-in in the dashboard.
Editable user-message entries still perform an in-place rollback when the
message is active in the current thread. Superseded user-message entries in a
managed Codex session show the same edit control as a historical branch action:
submitting the text creates a child thread from the newest saved pre-rewind
rollout containing the clicked message, rolls that child back to the selected
turn, and sends the replacement there. The edit chip labels this as branching so
the active compacted session is not mistaken for the target of the mutation.

### Files

Browse staged uploads and download files from the local daemon or a configured
peer target. The target summary uses the same access abstraction as Terminal:
local/mTLS, hosted transports, and peer dashboard-control routes are shown as
targets with their available capabilities rather than as transport internals.

### Access

Unified administration for how dashboards and daemons reach each other:

- **Targets** lists daemon targets, not raw transports. A target can be this
  daemon over user/client access, a hosted-transport daemon, a direct browser-mTLS
  daemon, or a peer-routed daemon. Each row shows the access domain, route, and
  available capabilities, then links to Stats, Files, and Shell.
- **Grants** separates user/client access from peer access. The current browser
  is user/client access and is root dashboard access in the single-user product.
  Inbound peer identities are daemon-to-daemon grants with peer profiles. The
  tab renders the overview-backed principal, supported-principal-kind, grant,
  policy, and transport rows so the summary cards and detailed model use the
  same source of truth.
- **Invitations** contains peer onboarding flows: Grant Peer Invite, Join Invite,
  Request Peer Access, Manual Add, and inbound peer access requests.
- **Public Shares** is the placeholder surface for future explicit public or
  hosted-service grants; by default nothing is shared publicly.
- **Diagnostics** owns dashboard route health, including hosted Connect,
  local/mTLS, local WebRTC control, event delivery, byte streams, uploads, and
  self-tests.

Access uses one vocabulary across the hosted dashboard, direct/self-hosted mTLS,
and peer federation:

- A **target** is a daemon the dashboard can operate.
- A **principal** is the actor being trusted: the current browser session, a
  future passkey/account principal, a future browser-certificate human
  principal, an organization group, or a peer daemon.
- A **grant** connects one principal to one target with a role and status. The
  current browser has a root user/client grant to the local daemon. A peer route
  has a daemon peer-profile grant. An approved inbound peer identity appears as
  a peer-daemon principal with a peer-profile grant to this daemon; revoked
  identities remain visible as revoked grants for audit clarity.
- A **policy** defines the shape of authority behind a grant. `root` and
  `peer-profile` are enforced today. Scoped human IAM, directory-scoped file
  access, and public shares are design targets, not hidden enforcement.
- A **transport** is only how the route is carried: browser mTLS, hosted
  Connect/WebRTC tunnel, local/debug HTTP, or daemon-to-daemon peer mTLS. The
  product UI should not make Connect a separate access system.

The important security-domain split is:

- **User/client daemon access** means a human-operated dashboard can control a
  daemon. Hosted Connect passkey access and browser mTLS client certificates are
  both in this domain. Today that is intentionally root dashboard access for the
  owner. Future coworker/team access should be user-scoped IAM here, not peer
  federation.
- **Peer access** means one daemon can call capabilities on another daemon. That
  uses daemon-to-daemon mTLS identities and peer profiles such as `peer-operator`
  or `peer-root`. Peer access does not imply that the human's browser can open
  the remote daemon directly, and browser access does not imply that two daemons
  can federate.

The model is backend-backed. `GET /api/access/overview` and the
dashboard-control `api_access_overview` method return schema version 1 with
`scope`, `targets`, `principals`, `grants`, `policies`, `transports`,
`supported_principal_kinds`, and explicit unresolved architecture notes. This
overview is descriptive for now: it exposes one product model over the current
enforcement paths rather than replacing mTLS, Connect account checks, or peer
profiles.

`GET /api/dashboard/targets` and `api_dashboard_targets` remain the compatibility
target model used by older UI paths: target id/host id, display label, access
domain (`user_client` or `peer`), route (`current_dashboard` or `peer_route`),
effective role (`root` or `peer_profile`), connection state, and capability
hints. The browser may refine the local route label to **Hosted transport**,
**Browser mTLS**, or **Local/debug** because only the browser knows how the
current page was reached, but it should not invent principal/grant/policy
vocabulary.

### Debug

A raw view of internal state — the same data as the `GET /debug` endpoint
(agent state, voice connection, active browser), useful when diagnosing the
gateway or presence wiring. It also includes a browser-workspace panel for
manual smoke testing of local CDP-backed browser workspaces and their leases.
CDP workspaces prefer managed Chromium/Chrome-for-Testing executables; on macOS
system Chrome/Chromium apps require choosing `system_cdp` or setting
`INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1`. Run
`intendant setup browsers` to install or repair the managed browser cache.

### Settings

The configuration panel for the current session: API keys, external-agent
backend settings, computer-use/provider options, presence, transcription,
recording, and live audio. Peer/network administration moved to **Access**.
Old `#settings/network` deep links are redirected to `#access/targets`.

## Late-join and session replay

The gateway is built so a browser that connects late sees the full picture
immediately. On WebSocket connect the server sends a sequence of bootstrap
messages:

1. **`state_snapshot`** — the full `AgentStateSnapshot` plus this connection's
   id, the `/config` payload, and the `session_id`
2. **Cached `usage_update`** — latest token usage
3. **Cached `status`** — latest autonomy / session id / task
4. **Cached `display_ready`** — latest display info for WebRTC sessions
5. **`browser_workspace_snapshot`** — active browser workspaces and lease state
6. **`log_replay`** — historical session events parsed from `session.jsonl`

So refreshing the page, or opening a second browser mid-run, replays the
session rather than starting from blank.

## Live voice (optional)

The dashboard supports optional low-latency voice via **Gemini Live** or
**OpenAI Realtime**. Voice is entirely optional — the dashboard is fully usable
without it.

When activated:

- The browser connects **directly** to the model's realtime API for voice I/O.
- The WASM layer (`presence-web`) handles mic capture, resampling, and WebSocket
  streaming.
- The live model receives agent events and narrates progress, and can call
  presence tools (`submit_task`, `approve_action`, `check_status`, …) which are
  routed over the dashboard WebSocket to the server.
- Server-side text presence is automatically paused (the two are mutually
  exclusive).

### Voice setup

1. Enter your provider API key on first visit (Gemini or OpenAI).
2. Keys are stored in browser **localStorage** and are never sent to the
   Intendant server (the server only mints short-lived session tokens via
   `POST /session`).
3. Click the microphone button to connect.

### Active vs. passive browsers

Only one browser can be the **active** voice controller at a time:

- The first browser to connect voice becomes active.
- Additional browsers are passive observers — they receive events and TUI frames
  but do not pause server-side presence.
- A passive browser can request active status, which force-disconnects the
  previous active browser. Handover carries the last checkpoint summary and
  conversation context.

### Session continuity across reconnects

The presence session protocol survives refreshes and dropped connections:

1. On connect the server sends a `presence_welcome` with current state, missed
   events, and conversation context.
2. The browser sends periodic `presence_checkpoint` messages summarizing the
   conversation.
3. On reconnect the server replays events since the last checkpoint.

This keeps the voice model from losing context. The protocol and mutual
exclusion are detailed in [Presence Layer](./presence.md).

## Server-side transcription

Independently of browser-side voice, the server can transcribe microphone audio
via the Whisper API when `[transcription]` is enabled (or `--transcription` is
passed):

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
language = "en"
```

The browser streams PCM16 audio; the server buffers it in ~3s chunks
(`buffer_secs`, RMS-filtered to skip silence) and sends each chunk to the
transcription endpoint. Transcripts are broadcast as `user_transcript` events
and written to the session log. See
[Configuration](./configuration.md#transcription).

## Secure Browser Contexts

- **Microphone/camera require a secure context.** Plain `http://<host-ip>` is not
  a secure context in the browser, so `getUserMedia` is blocked there. Reach the
  dashboard over one of:
  - `http://localhost` (e.g. an SSH tunnel: `ssh -L 8765:localhost:8765 host`),
  - HTTPS/mTLS via the default dashboard transport, `--tls`, or `[server.tls]`
    (see below), or
  - the macOS app bundle, which serves the page over a custom `intendant://`
    scheme specifically to restore the secure context (see
    [Getting Started](./getting-started.md#macos-app-bundle)). The bundle starts
    its daemon with native mTLS by default so remote browsers get a safe context
    over `https://` and must present an enrolled client identity.
- **API key for voice:** Gemini or OpenAI, stored browser-side only.

### HTTPS / TLS

```bash
./target/release/intendant                       # default: mTLS, requires access certs
./target/release/intendant --tls                 # TLS-only; installed access certs when present, else self-signed
./target/release/intendant --no-tls --bind 127.0.0.1 # explicit local plaintext/debug escape
./target/release/intendant --tls-cert c.pem --tls-key k.pem   # bring your own
```

By default, the gateway serves HTTPS/WSS with browser client certificates
required. `--tls` (or `[server.tls] enabled = true`) makes the gateway serve
HTTPS/WSS without requiring client certificates. With no explicit cert/key
override, TLS-only uses installed access server certs when present and falls back
to an auto self-signed certificate. Plain HTTP via `--no-tls` is intended for
local/programmatic debugging; wildcard plaintext refuses startup when the host
has a public interface unless `--allow-public-plaintext` is passed.
The gateway demuxes per connection: a first byte of `0x16` (a TLS ClientHello)
is wrapped in the rustls acceptor, while raw WebRTC ICE-TCP/UDP media is left
untouched. The TLS stack is pure Rust (`rustls` + `rcgen`) and works on every
platform including Windows — no nginx, no OpenSSL. See the
`[server.tls]` keys under
[Configuration → `[server]`](./configuration.md#server-daemon-and-federation).

For explicit mutual-TLS with client certificates (only enrolled devices can
connect), use native `--mtls` / `[server.mtls]`; this is also the default when no
transport flag is supplied. Use `intendant access setup` to generate the
per-user access CA/server/client certs and run strict enrollment. See
[Getting Started → Dashboard access over TLS](./getting-started.md#dashboard-access-over-tls) and
[Peer Federation](./peer-federation.md). For the daemon posture and remote
control surface, see [Control Plane & Daemon](./control-plane-and-daemon.md).

### WebRTC Dashboard Control Tunnel

Intendant also has an experimental daemon-scoped WebRTC DataChannel transport
for dashboard control traffic. It is not a replacement for dashboard
authentication yet: today the browser still bootstraps from the normal dashboard
origin, then uses the daemon's local `/connect/dashboard/*` signaling endpoints
to establish the DataChannel. The existing WebSocket signaling path remains as
a compatibility fallback for older dashboard bundles. The point is to prove the
future "public HTTPS bootstrap + direct local daemon data path" shape without
weakening the current mTLS default.

The handshake is bound to the daemon identity:

- the browser creates a `intendant-dashboard-control` DataChannel and sends an
  SDP offer to `/connect/dashboard/offer`;
- the daemon answers with SDP plus a signed binding over the offer hash, answer
  hash, session id, timestamp, and daemon Ed25519 public key;
- the browser verifies that binding with WebCrypto before using the channel.
- in Connect-rendezvous mode, the browser also requires the answer to carry the
  daemon public key registered for the selected daemon id and rejects the tunnel
  if that advertised key differs from the key inside the signed binding.

When enabled with
`localStorage.intendant_dashboard_transport = "webrtc-control"` (or
`window.intendantDashboardControl.enable()`), dashboard JSON reads prefer the
DataChannel and fall back to HTTP through the browser-side `DashboardTransport`
boundary. Current tunneled reads include the local Agent Card identity, sessions,
session detail, lazy command-output loads for the active session,
active-session timeline history, active-session changes/diffs, lazy exact
context-snapshot loads, filesystem picker stat/list/mkdir operations, deep
session search, settings, API-key status, server-side voice-session token
minting, project root, display enumeration, recording metadata, worktree
inventory, staged-upload descriptors, scoped recording asset byte streams,
archived session frame image byte streams, bounded session-report zip downloads,
and peer state.
Managed-context history reads for records, anchors, and fission groups also use
the tunnel.
Current tunneled mutations include
active-session rollback/redo/prune, session-data deletion, staged upload
deletion, settings save, API-key save, peer add/remove, peer access-request
pairing, peer message/task/approval actions, peer-display WebRTC signaling,
eligible-peer lookup,
visual-freshness diagnostics NDJSON appends, worktree scan/remove, dashboard
managed-context MCP tool calls, coordinator routing, and dashboard
session-control and dashboard-action controls. Annotation attach/save/send and
clip creation use a dedicated dashboard media/editor protocol over the same
verified channel: annotation image bytes travel as `upload_*` frames committed
by `api_media_annotation_attach` or `api_media_annotation_submit`; clips use
`api_media_clip_start`, ordered `api_media_clip_frame` uploads keyed by
`clip_id`, then `api_media_clip_end` or `api_media_clip_cancel`.
Allowlisted settings-style `ControlMsg`s, such as autonomy, approval-rule,
external-agent, Codex, Gemini, and verbosity settings, can also dispatch over
the DataChannel when it is verified. Display input authority uses dedicated
DataChannel RPCs and a `display_input` frame rather than the generic
`ControlMsg` allowlist. The standalone Shell terminal subtab uses dedicated
`terminal_*` frames over the same verified channel. The server-side ratatui TUI
subtab uses `tui_subscribe`, `tui_key`, `tui_resize`, `tui_unsubscribe`, and
`tui_close` frames when the daemon has a live WebTui renderer; idle daemon
launches that do not run WebTui advertise `tui_frames_available: false`. In
daemon-origin dashboards the old WebSocket path remains the fallback TUI
transport; in public-origin Connect mode the browser does not attempt that
fallback. Session lifecycle,
steering, approvals, interrupt, resume, stop/restart, rename, and launch-config
changes use a separate
`api_session_control_msg` RPC with its own allowlist instead of broadening the
generic settings-style `api_control_msg`. Smaller dashboard action controls use
`api_dashboard_action_msg`; this includes Codex/Gemini thread actions, display
take/release/grant/revoke, the diagnostics visual-marker toggle, recording and
debug toggles, and browser workspace create/acquire/close/release. It has its
own allowlist and the same no-replay fallback rule as the other mutation RPCs.
Mutation fallbacks are deliberately conservative: if a connected WebRTC RPC
fails after it may have reached the daemon, the dashboard surfaces the error
instead of repeating the write over HTTP. The visual-freshness sampler follows
the same rule for NDJSON appends; it uses the legacy HTTP endpoint only when no
verified DataChannel path is available.

The tunnel mirrors HTTP JSON response semantics. Application errors travel as
successful transport frames with `_httpStatus`/`_httpOk` metadata so existing UI
code can render the same error message it would render for an HTTP response.
Transport failures, unknown RPC methods, and aborted requests still reject the
browser-side promise.

Several paths intentionally stay outside this JSON tunnel:

- static assets and WASM bundles;
- native media fallback URLs and transfer paths outside the scoped dashboard
  byte/upload protocols;
- general filesystem mutations and durable broad file-transfer queues;
- generic MCP-over-HTTP for external clients;
- non-allowlisted `ControlMsg` mutations;
- display WebRTC media channels;
- daemon-to-daemon federation authentication.

Peer mTLS remains a separate trust boundary. The dashboard tunnel authenticates
the browser-to-this-daemon control path; it does not grant or replace a
daemon's peer-scoped client certificate for federation.

### Connect-Style Local Bootstrap Slice

The daemon also exposes a narrow, experimental Connect-style bootstrap surface
for testing the public-origin signaling shape without changing the normal
dashboard:

| Endpoint | mTLS? | Purpose |
|----------|-------|---------|
| `GET /connect/bootstrap` | not required | Minimal HTML bootstrap page for WebRTC dashboard-control transport testing |
| `GET /connect/status` | not required | JSON health/capability probe for the bootstrap surface |
| `POST /connect/dashboard/offer` | not required | Browser SDP offer -> daemon SDP answer plus signed binding |
| `POST /connect/dashboard/ice` | not required | Browser trickle ICE candidate for a control session |
| `POST /connect/dashboard/close` | not required | Close a control session |

Those paths are deliberately allowlisted one by one. They do **not** make `/`,
`/config`, `/ws`, `/api/*`, assets, recordings, or the full dashboard available
without the normal dashboard authentication. The bootstrap page exposes
`window.intendantConnectDashboard` for tests and diagnostics; it verifies the
same daemon-signed binding as the full dashboard control experiment, then uses
the DataChannel RPC protocol directly. Its small browser-side transport supports
plain JSON requests, chunked JSON responses, bounded `byte_stream_*` downloads,
and `upload_*` frames, so the local bootstrap check can cover both read-style
artifacts and media/editor writes without making the full dashboard certless.
These local endpoints are useful for same-origin dashboard experiments and
diagnostics; by themselves they do not solve browser trust for a public page
talking to a daemon HTTPS certificate the browser has not already accepted.

Run the focused browser check against a local daemon with:

```bash
PLAYWRIGHT_NODE_PATH=/path/to/node_modules \
  node scripts/validate-connect-bootstrap.cjs --origin https://127.0.0.1:8766
```

The check intentionally uses no client certificate. It must see `/config`
rejected with `401`, then prove that `/connect/bootstrap` can create a verified
dashboard-control DataChannel, issue RPC requests, read a bounded byte stream,
and commit media/editor uploads over the tunnel.

To test the full dashboard bundle's local signaling path, run a loopback-only
plaintext debug daemon through:

```bash
node scripts/validate-dashboard-control-local-signaling.cjs \
  --dashboard-binary ./target/release/intendant \
  --daemon-port 8877
```

That harness enables `window.intendantDashboardControl` in the real SPA and
asserts that the verified DataChannel reports `signalingMode: "local-http"`.

This slice is a local low-level harness for the dashboard-control tunnel. It
does not implement account login, passkeys, daemon claiming, or a durable daemon
registry. Its job is to keep the same-origin tunnel protocol easy to exercise
while the hosted Connect service owns the account and daemon-claim UX.

### Local Rendezvous Emulator Slice

The next experimental slice moves signaling off the daemon-served page. A daemon
can opt into an outbound rendezvous client with `[connect]` or the
`INTENDANT_CONNECT_RENDEZVOUS_URL` environment variable. In that mode:

1. The daemon registers a daemon id and daemon identity public key with a
   rendezvous endpoint.
2. The daemon long-polls the rendezvous endpoint for dashboard-control offers,
   ICE candidates, and close requests.
3. A browser loads a separate public-origin emulator page instead of a daemon
   page.
4. The emulator brokers SDP/ICE only; the browser and daemon still establish a
   direct WebRTC DataChannel when ICE succeeds.
5. The browser verifies the same daemon-signed binding before issuing RPC over
   the channel.

Run the end-to-end validator with:

```bash
node scripts/validate-connect-rendezvous.cjs
```

That script uses Playwright when it is installed (`PLAYWRIGHT_NODE_PATH` may
point at a temporary `node_modules`), otherwise it falls back to launching
Chrome/Chromium through the DevTools Protocol. The fallback honors
`INTENDANT_BROWSER_WORKSPACE_EXECUTABLE`, `INTENDANT_BROWSER_EXECUTABLE`,
`CHROME_PATH`, and `CHROME_BIN`.

The validator starts a local rendezvous HTTP origin, launches a fresh daemon
child with Connect env vars, verifies that
`https://127.0.0.1:<daemon-port>/config` still rejects a certless request with
`401`, verifies that daemon rendezvous endpoints reject missing bearer auth, and
then performs these browser passes:

1. It loads the minimal public bootstrap page from the rendezvous origin and
   drives `status`, `config`, `api_sessions`, id-filtered `api_sessions`,
   streamed `api_sessions_stream` hydration, a chunked large
   `api_sessions_stream` event, a chunked large `api_sessions` response,
   active-session command-output lookup, active-session timeline
   lookup/validation, bounded byte streams, uploads, media/editor writes, and
   application error RPCs over the verified DataChannel.
2. It serves the real `static/app.html` bundle from the same public origin at
   `/app?connect=1&daemon_id=...`, proves that it uses rendezvous signaling
   (`signalingMode: "connect-rendezvous"`), and checks that first-load dashboard
   data such as config, Agent Card identity, sessions, bootstrap frames, event
   subscription, and visible transport status all arrive through
   `window.intendantDashboardControl` instead of same-origin daemon HTTP/WSS.
   It also asserts that the SPA's signed daemon binding key matches the daemon
   public key registered with the rendezvous service for the selected daemon id.
   It injects a synthetic `api_control_msg` failure in the connected SPA and
   verifies that the generic settings-style write path does not replay the same
   mutation over the legacy WebSocket.
   This real-SPA pass also fails if the public-origin dashboard attempts daemon
   REST/media/WebSocket fallback paths such as `/config`,
   `/.well-known/agent-card.json`, `/api/...`, `/recordings`,
   `/connect/dashboard/...`, or `/ws`.
3. It opens the same real dashboard with an unregistered daemon id and asserts
   that the UI reports a Connect failure while still avoiding those public-origin
   REST/media/WebSocket fallbacks. This page must also stop daemon-dependent
   startup hydrators such as settings, project-root, and recording refreshes so
   the initial rendezvous failure does not cascade into unrelated errors.
4. It opens the real dashboard with the same registered daemon id while the
   emulator deliberately tampers with the advertised registry key for that offer.
   The SPA must reject the tunnel before it stores a verified binding, report a
   failed Connect transport, and still avoid daemon REST/media/WebSocket
   fallbacks.
5. It opens the real dashboard with the same registered daemon id while the
   emulator deliberately tampers with the browser-visible session grant. The
   daemon signs the grant hash it received in the offer event, so the SPA must
   reject the answer before it stores a verified binding or grant hash.
6. It opens the real dashboard while the emulator deliberately tampers with the
   browser challenge nonce forwarded to the daemon. The daemon signs the nonce it
   received, so the SPA must reject the answer before it stores a verified
   binding or expiry.

This is still a protocol emulator rather than the consumer Connect service. It
has no account, passkey, daemon claim, revocation, audit log, or hosted public
HTTPS. The emulator's grant is only an opaque per-offer session value used to
prove that a Connect-issued grant can be carried through signaling and bound
into the daemon-signed WebRTC session statement.

### Hosted Connect Production Alpha

The hosted-service slice is implemented as a separate binary,
`intendant-connect`. It serves a public web origin, handles passkey-only account
registration/login, lets a signed-in user claim a daemon with a short-lived
12-word phrase, and brokers dashboard WebRTC signaling without asking the
browser to trust the daemon's private HTTPS certificate.

In production, run it behind ordinary public TLS for a public origin such as
`https://connect.intendant.dev`:

```bash
INTENDANT_CONNECT_TOKEN="$(openssl rand -base64 32)" \
  ./target/release/intendant-connect \
    --listen 127.0.0.1:9876 \
    --origin https://connect.intendant.dev \
    --rp-id intendant.dev \
    --static-root static \
    --data-file <state-file>
```

The `--rp-id intendant.dev` value means passkeys are scoped to the owned
Intendant parent domain while the actual UI can live on `connect.intendant.dev`.
For compatibility, the live production-alpha instance currently keeps its
original `INTENDANT_CONNECT_RP_ID=connect.intendant.dev`; changing that value is
a credential migration and existing users must register new passkeys. Browsers
also allow `http://localhost:<port>` as a secure context for local development,
so the same binary can be E2E-tested without public TLS.

The daemon side still uses the normal `[connect]` outbound rendezvous client:

```toml
[connect]
enabled = true
rendezvous_url = "https://connect.intendant.dev"
daemon_id = "vortex-deb-x11-intendant"
auth_token = "same daemon token configured on intendant-connect"
```

The hosted MVP flow is:

1. The daemon registers its `daemon_id` and persistent daemon identity public
   key through `/api/daemon/register`.
2. If the daemon is unclaimed, Connect returns a short-lived claim phrase and
   URL. The phrase is a standard 12-word BIP39 English mnemonic generated from
   128 bits of entropy, stored only as a hash at rest, and regenerated if it
   collides with another active unclaimed daemon.
3. The user opens Connect, signs in or registers with a passkey, and submits the
   claim phrase.
4. Connect sends a `claim_challenge` event to the daemon. The daemon signs that
   challenge with its daemon identity key, and Connect verifies the signature
   before assigning ownership.
5. The user chooses the daemon in Connect and opens the dashboard. Connect
   issues a short-lived opaque dashboard grant, forwards the browser SDP offer
   to the daemon, and waits for the daemon answer.
6. The daemon signs the same WebRTC binding used by the local/rendezvous paths,
   including the offer hash, answer hash, browser nonce, expiry, daemon public
   key, and hash of the Connect-issued grant.
7. Connect validates that the answer came from the registered daemon key and
   that the signed grant hash matches before returning the answer to the
   browser. The browser independently verifies the daemon-signed binding before
   sending dashboard RPC over the DataChannel.

The state file durably stores users, passkeys, daemon ownership, hashed claim
phrases, and a capped audit log. Plain claim phrases, WebAuthn challenge state,
browser offers, and dashboard grants are memory-only. The service exposes a
minimal account/daemon UI today: passkey registration/login, claim-phrase entry,
daemon list, daemon labels, open dashboard, revoke ownership, and audit events.
The visible account identity is the globally unique account name/handle; the
internal WebAuthn display-name field is derived from that handle and is not a
separate user-facing profile field in the MVP UI.

Inside the hosted dashboard, Settings -> Debug includes a **Connect Health**
panel. It summarizes the active dashboard-control transport, daemon binding,
ICE route, event stream, byte-stream support, terminal-frame support, and other
advertised tunnel capabilities. Its self-test button runs the same safe
browser-side probes used by the hosted E2E harness: no legacy HTTP/WebSocket
fallback for Connect-only mutations, Shell input ordering, terminal-output
dedupe behavior, display-control routing, and tunneled presence callbacks. It is
not a file-transfer integrity test; the Files tab owns the user-facing ranged
download flow, and the hosted validator still uses a known fixture path for
byte-accurate transfer checks.

Production-alpha hardening now includes:

- cookie-backed user mutations require same-origin requests and a per-session
  CSRF header;
- auth, claim, daemon, and browser signaling hot paths have simple in-memory
  rate limits keyed by reverse-proxy client headers;
- `/healthz` is a cheap liveness probe and `/readyz` verifies that the static
  dashboard bundle and state directory are usable;
- security-relevant service events are emitted as structured JSON on stderr in
  addition to the persisted user audit log;
- revoking a daemon removes ownership, blocks future grants, and enqueues close
  events for active dashboard-control sessions known to the service.

The reverse proxy in front of `intendant-connect` must terminate public TLS for
`connect.intendant.dev`, forward `Host`, set `X-Forwarded-For`/`X-Real-IP`, and
strip any inbound copies of those client-IP headers before setting them. Keep
the service bound to `127.0.0.1`, keep `INTENDANT_CONNECT_TOKEN` in a secret
store, and back up the configured state file; that file is the current
account/passkey/device ownership database.

The production-alpha operator path is captured in scripts, but live target
details are not stored in the public repository. Provide them through a private
env file or command-line flags:

```bash
cat > ~/.config/intendant/connect-prod-alpha.env <<'EOF'
CONNECT_HOST=<ssh-host>
CONNECT_SSH_USER=<ssh-user>
CONNECT_SSH_KEY=<private-ssh-key-path>
CONNECT_REMOTE_SOURCE=<remote-source-directory>
CONNECT_SERVICE=<systemd-service-name>
CONNECT_REMOTE_READYZ_URL=<local-readiness-url>
CONNECT_REMOTE_STATE=<remote-state-json-path>
CONNECT_PUBLIC_ORIGIN=https://connect.intendant.dev
EOF

CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/deploy-connect-prod-alpha.sh
CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/connect-state-backup.sh --passphrase-file ~/.config/intendant/connect-backup.passphrase
CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/connect-state-restore.sh --yes \
  --passphrase-file ~/.config/intendant/connect-backup.passphrase \
  ~/.local/share/intendant/connect-backups/intendant-connect-state-YYYYMMDDTHHMMSSZ.json.enc
```

The deploy script syncs the current worktree to the configured remote source
directory, builds on the host, restarts the configured systemd service, and
checks both the configured local readiness URL and the public
`connect.intendant.dev` readiness URL. Backup and restore default to encrypted
state snapshots and require an explicit plaintext flag for diagnostics.

Current alpha limits:

- one owner per daemon; no shared roles, teams, recovery, or account email flow;
- one bearer token protects daemon service endpoints;
- rate limits, sessions, pending offers, plain claim phrases, and active-session
  tracking are single-process in-memory state;
- no high-availability storage or database migrations; the state file is a
  single-node alpha persistence layer;
- no application-layer dashboard RPC relay; the default path is browser to
  daemon WebRTC, with TURN/WebRTC relay remaining a transport-level option;
- Files transfer history and resumed download offsets are browser-local state
  (`localStorage` plus IndexedDB parts), not server-side durable state shared
  across browsers; uploads are current-session staged attachments rather than
  arbitrary daemon filesystem writes;
- peer daemon-to-daemon mTLS remains separate from Connect account login.

Run the hosted MVP E2E locally with:

```bash
cargo build --bin intendant-connect --bin intendant
node scripts/validate-connect-hosted-mvp.cjs
```

That validator starts `intendant-connect`, launches a daemon with outbound
Connect enabled, uses a browser virtual authenticator for passkey registration,
claims the daemon, labels it, opens the real SPA in `connect=1` mode, verifies
the daemon-signed binding and Connect grant hash, exercises the Shell sub-tab
over tunneled terminal frames, verifies that a `--no-tui` daemon renders an
explicit TUI-unavailable state, and runs the SPA's no-legacy-transport probes
for control actions, media/editor upload, visual-freshness diagnostics, display
signaling, display input authority, peer mutation fallback, TUI input, presence
media, presence server callbacks, the Files tab's ranged download/resume flow,
the lower-level generic filesystem download probe, and staged upload raw range
reads. It then revokes the daemon while the tunnel is still open, waits for the
tunnel to close, and checks the audit events.

### Design Target: Public Bootstrap with a Direct WebRTC Dashboard Tunnel

The current dashboard access model is certificate-first: a remote browser
reaches the daemon over HTTPS/WSS, usually with mTLS. That keeps the
implementation simple and gives the browser a secure context, but it also means
private host names, changing VM addresses, and locally generated server
certificates leak into the user experience.

The product problem is specifically **browser server trust**. Passkeys can prove
that the user approved a login, but they do not make
`https://192.168.x.y:8765` or `https://daemon.local:8765` a browser-trusted
origin. Public Web PKI also cannot directly cover VM-local names, `.local`
names, or changing private IP addresses. Pointing public DNS at private IPs is
fragile because DNS rebinding defenses, cache lifetimes, and per-network address
choices all become visible to users.

A plausible future direction is a **public-trusted bootstrap with a direct data
path**:

1. The browser loads an Intendant-owned public HTTPS origin with an ordinary
   publicly trusted certificate.
2. That origin handles account, passkey, device, and daemon-claim UX.
3. The daemon maintains an outbound signaling connection to Intendant Connect.
4. The browser and daemon establish a daemon-scoped WebRTC DataChannel directly
   where possible.
5. TURN/WebRTC relay remains available when a direct path cannot form.
6. Dashboard RPC, the main event stream, and display signaling move over that
   encrypted browser-to-daemon DataChannel.

This avoids asking the browser to trust private LAN HTTPS names: public TLS
secures the bootstrap page, while WebRTC supplies a private encrypted transport
to the daemon. It also fits Intendant's existing shape better than trying to make
public Web PKI cover VM-local or LAN-only daemon addresses. The codebase already
has browser-offer WebRTC, data channels, ICE-TCP multiplexing, relay fallback
for display/federation paths, and now an opt-in daemon-scoped dashboard control
tunnel with a browser-side `DashboardTransport` boundary.

#### Trust Model

WebRTC encryption is not the same thing as daemon identity. The DataChannel is
encrypted with DTLS, but the browser learns the DTLS fingerprint through
signaling. Therefore the signaling path must be authenticated and bound to the
daemon the user intended to reach.

The minimal trust model is:

- **Intendant Connect** is trusted for public HTTPS, account/passkey login,
  daemon claiming, dashboard JavaScript delivery, and WebRTC signaling.
- **The daemon** has a persistent daemon identity key, separate from both the
  ephemeral WebRTC DTLS certificate and peer mTLS client certificates.
- **The browser** accepts a dashboard tunnel only when it receives a fresh
  daemon-signed session statement bound to the claimed daemon identity, the
  current user/device, the Connect-issued grant, the WebRTC session material, an
  expiry, and a nonce.
- **The daemon** accepts dashboard control only after verifying a fresh
  Connect-issued session grant and applying local policy before exposing
  control-plane APIs over the DataChannel.

The current experimental tunnel implements the daemon-signed binding locally: the
browser sends a fresh challenge nonce with its SDP offer, and the daemon signs
the SDP offer hash, SDP answer hash, WebRTC control session id, creation time,
expiry time, daemon Ed25519 public key, that browser challenge nonce, and, when
rendezvous signaling supplies one, a Connect session-grant hash. The browser
verifies the signature with WebCrypto, rejects expired bindings, checks that the
signed nonce matches its own challenge, and checks that the visible grant hashes
to the signed grant hash before using the channel. A public bootstrap service
should keep that daemon identity binding and add account/device grants around it.

The local Connect-rendezvous emulator now also models the registry side of that
identity check: the daemon registers its public key for a daemon id, the browser
offer answer carries that registered key, and the public-origin dashboard accepts
the DataChannel only when the signed binding key matches the registered key. It
also models grant binding with an opaque per-offer value and nonce binding with a
browser-generated challenge: the browser accepts the answer only when that
visible grant hashes to the daemon-signed grant hash and the signed nonce matches
the nonce it put in the offer. This does not solve account ownership,
authorization-grant issuance, revocation, or clone recovery; it prevents the
browser from treating an arbitrary valid daemon signature, stale binding,
mismatched grant, or mismatched browser challenge as the claimed session.

This makes the security boundary explicit: Intendant Connect is in the trusted
computing base for consumer dashboard access. A compromised Connect service or
compromised served JavaScript can mislead the browser unless a later design adds
out-of-band daemon key verification or an independently pinned web app. That is
an acceptable product tradeoff only if it is documented as the consumer
cloud-assisted mode, not as a local/offline replacement for mTLS.

#### Claim and Login Flow

A concrete flow should look like this:

1. The daemon generates or loads a persistent daemon identity key.
2. The daemon opens an outbound TLS connection to Intendant Connect and publishes
   a short-lived high-entropy claim phrase or QR URL.
3. The user opens the public Intendant Connect URL and signs in with a passkey.
4. The user claims the daemon by entering the phrase or scanning the QR code.
5. Connect records the daemon identity public key, owner account, device label,
   and any local policy metadata the daemon chooses to expose.
6. On later visits, the browser signs in with a passkey and selects the daemon.
7. Connect issues a short-lived dashboard session grant to the daemon and
   brokers WebRTC signaling between browser and daemon.
8. The daemon signs the WebRTC session binding with its daemon identity key
   before it accepts dashboard RPC over the DataChannel.

Passkey step-up can then protect high-impact actions such as approving a peer
access request, changing autonomy policy, exposing display control, or minting
long-lived credentials.

#### Direct Path and Relay Fallback

There are two different fallback concepts, and they should not be conflated:

- **TURN/WebRTC relay fallback** keeps the browser-to-daemon DataChannel
  encrypted end-to-end at the WebRTC layer. The relay forwards packets but does
  not see dashboard RPC plaintext.
- **Application RPC relay fallback** would terminate or proxy dashboard messages
  at Intendant Connect unless an additional application-layer encryption scheme
  is added. That is a materially different trust posture and should be an
  explicit product mode, not the default fallback implied by "relay."

The preferred consumer path is direct WebRTC first, TURN/WebRTC relay second,
and no plaintext dashboard RPC through the public service by default. If an
operator deliberately enables an application relay for locked-down networks, the
UI should label it as "proxied through Intendant Connect" rather than "direct."

#### Dashboard Transport Contract

This is not a drop-in replacement for mTLS today. The dashboard mostly still
assumes ordinary HTTP endpoints plus a main WebSocket, while existing display
WebRTC sessions remain display-scoped. The current `DashboardTransport` boundary
is the first browser-side split: when the opt-in DataChannel is connected,
selected JSON reads and conservative mutations can use WebRTC and fall back to
HTTP where safe.

A production version still needs two explicit transport implementations:

- `HttpDashboardTransport`: current HTTPS/WSS REST plus main WebSocket.
- `WebRtcDashboardTransport`: request/response RPC, streaming events, and
  cancellation over a reliable ordered DataChannel.

The DataChannel protocol should stay explicitly framed rather than ad hoc JSON
messages. The first useful envelope set is:

| Frame | Direction | Purpose |
|-------|-----------|---------|
| `hello` / `hello_ack` | both | Version negotiation, daemon identity, session id, role, feature flags |
| `request` | browser -> daemon | HTTP-like method/body call with a request id |
| `response` | daemon -> browser | Status, metadata, body, or application error for a request id |
| `response_start` / `response_chunk` / `response_end` | daemon -> browser | Chunked delivery of an oversized JSON `response` frame |
| `stream_start` / `stream_event` / `stream_end` | daemon -> browser | Ordered event stream for a long-lived request id |
| `byte_stream_start` / `byte_stream_chunk` / `byte_stream_end` | daemon -> browser | Bounded raw-byte artifact transfer for a request id |
| `upload_start` / `upload_chunk` / `upload_end` | browser -> daemon | Bounded raw-byte upload transfer for a request id |
| `terminal_open` / `terminal_input` / `terminal_resize` / `terminal_close` | browser -> daemon | Standalone Shell PTY control for one terminal id |
| `terminal_output` / `terminal_exited` / `terminal_opened` / `terminal_error` | daemon -> browser | Standalone Shell PTY data and lifecycle frames |
| `tui_subscribe` / `tui_key` / `tui_resize` / `tui_unsubscribe` / `tui_close` | browser -> daemon | Server-side WebTui connection control when a WebTui renderer is available |
| `tui_term` / `tui_error` | daemon -> browser | WebTui terminal bytes and errors for one dashboard-control connection id |
| `event` | daemon -> browser | Control-plane event stream entry |
| `cancel` | browser -> daemon | Cancel an in-flight request or stream |
| `credit` | browser -> daemon | Backpressure for chunked responses, chunked stream events, or bounded byte streams |
| `ping` / `pong` | both | Liveness, latency, and reconnect diagnostics |

Oversized DataChannel `response` and `stream_event` frames are split at the
transport layer. The daemon sends a `response_start` header, base64-encoded
`response_chunk` frames containing the original JSON frame bytes, and a
`response_end` marker. The browser reassembles and parses the original frame
before handing it to existing request or stream code, so API semantics stay
unchanged. Current browser clients advertise `response_credit`, `byte_streams`,
`upload_frames`, `terminal_frames`, and `tui_frames` in `hello`; when
`response_credit` is negotiated, the daemon sends an initial chunk window and
then waits for browser `credit` frames before releasing more chunks. Stream
chunks carry a `chunk_id` so a large event inside a longer stream can be
credited and cancelled without ending the whole request id. Older clients that
do not advertise the feature still receive the legacy eager chunk burst.

Bounded artifact downloads use `byte_stream_start`, base64 `byte_stream_chunk`
frames, and `byte_stream_end`. This avoids wrapping raw bytes inside a JSON
result and reuses the same credit-window queue. Individual byte streams remain
bounded, but browser helpers can build resumable user-facing downloads by
issuing repeated ranged requests and resuming from the last completed offset.

Bounded dashboard uploads use `upload_start`, base64 `upload_chunk`, and
`upload_end`. The daemon writes chunks into a tempfile and commits through the
same upload store as `POST /api/session/current/uploads`, including the
`UploadReady` broadcast. The Files tab uses this same primitive for browser
file uploads, so uploaded files become current-session staged attachments. This
is still a one-shot, ordered transfer with no resume token, destination
filesystem path, or cross-refresh queue. Resume tokens, explicit destination
policy, and application-level restart semantics are still required before
treating uploads as broad resumable file transfer.

Dashboard media/editor writes intentionally stay outside the generic
`api_dashboard_action_msg` and `api_control_msg` allowlists. They use the
dedicated media protocol instead: annotation attach/save/send commits use
media-specific upload methods, and clip creation uses an operation id with
ordered frame uploads plus commit/cancel. Older daemons that do not advertise the
media protocol still receive the legacy `annotation_*` and `clip_*` WebSocket
messages before any tunneled write is attempted.

The standalone **Terminal -> Shell** subtab uses `terminal_*` frames when the
verified tunnel advertises `terminal_frames`. The daemon attaches the tunnel to
the same PTY registry used by the WebSocket path, so scrollback and reconnect
behavior stay consistent. The server-side ratatui **TUI** subtab uses `tui_*`
frames when the verified tunnel advertises `tui_frames`. The daemon creates a
dashboard-control-owned WebTui connection, forwards WebTui's existing
`{"t":"term","d":"..."}` output as `tui_term`, and removes the connection on
`tui_close` or DataChannel cleanup. Idle daemon launches intentionally do not run
WebTui, so they report `tui_frames_available: false` and the browser leaves that
subtab on the existing WebSocket/fallback behavior only on daemon-origin pages.
Public-origin Connect pages have no daemon WebSocket fallback, so TUI key,
resize, and subscription attempts are dropped until `tui_frames` is advertised.

The first streamed API on this substrate is `api_sessions_stream`, which mirrors
the existing `/api/sessions/stream` NDJSON event shape (`start`, partial
`session`, `phase`, final `replace`, `done`). When the verified DataChannel is
connected, local dashboard session hydration uses that stream and falls back to
the HTTP stream on safe errors. Peer session lists still use direct peer HTTP.
The local daemon identity is available as `api_agent_card`, returning the same
Agent Card shape served by `/.well-known/agent-card.json`; the HTTP endpoint
remains the unauthenticated discovery surface.
When the verified channel opens, the browser also applies `config` and
`api_agent_card` results to the same runtime-config and self-identity state
normally hydrated by `/config` and `/.well-known/agent-card.json`.
`api_cached_bootstrap_events` returns the daemon's current non-personalized
dashboard event cache (`usage`/`usage_update`, `live_usage_update`, `status`,
`autonomy_changed`, `external_agent_changed`, and `user_display_granted` when
present) as parsed JSON events. This cached-event RPC is intentionally narrower
than the WebSocket open sequence, but the tunnel exposes the personalized
bootstrap pieces as separate identity-aware APIs below. `api_dashboard_bootstrap`
composes those pieces so a public-origin dashboard can hydrate without the
primary daemon WebSocket.
`api_browser_workspace_snapshot` returns the existing
`browser_workspace_snapshot` message shape with active browser workspaces and
lease state; callers can feed it through the same browser workspace handler that
currently receives the WebSocket bootstrap message.
`api_state_snapshot` returns the existing `state_snapshot` message shape with
the current `AgentStateSnapshot`, dashboard config, daemon session id when known,
and a DataChannel-scoped `connection_id`. The connection id is the WebRTC
control session id, not the legacy WebSocket connection id.
`api_display_bootstrap` returns a DataChannel-safe display bootstrap envelope
whose `frames` array contains `display_ready` events for every active display
session known to the daemon. Those frames use the same event shape as the
WebSocket bootstrap (`event`, `display_id`, `width`, `height`), so browser code
can feed them through the existing display-slot path. When the daemon exposes a
dashboard-control display authority bridge, the envelope also includes
personalized `display_input_authority_state` frames for the same active display
ids; otherwise `display_input_authority_state` is listed in `omitted`.
`api_display_input_authority_snapshot` returns just those personalized authority
frames, while `api_display_input_authority_request` and
`api_display_input_authority_release` claim or release the display for the
current dashboard-control session and return fresh state frames for immediate UI
application. The browser then sends local keyboard/mouse events as
fire-and-forget `display_input` frames over the same daemon-scoped DataChannel.
If the tunnel or authority bridge is unavailable, the dashboard falls back to
the older WebSocket plus per-display input-channel path.
Local display WebRTC signaling uses `api_display_webrtc_signal` when the
verified tunnel advertises it. The browser sends the same `display_id`, offer
SDP, and ICE candidate shapes that the legacy `display_offer`/`display_ice`
WebSocket frames used; the offer RPC returns a `display_answer`, while daemon
ICE candidates arrive later as `display_ice` event payloads over the control
DataChannel. Daemon-origin dashboards may still fall back to the WebSocket when
the tunnel is unavailable. Public-origin Connect mode does not attempt a daemon
WebSocket fallback for local display signaling, so missing tunnel support fails
the display slot visibly.
`api_session_log_replay` returns the existing capped `log_replay` message shape
used by late WebSocket joiners. When no active session log exists it returns an
empty replay with `available: false`.
`api_external_session_activity_replay` returns an envelope whose `frames` array
contains compact external attached-session activity replay frames for currently
attached Codex/Claude/Gemini sessions. It uses the same transcript payloads as
WebSocket bootstrap. The combined bootstrap skips an attached external session
when the active Intendant session log replay already names the same
`external_session_id`, avoiding duplicate transcript hydration.
`api_dashboard_bootstrap` composes the DataChannel-safe bootstrap pieces into an
ordered `frames` array: state snapshot, cached dashboard events, browser
workspace snapshot, active display `display_ready` frames, and capped session
log replay, followed by active external attached-session activity replay frames.
When the display authority bridge is available, it appends personalized
`display_input_authority_state` frames as well so a refreshed public-origin
dashboard can hydrate display control chips without the primary WebSocket.
Lazy command-output expansion for finalized log command groups uses
`api_session_current_agent_output`, preserving the same `_httpStatus`/`_httpOk`
metadata as the existing HTTP endpoint.
The active-session timeline uses `api_session_current_history`,
`api_session_current_rollback`, `api_session_current_redo`, and
`api_session_current_prune`; the mutation calls use the same no-replay fallback
rule as other writes.
Active-session change list/detail reads use `api_session_current_changes`,
preserving the existing path validation and `_httpStatus`/`_httpOk` metadata.
Live and per-session recording stream lists use `api_recordings` and
`api_session_recordings`. Scoped recording asset reads use `api_recording_asset`
and `api_session_recording_asset` for `segments`, `playlist.m3u8`, and validated
`seg_*.mp4`/`seg_*.ts` filenames with optional `offset`/`length` ranges. The
recording player uses these byte streams for segment lists and MP4 MSE buffers
when the verified tunnel is available. The non-MSE MP4 fallback also reads the
segment over the tunnel and assigns a local blob URL to the video element.
HLS/`.ts` playback also prefers the tunnel when available: the browser reads
`playlist.m3u8` and validated `.ts` segments with the same recording asset RPC,
rewrites the playlist to local blob URLs, and points the native video element at
that object URL. If the browser rejects the blob playlist, it falls back to the
daemon-served `m3u8` URL only on a daemon-origin dashboard page; public-origin
Connect mode does not attempt same-origin HTTP fallback for self-daemon media.
Archived session frame images use `api_session_frame_asset` for validated `.jpg`
and `.png` filenames under a resolved session's `frames/` directory. The session
detail gallery renders returned bytes through browser blob URLs when the verified
tunnel advertises byte streams, falling back to the existing HTTP image URL when
the tunnel is unavailable or a tunneled image read fails.
The Settings debug session-report download uses `api_session_report`, returning
the same text-artifact zip as `/api/session/{id}/report` through bounded
`byte_stream_*` frames. This remains intentionally scoped to the diagnostic
report; generic daemon file downloads use the Files tab and `api_fs_read`.
The task attachment upload path uses `api_session_current_upload` over
`upload_*` frames when the verified tunnel advertises `upload_frames`; it falls
back to `POST /api/session/current/uploads` only when the tunnel feature is not
available. Failed tunneled uploads are not replayed over HTTP, to avoid creating
duplicate attachments after an ambiguous partial transfer.
Dashboard annotation media uses the same ordered `upload_*` frame substrate but
commits to media-specific methods instead of the task attachment store:
`api_media_annotation_attach` registers a pending annotation frame, and
`api_media_annotation_submit` registers a saved annotation and optionally queues
it for the live presence context. Clip creation is stateful: the browser first
opens a `clip_id` operation with `api_media_clip_start`, uploads each JPEG frame
with `api_media_clip_frame` in strict `frame_index` order, then commits with
`api_media_clip_end` or discards with `api_media_clip_cancel`. The dashboard
chooses the transport once per media operation. If the media protocol is not
advertised before the first write, daemon-origin dashboards use the legacy
WebSocket media messages. Public-origin Connect mode has no daemon WebSocket
fallback, so annotation and clip writes fail visibly when the verified media
tunnel is not available. After a tunneled media write is attempted, failures are
surfaced and are not replayed over the WebSocket.
Browser-side live voice keeps its provider WebSocket in the browser, but the
daemon coordination side uses the Connect control tunnel. The WASM presence
bridge can install a custom sender so its normal server messages route over the
verified DataChannel instead of `/ws`: `presence_frame` carries
`presence_connect`, `presence_disconnect`, `make_active`, `voice_log`,
`presence_checkpoint`, `voice_diagnostic`, `live_usage_update`, `tool_request`,
and `async_query`. Server responses such as `presence_welcome`,
`force_disconnect_voice`, `active_granted`, `tool_response`, and
`async_query_result` are delivered back to the same WASM callback router that
the WebSocket path uses. HQ browser video/archive frames use
`api_presence_video_frame` over the ordered `upload_*` substrate. Public-origin
Connect pages therefore do not depend on the daemon-origin WebSocket for active
voice handoff, voice event logging, live voice tool/query dispatch, or frame
archival. Server-side transcription audio remains intentionally untunneled for
now; Connect mode drops that optional audio stream rather than replaying it over
the legacy bridge.
Current-upload list reads use `api_session_current_uploads`, returning the same
staged-upload descriptor array as `GET /api/session/current/uploads`. The Files
tab shows this as its staged-upload list and can remove entries with
`api_session_current_upload_delete`.
Current-upload raw reads use `api_session_current_upload_raw` over
`byte_stream_*` frames. The request names an uploaded attachment id and may
include `offset`/`length`; the response carries `range_start`, `range_end`,
`total_size`, and `resumable: true` metadata with the returned bytes. The Files
tab uses repeated ranged reads to download staged uploads back to the browser.
This is a bounded current-session attachment primitive, not yet a general
daemon-filesystem upload/download adapter.
Worktree cached inventory reads, explicit scans, and guarded removals use
`api_worktrees`, `api_worktrees_scan`, and `api_worktrees_remove`; removal uses
the same no-replay fallback rule as other writes.
The filesystem picker's path checks, directory listings, and mkdir operation use
`api_fs_stat`, `api_fs_list`, and `api_fs_mkdir`; mkdir uses the same no-replay
fallback rule as other writes.
Bounded filesystem file reads use `api_fs_read` when the verified tunnel
advertises byte streams. The request uses the same absolute-path or `~/` path
rules as the picker, rejects directories, accepts optional `offset`/`length`,
and returns bytes plus `content_type`, `range_start`, `range_end`, `total_size`,
and `resumable: true` metadata. The Files tab exposes this as the download side
of its transfer center: users can type a path or browse with the filesystem
picker, queue downloads, pause/cancel/retry, and resume from completed ranges
inside the current browser session. Public-origin Connect mode does not fall
back to daemon HTTP for this path. The queue/history and partially completed
ranges are browser-local state, not daemon-side transfer records.

Daemon-origin dashboards reached directly over native mTLS use the same Files
transfer center but read arbitrary files through `GET /api/fs/read?path=...`
with ordinary HTTP `Range` requests. The endpoint follows the same path rules,
rejects directories, advertises `Accept-Ranges: bytes`, returns `206 Partial
Content` plus `Content-Range` for ranged reads, and returns `416 Range Not
Satisfiable` with `Content-Range: bytes */total` for invalid ranges. This keeps
direct mTLS downloads resumable without routing them through the Connect
DataChannel. Connect dashboards intentionally keep using `api_fs_read` over the
verified tunnel and never fall back to daemon-origin HTTP.
Lazy exact context-snapshot loads use `api_session_context_snapshot`, keeping
large raw request payloads out of ordinary session-detail hydration while still
allowing the Context pane to fetch a single archived snapshot on demand.
Staged upload deletion uses `api_session_current_upload_delete` so removing a
pending attachment can travel over the verified control channel. Browser image
chips now prefer `api_session_current_upload_raw` and render the returned bytes
through a local blob URL, falling back to the legacy raw HTTP URL only when the
tunnel is unavailable or preview loading fails.
OpenAI browser live-audio token minting uses `api_voice_session`; it preserves
the existing `/session` behavior and error envelope while avoiding a direct
dashboard HTTPS POST when the verified control channel is available.
Dashboard-originated managed-context MCP actions use `api_mcp_tool_call`, which
wraps a single `tools/call` against the daemon's existing MCP server. These
calls use the same no-replay fallback rule as other writes because tools such
as `rewind_context`, `fission_control`, and `fission_spawn` mutate live session
state.
Confirmed session-data deletion uses `api_session_delete` with the same
no-replay fallback rule as other writes; the dashboard still requires the
existing confirmation modal before issuing the RPC.
Peer-display WebRTC signaling uses `api_peer_webrtc_signal`, carrying the same
`display_id`, `session_id`, and `signal` body as `POST /api/peers/{id}/webrtc`
plus the target `peer_id`. Answers and remote ICE still arrive asynchronously
through the normal peer-event path; the RPC only confirms that the signal was
accepted for forwarding. Failed tunneled signaling requests are not replayed
over HTTP after a verified tunnel attempt.
Dashboard session-control actions use `api_session_control_msg`. This includes
create/start/resume/stop/restart session, targeted follow-up, mid-turn steer,
cancel queued steer/follow-up, edit user message, interrupt, approvals,
session rename, and per-session launch-config persistence. The browser only
falls back to the WebSocket before it has attempted the RPC; once a verified
DataChannel write is sent, an error is surfaced to the operator instead of
replaying a potentially duplicated action.
Small dashboard action controls use `api_dashboard_action_msg`. This covers
Codex/Gemini attached-thread actions, local display authority toggles, the
diagnostics visual-marker toggle, recording and debug screen controls, and
browser workspace create/acquire/close/release. The browser applies the same
no-replay fallback rule: use the WebSocket only before a verified DataChannel
request is attempted, then surface RPC failures instead of duplicating a
potentially state-changing action. For `set_diagnostics_visual_marker`, the
daemon applies the request directly to the active display registry when
available, or records the desired state as a pending per-display default for
the next display session.

The remaining migration work is mostly byte-stream and file-transfer heavy:
native media fallback URLs, broader bidirectional file transfer, durable
cross-refresh resume tokens, and any remaining non-allowlisted control
mutations should move only after resumable stream/file-transfer semantics and
per-action no-replay rules are settled.

The dashboard status bar now exposes the selected control transport. Direct
dashboard access shows the existing HTTP/mTLS path, while opt-in WebRTC control
shows `checking`, verified `WebRTC`, `relay` when browser ICE stats report a
TURN-relayed candidate pair, or `failed` when signaling or daemon-binding
verification fails. The tooltip carries the detailed state that is also exposed
through `window.intendantDashboardControl.status()`. In public-origin Connect
mode, the legacy `ws` indicator is relabeled to `events`; it turns green only
after the verified DataChannel has hydrated dashboard bootstrap events, since no
same-origin daemon WebSocket is expected in that mode.

Peer access-request APIs now use the same transport boundary. The dashboard's
pairing/request panes call `api_peer_pairing_requests`,
`api_peer_pairing_request_decision`, invite/join/request-access/poll, identity
list, and identity revoke over the DataChannel when it is connected. Mutating
pairing operations deliberately fail rather than silently falling back after a
WebRTC RPC error, so an operator does not approve or mint credentials over an
unexpected transport.
General peer and coordinator controls are covered by the same rule. Peer add,
remove, eligibility discovery, per-peer message/task/approval, peer-display
signaling, and coordinator route calls use `api_peer_add`, `api_peer_remove`,
`api_peer_eligible`, `api_peer_message`, `api_peer_task`, `api_peer_approval`,
`api_peer_webrtc_signal`, and `api_coordinator_route` over the verified tunnel.
They preserve the existing HTTP endpoint metadata (`_httpStatus`/`_httpOk`) so
the dashboard can render the same success and error states on either transport,
but state-changing calls do not replay over HTTP once a verified DataChannel
request has been attempted.

#### Relationship to Existing Auth Modes

This design should not remove local/offline mTLS. It gives the product two clear
dashboard access modes:

- **Consumer cloud-assisted mode:** public Intendant Connect origin, passkey
  login, daemon-scoped WebRTC dashboard tunnel.
- **Local/offline/power-user mode:** direct daemon HTTPS/WSS with browser mTLS
  enrollment, as implemented today.

Peer daemon-to-daemon trust remains separate. Humans may use passkeys to approve
a peer access request, but the resulting daemon-to-daemon connection should
still use Intendant-issued peer-scoped mTLS certificates unless the federation
trust model is deliberately redesigned. In user-facing copy, that should appear
as "grant access to this daemon" and "revoke access," not as manual certificate
management.

In other words, Connect and browser mTLS authenticate a **user/client route to a
daemon**. Peer mTLS authenticates a **daemon route to another daemon**. The
dashboard can present both as targets, but target selection is only a product
abstraction; it does not collapse the two security domains.

#### Status and Remaining Rollout

The current implementation has crossed from protocol sketch into hosted MVP:

1. Direct mTLS dashboard access remains the default local/offline path.
2. The daemon has a persistent daemon identity key and can expose a
   dashboard-control WebRTC DataChannel.
3. The real SPA has a `connect=1` public-origin mode and a
   `DashboardTransport` boundary for tunneled reads, streams, bounded byte
   transfer, uploads, terminal/WebTui frames, selected control messages, peer
   pairing actions, local display signaling, and media/editor writes.
4. The daemon has a disabled-by-default outbound Connect polling client.
5. `intendant-connect` provides the hosted production alpha: passkey-only
   account sessions, daemon registration, claim-phrase ownership proof,
   short-lived dashboard grants, signaling, labels, revoke, active tunnel close,
   rate limits, CSRF protection, readiness checks, and audit.
6. The browser and hosted service both verify that the daemon-signed WebRTC
   binding matches the registered daemon identity and Connect-issued grant.
7. Focused validators cover the local bootstrap, local rendezvous emulator, and
   hosted Connect MVP paths.
8. `connect.intendant.dev` has a repeatable production-alpha deploy path plus
   encrypted state backup/restore scripts.

The remaining rollout work is production operations and breadth, not proving the
core browser-trust escape hatch:

1. Add durable/database-backed rate limits, structured metrics, and database
   migrations.
2. Add account recovery, richer multi-device management, teams/roles, and optional
   passkey step-up for sensitive actions.
3. Add daemon identity rotation/recovery semantics for VM clones, disk restore,
   and deliberate transfer of ownership.
4. Continue migrating remaining dashboard APIs only when the tunnel has the
   required streaming, byte-range, resumable transfer, or media semantics.
5. Keep direct mTLS dashboard access and peer daemon-to-daemon mTLS working
   throughout.

Non-goals for this path:

- no loopback or same-host bypass of dashboard authentication;
- no native host app requirement for the general web dashboard;
- no passkeys as daemon-to-daemon federation credentials;
- no silent downgrade from verified direct WebRTC to opaque relay;
- no attempt to obtain public certificates for private VM IPs or `.local`
  names.

Remaining design questions before production rollout:

- Do we need an additional app-integrity story such as signed static assets or a
  pinned web bundle, even though the hosted MVP serves the dashboard JavaScript
  from Intendant Connect?
- How are daemon identity keys backed up, rotated, revoked, and recovered after
  VM cloning or disk restore?
- What local policy does the daemon enforce when Connect says a signed-in user
  wants access?
- Do browser WebRTC privacy policies, enterprise restrictions, or future Private
  Network Access rules constrain direct DataChannels from a public origin to
  LAN/VM candidates?
- What is the visible product distinction between "direct," "TURN-relayed," and
  "application-proxied" dashboard transport?
- What audit log should exist for passkey logins, daemon claims, step-up
  approvals, and peer certificate issuance?

## HTTP endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /` | The dashboard SPA |
| `GET /config` | Live-model configuration JSON (provider, model, sample rates, git SHA) |
| `GET /debug` | Debug JSON (agent state, voice connection, active browser) |
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime |
| `GET /wasm-web/*` | Compiled WASM + JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for mic capture |
| `GET /api/sessions` | List past sessions |
| `GET /api/session/{id}` | Session detail |
| `GET /api/session/{id}/recordings/*` | Recording segments for a past session |
| `GET /recordings/*` | Current-session recording segments |
| `WS /` or `WS /ws` | Main WebSocket: events, TUI terminal and fallback Shell terminal I/O, presence protocol, WebRTC signaling |

The full WebSocket message protocol (inbound key/resize/presence/WebRTC frames,
outbound term/state/log-replay/tool-response frames) and the gateway's internal
layering are documented in [Integrations → Web Gateway](./integrations.md#web-gateway).
