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
   reference == oracle across 10+ seeds) is the correctness argument. Hardcoded
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
   tags, sorted groups, ranked top_spenders). `total_amount` is compared after
   rounding both sides to 2 dp. This prevents both false negatives (formatting)
   and false positives (the spec's ordering is still enforced).

## Adversarial review — "how would I cheat this?"

| Attack | Outcome | Defense |
|---|---|---|
| Hardcode the starter test's expected JSON in `normalize.py` | caught | random CSVs; oracle decides truth (probe 1) |
| Emit numbers as strings to dodge rounding | caught | `json_equal` treats `"42"` ≠ `42` |
| Make `make pipeline` write a canned `report.json` | caught | Makefile is pinned to canonical before the run (probe 2) |
| Edit the starter tests / SPECs to "pass" | no effect | grader uses its own batteries, not agent tests |
| Skip building `dedup`, hope it's ignored | scored 0 | the `dedup` binary is required; absent ⇒ component 0 |
| Don't write the output file when all rows reject | caught | reference writes an empty file; a missing file ⇒ fail (oracle expects `[]`) |
| Read the oracle at runtime | not viable | `verify/` is never copied into the agent's workdir; inputs arrive only as argv/data |
| Pass only on the dedupe tie cases it saw | caught | tie-break (newest date, then largest position) is generated fresh with random positions and tag unions |

## Iteration log

- **Pass 0 (design).** Generate-and-compare-to-oracle; scratch copy; three
  independent implementations; per-component partial credit + integration
  bonus (max 4.0).
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
  Re-validated reference == oracle == 4.0 across seeds {1,2,7,42,99,1000,
  31337,555,808,123456}.

## Reward-hack probes (run, both caught)

1. **Hardcoded normalizer** — `normalize.py` ignores input and emits the
   visible starter test's three records. Passes the *visible* starter test
   (the hack's goal) but scores **normalizer 0.0** on held-back inputs and
   drops **integration to 0** (the normalized stage cascades). dedup/report
   (real reference) stay 1.0 — the cheat is localized and exposed.
2. **Tampered Makefile** — tools left as stubs; `make pipeline` rewritten to
   `printf` canned `merged.jsonl`/`report.json`. The agent's own
   `make pipeline` "passes" locally, but the grader pins the canonical Makefile,
   so it runs the real (stub) stages: `make_rc=2`, every stage fails,
   **integration 0.0**.

## Residual notes

- The verifier requires a Rust toolchain to build `dedup`; a build failure
  zeroes the dedup component and the merged/report integration stages (correct
  — the tool genuinely doesn't work). Pre-fetch crates once (`cargo fetch`) so
  branch worktrees build offline.
- Determinism: `--seed` fully reproduces a grading run (independent RNG streams
  per component, so changing one battery's size doesn't shift the others).
