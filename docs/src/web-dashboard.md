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

The top tab bar has eight tabs: **Activity**, **Stats**, **Terminal**,
**Video**, **Station**, **Sessions**, **Debug**, **Settings**. New events
arriving while you are on another tab raise a notification badge.

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
whether rewind-only gating is active. Historical sessions show `historical`
status — records and anchors stay readable, but live actions are disabled.
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
existing dashboard WebSocket.

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

The configuration panel for the current session. **Network → Daemons** is the
dashboard entry point for peer relationships:

- **Grant Invite / Join Invite** handles the direct secret handoff flow.
- **Request Access** lets this daemon ask another daemon for a peer-scoped mTLS
  identity without receiving a private-key-bearing invite.
- **Inbound Access Requests** shows doorbell requests from other daemons and
  lets the local operator approve or deny them.
- **Inbound Access Grants** lists client identities this daemon will accept
  from other daemons, including the granted role/profile.

The panel keeps the manual URL add path for already-enrolled peers, tunnels, and
local/debug daemons. Manual additions are runtime-only unless **Save to
intendant.toml** is checked.

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
origin and sends the WebRTC offer over the existing WebSocket. The point is to
prove the future "public HTTPS bootstrap + direct local daemon data path" shape
without weakening the current mTLS default.

The handshake is bound to the daemon identity:

- the browser creates a `intendant-dashboard-control` DataChannel and sends an
  SDP offer over `/ws`;
- the daemon answers with SDP plus a signed binding over the offer hash, answer
  hash, session id, timestamp, and daemon Ed25519 public key;
- the browser verifies that binding with WebCrypto before using the channel.

When enabled with
`localStorage.intendant_dashboard_transport = "webrtc-control"` (or
`window.intendantDashboardControl.enable()`), dashboard JSON reads prefer the
DataChannel and fall back to HTTP through the browser-side `DashboardTransport`
boundary. Current tunneled reads include sessions, session detail, deep session
search, settings, API-key status, project root, display enumeration, and peer
state. Managed-context history reads for records, anchors, and fission groups
also use the tunnel. Current tunneled mutations include settings save, API-key
save, peer add/remove, peer access-request pairing, peer message/task/approval
actions, eligible-peer lookup, and coordinator routing.
Mutation fallbacks are deliberately conservative: if a connected WebRTC RPC
fails after it may have reached the daemon, the dashboard surfaces the error
instead of repeating the write over HTTP.

The tunnel mirrors HTTP JSON response semantics. Application errors travel as
successful transport frames with `_httpStatus`/`_httpOk` metadata so existing UI
code can render the same error message it would render for an HTTP response.
Transport failures, unknown RPC methods, and aborted requests still reject the
browser-side promise.

Several paths intentionally stay outside this JSON tunnel:

- static assets and WASM bundles;
- frames, recordings, and file uploads;
- MCP-over-HTTP;
- diagnostics NDJSON uploads;
- display WebRTC media/control channels and peer-display signaling;
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
| `GET /connect/bootstrap` | not required | Minimal HTML bootstrap page for WebRTC dashboard-control testing |
| `GET /connect/status` | not required | JSON health/capability probe for the bootstrap surface |
| `POST /connect/dashboard/offer` | not required | Browser SDP offer -> daemon SDP answer plus signed binding |
| `POST /connect/dashboard/ice` | not required | Browser trickle ICE candidate for a control session |
| `POST /connect/dashboard/close` | not required | Close a control session |

Those paths are deliberately allowlisted one by one. They do **not** make `/`,
`/config`, `/ws`, `/api/*`, assets, recordings, or the full dashboard available
without the normal dashboard authentication. The bootstrap page exposes
`window.intendantConnectDashboard` for tests and diagnostics; it verifies the
same daemon-signed binding as the full dashboard control experiment, then uses
the DataChannel RPC protocol directly.

Run the focused browser check against a local daemon with:

```bash
PLAYWRIGHT_NODE_PATH=/path/to/node_modules \
  node scripts/validate-connect-bootstrap.cjs --origin https://127.0.0.1:8766
```

The check intentionally uses no client certificate. It must see `/config`
rejected with `401`, then prove that `/connect/bootstrap` can create a verified
dashboard-control DataChannel and issue a few RPC requests over it.

This slice is a local stand-in for a future hosted Intendant Connect service. It
does not implement account login, passkeys, daemon claiming, or a durable daemon
registry. Its job is to make the signaling boundary concrete and testable before
moving the real SPA and hosted account UX onto that boundary.

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
PLAYWRIGHT_NODE_PATH=/path/to/node_modules \
  node scripts/validate-connect-rendezvous.cjs
```

That script starts a local rendezvous HTTP origin, launches a fresh daemon child
with Connect env vars, verifies that `https://127.0.0.1:<daemon-port>/config`
still rejects a certless request with `401`, then loads the browser from the
rendezvous origin and drives `status`, `config`, `api_sessions`, a chunked
large `api_sessions` response, and application error RPCs over the verified
DataChannel.

This still is not consumer Connect. It has no account, passkey, daemon claim,
grant issuance, revocation, audit log, or hosted public HTTPS. It is the
smallest complete signaling proof for the future hosted service boundary.

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
daemon signs the SDP offer hash, SDP answer hash, WebRTC control session id,
timestamp, and daemon Ed25519 public key, and the browser verifies the signature
with WebCrypto before using the channel. A public bootstrap service should keep
that daemon identity binding and add account/device grants around it.

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
   a short-lived claim code or QR URL.
3. The user opens the public Intendant Connect URL and signs in with a passkey.
4. The user claims the daemon by entering the code or scanning the QR code.
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
| `event` | daemon -> browser | Control-plane event stream entry |
| `cancel` | browser -> daemon | Cancel an in-flight request or stream |
| `credit` | both | Backpressure for large responses or long streams |
| `ping` / `pong` | both | Liveness, latency, and reconnect diagnostics |

Oversized DataChannel `response` frames are now split at the transport layer.
The daemon sends a `response_start` header, base64-encoded `response_chunk`
frames containing the original JSON response bytes, and a `response_end` marker.
The browser reassembles and parses the original `response` frame before handing
it to existing request/response code, so API semantics stay unchanged. This is
chunking only; adaptive credit-based flow control and resumable transfers are
still required before moving uploads, downloads, recordings, terminal streams,
or file transfer.

The first production APIs should be small and high value: `/config`, the main
event stream, peer access-request list/approve/deny, and a basic health
endpoint. Uploads, downloads, recordings, terminal streams, and file transfer
should move later after chunking, flow control, and resume semantics are settled.

The dashboard status bar now exposes the selected control transport. Direct
dashboard access shows the existing HTTP/mTLS path, while opt-in WebRTC control
shows `checking`, verified `WebRTC`, `relay` when browser ICE stats report a
TURN-relayed candidate pair, or `failed` when signaling or daemon-binding
verification fails. The tooltip carries the detailed state that is also exposed
through `window.intendantDashboardControl.status()`.

Peer access-request APIs now use the same transport boundary. The dashboard's
pairing/request panes call `api_peer_pairing_requests`,
`api_peer_pairing_request_decision`, invite/join/request-access/poll, identity
list, and identity revoke over the DataChannel when it is connected. Mutating
pairing operations deliberately fail rather than silently falling back after a
WebRTC RPC error, so an operator does not approve or mint credentials over an
unexpected transport.

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

#### Staged Rollout

Treat this as a staged target, not current behavior:

1. Keep the current mTLS dashboard as the default and keep WebRTC dashboard
   control opt-in.
2. Define daemon identity keys, claim codes, Connect account binding, and
   revocation semantics.
3. Host a public static dashboard shell with a placeholder sign-in/device model.
4. Add daemon outbound signaling to Intendant Connect. The local emulator now
   exercises this shape without hosted auth.
5. Add a signaling rendezvous API keyed by browser session and daemon id. The
   local emulator implements the minimal offer/answer/ICE/close subset.
6. Let a locally running daemon register/poll that rendezvous while it is online.
   The daemon now has a disabled-by-default outbound polling client.
7. Reuse the existing daemon binding and DataChannel RPC frame format. This is
   shared by the direct local bootstrap and rendezvous-emulator slices.
8. Add visible transport status: disconnected, mTLS HTTP fallback, WebRTC direct,
   WebRTC relayed, failed verification, or application-proxied. The dashboard
   now shows mTLS/HTTP, checking, verified WebRTC, TURN-relay, and failed states
   for the current control transport.
9. Carry peer access-request approve/deny over the DataChannel. The dashboard
   now routes peer access-request list/decision and related pairing APIs through
   the DataChannel transport boundary; hosted passkey step-up still belongs to
   the future public Connect UI.
10. Gradually migrate larger API surfaces. Managed-context history reads now use
    the tunnel, and oversized JSON responses now use chunked response framing.
    Uploads, downloads, recordings, terminals, and file transfer still wait for
    credit-based flow control and resume semantics.
11. Keep direct mTLS dashboard access and peer daemon-to-daemon mTLS working
    throughout.

Non-goals for this path:

- no loopback or same-host bypass of dashboard authentication;
- no native host app requirement for the general web dashboard;
- no passkeys as daemon-to-daemon federation credentials;
- no silent downgrade from verified direct WebRTC to opaque relay;
- no attempt to obtain public certificates for private VM IPs or `.local`
  names.

Open design questions before implementation:

- Is Intendant Connect allowed to serve all dashboard JavaScript, or do we want
  an additional app-integrity story such as signed static assets or a pinned web
  bundle?
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
| `WS /` or `WS /ws` | Main WebSocket: events, terminal I/O, presence protocol, WebRTC signaling |

The full WebSocket message protocol (inbound key/resize/presence/WebRTC frames,
outbound term/state/log-replay/tool-response frames) and the gateway's internal
layering are documented in [Integrations → Web Gateway](./integrations.md#web-gateway).
