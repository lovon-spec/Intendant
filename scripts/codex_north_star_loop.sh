#!/usr/bin/env bash
set -euo pipefail

ROOT="/home/user/projects/intendant-codex-fork"
LOG_DIR="${ROOT}/.intendant/controller-loop"
RUN_TS="$(date -u +"%Y%m%dT%H%M%SZ")"
RUN_ID="${RUN_TS}-$$"
RUN_DIR="${LOG_DIR}/${RUN_ID}"
OUT_FILE="${RUN_DIR}/codex.jsonl"
STATUS_FILE="${RUN_DIR}/status.json"
SUMMARY_FILE="${RUN_DIR}/summary.json"
HEARTBEAT_FILE="${RUN_DIR}/heartbeat.txt"
LATEST_LINK="${LOG_DIR}/latest"
LATEST_PID_FILE="${LOG_DIR}/latest.pid"
LATEST_OUT_FILE="${LOG_DIR}/latest.jsonl"
LATEST_STATUS_FILE="${LOG_DIR}/latest.status.json"
LATEST_RUN_ID_FILE="${LOG_DIR}/latest.run_id"
CODEX_PID_FILE="${RUN_DIR}/codex.pid"
INTERVENTION_LOG="${RUN_DIR}/intervention.log"
STOP_FILE="${LOG_DIR}/request_stop"
ABORT_FILE="${LOG_DIR}/request_abort"

HB_PID=""
CODEX_PID=""
CONTROL_PID=""
FINALIZED="0"
STARTED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

log_intervention() {
  printf '%s %s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" "$*" >> "$INTERVENTION_LOG"
}

capture_signal_diagnostics() {
  local sig="$1"
  local self_meta parent_meta
  self_meta="$(ps -o pid=,ppid=,pgid=,sid=,tty=,stat=,etime=,cmd= -p "$$" 2>/dev/null | sed 's/^ *//')"
  parent_meta="$(ps -o pid=,ppid=,pgid=,sid=,tty=,stat=,etime=,cmd= -p "$PPID" 2>/dev/null | sed 's/^ *//')"
  log_intervention "signal_received=$sig self=[$self_meta] parent=[$parent_meta] codex_pid=${CODEX_PID:-unset}"
  if [[ -n "${CODEX_PID:-}" ]]; then
    local codex_meta
    codex_meta="$(ps -o pid=,ppid=,pgid=,sid=,tty=,stat=,etime=,cmd= -p "$CODEX_PID" 2>/dev/null | sed 's/^ *//')"
    log_intervention "signal_context_codex=[$codex_meta]"
  fi
}

write_status() {
  local state="$1"
  local exit_code="$2"
  local reason="${3:-}"
  local finished_at
  finished_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"run_id":"%s","state":"%s","pid":%s,"codex_pid":"%s","exit_code":%s,"started_at":"%s","finished_at":"%s","reason":"%s","output":"%s"}\n' \
    "$RUN_ID" "$state" "$$" "${CODEX_PID:-}" "$exit_code" "$STARTED_AT" "$finished_at" "$reason" "$OUT_FILE" > "$STATUS_FILE"
  cp "$STATUS_FILE" "$LATEST_STATUS_FILE"
  printf '{"run_id":"%s","state":"%s","exit_code":%s,"finished_at":"%s"}\n' \
    "$RUN_ID" "$state" "$exit_code" "$finished_at" > "$SUMMARY_FILE"
}

cleanup() {
  local state="$1"
  local exit_code="$2"
  local reason="${3:-}"
  if [[ "$FINALIZED" == "1" ]]; then
    return
  fi
  FINALIZED="1"
  log_intervention "cleanup_begin state=$state exit_code=$exit_code reason=${reason:-none} codex_pid=${CODEX_PID:-unset}"
  if [[ -n "$HB_PID" ]]; then
    kill "$HB_PID" >/dev/null 2>&1 || true
    wait "$HB_PID" 2>/dev/null || true
  fi
  if [[ -n "$CONTROL_PID" ]]; then
    kill "$CONTROL_PID" >/dev/null 2>&1 || true
    wait "$CONTROL_PID" 2>/dev/null || true
  fi
  if [[ -n "$CODEX_PID" ]]; then
    if kill -0 "$CODEX_PID" >/dev/null 2>&1; then
      kill -TERM "$CODEX_PID" >/dev/null 2>&1 || true
      for _ in 1 2 3 4 5; do
        if ! kill -0 "$CODEX_PID" >/dev/null 2>&1; then
          break
        fi
        sleep 1
      done
      if kill -0 "$CODEX_PID" >/dev/null 2>&1; then
        kill -KILL "$CODEX_PID" >/dev/null 2>&1 || true
      fi
    fi
    wait "$CODEX_PID" 2>/dev/null || true
  fi
  write_status "$state" "$exit_code" "$reason"
  log_intervention "cleanup_end state=$state exit_code=$exit_code reason=${reason:-none}"
}

on_signal() {
  local sig="$1"
  capture_signal_diagnostics "$sig"
  cleanup "signaled" 143 "$sig"
  exit 143
}

read -r -d '' PROMPT <<'EOF' || true
North star: recursively improve intendant toward state-of-the-art CLI/TUI/MCP controller behavior.

Execution policy:
- Complete one concrete improvement per cycle.
- Include tests and docs updates for each improvement.
- Keep changes incremental and shippable.
- Run intendant E2E tests each cycle before handoff.
- If E2E or regression tests fail, fix the bugs in the same cycle before scheduling restart.
- The repository may already contain uncommitted changes from prior loop cycles; treat those as expected baseline context, not as unexpected external edits.
- Do not stop only because `git status` is dirty at turn start; continue from current workspace state.
- Commit each completed cycle before restart handshake.
- Use one commit per cycle with message format: `loop: <short summary> [run <YYYYMMDDTHHMMSSZ>]`.
- Do not amend prior commits.
- Do not push unless explicitly requested by the user.
- Before restart handshake, ensure there are no staged/unstaged tracked changes left (`git status --porcelain --untracked-files=no` should be empty).

Controller recursion policy:
- Near turn end, call intendant MCP tool schedule_controller_restart with:
  - controller_id: "codex"
  - north_star_goal: this same north-star objective
  - restart_after: "turn_end"
  - auto_start_task: false
  - restart_command: "bash /home/user/projects/intendant-codex-fork/scripts/codex_north_star_loop.sh"
- Then call controller_turn_complete as the final controller action.
- Do not use start_task for normal work loops (only explicit E2E testing).
EOF

mkdir -p "$RUN_DIR"
ln -sfn "$RUN_DIR" "$LATEST_LINK"
printf '%s\n' "$$" > "$LATEST_PID_FILE"
printf '%s\n' "$OUT_FILE" > "$LATEST_OUT_FILE"
printf '%s\n' "$RUN_ID" > "$LATEST_RUN_ID_FILE"
# Clear stale operator intervention requests from prior runs.
rm -f "$STOP_FILE" "$ABORT_FILE"
write_status "starting" -1 ""
log_intervention "run_started run_id=$RUN_ID pid=$$ ppid=$PPID"

cd "$ROOT"
(
  while true; do
    date -u +"%Y-%m-%dT%H:%M:%SZ heartbeat pid=$$" > "$HEARTBEAT_FILE"
    sleep 15
  done
) &
HB_PID=$!

(
  while true; do
    current_pid=""
    if [[ -f "$CODEX_PID_FILE" ]]; then
      current_pid="$(cat "$CODEX_PID_FILE" 2>/dev/null || true)"
    elif [[ -n "$CODEX_PID" ]]; then
      current_pid="$CODEX_PID"
    fi
    if [[ -n "$current_pid" ]] && kill -0 "$current_pid" >/dev/null 2>&1; then
      if [[ -f "$STOP_FILE" ]]; then
        log_intervention "operator_request=stop codex_pid=$current_pid"
        rm -f "$STOP_FILE"
        kill -TERM "$current_pid" >/dev/null 2>&1 || true
      fi
      if [[ -f "$ABORT_FILE" ]]; then
        log_intervention "operator_request=abort codex_pid=$current_pid"
        rm -f "$ABORT_FILE"
        kill -KILL "$current_pid" >/dev/null 2>&1 || true
      fi
    fi
    sleep 2
  done
) &
CONTROL_PID=$!

trap 'on_signal TERM' TERM
trap 'on_signal INT' INT
trap 'on_signal HUP' HUP
trap 'on_signal QUIT' QUIT

set +e
codex exec \
  --cd "$ROOT" \
  --sandbox workspace-write \
  --full-auto \
  --json \
  "$PROMPT" >> "$OUT_FILE" 2>&1 &
CODEX_PID="$!"
printf '%s\n' "$CODEX_PID" > "$CODEX_PID_FILE"
log_intervention "codex_started codex_pid=$CODEX_PID"
wait "$CODEX_PID"
EXIT_CODE=$?
set -e

cleanup "exited" "$EXIT_CODE" ""
exit "$EXIT_CODE"
