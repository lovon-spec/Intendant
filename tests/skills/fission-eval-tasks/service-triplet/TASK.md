# Task: jobline service quartet

You are working in the `jobline` repository (your current directory): a small
job-processing system made of a REST API, a worker, a CLI client, and a
metrics service, all in Python 3 (standard library only). The scaffolding and
specs are in place, but the four programs are unimplemented stubs and their
tests fail.

## Your goal

Make the whole repository work:

1. `make test` passes (all four components' starter tests succeed).
2. The programs interoperate over the shared HTTP protocol: the CLI can
   submit jobs (singly and in batches) to the API, the worker picks them up
   and computes their results, the CLI can wait for / read / requeue them,
   and the metrics service reports accurate aggregates over the store.

## The four components are independent

Each component lives in its own directory with its own authoritative spec and
its own starter tests:

- `api/` — `server.py`, a REST job store (queue + lifecycle + requeue/delete +
  listing with filters and pagination; it stores jobs but computes nothing).
  Spec: `api/SPEC.md`.
- `worker/` — `worker.py`, which computes job results (it owns the op
  semantics — thirteen ops over lists, strings, and small objects, with
  precise validation rules) and has a serve loop that drives jobs through the
  API. Spec: `worker/SPEC.md`.
- `cli/` — `client.py`, a client with `submit` / `submit-batch` / `get` /
  `wait` / `requeue` verbs. Spec: `cli/SPEC.md`.
- `metrics/` — `metrics.py`, a read-only aggregation service with its own
  HTTP endpoint that summarizes the job store on demand. Spec:
  `metrics/SPEC.md`.

They communicate only through the shared HTTP protocol and op semantics
documented in `README.md` and the per-component specs; no component imports
another's source. Each can be implemented and tested on its own — the API
against raw HTTP requests, the worker's `compute` as a pure function, the CLI
and metrics against any conforming server. Read each spec carefully — the
edge-case rules (validation, lifecycle transitions, pagination, tie-breaks,
booleans-are-not-numbers) are precise and are what the tests check. The
worker and API specs also carry explicit performance budgets that the grader
enforces on generated large inputs.

## Rules

- Implement to the specs. Do not edit the test files, the `Makefile`, the
  `SPEC.md` files, or `README.md`; implement the four programs
  (`api/server.py`, `worker/worker.py`, `cli/client.py`,
  `metrics/metrics.py`) and add supporting source if you wish.
- Python 3 standard library only — no third-party packages. No network access
  beyond binding/connecting to localhost.

## How you are evaluated

A held-back grader checks each component independently against generated
inputs — it drives your API over raw HTTP and inspects the job lifecycle,
runs your worker's `compute` against an independent oracle (including the
performance budgets), and runs your CLI and metrics service against a
conforming reference server — then runs everything together: it starts your
API, worker, and metrics on random ports and uses your CLI to submit
generated jobs, wait for their results, and requeue failures, checking every
outcome against the oracle and your metrics output against the actual store.
Partial credit is awarded per component plus a bonus for the end-to-end flow,
so a correct component counts even if another is incomplete.
