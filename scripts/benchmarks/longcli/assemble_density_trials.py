#!/usr/bin/env python3
"""Assemble LongCLI pilot trials into the layout managed_density_report.py reads.

LongCLI trial layout (host, per single-task run):
    <jobs>/<run-id>/<task>/<task>.1-of-1.<run-id>/
        agent-logs/codex-home/sessions/**/rollout-*.jsonl
        agent-logs/intendant/{context_rewinds/, fission_ledger.json, ...}   (managed lanes)
        results.json sits at <jobs>/<run-id>/results.json (run-level) and the
        trial dir carries test_output/metrics_turn1.json after the test phase.

managed_density_report.resolve_trial wants:
    <trial>/agent/sessions/...      (codex rollouts)
    <trial>/agent/intendant/...     (managed lanes)
    <trial>/result.json             (optional; harbor schema)

This script symlinks the former into the latter and synthesizes result.json
with the LongCLI scores mapped into the harbor reward slot:
    reward          = f2p_is_pass (the headline pass/fail)
    plus a `long_cli` block carrying the full step scores for the report.

Usage:
    assemble_density_trials.py --raw-root /tmp/longcli-pilot3/raw \
        --out-root /tmp/longcli-pilot3/trials

--raw-root holds one directory per run-id (rsync of the host run dirs).
Trial names become <lane>-<n>-<task> parsed from run-ids of the form
pilot3-<lane>-<date>-<n>-<task>.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

RUN_ID_RE = re.compile(
    r"^pilot3-(?P<lane>vanilla|managed-current|managed-density)-"
    r"(?P<date>\d{8})-(?P<idx>\d+)-(?P<task>.+)$"
)


def find_trial_dir(run_dir: Path, task: str) -> Path | None:
    task_dir = run_dir / task
    if not task_dir.is_dir():
        return None
    for trial in sorted(task_dir.iterdir()):
        if trial.is_dir() and (trial / "agent-logs").is_dir():
            return trial
    return None


def load_long_cli_metrics(run_dir: Path, trial_dir: Path, task: str) -> dict | None:
    metrics = None
    for candidate in (
        trial_dir / "test_output" / "metrics_turn1.json",
        trial_dir / "metrics_turn1.json",
    ):
        if candidate.is_file():
            try:
                metrics = json.loads(candidate.read_text())
                break
            except (OSError, json.JSONDecodeError):
                pass
    results_json = run_dir / "results.json"
    trial_entry = None
    if results_json.is_file():
        try:
            doc = json.loads(results_json.read_text())
            for entry in doc.get("results", []):
                if entry.get("task_id") == task:
                    trial_entry = entry
                    break
        except (OSError, json.JSONDecodeError):
            pass
    if metrics is None and trial_entry is not None:
        metrics = trial_entry.get("long_cli")
    if metrics is None:
        return None
    out = {
        "f2p_is_pass": metrics.get("f2p_is_pass"),
        "f2p_step_score": metrics.get("f2p_step_score"),
        "p2p_is_pass": metrics.get("p2p_is_pass"),
        "p2p_step_score": metrics.get("p2p_step_score"),
        "agent_duration_time": metrics.get("agent_duration_time"),
    }
    if trial_entry is not None:
        out["is_resolved"] = trial_entry.get("is_resolved")
        out["failure_mode"] = trial_entry.get("failure_mode")
    return out


def assemble(raw_root: Path, out_root: Path) -> int:
    out_root.mkdir(parents=True, exist_ok=True)
    n = 0
    for run_dir in sorted(raw_root.iterdir()):
        match = RUN_ID_RE.match(run_dir.name)
        if not match or not run_dir.is_dir():
            continue
        lane, idx, task = match["lane"], match["idx"], match["task"]
        trial_dir = find_trial_dir(run_dir, task)
        if trial_dir is None:
            print(f"WARN: no trial dir under {run_dir}", file=sys.stderr)
            continue
        agent_logs = trial_dir / "agent-logs"
        sessions = agent_logs / "codex-home" / "sessions"
        if not sessions.is_dir():
            print(f"WARN: no sessions dir in {agent_logs}", file=sys.stderr)
            continue

        # Layout: <out-root>/<lane>/<lane>__<idx>__<task>/ — the per-lane
        # subdirectory is a "run dir" for summarize_harbor_results.py (which
        # discovers trials by glob("*__*") and treats each run dir as a
        # lane), while each trial dir individually feeds
        # managed_density_report.py.
        name = f"{lane}__{idx}__{task}"
        dest = out_root / lane / name
        agent = dest / "agent"
        agent.mkdir(parents=True, exist_ok=True)
        for link_name, target in (
            ("sessions", sessions),
            ("intendant", agent_logs / "intendant"),
        ):
            link = agent / link_name
            if link.is_symlink() or link.exists():
                link.unlink()
            if target.is_dir():
                link.symlink_to(target.resolve())

        metrics = load_long_cli_metrics(run_dir, trial_dir, task)
        result = {
            "task_name": task,
            "trial_name": name,
            "verifier_result": {
                "rewards": {
                    "reward": (metrics or {}).get("f2p_is_pass"),
                }
            },
            "agent_result": {},
            "long_cli": metrics,
            "source_run_dir": str(run_dir),
            "source_trial_dir": str(trial_dir),
        }
        (dest / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"assembled {dest}")
        n += 1
    return n


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--raw-root", type=Path, required=True)
    parser.add_argument("--out-root", type=Path, required=True)
    args = parser.parse_args()
    n = assemble(args.raw_root, args.out_root)
    print(f"{n} trials assembled under {args.out_root}")
    if n == 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
