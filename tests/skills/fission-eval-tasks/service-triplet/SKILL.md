---
name: service-triplet-fission-eval
description: >
  Fission-shaped evaluation runner. Launches a managed Codex session on the
  jobline service quartet (a REST API job store, a worker, a CLI client, and a
  read-only metrics service — all Python stdlib, in disjoint directories),
  then scores the result behaviorally with verify.sh (per-component partial
  credit + a live integration bonus, max 5.0) and records whether the model
  spontaneously used model-driven fission. The four components own disjoint
  write scopes and share only the HTTP protocol, so fission into chartered
  worktree branches is a genuine win — but it is never required and never
  mentioned to the agent. Whether it fissions is the measurement.
compatibility: >
  Requires OpenAI Codex auth in ~/.codex/auth.json, the patched Codex fork at
  /Users/vm/projects/codex-minimal-lineage, a managed-build intendant binary,
  and python3 + jq in PATH (no other toolchain). Daemon/dashboard mode (do NOT
  pass --no-web). Real model calls; not for CI.
allowed-tools: Bash Read
disable-model-invocation: false
---

# service-triplet — fission-shaped eval runner

## Purpose

Same measurement goal as the polyglot-pipeline runner, with an all-Python
(no-toolchain) quartet: hand a managed Codex agent a four-service repo whose
components own disjoint write scopes (`api/`, `worker/`, `cli/`, `metrics/`)
and see whether it **chooses** model-driven fission (`fission_spawn` into
chartered worktree branches) — scoring the work behaviorally either way.
Fission is never required and never mentioned. Sized for ~30-60 min of serial
work.

## What it measures

- **Behavioral score** (`verify.sh` → JSON): `component_scores` for `api`
  (driven over raw HTTP: lifecycle, requeue/delete, listing/pagination, bulk
  perf), `worker` (pure `compute` vs an independent oracle across 13 ops,
  incl. perf budgets), `cli` (run against a conforming reference server), and
  `metrics` (against the reference server, incl. freshness + API-down
  checks), plus an `integration` bonus that starts the agent's API + worker +
  metrics on random ports and drives generated jobs end-to-end through the
  agent's CLI. `total` is out of 5.0; partial credit per component.
- **Did it fission?** Presence/shape of a group in the parent's
  `fission_ledger.json` (branch count, charters, write scopes, terminal
  status, import/claim). Zero fission is valid data.
- **Lineage/concurrency value:** wall-clock to a passing `total`; per-branch
  component scores when work lands in branch worktrees before import (score
  with `verify.sh <branch-worktree>`).

## Setup

```bash
# 1. Patched Codex (minimal-lineage fork)
cd /Users/vm/projects/codex-minimal-lineage/codex-rs && cargo build -p codex-cli --bin codex

# 2. Managed-context intendant build (your own worktree, NOT the shared main).
cd /path/to/your/intendant/worktree && cargo build --release --bin intendant

# 3. Throwaway task repo from skeleton/ ONLY (never copy verify/ or reference/).
SKILL_DIR=/Users/vm/projects/intendant/.worktrees/fission-tasks/tests/skills/fission-eval-tasks/service-triplet
REPO=$(mktemp -d /tmp/triplet-eval.XXXX)
cp -R "$SKILL_DIR"/skeleton/. "$REPO"/
cp "$SKILL_DIR"/TASK.md "$REPO"/TASK.md
git -C "$REPO" init -q && git -C "$REPO" add -A
git -C "$REPO" -c user.email=eval@local -c user.name=eval commit -qm 'jobline skeleton'

# 4. Per-project managed Codex config.
cat > "$REPO/intendant.toml" <<'EOF'
[agent.codex]
command = "/Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex"
managed_context = "managed"
approval_policy = "never"
sandbox = "workspace-write"
EOF
```

## Run

Pick a free port (never the shared 8765). Launch from the repo with the
dashboard ON. Feed the prompt verbatim from `TASK.md` — **neutral wording; no
mention of fission, branching, or parallelism.**

```bash
PORT=18941
cd "$REPO"
/path/to/your/intendant/worktree/target/release/intendant --agent codex \
  --web $PORT --no-tls --bind 127.0.0.1 "$(cat TASK.md)"
```

A competent serial agent finishes in ~30-60 min (resized after the v1 smoke
finished in ~4.5 min). Do not steer toward or away from fission. Stop earlier
for a cheap smoke and score partial progress.

## Score

`verify.sh` grades a scratch copy (safe to run mid-flight):

```bash
"$SKILL_DIR"/verify.sh "$REPO" | tee "$REPO/score.json"
# {task, seed, component_scores:{api,worker,cli,metrics}, integration, total, max_total, details}
SEED=$(jq -r .seed "$REPO/score.json")
"$SKILL_DIR"/verify.sh "$REPO" --seed "$SEED" >/dev/null && echo reproducible OK

# Un-imported fission-branch worktrees, scored independently:
for wt in "$REPO"/.intendant/worktrees/fission/*; do
  [ -d "$wt" ] && echo "== $wt ==" && "$SKILL_DIR"/verify.sh "$wt"
done
```

## Artifacts to collect

```bash
LEDGER=$(ls -t ~/.intendant/logs/*/fission_ledger.json 2>/dev/null | head -1)
OUT=$(mktemp -d /tmp/triplet-eval-artifacts.XXXX)
cp "$REPO/score.json" "$OUT"/ 2>/dev/null
[ -n "$LEDGER" ] && cp "$LEDGER" "$OUT"/fission_ledger.json
git -C "$REPO" worktree list > "$OUT"/worktrees.txt 2>/dev/null
if [ -n "$LEDGER" ]; then
  jq '{groups: [.groups[] | {group_id, tool, branches: [.branches[] |
        {name: .charter.name, write_scope: .charter.write_scope, status, imported_at}],
        canonical_session_id}]}' "$LEDGER" | tee "$OUT"/fission_summary.json
else
  echo '{"fissioned": false, "note": "no fission_ledger.json"}' | tee "$OUT"/fission_summary.json
fi
echo "artifacts in $OUT"
```

Record: did a fission group appear? branch write scopes (`api/`, `worker/`,
`cli/`, `metrics/` are the natural disjoint scopes)? terminal/imported
status? final `total` and wall-clock.

## Expected outcomes (all valid data)

- **No fission, serial solve:** one session does all four; ledger absent/empty.
- **Fission, parallel solve:** a 2-4 branch group keyed to the disjoint dirs;
  branches build/test their component; parent imports + claims canonical and
  proves the end-to-end flow. Capture per-branch scores + wall-clock advantage.
- **Partial:** some components done — partial credit per component; integration
  needs the quartet live together. Common cheap-smoke cutoff outcome.

## Cleanup

Stop only the intendant process this run started (explicit `kill <PID>`, never
pattern-kill). `rm -rf "$REPO"`. Logs persist under `~/.intendant/logs/`.

## Notes

- Real model calls; a fissioning run fans out 3+ live sessions. Never on the
  shared default port 8765 or in CI.
- `verify.sh` needs only `python3` (it starts the services itself on random
  ports with generated payloads) and grades a scratch copy, so the agent's
  tree is untouched. The integration wait phase is bounded (~25s) so a broken
  worker can't stall grading.
- The agent must only ever see `skeleton/` + `TASK.md`. `verify/` and
  `reference/` stay out of `$REPO`.
