# Codex Managed-Context Benchmark — Constrained Window (2026-06-12)

> **Status: FINAL.** Both tiers are complete and analyzed: the PRIMARY tier
> (20 tasks x 2 attempts, context window 40k — the headline result) and the
> DEEP tier (8 tasks x 2 attempts, window 28k — a mechanism-robustness stress
> test at an artificial floor, §Deep Tier). Campaign timeline, total cost,
> and the follow-up tracks this data motivates are at the end.

This is the constrained-window follow-up to
`codex_lineage_benchmark_2026-05-27.md`. The May run showed reward parity at
lower managed cost — but Terminal-Bench never touched the context machinery
(zero compactions, zero rewinds in 44 trials). This run constrains the model
context window to 40k tokens specifically to force engagement, and it did:
33/40 managed trials produced rewind records (53 total) and 37/40 vanilla
trials auto-compacted (114 events).

**The headline is a real negative result for cliff-edge managed mode.**
Vanilla Codex beat Intendant-managed Codex on both reward (25/40 vs 20/40)
and cost ($51.96 vs $72.65) at w40. The deficit is not noise and it is not
context *quality* — managed context stayed denser-than-baseline and primers
carried 83% of facts across rewinds. The deficit is the *control flow and
price of the intervention itself*: every managed-specific lost trial sits in
the gate-forced engagement group, 8 of 20 managed misses ended *inside* the
management protocol (7 recovery-step-limit anchor-paging loops + 1
anchor-handoff dead-end), and the gate's cache-busting interrupts put 53% of
the lane's uncached input spend into the largest prompts. The numbers below
quantify each mechanism; they are the empirical case for the density-first
overhaul that landed after these binaries were built (`feat/density-policy`,
now merged at `b49d6923`: noise-triggered pruning, living-index primer).

## Environment

- Remote Terminal-Bench host: `user@192.168.1.206` (Debian)
- Terminal-Bench dataset: `/home/user/tbench-datasets/terminal-bench`
- Harbor venv: `/home/user/tbench-harbor-venv/bin/harbor`
- Benchmark binaries: `/home/user/projects/bench-binaries-20260611/{codex,intendant}`
  - `codex` = lineage fork `f7a06d81f` (ubuntu:22.04 / glibc 2.34 release build)
  - `intendant` = `bench/managed-harness` @ `edc13230` (debian:12 / glibc 2.36
    release build; `a4fd05ec` + pilot fixes: rollback-aware anchor catalog,
    autonomous density-gate continuation)
- Vanilla comparator: npm Codex `0.133.0`
- Agent defs: `/home/user/tbench-agents/` (June revision;
  `harbor_intendant_codex_agent:IntendantCodex` vs
  `harbor_persistent_codex_agent:PersistentAuthCodex`)
- Model: `gpt-5.5`, reasoning effort `xhigh`
- Window: `context_window=40000` both lanes. Vanilla gets
  `-c model_context_window=40000 -c model_auto_compact_token_limit=36000`;
  managed writes `model_context_window = 40000` into `$CODEX_HOME/config.toml`
  and Intendant forces `model_auto_compact_token_limit=i64::MAX` (no hidden
  compaction; rewind is the only pressure valve). Both lanes' rollouts report
  an effective `model_context_window` of **38,000** (95% of configured); the
  managed rewind-only gate sits at **32,300** (85% of 38,000, mirroring
  `mcp.rs`), vanilla auto-compacts at **36,000**.
- Auth: per-lane Codex auth homes refreshed immediately before launch
  (`/home/user/tbench-codex-homes/{managed-w40,vanilla-w40}`); lanes strictly
  serialized (managed finished before vanilla started).
- Pilot gate: PASSED at w40 with no retune (`pilot-managed-w40` 3/6,
  `pilot-vanilla-w40` 4/6 on 3 tasks x 2 — same direction as the full tier).
- Analysis tooling: `bench/managed-harness` @ `b49d6923`
  (`scripts/benchmarks/summarize_harbor_results.py`,
  `scripts/benchmarks/managed_density_report.py`).

Run artifacts:

- Managed (primary): `/home/user/tbench-jobs/managed-w40-p20/2026-06-12__01-35-17`
- Vanilla (primary): `/home/user/tbench-jobs/vanilla-w40-p20/2026-06-12__05-02-38`
- Managed (deep): `/home/user/tbench-jobs/managed-w28-d8/2026-06-12__08-43-17`
- Vanilla (deep): `/home/user/tbench-jobs/vanilla-w28-d8/2026-06-12__10-39-43`

The deep tier reuses everything above except: `context_window=28000` (reported
effective window **26,600** = 95%, hard window 28,000; managed rewind-only
gate **22,610** = 85% of 26,600; vanilla auto-compact **25,200** = 0.9 x 28,000
per the agent def), fresh per-lane auth homes
(`/home/user/tbench-codex-homes/{managed-w28,vanilla-w28}`), and strict
serialization again (managed finished 10:03, vanilla started 10:39).

## Terminal-Bench Summary (primary tier, w40)

| Lane | Trials | Reward | pass@2 | Cost | Input tokens | Cached (hit rate) | Output | Agent s (sum) | Job wall | Exceptions |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Intendant-managed Codex | 40 | 20.0/40, 0.500 | 0.60 | $72.65 | 32,709,403 | 24,740,096 (75.6%) | 681,139 | 23,032 | 9,075s | 2 (train-fasttext timeouts) |
| Vanilla Codex 0.133.0 | 40 | 25.0/40, 0.625 | 0.70 | $51.96 | 30,567,494 | 26,113,920 (85.4%) | 554,406 | 27,485 | 10,498s | 2 (train-fasttext timeouts) |

Context-machinery engagement: managed 53 rewind records across 33 trials
(plus 2 more trials that entered the recovery-required state without ever
completing a rewind), 0 compactions; vanilla 114 compaction events across 37
trials, 0 rewinds. Exceptions are the matched `train-fasttext` 3600s agent
timeouts in both lanes (also timed out in May).

Managed vs vanilla:

- Reward: managed **-5 tasks** (20 vs 25; mean 0.500 vs 0.625).
- Cost: managed **+39.8%** ($72.65 vs $51.96).
- Wall-clock: managed **-16.2% agent time** (23,032s vs 27,485s) and -13.6%
  job wall — managed is *faster*, just much more expensive per token.
- Cache: 75.6% vs 85.4% hit rate (decomposed in §Overhead).

For reference, the May-27 unconstrained run on the same task family:
managed 17/22 = vanilla 17/22, managed $33.37 vs $37.99 (-12.2%), zero
engagement in either lane. The constrained window flipped the sign of both
deltas; §May-27 comparison below explains why that is the expected
consequence of cliff-edge engagement rather than a contradiction.

## Task Matrix

Per task: two attempts per lane (sorted by trial suffix). `P/F` = reward 1/0,
then cost, agent wall-clock, `r<N>` = rewind records (managed) / `c<N>` =
compaction events (vanilla). `EXC` = harness exception (timeout).

| Task | Managed a1 | Managed a2 | Vanilla a1 | Vanilla a2 | M/V |
| --- | --- | --- | --- | --- | --- |
| build-cython-ext | F $1.00 175s r1 | P $1.25 338s r1 | P $1.69 415s c2 | P $2.24 706s c5 | 1/2 |
| configure-git-webserver | F $1.84 576s r1 | F $1.26 368s r1 | F $0.91 353s c1 | F $0.74 242s c1 | 0/0 |
| custom-memory-heap-crash | F $2.13 436s r4 | F $1.39 295s r1 | F $0.91 273s c2 | F $0.71 262s c2 | 0/0 |
| db-wal-recovery | F $4.08 507s r2 | P $0.19 74s r0 | F $0.99 424s c2 | F $0.90 456s c4 | 1/0 |
| extract-elf | F $1.01 322s r1 | F $0.72 264s r1 | F $0.82 296s c1 | F $0.80 388s c2 | 0/0 |
| financial-document-processor | P $1.62 357s r1 | P $1.21 352s r2 | P $0.86 288s c3 | P $0.98 293s c2 | 2/2 |
| gcode-to-text | F $1.57 324s r1 | F $2.73 532s r1 | F $1.59 383s c2 | F $0.76 176s c1 | 0/0 |
| large-scale-text-editing | P $1.31 386s r0 | P $0.67 238s r0 | P $0.72 261s c0 | P $0.80 306s c0 | 2/2 |
| llm-inference-batching-scheduler | P $1.03 356s r1 | P $1.49 499s r1 | P $1.10 450s c2 | P $1.42 671s c4 | 2/2 |
| make-mips-interpreter | F $2.77 762s r5 | F $2.12 223s r1 | P $1.48 1021s c6 | P $1.03 822s c6 | 0/2 |
| portfolio-optimization | P $1.27 457s r1 | P $0.89 343s r0 | P $0.81 291s c1 | P $1.05 493s c2 | 2/2 |
| regex-chess | P $1.54 665s r1 | P $2.87 1229s r1 | P $1.74 1357s c3 | F $1.79 2021s c3 | 2/1 |
| reshard-c4-data | P $1.25 444s r1 | P $0.87 361s r1 | P $1.67 550s c2 | P $1.33 415s c1 | 2/2 |
| rstan-to-pystan | F $1.99 221s r0 | F $2.44 364s r1 | P $1.17 958s c3 | P $1.60 967s c3 | 0/2 |
| sanitize-git-repo | P $3.95 456s r5 | P $2.44 545s r2 | F $1.44 531s c3 | P $1.40 673s c5 | 2/1 |
| schemelike-metacircular-eval | P $3.15 711s r2 | F $0.93 232s r1 | P $0.94 523s c3 | P $1.31 636s c4 | 1/2 |
| sqlite-with-gcov | P $0.84 237s r1 | F $1.98 189s r0 | P $0.87 281s c2 | P $1.07 256s c1 | 1/2 |
| train-fasttext | F EXC $4.62 3603s r3 | F EXC $3.49 3603s r1 | F EXC $4.43 3601s c12 | F EXC $3.21 3611s c12 | 0/0 |
| video-processing | F $2.80 818s r4 | F $2.19 527s r1 | F $1.36 455s c2 | P $1.59 699s c3 | 0/1 |
| write-compressor | P $0.75 282s r0 | P $0.98 358s r1 | P $1.17 461s c1 | P $0.57 220s c0 | 2/2 |

Reward flips (per-task, out of 2):

- **Managed losses (-7):** build-cython-ext (1/2 vs 2/2),
  make-mips-interpreter (0/2 vs 2/2), rstan-to-pystan (0/2 vs 2/2),
  schemelike-metacircular-eval (1/2 vs 2/2), sqlite-with-gcov (1/2 vs 2/2),
  video-processing (0/2 vs 1/2). Every one of the seven lost trials on these
  tasks except video-processing ended inside the management protocol (see
  §Failure taxonomy).
- **Managed wins (+3):** db-wal-recovery (1/2 vs 0/2 — the managed pass is a
  legitimate, unusually efficient 12-tool-call WAL-header repair at $0.19/74s;
  the other managed attempt cliff-rewound twice and failed at $4.08),
  regex-chess (2/2 vs 1/2), sanitize-git-repo (2/2 vs 1/2).
- **sanitize-git-repo — the May regression did NOT recur; it inverted.** In
  May the managed lane was the only sanitize failure. Here managed passed
  both attempts *through* 5- and 2-rewind sessions (the heaviest successful
  engagement in the lane; primer carry preserved the contaminated-path
  checklist across resets), while vanilla's failing attempt (`HGqEUVz`)
  missed `test_correct_replacement_of_secret_information` — it left
  contaminated paths unsanitized after 3 compactions.
- **Both-lane failures (5 tasks, 0/2 + 0/2):** configure-git-webserver
  (verifier asserts in both lanes, as in May), custom-memory-heap-crash (the
  May-documented Valgrind fd-limit environment failure, all four trials),
  gcode-to-text (flag case-transcription errors in both lanes — managed
  `AMSp7Fz` wrote `iZ` for `iz`, the same error class vanilla made in May),
  train-fasttext (3600s cap, both lanes, as in May), and **extract-elf — a
  new both-lane casualty of the constrained window** (both lanes passed it
  unconstrained in May; at w40 all four trials produced extractors whose
  output failed verifier compilation).

## Engagement-Conditional Reward

"Engaged" for managed = >=1 rewind record; the two trials that entered the
recovery-required gate but died without completing a rewind
(`rstan-to-pystan__CmUVh5b`, `sqlite-with-gcov__R8siwqY`, both r0) are
counted as engaged-forced in the three-way split. Vanilla engaged = >=1
compacted event.

| Group | Trials | Pass | Rate |
| --- | ---: | ---: | ---: |
| Managed, never engaged | 5 | 5 | **100%** |
| Managed, voluntary rewinds only (all records below the 32.3k gate) | 11 | 7 | 64% |
| Managed, gate-forced (>=1 record at/over the gate, or gate-death) | 24 | 8 | **33%** |
| Vanilla, never compacted | 3 | 3 | 100% |
| Vanilla, compacted | 37 | 22 | 59% |

(Flat two-way cut for comparability: managed engaged 15/33 = 45% vs vanilla
engaged 22/37 = 59%; both lanes' unengaged trials all passed.)

The managed deficit is **entirely concentrated in forced engagement**:

- The 4 failures in the voluntary-only group are configure-git-webserver x2
  and extract-elf x2 — tasks vanilla also failed 0/2. Voluntary, below-gate
  rewinds cost **zero reward relative to vanilla**.
- All nine managed-specific lost trials (the -7 task flips above) are in the
  gate-forced group.
- Both lanes degrade under engagement (constrained windows are simply harder:
  59% engaged vanilla vs 100% unengaged), but managed degrades much further
  (33% forced vs 59%), and that extra drop is the machinery, not the tasks.

## Rewind Pressure Bands (the overhaul datum)

All 53 rewind records predate the `pressure_at_rewind` instrumentation
(binaries were built before `feat/density-policy`); bands are recovered from
each record's archived pre-rewind rollout (last `token_count`, the exact
value the record writer would capture) against the reported 38,000 window:
`ok` < 32,300 (below the rewind-only gate = voluntary), `watch` 32,300-37,999
(at/over the gate), `high` >= 38,000 (past the reported window).

| Band | Records | Share | Reading |
| --- | ---: | ---: | --- |
| ok (voluntary) | 24 | 45% | model-initiated pruning at 29-83% fill |
| watch (gate zone) | 15 | 28% | forced at/over the 85% gate |
| high (over window) | 14 | 26% | cliff overshoot — up to 74,676 used (196% of window, `make-mips__gcwDMQC`); 6 records over 47k |
| critical | 0 | 0% | — |

Per-trial outcome by worst band: trials whose records are all `ok` pass 64%
(and lose nothing vs vanilla); trials with >=1 `high` record pass **3/11 =
27%**. The cliff, not the rewind, is what kills.

Cliff dynamics (from record timestamps + per-request token series):

- **Overshoot is single-turn:** the worst records were produced by one turn
  ingesting 13-37k tokens of tool output (e.g. three parallel
  `exec_command`s with `max_output_tokens` budgets of 12k/18k/16k fired from
  a 30.8k prompt). The gate can only interrupt *between* requests, so by the
  time it fires the prompt is already 43-75k.
- **14 rapid re-rewind pairs (<=150s apart):** 9 of the 14 `high`-band
  records were followed by another rewind, and in every such case the
  follow-up came 24-70s later at only 11-21k used — the model rewinds, then
  immediately rewinds again instead of resuming work (anchor repositioning
  without progress). The other 5 `high` records were single-rewind trials.
- **3 re-blow loops:** in `custom-memory-heap-crash__Kqr8nEm`,
  `make-mips-interpreter__WysfhkX`, and `sanitize-git-repo__mL4BnLv` the
  context jumped from ~13k back over the window within ~90-100s of a rewind —
  post-rewind re-derivation re-ran the same huge-output command and slammed
  the cliff again. Only sanitize survived this (and paid $3.95, 2.8x the
  vanilla pass price, for it).

Worked example, `make-mips-interpreter__WysfhkX` (failed, 5 rewinds), the
forced-rewind round-trip at ~06:03:

| Req | Input | Cached | What happened |
| --- | ---: | ---: | --- |
| r13 | 43,630 | 30,592 | 3 parallel objdump/strings calls; 3rd output replaced by the gate message ("pressure at or above the rewind-only threshold (43984/38000)... only get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, rewind_backout available"); Intendant aborts the turn (`<turn_aborted>` injected) |
| r14 | 46,999 | 8,192 | model calls `list_rewind_anchors` — the listing round-trip re-bills a 47k prompt at 17% cache |
| r15 | 47,726 | 8,192 | model calls `rewind_context`; the call is aborted ("aborted by user after 0.1s") and executed out-of-band → `thread_rolled_back`, primer injected |
| r16 | 15,154 | 8,192 | post-rewind continuation |

One forced episode = ~91k uncached input tokens ≈ **$0.46** before any
recovery work. A vanilla auto-compaction on the same hardware shape is one
mostly-cached summarization request ≈ **$0.02-0.05**, with no turn abort and
no tool-surface change: roughly a **10x per-cycle price difference**, before
counting indecision loops.

## Failure Taxonomy — all 20 managed misses

Class A — **recovery-step-limit indecision (7 trials, the headline failure
mode).** The trial crosses the rewind-only gate; the model calls
`list_rewind_anchors` repeatedly — usually re-listing offset 0 instead of
paging or committing (the catalog pages 5 anchors with `next_offset`; each
listing also *adds* an item to the thread, so the catalog grows 41→46 while
it loops); the fork's recovery follow-up step limit fires ("Managed context
recovery reached the follow-up step limit before reducing context");
Intendant kickstarts recovery (up to 2x, the pilot's autonomous continuation
fix); the model loops again; the session ends with the task half-done, final
message a forward-looking plan. Wall-clock at exit 175-532s — these trials
*had* 3,000+ seconds of budget left.

| Trial | Exit | Notes |
| --- | --- | --- |
| rstan-to-pystan__CmUVh5b | 221s, $1.99, r0 | 6+ offset-0 listings; 2 kickstarts; "refusing to mark the session complete"; **never completed any rewind** |
| rstan-to-pystan__oegRGCc | 364s, $2.44, r1 | step limit after post-rewind listings |
| build-cython-ext__sYNgg8k | 175s, $1.00, r1 | catalog had only 3 eligible anchors; final message is about checking pressure, not the task |
| sqlite-with-gcov__R8siwqY | 189s, $1.98, r0 | gate-death without a completed rewind |
| schemelike-metacircular-eval__rgugkxz | 232s, $0.93, r1 | `done_signal` 0.8s after the step-limit warning |
| make-mips-interpreter__gcwDMQC | 223s, $2.12, r1 | 196%-of-window overshoot, then step limit |
| gcode-to-text__CQnpwVq | 532s, $2.73, r1 | did page (offset 13) but never committed; vanilla also failed this task |

Class B — **anchor-handoff dead-end (1):**
`make-mips-interpreter__WysfhkX` (762s, r5). After 5 rewinds the density
handoff asked for another rewind; the model's last message: "No density
rewind applied. The only density candidate returned was a management/status
anchor, and the handoff explicitly disallowed management-tool anchors" —
then `done_signal`. The protocol cornered itself: management items polluted
the anchor catalog until no eligible anchor remained.

Class C — **both-lane task/environment failures (10):**
configure-git-webserver x2, custom-memory-heap-crash x2 (Valgrind fd-limit
environment issue, May-documented), extract-elf x2 (new both-lane
constrained-window casualty), gcode-to-text__AMSp7Fz (flag case error;
vanilla failed both attempts of this task too), train-fasttext x2 (3600s
timeouts, matched in vanilla), db-wal-recovery__NJSovog (vanilla 0/2 as
well; this trial additionally showed the worst paging — 16 anchor listings
for 2 rewinds — but the task itself defeated both lanes).

Class D — **ordinary quality failures, vanilla split (2):**
video-processing__GmXHqnQ (818s, 4 rewinds incl. 2 rapid pairs; implemented
a fragile analyzer that failed the verifier) and video-processing__zQupi3q
(527s, 1 watch-band rewind; same verifier failure; vanilla also failed 1 of
2). Context machinery added overhead but the misses look like solution
quality.

Net reward accounting: classes A+B on tasks vanilla swept = -7; managed-only
wins (db-wal, regex-chess, sanitize) = +3; video split = -1 → **-5**, the
entire topline gap. Remove the A/B protocol endings and the lanes are at
parity on this tier.

## Overhead Accounting

**Fitted pricing** (exact least-squares fit across all 80 trials, residual
<$0.0001): gpt-5.5 = $5.00/M uncached input, $0.50/M cached input, $30/M
output.

**Cost delta decomposition (managed - vanilla):**

| Component | Token delta | $ delta |
| --- | ---: | ---: |
| Uncached input | +3.52M | **+$17.58** |
| Output | +127k | +$3.80 |
| Cached input | -1.37M | -$0.69 |
| **Total** | | **+$20.69** |

**Structural surfaces (measured, corrects the planning assumption):**

| | Managed | Vanilla |
| --- | ---: | ---: |
| First-request prompt (system+tools+task) | 14,040 median (13,840-14,942) | 22,919 median (22,607-23,821) |
| Reported window | 38,000 | 38,000 |
| Forced-action ceiling | 32,300 (rewind-only gate) | 36,000 (auto-compact) |
| Working room per cycle | ~18.3k | ~13.1k |
| Cycles (resets) | 53 (1.3/trial) | 114 (2.9/trial) |
| Model requests | 1,265 | 1,034 |
| Mean prompt size | 26.0k | 29.7k |

The pre-run framing ("managed effective room ~26k vs vanilla ~37k") was
wrong on the vanilla side: vanilla 0.133.0's measured baseline is ~22.9k,
not ~3k, so managed actually ran with the *leaner* per-request surface
(~13.8-14.0k, matching the expected managed baseline) and *more* working
room per cycle, and it cycled 2.2x *less* often. **The handicap is not
room — it is the price and failure rate of each cycle.**

**Where the uncached tokens went** (every request bucketed; gate zone =
prompt >= 32,300):

| Bucket | Managed req / uncached | avg/req | Vanilla req / uncached | avg/req |
| --- | --- | ---: | --- | ---: |
| First request | 40 / 330k (4%) | 8.2k | 40 / 609k (14%) | 15.2k |
| Gate zone (>=32.3k prompt) | 236 / **4,269k (53%)** | **18.1k** | 316 / 1,119k (25%) | 3.5k |
| Within 8 req after reset | 283 / 1,202k (15%) | 4.2k | 433 / 1,553k (35%) | 3.6k |
| Other deep busts (<50% hit) | 60 / 1,137k (14%) | 19.0k | 34 / 798k (18%) | 23.5k |
| Normal | 646 / 1,127k (14%) | 1.7k | 211 / 385k (9%) | 1.8k |
| **Total** | **8,066k** | | **4,464k** | |

The single dominant term: **managed pays 18.1k uncached per request in the
pressure zone where vanilla pays 3.5k — a 5.1x penalty exactly where prompts
are biggest.** The gate-zone gap (+3.15M tokens, +$15.75) explains ~76% of
the total cost delta on its own. Mechanism (visible in the worked example):
the gate interrupt injects a `<turn_aborted>` message and swaps the tool
surface to the 5 rewind tools, invalidating the prompt prefix beyond a
shared ~8,192-token head, so the 38-48k prompts at the cliff are re-billed
nearly uncached — typically 2-3 times per episode (listing, rewind call,
sometimes a blocked ordinary call first). Vanilla's compaction does nothing
to the prefix until it rewrites history once, and its cache hit rate is flat
(~3.8-4.0k uncached/request) right through the compaction point.

**Hygiene round-trips:** 162 `list_rewind_anchors` calls for 53 completed
rewinds (**3.06 listings per rewind**; worst trial 16 listings / 2 rewinds),
53 `rewind_context`, 24 `get_status`, 0 `inspect_rewind_anchor`, 0
`rewind_backout`. >=1 recovery kickstart in 20/40 trials; step-limit
warnings in 10/40; 101 `<turn_aborted>` injections lane-wide. Hygiene tools
+ their outputs occupy a mean 3.1% (max 10.1%) of the billed prompt in
engaged subset trials (M2), and the hygiene round-trips are ~19% of the
lane's extra request count (1,265 vs 1,034).

**Output-side:** +127k output tokens (+$3.80) ≈ 53 rewind payloads (primer
median ~870 tok + preserve + next_steps ≈ 51k total) plus recovery-turn
planning and anchor-paging reasoning spread across the extra ~230 requests.

**Cache-hit summary:** 75.6% vs 85.4% lane-wide. Attribution: gate-zone
surface swaps + turn aborts (the 5.1x zone penalty above), 2-3 reduced-cache
recovery requests per rewind (~8.2k-head hits), and occasional head-boundary
busts where cache falls back to exactly the 13.7-14.7k static head
mid-trial. Vanilla shows none of these shapes; its only systematic misses
are first requests and the single history rewrite per compaction.

## Density Deep-Dive (M1-M4 on the 5-task subset)

Both lanes, both attempts of build-cython-ext, make-mips-interpreter,
rstan-to-pystan, sanitize-git-repo, financial-document-processor (20
trials; calibration TRUSTED at corrected p50 = 1.000 on every trial,
tiktoken-o200k_base). Full per-trial JSON + `report.md` regenerable via the
commands in §Reproduction.

| Metric | Managed (n=10) | Vanilla (n=10) |
| --- | ---: | ---: |
| Mean density (1 - stale share) | 0.973 | **0.991** |
| Tail density (last quarter) | 0.962 | **1.000** |
| Tier-1 old-output share (mean / max) | 0.219 / 0.852 | **0.057** / 0.595 |
| Turns-to-prune >2k outputs (median; pruned share) | 6.5; 86% of 35 | **2; 100% of 75** |
| Hygiene-tool prompt share (mean / max) | 3.1% / 10.1% | 0 / 0 |
| Re-derivation: dups (in-context / post-prune) | 38 (33/5) | 67 (43/**24**) |
| Post-prune re-derived token weight | **1,084** | 6,529 |
| Primer facts carried → next primer | 83% (245/296) | n/a |
| Primer facts referenced after rewind | 74% (432/587) | n/a |
| Primer tokens (median / max) | 1,100 / 1,853 | n/a |

Honest readout of the mechanism quality:

- **The managed *content* machinery works.** Primers are high quality (83%
  carry, 74% post-rewind reference, no runaway growth — the make-mips chain
  even shrank 1,355→1,004 tokens), and post-prune re-derivation is 5 cases /
  1.1k tokens vs vanilla's 24 / 6.5k — the primers genuinely prevent
  re-derivation ~6x better than compaction summaries. All 5 managed
  post-prune dups were `primer_ignored` (fact present, model re-ran anyway).
- **But the saved quantity is ~260x smaller than the machinery's price.**
  ~5.4k tokens of avoided post-prune re-derivation across these 10 trials vs
  +1.41M extra uncached input tokens on the same 10 trials (managed 2.75M vs
  vanilla 1.34M).
- **At w40, vanilla's effective context is *cleaner* than managed's by the
  staleness metrics.** Auto-compaction prunes every >2k output within a
  median of 2 requests (100% pruned), so old noise simply doesn't live long
  enough to accumulate (tier-1 share 0.057); managed lets outputs age ~6.5
  requests and prunes 86%, carrying 3.8x the old-output share (0.219).
  Episodic rewinds lose the janitorial race against per-turn compaction even
  while "density" stays nominally high in both lanes.
- Failing managed trials did not fail stale: `make-mips__WysfhkX` held
  density 1.000 through 5 rewinds and still failed — the failure is
  disruption and dead-ends, not noise.

## May-27 Comparison — what changed

| | May-27 (unconstrained) | Jun-12 (w40) |
| --- | --- | --- |
| Reward | managed 17/22 = vanilla 17/22 | managed 20/40 vs vanilla 25/40 |
| Cost | managed -12.2% | managed +39.8% |
| Engagement | none (0 compactions, 0 rewinds) | forced (53 rewinds / 114 compactions) |
| Cache | managed advantage (smaller prompts) | managed 75.6% vs vanilla 85.4% |

Same tasks, same model family, same harness lineage. Unconstrained, the
managed lane's leaner prompt surface (~14k vs ~23k baseline) made it
*cheaper* at equal reward, and the context machinery was never exercised —
the May report said exactly that and gated the feature on a synthetic stress
harness instead. The synthetic harness proved the *mechanics* (rewind fires,
anchors hold, gate blocks); it could not price the *economics*. w40 prices
them: when the window forces engagement, every engagement is a cliff-edge
event — interrupt, surface swap, full-price re-bill of the largest prompts,
round-trip, and a model-driven anchor decision under duress with a step
limit ticking. Vanilla's compaction is a worse memory mechanism (24
post-prune re-derivations, lost-fact failures like sanitize `HGqEUVz`) but
it is ~10x cheaper per cycle, invisible to the model, and it cannot
dead-end. At 2.9 cycles/trial, the cheap-dumb valve beats the
expensive-smart one.

## Implications for the density-first overhaul

This tier is the empirical case for the `feat/density-policy` redesign
(landed after these binaries were built; merged at `b49d6923`):

1. **Prune at ok/watch, never at the cliff.** Voluntary below-gate rewinds
   cost zero reward vs vanilla (7/11 with all 4 failures shared); gate-forced
   engagements pass 33%. Noise-triggered pruning moves all hygiene into the
   voluntary band by construction.
2. **The reset must not be a round-trip.** 3.06 catalog listings per rewind
   at 38-48k uncached tokens each is the dominant cost line (53% of uncached
   spend). A living-index primer maintained *during* normal turns removes
   the at-pressure listing/decision loop entirely.
3. **Protocol dead-ends must be impossible.** 8 of 20 misses ended in the
   step-limit loop or the no-eligible-anchor corner with hours of budget
   left. Whatever the model does, the harness must converge to *some*
   context reduction (auto-pick fallback anchor, exclude management/status
   items from the catalog — they polluted it until nothing eligible
   remained, and each listing made the catalog bigger), and a failed
   recovery must hand the task back, not end the session.
4. **Don't bust the cache at maximum prompt size.** The turn-abort +
   tool-surface swap at the gate re-bills the biggest prompts at 17% cache,
   2-3x per episode. Pressure interception needs a prefix-stable mechanism
   (constant tool surface, appended-not-injected control messages).
5. **Single-turn overshoot needs a budget guard.** The worst cliffs were
   parallel tool calls with 12-18k `max_output_tokens` budgets fired from
   30k prompts; the gate can only react after ingestion. Cap per-turn
   ingestion against remaining headroom.

What managed mode should keep: the primer/carry machinery (83%/74%, 6x less
re-derivation than compaction summaries) and the lean baseline (14k vs 23k,
which is also why managed remains 16% faster in wall-clock).

## Deep Tier (w28, 8 tasks x 2) — Mechanism Robustness at an Artificial Floor

> **Read this section as a stress test, not a product benchmark.** w28 is an
> artificial floor chosen so the context machinery must cycle several times
> per trial: vanilla's ~22.9k baseline prompt starts at **91% of its own
> auto-compact ceiling** (25,200) and managed's ~14.1k baseline at 62% of the
> rewind gate (22,610). No realistic deployment runs gpt-5.5 at a 28k window
> on these tasks. The question answered here is **mechanism robustness** —
> which w40 failure modes recur, amplify, or vanish when *every* trial is
> forced into heavy engagement — explicitly **not** which lane is the better
> product. The w40 primary tier above remains the headline result. The
> managed lane ran the **same pre-`feat/density-policy` binaries** as the
> primary tier, i.e. the OLD cliff-edge policy this report indicts.
>
> The stub for this section pre-registered the test: *"more forced cycles
> per trial should amplify the cliff-edge mechanisms quantified above; if it
> does not, that is evidence the w40 failure modes are threshold-tuning
> artifacts rather than structural."* Verdict below: **amplified — structural.**

### Summary (deep tier, w28)

| Lane | Trials | Reward | pass@2 | Cost | Input tokens | Cached (hit rate) | Output | Agent s (sum) | Job wall | Cap-outs |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Intendant-managed Codex | 16 | 2.0/16, 0.125 | 0.25 | $37.17 | 14,192,448 | 9,669,760 (68.1%) | 324,034 | 12,241 | 4,829s | **0** |
| Vanilla Codex 0.133.0 | 16 | 4.0/16, 0.250 | 0.375 | $18.73 | 10,196,042 | 8,239,616 (80.8%) | 161,037 | 25,445 | 9,245s | **11** (AgentTimeoutError) |

Both lanes collapse at this floor — w40 pass rates on these same 8 tasks
were managed 5/16, vanilla 11/16, so the window squeeze cost vanilla more
ground (-7) than managed (-3) on the matched subset while leaving vanilla
ahead — and they collapse in **opposite ways**: vanilla
burned 80% of the lane's aggregate task-cap budget (25,445s of 31,800s) and
died at the time cap in 11/16 trials; managed used 38% of budget, hit zero
caps and zero harness exceptions, and instead ended **13/16 sessions at the
recovery step limit**. Managed cost 1.98x vanilla ($2.32 vs $1.17 per trial)
while being 2.1x faster in agent wall-clock. Engagement was total: managed
40 rewind records across 16/16 trials (2.50/trial), vanilla 325 compaction
events across 16/16 trials (20.3/trial). Cost decomposition (managed -
vanilla): uncached input +2.57M = +$12.83, output +163k = +$4.89, cached
+1.43M = +$0.72 → +$18.44 (fitted pricing identical to the primary tier,
residual <$0.0001 across all 32 trials).

### Task Matrix (deep tier)

Same format as the primary matrix; both attempts sorted by trial suffix.
`P/F EXC` = the trial hit the task time cap (900s build-cython/gcode/sqlite,
1800s make-mips/rstan, 2400s schemelike, 3600s regex/train-fasttext).

| Task | Managed a1 | Managed a2 | Vanilla a1 | Vanilla a2 | M/V |
| --- | --- | --- | --- | --- | --- |
| build-cython-ext | P $2.75 644s r5 | F $2.00 337s r2 | F EXC $0.79 901s c12 | F EXC $0.84 900s c13 | 1/0 |
| gcode-to-text | F $1.43 346s r1 | F $1.75 358s r1 | F EXC $1.01 900s c10 | F EXC $1.03 901s c12 | 0/0 |
| make-mips-interpreter | F $2.31 379s r4 | F $1.77 308s r1 | F EXC $1.48 1801s c20 | F EXC $1.27 1801s c20 | 0/0 |
| regex-chess | F $3.15 1733s r5 | F $3.36 846s r4 | P EXC $2.47 3601s c46 | F $0.58 712s c5 | 0/1 |
| rstan-to-pystan | F $2.00 257s r1 | F $1.81 297s r1 | F EXC $1.17 1801s c19 | F EXC $1.27 1801s c25 | 0/0 |
| schemelike-metacircular-eval | F $1.09 178s r1 | P $2.55 614s r4 | P $0.49 735s c8 | F $1.24 1299s c19 | 1/1 |
| sqlite-with-gcov | F $1.04 272s r1 | F $1.90 334s r2 | P $0.53 639s c12 | P $0.58 419s c8 | 0/2 |
| train-fasttext | F $4.87 3561s r5 | F $3.39 1777s r2 | F EXC $1.97 3615s c48 | F EXC $2.02 3620s c48 | 0/0 |

Per-task reading (n=2, so single flips are noise; the *shapes* are the data):

- **Managed's lone task win, build-cython-ext (1/2 vs 0/2):** the pass went
  *through* 5 rewinds — including a 139%-of-window record and a
  no-eligible-anchor dead-end message it survived by declining the rewind
  ("No density-valid rewind anchor is available, so I'm leaving context
  unchanged") — at $2.75, 3.3x vanilla's cap-out price. Vanilla burned both
  900s caps in compaction churn (12-13 events) without converging.
- **Vanilla's wins, sqlite-with-gcov (2/2 vs 0/2) and regex-chess (1/2 vs
  0/2):** sqlite is the cleanest protocol-kill contrast — vanilla finished
  in 419-639s; managed died at the step limit at 272s/334s on a 900s cap
  with the build mid-flight. Vanilla's regex pass spent the *full* 3600s cap
  (46 compactions) and still passed because the work was already on disk
  when the cap hit (`P EXC`).
- **Both-lane zeros (5 tasks):** gcode (managed transcribed flag characters
  again, vanilla double-capped), make-mips (vanilla reached Doom's demo loop
  at both 1800s caps; managed step-limited at 308s/379s), rstan (vanilla
  capped mid-sampling x2), train-fasttext (training-dominated in both lanes,
  as at w40 and in May), schemelike split 1/1.

### Engagement: bands, conditional reward, and context quality

All 40 managed rewind records predate `pressure_at_rewind` (same binaries as
the primary tier); bands recovered from each record's archived pre-rewind
rollout via `summarize_harbor_results.py` (band source `rollout` for 40/40),
thresholds vs the reported 26,600 window: gate 22,610, window 26,600.

| Band | Records (w28) | Share | w40 share |
| --- | ---: | ---: | ---: |
| ok (voluntary) | 14 | 35% | 45% |
| watch (gate zone) | 15 | 38% | 28% |
| high (over window) | 11 | 28% | 26% |

The forced share grew 55%→65%, and the overshoot got *relatively* worse: max
54,067 used = **203% of the reported window** (193% of the 28,000 hard
window; the w40 max was 196% of its reported window), with 5 records past
the hard window (54.1k, 37.0k, 36.7k, 34.6k, 32.2k —
`make-mips__VF3n96r` the worst). Single-turn parallel-tool ingestion remains the
overshoot mechanism — and it is not managed-specific: **21% of vanilla's
requests (83/396) also exceeded the reported window intra-turn** (max
36,616); auto-compact can't pre-empt mid-turn ingestion either.

Engagement-conditional reward collapses to one row per lane at w28 — there
is no control group left, which is exactly what the tier was built to do:

| Group | Trials | Pass | Rate | w40 rate |
| --- | ---: | ---: | ---: | ---: |
| Managed, never engaged | 0 | — | — | 100% |
| Managed, voluntary rewinds only | 0 | — | — | 64% |
| Managed, gate-forced | 16 | 2 | **12.5%** | 33% |
| Vanilla, never compacted | 0 | — | — | 100% |
| Vanilla, compacted | 16 | 4 | **25%** | 59% |

Both lanes' engaged pass rates roughly halve from w40 (33→12.5%, 59→25%) —
the *ordering* is preserved and the forced-engagement gap (~2x) neither
opens nor closes. Context-quality metrics
(`managed_density_report.py` on all 32 trials; calibration TRUSTED at
corrected p50 1.000-1.001 everywhere):

| Metric | Managed (n=16) | Vanilla (n=16) |
| --- | ---: | ---: |
| Mean density / tail density | 0.950 / 0.884 | **1.000 / 1.000** |
| Tier-1 old-output share (mean) | 0.150 | **0.000** |
| Re-derivation, post-prune (count / tokens) | **7 / 2,626** (all `primer_ignored`) | 91 / 192,056 (all `compaction`) |
| Re-derivation, in-context (count / tokens) | 102 / 42,127 | 1 / 122 |
| Primer facts carried → next primer | 90% (577/644) | n/a |
| Primer facts referenced after rewind | 83% (928/1,124) | n/a |
| Primer tokens (median / max, n=40) | 814 / 1,556 | n/a |

The w40 readout sharpens into caricature at the floor. Vanilla compacting
every ~1.2 requests keeps its visible context *perfectly* clean (density
1.000 — nothing stale survives long enough to be measured) while losing so
much state that it re-derived **192k tokens of already-done work** (73x
managed's post-prune weight) — re-running builds and tests is a large part
of why 11 trials hit the wall clock. Managed's primer machinery, conversely,
got *better* under pressure (90%/83% vs 83%/74% at w40) and held post-prune
re-derivation to 2.6k tokens. (Managed's large *in-context* duplicate count
— 102 calls / 42k tokens vs vanilla's ~0 — is almost entirely `write_stdin`
poll loops on long-running processes, 70 of them in the two train-fasttext
trials and 14 in regex `4yFSpez`; process-watching behavior, not a context
mechanism. Hygiene tools are excluded from duplicate tracking.) The
mechanisms keep their w40 characters: compaction is clean-but-lossy,
managed rewind is retentive-but-disruptive.

### Timeout Taxonomy — who degrades how at 28k

**Vanilla: 11/16 trials died at the task cap** (verified per-trial
`AgentTimeoutError`: 4 @ 900s — build-cython x2, gcode x2; 4 @ 1800s —
make-mips x2, rstan x2; 3 @ 3600s — regex-chess `anNMUu4` (which still
passed: work was on disk before the cap), train-fasttext x2). Where the time
went: **18,715s — 73.6% of vanilla's total agent wall-clock — sat inside
compaction summarization gaps** (325 gaps, mean 57.6s, measured from each
`compacted` event back to the preceding rollout line; same measure at w40:
6,345s = 23.1%). The per-event price is flat across windows (~56-58s); the
*frequency* exploded (2.9 → 20.3/trial). regex `anNMUu4` spent 2,830s of its
3,601s inside summarization; the train-fasttext pair ~2,100-2,600s each. Add
the 192k tokens of post-compaction re-derivation above and vanilla's cap-out
mode is fully explained: it never stops, it just pays a ~58s tax every ~1.2
requests, re-runs what the summaries forgot, and runs out of clock.

**Managed: 0 cap-outs, 0 exceptions — 13/16 sessions ended at the recovery
step limit instead.** In every one of the 13, the step-limit event
("Managed context recovery reached the follow-up step limit before reducing
context") is the *final* session-log event — gap to session end 0s. Those
endings came at 178-846s on 900-3600s caps (the 12 step-limit *misses* left
51-93% of their task budget unused, mean ~72%), after 1-5 recovery
kickstarts (34 lane-wide, in 15/16 trials; w40: >=1 in 20/40) and 53-261s of
post-kickstart work. The remaining 3 trials: `9UEb8fw` (pass, survived a
no-eligible-anchor dead-end), `4yFSpez` (delivered a wrong regex solution at
1,733s — ordinary quality miss; its session ended at a kickstart), and
`krPycUZ` (3,561s ≈ the 3600s cap monitoring a real training run, the one
managed trial that behaved like vanilla's cap-outs).

So at 28k the degradation modes are mirror images: **vanilla degrades by
externally-imposed truncation after spending the whole budget on churn;
managed degrades by self-truncation with most of the budget unspent.** Note
the censoring asymmetry when reading the 2-vs-4 reward gap: a cap-out can
still pass if work landed early (vanilla got exactly one of those), while a
step-limit death ends the session mid-flight by construction.

### Recovery-step-limit recurrence — the pre-registered check

The primary tier's headline failure mode **recurred and amplified**, on
every axis the w40 taxonomy quantified:

| Mechanism | w40 (40 trials) | w28 (16 trials) |
| --- | ---: | ---: |
| Trials hitting the recovery step limit | 10 (25%) | **13 (81%)** |
| Misses ending inside the protocol | 8 of 20 (Class A+B) | **12 of 14** |
| `list_rewind_anchors` per completed rewind | 3.06 (162/53) | **4.60 (184/40)** |
| Worst listing loop | 16 listings / 2 rewinds | 14 listings / 1 rewind (`XLua3cM`); 23 / 4 (`cwtbQZc`) |
| Recovery kickstarts (trials with >=1) | 20/40 | 15/16 (34 total) |
| `<turn_aborted>` injections per trial | 2.5 (101/40) | **5.2 (83/16)** |
| `rewind_context` calls / completions | 53/53 | 45/40 (5 aborted/failed calls) |
| `inspect_rewind_anchor` / `rewind_backout` | 0 / 0 | 0 / 0 |
| Rapid re-rewind pairs (<=150s) / records | 14/53 (26%) | **15/40 (38%)** |
| Resets re-exceeding the window <=100s later | 7/53 (13%) | **14/40 (35%)** |

Two qualitative notes. First, the step limit is now visibly the session
*terminator*, not a warning the model recovers from — 13 terminal events,
including one in a passing trial (`eZwHvX3`: the solution was already
written when the protocol died, the verifier passed it posthumously).
Second, the Class-B anchor-catalog corner recurred too (`9UEb8fw`: handoff
demanded a rewind, catalog offered only management/status anchors) but the
model survived it this time by refusing to rewind and resuming work — at
w28's shallower depth that refusal was recoverable; at w40 it dead-ended.
Per the stub's pre-registered criterion, this is the structural verdict:
**the w40 failure modes are not threshold-tuning artifacts.**

### Cross-window per-cycle scaling

Uniform method across all four lanes (forced episode = the contiguous run
of gate-zone requests ending in a completed rewind, priced uncached;
vanilla cycle = uncached on the requests flanking each `compacted` event):

| | Managed w40 | Managed w28 | Vanilla w40 | Vanilla w28 |
| --- | ---: | ---: | ---: | ---: |
| Cycles per trial | 1.33 | 2.50 | 2.85 | **20.3** |
| Forced cycles (episodes) | 26 | 23 | 114 | 325 |
| Per-cycle uncached tokens (mean) | 107.8k | 61.1k | 13.4k | 9.4k |
| Per-cycle $ | $0.54 | $0.31 | $0.067 | $0.047 |
| Managed/vanilla per-cycle ratio | 8.0x | 6.5x | | |
| Per-cycle wall-clock (mean) | 81s | 62s | ~56s | ~58s |
| Gate-zone share of lane uncached | 53% | 59% | 25% | 89%* |

\* At w28 vanilla *lives* above the 22.6k cut (96% of its requests), so its
"gate-zone" share is just life near the ceiling, not interception cost; the
within-lane comparable number is the 73.6% of wall-clock in summarization.
(The uniform per-episode measure also refines the w40 worked example's ~10x
single-episode illustration to a measured lane-wide 8.0x.)

The scaling law this table fixes: **per-cycle price tracks prompt size
(window), cycle count tracks inverse headroom.** Managed's forced episode
got 43% cheaper when the window shrank 30% (smaller prompts at the cliff,
4.5-4.8 gate-zone requests per episode at both windows — the round-trip
*count* is policy-determined, not window-determined). Run the same
structure at a production window (200k+) and each forced episode re-bills
5x-larger prompts: the cliff tax grows with exactly the deployments that
matter, while vanilla's compaction stays a fixed ~58s/~$0.05 nuisance. The
gate-zone uncached penalty compressed from 5.1x to 2.2x (9.98k vs 4.59k
per request) for the same reason in reverse — vanilla at w28 pays its own
near-ceiling cache penalties — without changing the structural conclusion.

### Did managed thrash at 28k?

Quantified: **more churn per trial, but no runaway thrash — the failure
mode is fast protocol death, not budget burn.** Cycles rose to 2.50/trial
(max 5, in three trials: the build-cython pass, regex `4yFSpez`, train
`krPycUZ`); 38% of records were followed by another rewind within 150s; 35%
of resets re-exceeded the window within 100s (re-blow). But the lane
completed in 4,829s of job wall (vanilla: 9,245s), used 38% of its
aggregate cap budget, produced zero harness exceptions, and the launch
notes' thrash guard — *"stop the deep tier if >2 consecutive trials burn
the full 3600s thrashing"* — never tripped: the only near-cap trial
(`krPycUZ`, 3,561s) was supervising an actual fastText training run, and
its 5 rewinds were spread across 3,561s, not looping. The step limit is
doing its job as a circuit breaker in the narrow sense — it reliably stops
infinite loops — but at w28 it converted 81% of trials' recovery attempts
into session terminations, which is the wrong convergence (see
§Implications #3: a failed recovery must hand the task back).

### What the deep tier adds

1. The w40 protocol failure modes are **structural** (pre-registered test
   passed: step-limit endings 25%→81%, listings/rewind 3.06→4.60, rapid
   pairs 26%→38%).
2. The primer/content machinery is robust at the floor (90% carry, 83%
   post-rewind reference, 73x less post-prune re-derivation than
   compaction) — the *content* half of managed mode keeps earning its keep.
3. Vanilla's cheap valve stops scaling when per-cycle frequency explodes:
   73.6% of wall-clock in summarization, 11/16 cap-outs, 192k re-derived
   tokens. Cheap-dumb wins at moderate pressure (w40), loses its margin at
   extreme pressure — its 4/16 is no triumph either.
4. Neither mechanism handles single-turn overshoot (managed 203% of window;
   21% of vanilla requests over-window intra-turn) — per-turn ingestion
   budgeting (§Implications #5) is mechanism-independent.

## Campaign Timeline & Cost

| When (2026) | Stage | Outcome | Cost |
| --- | --- | --- | ---: |
| May 27 | Unconstrained baseline (`codex_lineage_benchmark_2026-05-27.md`): 22 tasks x 1, effort `low`, no window cap | Parity 17/22 = 17/22; managed -12.2% cost; zero context-machinery engagement | $72.32 (incl. $0.96 bare-patched sanity lane) |
| Jun 11 | Benchmark binaries built (`bench-binaries-20260611`): codex fork `f7a06d81f`, intendant `bench/managed-harness` @ `a4fd05ec` | — | — |
| Jun 11 23:44 | Pilot attempt 1 (`pilot-managed-w40-attempt1-mtlsfail`): 6 trials | All 6 errored at agent launch (mTLS/env mistake); zero model calls | $0 |
| Jun 11 23:49 | Pilot attempt 2 (`pilot-managed-w40-attempt2-bugdiag`): 3 trials | Surfaced the two pilot-era managed-context bugs; fixes committed as `edc13230` (rollback-aware anchor catalog, autonomous density-gate continuation), intendant binary rebuilt at that sha | $4.18 |
| Jun 12 00:38 / 01:07 | Pilot gate (`pilot-managed-w40` 3/6, `pilot-vanilla-w40` 4/6) | PASSED with no retune | $15.73 |
| Jun 12 01:35→04:06 / 05:02→07:57 | **Primary tier** `managed-w40-p20` / `vanilla-w40-p20` (20 tasks x 2, w40, effort `xhigh`) | Managed 20/40 vs vanilla 25/40; +39.8% cost | $124.61 |
| Jun 12 08:43→10:03 / 10:39→13:13 | **Deep tier** `managed-w28-d8` / `vanilla-w28-d8` (8 tasks x 2, w28) | Managed 2/16 vs vanilla 4/16; mechanisms amplified | $55.90 |

Version pins for the June campaign: lineage-fork codex `f7a06d81f`
(ubuntu:22.04 build), vanilla npm codex `0.133.0`, intendant
`bench/managed-harness` @ `edc13230` (= `a4fd05ec` + the pilot-era fixes
above; debian:12 build), model `gpt-5.5` at reasoning effort `xhigh`,
analysis tooling @ `b49d6923`. Note the May baseline ran at effort `low` —
the May/June cost levels are not directly comparable, the within-June
managed-vs-vanilla deltas are.

**Total campaign cost: $200.42** for the June constrained-window campaign
(133 trials inventoried in `INVENTORY-20260612.txt`: $0 + $4.18 + $15.73 +
$124.61 + $55.90, pilot attempts priced from rollout token counts at the
fitted rates where `result.json` usage is absent); **$272.74** for the full
program including the May-27 baseline.

**Fission usage: 0.** No trial in any lane of the campaign produced a
fission ledger (inventory: "fission usage (trials with a
fission_ledger.json): 0" across all 133 trials; the summarizer confirms 0
groups / 0 branches in all four June lanes). The fission machinery shipped
in these binaries but was never triggered at constrained windows — every
context-pressure event resolved through the rewind/compaction paths
measured above, so nothing in this report's economics is attributable to
fission.

## What This Motivates

This campaign bought, for ~$200, a measured mechanism-by-mechanism bill of
what cliff-edge managed context actually costs. The numbers that matter:
**33% pass under gate-forced recovery vs 64% under voluntary rewinds**
(w40; 12.5% when w28 forces everything), the **5.1x gate-zone uncached
penalty** (w40; the gate interrupt re-bills the largest prompts at ~17%
cache 2-3x per episode), **cache-prefix busting** by the turn-abort +
tool-surface swap (53-59% of lane uncached spend lands in the gate zone),
and **recovery-loop deaths** (step-limit endings in 25% of w40 trials →
81% at w28; 3.06 → 4.60 listings per rewind; the catalog that grows as you
page it). Each maps to a follow-up track already in flight:

1. **Density-first policy** — `feat/density-policy`, merged at `b49d6923`;
   built + live-validated. Kills the cliff by construction: noise-triggered
   pruning operates in the ok/watch band where this report measures zero
   reward cost vs vanilla, and the living-index primer is maintained during
   normal turns so no at-pressure listing/decision loop exists. Directly
   targets the 33%-vs-64% split and the band distribution (45% voluntary at
   w40 shrinking to 35% at w28 — the old policy drifts cliff-ward exactly
   when pressure rises).
2. **Recovery robustness** — `fix/recovery-robustness` (idempotent,
   dead-end-proof anchor catalog; state-aware kickstart/gate texts that
   commit from a catalog already in view) plus the fork-side
   `fix/recovery-instruction`; built + live-validated, with listings per
   rewind measured 3.06 → <=1 in live validation. Directly targets the
   step-limit deaths (7 w40 misses, 12 w28 misses), the offset-0 re-listing
   loop, the catalog-pollution corner (`WysfhkX` w40, survived-by-luck
   `9UEb8fw` w28), and the rule that a failed recovery must hand the task
   back rather than end the session.
3. **Cache-stable gating** — designed, pending pilot data. Targets the
   prefix-busting economics: constant tool surface and appended-not-injected
   control messages so pressure interception stops re-billing 38-48k
   prompts; the cross-window scaling result (per-cycle price tracks window
   size — $0.31 at w28, $0.54 at w40, growing with prompt size into
   production windows) is the quantified reason this track exists.

What the campaign says to *keep*: the primer/carry machinery (83-90% fact
carry, 73x less post-prune re-derivation than compaction at w28) and the
lean ~14k baseline (which is also why managed stays 16% (w40) to 52% (w28)
faster in agent wall-clock). The mechanisms are sound; the *policy* around
them — when to engage, what it costs to engage, and what happens when
engagement stalls — is what the three tracks replace. A follow-up
constrained-window run on the density-first binaries, against this report
as the baseline, is the natural next benchmark.

## Reproduction

Lane launches (full file: `/home/user/tbench-jobs/FULLRUN-COMMANDS-20260611.md`
on the host; auth homes refreshed per-lane immediately before launch):

```bash
PRIMARY_TASKS="-i build-cython-ext -i configure-git-webserver -i custom-memory-heap-crash -i db-wal-recovery -i extract-elf -i financial-document-processor -i gcode-to-text -i large-scale-text-editing -i llm-inference-batching-scheduler -i make-mips-interpreter -i portfolio-optimization -i regex-chess -i reshard-c4-data -i rstan-to-pystan -i sanitize-git-repo -i schemelike-metacircular-eval -i sqlite-with-gcov -i train-fasttext -i video-processing -i write-compressor"

# managed lane
cd /home/user/tbench-agents && /home/user/tbench-harbor-venv/bin/harbor run \
  -p /home/user/tbench-datasets/terminal-bench $PRIMARY_TASKS \
  --agent-import-path harbor_intendant_codex_agent:IntendantCodex -m gpt-5.5 \
  --ak binary_path=/home/user/projects/bench-binaries-20260611/codex \
  --ak intendant_path=/home/user/projects/bench-binaries-20260611/intendant \
  --ak reasoning_effort=xhigh --ak context_window=40000 \
  --ae CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/managed-w40/auth.json \
  -n 4 -k 2 --debug -o /home/user/tbench-jobs/managed-w40-p20

# vanilla lane (after managed completes)
cd /home/user/tbench-agents && /home/user/tbench-harbor-venv/bin/harbor run \
  -p /home/user/tbench-datasets/terminal-bench $PRIMARY_TASKS \
  --agent-import-path harbor_persistent_codex_agent:PersistentAuthCodex -m gpt-5.5 \
  --ak version=0.133.0 --ak reasoning_effort=xhigh --ak context_window=40000 \
  --ae CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/vanilla-w40/auth.json \
  -n 4 -k 2 --debug -o /home/user/tbench-jobs/vanilla-w40-p20
```

Deep tier: identical launches with `$DEEP_TASKS` instead of
`$PRIMARY_TASKS`, `--ak context_window=28000`, auth homes
`{managed-w28,vanilla-w28}`, and `-o .../{managed-w28-d8,vanilla-w28-d8}`
(w28 not 24k: the managed baseline is ~13.8k + ~8k post-rewind headroom, so
24k sits on the floor; 28k forces many cycles while staying workable):

```bash
DEEP_TASKS="-i build-cython-ext -i gcode-to-text -i make-mips-interpreter -i regex-chess -i rstan-to-pystan -i schemelike-metacircular-eval -i sqlite-with-gcov -i train-fasttext"
```

Analysis (run from this repo; rsync the run dirs locally first, excluding the
heavyweight `file_snapshots`/`frames` subdirs):

```bash
rsync -a --exclude file_snapshots --exclude frames \
  user@192.168.1.206:/home/user/tbench-jobs/managed-w40-p20/2026-06-12__01-35-17/ /tmp/mbench/managed-w40-p20/
rsync -a \
  user@192.168.1.206:/home/user/tbench-jobs/vanilla-w40-p20/2026-06-12__05-02-38/ /tmp/mbench/vanilla-w40-p20/

python3 scripts/benchmarks/summarize_harbor_results.py \
  /tmp/mbench/managed-w40-p20 /tmp/mbench/vanilla-w40-p20 \
  --csv /tmp/mbench/trials.csv --lanes-csv /tmp/mbench/lanes.csv

python3 scripts/benchmarks/managed_density_report.py \
  /tmp/mbench/{managed,vanilla}-w40-p20/{make-mips-interpreter,rstan-to-pystan,build-cython-ext,sanitize-git-repo,financial-document-processor}__* \
  --out /tmp/mbench/density-subset --no-plot
```

Deep-tier analysis, same tools (density on all 32 trials):

```bash
rsync -a --exclude file_snapshots --exclude frames \
  user@192.168.1.206:/home/user/tbench-jobs/managed-w28-d8/2026-06-12__08-43-17/ /tmp/mbench/managed-w28-d8/
rsync -a \
  user@192.168.1.206:/home/user/tbench-jobs/vanilla-w28-d8/2026-06-12__10-39-43/ /tmp/mbench/vanilla-w28-d8/

python3 scripts/benchmarks/summarize_harbor_results.py \
  /tmp/mbench/managed-w28-d8 /tmp/mbench/vanilla-w28-d8 \
  --csv /tmp/mbench/trials-w28.csv --lanes-csv /tmp/mbench/lanes-w28.csv

python3 scripts/benchmarks/managed_density_report.py \
  /tmp/mbench/{managed,vanilla}-w28-d8/*__* \
  --out /tmp/mbench/density-w28 --no-plot
```

The request-level buckets, episode pricing, and cross-window scaling tables
were computed from the rollout `token_count` series with the same bucket
definitions as the primary tier (validated by exact reproduction of the
primary tier's published request counts, bucket totals, first-prompt
medians, hygiene-tool counts, and overshoot maximum before being applied to
w28).

## Limitations

- 2 attempts per task; per-task flips of +-1 are within attempt noise —
  the engagement-conditional and taxonomy results, which aggregate across
  tasks, are the load-bearing findings.
- Rewind-record pressure bands use the archived pre-rewind rollout fallback
  (all 53 w40 records and all 40 w28 records; the records predate the
  `pressure_at_rewind` fields) — band source is uniform, so the
  distributions are internally consistent.
- Primary-tier density/M-metrics are computed on a 5-task subset (20
  trials), not the full 80; the deep tier's density metrics cover all 32
  w28 trials.
- The vanilla lane ran 3.5h after the managed lane (serialized); both lanes
  hit the same org prompt-cache, and the first-request cache behavior was
  symmetric (2,432-token static-preamble hits only).
- `db-wal-recovery`'s managed pass is legitimate but unusually efficient;
  with n=2 it contributes a +1 flip that should not be over-read.
- Deep tier: rewards sit near the floor (2/16 vs 4/16), so the w28 reward
  gap itself carries wide error bars; the load-bearing w28 results are the
  mechanism counts (step-limit endings, cycles/trial, band shares,
  summarization wall-clock), which aggregate many events per trial.
- The w28 lanes' losses are censored differently — vanilla's by the
  external task cap (partial work still verifiable; one cap-out passed),
  managed's by the protocol's own step limit (always mid-flight) — so the
  reward comparison at w28 compares truncation regimes, not just solution
  quality. This is intended (it is the mechanism contrast under study) but
  it is not a clean quality measurement.
- At w28, vanilla's per-cycle working room (~0.3-2.3k tokens between its
  post-compaction prompt and the 25,200 auto-compact ceiling) makes the
  per-request bucket shares interpretation-different across lanes; cross-
  lane bucket comparisons at w28 are reported but the within-lane
  wall-clock and re-derivation numbers are the reliable contrasts.
- Pilot lanes did not record usage in `result.json`; their costs in the
  campaign total are priced from rollout `token_count` series at the fitted
  rates (validated exact on the one pilot trial that did record cost).
