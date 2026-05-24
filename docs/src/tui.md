# TUI & Autonomy

## TUI

`intendant` ships a ratatui-based terminal UI (`src/bin/caller/tui/`) that
launches when stdin and stdout are both terminals (and neither `--no-tui`,
`--mcp`, nor `--json` is set). It gives real-time monitoring and control of the
agent loop.

> **The TUI is a display-only client of the control plane.** It renders state
> and emits [`ControlMsg`](./integrations.md) values onto the `EventBus`; it does
> **not** mutate shared state directly. The centralized `control_plane.rs` is the
> single writer for autonomy level, external-agent backend, Codex/Gemini config,
> etc. Its module doc states this explicitly: *"Frontends remain display-only —
> they render state changes but never write to shared state."* Approval
> resolutions go through the shared `ApprovalRegistry`; everything else is a
> `ControlMsg`. The same TUI renderer is also streamed to the browser (see
> [Web TUI](#web-tui)).

### Layout

```
┌─────────────────────────────────────────────┐
│ StatusBar: provider │ model │ turn │ budget  │  1 line
├─────────────────────────────────────────────┤
│ ActionPanel: phase + spinner + key hints     │  2 lines
├─────────────────────────────────────────────┤
│                                               │
│ LogPanel: scrollable, color-coded, tabbed     │  fills remaining
│                                               │
├─────────────────────────────────────────────┤
│ Approval / AskHuman / FollowUp (conditional)  │  3-4 lines
└─────────────────────────────────────────────┘
```

### Panels & overlays

- **Status bar** — provider, model, turn, budget %, autonomy level.
- **Action panel** — the current `Phase` with a spinner: `Thinking`,
  `RunningAgent`, `Orchestrating`, `WaitingApproval`, `WaitingHuman`,
  `WaitingFollowUp`, `Idle`, `Done`, plus the transient `Interrupting` /
  `Interrupted` states.
- **Log panel** — scrollable chronological log, color-coded by `LogLevel`
  (`Info`, `Model`, `Agent`, `Error`, `Warn`, `SubAgent`, `Detail`, `Debug`),
  with turn-grouping you can expand/collapse and **log-source tabs** (see below).
- **Approval panel** — shown when an action needs approval (command preview +
  category, `y`/`s`/`a`/`n`).
- **AskHuman input** — shown when `askHuman` fires; a `tui-textarea` for the
  reply.
- **Follow-up input** — shown when a round completes and the agent awaits
  follow-up.
- **Help overlay** (`?`) and **Inspect overlay** (`i`) — these are *per-view*
  overlays (in `ViewState`), so they don't block the shared interactive mode.

### Modes and log tabs

Interactive state is split between a shared `AppMode` and a per-connection
`ViewState`:

- **`AppMode`** (shared) — `Normal`, `AskHuman`, `Approval`, `FollowUp`
  (`Help` and `Inspect` also exist but are driven per-view).
- **`LogTab`** (per-view) — `All`, `Agent`, `Presence`. The `Presence` tab shows
  presence-layer chatter (see [Presence Layer](./presence.md)); `Agent` hides
  presence lines; `All` shows everything.

### Key bindings

| Key | Action |
|-----|--------|
| `q` / `Ctrl-C` | Quit |
| `v` | Cycle verbosity (quiet → normal → verbose → debug) |
| `i` | Open the Inspect overlay for the focused entry |
| `?` | Help overlay |
| `+` / `=` / `-` | Cycle autonomy level up / up / down |
| `d` | Toggle user-session display access (grant/revoke `DisplayControl`) |
| `f` | Re-open the follow-up input when a round is waiting or the task is done |
| `Tab` | Cycle the log tab (All → Agent → Presence) |
| `1` / `2` / `3` | Select log tab All / Agent / Presence |
| `Enter` / `→` | Expand the focused turn (or submit in input modes) |
| `←` | Collapse the focused turn |
| `↑` / `↓` / `PgUp` / `PgDn` | Scroll the log |
| `Home` / `End` | Jump to top / bottom of the log |

Approval mode keys (when an approval is pending):

| Key | Action |
|-----|--------|
| `y` / `Enter` | Approve |
| `s` | Skip (continue with the next command) |
| `a` | Approve all (also raises autonomy to Full) |
| `n` | Deny (and stop) |

> **Correction vs. older docs:** `1`/`2`/`3` are **log-tab selectors** (All /
> Agent / Presence), *not* panel toggles. `i` opens Inspect, `Tab` cycles the log
> tab, and `f` re-opens the follow-up input — these were previously undocumented.

### Markdown rendering

Model responses are rendered with lightweight markdown highlighting in the log
panel (`tui/markdown.rs`): headers (`#`–`####`) in blue, bold in bright, italic
in lavender, inline code and fenced blocks in green, list bullets in yellow,
horizontal rules as dim lines.

### Streaming display

While a model generates, text deltas arrive as `AppEvent::ModelResponseDelta`
and accumulate in a streaming buffer for immediate feedback; the buffer is
cleared when the full response lands.

### Theme

A Catppuccin Mocha-inspired palette (`tui/theme.rs`) with budget-aware thresholds
(green → yellow → red as context fills).

### Web TUI

The same ratatui interface is streamed to the browser. `tui/web.rs` renders the
UI to an in-memory buffer and broadcasts the **ANSI output over a WebSocket**
(via a `broadcast` channel) to an xterm.js terminal — this is the dashboard's
"Terminal" tab. Key events and resizes from xterm.js are parsed back into
crossterm `KeyEvent`s, so the web terminal is interactive. Each browser
connection renders its own view (its own scroll position, tab, and overlays)
while sharing the same `AppMode`. See [Web Dashboard](./web-dashboard.md).

## Autonomy System

Autonomy controls which actions require human approval (`autonomy.rs`). It
layers three mechanisms.

### Layer 1 — global level

Set with `--autonomy` (or cycled in the TUI with `+`/`-`). `AutonomyLevel`:

| Level | Behavior |
|-------|----------|
| Low | Ask before every category except `FileRead` |
| Medium (default) | Ask for writes, deletes, destructive, and network |
| High | Don't ask for the above (only the always-ask categories below) |
| Full | Never ask, except `HumanInput` |

### Layer 2 — per-category rules

The `[approval]` section of `intendant.toml` sets a per-category rule that
overrides the global level: `auto` (always approve), `ask` (require approval),
`deny` (always deny — surfaced as an approval that will be denied).

### Layer 3 — per-action approval

When approval is required, the agent loop pauses and the TUI shows the command
preview and category. The user approves (`y`), skips (`s`), approves-all (`a`,
which also flips autonomy to Full), or denies (`n`). MCP/web frontends expose the
same choices ([MCP Server](./mcp-server.md)).

### How `needs_approval` actually resolves

The precise logic (`Autonomy::needs_approval`) has nuances worth knowing:

- **Always ask, regardless of level:** `HumanInput` and `LiveAudioSpawn` — these
  always require a human even at Full. (`HumanInput` is the only thing Full still
  asks for; `LiveAudioSpawn` is treated the same way.)
- **`DisplayControl`** — asks on *first* use, then the session grant takes over
  (`return !user_display_granted`).
- **Full** — auto-approves everything else.
- **Low** — asks for everything except `FileRead` (a `deny` rule still blocks).
- **Medium / High** — start from the per-category rule. For an `ask` rule,
  Medium asks only for `FileWrite` / `FileDelete` / `Destructive` /
  `NetworkRequest`; High asks for none of them.

### Action classification

Commands are classified into categories by inspecting the command JSON
(`classify_command`):

| Category | Examples |
|----------|----------|
| FileRead | `inspectPath`, `recallMemory` |
| FileWrite | `editFile`, `writeFile`, `storeMemory` |
| FileDelete | shell commands with `rm`, `rmdir` |
| CommandExec | `execAsAgent`, `execPty` |
| NetworkRequest | shell commands with `curl`, `wget`, `ssh`, `git` |
| Destructive | shell commands with `rm -rf`, `kill`, `dd`, `mkfs`, `sudo` |
| HumanInput | `askHuman` |
| LiveAudioSpawn | `spawn_live_audio` (voice sessions, phone calls) |
| DisplayControl | user-session display access (session-grant via `d`) |

For shell commands (`execAsAgent`/`execPty`), the command string is further
inspected for destructive patterns, network tools, and file writes (redirects,
`tee`, `mv`, `cp`). A `sudo` prefix is flagged Destructive *and* the command
after `sudo` is classified too. When multiple categories apply, the highest-
severity one drives the prompt label.

### DisplayControl session grant

`DisplayControl` uses a **session-grant** model: approve once with the `d` hotkey
(or via the dashboard) and the agent keeps access to the user's display for the
rest of the session (used by both [computer use](./computer-use-and-audio.md) and
WebRTC streaming). Press `d` again — or revoke from the dashboard — to drop it.

## Web Dashboard

`--web` starts a multi-tab dashboard (Activity, Stats, Terminal, Video, Sessions,
Settings). The Terminal tab is the [Web TUI](#web-tui) over xterm.js; the others
add event logging, cost tracking, WebRTC display viewing, and recording replay,
with optional live-voice [presence](./presence.md).

```bash
./target/release/intendant --web         # default port 8765
./target/release/intendant --web 9000     # custom port
```

> **Correction:** `--web` does **not** imply `--mcp`. They are separate run
> modes — `--mcp` speaks JSON-RPC on stdio, while `--web` serves the dashboard
> and dispatches via `ControlMsg`/`control_plane`. The web dashboard can start
> idle (no initial task) and accept tasks dynamically; that idle behavior comes
> from the web-daemon path, not from MCP.

See [Web Dashboard](./web-dashboard.md) for the full UI and
[Integrations — Web Gateway](./integrations.md#web-gateway) for the WebSocket
protocol.
