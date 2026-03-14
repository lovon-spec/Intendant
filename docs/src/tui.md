# TUI & Autonomy

## TUI

`intendant` includes a ratatui-based terminal UI that launches automatically when both stdin and stdout are terminals. The TUI provides real-time monitoring and control of the agent loop.

### Layout

```
┌─────────────────────────────────────────────┐
│ StatusBar: provider │ model │ turn │ budget  │  1 line
├─────────────────────────────────────────────┤
│ ActionPanel: phase + spinner + key hints    │  2 lines
├─────────────────────────────────────────────┤
│                                             │
│ LogPanel: scrollable, color-coded entries   │  fills remaining
│                                             │
├─────────────────────────────────────────────┤
│ ApprovalPanel / InputPanel (conditional)    │  3-4 lines
└─────────────────────────────────────────────┘
```

### Panels

- **Status bar**: Provider, model, turn count, budget percentage, autonomy level
- **Action panel**: Current phase with spinner — Thinking, RunningAgent, Orchestrating, WaitingApproval, WaitingHuman, WaitingFollowUp, Idle, Done
- **Log panel**: Scrollable chronological log with color-coded levels (Info, Warning, Error, Debug)
- **Approval panel**: Shown when an action needs user approval — command preview + category, y/s/a/n keys
- **Input panel**: Shown when `askHuman` is triggered — `tui-textarea` for response
- **Follow-up panel**: Shown when agent completes a round and awaits follow-up input
- **Help overlay**: Key bindings reference (`?` key)
- **Inspect overlay**: Detailed view of selected log entry

### Key Bindings

| Key | Action |
|-----|--------|
| `q` / `Ctrl-C` | Quit |
| `v` | Toggle verbose mode (cycle through quiet/normal/verbose/debug) |
| `?` | Help overlay |
| `+` / `-` | Cycle autonomy level |
| `Up`/`Down`/`PgUp`/`PgDn` | Scroll log |
| `Home` / `End` | Jump to top/bottom of log |
| `1`-`3` | Toggle panels (status, action, log) |
| `y` / `Enter` | Approve pending action |
| `s` | Skip pending action |
| `a` | Auto-approve all remaining |
| `n` | Deny and stop |

### Streaming Display

When a model is generating a response, text deltas are forwarded to the TUI in real-time via `AppEvent::ModelResponseDelta` and accumulated in a streaming buffer. The buffer is cleared when the full response arrives. This gives immediate feedback during long model responses.

### Theme

The TUI uses a Catppuccin Mocha-inspired color scheme with budget-aware color thresholds (green → yellow → red as context fills up).

## Autonomy System

The autonomy system controls which actions require human approval. It operates on three layers:

### Layer 1 — Global Level

Set via CLI `--autonomy` flag, toggleable in TUI with `+`/`-`:

| Level | Behavior |
|-------|----------|
| Low | Ask before every command execution |
| Medium | Ask before writes, network, destructive (default) |
| High | Only ask for unavoidable human input |
| Full | Never ask (fully autonomous) |

### Layer 2 — Per-Category Rules

From `intendant.toml` `[approval]` section. Overrides the global level for specific action categories. Rules: `auto` (always approve), `ask` (require approval), `deny` (always deny).

### Layer 3 — Per-Action Approval

When approval is needed, the agent loop pauses and the TUI shows the command preview. The user can approve, skip, deny, or switch to auto-approve mode.

### Action Classification

Commands are classified into categories by inspecting the command JSON:

| Category | Examples |
|----------|----------|
| FileRead | `inspectPath`, `recallMemory` |
| FileWrite | `editFile`, `writeFile`, `storeMemory` |
| FileDelete | Commands with `rm`, `rmdir` |
| CommandExec | `execAsAgent`, `execPty` |
| NetworkRequest | Commands with `curl`, `wget`, `ssh`, `git` |
| Destructive | Commands with `rm -rf`, `kill`, `dd`, `mkfs`, `sudo` |
| HumanInput | `askHuman` |

Shell commands are further classified by inspecting the command string for destructive patterns, network tools, and file writes (redirects, `tee`, `mv`, `cp`). The `sudo` prefix is detected as Destructive and the actual command after `sudo` is also classified.

## Web TUI

The `--web` flag serves the TUI remotely via WebSocket using xterm.js. The full ratatui interface is rendered server-side into an ANSI buffer and streamed to connected browsers.

### Running

```bash
# Default port 8765
./target/release/intendant --web

# Custom port
./target/release/intendant --web 9000
```

Open `http://<host>:8765/` in a browser. The terminal renders the same layout as the native TUI — status bar, log panel, action panel, approval/input panels. Key presses and terminal resizes in the browser are sent back to the server.

The `--web` flag implies `--mcp`, so no initial task is required — the agent starts idle and accepts tasks dynamically via the web UI or programmatically.

### Voice Overlay

The web TUI includes an optional voice overlay for browser-side live model interaction (Gemini Live / OpenAI Realtime). When activated:

- The browser connects directly to the model's realtime API for low-latency voice I/O
- The live model receives agent events and narrates progress in first person
- Tool calls from the live model are routed through the WebSocket protocol to the server
- Server-side presence is automatically paused (mutual exclusion)

Voice requires an API key (Gemini or OpenAI), stored in browser localStorage. The remote TUI works without voice enabled.

See [Integrations — Web Gateway](./integrations.md#web-gateway) for the full WebSocket protocol documentation and [Presence Layer](./presence.md) for details on presence mutual exclusion.
