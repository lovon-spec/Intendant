# Live smoke — one managed-Codex run on service-triplet

> **Post-smoke note (2026-06-12, later the same day):** this smoke measured
> the **v1** tasks and is kept verbatim as the record that motivated the
> resize. Its headline finding — a fully-correct serial solve in **~4.5
> minutes** — showed v1 was far below the 30–60-min serial design target, so
> fission was never rational. Both tasks were then resized (v2): a fourth
> disjoint component each (`quarantine/`, `metrics/`), substantially deeper
> per-component specs, larger held-back batteries, per-component performance
> budgets, and `max_total` rescaled to 5.0. The v1 numbers below (4.0/4.0,
> battery sizes, timings) do **not** describe the current tasks; the
> post-resize validation lives in `README.md` ("Validation status") and each
> task's `HARDENING.md` (Pass 3).

One cheap live run (2026-06-12) to confirm a task boots under a real managed
Codex session, that the agent makes sensible progress, and to observe whether
it **spontaneously** fissions (it was never prompted to). The agent's output at
cutoff was scored with `verify.sh` to prove the scoring path works on real
output. service-triplet was chosen for cost (pure-stdlib, no toolchain builds,
fast verify).

## Setup

| | |
|---|---|
| intendant binary | `/Users/vm/projects/intendant/.worktrees/managed-bench/target/release/intendant` |
| Codex fork | `/Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex` |
| port | `18947` (isolated; not the shared 8765) |
| config | `[agent.codex] managed_context = "managed"`, `approval_policy = "never"`, `sandbox = "workspace-write"` |
| task repo | `/tmp/triplet-smoke` = `skeleton/` + `TASK.md` (no `verify/` or `reference/`) |
| prompt | `TASK.md` verbatim (neutral — no mention of fission/branching/parallelism) |

## What happened

Booted cleanly: web gateway up on 18947, Codex running on its own auth (the
"no native model provider" warning is expected — the controller holds no
keys). Session `3c58e486`, Codex rollout `019ebda4`.

Progress trace (from the rollout): the agent **surveyed all three components
first** — read `README.md`, all three `SPEC.md`, all three stubs, all three
test files, and the `Makefile` — then implemented `api/server.py`,
`worker/worker.py`, and `cli/client.py` and ran `make test`. ~25 tool calls,
441 lines written across the three files, all in the **main worktree**.

- **Model activity window:** 21:01:49Z → 21:06:15Z ≈ **4.5 min** (it reached a
  natural stopping point and went idle; not artificially cut).
- **Token usage:** 526,892 total (input 514,610 of which **457,472 cached** —
  an ~89% prompt-cache hit, the managed lineage cache working as intended;
  output 12,282, reasoning 5,840).

## Did it fission? **No.**

- `fission_spawn` tool **calls** in the rollout: **0**.
- No `fission_ledger.json` for the session; `git worktree list` shows only the
  main worktree (no `fission/` branches).
- The fission tooling *was* available and described: the injected
  `<managed_context>` **developer** instruction explains `fission_spawn` and
  when to prefer it. So the option was present and surfaced — the model simply
  **chose a serial solve**. This is valid, expected data: a ~4.5-min,
  3-component task that one context can hold comfortably gives a competent model
  little reason to branch. The lineage/concurrency advantage these tasks are
  built to expose grows with task size, contention, and context pressure;
  a single cheap smoke at the low end is exactly where serial should win.

This mirrors, at n=1 and live, the benchmark's 0-organic-fission finding:
availability alone doesn't trigger fission — the work has to be big or
pressured enough that branching pays. The pair is sized so fission *can* win
(disjoint scopes, independent streams) without being *required*, so both
outcomes are measurable.

## Score on the real output (cutoff)

`verify.sh /tmp/triplet-smoke` (held-back behavioral grader, fresh random
seed):

```json
{"task":"service-triplet","seed":594920270,
 "component_scores":{"api":1.0,"worker":1.0,"cli":1.0},
 "integration":1.0,"total":4.0,"max_total":4.0,
 "details":{"api":{"passed":15,"total":15},"worker":{"passed":22,"total":22},
            "cli":{"passed":5,"total":5},"integration":{"jobs":7,"correct":7}}}
```

The agent's serial solve was **fully correct** — **4.0/4.0**, all three
components and 7/7 end-to-end integration jobs against verifier-generated
payloads on random ports. Re-running with `--seed 594920270` reproduced 4.0
(deterministic). This proves the grader passes a *genuine* agent solution (not
only the bundled `reference/`), and exercises the real scoring path on real
output. (The partial-credit path itself is proven separately: skeleton scores
0.0, and the reward-hack probes show isolated per-component zeros — e.g. a
faking CLI yields `api=1, worker=1, cli=0, integration=0`.)

## Bug surfaced and fixed by the smoke

Scoring **without** `--seed` (the default, primary usage in the runners)
crashed both `verify.sh` scripts: `"${SEED_ARG[@]}"` on an empty array under
`set -u` is an "unbound variable" error in **bash 3.2** (macOS's `/bin/bash`).
Every earlier validation had passed `--seed`, masking it. Fixed in both
scripts with a length-guarded branch (`[ "${#SEED_ARG[@]}" -gt 0 ]`);
re-verified the no-seed path on the reference (4.0) and the smoke output (4.0).

## Cleanup

Killed only the intendant PID this run started (explicit `kill`, never
pattern-kill); port released, the Codex child reaped. A concurrent unrelated
agent process was left untouched. `/tmp/triplet-smoke` and the session logs
under `~/.intendant/logs/` are left for post-mortem.
