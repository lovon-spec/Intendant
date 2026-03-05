---
name: tui-e2e
description: >
  E2E test the intendant TUI on a virtual display. Launches Xvfb, runs the
  TUI in xterm, takes screenshots, and controls it via the Unix socket.
compatibility: Requires Xvfb, xterm, ImageMagick (import), socat, x11vnc
allowed-tools: Bash Read
disable-model-invocation: true
---

# Test TUI E2E

Install prerequisites: `sudo apt-get install -y socat x11vnc`

## Launch

**IMPORTANT:** Always use display **:50** (intendant reserves :99+ for its own Xvfb).
Always start `x11vnc` so the human can follow along via VNC on port 5950.
Both Xvfb and x11vnc MUST be started before launching xterm.

```bash
# 1. Kill stale processes from prior runs
pkill -f 'Xvfb :50' 2>/dev/null; pkill -f 'x11vnc.*:50' 2>/dev/null
pkill -f 'intendant.*control-socket' 2>/dev/null; sleep 0.5

# 2. Start Xvfb + x11vnc (MANDATORY — human needs VNC to observe)
Xvfb :50 -screen 0 1280x720x24 &
sleep 0.5
x11vnc -display :50 -rfbport 5950 -nopw -forever -quiet &
sleep 0.5

# 3. Launch intendant in xterm on display :50
# NOTE: 100x30 fits inside 1280x720. Larger geometries (e.g. 120x35 at fs 12)
# overflow the screen and clip the bottom panel rows.
DISPLAY=:50 xterm -geometry 100x30 -fa Monospace -fs 12 \
  -e bash -c 'source .env && ./target/release/intendant \
    --direct --autonomy low --control-socket \
    "your task" 2>/tmp/intendant-tui-stderr.log; sleep 120' &
```

## Screenshot

Always use `DISPLAY=:50`:

```bash
DISPLAY=:50 import -window root /tmp/tui-screenshot.png
```

## Control socket

Path: `/tmp/intendant-<PID>.sock` (find PID with `pgrep -a intendant`).

```bash
echo '{"action":"status"}' | socat - UNIX-CONNECT:/tmp/intendant-<PID>.sock
echo '{"action":"approve","id":<TURN>}' | socat - UNIX-CONNECT:/tmp/intendant-<PID>.sock
```

Actions:

| Action | Fields | Notes |
|--------|--------|-------|
| `status` | — | Returns turn, phase, autonomy, session_id, task |
| `usage` | — | Returns per-model token usage |
| `approve` | `id` (turn) | Approve pending action |
| `deny` | `id` (turn) | Deny pending action |
| `skip` | `id` (turn) | Skip pending action |
| `approve_all` | `id` (turn) | Approve all pending actions |
| `set_autonomy` | `level` | low/medium/high/full |
| `set_verbosity` | `level` | quiet/normal/verbose/debug |
| `input` | `text` | Reply to askHuman prompt only |
| `follow_up` | `text` | Send follow-up after round completes |
| `start_task` | `task`, `orchestrate`? | Start a new task (optional orchestrator mode) |
| `query_detail` | `scope`, `target`? | Query detail about a scope |
| `recall_memory` | `keywords`?, `tags`?, `channel`? | Recall stored knowledge |
| `quit` | — | End session |

## Cleanup

```bash
pkill -f 'intendant.*control-socket'; pkill -f 'Xvfb :50'; pkill -f 'x11vnc.*:50'
```
