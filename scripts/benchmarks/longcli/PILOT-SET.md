# LongCLI 3-lane pilot — pinned task set (2026-06-12)

Five tasks, identical set and order across all three lanes (vanilla /
managed-current / managed-density), full model window, `--n-concurrent 1`,
lanes strictly serialized. Selected per the density-first pilot brief:
span the benchmark's four task families, favor organically long tasks,
≤ 2 c-env tasks, and exclude anything a smoke run already touched
(`cs61_fa24_hog` — both 2026-06-12 smokes).

## The set (run in this order)

| # | task | family | image | why |
|---|---|---|---|---|
| 1 | `61810_fs` | feature_add | make-pytest | Longest xv6 lab by the only real metadata estimate in the suite (expert 800 min vs 480 for cow). Two independent kernel features (large files via doubly-indirect blocks, then symlinks) force a long two-arc session in a 12k-line codebase; 9 files to change. |
| 2 | `61810_lock` | project_refactor | make-pytest | The suite's only clean refactor: re-design kmem allocator (per-CPU freelists) + buffer cache (bucketed locks) to cut lock contention without behavior change. kalloctest/bcachetest emit voluminous perf dumps — organic noise pressure, exactly what noise-triggered pruning is for. |
| 3 | `cs61_fa24_scheme` | from_scratch (interpreter core) | make-pytest | The CS61A capstone: build a Scheme interpreter in Python. Largest cs61 project in the set (4 files, ~570 target lines, 5.3k-word spec); long doctest/test cycles. |
| 4 | `cmu15_445_p2` | bug_fix hybrid (+feature_add) | c-env (1/2) | B+Tree index over BusTub, *on top of a provided Project-1 implementation whose "completeness and correctness are not guaranteed"* (instruction text) — the set's closest thing to bug_fix: diagnose/repair P1 while building P2. Industrial-scale C++ codebase, heaviest task in the suite. |
| 5 | `ap1400_2_hw26` | from_scratch (system design) | c-env (2/2) | AP1400-2 HW2 (system design: implement a full system from skeleton headers) + HW6 (STL algorithms) in one task — a deliberate mid-task domain switch, 24 gtest cases. Distinct ecosystem from BusTub; good early-fact recall material (HW2 facts probed after HW6 work). |

## Family coverage caveat

The released 20-task set contains **no pure bug_fix task** (the four-family
taxonomy is the benchmark's authoring taxonomy; the shipped tasks skew
feature_add). `cmu15_445_p2` is the bug-fix-adjacent pick — its P1 layer is
explicitly unverified and historically buggy, so P2 progress requires fault
localization and repair in inherited code.

## Other candidates, and why not

- `61810_cow` (feature_add, expert 480 min) — dominated by `61810_fs` (800 min,
  two-feature arc); one xv6 feature_add slot is enough next to the lock refactor.
- `61810_util` (from-scratch utilities) — five small independent programs, not
  one long arc; also sits on LongCLI's own analysis ignorelist.
- `cs61_fa24_ants`/`cats` — single-file medium projects, shorter than scheme.
- `cmu15_445_p1` — subsumed by p2 (which contains a P1 anyway); p2 is longer.
- `ap1400_2_hw35` (BST + coffee shop) — interchangeable with hw26; hw26 chosen
  for the system-design (from-scratch) angle vs hw35's textbook BST.
- `cs61_fa24_hog` — contaminated (both smoke lanes ran it 2026-06-12).

## Order rationale

Three proven-image (make-pytest) tasks first, the two c-env tasks last — the
c-env base image was built today; if it misbehaves, three trials of signal
exist before it bites, and the failure point is identical in every lane.

## Lane parameters (pinned)

- model `gpt-5.5`, reasoning effort **xhigh in all three lanes** — the managed
  agent's default; the vanilla agent gets it via the new optional
  `reasoning_effort` kwarg (`-c model_reasoning_effort=...` appended to the
  stock exec command). The 2026-06-12 smokes ran vanilla at the codex default
  (`reasoning_effort: null` in the rollout) vs managed xhigh; the pilot
  equalizes.
- `command_timeout_sec=7000`, `--global-agent-timeout-sec` defaulted (task
  budget 7200s), `--n-attempts 1`.
- Managed lanes: `intendant_binary_path` = `intendant-managed-current` /
  `intendant-managed-density` (ELF x86_64 rebuilds of e0bcad80 /
  b49d6923 — both post-instrumentation, see PROVENANCE), full window (no
  `model_context_window` written).
- Auth: fresh `install -m 600 ~/.codex/auth.json` into a per-lane
  `~/tbench-codex-homes/longcli-pilot3-<lane>/` immediately before that lane
  starts; lanes serialized.
