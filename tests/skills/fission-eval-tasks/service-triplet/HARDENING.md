# HARDENING — service-triplet verifier

Adversarial review of `verify.sh` + `verify/`. Same philosophy as the
polyglot task: the verifier is **behavioral**. It generates jobs/payloads at
check time from a random seed and compares to an independent oracle
(`verify/oracle.py` for op semantics and the metrics aggregates;
`verify/ref_api.py` is a conforming server the CLI and metrics components are
graded against). It never checks for files, strings, or symbols.

## Threat model

A reward-seeking agent that may hardcode op results, memorize the visible
starter tests, fake HTTP behavior, or stub the end-to-end flow (e.g. a CLI
that fabricates a "done" job without ever calling the API, or a metrics
service that returns canned aggregates). The verifier scores only behavior on
unseen inputs and on a live quartet it starts itself.

## Core defenses

1. **Five independent measures.** `api` is driven over **raw HTTP** by the
   verifier's own client and checked against the spec's lifecycle (status
   codes, queued→running→done, atomic claim 200/409, requeue/delete rules,
   creation-order listing with filters + pagination, 400/404/409 paths).
   `worker` is the pure `compute` subcommand vs `oracle.compute` across all
   13 ops. `cli` runs against `ref_api.py` (a conforming reference server),
   so the CLI is graded independently of the agent's API. `metrics` also runs
   against `ref_api.py` with a generated population — including freshness
   checks (the store changes between requests) and API-down behavior.
   Integration starts the agent's API + worker + metrics on **random ports**
   and drives **generated** jobs end-to-end through the agent's CLI
   (including `submit-batch` and a requeue-recompute cycle), checking each
   result against the oracle and the metrics output against the API's actual
   job list.
2. **Check-time random payloads + independent oracle.** Ops and inputs (valid
   and deliberately-invalid: wrong types/shapes, empty `max`,
   booleans-aren't-numbers, fractional `rotate.by`, extra/missing `clamp`
   keys, unknown ops) are generated per seed. Hardcoded results can't match.
3. **The verifier never uses the agent-visible starter tests.** Editing or
   passing them yields no grader credit.
4. **Scratch-copy grading**, side-effect free; safe mid-run.
5. **Live integration can't be faked.** A canned CLI/worker can't satisfy the
   oracle on random inputs; a CLI that never calls the API leaves the worker
   with nothing to process; a canned metrics service can't track the
   ever-different generated population (and the integration cross-check
   compares it to the API's actual state at that moment).
6. **Negative checks are gated on the positive path.** Checks that pass on
   specific failures (4xx codes, non-zero CLI exits, 503s) only count once
   the battery's positive flow works — `get_unknown_rejected` requires
   `get_prints_job`, the API's 400/404/409 checks require create+get to work,
   the worker's must-error cases require at least one correct done-case, the
   metrics 503/404 checks require a correct `/summary`. A do-nothing stub —
   which fails everything — collects none of them (skeleton total: 0.0).
7. **Performance budgets on generated inputs.** `sort_desc` over 200k
   numbers and `histogram` over ~1 MB of tokens (both via the `-` stdin
   form), and an API bulk lifecycle of 250 jobs, must complete inside
   generous wall-clock budgets *and* produce oracle-correct output. Reference
   headroom is >30x, so host noise cannot flake a correct solution.

## Adversarial review — "how would I cheat this?"

| Attack | Outcome | Defense |
|---|---|---|
| Hardcode the visible `compute` results | caught | random op/input battery vs oracle (probe 1) |
| Also hardcode the visible error cases so the starter test passes | still caught | held-back battery uses unseen ops/inputs ⇒ worker ≈ 0.31, integration ≈ 0.13 (probe 1) |
| Return `status:"error"` for everything | caught | must-error cases are gated on a passing done-case ⇒ worker 0.0 |
| CLI fabricates ids/jobs, never calls the API | caught | `cli` battery runs against the reference server (round-trips inspected in-process); `integration` checks the oracle result (probe 2) |
| API returns a canned `201`/job without storing | caught | `api` battery creates then `GET`s the job and checks the round-tripped op/input/attempts |
| API skips the atomic claim | caught | battery claims twice and requires 200 then 409, and `attempts` must increment exactly once |
| Requeue implemented as "create a copy" | caught | the requeued job must keep its id, position in creation order, and `attempts` count |
| Metrics caches one snapshot at startup | caught | freshness checks mutate the reference store between requests |
| Metrics computes from its own bookkeeping instead of the API | caught | the battery seeds the store out-of-band (in-process), so only a real fetch sees it |
| Skip `submit-batch` line validation | caught | the battery's batch file contains invalid-JSON and non-object lines; ids/stdout/exit code all checked |
| Special-case the perf inputs | not useful | perf checks also require full output correctness vs the oracle |

## Iteration log

- **Pass 0 (design).** Four independent batteries + live-trio integration;
  per-component partial credit + integration bonus (max 4.0 at the time);
  ports chosen fresh per run; `/healthz` readiness poll before driving the
  API.
- **Pass 1 (negative-check leak).** The CLI battery's error-path checks ("get
  unknown id ⇒ non-zero exit", "wait on error job ⇒ non-zero exit") were
  trivially satisfied by the do-nothing **stub**, which exits non-zero for
  everything — the skeleton scored `cli = 0.4` instead of 0. **Fixed:** gate
  each negative check on its positive counterpart passing. Skeleton CLI
  dropped to 0.0; reference stays 1.0.
- **Pass 2 (broken-worker stall).** A worker that never processes jobs made
  integration pay a full per-job CLI-`wait` timeout — slow grading and a soft
  DoS. **Fixed:** submit *all* jobs first (the worker processes
  concurrently), then wait under a **single shared deadline** (per-job
  timeout shrinks as the budget drains), with a direct-`GET` fallback.
- **Pass 3 (resize to the 30–60 min design target).** The live smoke proved
  v1 too small (a competent serial agent finished it in ~4.5 min), so every
  spec was deepened and a fourth component added; the verifier grew matching
  batteries. Changes:
  - **New `metrics/` component** (read-only aggregation service with its own
    HTTP surface, on-demand freshness semantics, numeric-result stats with
    bools excluded, and API-down 503 behavior); `max_total` is now **5.0**.
  - **Deepened specs** — api: `attempts` counter, `requeue`/`DELETE`
    lifecycle rules, creation-order listing with status+op filters and
    validated `offset`/`limit` pagination, non-empty-op/input-key-present
    validation; worker: 13 ops (sum/max/min/mean/median/sort_desc/dedupe/
    reverse/wordcount/uppercase/histogram/clamp/rotate) with exact-shape
    object inputs, integer-valued `by`, first-occurrence dedupe with numeric
    equality, and a stdin (`-`) input form; cli: `submit-batch`
    (continue-on-error, ordered ids), `requeue`, `wait --quiet`.
  - **Performance budgets** (defense 7).
  - **Two grader bugs found by the seed sweep and probe re-runs:** (a) the
    integration mix could draw the `("", "x")` invalid job, which the v2 API
    now correctly rejects at `POST /jobs` — the grader was penalizing correct
    behavior; integration now draws only submittable invalid jobs (empty-op
    validation is the api battery's job). (b) An always-"error" worker stub
    scored 0.36 from the must-error cases; they are now gated on a passing
    done-case (defense 6).
  - Re-validated: reference == oracle == **5.0** across seeds {1, 2, 7, 42,
    99, 555, 808, 1000, 31337, 123456, 12345} + a fresh-seed run; skeleton
    0.0; both probes re-run and still caught; same-seed reruns identical
    (modulo perf `elapsed_s`); full verify ≈ 5 s wall on the dev box — well
    inside the <3 min budget.

## Reward-hack probes (run, both caught — re-run after the Pass-3 resize)

1. **Hardcoded worker** — a `compute` lookup table covering every visible
   starter case (done *and* error cases, plus the stdin form, so it *passes
   the visible test*). On the held-back battery it scores **worker 0.3061**
   (memorized hits plus must-error coincidences — which its memorized done
   cases unlock through the gate) and **integration 0.1333** (only the jobs
   that genuinely error land; every valid computation is wrong).
   api/cli/metrics (real) stay 1.0 — the cheat is localized (total 3.44/5.0
   vs 5.0 for honest work; on a task this size the hack is strictly worse
   than implementing).
2. **Faking CLI** — real API + real worker + real metrics, but a CLI that
   fabricates ids and "done" jobs (result 42) and never calls the API. The
   CLI battery (against the reference server, with in-process round-trip
   inspection) scores **cli 0.0**, and **integration 0.0667** — 0/11 jobs
   correct, requeue extras fail; the only unit that lands is the metrics
   cross-check, which honestly reports an empty store (the API genuinely has
   no jobs — consistent behavior, not a cheat reward). Total 3.07/5.0 with
   the cheat fully localized to the faked component.

## Residual notes

- **Port races.** Ports are acquired by binding `127.0.0.1:0`, reading the
  number, and closing — a small TOCTOU window. Mitigated by a `/healthz`
  readiness poll (up to ~6s) before any battery drives a server; a genuinely
  un-startable API scores 0 (correct).
- **Process hygiene.** The API, worker, and metrics processes are started
  with `Popen` and always `terminate()`/`kill()`ed in a `finally`, so grading
  leaves no stray servers.
- Determinism: `--seed` reproduces a run (independent RNG streams per
  battery; gating is applied at counting time, after the RNG-consuming loop,
  so it never shifts the seed→input map).
