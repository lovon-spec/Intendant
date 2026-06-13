# Fission-Shaped Evaluation Tasks

Two evaluation tasks designed so that Intendant's managed-Codex **model-driven
fission** (`fission_spawn` / `fission_control` / `claim_fission_canonical` —
full-context branches with charters and isolated git worktrees) has a genuine,
measurable reason to fire. The constrained-window Terminal-Bench run
(133 trials, see `scripts/benchmarks/codex_managed_benchmark_2026-06-12.md`)
measured **zero organic fission uses** — single-container, single-stream tasks
give the model no reason to branch. These tasks do: each is one repo with
**four** separable work streams that own **disjoint write scopes** and share
only a small contract, sized so a competent agent needs **30–60 minutes
serially** (the v1 pair was solved serially in ~4.5 min by a live smoke —
see `SMOKE.md` — and was resized: deeper per-component specs, bigger
held-back batteries, per-component performance budgets, and a fourth
component each). Fission is never required and never mentioned to the agent;
whether the model chooses it is the measurement.

| Task | Components (disjoint write scopes) | Integration check |
|---|---|---|
| `polyglot-pipeline/` | `normalizer/` (Python CSV→JSONL), `quarantine/` (Python business-rule screen), `dedup/` (Rust merge/dedupe, 500k-line perf budget), `report/` (jq/shell aggregator) | `make pipeline` end-to-end (4 stages) on verifier-generated CSVs |
| `service-triplet/` | `api/` (REST job store + requeue/delete/pagination), `worker/` (13-op processor, perf budgets), `cli/` (5-verb client), `metrics/` (read-only aggregation service) — all Python stdlib | live quartet booted by the verifier, driven through the agent's own CLI on random ports/payloads, metrics cross-checked against the actual store |

(`service-triplet/` keeps its historical directory name; the v2 task is a
quartet.)

## Layout (per task)

```
<task>/
├── TASK.md       # the agent-facing prompt — NEUTRAL: never mentions fission,
│                 # branching, parallelism, or sub-agents
├── SKILL.md      # runner: launch a managed Intendant session on the task,
│                 # score it, collect artifacts (incl. fission_ledger.json)
├── HARDENING.md  # adversarial review of the verifier + reward-hack probes
├── verify.sh     # scorer: verify.sh <workdir> [--seed N] → JSON on stdout
├── verify/       # verifier internals (generators + independent reference logic)
├── skeleton/     # the repo the agent gets — copy ONLY this into the workdir
└── reference/    # full solutions for verifier self-test — NEVER expose to agents
```

**The agent must only ever see `skeleton/` contents.** `verify/`, `reference/`,
and the task docs stay outside its working directory (the SKILL runners do this
correctly; keep it that way).

The teaching-efficacy A/B experiment for the fission *trigger heuristic*
(generic managed policy vs. policy + a project-level trigger delivered through
`.intendant/codex-managed-instructions.md`) lives in `trigger-ab/`.

## Scoring contract

`verify.sh <workdir> [--seed N]` always exits 0 when scoring completed
(non-zero only on harness-internal error) and prints a single JSON object on
stdout:

```json
{
  "task": "polyglot-pipeline",
  "seed": 12345,
  "component_scores": {"normalizer": 0.83, "quarantine": 1.0, "dedup": 1.0, "report": 1.0},
  "integration": 0.5,
  "total": 4.33,
  "max_total": 5.0,
  "details": {"...": "per-subcheck booleans, perf timings, first-failure snippets"}
}
```

- Each component score is `passed/total` over an independent behavioral
  battery; `total = sum(component_scores) + integration` (max **5.0**).
- **Behavioral, hack-resistant:** every battery runs the agent's code against
  inputs *generated at check time* from a random seed, compared against the
  verifier's own independent implementation of the spec (`verify/`). Nothing
  checks for the presence of files or strings. Negative checks (expected
  non-zero exits / 4xx codes) are gated on the battery's positive path, so a
  do-nothing stub scores 0. Performance budgets are graded on generated large
  inputs and also require output correctness. `--seed` reproduces a run
  exactly; omitting it draws a fresh seed (printed in the JSON).
- Scoring runs on a scratch **copy** of the workdir (`.git`/`.intendant`
  excluded), so it is safe mid-run and has no side effects on the agent's
  tree. It can also be pointed at an individual fission-branch worktree
  (`<workdir>/.intendant/worktrees/fission/<x>`) to score un-merged branch
  work separately.

## Validation status (2026-06-12, post-resize)

Both verifiers: reference solutions score **5.0/5.0** across seeds
{1, 2, 7, 42, 99, 555, 808, 1000, 31337, 123456, 12345} plus a fresh-seed
run; skeletons score **0.0**; same-seed reruns are identical (modulo perf
`elapsed_s`); the reward-hack probes in each `HARDENING.md` were re-run after
the resize and are still caught. Full verify wall-clock: ~8.5 s
(polyglot-pipeline, sccache-cached Rust build; allow ~40 s cold) and ~5 s
(service-triplet) — both far inside the 3-minute budget.

## Environment

Local throwaway dir (macOS/Linux) or a plain docker container. Needs:
`python3` (≥3.9, stdlib only), `bash`, `make`, `git`, `jq` (≥1.6), and for
polyglot-pipeline a Rust toolchain with the `serde_json` crate either
cached in `CARGO_HOME` or fetchable (registry traffic only — the package-manager
equivalent of pip/npm; run `cargo fetch` in `dedup/` once at setup, after which
everything builds offline). service-triplet needs no toolchain beyond python3.
Docker example: `rust:1-bookworm` + `apt-get install -y python3 jq make git`.

Smoke-run notes for the (v1) pair live in `SMOKE.md`; the A/B protocol in
`trigger-ab/PROTOCOL.md`.
