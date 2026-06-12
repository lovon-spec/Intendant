# LongCLI lane invocations (.206 bench host)

All commands run on the bench host as `user`. One-time setup is at the
bottom. Conventions shared with the harbor lanes: binaries from
`~/projects/bench-binaries-<rev>/`, per-lane auth homes under
`~/tbench-codex-homes/`, jobs under `~/longcli-jobs/`, lanes strictly
serialized (managed completes before vanilla starts), `--n-concurrent 1`
always (one ChatGPT auth chain per lane; trials must not race the refresh).

Both lanes were smoked end-to-end on `cs61_fa24_hog` 2026-06-12
(`~/longcli-jobs/smoke-{vanilla,managed}-hog-20260612`).

```bash
# Shared shell prelude for every invocation below
cd ~/longcli-bench                       # pinned checkout (fetch-longcli.sh)
export PYTHONPATH=/home/user/longcli-agents   # tb_*_codex_agent.py + setup templates
TB=~/longcli-venv/bin/tb
BIN=/home/user/projects/bench-binaries-20260611   # codex fork + intendant
MODEL=gpt-5.5
```

## Smoke (one cheap task per lane — what was already run)

```bash
# Lane auth (staged fresh per lane, per host convention)
mkdir -p ~/tbench-codex-homes/longcli-smoke
install -m 600 ~/.codex/auth.json ~/tbench-codex-homes/longcli-smoke/auth.json

# Vanilla
CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/longcli-smoke/auth.json \
nohup $TB run --dataset-path tasks_long_cli --task-id cs61_fa24_hog \
  --agent-import-path tb_persistent_codex_agent:PersistentAuthCodex \
  --model $MODEL \
  --agent-kwarg command_timeout_sec=900 --global-agent-timeout-sec 1200 \
  --n-concurrent 1 --n-attempts 1 \
  --output-path /home/user/longcli-jobs --run-id smoke-vanilla-hog-$(date +%Y%m%d) \
  > /home/user/longcli-jobs/smoke-vanilla-hog-$(date +%Y%m%d).launch.log 2>&1 &

# Managed
CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/longcli-smoke/auth.json \
nohup $TB run --dataset-path tasks_long_cli --task-id cs61_fa24_hog \
  --agent-import-path tb_intendant_codex_agent:IntendantCodex \
  --model $MODEL \
  --agent-kwarg codex_binary_path=$BIN/codex \
  --agent-kwarg intendant_binary_path=$BIN/intendant \
  --agent-kwarg command_timeout_sec=1100 --global-agent-timeout-sec 1400 \
  --n-concurrent 1 --n-attempts 1 \
  --output-path /home/user/longcli-jobs --run-id smoke-managed-hog-$(date +%Y%m%d) \
  > /home/user/longcli-jobs/smoke-managed-hog-$(date +%Y%m%d).launch.log 2>&1 &
```

## 5-task pilot (×2 lanes)

`PILOT_TASKS` below is a placeholder spread (3 make-pytest + 2 c-env, mixing
xv6 / CS61A / BusTub / AP1400) — **replace with the selected pilot set** and
keep it identical across lanes. Full task budget (7200s) applies;
`command_timeout_sec=7000` keeps the in-container command (and therefore the
auth persist-back) inside it.

```bash
PILOT_TASKS="-t 61810_cow -t 61810_util -t cs61_fa24_cats -t cmu15_445_p0 -t ap1400_2_hw26"

# Lane auth — fresh copy per lane immediately before launch
mkdir -p ~/tbench-codex-homes/longcli-pilot-{managed,vanilla}
install -m 600 ~/.codex/auth.json ~/tbench-codex-homes/longcli-pilot-managed/auth.json
install -m 600 ~/.codex/auth.json ~/tbench-codex-homes/longcli-pilot-vanilla/auth.json

# Lane 1 — managed (run to completion before lane 2)
CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/longcli-pilot-managed/auth.json \
nohup $TB run --dataset-path tasks_long_cli $PILOT_TASKS \
  --agent-import-path tb_intendant_codex_agent:IntendantCodex \
  --model $MODEL \
  --agent-kwarg codex_binary_path=$BIN/codex \
  --agent-kwarg intendant_binary_path=$BIN/intendant \
  --agent-kwarg command_timeout_sec=7000 \
  --n-concurrent 1 --n-attempts 1 \
  --output-path /home/user/longcli-jobs --run-id pilot-managed-$(date +%Y%m%d) \
  > /home/user/longcli-jobs/pilot-managed-$(date +%Y%m%d).launch.log 2>&1 &

# Lane 2 — vanilla (after lane 1 finishes)
CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/longcli-pilot-vanilla/auth.json \
nohup $TB run --dataset-path tasks_long_cli $PILOT_TASKS \
  --agent-import-path tb_persistent_codex_agent:PersistentAuthCodex \
  --model $MODEL \
  --agent-kwarg command_timeout_sec=7000 \
  --n-concurrent 1 --n-attempts 1 \
  --output-path /home/user/longcli-jobs --run-id pilot-vanilla-$(date +%Y%m%d) \
  > /home/user/longcli-jobs/pilot-vanilla-$(date +%Y%m%d).launch.log 2>&1 &
```

Runtime: tasks range from ~3 min (hog, observed) to the full 2 h budget (the
paper reports most agents stall on the hard tasks). Plan **≤ 10 h per 5-task
lane** worst-case, ~3–6 h typical → pilot ≈ one day wall-clock with lanes
serialized.

With probe sets authored (post-pilot-selection), add to the managed lane:

```bash
  --agent-kwarg probes_dir=/home/user/longcli-agents/probe-sets \
```

and run vanilla probes post-hoc per trial (see ../probes/protocol.md):

```bash
~/longcli-venv/bin/python /home/user/longcli-agents/inject_probes.py vanilla \
  --codex-home <trial>/agent-logs/codex-home \
  --codex-bin <nvm or npm codex 0.133.0 on the host> \
  --probes /home/user/longcli-agents/probe-sets/<task>.json \
  --out <trial>/probe_answers.json
```

## 20-task full run (×2 lanes)

Same shape; the full set is every directory under `tasks_long_cli/` except
the `terminal-bench_task` example:

```bash
FULL_TASKS="-t 61810_cow -t 61810_fs -t 61810_lock -t 61810_mmap -t 61810_net \
 -t 61810_pgtbl -t 61810_syscall -t 61810_thread -t 61810_traps -t 61810_util \
 -t ap1400_2_hw26 -t ap1400_2_hw35 -t cmu15_445_p0 -t cmu15_445_p1 -t cmu15_445_p2 \
 -t cs61_fa24_ants -t cs61_fa24_cats -t cs61_fa24_hog -t cs61_fa24_hw08 -t cs61_fa24_scheme"
```

Swap `$PILOT_TASKS` → `$FULL_TASKS`, run-ids `full-managed-…` /
`full-vanilla-…`, fresh auth homes `longcli-full-{managed,vanilla}`. Budget
**≤ 40 h per lane** worst-case (20 × 2 h), realistically ~12–24 h; lanes
serialized → 1–3 days wall-clock. Disk: ~1–2 GB per lane run dir (rollouts +
asciinema casts).

## Scoring / aggregation

Per-run: `results.json` + per-trial `test_output/metrics_turn1.json`
(`f2p_is_pass`, `f2p_step_score`, `p2p_is_pass`, `p2p_step_score`). LongCLI's
aggregate tooling works across run dirs:

```bash
~/longcli-venv/bin/python scripts_python/longcli_aggregate_results.py \
  --input-dirs /home/user/longcli-jobs/full-managed-<date> /home/user/longcli-jobs/full-vanilla-<date> \
  --tasks-dir tasks_long_cli \
  --output-json long_cli_summary.json --output-csv long_cli_summary.csv --tables-dir .
```

(Note: the aggregator parses run-ids of the form
`<agent>_<model>_<task>_<n>_<turns>` for some rollups; the per-trial
metrics_turn1.json files are authoritative either way.)

Probe grading, per trial (both lanes):

```bash
~/longcli-venv/bin/python /home/user/longcli-agents/grade_probes.py \
  --probes /home/user/longcli-agents/probe-sets/<task>.json \
  --answers <trial>/agent-logs/probe_answers.json \
  --codex-home <trial>/agent-logs/codex-home \
  --rewind-archive <trial>/agent-logs/intendant/context_rewinds \
  --out <trial>/probe_grades.json
```

## One-time setup (already done 2026-06-12)

```bash
# 1. Pinned checkout + venv (python 3.13)
<repo>/scripts/benchmarks/longcli/fetch-longcli.sh ~/longcli-bench
python3 -m venv ~/longcli-venv
~/longcli-venv/bin/pip install -e ~/longcli-bench
~/longcli-venv/bin/pip install websockets        # probes injector (managed)

# 2. Base images (make-pytest: 15/20 tasks; c-env: 5/20, heavier — gtest build)
docker build -f ~/longcli-bench/longcli_dockerImage/Dockerfile.make-pytest-base \
  -t tb/make-pytest:v0 ~/longcli-bench/longcli_dockerImage
docker build -f ~/longcli-bench/longcli_dockerImage/Dockerfile.c-env-base \
  -t tb/c-env:v0 ~/longcli-bench/longcli_dockerImage   # pending — build before any c-env task

# 3. Agent files
mkdir -p ~/longcli-agents
cp <repo>/scripts/benchmarks/longcli/tb_*_codex_agent.py \
   <repo>/scripts/benchmarks/longcli/tb-*-setup.sh.j2 ~/longcli-agents/
cp <repo>/scripts/benchmarks/probes/{inject_probes,grade_probes,rollout_lib}.py ~/longcli-agents/
```
