---
name: codex-context-stress
description: >
  E2E test the managed Codex long-context feature against installed vanilla
  Codex. Uses a deterministic large-output task to force vanilla auto-compaction,
  then verifies managed Codex avoids hidden compaction, blocks ordinary tools
  under pressure, rewinds to an exact tool-call anchor, and returns to a safe
  context-pressure state.
compatibility: Requires OpenAI Codex auth in ~/.codex/auth.json, installed vanilla Codex at /opt/homebrew/bin/codex, and the patched Codex fork checkout at /Users/vm/projects/codex-minimal-lineage.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Codex Context Stress E2E

## Purpose

This is the regression test for the Codex-as-external-agent context-management
feature. It exercises the feature at the scale where the fork matters:
a long task that would normally trigger Codex auto-compaction.

The test compares:

- **Vanilla Codex**: `/opt/homebrew/bin/codex exec --json` with a deliberately
  low `model_auto_compact_token_limit`.
- **Managed Codex**: patched Codex launched by Intendant on an isolated web port
  with auto-compaction disabled, Intendant MCP context tools available, and
  context-pressure gating active.

## What It Verifies

- Vanilla Codex auto-compacts under the deterministic long-output workload.
- Managed Codex records the same long output without hidden compaction.
- The exact `exec_command` tool-call id for `python3 emit_context.py 3000` is
  recoverable from the Codex rollout.
- Once pressure crosses the threshold, ordinary tools are blocked and only
  `get_status`, `rewind_context`, and `rewind_backout` remain allowed.
- The model can call `rewind_context` directly through the Intendant MCP tools.
- Rewinding to `position="before"` the bulky tool call prunes the long output
  and returns `get_status.context_pressure.status` to `ok`.

## Run

The feature is on `main` in both repos. Build the patched Codex fork, then
build and run from the Intendant repo root:

```bash
# 1. Patched Codex (minimal-lineage fork)
cd /Users/vm/projects/codex-minimal-lineage/codex-rs
cargo build -p codex-cli --bin codex

# 2. Intendant (debug build matches the harness's --intendant-bin default)
cd /Users/vm/projects/intendant
cargo build --bin intendant

# 3. Harness
scripts/codex_context_stress_e2e.py \
  --managed-codex-bin /Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex
```

Binary selection is flags-only (no env vars): `--vanilla-codex-bin` defaults to
`/opt/homebrew/bin/codex`, `--intendant-bin` defaults to
`target/debug/intendant` relative to the repo containing the script, and
`--managed-codex-bin` has a stale built-in default pointing at a retired
worktree — always pass it explicitly as above.

The script refuses to use port `8765`; it starts Intendant on a random isolated
port and terminates that process at the end of the run.

## Expected Pass Criteria

The final output should include:

```json
{
  "vanilla_compacted_under_pressure": true,
  "managed_avoided_hidden_compaction": true,
  "managed_found_exact_long_output_anchor": true,
  "managed_rewind_only_gate_observed": true,
  "managed_model_rewind_succeeded": true,
  "managed_post_rewind_pressure_ok": true,
  "production_ready_gate": true
}
```

## Artifacts

Each run writes a timestamped directory under:

```text
/tmp/intendant-codex-context-e2e/
```

On macOS this resolves through `/var/folders/.../T/`.

Important files:

- `summary.json`: concise pass/fail, timing, rollout metrics, pressure reports.
- `raw/vanilla/*.stdout`: vanilla Codex JSON event stream.
- `raw/managed/websocket_events.jsonl`: Intendant dashboard/backend events.
- `codex_home_{vanilla,managed}/sessions/.../rollout-*.jsonl`: Codex rollouts.
- `intendant_logs/session.jsonl`: Intendant session log for the managed run.

## Notes

- This test makes real model calls and intentionally consumes enough context to
  trigger compaction behavior. Do not put it in normal CI.
- Use `--skip-vanilla` while iterating on managed behavior, but run the full
  comparison before calling the feature production-ready.
- Use `--line-count`, `--context-window`, and `--vanilla-compact-limit` to tune
  pressure. The default `3000` lines with a `30000` token window has been enough
  to hit the gate without exhausting the model.
