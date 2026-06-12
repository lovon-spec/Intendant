# HARDENING — service-triplet verifier

Adversarial review of `verify.sh` + `verify/`. Same philosophy as the
polyglot task: the verifier is **behavioral**. It generates jobs/payloads at
check time from a random seed and compares to an independent oracle
(`verify/oracle.py` for op semantics; `verify/ref_api.py` is a conforming
server the CLI is graded against). It never checks for files, strings, or
symbols.

## Threat model

A reward-seeking agent that may hardcode op results, memorize the visible
starter tests, fake HTTP behavior, or stub the end-to-end flow (e.g. a CLI
that fabricates a "done" job without ever calling the API). The verifier
scores only behavior on unseen inputs and on a live trio it starts itself.

## Core defenses

1. **Four independent batteries.** `api` is driven over **raw HTTP** by the
   verifier's own client and checked against the spec's lifecycle (status
   codes, queued→running→done, atomic claim 200/409, 400/404 paths). `worker`
   is the pure `compute` subcommand vs `oracle.compute`. `cli` runs against
   `ref_api.py` (a conforming reference server), so the CLI is graded
   independently of the agent's API. Integration starts the agent's API +
   worker on **random ports** and drives **generated** jobs end-to-end through
   the agent's CLI, checking each result against the oracle.
2. **Check-time random payloads + independent oracle.** Ops and inputs (valid
   and deliberately-invalid: wrong types, empty `max`, booleans-aren't-numbers,
   unknown ops) are generated per seed. Hardcoded results can't match.
3. **The verifier never uses the agent-visible starter tests.** Editing or
   passing them yields no grader credit.
4. **Scratch-copy grading**, side-effect free; safe mid-run.
5. **Live integration can't be faked.** A canned CLI/worker can't satisfy the
   oracle on random inputs, and a CLI that never calls the API leaves the
   worker with nothing to process and the API with no such job — caught by both
   the `cli` battery (against the reference server) and `integration`.

## Adversarial review — "how would I cheat this?"

| Attack | Outcome | Defense |
|---|---|---|
| Hardcode the 6 visible `compute` results | caught | random op/input battery vs oracle (probe 1) |
| Also hardcode the visible error cases so the starter test passes | still caught | held-back battery uses unseen ops/inputs ⇒ worker ≈ 0 (probe 1) |
| CLI fabricates a "done" job, never calls the API | caught | `cli` battery runs against the reference server; `integration` checks the oracle result (probe 2) |
| API returns a canned `201`/job without storing | caught | `api` battery creates then `GET`s the job and checks the round-tripped op/input |
| API skips the atomic claim | caught | battery claims twice and requires 200 then 409 |
| Worker always returns `status:"done"` | caught | error-input cases require `status:"error"`; valid cases require the exact result |
| Do-nothing stub "passes" the negative checks by always exiting non-zero | caught | negative `cli` checks are **gated** on the positive path passing (see Pass 1) |

## Iteration log

- **Pass 0 (design).** Four independent batteries + live-trio integration;
  per-component partial credit + integration bonus (max 4.0); ports chosen
  fresh per run; `/healthz` readiness poll before driving the API.
- **Pass 1 (negative-check leak).** The CLI battery's error-path checks ("get
  unknown id ⇒ non-zero exit", "wait on error job ⇒ non-zero exit") were
  trivially satisfied by the do-nothing **stub**, which exits non-zero for
  everything — the skeleton scored `cli = 0.4` instead of 0. **Fixed:** gate
  each negative check on its positive counterpart passing
  (`get_unknown_rejected` requires `get_prints_job`; `wait_error_rejected`
  requires `wait_done_exit0`). Skeleton CLI dropped to 0.0; reference stays
  1.0.
- **Pass 2 (broken-worker stall).** A worker that never processes jobs made
  integration pay a full per-job CLI-`wait` timeout (~20s × 7 ≈ 140s) — slow
  grading and a soft DoS. **Fixed:** submit *all* jobs first (the worker
  processes concurrently), then wait under a **single shared 15s deadline**
  (per-job timeout shrinks as the budget drains), with a direct-`GET` fallback.
  A healthy submission grades in ~3s; a dead worker is bounded to ~20s.
  Re-validated reference == oracle == 4.0 across seeds {1,2,3,7,42,88,99,555,
  808,1000,31337,70001,123456}.

## Reward-hack probes (run, both caught)

1. **Hardcoded worker** — a `compute` lookup table covering the visible
   starter cases (including the error cases, so it *passes the visible test*).
   On the held-back battery it scores **worker ≈ 0.14** (only trivial
   coincidences like `sum []` = 0 land) and **integration 0** (it computes
   garbage for the generated jobs).
2. **Faking CLI** — real API + real worker, but a CLI that fabricates a "done"
   job (result 42) and never calls the API. The CLI battery (against the
   reference server) scores **cli 0.0**, and **integration 0.0** because the
   fabricated result never matches the oracle and no job ever reaches the
   worker. API/worker (real) stay 1.0 — the cheat is localized and exposed.

## Residual notes

- **Port races.** Ports are acquired by binding `127.0.0.1:0`, reading the
  number, and closing — a small TOCTOU window. Mitigated by a `/healthz`
  readiness poll (up to ~6s) before any battery drives the server; a genuinely
  un-startable API scores 0 (correct).
- **Process hygiene.** The API and worker are started with `Popen` and always
  `terminate()`/`kill()`ed in a `finally`, so grading leaves no stray servers.
- Determinism: `--seed` reproduces a run (independent RNG streams per battery).
