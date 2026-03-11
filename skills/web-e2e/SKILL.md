---
name: web-e2e
description: >
  E2E test the --web live mode. Launches Xvfb, runs intendant --web as a
  background process (no xterm needed), opens Firefox on the web TUI,
  and takes screenshots. User monitors via VNC.
compatibility: Requires Xvfb, Firefox, ImageMagick (import), x11vnc, xdotool
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
- **Voice model connects from browser**: The server holds permanent API keys
  and mints ephemeral tokens via `POST /session`. The browser clicks the mic
  button, fetches an ephemeral token, and connects directly to the vendor
  WebSocket (Gemini Live / OpenAI Realtime). No localStorage needed.
- **Control is via browser**: No `--control-socket` or socat needed. The
  browser IS the control interface (approval buttons, voice commands, etc.)

## Launch

**IMPORTANT:** Always use display **:50** (intendant reserves :99+ for its own Xvfb).
Always start `x11vnc` so the human can follow along via VNC on port 5950.

```bash
# 1. Kill stale processes from prior runs
pkill -f 'Xvfb :50' 2>/dev/null; pkill -f 'x11vnc.*:50' 2>/dev/null
pkill -f 'intendant.*web' 2>/dev/null; pkill -f firefox 2>/dev/null
sleep 0.5

# 2. Start Xvfb + x11vnc (MANDATORY — human needs VNC to observe)
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -nopw -forever -quiet > /dev/null 2>&1 &
sleep 0.5

# 3. Launch intendant --web as background process (no xterm needed)
> /tmp/intendant-web-stderr.log
cd /home/user/projects/intendant-codex-fork && source .env && \
  nohup ./target/release/intendant --direct --autonomy low --web \
  "your task here" > /dev/null 2>/tmp/intendant-web-stderr.log &

# 4. Wait for web gateway to start
sleep 3
cat /tmp/intendant-web-stderr.log  # Should show "Web TUI: http://0.0.0.0:8765"

# 5. Launch Firefox on display :50 pointing to the web TUI
DISPLAY=:50 nohup firefox --new-window http://localhost:8765 > /dev/null 2>&1 &
```

## Debugging

**Use `curl` and the `/debug` endpoint for all debugging — no screenshots needed.**

```bash
# Server state: agent phase, pending approvals, voice connection, voice logs
curl -s http://localhost:8765/debug | python3 -m json.tool

# Config: provider, model, sample rates (no secrets)
curl -s http://localhost:8765/config

# Session: mint ephemeral token (called by browser on mic click)
curl -s -X POST http://localhost:8765/session
```

The `/debug` endpoint returns:
- `agent_state`: phase, turn, budget, pending_approval, last_command
- `voice.connected`: whether browser voice model is connected
- `voice.voice_log_count`: number of voice text/tool logs received
- `voice.last_voice_log`: most recent voice model text response

**Test Gemini WebSocket from terminal** (requires `pip3 install websockets`):
```bash
TOKEN=$(curl -s -X POST http://localhost:8765/session | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')
python3 - "$TOKEN" << 'PYEOF'
import asyncio, json, websockets, sys
TOKEN = sys.argv[1]
async def test():
    url = f"wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContentConstrained?access_token={TOKEN}"
    async with websockets.connect(url) as ws:
        print("Connected!")
        setup = {"setup": {"system_instruction": {"parts": [{"text": "Say hello"}]}, "tools": [{"function_declarations": []}]}}
        await ws.send(json.dumps(setup))
        msg = await asyncio.wait_for(ws.recv(), timeout=10)
        print(f"Response: {msg[:200]}")  # Should show setupComplete
asyncio.run(test())
PYEOF
```

**For browser-side JS debugging** (only needed for WASM/JS errors):

Firefox `--start-debugger-server` requires `devtools.debugger.remote-enabled=true`
in the Firefox profile's `user.js`. Once enabled, connect with a raw socket script
(see `/tmp/ff-eval.py` if it exists from prior runs) — no pip dependencies needed.

## Simulating Voice Input

Since there's no real microphone on a headless display, simulate voice input
by sending text directly to the live model via the Firefox debugger or
`pw.send_text()` from the browser console.

**Via Firefox debugger** (if `--start-debugger-server 6000` is active):
```bash
python3 /tmp/ff-eval.py "window.__presenceWeb.send_text('Hello, what is happening?')"
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

Then check `curl -s http://localhost:8765/debug` to see voice logs.

## Keyboard Input via xdotool

Click inside the xterm.js terminal first to give it focus, then send keys:

```bash
DISPLAY=:50 xdotool mousemove 500 300 click 1
sleep 0.2
DISPLAY=:50 xdotool key y   # approve
```

**Gotcha**: If the follow-up text input panel is active, keyboard shortcuts
go into the text input. Press Escape first to dismiss it.

## Screenshot

```bash
DISPLAY=:50 import -window root /tmp/web-e2e-screenshot.png
```

## Known Gotchas

- **Ephemeral tokens require `BidiGenerateContentConstrained`**: The non-constrained
  `BidiGenerateContent` returns WebSocket close code 1008 with ephemeral tokens.
- **Constrained endpoint sends binary frames**: Unlike the text-frame
  `BidiGenerateContent`, `BidiGenerateContentConstrained` sends ArrayBuffer
  WebSocket frames. The WASM must set `ws.set_binary_type(BinaryType::Arraybuffer)`
  before connecting — default `Blob` type silently drops binary messages.
- **Constrained endpoint does NOT support tool calling**: With ephemeral tokens,
  the model narrates about calling tools but never actually emits `toolCall` messages.
  Tool calls work with `BidiGenerateContent` + API key auth. This is a Gemini API
  limitation — a WebSocket proxy would be needed to fix.
- **`response_modalities` must be `["AUDIO"]` only**: Adding `"TEXT"` causes
  WebSocket close code 1007 ("Invalid argument") on the constrained endpoint.
- **Firefox WASM cache**: After rebuilding WASM, you MUST clear the cache manually:
  `rm -rf ~/.mozilla/firefox/*/cache2/ ~/.cache/mozilla/firefox/*/cache2/`
  then relaunch Firefox. Ctrl+Shift+R is NOT sufficient.
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

## Cleanup

```bash
pkill -f 'intendant.*web' 2>/dev/null
pkill -f firefox 2>/dev/null
pkill -f 'Xvfb :50' 2>/dev/null
pkill -f 'x11vnc.*:50' 2>/dev/null
```
