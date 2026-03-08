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

### Key Bindings

| Key | Action |
|-----|--------|
| `q` / `Ctrl-C` | Quit |
| `v` | Toggle verbose mode |
| `?` | Help overlay |
| `+` / `-` | Cycle autonomy level |
| `Up`/`Down`/`PgUp`/`PgDn` | Scroll log |
| `Home` / `End` | Jump to top/bottom of log |
| `1`-`3` | Toggle panels (status, action, log) |
| `y` / `Enter` | Approve pending action |
| `s` | Skip pending action |
| `a` | Auto-approve all remaining |
| `n` | Deny and stop |

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

Action categories are determined by analyzing command JSON: shell commands are classified by inspecting for destructive patterns (`rm`, `kill`, `dd`, `mkfs`, `sudo`), network operations (`curl`, `wget`, `ssh`), file operations, etc.

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

### Voice Overlay

The web TUI includes an optional voice overlay for browser-side live model interaction (Gemini Live / OpenAI Realtime). When activated:

- The browser connects directly to the model's realtime API for low-latency voice I/O
- The live model receives agent events and narrates progress in first person
- Tool calls from the live model are routed through the WebSocket protocol to the server
- Server-side presence is automatically paused (mutual exclusion)

Voice requires an API key (Gemini or OpenAI), stored in browser localStorage. The remote TUI works without voice enabled.

See [Web Gateway](./integrations.md#web-gateway) for the full WebSocket protocol documentation.
