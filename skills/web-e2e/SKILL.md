---
name: web-e2e
description: >
  E2E test the --web live mode. Launches Xvfb, runs intendant --web as a
  background process, opens Firefox on the web TUI, and asserts via /debug
  endpoint and WebSocket JSON. Human monitors via VNC on port 5950.
compatibility: Requires Xvfb, Firefox, x11vnc, xdotool, curl
allowed-tools: Bash Read
disable-model-invocation: true
---

# Test --web Live Mode E2E

## Key Differences from TUI E2E

- **No xterm needed**: `--web` uses `WebTui` (buffer-backed ratatui backend).
  Intendant runs as a plain background process, not inside a terminal emulator.
- **Firefox is the UI**: Open Firefox on the Xvfb display pointing to
  `http://localhost:8765`. The browser renders the TUI via xterm.js and
  provides voice model controls.
- **Voice model connects from browser**: Two auth modes for Gemini Live:
  1. **API key in localStorage** (preferred) — supports tool calling via
     `BidiGenerateContent`. Set `gemini_api_key` in localStorage.
  2. **Ephemeral tokens** (fallback) — server mints tokens via `POST /session`,
     uses `BidiGenerateContentConstrained`. No tool calling support.
  The browser checks localStorage first; falls back to `/session` if no key.
- **Control is via browser**: No `--control-socket` or socat needed. The
  browser IS the control interface (approval buttons, voice commands, etc.)

## Launch

**IMPORTANT:** Always use display **:50** (intendant reserves :99+ for its own Xvfb).
Always start `x11vnc` so the human can follow along via VNC on port 5950.

```bash
# 1. Kill stale processes from prior runs (use -9 for firefox — it ignores SIGTERM)
pkill -f 'Xvfb :50' 2>/dev/null
pkill -f 'x11vnc.*:50' 2>/dev/null
pkill -9 -f firefox 2>/dev/null
pkill -f intendant 2>/dev/null
sleep 0.5

# 2. Start Xvfb + x11vnc (MANDATORY — human needs VNC to observe)
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -nopw -forever -quiet > /dev/null 2>&1 &
sleep 0.5

# 3. Launch intendant --web as background process (no xterm needed)
> /tmp/intendant-web-stderr.log
cd /home/user/projects/intendant && source .env && \
  nohup ./target/release/intendant --direct --autonomy low --web \
  "your task here" > /dev/null 2>/tmp/intendant-web-stderr.log &

# 4. Wait for web gateway to start
sleep 3
cat /tmp/intendant-web-stderr.log  # Should show "Web TUI: http://0.0.0.0:8765"

# 5. Launch Firefox on display :50 pointing to the web TUI
DISPLAY=:50 nohup firefox --new-window http://localhost:8765 > /dev/null 2>&1 &
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

### Check voice connection
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

## Simulating Voice Input

Since there's no real microphone on a headless display, simulate voice input
by sending text directly to the live model via the Firefox debugger or
`pw.send_text()` from the browser console.

**Via Firefox debugger** (if `--start-debugger-server 6000` is active):
```bash
python3 /tmp/ff-eval.py "pw.send_text('Hello, what is happening?')"
```

**Setting API key in localStorage** (required for tool calling):
```bash
source .env && python3 /tmp/ff-eval.py "localStorage.setItem('gemini_api_key', '$GEMINI_API_KEY'); 'stored'"
# Then reload the page:
python3 /tmp/ff-eval.py "location.reload(); 'reloading'"
```

**Via xdotool** (open DevTools F12 → Console tab → type):
```bash
DISPLAY=:50 xdotool key F12
sleep 2
DISPLAY=:50 xdotool mousemove 400 658 click 1
sleep 0.3
DISPLAY=:50 xdotool type --clearmodifiers 'pw.send_text("Hello, what are you working on?")'
DISPLAY=:50 xdotool key Return
```

Then verify with `curl -s http://localhost:8765/debug` to see voice logs.

## Keyboard Input via xdotool

Click inside the xterm.js terminal first to give it focus, then send keys:

```bash
DISPLAY=:50 xdotool mousemove 500 300 click 1
sleep 0.2
DISPLAY=:50 xdotool key y   # approve
```

**Gotcha**: If the follow-up text input panel is active, keyboard shortcuts
go into the text input. Press Escape first to dismiss it.

## Screenshot (optional — for human VNC verification only)

```bash
DISPLAY=:50 import -window root /tmp/web-e2e-screenshot.png
```

This is **not needed for assertions** — use `/debug` instead.

## Known Gotchas

- **Two Gemini endpoints, different capabilities**:
  | | `BidiGenerateContent` | `BidiGenerateContentConstrained` |
  |---|---|---|
  | Auth | API key (`?key=`) | Ephemeral token (`?access_token=`) |
  | Frames | Text | Binary (ArrayBuffer) |
  | Tool calling | Yes | No (model narrates but never emits `toolCall`) |
  | Setup message | Full (model + generation_config + tools) | Minimal (tools + system_instruction only) |
- **Binary frame handling**: WASM must set `ws.set_binary_type(BinaryType::Arraybuffer)`
  before connecting. Default `Blob` type silently drops binary messages.
- **`serde_wasm_bindgen` Map vs Object**: Version 0.6+ serializes `serde_json::Value`
  maps as ES6 `Map`, not plain `Object`. This breaks `Object.keys()` and property access.
  Use `Serializer::new().serialize_maps_as_objects(true)` for any value passed to JS callbacks.
- **`response_modalities` must be `["AUDIO"]` only**: Adding `"TEXT"` causes
  WebSocket close code 1007 ("Invalid argument") on the constrained endpoint.
  Note: `["AUDIO"]` mode still sends BOTH audio AND text parts — the model
  outputs text alongside audio. The restriction is about what you request, not
  what the model produces.
- **WASM cache**: Content-hash versioning (`?v=<hash>`) on WASM/JS URLs means
  browsers automatically fetch new assets after rebuilds. No manual cache
  clearing needed.
- **WASM rebuild**: From `crates/presence-web/`:
  `wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web`
  Then `cargo build --release -p intendant` (use `-p intendant` to skip WASM-only crate).
- **Gemini REST token endpoint**: `POST /v1alpha/auth_tokens` (snake_case, NOT
  camelCase `authTokens`). Body uses flat fields, NOT wrapped in `"config"`.
  Use `bidi_generate_content_setup` to bake model+config into the token.
- **AudioContext warning** on headless displays is expected and harmless.
- **Follow-up panel** captures keystrokes — Escape first before sending shortcuts.
- **Firefox profile lock**: If Firefox won't start, remove lock files:
  `rm -f ~/.mozilla/firefox/*/.parentlock ~/.mozilla/firefox/*/lock`

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
Then launch Firefox with `--start-debugger-server 6000` and use `/tmp/ff-eval.py`
(a zero-dependency raw socket script) for JS evaluation — no pip packages needed.
If `/tmp/ff-eval.py` doesn't exist, create it from the project's prior test artifacts
or write a fresh one using the Firefox remote debug protocol (`length:json` framing).

## Cleanup

```bash
pkill -f 'intendant.*web' 2>/dev/null
pkill -f firefox 2>/dev/null
pkill -f 'Xvfb :50' 2>/dev/null
pkill -f 'x11vnc.*:50' 2>/dev/null
```
