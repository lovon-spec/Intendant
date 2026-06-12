# LongCLI-Bench lanes (density-first managed-context evaluation)

Port of the two benchmark lanes (vanilla Codex / Intendant-managed Codex)
from the harbor harness (`../harbor_*.py`, terminal-bench dataset) to
**LongCLI-Bench** — arXiv 2602.14337, github.com/finyorko/longcli-bench — 20
organic long-horizon CLI tasks (xv6 OS labs, CMU 15-445 BusTub, CS61A
projects, AP1400-2) scored with a dual test protocol (F2P requirement
fulfillment + P2P regression avoidance) and step-level completion scores.
Long-horizon + step-level scoring is exactly the regime the managed-context
overhaul targets, which makes this the headline lane.

## How LongCLI-Bench runs (feasibility findings, 2026-06-12)

- LongCLI **vendors its own `terminal_bench` 0.2.18 fork** — `tb run` comes
  from the LongCLI checkout itself, NOT from upstream terminal-bench and NOT
  from harbor. Its harness drives task containers over docker-compose + tmux.
- The checkout is ~213 MB (180 MB of task payloads), so it is fetched, never
  vendored: `fetch-longcli.sh` pins commit
  `e20364ba3eb4c083f582843cdd4e2d5fe3b5a729`.
- Agents implement `AbstractInstalledAgent` (`_env`,
  `_install_agent_script_path`, `_run_agent_commands()`), are installed
  *inside* the task container, and are selected with
  `tb run --agent-import-path module:Class` + `--agent-kwarg key=value`.
  This file's directory goes on `PYTHONPATH`.
- tb's stock codex agent only supports `OPENAI_API_KEY` auth (writes a
  throwaway auth.json) and pins nothing; it has **no ChatGPT-token refresh
  handling** — hence the vanilla port below.
- Tasks need two locally-built base images (`tb/make-pytest:v0`,
  `tb/c-env:v0`); 12/20 tasks use make-pytest, 8/20 use c-env (gtest +
  preinstalled npm codex 0.98.0 — both lanes override the codex binary).
- All 20 `solution.sh` are **empty**: the oracle agent cannot pass anything
  (anti-contamination choice by the authors), so harness proof requires a
  real agent run.
- `/agent-logs` inside the container is bind-mounted to the trial's host
  `agent-logs/` dir — both lanes anchor `CODEX_HOME` (and the managed lane
  its intendant `--log-file`) inside it, so rollouts, rewind archives, the
  fission ledger and the live auth.json are durable *by construction*, even
  on timeouts.
- `task.yaml` budgets: `max_agent_timeout_sec: 7200` per task,
  `max_test_timeout_sec: 300`. Self-correction (`--give-test-output N`) calls
  the agent again per turn — the density lanes run single-turn.

## Files

| file | role |
|---|---|
| `fetch-longcli.sh` | reproducible pinned checkout (no vendoring) |
| `tb_persistent_codex_agent.py` | **vanilla lane**: stock tb codex flow (nvm + npm `@openai/codex@0.133.0`, stock `codex exec` invocation) + ChatGPT auth.json upload / refresh persist-back + durable `CODEX_HOME` |
| `tb-codex-setup.sh.j2` | vanilla in-container setup (stock minus the auth heredoc) |
| `tb_intendant_codex_agent.py` | **managed lane**: uploads the prebuilt codex fork + intendant binaries, writes `intendant.toml` (`managed_context = "managed"`, `context_archive = "exact"`, full window — no caps), launches `intendant --no-tls --bind ... --web ... --no-tui --no-presence --agent codex --log-file /agent-logs/intendant --task-file ...`, polls for `task_complete` in a **parent** rollout (fission-branch rollouts carry `<fission_charter>` and are excluded — fixes a false-trigger latent in the harbor agent), optional post-completion recall probes |
| `tb-intendant-setup.sh.j2` | managed in-container deps (glibc chain incl. the noble libvpx7 fallback) |
| `RUN-COMMANDS.md` | exact invocations: smoke, 5-task pilot, 20-task full run, aggregation |

## Auth convention (matches the existing host lanes)

Stage a fresh copy per lane immediately before launch, never share a copy
across concurrently-running lanes, and always run `--n-concurrent 1`
(trials in a lane run sequentially against one refresh chain):

```bash
mkdir -p ~/tbench-codex-homes/<lane-home>
install -m 600 ~/.codex/auth.json ~/tbench-codex-homes/<lane-home>/auth.json
CODEX_AUTH_JSON_PATH=~/tbench-codex-homes/<lane-home>/auth.json tb run ...
```

The agent uploads it into the container at `$CODEX_HOME/auth.json` and copies
the refreshed file back to the same host path after every trial (in-container
chown to the harness uid, then an atomic host-side replace).

## Probes integration (managed lane)

`--agent-kwarg probes_dir=<dir>` with `<dir>/<task-id>.json` present keeps
intendant alive after `task_complete`, binds the gateway on `0.0.0.0`
(reachable from the docker host via the container IP), and drives
post-completion follow-up turns via `../probes/inject_probes.py` (gateway
WebSocket `ControlMsg::FollowUp`). Answers land in
`<trial>/agent-logs/probe_answers.json`. Without the kwarg the lane binds
`127.0.0.1` and behaves exactly like the proven harbor lane. Vanilla-lane
probes run post-hoc against the archived `codex-home` (see
`../probes/protocol.md`) — no agent hook needed.

## Verified on the .206 host (2026-06-12)

- `~/longcli-venv` (python 3.13.5) + pinned checkout at `~/longcli-bench`;
  `tb/make-pytest:v0` built (1.9 GB; `tb/c-env:v0` still pending — build it
  before any cmu15_445/ap1400 task).
- **Vanilla smoke** on `cs61_fa24_hog`: PASS — F2P 1.0 + P2P 1.0, agent time
  138 s, npm codex 0.133.0 installed in-container, ChatGPT auth uploaded and
  persisted back, rollout archived on the bind mount
  (`~/longcli-jobs/smoke-vanilla-hog-20260612`).
- **Managed smoke** on `cs61_fa24_hog`: PASS — F2P 1.0 + P2P 1.0, agent time
  240 s; rollout confirms the managed protocol engaged
  (`managed_context=managed` MCP handshake + `<managed_context>` instruction)
  at the **full 258400-token window** (no caps), intendant log dir persisted
  in place, clean shutdown
  (`~/longcli-jobs/smoke-managed-hog-20260612`).

## Known caveats

- **Timeout abandonment:** tb cancels the agent's worker thread on the
  harness-level timeout without running agent cleanup. Both lanes therefore
  bound the in-container command themselves (`command_timeout_sec` kwarg,
  default 7000s managed / unbounded vanilla unless set) and keep all
  artifacts on the bind mount, so nothing is lost either way; set
  `command_timeout_sec` below `--global-agent-timeout-sec` /
  `max_agent_timeout_sec` so the auth persist-back always runs.
- **Multi-turn (`--give-test-output`):** each turn re-invokes the agent; the
  managed lane would launch a fresh intendant session per turn (fresh codex
  thread). Density lanes run single-turn; do not enable multi-turn without
  deciding what session continuity should mean for it.
- **c-env image:** ships npm codex 0.98.0 at `/usr/local/bin/codex` — both
  lanes replace it (vanilla: npm global install wins on PATH precedence via
  nvm; managed: the binary upload `rm -f`s the symlink first).
- run-tests.sh files in some tasks contain stray non-comment prose lines
  (upstream sloppiness); they print `command not found` noise but do not
  abort the test phase. Leave them as-is — patched task definitions would
  fork the benchmark.
