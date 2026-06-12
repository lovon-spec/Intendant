---
name: polyglot-pipeline-fission-eval
description: >
  Fission-shaped evaluation runner. Launches a managed Codex session on the
  salestream ETL task (four independent components — a Python CSV→JSONL
  normalizer, a Python business-rule quarantine stage, a Rust merge/dedupe
  tool, a bash+jq report generator — plus an end-to-end Makefile pipeline),
  then scores the result behaviorally with verify.sh (per-component partial
  credit + an integration bonus, max 5.0) and records whether the model
  spontaneously used model-driven fission. The four work streams own disjoint
  write scopes, so fission into chartered worktree branches is a genuine
  wall-clock/context win — but it is never required and never mentioned to
  the agent. Whether it fissions is the measurement.
compatibility: >
  Requires OpenAI Codex auth in ~/.codex/auth.json, the patched Codex fork at
  /Users/vm/projects/codex-minimal-lineage, a managed-build intendant binary,
  and a Rust toolchain + jq + make in PATH. Daemon/dashboard mode (do NOT pass
  --no-web; fission branches resume through the session supervisor). Real model
  calls; not for CI.
allowed-tools: Bash Read
disable-model-invocation: false
---

# polyglot-pipeline — fission-shaped eval runner

## Purpose

Measure whether a managed Codex agent, handed a genuinely multi-component
repo, **chooses** model-driven fission (`fission_spawn` into chartered,
write-scoped git worktrees) — and score the work behaviorally either way. The
constrained-window Terminal-Bench run measured 0 organic fission uses across
133 trials because those tasks were single-stream; this task is built to give
fission a real reason to fire (four independent work streams, disjoint write
scopes, sized for ~30–60 min of serial work) without ever asking for it.

## What it measures

- **Behavioral score** (`verify.sh` → JSON): `component_scores` for
  `normalizer` / `quarantine` / `dedup` / `report` (each `passed/total` over a
  held-back battery generated at check time and compared to an independent
  oracle, including per-component performance budgets) plus an `integration`
  bonus (the real `make pipeline` on fresh CSVs, Makefile pinned by the
  grader). `total` is out of 5.0. Partial credit per component, so a finished
  stream counts even if another is incomplete.
- **Did it fission?** Presence and shape of a fission group in the parent's
  `fission_ledger.json`: how many branches, their charters/write scopes,
  whether branches reached terminal status, and whether the parent imported
  and claimed canonical. Zero fission is valid data (the point is to measure,
  not to coerce).
- **Lineage/concurrency value** (the thing the benchmark couldn't see in
  single-container tasks): wall-clock from first tool call to a passing
  `total`, and per-branch component scores when work lands in branch worktrees
  before import (score them with `verify.sh <branch-worktree>`).

## Setup

```bash
# 1. Patched Codex (minimal-lineage fork)
cd /Users/vm/projects/codex-minimal-lineage/codex-rs
cargo build -p codex-cli --bin codex      # debug build is fine

# 2. Managed-context intendant build (use a managed-harness build, NOT the
#    shared main checkout). Your own worktree's target/release/intendant.
cd /path/to/your/intendant/worktree
cargo build --release --bin intendant

# 3. Assemble a throwaway task repo from skeleton/ ONLY (never copy verify/ or
#    reference/ — the agent must not see the oracle or the held-back solutions).
TASKDIR=$(cd "$(dirname "$0")" && pwd)            # this task's dir, or hardcode it
SKILL_DIR=/Users/vm/projects/intendant/.worktrees/fission-tasks/tests/skills/fission-eval-tasks/polyglot-pipeline
REPO=$(mktemp -d /tmp/polyglot-eval.XXXX)
cp -R "$SKILL_DIR"/skeleton/. "$REPO"/
cp "$SKILL_DIR"/TASK.md "$REPO"/TASK.md
git -C "$REPO" init -q
git -C "$REPO" add -A
git -C "$REPO" -c user.email=eval@local -c user.name=eval commit -qm 'salestream skeleton'
# Pre-fetch crates so the agent (and its branches) build offline.
( cd "$REPO/dedup" && cargo fetch ) >/dev/null 2>&1 || true

# 4. Per-project managed Codex config (managed_context = managed is what flips
#    on the fission/rewind tool surface).
cat > "$REPO/intendant.toml" <<'EOF'
[agent.codex]
command = "/Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex"
managed_context = "managed"
approval_policy = "never"
sandbox = "workspace-write"
EOF
```

## Run

Pick a free port (never the shared 8765). Launch from the task repo with the
dashboard ON (the default). Feed the task prompt verbatim from `TASK.md` —
**neutral wording; it must not mention fission, branching, parallelism, or
sub-agents.** Whether the model decomposes into fission branches is exactly
what we are measuring.

```bash
PORT=18931
cd "$REPO"
/path/to/your/intendant/worktree/target/release/intendant --agent codex \
  --web $PORT --no-tls --bind 127.0.0.1 \
  "$(cat TASK.md)"
```

Let it work. A competent serial agent finishes in ~30-60 min; budget the run
accordingly (or stop earlier for a cheap smoke and score partial progress —
the partial-credit path is the point). Do not steer it toward or away from
fission.

## Score

`verify.sh` grades a scratch copy of the repo (it never mutates the agent's
tree, so it is safe to run mid-flight for progress checks):

```bash
"$SKILL_DIR"/verify.sh "$REPO" | tee "$REPO/score.json"
# {task, seed, component_scores:{normalizer,quarantine,dedup,report}, integration, total, max_total, details}

# Reproducible re-grade (same generated inputs):
SEED=$(jq -r .seed "$REPO/score.json")
"$SKILL_DIR"/verify.sh "$REPO" --seed "$SEED" >/dev/null && echo "reproducible OK"
```

If the agent fissioned and left work in branch worktrees that were not yet
imported, score those independently to capture concurrency value:

```bash
for wt in "$REPO"/.intendant/worktrees/fission/*; do
  [ -d "$wt" ] && echo "== $wt ==" && "$SKILL_DIR"/verify.sh "$wt"
done
```

## Artifacts to collect

```bash
LEDGER=$(ls -t ~/.intendant/logs/*/fission_ledger.json 2>/dev/null | head -1)
LOG_DIR=$(dirname "$LEDGER" 2>/dev/null)
OUT=$(mktemp -d /tmp/polyglot-eval-artifacts.XXXX)

cp "$REPO/score.json"                      "$OUT"/ 2>/dev/null
[ -n "$LEDGER" ]  && cp "$LEDGER"          "$OUT"/fission_ledger.json
[ -n "$LOG_DIR" ] && cp -R "$LOG_DIR"/context_rewinds "$OUT"/ 2>/dev/null
[ -n "$LOG_DIR" ] && ls "$LOG_DIR" > "$OUT"/session_log_listing.txt
git -C "$REPO" worktree list              > "$OUT"/worktrees.txt 2>/dev/null
git -C "$REPO" log --oneline --all        > "$OUT"/git_log.txt   2>/dev/null

# Fission summary (empty groups array => the model did NOT fission — valid data)
if [ -n "$LEDGER" ]; then
  jq '{groups: [.groups[] | {group_id, tool, branches: [.branches[] |
        {name: .charter.name, write_scope: .charter.write_scope,
         status, imported_at}], canonical_session_id}]}' "$LEDGER" \
    | tee "$OUT"/fission_summary.json
else
  echo '{"fissioned": false, "note": "no fission_ledger.json — no fission group was created"}' \
    | tee "$OUT"/fission_summary.json
fi
echo "artifacts in $OUT"
```

Record: did a fission group appear? how many branches, with what write scopes
(`normalizer/`, `quarantine/`, `dedup/`, `report/` are the natural disjoint
scopes)? did
branches reach terminal status and get imported/claimed? final `total` and the
wall-clock to reach it.

## Expected outcomes (all valid data)

- **No fission, serial solve:** one session implements all four components;
  `fission_ledger.json` absent or empty. Score reflects serial quality.
- **Fission, parallel solve:** a `fission_spawn` group with 2-4 branches keyed
  to the disjoint component dirs; branches build/test their own component;
  parent imports + claims canonical and wires `make pipeline`. Capture the
  per-branch scores and the wall-clock advantage.
- **Partial:** some components done, others stubbed — partial credit lands per
  component; integration is 0 until all four wire together. This is the
  common cheap-smoke cutoff outcome.

## Cleanup

Stop only the intendant process this run started (Ctrl-C in its terminal, or
`kill <PID>` with the explicit PID — never pattern-kill). Then `rm -rf "$REPO"`
(also removes its fission worktrees). Session logs persist under
`~/.intendant/logs/` and Codex rollouts under `~/.codex/sessions/`.

## Notes

- Real model calls; a fissioning run fans out 3+ live sessions. Never run on
  the shared default port 8765 or in normal CI.
- `verify.sh` needs `python3`, `bash`, `make`, `jq`, and a Rust toolchain
  (to build the agent's `dedup`); it builds on a scratch copy, so the agent's
  tree is untouched.
- The agent must only ever see `skeleton/` + `TASK.md`. `verify/` (oracle +
  generators) and `reference/` (held-back solutions) stay out of `$REPO`.
