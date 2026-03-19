---
name: tui-e2e
description: >
  E2E test the intendant TUI on a virtual display. Launches Xvfb, runs the
  TUI in xterm, and controls/asserts via the Unix control socket JSON protocol.
  Human monitors via VNC on port 5950.
compatibility: Requires Xvfb, xterm, socat, x11vnc
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
# 1. Kill stale processes from prior runs (use -9 for firefox — it ignores SIGTERM)
pkill -f 'Xvfb :50' 2>/dev/null
pkill -f 'x11vnc.*:50' 2>/dev/null
pkill -f 'intendant.*control-socket' 2>/dev/null
sleep 0.5

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

## Asserting on State (instead of screenshots)

Use the control socket to verify behavior programmatically. Every assertion
reads structured JSON — no vision model needed.

### Check phase and status
```bash
SOCK=/tmp/intendant-$(pgrep -f 'intendant.*control-socket' | head -1).sock

# Get current state
RESULT=$(echo '{"action":"status"}' | socat - UNIX-CONNECT:$SOCK 2>/dev/null)
echo "$RESULT" | python3 -m json.tool

# Assert phase
echo "$RESULT" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
phase = d.get('phase', '')
print(f'Phase: {phase}')
assert phase in ('Thinking', 'RunningAgent', 'WaitingApproval', 'Idle', 'Done'), f'Unexpected phase: {phase}'
"
```

### Wait for a specific phase
```bash
# Poll until approval is pending (up to 30s)
for i in $(seq 1 30); do
  RESULT=$(echo '{"action":"status"}' | socat - UNIX-CONNECT:$SOCK 2>/dev/null)
  PHASE=$(echo "$RESULT" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('phase',''))" 2>/dev/null)
  [ "$PHASE" = "WaitingApproval" ] && break
  sleep 1
done
echo "Phase reached: $PHASE"
```

### Approve and verify execution
```bash
# Get the turn number from status
TURN=$(echo '{"action":"status"}' | socat - UNIX-CONNECT:$SOCK 2>/dev/null | \
  python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('turn',0))")

# Approve
echo "{\"action\":\"approve\",\"id\":$TURN}" | socat - UNIX-CONNECT:$SOCK

# Wait for phase to leave WaitingApproval
sleep 2
RESULT=$(echo '{"action":"status"}' | socat - UNIX-CONNECT:$SOCK 2>/dev/null)
echo "$RESULT" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
assert d.get('phase') != 'WaitingApproval', 'Still waiting for approval after approve sent'
print(f'Approved — now in phase: {d.get(\"phase\")}')
"
```

### Check token usage
```bash
USAGE=$(echo '{"action":"usage"}' | socat - UNIX-CONNECT:$SOCK 2>/dev/null)
echo "$USAGE" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
main = d.get('main', {})
print(f'Provider: {main.get(\"provider\")}, Model: {main.get(\"model\")}')
print(f'Tokens: {main.get(\"tokens_used\")}/{main.get(\"context_window\")} ({main.get(\"usage_pct\",0):.1f}%)')
"
```

### Listen for events (streaming)
```bash
# Open a persistent connection and read events as they arrive
socat - UNIX-CONNECT:$SOCK &
SOCAT_PID=$!
# Events stream as JSON lines: {"event":"approval_required","id":2,...}
# Kill with: kill $SOCAT_PID
```

## Screenshot (optional — for human VNC verification only)

If you want a visual snapshot for the VNC viewer:

```bash
DISPLAY=:50 import -window root /tmp/tui-screenshot.png
```

This is **not needed for assertions** — use the control socket instead.

## Cleanup

```bash
pkill -f 'intendant.*control-socket' 2>/dev/null
pkill -f 'Xvfb :50' 2>/dev/null
pkill -f 'x11vnc.*:50' 2>/dev/null
```
