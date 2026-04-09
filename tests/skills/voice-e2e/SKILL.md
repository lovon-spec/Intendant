---
name: voice-e2e
description: >
  E2E test the live audio pipeline with real audio. Uses espeak-ng TTS +
  PulseAudio virtual mic to feed synthesized speech to Gemini Live or
  OpenAI Realtime through the browser's getUserMedia on /app.
  Asserts via /debug endpoint JSON.
  Human monitors via VNC on port 5950.
  Tests the full audio path:
  TTS -> virtual mic -> Firefox -> AudioWorklet -> WASM -> live model -> tool calls -> agent.
compatibility: Requires Xvfb, Firefox, x11vnc, espeak-ng, ffmpeg, PulseAudio, xdotool
allowed-tools: Bash Read
disable-model-invocation: true
---

# Live Audio E2E Testing with Real Audio

## Overview

This skill tests the **full audio pipeline** end-to-end: synthesized speech flows
through a PulseAudio virtual microphone into Firefox's `getUserMedia`, through the
AudioWorklet and WASM layer, to the live model (Gemini Live or OpenAI Realtime),
which processes the audio and emits tool calls that drive the agent.

Unlike `web-e2e` (which uses `app.send_text()` to bypass audio), this tests the
actual audio capture, resampling, PCM conversion, and WebSocket audio streaming.

All assertions use the `/debug` JSON endpoint — no screenshots needed.
The graphical stack (Firefox on Xvfb) runs for human VNC observation.

The browser opens `/app` which has full live mode built into the WASM layer.

## Architecture

```
espeak-ng "text"
    |
    v
ffmpeg (resample to 48kHz mono s16le)      <-- match browser's native AudioContext rate
    |
    v
paplay --device=virtual_mic (PulseAudio)
    |
    v
virtual_mic.monitor (PulseAudio source)     <-- Firefox sees this as default mic
    |
    v
Firefox getUserMedia({audio: true})
    |
    v
AudioWorklet (audio-processor.js)
    |
    v
WASM (presence-web PresenceWeb) -- resample to 16kHz/24kHz, PCM16, base64
    |
    v
WebSocket to live model (Gemini Live / OpenAI Realtime)
    |
    v
Live model processes audio, emits tool calls + audio responses
    |
    v
WASM callbacks -> browser UI -> WebSocket -> intendant agent
```

## Prerequisites

```bash
# Install if missing
sudo apt-get install -y espeak-ng ffmpeg pulseaudio pulseaudio-utils pipewire-alsa
```

**`pipewire-alsa` is critical** — Firefox-ESR uses ALSA for audio. Without the
PipeWire-ALSA bridge, Firefox cannot see PipeWire virtual audio devices at all
(`enumerateDevices()` returns 0 devices, `getUserMedia` fails with NotFoundError).

PulseAudio/PipeWire must be running (`pactl info` should succeed).

**Only one browser can be active.** If you have `/app` open on another device
(e.g. your host browser), close it or disconnect its voice first — otherwise
the VM Firefox is passive and its audio is ignored by the presence layer.

## Setup

### 1. Clean up stale processes

NEVER use `pkill` — Claude Code blocks it entirely (exit code 144), killing the whole
command. Use `kill $(pgrep -f 'pattern')` instead. Always verify kills with `pgrep` after.

Use `-9` for Firefox (it ignores SIGTERM). Firefox may be named `firefox` or
`firefox-esr` depending on the system — kill both.

```bash
for p in $(pgrep -x Xvfb 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x x11vnc 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
for p in $(pgrep -x intendant 2>/dev/null); do kill "$p" 2>/dev/null; done
sleep 0.5
# Verify all dead
pgrep -x 'Xvfb|x11vnc|firefox|firefox-esr|intendant' && echo "WARN: stale processes remain" || echo "All clean"
```

### 2. Create PulseAudio virtual microphone

```bash
# Unload any prior virtual_mic module
pactl unload-module $(pactl list short modules | grep 'sink_name=virtual_mic' | awk '{print $1}') 2>/dev/null

# Create null sink — its .monitor becomes the virtual mic source
pactl load-module module-null-sink sink_name=virtual_mic \
  sink_properties=device.description="VirtualMic" \
  rate=48000 channels=1 format=s16le

# Set the virtual mic monitor as the default recording source
pactl set-default-source virtual_mic.monitor

# Verify
pactl list short sources | grep virtual_mic
# Should show: virtual_mic.monitor
```

**Why 48kHz?** The browser's `AudioContext` uses the system's native sample rate
(typically 48kHz). The WASM layer handles downsampling to the live model's
target rate (16kHz for Gemini, 24kHz for OpenAI). We match the browser rate
at the PulseAudio level so no extra resampling happens in PulseAudio.

### 3. Start Xvfb + x11vnc

```bash
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &
sleep 0.5
```

### 4. Launch intendant --web

Prefer running `intendant --web` in a long-lived session/PTY. In this environment,
short-lived shell wrappers around `nohup ... &` let the web server disappear
between steps.

```bash
> /tmp/intendant-web-stderr.log
cd /home/user/projects/intendant && source .env && \
  nohup ./target/release/intendant --direct --autonomy low --web \
  "your task here" > /dev/null 2>/tmp/intendant-web-stderr.log &
sleep 3
cat /tmp/intendant-web-stderr.log  # Should show "Web TUI: http://0.0.0.0:8765"
```

### 5. Check the target live provider

```bash
curl -s http://localhost:8765/config
# Returns: {"provider":"gemini","model":"...","input_sample_rate":16000,"output_sample_rate":24000}
# or:      {"provider":"openai","model":"...","input_sample_rate":24000,"output_sample_rate":24000}
```

Use the `provider` field to decide the Gemini vs OpenAI path below.

### 6. Launch Firefox with debugger

Set up Firefox remote debug (once per environment):
```bash
PROFILE=$(ls -d ~/.mozilla/firefox/*.default* | head -1)
grep -q 'devtools.debugger.remote-enabled' "$PROFILE/user.js" 2>/dev/null || \
cat >> "$PROFILE/user.js" << 'EOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
EOF
```

Launch (pointing to `/app`). Use `-P default` so Firefox uses the profile where
we set debugger prefs and mic permissions — without it, it may pick a different
profile and the debugger server won't bind:
```bash
rm -f ~/.mozilla/firefox/*/.parentlock ~/.mozilla/firefox/*/lock 2>/dev/null
DISPLAY=:50 nohup firefox -P default --start-debugger-server 6000 \
  --new-window http://localhost:8765/app > /dev/null 2>&1 &
sleep 5
```

### 7. Set API key in browser localStorage (Gemini)

For Gemini Live with tool calling, an API key must be in localStorage:
```bash
set -a && source .env && set +a
JS=$(python3 - <<'PY'
import json, os
print("localStorage.setItem('gemini_api_key', %s); 'stored'" % json.dumps(os.environ['GEMINI_API_KEY']))
PY
)
python3 scripts/ff-eval.py "$JS"
python3 scripts/ff-eval.py "!!localStorage.getItem('gemini_api_key')"
python3 scripts/ff-eval.py "location.reload(); 'reloading'"
sleep 3
```

**Important**: if the first-run voice dialog is already open, storing the key in
`localStorage` does not dismiss it. Reload after storing, or click the mic button
again once the key exists.

For OpenAI Realtime, set:
```bash
source .env && python3 scripts/ff-eval.py \
  "localStorage.setItem('openai_api_key', '$OPENAI_API_KEY'); 'stored'"
python3 scripts/ff-eval.py "location.reload(); 'reloading'"
sleep 3
```

## Sending Audio

### The `say` helper

This is the core function. It synthesizes speech with espeak-ng, converts to the
format PulseAudio expects, and plays it into the virtual mic sink:

```bash
say() {
  local text="$1"
  local speed="${2:-140}"  # words per minute (default 140, slower = clearer for ASR)
  espeak-ng "$text" -s "$speed" --stdout | \
    ffmpeg -loglevel error -i pipe:0 \
      -f s16le -ar 48000 -ac 1 pipe:1 | \
    paplay --device=virtual_mic --format=s16le --rate=48000 --channels=1 --raw
}
```

### Usage

```bash
# Simple utterance
say "Hello, what is happening with the agent?"

# Slower speech for better recognition
say "Please submit a task to list files in /tmp" 120

# Short commands (live models handle these well)
say "approve"
say "check status"
say "yes"
```

### Sending silence (keeps the connection alive)

```bash
# 2 seconds of silence at 48kHz mono s16le = 192000 bytes of zeros
dd if=/dev/zero bs=192000 count=1 2>/dev/null | \
  paplay --device=virtual_mic --format=s16le --rate=48000 --channels=1 --raw
```

## Connecting Live Mode from Browser

The live model must be connected from the browser before audio will be processed.
Click the mic button or use the debugger:

```bash
# Click the mic button programmatically
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3
```

Or via xdotool (find mic button position via VNC):
```bash
DISPLAY=:50 xdotool mousemove 640 680 click 1
sleep 1
```

Then verify connection via `/debug` (see Assertions below).

**Important**: `voice.connected=true` only proves the live model connection is up.
It does **not** prove Firefox is actively capturing microphone audio. Verify both.

## Asserting on State (primary method — no screenshots)

### Verify live connection
```bash
curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('voice', {})
connected = v.get('connected', False)
print(f'Live connected: {connected}')
assert connected, 'Live model not connected'
"
```

### Wait for live connection
```bash
for i in $(seq 1 15); do
  CONNECTED=$(curl -s http://localhost:8765/debug | python3 -c "
import sys, json; print(json.load(sys.stdin).get('voice', {}).get('connected', False))" 2>/dev/null)
  [ "$CONNECTED" = "True" ] && break
  sleep 1
done
echo "Live connected: $CONNECTED"
```

### Verify Firefox is actively capturing from the virtual mic
After activating the mic, verify Firefox has a recording stream:

```bash
pactl list short source-outputs
```

Expected: at least one `source-output` owned by Firefox. If empty, the browser is
connected to live mode but is not currently capturing audio.

If the mic button state looks wrong after earlier setup failures, force a fresh
`getUserMedia` start by clicking the visible mic button directly:

```bash
# Example coordinates from a 1280x720 Xvfb display; adjust via VNC if needed
DISPLAY=:50 xdotool mousemove --sync 1112 608 click 1
sleep 2
pactl list short source-outputs
```

### Check live activity after speaking
```bash
say "What is the current status?" 130
sleep 5

curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('voice', {})
print(f'Voice logs: {v.get(\"voice_log_count\", 0)}')
print(f'Last voice log: {v.get(\"last_voice_log\", \"(none)\")}')
assert v.get('voice_log_count', 0) > 0, 'No voice logs — model may not have received audio'
"
```

### Verify browser audio is being sent to the live model
If `/debug` stays quiet, inspect the session log for `voice:audio_send` diagnostics:

```bash
tail -n 200 ~/.intendant/logs/*/session.jsonl | grep 'voice:audio_send'
```

This confirms the path up to the browser send loop is active:
`virtual mic -> Firefox -> getUserMedia -> AudioWorklet/WASM -> live audio send`.

### Verify task was submitted via live
```bash
say "Please list the files in /tmp" 130
sleep 8

curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
state = d.get('agent_state', d)
phase = state.get('phase', 'idle')
print(f'Phase: {phase}')
assert phase != 'idle', f'Task not started — phase still idle'
"
```

### Verify approval pending and approve via live
```bash
# Wait for approval
for i in $(seq 1 30); do
  PENDING=$(curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
print('yes' if pa and str(pa) != 'null' else 'no')
" 2>/dev/null)
  [ "$PENDING" = "yes" ] && break
  sleep 1
done
echo "Approval pending: $PENDING"

# Approve via live
say "Yes, approve that" 130
sleep 5

# Verify approval cleared
curl -s http://localhost:8765/debug | python3 -c "
import sys, json
d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
print(f'Pending approval after live approve: {pa}')
assert pa is None or str(pa) == 'null', f'Approval not cleared: {pa}'
"
```

## Example Test Scenarios

### Scenario 1: Live submits task, live approves

```bash
# 1. Connect live
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3

# 2. Verify connected
curl -s http://localhost:8765/debug | python3 -c "
import sys, json; d = json.load(sys.stdin)
assert d.get('voice',{}).get('connected'), 'Live not connected'
print('Live connected OK')
"

# 3. Submit task via live
say "Please list the files in /tmp" 130
sleep 8

# 4. Assert task started
curl -s http://localhost:8765/debug | python3 -c "
import sys, json; d = json.load(sys.stdin)
phase = d.get('agent_state', d).get('phase', 'idle')
assert phase != 'idle', f'Task not started: {phase}'
print(f'Task started — phase: {phase}')
"

# 5. Wait for and verify approval
for i in $(seq 1 30); do
  PENDING=$(curl -s http://localhost:8765/debug | python3 -c "
import sys,json; pa=json.load(sys.stdin).get('agent_state',{}).get('pending_approval')
print('yes' if pa and str(pa)!='null' else 'no')" 2>/dev/null)
  [ "$PENDING" = "yes" ] && break; sleep 1
done

# 6. Approve via live and verify
say "Yes, approve that" 130
sleep 5
curl -s http://localhost:8765/debug | python3 -c "
import sys, json; d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
assert pa is None or str(pa) == 'null', f'Approval not cleared: {pa}'
print('Approved OK')
"
```

## Troubleshooting

### No audio reaching the live model

1. **Check virtual mic exists**: `pactl list short sources | grep virtual_mic`
2. **Check default source**: `pactl get-default-source` — should be `virtual_mic.monitor`
3. **Check Firefox is actually recording**: `pactl list short source-outputs`
   - If empty, live may be connected but `getUserMedia` is not active.
3. **Test audio flow**: Play a tone and check PulseAudio levels:
   ```bash
   ffmpeg -f lavfi -i "sine=frequency=440:duration=2" -f s16le -ar 48000 -ac 1 pipe:1 2>/dev/null | \
     paplay --device=virtual_mic --format=s16le --rate=48000 --channels=1 --raw &
   pactl list short sources | grep RUNNING  # Should show virtual_mic.monitor as RUNNING
   ```
4. **Check browser mic permission**: Firefox may not have granted mic access.
   Pre-grant via:
   ```bash
   PROFILE=$(ls -d ~/.mozilla/firefox/*.default* | head -1)
   python3 -c "
   import sqlite3, time
   db = sqlite3.connect('$PROFILE/permissions.sqlite')
   db.execute('''INSERT OR REPLACE INTO moz_perms
     (origin, type, permission, expireType, expireTime, modificationTime)
     VALUES ('http://localhost:8765', 'microphone', 1, 0, 0, ?)''',
     (int(time.time() * 1000),))
   db.commit()
   db.close()
   print('Microphone permission granted for localhost:8765')
   "
   ```
   **IMPORTANT**: Set this permission BEFORE launching Firefox.

### Live model not responding to speech

- **espeak-ng quality**: espeak-ng is robotic. Live models may struggle with it.
  Try speaking slower (`say "text" 100`) or using shorter, clearer phrases.
- **Ambient noise**: The virtual mic is clean (no noise), which is actually ideal.

## Provider-Specific Notes

### Gemini Live

- **API key mode** (`BidiGenerateContent`): Supports tool calling. Set
  `gemini_api_key` in localStorage.
- Input sample rate: **16kHz** (WASM downsamples from browser's 48kHz)
- `response_modalities: ["AUDIO"]` only (adding `"TEXT"` causes close code 1007)
- espeak-ng speech recognition quality is decent — Gemini handles robotic speech
  reasonably well.

### OpenAI Realtime

- Browser gets a client secret via `POST /session` (server-side minting with
  the API key).
- Or set `openai_api_key` in localStorage for direct browser auth.
- Input sample rate: **24kHz** (WASM downsamples from 48kHz)
- `modalities: ["audio", "text"]`
- OpenAI Realtime tends to be more sensitive to audio quality — speak slower
  and clearer with espeak-ng.

## Cleanup

```bash
# Remove virtual mic
pactl list short modules | grep 'sink_name=virtual_mic' | awk '{print $1}' | xargs -r -I{} pactl unload-module {} 2>/dev/null

# Kill processes (use pgrep -x to avoid matching the shell itself)
for p in $(pgrep -x intendant 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
for p in $(pgrep -x Xvfb 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x x11vnc 2>/dev/null); do kill "$p" 2>/dev/null; done
```

## Notes from verified runs

- On this environment, verify VNC with `ss -ltnp | grep 5950` after startup.
  `x11vnc` can appear to start and then exit.
- A stable VNC launch here was one that remained attached to the live `Xvfb :50`
  display and was verified by checking the actual listener.
