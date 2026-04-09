---
name: recording-e2e
description: >
  E2E test the display recording and replay system. Launches intendant --web
  with recording enabled, triggers Xvfb display creation, verifies ffmpeg
  recording starts, segments are created and serveable, and the replay UI
  loads in the browser. Asserts via /recordings and /debug HTTP endpoints.
  Human monitors via VNC on port 5950.
compatibility: Requires Xvfb, Firefox, x11vnc, ffmpeg, curl, xdotool
allowed-tools: Bash Read
disable-model-invocation: true
---

# Display Recording & Replay E2E Testing

## Overview

This skill tests the **display recording pipeline** end-to-end: intendant
auto-launches Xvfb, the recording listener detects `DisplayReady`, spawns
ffmpeg to record the display, segments accumulate on disk, and the web
dashboard replay UI loads and plays them back.

Tests the full path:
```
Agent task → Xvfb auto-launch → DisplayReady event → RecordingStarted →
ffmpeg x11grab → segmented MP4s → /recordings API → RecordingPlayer UI
```

All assertions use HTTP endpoints (`/recordings`, `/debug`) — no screenshots needed.
The graphical stack (Firefox on Xvfb :50) runs for human VNC observation.

## Prerequisites

```bash
# Install if missing — ffmpeg is REQUIRED, recording silently does nothing without it
sudo apt-get install -y xvfb x11vnc firefox-esr ffmpeg xdotool imagemagick curl
```

ffmpeg must support `libx264` and `x11grab`:
```bash
ffmpeg -hide_banner -encoders 2>/dev/null | grep libx264
ffmpeg -hide_banner -devices 2>/dev/null | grep x11grab
```

**IMPORTANT — Worktree builds**: When running from a git worktree (e.g.
`.claude/worktrees/<name>/`), `cargo build` outputs to the **worktree's own
`target/` directory**, not the main repo's. Always use the binary from the
worktree's target dir:
```bash
# Correct (worktree binary):
/path/to/worktree/target/release/intendant

# Wrong (main repo binary — won't have your changes):
/home/user/projects/intendant/target/release/intendant
```

## Setup

### 1. Build the binary

```bash
cd /home/user/projects/intendant && source ~/.cargo/env && \
  cargo build --release 2>&1 | tail -3
```

### 2. Clean up stale processes

NEVER use `pkill` — Claude Code blocks it entirely (exit code 144), killing the whole
command. Use `kill $(pgrep -f 'pattern')` instead. Always verify kills with `pgrep` after.

```bash
for p in $(pgrep -x Xvfb 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x x11vnc 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
for p in $(pgrep -x intendant 2>/dev/null); do kill "$p" 2>/dev/null; done
sleep 0.5
pgrep -x 'Xvfb|x11vnc|firefox|firefox-esr|intendant' && echo "WARN: stale processes remain" || echo "All clean"
```

### 3. Create intendant.toml with recording enabled

Recording is disabled by default. Create a temporary project directory with
recording enabled and short segment duration for faster testing:

```bash
TESTDIR=$(mktemp -d /tmp/intendant-rec-test-XXXXXX)
cat > "$TESTDIR/intendant.toml" << 'EOF'
[recording]
enabled = true
framerate = 10
segment_duration_secs = 8
quality = "low"
EOF
echo "Test project dir: $TESTDIR"
```

**Why these values?**
- `framerate = 10`: Lower than default (30) to reduce CPU during testing
- `segment_duration_secs = 8`: Short segments so we don't wait 60s for the first one
- `quality = "low"`: CRF 35, smallest files, faster encoding

### 4. Start Xvfb + x11vnc for human monitoring

**IMPORTANT:** Use display **:50** — intendant reserves :99+ for its own Xvfb.

```bash
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &
sleep 0.5
```

### 5. Launch intendant --web with recording enabled

The task must trigger Xvfb auto-launch. Good tasks:
- "take a screenshot of the display" (triggers captureScreen → Xvfb launch)
- "run xeyes for 30 seconds then close it" (triggers execAsAgent with GUI app)
- "open xterm and run 'echo hello && sleep 30'" (triggers GUI terminal)

Use `--autonomy high` so the agent auto-approves safe commands without prompting.
Run from `$TESTDIR` so intendant picks up the `intendant.toml` config.

```bash
> /tmp/intendant-rec-stderr.log
cd "$TESTDIR" && source /home/user/projects/intendant/.env && \
  nohup /home/user/projects/intendant/target/release/intendant \
    --direct --autonomy high --web \
    "run 'xeyes' in the background, then take a screenshot after 5 seconds. After the screenshot, run 'xclock' and wait 20 seconds." \
    > /dev/null 2>/tmp/intendant-rec-stderr.log &
sleep 3
cat /tmp/intendant-rec-stderr.log
# Should show "Web TUI: http://0.0.0.0:8765"
```

### 6. Launch Firefox on display :50

```bash
rm -f ~/.mozilla/firefox/*/.parentlock ~/.mozilla/firefox/*/lock 2>/dev/null
DISPLAY=:50 nohup firefox -P default --new-window http://localhost:8765/app > /dev/null 2>&1 &
sleep 8
```

## Asserting on Recording State

### Wait for recording to start

The recording starts automatically when the agent triggers Xvfb. Poll the
`/recordings` endpoint until a stream appears:

```bash
for i in $(seq 1 60); do
  STREAMS=$(curl -s http://localhost:8765/recordings 2>/dev/null)
  COUNT=$(echo "$STREAMS" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(len(data))
except: print(0)
" 2>/dev/null)
  [ "$COUNT" != "0" ] && [ "$COUNT" != "" ] && break
  sleep 1
done
echo "Recording streams found: $COUNT"
echo "$STREAMS" | python3 -m json.tool 2>/dev/null
```

### Verify recording stream metadata

```bash
curl -s http://localhost:8765/recordings | python3 -c "
import sys, json
data = json.load(sys.stdin)
assert len(data) > 0, 'No recording streams found'
stream = data[0]
name = stream.get('stream_name', '')
print(f'Stream: {name}')
assert name.startswith('display_'), f'Expected display_ stream, got: {name}'

manifest = stream.get('manifest', {})
print(f'Source: {manifest.get(\"source\", \"unknown\")}')
print(f'Codec: {manifest.get(\"codec\", \"unknown\")}')
print(f'FPS: {manifest.get(\"framerate\", \"unknown\")}')
print(f'Resolution: {manifest.get(\"resolution\", \"unknown\")}')
assert manifest.get('source') == 'x11grab', f'Expected x11grab source'
assert manifest.get('codec') == 'h264', f'Expected h264 codec'
print('Stream metadata OK')
"
```

### Wait for first segment to appear

With `segment_duration_secs = 8`, the first segment finalizes after ~8 seconds
of recording. ffmpeg writes to `segments.csv` when a segment completes.

```bash
# Extract stream name first
STREAM=$(curl -s http://localhost:8765/recordings | python3 -c "
import sys, json
data = json.load(sys.stdin)
print(data[0]['stream_name'] if data else '')
" 2>/dev/null)
echo "Waiting for segments on stream: $STREAM"

for i in $(seq 1 30); do
  SEGCOUNT=$(curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(len(data))
except: print(0)
" 2>/dev/null)
  [ "$SEGCOUNT" != "0" ] && [ "$SEGCOUNT" != "" ] && break
  sleep 1
done
echo "Segments found: $SEGCOUNT"
```

### Verify segment metadata

```bash
curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys, json
segments = json.load(sys.stdin)
assert len(segments) > 0, 'No segments found'

seg = segments[0]
print(f'First segment: {seg[\"filename\"]}')
print(f'  Start: {seg[\"start_secs\"]}s')
print(f'  End: {seg[\"end_secs\"]}s')
print(f'  Duration: {seg[\"end_secs\"] - seg[\"start_secs\"]}s')
assert seg['filename'].startswith('seg_'), f'Bad filename: {seg[\"filename\"]}'
assert seg['filename'].endswith('.mp4'), f'Not MP4: {seg[\"filename\"]}'
assert seg['end_secs'] > seg['start_secs'], 'Segment has zero duration'
print(f'Total segments: {len(segments)}')
print('Segment metadata OK')
"
```

### Verify segment file is serveable and valid MP4

```bash
# Get first segment filename
SEG_FILE=$(curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys, json
data = json.load(sys.stdin)
print(data[0]['filename'] if data else '')
" 2>/dev/null)

# Download segment and check it's a valid MP4
curl -s "http://localhost:8765/recordings/$STREAM/$SEG_FILE" -o /tmp/test_segment.mp4
FILE_SIZE=$(stat -c%s /tmp/test_segment.mp4 2>/dev/null || echo 0)
echo "Segment file size: $FILE_SIZE bytes"

# Verify it's a valid MP4 with ffprobe
ffprobe -v quiet -print_format json -show_format -show_streams /tmp/test_segment.mp4 | python3 -c "
import sys, json
data = json.load(sys.stdin)
fmt = data.get('format', {})
streams = data.get('streams', [])
assert len(streams) > 0, 'No streams in MP4'

video = [s for s in streams if s.get('codec_type') == 'video']
assert len(video) > 0, 'No video stream in MP4'

v = video[0]
print(f'Codec: {v.get(\"codec_name\")}')
print(f'Resolution: {v.get(\"width\")}x{v.get(\"height\")}')
print(f'Duration: {fmt.get(\"duration\", \"unknown\")}s')
print(f'Format: {fmt.get(\"format_name\")}')
assert v.get('codec_name') == 'h264', f'Expected h264, got {v.get(\"codec_name\")}'
print('Segment file valid MP4 OK')
"
```

### Verify path traversal protection

```bash
# These should return 400 or 404, not serve files
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:8765/recordings/$STREAM/../../../etc/passwd")
echo "Path traversal attempt: HTTP $STATUS"
[ "$STATUS" = "400" ] || [ "$STATUS" = "404" ] && echo "Path traversal blocked OK" || echo "WARN: unexpected status"

STATUS=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:8765/recordings/$STREAM/notaseg.mp4")
echo "Invalid filename: HTTP $STATUS"
[ "$STATUS" = "400" ] && echo "Invalid filename rejected OK" || echo "WARN: unexpected status"
```

### Verify multiple segments accumulate over time

Wait for at least 2 segments (requires ~16s with segment_duration_secs=8):

```bash
for i in $(seq 1 30); do
  SEGCOUNT=$(curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys, json
try: print(len(json.load(sys.stdin)))
except: print(0)
" 2>/dev/null)
  [ "$SEGCOUNT" -ge 2 ] 2>/dev/null && break
  sleep 2
done
echo "Segments after waiting: $SEGCOUNT"

curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys, json
segments = json.load(sys.stdin)
assert len(segments) >= 2, f'Expected >= 2 segments, got {len(segments)}'
# Verify segments are contiguous
for i in range(1, len(segments)):
    gap = abs(segments[i]['start_secs'] - segments[i-1]['end_secs'])
    assert gap < 1.0, f'Gap between segments {i-1} and {i}: {gap}s'
print(f'{len(segments)} contiguous segments OK')

# Verify total duration makes sense
total = segments[-1]['end_secs']
print(f'Total recorded duration: {total:.1f}s')
"
```

## Asserting on Replay UI via Firefox

### Verify recording section is visible in browser

Use `ff-eval.py` if Firefox debugger is active, or use xdotool to navigate.

If Firefox was launched with `--start-debugger-server 6000`:
```bash
# First set up Firefox debugger (if not already done)
PROFILE=$(ls -d ~/.mozilla/firefox/*.default* | head -1)
grep -q 'devtools.debugger.remote-enabled' "$PROFILE/user.js" 2>/dev/null || \
cat >> "$PROFILE/user.js" << 'DEOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
DEOF
```

Then relaunch Firefox with debugger:
```bash
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
sleep 1
rm -f ~/.mozilla/firefox/*/.parentlock ~/.mozilla/firefox/*/lock 2>/dev/null
DISPLAY=:50 nohup firefox -P default --start-debugger-server 6000 \
  --new-window http://localhost:8765/app > /dev/null 2>&1 &
sleep 8
```

Check recording UI state via JavaScript:
```bash
# Switch to Displays tab
python3 scripts/ff-eval.py "document.querySelector('[data-tab=\"displays\"]')?.click(); 'clicked'"
sleep 1

# Check recording section visibility
python3 scripts/ff-eval.py "
  const section = document.getElementById('recording-section');
  const hidden = section?.classList.contains('hidden');
  const select = document.getElementById('recording-stream-select');
  const options = select ? select.options.length : 0;
  JSON.stringify({visible: !hidden, streamCount: options})
"
# Expected: {"visible":true,"streamCount":1} (or more)
```

### Verify RecordingPlayer loaded segments

```bash
python3 scripts/ff-eval.py "
  const player = window.recPlayer;
  if (!player) 'no player';
  else JSON.stringify({
    streamName: player.streamName,
    segmentCount: player.segments.length,
    totalDuration: player.totalDuration,
    currentSegIdx: player.currentSegIdx,
    playing: player.playing
  })
"
# Expected: segments > 0, totalDuration > 0
```

### Test playback controls

```bash
# Start playback
python3 scripts/ff-eval.py "
  const btn = document.getElementById('rec-play-btn');
  btn?.click();
  'play clicked'
"
sleep 2

# Check if playing
python3 scripts/ff-eval.py "
  const player = window.recPlayer;
  JSON.stringify({
    playing: player?.playing,
    currentTime: player?.globalTime(),
    videoReadyState: player?.video?.readyState
  })
"
# Expected: playing: true, currentTime > 0

# Pause
python3 scripts/ff-eval.py "
  document.getElementById('rec-play-btn')?.click();
  'pause clicked'
"
```

### Test speed control

```bash
python3 scripts/ff-eval.py "
  const select = document.getElementById('rec-speed');
  select.value = '4';
  select.dispatchEvent(new Event('change'));
  'speed set to 4x'
"
sleep 1
python3 scripts/ff-eval.py "window.recPlayer?.video?.playbackRate"
# Expected: 4
```

### Test timeline seeking

```bash
# Seek to middle of recording
python3 scripts/ff-eval.py "
  const player = window.recPlayer;
  const mid = player.totalDuration / 2;
  player.seekToGlobal(mid);
  JSON.stringify({seekedTo: mid, currentTime: player.globalTime()})
"
# Expected: currentTime near seekedTo value
```

## WebSocket Event Verification

Connect via WebSocket and verify recording events are broadcast:

```bash
python3 -c "
import asyncio, json, websockets

async def check_events():
    async with websockets.connect('ws://localhost:8765') as ws:
        events_seen = set()
        for _ in range(50):  # Read up to 50 messages
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=2)
                data = json.loads(msg)
                event = data.get('event', '')
                if 'recording' in event.lower():
                    events_seen.add(event)
                    print(f'Recording event: {event} — {json.dumps(data)}')
            except asyncio.TimeoutError:
                break
            except: pass
        print(f'Recording events seen: {events_seen}')

asyncio.run(check_events())
" 2>/dev/null
```

**Note:** `recording_started` events may have already been broadcast before the
WebSocket connects. Use `/recordings` endpoint for reliable state checks.

## Example Full Test Scenario

```bash
# ── Setup ──
TESTDIR=$(mktemp -d /tmp/intendant-rec-test-XXXXXX)
cat > "$TESTDIR/intendant.toml" << 'EOF'
[recording]
enabled = true
framerate = 10
segment_duration_secs = 8
quality = "low"
EOF

# Kill stale
for p in $(pgrep -x Xvfb 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x x11vnc 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
for p in $(pgrep -x intendant 2>/dev/null); do kill "$p" 2>/dev/null; done
sleep 0.5

# Start observer Xvfb
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &

# Launch intendant
> /tmp/intendant-rec-stderr.log
cd "$TESTDIR" && source /home/user/projects/intendant/.env && \
  nohup /home/user/projects/intendant/target/release/intendant \
    --direct --autonomy high --web \
    "run 'xeyes' in the background, then wait 30 seconds" \
    > /dev/null 2>/tmp/intendant-rec-stderr.log &
sleep 3
cat /tmp/intendant-rec-stderr.log

# ── Assert 1: Recording stream exists ──
for i in $(seq 1 60); do
  COUNT=$(curl -s http://localhost:8765/recordings 2>/dev/null | python3 -c "
import sys,json
try: print(len(json.load(sys.stdin)))
except: print(0)" 2>/dev/null)
  [ "$COUNT" != "0" ] && [ "$COUNT" != "" ] && break; sleep 1
done
echo "ASSERT 1: Streams found: $COUNT"

# ── Assert 2: Manifest has correct metadata ──
curl -s http://localhost:8765/recordings | python3 -c "
import sys, json
data = json.load(sys.stdin)
assert len(data) > 0, 'FAIL: No streams'
s = data[0]
assert s['manifest']['source'] == 'x11grab'
assert s['manifest']['codec'] == 'h264'
print(f'ASSERT 2 PASS: {s[\"stream_name\"]} — {s[\"manifest\"][\"source\"]}, {s[\"manifest\"][\"codec\"]}')"

# ── Assert 3: Wait for segments ──
STREAM=$(curl -s http://localhost:8765/recordings | python3 -c "
import sys,json; print(json.load(sys.stdin)[0]['stream_name'])" 2>/dev/null)

for i in $(seq 1 45); do
  SEGCOUNT=$(curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys,json
try: print(len(json.load(sys.stdin)))
except: print(0)" 2>/dev/null)
  [ "$SEGCOUNT" -ge 1 ] 2>/dev/null && break; sleep 1
done
echo "ASSERT 3: Segments: $SEGCOUNT"

# ── Assert 4: Segment is valid MP4 ──
SEG_FILE=$(curl -s "http://localhost:8765/recordings/$STREAM/segments" | python3 -c "
import sys,json; print(json.load(sys.stdin)[0]['filename'])" 2>/dev/null)
curl -s "http://localhost:8765/recordings/$STREAM/$SEG_FILE" -o /tmp/test_seg.mp4
ffprobe -v quiet -print_format json -show_streams /tmp/test_seg.mp4 | python3 -c "
import sys, json
data = json.load(sys.stdin)
video = [s for s in data['streams'] if s['codec_type']=='video']
assert len(video) > 0, 'FAIL: No video stream'
assert video[0]['codec_name'] == 'h264', 'FAIL: Not h264'
print(f'ASSERT 4 PASS: {video[0][\"codec_name\"]} {video[0][\"width\"]}x{video[0][\"height\"]}')"

# ── Assert 5: Path traversal blocked ──
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:8765/recordings/$STREAM/../../../etc/passwd")
echo "ASSERT 5: Path traversal HTTP $STATUS (expect 400 or 404)"

# ── Cleanup ──
for p in $(pgrep -x intendant 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
for p in $(pgrep -x Xvfb 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x x11vnc 2>/dev/null); do kill "$p" 2>/dev/null; done
rm -rf "$TESTDIR" /tmp/test_seg.mp4
echo "Done"
```

## Troubleshooting

### No recording streams appear

1. **Check intendant.toml**: Recording must be `enabled = true`. Verify intendant
   found the config:
   ```bash
   cat /tmp/intendant-rec-stderr.log | grep -i record
   ```
2. **Check ffmpeg**: `ffmpeg -version` must succeed. libx264 and x11grab required.
3. **Check Xvfb auto-launch**: The agent must trigger a command that needs a display.
   If the task doesn't involve GUI commands, no Xvfb launches, no recording starts.
4. **Check DisplayReady event**: Connect via WebSocket and look for `display_ready`:
   ```bash
   python3 -c "
   import asyncio, json, websockets
   async def watch():
       async with websockets.connect('ws://localhost:8765') as ws:
           for _ in range(100):
               msg = await asyncio.wait_for(ws.recv(), timeout=30)
               d = json.loads(msg)
               if d.get('event') in ('display_ready','recording_started','recording_error'):
                   print(json.dumps(d, indent=2))
   asyncio.run(watch())
   " 2>/dev/null
   ```

### Segments never appear (recording started but no segments.csv)

- ffmpeg only writes to `segments.csv` when a segment **completes** (i.e., after
  `segment_duration_secs` of recording). With default 60s, you'd wait a full minute.
  Use `segment_duration_secs = 8` for testing.
- Check if ffmpeg is actually running:
  ```bash
  pgrep -fa 'ffmpeg.*x11grab'
  ```
- Check the session recording directory directly:
  ```bash
  find ~/.intendant/logs/ -path '*/recordings/*' -name '*.mp4' -ls 2>/dev/null | tail -5
  ```

### Segment serves but ffprobe fails

- The segment may still be in progress (partial write). Wait for the segment to
  finalize (next segment starts, or recording stops).
- Verify with: `ffprobe -v error /tmp/test_seg.mp4`

### RecordingPlayer shows 0 segments in browser

- The player fetches from `/recordings/{stream}/segments`. If this returns `[]`,
  segments haven't been finalized yet.
- The player refreshes every 5 seconds for active recordings. Wait and reload.
- Check browser console for fetch errors (CORS, 404).

### Firefox can't play MP4 segments

- Firefox ESR may lack H.264 support on some Linux distros. Install:
  ```bash
  sudo apt-get install -y libavcodec-extra
  ```
- Check browser console for "media resource could not be decoded" errors.

## Cleanup

```bash
for p in $(pgrep -x intendant 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x firefox -x firefox-esr 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
for p in $(pgrep -x Xvfb 2>/dev/null); do kill "$p" 2>/dev/null; done
for p in $(pgrep -x x11vnc 2>/dev/null); do kill "$p" 2>/dev/null; done
rm -rf /tmp/intendant-rec-test-* /tmp/test_seg.mp4 /tmp/test_segment.mp4
```
