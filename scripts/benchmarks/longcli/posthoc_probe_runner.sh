#!/usr/bin/env bash
# Post-hoc recall-probe runner for LongCLI pilot trials (both lane kinds).
#
# Managed lanes: validated 2026-06-12 on the managed smoke trial —
#   1. copy the trial's agent-logs (originals stay pristine; probes run on
#      the copy),
#   2. fresh auth.json into the copy's codex-home,
#   3. fresh debian:12 container with the LANE's intendant + the codex fork,
#   4. `intendant --resume /agent-logs/intendant --task-file <READY handshake>`
#      — the startup resume consumes the persisted external identity from the
#      archived session.jsonl (session_identity event) and the first agent
#      build resumes the archived codex thread (same rollout file grows),
#   5. wait for the handshake task_complete, parse the live web port from the
#      console log, then inject_probes.py managed over the gateway WS.
# Vanilla lanes: codex exec resume against a copy of the archived codex-home
#   (inject_probes.py vanilla), same pristine-original rule.
#
# Usage:
#   posthoc_probe_runner.sh managed <trial-agent-logs-dir> <intendant-binary> <probes.json> <work-dir>
#   posthoc_probe_runner.sh vanilla <trial-agent-logs-dir> <codex-bin> <probes.json> <work-dir>
#
# Outputs in <work-dir>: agent-logs/ (the copy), probe_answers.json,
# probe-console.log (managed). Grading runs separately against the ORIGINAL
# trial archives.
set -uo pipefail

MODE="${1:?mode: managed|vanilla}"
TRIAL_LOGS="${2:?trial agent-logs dir}"
BIN_ARG="${3:?intendant binary (managed) or codex bin (vanilla)}"
PROBES="${4:?probes json}"
WORK="${5:?work dir}"

BENCH_BIN=/home/user/projects/bench-binaries-20260611
VENV_PY=/home/user/longcli-venv/bin/python
AGENTS=/home/user/longcli-agents
AUTH_SRC=/home/user/.codex/auth.json

[ -d "$TRIAL_LOGS" ] || { echo "no such trial agent-logs: $TRIAL_LOGS" >&2; exit 2; }
[ -f "$PROBES" ] || { echo "no such probes file: $PROBES" >&2; exit 2; }

mkdir -p "$WORK"
# Copy agent-logs with root-owned files handled (bind-mounted files are often
# container-root); docker does the copy as root then chowns to the harness uid.
rm -rf "$WORK/agent-logs"
docker run --rm -v "$TRIAL_LOGS":/src:ro -v "$WORK":/dst debian:12 \
  bash -c "cp -a /src /dst/agent-logs && chown -R $(id -u):$(id -g) /dst/agent-logs" \
  || { echo "agent-logs copy failed" >&2; exit 3; }
install -m 600 "$AUTH_SRC" "$WORK/agent-logs/codex-home/auth.json"

if [ "$MODE" = vanilla ]; then
  "$VENV_PY" "$AGENTS/inject_probes.py" vanilla \
    --codex-home "$WORK/agent-logs/codex-home" \
    --codex-bin "$BIN_ARG" \
    --probes "$PROBES" \
    --turn-timeout 600 \
    --out "$WORK/probe_answers.json"
  exit $?
fi

# ---- managed ----
INTENDANT_BIN="$BIN_ARG"
[ -x "$INTENDANT_BIN" ] || { echo "no such intendant binary: $INTENDANT_BIN" >&2; exit 2; }
NAME="probe-$(basename "$WORK" | tr -cs 'a-zA-Z0-9' '-' | cut -c1-40)$$"

cat > "$WORK/intendant.toml" << 'TOML'
[agent]
default_backend = "codex"

[agent.codex]
command = "/usr/local/bin/codex"
managed_command = "/usr/local/bin/codex"
managed_context = "managed"
context_archive = "exact"
model = "gpt-5.5"
approval_policy = "never"
sandbox = "danger-full-access"
network_access = true
web_search = false
reasoning_effort = "xhigh"
TOML

cat > "$WORK/probe-task.txt" << 'T'
Session resumed for a post-task recall review. Reply with exactly: READY. Do not run any commands or edit any files.
T
cp "$WORK/probe-task.txt" "$WORK/agent-logs/probe-task.txt"

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" \
  -v "$WORK/agent-logs":/agent-logs \
  -v "$BENCH_BIN/codex":/usr/local/bin/codex:ro \
  -v "$INTENDANT_BIN":/usr/local/bin/intendant:ro \
  -v "$WORK/intendant.toml":/app/intendant.toml \
  -w /app debian:12 sleep 7200 >/dev/null || exit 3

docker exec "$NAME" bash -c '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends \
  ca-certificates curl ripgrep libzstd1 zlib1g libssl3 libvpx7 \
  libpipewire-0.3-0 libxcb1 libxcb-shm0 libxcb-randr0 >/dev/null
codex --version >/dev/null && intendant --help >/dev/null' \
  || { echo "container dep install failed" >&2; exit 3; }

BASE_COMPLETES=$(cat "$WORK"/agent-logs/codex-home/sessions/*/*/*/rollout-*.jsonl 2>/dev/null | grep -c '"type":"task_complete"')

docker exec -d "$NAME" bash -c '
export CODEX_HOME=/agent-logs/codex-home NO_COLOR=1 TERM=dumb
cd /app
intendant --no-tls --bind 0.0.0.0 --web 8901 --no-tui --no-presence \
  --agent codex --resume /agent-logs/intendant \
  --task-file /agent-logs/probe-task.txt \
  > /agent-logs/probe-console.log 2>&1 </dev/null'

# Wait for the READY handshake turn to complete (counts grow by >= 1).
ok=""
for _ in $(seq 1 120); do
  total=$(cat "$WORK"/agent-logs/codex-home/sessions/*/*/*/rollout-*.jsonl 2>/dev/null | grep -c '"type":"task_complete"')
  if [ "$total" -gt "$BASE_COMPLETES" ]; then ok=1; break; fi
  sleep 5
done
if [ -z "$ok" ]; then
  echo "handshake turn did not complete; console tail:" >&2
  tail -20 "$WORK/agent-logs/probe-console.log" >&2 || true
  exit 4
fi

PORT=$(grep -oE 'Web TUI: http://[0-9.]+:([0-9]+)' "$WORK/agent-logs/probe-console.log" | grep -oE '[0-9]+$' | tail -1)
[ -n "$PORT" ] || { echo "no web port in console log" >&2; exit 4; }
IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$NAME")
[ -n "$IP" ] || { echo "no container IP" >&2; exit 4; }
echo "gateway: ws://$IP:$PORT/ws (handshake complete)"

"$VENV_PY" "$AGENTS/inject_probes.py" managed \
  --gateway "ws://$IP:$PORT/ws" \
  --codex-home "$WORK/agent-logs/codex-home" \
  --console-log "$WORK/agent-logs/probe-console.log" \
  --probes "$PROBES" \
  --turn-timeout 600 \
  --out "$WORK/probe_answers.json"
rc=$?
docker exec "$NAME" bash -c 'pkill -f "intendant --no-tls" 2>/dev/null; true'
exit $rc
