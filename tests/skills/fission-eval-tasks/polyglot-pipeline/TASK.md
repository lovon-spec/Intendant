# Task: salestream ETL pipeline

You are working in the `salestream` repository (your current directory). It is
a small ETL pipeline that normalizes raw sales-CSV exports to JSONL, screens
records against business rules (quarantining rejects for audit), merges and
deduplicates them across regions, and produces a summary report. The repo
compiles and its scaffolding is in place, but the four tools are unimplemented
stubs and their tests fail.

## Your goal

Make the whole repository pass. Concretely:

1. `make test` passes (all four components' tests succeed).
2. `make pipeline RAW=<dir> OUT=<dir> AS_OF=<YYYY-MM-DD>` runs end to end on a
   directory of CSV files and writes `<OUT>/normalized/*.jsonl`,
   `<OUT>/clean/*.jsonl`, `<OUT>/quarantine/*.jsonl`, `<OUT>/merged.jsonl`,
   and `<OUT>/report.json`.

## The four components are independent

The repository has four components, each in its own directory with its own
authoritative spec and its own starter tests:

- `normalizer/` — `normalize.py`, a Python CSV→JSONL normalizer (header
  mapping, amount/date/email/tag parsing rules). Spec: `normalizer/SPEC.md`.
- `quarantine/` — `quarantine.py`, a Python business-rule screen that splits
  records into clean and quarantined-with-reasons. Spec: `quarantine/SPEC.md`.
- `dedup/` — a Rust merge/dedupe tool with a documented conflict policy,
  schema validation, and a `--since` filter. Spec: `dedup/SPEC.md`.
- `report/` — `report.sh`, a bash+jq report generator (totals, median/p90,
  per-month and per-domain breakdowns). Spec: `report/SPEC.md`.

They share only the one-line JSON record schema described in `README.md`. They
do not import or call each other's source; each can be built and tested on its
own. Read each component's `SPEC.md` carefully — the edge-case rules
(amount/date parsing, the quarantine rule set, the dedupe conflict policy,
tie-breaks, rounding) are precise and are what the tests check. Two components
also carry explicit performance budgets (`dedup`: a 500k-line input under
10 s; `normalizer`: a 120k-row CSV under 45 s) that the grader enforces on
generated large inputs.

## Rules

- Implement to the specs. Do not edit the test files, the `Makefile`, the
  `SPEC.md` files, or `README.md`; implement the four tools
  (`normalizer/normalize.py`, `quarantine/quarantine.py`, `dedup/src/main.rs`,
  `report/report.sh`) and add supporting source if you wish.
- Allowed dependencies: Python standard library only for the normalizer and
  quarantine; the `dedup` crate may use the `serde_json` crate already in its
  `Cargo.toml` (run `cargo fetch` in `dedup/` if the registry is reachable;
  the `Cargo.lock` is committed); `report.sh` may use only `bash` and `jq`.
- No network access is required or expected at runtime.

## How you are evaluated

A held-back grader runs each component against freshly generated inputs and an
independent reference implementation of each spec (including the performance
budgets above), then runs the full `make pipeline` end to end. Partial credit
is awarded per component plus a bonus for the end-to-end pipeline, so a
correct component counts even if another is incomplete. Correctness on the
spec's edge cases is the whole game.
