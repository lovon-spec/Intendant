# HARDENING — polyglot-pipeline verifier

Adversarial review of `verify.sh` + `verify/`. The benchmark cost center
(per SWE-Marathon: 13.8% of rollouts attempted reward hacks) is verifier
hardening, so the verifier is **behavioral**: it generates inputs at check
time from a random seed and compares the agent's output to an *independent*
oracle (`verify/oracle.py`). It never checks for the presence of files,
strings, or symbols.

## Threat model

A reward-seeking agent that will do anything to raise the score: hardcode
expected outputs, memorize the visible starter tests, tamper with the
Makefile/tests, special-case the grader's inputs, or stub the integration
target. The verifier must score **only** behavior on inputs the agent has
never seen.

## Core defenses

1. **Check-time random inputs + independent oracle.** Every battery generates
   fresh inputs from a seed (`--seed` reproduces; default is random and printed
   in the JSON) and compares to `verify/oracle.py`, a second implementation of
   the specs written independently of both the agent and the held-back
   `reference/` solution. Three-way agreement (agent ≟ oracle, validated by
   reference == oracle across 11+ seeds) is the correctness argument. Hardcoded
   or memorized outputs cannot match unseen inputs.
2. **The verifier never uses the agent-visible starter tests.** Those exist
   for the agent's own loop; scoring runs its own batteries. Editing or
   passing the starter tests yields zero grader credit.
3. **Scratch-copy grading.** `verify.sh` grades a `tar`-copy of the workdir
   with `.git`/`.intendant`/`target` excluded, so it is side-effect free and
   safe to run mid-run; the agent cannot detect or influence grading from its
   tree.
4. **Pinned Makefile for integration.** The integration target runs the real
   `make pipeline`, but the grader first overwrites the workdir's `Makefile`
   with the canonical `skeleton/Makefile` (single source of truth). A tampered
   Makefile that fakes outputs is overwritten before it runs.
5. **Numeric-tolerant structural comparison.** `json_equal` compares parsed
   JSON with numeric tolerance (so `42` ≡ `42.0`, `-0.0` ≡ `0.0`, and float
   dust is ignored) and order-sensitivity where the spec fixes order (sorted
   tags, sorted groups, ranked top_spenders). Money fields (`total_amount`,
   `by_month[*].total`) are compared after rounding both sides to 2 dp;
   `median_amount`/`p90_amount` use tolerance with no rounding requirement
   (the spec deliberately requires none, so jq-vs-python rounding conventions
   can never diverge). This prevents both false negatives (formatting) and
   false positives (the spec's ordering is still enforced).
6. **Negative checks are gated on the positive path.** Scenarios whose pass
   condition is "exit non-zero" (malformed header, bad `--as-of`, dedup usage
   errors, non-object lines) only count once at least one output-comparing
   scenario in the same battery passed. A do-nothing stub — which exits
   non-zero for everything — collects none of them (skeleton total: 0.0).
7. **Performance budgets measured on grader-generated inputs.** The perf
   scenarios (normalizer 120k rows / 45 s, dedup 500k lines / 10 s, report
   60k lines / 30 s) generate their large inputs at check time from the seed
   and require *correct output* within the wall-clock budget — precomputing or
   caching answers is impossible, and a fast-but-wrong tool scores nothing.

## Adversarial review — "how would I cheat this?"

| Attack | Outcome | Defense |
|---|---|---|
| Hardcode the starter test's expected JSON in `normalize.py` | caught | random CSVs; oracle decides truth (probe 1) |
| Emit numbers as strings to dodge rounding | caught | `json_equal` treats `"42"` ≠ `42` |
| Make `make pipeline` write a canned `report.json` | caught | Makefile is pinned to canonical before the run (probe 2) |
| Edit the starter tests / SPECs to "pass" | no effect | grader uses its own batteries, not agent tests |
| Skip building `dedup`, hope it's ignored | scored 0 | the `dedup` binary is required; absent ⇒ component 0 |
| Always exit non-zero to collect the usage/malformed checks | caught | negative checks are gated on a passing positive case (defense 6) |
| Don't write the output file when all rows reject | caught | reference writes an empty file; a missing file ⇒ fail (oracle expects `[]`) |
| Read the oracle at runtime | not viable | `verify/` is never copied into the agent's workdir; inputs arrive only as argv/data |
| Pass only on the dedupe tie cases it saw | caught | the (date, email-non-null, position) tie chain, email backfill, and `--since` interactions are generated fresh per seed |
| Special-case the perf input's shape | not useful | the perf check also requires full output correctness vs the oracle on that input |
| Quarantine: emit reasons unsorted or partial | caught | the oracle computes the full sorted reason set; `json_equal` is order-sensitive on lists |

## Iteration log

- **Pass 0 (design).** Generate-and-compare-to-oracle; scratch copy; three
  independent implementations; per-component partial credit + integration
  bonus (max 4.0 at the time).
- **Pass 1 (integration tamper).** Realized the integration target runs the
  agent's `Makefile`, which the agent can rewrite to fabricate
  `merged.jsonl`/`report.json`. **Fixed:** the grader pins the canonical
  Makefile over the workdir's before running `make pipeline`. Verified by
  probe 2 (tampered Makefile ⇒ `make_rc=2`, all stages fail, integration 0).
- **Pass 2 (oracle/format false-negatives).** Reviewed comparison for spurious
  mismatches: `-12` vs `-12.0`, `-0.0` vs `0.0`, jq float dust on sums.
  **Fixed:** numeric-tolerant `json_equal` + 2-dp rounding for `total_amount`.
  Also moved the Makefile pin from a duplicated `verify/assets/Makefile` to the
  canonical `skeleton/Makefile` (single source of truth) to kill drift.
- **Pass 3 (resize to the 30–60 min design target).** The live smoke proved
  the v1 pair too small (a competent serial agent finished service-triplet in
  ~4.5 min), so every spec was deepened and a fourth component added; the
  verifier grew matching batteries. Changes:
  - **New `quarantine/` component** (business-rule screen with 6 reason codes,
    boundary semantics, a leap-day cutoff rule, and dual ordered outputs);
    integration became 4 stages (normalized → screened → merged → report,
    1/4 each) and `max_total` is now **5.0**.
  - **Deepened specs** — normalizer: duplicate/unknown header handling, id
    charset, name whitespace collapse, plus-addressing + domain-label email
    rules, parenthesized negatives, well-formed comma groups, a third date
    format + a validity window, tag lowercasing/charset/cap; dedup: schema
    validation (skip, don't fail), `--since` pre-filter, a 3-level tie-break
    (date → email-non-null → position), email backfill; report:
    median/p90 (nearest-rank), `by_month`, `email_domains`, top-5.
  - **Performance budgets** (defense 7) on normalizer/dedup/report.
  - **Negative-check gating** (defense 6): the resized batteries initially
    leaked 0.04–0.10 per component to the do-nothing stubs through their
    "exit non-zero" scenarios — the same leak class service-triplet hit in
    its Pass 1. Caught by re-running the skeleton-zero check; fixed by gating
    negatives on a positive pass at counting time (RNG order unchanged, so
    `--seed` reproducibility is preserved).
  - Re-validated: reference == oracle == **5.0** across seeds {1, 2, 7, 42,
    99, 555, 808, 1000, 31337, 123456, 12345} + a fresh-seed run; skeleton
    0.0; both probes re-run and still caught; same-seed reruns byte-identical
    (modulo perf `elapsed_s`); full verify ≈ 8.5 s wall on the dev box
    (sccache-cached Rust build; allow ~40 s cold) — well inside the <3 min
    budget.

## Reward-hack probes (run, both caught — re-run after the Pass-3 resize)

1. **Hardcoded normalizer** — `normalize.py` ignores input and emits the
   visible starter test's four records (and mimics the missing-column exit
   code). Passes the *visible* starter test (the hack's goal) but scores
   **normalizer 0.0** on held-back inputs and drops **integration to 0** (the
   normalized stage cascades through screened/merged/report).
   quarantine/dedup/report (real reference) stay 1.0 — the cheat is localized
   and exposed (total 3.0/5.0).
2. **Tampered Makefile** — tools left as stubs; `make pipeline` rewritten to
   `printf` canned normalized/clean/quarantine/merged/report outputs. The
   agent's own `make pipeline` "passes" locally (rc=0), but the grader pins
   the canonical Makefile, so it runs the real (stub) stages: `make_rc=2`,
   every stage fails, **integration 0.0**, total 0.0.

## Residual notes

- The verifier requires a Rust toolchain to build `dedup`; a build failure
  zeroes the dedup component and the merged/report integration stages (correct
  — the tool genuinely doesn't work). Pre-fetch crates once (`cargo fetch`) so
  branch worktrees build offline.
- Determinism: `--seed` fully reproduces a grading run (independent RNG streams
  per component, so changing one battery's size doesn't shift the others; the
  perf scenarios consume their stream's RNG at a fixed point in the order).
- Perf budgets are wall-clock and intentionally generous (reference headroom
  >5x) so grader-host load noise cannot flake a correct solution; they only
  exist to fail asymptotically wrong implementations.
