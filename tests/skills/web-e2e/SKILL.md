---
name: web-e2e
description: >
  E2E test the --web app UI. Launches Xvfb, runs intendant --web as a
  background process, opens Firefox on /app, and asserts via /debug
  endpoint and WebSocket JSON. Human monitors via VNC on port 5950.
compatibility: Requires Xvfb, Firefox, x11vnc, xdotool, curl
allowed-tools: Bash Read
disable-model-invocation: true
---

# Test --web App UI E2E

## Key Concepts

- **No xterm needed**: `--web` uses `WebTui` (buffer-backed ratatui backend).
  Intendant runs as a plain background process, not inside a terminal emulator.
- **Firefox renders `/app`**: The tabbed web UI with Activity, Usage, Terminal,
  Displays tabs. All logic runs in WASM (presence-web), JS is a thin rendering layer.
- **Live mode available**: `/app` has a mic button for connecting Gemini Live or
  OpenAI Realtime. Set API key in localStorage first.
- **Approval via browser**: Approval buttons in the Activity tab, or keyboard
  shortcuts (y/s/a/n). No `--control-socket` or socat needed.
- **Display streaming**: The Displays tab shows live VNC via noVNC. Agent's
  display :99 is streamed in real-time.

## Launch

**IMPORTANT:** Always use display **:50** (intendant reserves :99+ for its own Xvfb).
Always start `x11vnc` so the human can follow along via VNC on port 5950.

```bash
# 1. Kill stale processes from prior runs (use -9 for firefox — it ignores SIGTERM)
# NEVER use pkill — Claude Code blocks it (exit 144).
# First check what's running, then kill specific PIDs:
pgrep -fa 'Xvfb :50|x11vnc.*:50|firefox|intendant'
# Then kill the relevant PIDs from the output above:
# kill <pid1> <pid2> ...
# kill -9 <firefox_pid>  (firefox ignores SIGTERM)
sleep 0.5

# 2. Start Xvfb + x11vnc (MANDATORY — human needs VNC to observe)
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &
sleep 0.5

# 3. Launch intendant --web as background process (no xterm needed)
> /tmp/intendant-web-stderr.log
cd /home/user/projects/intendant && source .env && \
  nohup ./target/release/intendant --direct --autonomy low --web \
  "your task here" > /dev/null 2>/tmp/intendant-web-stderr.log &

# 4. Wait for web gateway to start
sleep 3
cat /tmp/intendant-web-stderr.log  # Should show "Web TUI: http://0.0.0.0:8765"

# 5. Launch Firefox on display :50 pointing to /app
rm -f ~/.mozilla/firefox/*/.parentlock ~/.mozilla/firefox/*/lock 2>/dev/null
DISPLAY=:50 nohup firefox -P default --new-window http://localhost:8765/app > /dev/null 2>&1 &
sleep 8
```

## Asserting on State (primary method — no screenshots)

### /debug endpoint

The `/debug` endpoint returns the full agent state as JSON. Use it for all assertions.

```bash
# Full state dump
curl -s http://localhost:8765/debug | python3 -m json.tool

# Assert on specific fields
curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
state = d.get('agent_state', d)
print(f'Phase: {state.get(\"phase\")}')
print(f'Turn: {state.get(\"turn\")}')
print(f'Pending approval: {state.get(\"pending_approval\")}')
voice = d.get('voice', {})
print(f'Voice connected: {voice.get(\"connected\", False)}')
print(f'Voice logs: {voice.get(\"voice_log_count\", 0)}')
"
```

### Wait for a specific state
```bash
# Poll until approval is pending (up to 30s)
for i in $(seq 1 30); do
  PENDING=$(curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
print('yes' if pa and pa != 'null' else 'no')
" 2>/dev/null)
  [ "$PENDING" = "yes" ] && break
  sleep 1
done
echo "Approval pending: $PENDING"
```

### Wait for task completion
```bash
for i in $(seq 1 60); do
  PHASE=$(curl -s http://localhost:8765/debug | python3 -c "
import sys, json; print(json.load(sys.stdin).get('agent_state', {}).get('phase', ''))" 2>/dev/null)
  [ "$PHASE" = "Done" ] || [ "$PHASE" = "Idle" ] && break
  sleep 1
done
echo "Final phase: $PHASE"
```

### Approve via WebSocket (programmatic, no browser interaction)
```bash
python3 -c "
import asyncio, json, websockets
async def approve():
    async with websockets.connect('ws://localhost:8765') as ws:
        await asyncio.wait_for(ws.recv(), timeout=3)  # bootstrap
        await ws.send(json.dumps({'action': 'approve', 'id': 1}))
        print('Approved')
asyncio.run(approve())
"
```

### Check voice/live connection
```bash
curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('voice', {})
assert v.get('connected') == True, f'Voice not connected: {v}'
print('Voice connected OK')
print(f'Last voice log: {v.get(\"last_voice_log\", \"(none)\")}')
"
```

### Other endpoints
```bash
# Config: provider, model, sample rates (no secrets)
curl -s http://localhost:8765/config

# Session: mint ephemeral token (called by browser on mic click)
curl -s -X POST http://localhost:8765/session
```

## Simulating Live Input

Since there's no real microphone on a headless display, simulate live input
by sending text directly via the Firefox debugger.

**Via Firefox debugger** (if `--start-debugger-server 6000` is active):
```bash
# The WASM instance in /app is exposed as window.app (PresenceWeb)
python3 scripts/ff-eval.py "app.send_text('Hello, what is happening?')"
```

**Setting API key in localStorage** (required for tool calling):
```bash
source .env && python3 scripts/ff-eval.py "localStorage.setItem('gemini_api_key', '$GEMINI_API_KEY'); 'stored'"
# Then reload the page:
python3 scripts/ff-eval.py "location.reload(); 'reloading'"
```

**Clicking the mic button**:
```bash
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3
```

## Approval via Browser UI

The `/app` Activity tab has approval buttons (Approve/Skip/Approve All/Deny).
You can also use keyboard shortcuts when the page has focus:

```bash
# Press 'y' to approve (the page must have focus)
DISPLAY=:50 xdotool key y
```

**Gotcha**: If the follow-up text input panel is active, keyboard shortcuts
go into the text input. Press Escape first to dismiss it.

## Displays Tab

When the agent runs commands that trigger Xvfb (display :99), the Displays tab
shows a live VNC stream via noVNC. Features:

- **View-only** by default — watch what the agent does
- **Take Control** button — switch to interactive mode (mouse/keyboard forwarded)
- **Release** button — return to view-only, optional note for the agent
- Auto-connects when `display_ready` event arrives

## Screenshot (optional — for human VNC verification only)

```bash
DISPLAY=:50 import -window root /tmp/web-e2e-screenshot.png
```

This is **not needed for assertions** — use `/debug` instead.

## Known Gotchas

- **WASM cache**: Content-hash versioning (`?v=<hash>`) on WASM/JS URLs means
  browsers automatically fetch new assets after rebuilds. No manual cache
  clearing needed.
- **WASM rebuild**: From `crates/presence-web/`:
  `wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web`
  Then `cargo build --release -p intendant` (use `-p intendant` to skip WASM-only crate).
- **AudioContext warning** on headless displays is expected and harmless.
- **Follow-up panel** captures keystrokes — Escape first before sending shortcuts.
- **Firefox profile lock**: If Firefox won't start, remove lock files:
  `rm -f ~/.mozilla/firefox/*/.parentlock ~/.mozilla/firefox/*/lock`
- **Late-connect**: If you reload the browser mid-session, the Activity tab
  replays the full session log. Usage tab gets cached data. Displays tab
  auto-reconnects to VNC.
- **Two Gemini endpoints, different capabilities**:
  | | `BidiGenerateContent` | `BidiGenerateContentConstrained` |
  |---|---|---|
  | Auth | API key (`?key=`) | Ephemeral token (`?access_token=`) |
  | Frames | Text | Binary (ArrayBuffer) |
  | Tool calling | Yes | No |
  | Setup message | Full (model + config + tools) | Minimal (tools + system_instruction only) |

**For browser-side JS debugging** (only needed for WASM/JS errors):

Firefox `--start-debugger-server` requires `devtools.debugger.remote-enabled=true`
in the Firefox profile's `user.js`. Set up once per environment:
```bash
PROFILE=$(ls -d ~/.mozilla/firefox/*.default* | head -1)
cat >> "$PROFILE/user.js" << 'EOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
EOF
```
Then launch Firefox with `--start-debugger-server 6000` and use `scripts/ff-eval.py`.

## Cleanup

```bash
# NEVER use pkill. Check what's running, then kill specific PIDs:
pgrep -fa 'intendant|firefox|Xvfb :50|x11vnc.*:50'
# kill <pid1> <pid2> ...
```
