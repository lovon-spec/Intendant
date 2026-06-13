# Teaching-efficacy A/B — does a *trigger heuristic* unlock organic fission?

## Question

The constrained-window benchmark (133 trials) and the v1 live smoke both
measured **zero organic fission**, even though the managed policy describes
the fission tools and the smoke task had cleanly disjoint components. Three
competing explanations:

1. **Teaching gap** — the generic policy says *what* fission is and *that*
   separable subtasks should prefer it, but never anchors *when* in the
   model's workflow to evaluate the choice, so the option never enters the
   plan.
2. **Structural/sizing** — the tasks simply weren't big enough for branching
   to be rational (addressed separately: both eval tasks were resized to a
   30–60 min serial target with four disjoint components each).
3. **Model preference** — the model sees the option, sizes it correctly, and
   still prefers serial.

This experiment isolates (1) against (2)/(3) by holding everything constant
except **one paragraph of policy** delivered through the existing per-project
managed-instructions mechanism.

## The two arms

### Arm A — generic managed policy (control)

Exactly the policy as merged: `MANAGED_CONTEXT_DEVELOPER_INSTRUCTIONS` in
`src/bin/caller/external_agent/codex.rs`, whose fission section already says
(verbatim, the relevant lines):

> Fission (full-context branch spawning), when fission tools are available:
> - When a coherent subtask is separable or parallelizable, prefer
>   `fission_spawn` with a self-contained charter over a deep in-context
>   detour. […]
> - Favor breadth over depth, before pressure builds: fission is ex-ante,
>   rewind is ex-post […]

No project instructions file. This is what every prior zero-fission
measurement ran with.

### Arm B — generic policy + project-level trigger heuristic

Same binary, same config, same prompt — plus the file
`arm-b/codex-managed-instructions.md` from this directory dropped into the
task workdir at `.intendant/codex-managed-instructions.md`. Its full text:

> Decomposition trigger (project policy):
>
> When a task presents two or more independent components with disjoint write
> scopes, evaluate chartering them as fission branches in your FIRST planning
> step; spawn when the components exceed what one focused pass holds
> comfortably; state the decision either way in one sentence.

**Design intent — a trigger, not a mandate.** The heuristic deliberately
prescribes only *when to evaluate* the choice (first planning step) and *what
evidence decides it* (components vs. one focused pass), never the outcome.
"State the decision either way in one sentence" makes the deliberation
observable in the transcript whichever way it goes, so a serial Arm-B solve
still tells us the trigger fired and was weighed — without coercing a spawn.
A mandate would corrupt the measurement: we want to know whether *teaching
the trigger* changes behavior, not whether the model can follow orders. The
agent-facing `TASK.md` stays completely neutral (it never mentions fission /
branching / parallelism); the heuristic arrives through the policy layer,
exactly where a real deployment would put it.

## Delivery mechanism (verified in source)

`<working_dir>/.intendant/codex-managed-instructions.md` is the **existing**
per-project extension point for managed sessions —
`src/bin/caller/external_agent/codex.rs`:

- `start_thread()` → `effective_managed_context_developer_instructions()`
  (only when `managed_context` is on) →
  `managed_context_developer_instructions_for_project(existing, working_dir)`;
- that calls `project_managed_context_instructions(working_dir)`, which reads
  `working_dir.join(".intendant").join("codex-managed-instructions.md")`
  (`project_managed_context_instructions_path`), trims it, caps it at 16 KiB
  (truncation marker on overflow), and appends it under the heading
  `Project managed-context instructions (.intendant/codex-managed-instructions.md):`
  **after** the generic block;
- the combined text is sent as `developerInstructions` in the
  `thread/start` / `thread/resume` params (covered by unit tests
  `managed_context_instructions_append_project_file_when_present`, `…_cap_project_file_size`,
  `…_without_project_file_are_generic_only`).

So the runner only needs to place the file in the task repo before launch —
no binary change, no config change. Two operational notes:

- Drop the file **after** the skeleton commit (like `intendant.toml`) so it
  stays untracked; grading is unaffected either way (`verify.sh` excludes
  `.intendant/` from the scratch copy).
- Fission **branch** sessions run in their own worktrees, where the untracked
  file does not exist — branches get the generic block only. That is the
  intended shape: the trigger teaches the *parent* when to decompose; nested
  re-decomposition is not part of this measurement.
- **Per-run integrity check:** after the session boots, confirm the trigger
  text reached the model by grepping the newest Codex rollout for a
  distinctive phrase (see "Run protocol" step 5). A one-off live sanity check
  of this mechanism (managed boot in a throwaway dir with the Arm-B file,
  rollout inspected, killed within ~3 min) is recorded at the bottom.

## Design

| | |
|---|---|
| Tasks | `polyglot-pipeline` (4 components: normalizer/quarantine/dedup/report), `service-triplet` (4 components: api/worker/cli/metrics) — both v2 (30–60 min serial target) |
| Arms | A (generic), B (generic + trigger) |
| Attempts | **n = 3 per task per arm = 12 runs** |
| Window | full (no constrained-window flags) — sizing pressure must come from the task, not an artificial cap |
| Binary | one managed-context intendant build for all 12 runs (record commit); patched Codex fork (record commit) |
| Ports | isolated, one per run, never 8765: polyglot A 18951–18953, polyglot B 18954–18956, triplet A 18961–18963, triplet B 18964–18966 |
| Order | interleave arms (A,B,A,B,…) per task to balance time-of-day/provider drift |
| Cap | stop a run at natural idle or **75 min** wall, whichever first; score whatever exists (partial credit is designed in) |

n=3 is deliberately small: fission usage is a near-binary per-run signal
(group exists / doesn't), so 3 runs per cell resolve presence/absence
patterns; score deltas at n=3 are directional only. Do not read means with
error bars into this — read the contingency pattern.

## Per-run measurements

1. **Behavioral score** — `verify.sh <repo>` total + per-component breakdown
   (fresh seed; record it). Score branch worktrees separately when un-merged
   work exists (`<repo>/.intendant/worktrees/fission/*`).
2. **Fission usage** — from the session's `fission_ledger.json`
   (`~/.intendant/logs/<session>/`): number of groups, branches per group,
   charters' `write_scope`s, branch terminal statuses, imports
   (`imported_at`), canonical claim. Zero groups (file absent) is the A-side
   expected outcome and is valid data.
3. **Wall clock** — model activity window: first→last event timestamp in the
   Codex rollout (`~/.codex/sessions/.../rollout-*.jsonl`), plus launch→idle
   wall time as observed.
4. **Tokens** — cumulative usage from the rollout `token_count` events
   (input/cached/output/reasoning), as in the v1 smoke.
5. **The decision sentence** (Arm B) — the one-sentence spawn-or-serial
   statement from the transcript; its mere presence/absence also measures
   whether the trigger reached the plan.

## Run protocol (exact commands)

Per run: `TASK` ∈ {polyglot-pipeline, service-triplet}, `ARM` ∈ {a, b},
`PORT` from the table. The SKILL runners' setup is reused verbatim; Arm B
adds one file drop. `BENCH_BIN` is the managed-context build used for all
runs.

```bash
SUITE=/Users/vm/projects/intendant/.worktrees/fission-tasks/tests/skills/fission-eval-tasks
SKILL_DIR=$SUITE/$TASK
BENCH_BIN=/Users/vm/projects/intendant/.worktrees/managed-bench/target/release/intendant
CODEX_FORK=/Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex

# 1. fresh task repo from skeleton/ ONLY (never verify/ or reference/)
REPO=$(mktemp -d /tmp/trigger-ab-$TASK-$ARM.XXXX)
cp -R "$SKILL_DIR"/skeleton/. "$REPO"/
cp "$SKILL_DIR"/TASK.md "$REPO"/TASK.md
git -C "$REPO" init -q && git -C "$REPO" add -A
git -C "$REPO" -c user.email=eval@local -c user.name=eval commit -qm 'skeleton'
# polyglot-pipeline only: pre-fetch crates so branches build offline
[ -d "$REPO/dedup" ] && ( cd "$REPO/dedup" && cargo fetch ) >/dev/null 2>&1 || true

# 2. managed config (identical for both arms)
cat > "$REPO/intendant.toml" <<EOF
[agent.codex]
command = "$CODEX_FORK"
managed_context = "managed"
approval_policy = "never"
sandbox = "workspace-write"
EOF

# 3. ARM B ONLY: drop the trigger heuristic (untracked, like intendant.toml)
if [ "$ARM" = b ]; then
  mkdir -p "$REPO/.intendant"
  cp "$SUITE/trigger-ab/arm-b/codex-managed-instructions.md" \
     "$REPO/.intendant/codex-managed-instructions.md"
fi

# 4. launch (dashboard ON — fission branches resume through the supervisor)
cd "$REPO"
"$BENCH_BIN" --agent codex --web $PORT --no-tls --bind 127.0.0.1 \
  "$(cat TASK.md)" & echo "intendant PID $!"

# 5. ARM B integrity check (~1 min in): the trigger text must be in the
#    developer instructions the model received. Grep for a needle that sits
#    on ONE line of the policy file - rollout records JSON-escape newlines
#    as \n, so phrases that span a line break in the file never match.
ROLLOUT=$(ls -t ~/.codex/sessions/*/*/*/rollout-*.jsonl | head -1)
grep -l "Decomposition trigger (project policy):" "$ROLLOUT" \
  && echo "arm-b trigger reached the model" \
  || echo "ARM-B DELIVERY FAILED - abort this run"

# 6. at natural idle or the 75-min cap: stop ONLY the PID from step 4
#    (explicit kill; never pattern-kill), then score + collect.
"$SKILL_DIR"/verify.sh "$REPO" | tee "$REPO/score.json"
for wt in "$REPO"/.intendant/worktrees/fission/*; do
  [ -d "$wt" ] && echo "== branch $wt ==" && "$SKILL_DIR"/verify.sh "$wt"
done
LEDGER=$(ls -t ~/.intendant/logs/*/fission_ledger.json 2>/dev/null | head -1)
[ -n "$LEDGER" ] && jq '{groups: [.groups[] | {group_id, tool,
    branches: [.branches[] | {name: .charter.name,
    write_scope: .charter.write_scope, status, imported_at}],
    canonical_session_id}]}' "$LEDGER" || echo '{"fissioned": false}'
```

Collect per run into `trigger-ab/results/<task>-<arm>-<n>/`: `score.json`,
`fission_summary.json`, branch scores, the rollout path, wall-clock + token
numbers, and (Arm B) the decision sentence.

## Results table (fill per run)

| run | task | arm | fissioned? | branches (write scopes) | imported/canonical | total /5.0 | wall (min) | tokens (in/cached/out) | decision sentence |
|---|---|---|---|---|---|---|---|---|---|
| 1 | polyglot | A | | | | | | | n/a |
| … | | | | | | | | | |

## Decision rules (read the pattern, then the scores)

- **B fissions where A doesn't** (e.g. B ≥ 4/6 runs with a group, A ≤ 1/6)
  **and B's outcome is non-inferior** (mean B total ≥ mean A total − 0.25 on
  the 5-point scale, no B run catastrophically below its A counterparts) ⇒
  **teaching gap confirmed**: the generic policy under-teaches the trigger;
  promote the heuristic (or a refined version) into the generic block or the
  per-project default.
- **Neither arm fissions** ⇒ structure/sizing is *still* insufficient at this
  scale, or the model's preference dominates any wording ⇒ escalate the
  structural lever first (bigger batteries, more components, real
  cross-component contention) before iterating on policy text; treat policy
  wording as unproven either way.
- **Both arms fission** ⇒ the resized structure alone suffices; the trigger
  paragraph is redundant for this model — keep the generic policy and drop
  the per-project heuristic (avoid policy bloat).
- **B fissions but is inferior** (scores or wall clock clearly worse) ⇒ the
  trigger fires but fission doesn't pay at this size for this model —
  valuable calibration: revisit the "exceeds what one focused pass holds"
  clause (it under-gates), not the delivery mechanism.
- **A fissions where B doesn't** ⇒ noise dominates at n=3; re-run the
  affected cells before concluding anything.

Secondary reads (any pattern): per-branch verify totals vs. the parent's
(does branch work actually land?), wall-clock fission vs. serial on the same
task, token overhead per branch, and whether Arm-B serial runs still *state*
the decision (trigger reached the plan but lost the argument — that is
evidence for "preference", not "teaching gap").

## Mechanism sanity check (performed 2026-06-12 — delivery confirmed)

One cheap live boot, no task work: a managed session was launched with the
bench binary (`.worktrees/managed-bench`, isolated port 18971, dashboard on)
in a throwaway dir containing only `intendant.toml` and the Arm-B
`.intendant/codex-managed-instructions.md`, with a trivial one-line prompt.
The session's Codex rollout
(`rollout-2026-06-12T19-25-40-019ebe27-….jsonl`) was inspected: **line 2** —
a `response_item`/`message` record at the head of the thread (the injected
developer instructions) — contains the project heading and the full trigger
paragraph verbatim:

```
Project managed-context instructions (.intendant/codex-managed-instructions.md):
 | Decomposition trigger (project policy): |  | When a task presents two or
more independent components with disjoint write | scopes, evaluate chartering
them as fission branches in your FIRST planning | step; spawn when the
components exceed what one focused pass holds | comfortably …
```

(`|` marks JSON-escaped newlines.) This confirms the
`managed_context_developer_instructions_for_project` chain end-to-end on the
exact binary the experiment will use. The intendant process was killed by
explicit PID ~3.5 min after launch; the port was released and the temp dir
removed. One operational lesson folded into step 5 above: pick a grep needle
that does not span a line break in the policy file.
