#!/usr/bin/env python3
"""Summarize Harbor / Terminal-Bench runs with Intendant context signals.

The top-level Harbor result gives aggregate pass/cost/token counts, but the
managed-context (density-first) work needs per-trial context-management
diagnostics too. This script reads one or more Harbor job/run directories
("lanes") and emits one row per trial plus a lane summary block, with:

- Terminal-Bench reward and exception status
- per-trial token/cost counters and wall-clock durations
- context-rewind record counts and their pressure-band distribution
  (`ok|watch|high|critical`), using the record's own
  `used_tokens_at_rewind`/`context_window_at_rewind`/`pressure_band_at_rewind`
  fields when present; for records written before those fields existed it
  falls back to the record's archived pre-rewind rollout
  (`<record_id>-source-rollout.jsonl`, last `token_count` report — the exact
  value the record writer would have captured), then to a created_at join
  against the session-log context-snapshot timeline
- `"type":"compacted"` event counts from the exported Codex rollouts
  (agent/sessions)
- fission ledger group/branch counts
- legacy compaction/rewind/auth/backend term signals from task logs

Output: a markdown report (stdout or --markdown), optional per-trial CSV
(--csv), optional lane-summary CSV (--lanes-csv), or full JSON (--json).

Pressure-band thresholds mirror the live managed-context gates in
src/bin/caller/main.rs: `watch` starts at MANAGED_CONTEXT_DENSITY_THRESHOLD_PCT
(85%) of the effective window, `high` at the window, `critical` at the hard
window when known.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import re
from pathlib import Path
from typing import Any


# Mirrors MANAGED_CONTEXT_DENSITY_THRESHOLD_PCT in src/bin/caller/main.rs.
DENSITY_THRESHOLD_PCT = 85.0

BAND_ORDER = ("ok", "watch", "high", "critical", "unknown")
BAND_SOURCE_ORDER = ("fields", "rollout", "join", "none")

AUTH_ERROR_TERMS = (
    "refresh_token_reused",
    "token_expired",
    "401 Unauthorized",
    "codex auth",
)
COMPACTION_TERMS = (
    "context_compacted",
    "contextCompaction",
    "context compaction",
    "auto-compaction",
    "auto_compaction",
)
REWIND_TERMS = (
    "rewind_context",
    "context_rewind",
    "conversation_rolled_back",
    "rolled_back",
)
BACKEND_ERROR_TERMS = (
    "responseStreamDisconnected",
    "stream disconnected",
    "codex backend error",
)
COMPACTED_EVENT_TERM = '"type":"compacted"'


def load_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8", errors="replace"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def parse_time(value: str | None) -> dt.datetime | None:
    if not value:
        return None
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    # fromisoformat rejects >6 fractional digits (chrono RFC3339 emits 9).
    value = re.sub(r"\.(\d{6})\d+", r".\1", value)
    try:
        return dt.datetime.fromisoformat(value)
    except ValueError:
        return None


def seconds_between(start: str | None, finish: str | None) -> float | None:
    started = parse_time(start)
    finished = parse_time(finish)
    if started is None or finished is None:
        return None
    return max(0.0, (finished - started).total_seconds())


def iter_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(value, dict):
                    rows.append(value)
    except OSError:
        pass
    return rows


def count_terms(path: Path, terms: tuple[str, ...]) -> int:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return 0
    return sum(text.count(term) for term in terms)


# ---------------------------------------------------------------------------
# Pressure bands
# ---------------------------------------------------------------------------


def pressure_band(
    used_tokens: int, context_window: int, hard_context_window: int | None = None
) -> str:
    """Band for a usage measurement; mirrors context_rewind_pressure_band."""
    if hard_context_window and used_tokens >= hard_context_window:
        return "critical"
    if used_tokens >= context_window:
        return "high"
    if used_tokens >= int(context_window * DENSITY_THRESHOLD_PCT / 100.0):
        return "watch"
    return "ok"


def last_rollout_token_count(path: Path) -> tuple[int, int] | None:
    """Last backend `token_count` report in a Codex rollout: (used, window)."""
    latest: tuple[int, int] | None = None
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if '"token_count"' not in line:
                    continue
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if entry.get("type") != "event_msg":
                    continue
                payload = entry.get("payload")
                if not isinstance(payload, dict) or payload.get("type") != "token_count":
                    continue
                info = payload.get("info")
                if not isinstance(info, dict):
                    continue
                window = info.get("model_context_window") or info.get("modelContextWindow")
                last = info.get("last_token_usage") or info.get("lastTokenUsage")
                if not isinstance(window, int) or window <= 0 or not isinstance(last, dict):
                    continue
                used = last.get("total_tokens") or last.get("totalTokens")
                if isinstance(used, int):
                    latest = (used, window)
    except OSError:
        return None
    return latest


def seconds_of_day(value: dt.datetime | None) -> float | None:
    if value is None:
        return None
    return value.hour * 3600 + value.minute * 60 + value.second + value.microsecond / 1e6


def parse_log_ts_seconds(value: str | None) -> float | None:
    """session.jsonl `ts` values are time-of-day strings like 06:11:08.450."""
    if not isinstance(value, str):
        return None
    match = re.match(r"^(\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?$", value.strip())
    if not match:
        return None
    hours, minutes, seconds = (int(match.group(1)), int(match.group(2)), int(match.group(3)))
    frac = float(f"0.{match.group(4)}") if match.group(4) else 0.0
    return hours * 3600 + minutes * 60 + seconds + frac


def context_snapshot_timeline(trial_dir: Path) -> list[tuple[float, int, int, int | None]]:
    """Backend-reported context snapshots from the Intendant session log:
    (seconds_of_day, token_count, context_window, hard_context_window)."""
    timeline: list[tuple[float, int, int, int | None]] = []
    for path in sorted(trial_dir.glob("agent/intendant/session.jsonl")):
        for row in iter_jsonl(path):
            if row.get("event") != "context_snapshot":
                continue
            ts = parse_log_ts_seconds(row.get("ts"))
            data = row.get("data")
            if ts is None or not isinstance(data, dict):
                continue
            if data.get("token_count_kind") != "backend_reported":
                continue
            tokens = data.get("token_count")
            window = data.get("context_window")
            hard = data.get("hard_context_window")
            if isinstance(tokens, int) and isinstance(window, int) and window > 0:
                timeline.append((ts, tokens, window, hard if isinstance(hard, int) else None))
    timeline.sort(key=lambda item: item[0])
    return timeline


def rewind_record_band(
    record: dict[str, Any],
    record_path: Path,
    timeline: list[tuple[float, int, int, int | None]],
) -> tuple[str, str]:
    """(band, source) for one context-rewind record.

    Preference order:
    1. the record's own pressure fields (records written after the
       pressure_at_rewind instrumentation landed),
    2. the archived pre-rewind rollout next to the record — its last
       token_count report is exactly what the record writer captures,
    3. created_at join against the session-log context-snapshot timeline.
    """
    band = record.get("pressure_band_at_rewind")
    if isinstance(band, str) and band in BAND_ORDER:
        return band, "fields"
    used = record.get("used_tokens_at_rewind")
    window = record.get("context_window_at_rewind")
    if isinstance(used, int) and isinstance(window, int) and window > 0:
        return pressure_band(used, window), "fields"

    record_id = record.get("record_id") or record_path.stem
    rollout_copy = record_path.with_name(f"{record_id}-source-rollout.jsonl")
    usage = last_rollout_token_count(rollout_copy) if rollout_copy.is_file() else None
    if usage is not None:
        return pressure_band(usage[0], usage[1]), "rollout"

    created_sec = seconds_of_day(parse_time(record.get("created_at")))
    if created_sec is not None:
        candidates = [item for item in timeline if item[0] <= created_sec]
        if candidates:
            _, tokens, window, hard = candidates[-1]
            return pressure_band(tokens, window, hard), "join"
    return "unknown", "none"


def rewind_metrics(trial_dir: Path) -> dict[str, Any]:
    records_dir = trial_dir / "agent" / "intendant" / "context_rewinds"
    band_counts = {band: 0 for band in BAND_ORDER}
    source_counts = {source: 0 for source in BAND_SOURCE_ORDER}
    count = 0
    if records_dir.is_dir():
        timeline: list[tuple[float, int, int, int | None]] | None = None
        for path in sorted(records_dir.glob("*.json")):
            record = load_json(path)
            if record is None or "record_id" not in record:
                continue
            count += 1
            if timeline is None:
                timeline = context_snapshot_timeline(trial_dir)
            band, source = rewind_record_band(record, path, timeline)
            band_counts[band] += 1
            source_counts[source] += 1
    return {
        "rewind_records": count,
        "rewind_bands": band_counts,
        "rewind_band_sources": source_counts,
    }


# ---------------------------------------------------------------------------
# Other per-trial event signals
# ---------------------------------------------------------------------------


def compacted_event_count(trial_dir: Path) -> int:
    """Occurrences of '"type":"compacted"' in the exported Codex rollouts."""
    sessions_dir = trial_dir / "agent" / "sessions"
    if not sessions_dir.is_dir():
        return 0
    return sum(
        count_terms(path, (COMPACTED_EVENT_TERM,))
        for path in sorted(sessions_dir.rglob("*.jsonl"))
    )


def fission_metrics(trial_dir: Path) -> dict[str, int]:
    ledger = load_json(trial_dir / "agent" / "intendant" / "fission_ledger.json") or {}
    groups = ledger.get("groups")
    if not isinstance(groups, list):
        groups = []
    branches = sum(
        len(group.get("branches", []))
        for group in groups
        if isinstance(group, dict) and isinstance(group.get("branches"), list)
    )
    return {"fission_groups": len(groups), "fission_branches": branches}


def context_metrics(trial_dir: Path) -> dict[str, Any]:
    session_logs = sorted(trial_dir.glob("agent/intendant/session.jsonl"))
    max_tokens = None
    max_items = None
    snapshot_count = 0
    compactions = 0
    rewinds = 0
    auth_errors = 0
    backend_errors = 0

    for path in session_logs:
        for row in iter_jsonl(path):
            row_text = json.dumps(row, separators=(",", ":"))
            if any(term in row_text for term in COMPACTION_TERMS):
                compactions += 1
            if any(term in row_text for term in REWIND_TERMS):
                rewinds += 1
            if any(term in row_text for term in AUTH_ERROR_TERMS):
                auth_errors += 1
            if any(term in row_text for term in BACKEND_ERROR_TERMS):
                backend_errors += 1
            if row.get("event") != "context_snapshot":
                continue
            data = row.get("data")
            if not isinstance(data, dict):
                data = row
            token_count = data.get("token_count")
            item_count = data.get("item_count")
            if isinstance(token_count, int):
                snapshot_count += 1
                max_tokens = token_count if max_tokens is None else max(max_tokens, token_count)
            if isinstance(item_count, int):
                max_items = item_count if max_items is None else max(max_items, item_count)

    for path in [trial_dir / "trial.log", trial_dir / "agent" / "codex.txt"]:
        auth_errors += count_terms(path, AUTH_ERROR_TERMS)
        compactions += count_terms(path, COMPACTION_TERMS)
        rewinds += count_terms(path, REWIND_TERMS)
        backend_errors += count_terms(path, BACKEND_ERROR_TERMS)

    return {
        "context_snapshot_count": snapshot_count,
        "max_context_tokens": max_tokens,
        "max_context_items": max_items,
        "compaction_signals": compactions,
        "rewind_signals": rewinds,
        "auth_error_signals": auth_errors,
        "backend_error_signals": backend_errors,
    }


def exception_label(exception_info: Any) -> str | None:
    if not exception_info:
        return None
    if isinstance(exception_info, dict):
        label = exception_info.get("exception_type") or exception_info.get("type")
        if isinstance(label, str) and label.strip():
            return label.strip()
        message = exception_info.get("exception_message") or exception_info.get("message")
        if isinstance(message, str) and message.strip():
            return message.strip().splitlines()[0][:80]
        return "exception"
    if isinstance(exception_info, str) and exception_info.strip():
        return exception_info.strip().splitlines()[0][:80]
    return "exception"


def summarize_trial(trial_dir: Path) -> dict[str, Any] | None:
    result = load_json(trial_dir / "result.json")
    if result is None:
        return None
    agent_result = result.get("agent_result") or {}
    verifier_result = result.get("verifier_result") or {}
    rewards = verifier_result.get("rewards") if isinstance(verifier_result, dict) else None
    reward = rewards.get("reward") if isinstance(rewards, dict) else None
    agent_execution = result.get("agent_execution") or {}
    verifier = result.get("verifier") or {}
    exception_info = result.get("exception_info")

    summary = {
        "task_name": result.get("task_name"),
        "trial_name": result.get("trial_name", trial_dir.name),
        "reward": reward,
        "exception": exception_label(exception_info),
        "n_input_tokens": agent_result.get("n_input_tokens"),
        "n_cache_tokens": agent_result.get("n_cache_tokens"),
        "n_output_tokens": agent_result.get("n_output_tokens"),
        "cost_usd": agent_result.get("cost_usd"),
        "agent_seconds": seconds_between(
            agent_execution.get("started_at"),
            agent_execution.get("finished_at"),
        ),
        "verifier_seconds": seconds_between(
            verifier.get("started_at"),
            verifier.get("finished_at"),
        ),
    }
    summary.update(context_metrics(trial_dir))
    summary.update(rewind_metrics(trial_dir))
    summary["compacted_events"] = compacted_event_count(trial_dir)
    summary.update(fission_metrics(trial_dir))
    return summary


def lane_name(run_dir: Path) -> str:
    """Job/lane label: the parent dir for Harbor timestamp run dirs."""
    if re.match(r"^\d{4}-\d{2}-\d{2}__", run_dir.name) and run_dir.parent.name:
        return run_dir.parent.name
    return run_dir.name


def numeric_sum(trials: list[dict[str, Any]], key: str) -> float | int | None:
    values = [trial.get(key) for trial in trials if isinstance(trial.get(key), (int, float))]
    if not values:
        return None
    total = sum(values)
    return int(total) if all(isinstance(value, int) for value in values) else total


def summarize_run(run_dir: Path) -> dict[str, Any]:
    top = load_json(run_dir / "result.json") or {}
    trials = [
        item
        for item in (
            summarize_trial(path) for path in sorted(run_dir.glob("*__*")) if path.is_dir()
        )
        if item is not None
    ]
    rewards = [trial["reward"] for trial in trials if isinstance(trial.get("reward"), (int, float))]
    band_totals = {band: sum(trial["rewind_bands"][band] for trial in trials) for band in BAND_ORDER}
    source_totals = {
        source: sum(trial["rewind_band_sources"][source] for trial in trials)
        for source in BAND_SOURCE_ORDER
    }
    total_records = sum(trial["rewind_records"] for trial in trials)
    band_distribution = {
        band: (band_totals[band] / total_records) if total_records else None
        for band in BAND_ORDER
    }
    return {
        "run_dir": str(run_dir),
        "lane": lane_name(run_dir),
        "started_at": top.get("started_at"),
        "finished_at": top.get("finished_at"),
        "n_total_trials": top.get("n_total_trials"),
        "n_trials_with_result": len(trials),
        "n_exceptions": sum(1 for trial in trials if trial.get("exception")),
        "mean_reward": (sum(rewards) / len(rewards)) if rewards else None,
        "sum_reward": sum(rewards) if rewards else None,
        "sum_cost_usd": numeric_sum(trials, "cost_usd"),
        "sum_input_tokens": numeric_sum(trials, "n_input_tokens"),
        "sum_cache_tokens": numeric_sum(trials, "n_cache_tokens"),
        "sum_output_tokens": numeric_sum(trials, "n_output_tokens"),
        "sum_agent_seconds": numeric_sum(trials, "agent_seconds"),
        "sum_verifier_seconds": numeric_sum(trials, "verifier_seconds"),
        "rewind_records": total_records,
        "rewind_records_per_trial": (total_records / len(trials)) if trials else None,
        "rewind_bands": band_totals,
        "rewind_band_distribution": band_distribution,
        "rewind_band_sources": source_totals,
        "compacted_events": numeric_sum(trials, "compacted_events") or 0,
        "fission_groups": numeric_sum(trials, "fission_groups") or 0,
        "fission_branches": numeric_sum(trials, "fission_branches") or 0,
        "aggregate": top.get("stats", {}),
        "trials": trials,
    }


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------


def fmt_float(value: Any, digits: int = 2) -> str:
    if isinstance(value, (int, float)):
        return f"{value:.{digits}f}"
    return "-"


def fmt_int(value: Any) -> str:
    if isinstance(value, (int, float)):
        return str(int(value))
    return "-"


def fmt_bands(bands: dict[str, int]) -> str:
    parts = [f"{band}:{bands[band]}" for band in BAND_ORDER if bands.get(band)]
    return " ".join(parts) if parts else "-"


def fmt_band_distribution(run: dict[str, Any]) -> str:
    if not run["rewind_records"]:
        return "-"
    parts = []
    for band in BAND_ORDER:
        share = run["rewind_band_distribution"][band]
        if share:
            parts.append(f"{band} {share * 100:.0f}%")
    return ", ".join(parts) if parts else "-"


def markdown_report(runs: list[dict[str, Any]]) -> str:
    lines: list[str] = []
    for run in runs:
        lines.append(f"## Lane: {run['lane']} ({run['run_dir']})")
        lines.append("")
        lines.append(
            "| task | trial | reward | exception | cost_usd | input | cached | output "
            "| agent_s | rewinds | bands | compacted | fission g/b |"
        )
        lines.append("| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | --- |")
        for trial in run["trials"]:
            lines.append(
                "| "
                + " | ".join(
                    [
                        str(trial.get("task_name")),
                        str(trial.get("trial_name")),
                        fmt_float(trial.get("reward"), 1),
                        trial.get("exception") or "-",
                        fmt_float(trial.get("cost_usd"), 3),
                        fmt_int(trial.get("n_input_tokens")),
                        fmt_int(trial.get("n_cache_tokens")),
                        fmt_int(trial.get("n_output_tokens")),
                        fmt_float(trial.get("agent_seconds"), 0),
                        str(trial.get("rewind_records")),
                        fmt_bands(trial.get("rewind_bands", {})),
                        str(trial.get("compacted_events")),
                        f"{trial.get('fission_groups')}/{trial.get('fission_branches')}",
                    ]
                )
                + " |"
            )
        lines.append("")
        sources = run["rewind_band_sources"]
        lines.append("**Lane summary**")
        lines.append("")
        lines.append(f"- trials with result: {run['n_trials_with_result']}"
                     + (f" (of {run['n_total_trials']})" if run.get("n_total_trials") else ""))
        lines.append(
            f"- reward: mean {fmt_float(run['mean_reward'], 3)}, "
            f"sum {fmt_float(run['sum_reward'], 1)}"
        )
        lines.append(f"- exceptions: {run['n_exceptions']}")
        lines.append(f"- cost: ${fmt_float(run['sum_cost_usd'], 3)}")
        lines.append(
            f"- tokens: input {fmt_int(run['sum_input_tokens'])}, "
            f"cached {fmt_int(run['sum_cache_tokens'])}, "
            f"output {fmt_int(run['sum_output_tokens'])}"
        )
        lines.append(
            f"- wall-clock: agent {fmt_float(run['sum_agent_seconds'], 0)}s, "
            f"verifier {fmt_float(run['sum_verifier_seconds'], 0)}s"
        )
        lines.append(
            f"- rewind records: {run['rewind_records']} "
            f"({fmt_float(run['rewind_records_per_trial'], 2)}/trial); "
            f"band distribution: {fmt_band_distribution(run)} "
            f"[{fmt_bands(run['rewind_bands'])}; sources "
            + " ".join(f"{source}:{sources[source]}" for source in BAND_SOURCE_ORDER if sources[source])
            + "]"
            if run["rewind_records"]
            else "- rewind records: 0"
        )
        lines.append(f"- compacted events: {run['compacted_events']}")
        lines.append(
            f"- fission: {run['fission_groups']} groups / {run['fission_branches']} branches"
        )
        lines.append("")
    return "\n".join(lines)


TRIAL_CSV_COLUMNS = [
    "lane",
    "task_name",
    "trial_name",
    "reward",
    "exception",
    "cost_usd",
    "n_input_tokens",
    "n_cache_tokens",
    "n_output_tokens",
    "agent_seconds",
    "verifier_seconds",
    "rewind_records",
    "band_ok",
    "band_watch",
    "band_high",
    "band_critical",
    "band_unknown",
    "band_source_fields",
    "band_source_rollout",
    "band_source_join",
    "band_source_none",
    "compacted_events",
    "fission_groups",
    "fission_branches",
    "max_context_tokens",
]


def write_trials_csv(path: Path, runs: list[dict[str, Any]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(TRIAL_CSV_COLUMNS)
        for run in runs:
            for trial in run["trials"]:
                bands = trial.get("rewind_bands", {})
                sources = trial.get("rewind_band_sources", {})
                writer.writerow(
                    [
                        run["lane"],
                        trial.get("task_name"),
                        trial.get("trial_name"),
                        trial.get("reward"),
                        trial.get("exception"),
                        trial.get("cost_usd"),
                        trial.get("n_input_tokens"),
                        trial.get("n_cache_tokens"),
                        trial.get("n_output_tokens"),
                        trial.get("agent_seconds"),
                        trial.get("verifier_seconds"),
                        trial.get("rewind_records"),
                        *(bands.get(band, 0) for band in BAND_ORDER),
                        *(sources.get(source, 0) for source in BAND_SOURCE_ORDER),
                        trial.get("compacted_events"),
                        trial.get("fission_groups"),
                        trial.get("fission_branches"),
                        trial.get("max_context_tokens"),
                    ]
                )


LANE_CSV_COLUMNS = [
    "lane",
    "run_dir",
    "n_trials_with_result",
    "n_exceptions",
    "mean_reward",
    "sum_reward",
    "sum_cost_usd",
    "sum_input_tokens",
    "sum_cache_tokens",
    "sum_output_tokens",
    "sum_agent_seconds",
    "sum_verifier_seconds",
    "rewind_records",
    "rewind_records_per_trial",
    "band_ok",
    "band_watch",
    "band_high",
    "band_critical",
    "band_unknown",
    "band_source_fields",
    "band_source_rollout",
    "band_source_join",
    "band_source_none",
    "compacted_events",
    "fission_groups",
    "fission_branches",
]


def write_lanes_csv(path: Path, runs: list[dict[str, Any]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(LANE_CSV_COLUMNS)
        for run in runs:
            writer.writerow(
                [
                    run["lane"],
                    run["run_dir"],
                    run["n_trials_with_result"],
                    run["n_exceptions"],
                    run["mean_reward"],
                    run["sum_reward"],
                    run["sum_cost_usd"],
                    run["sum_input_tokens"],
                    run["sum_cache_tokens"],
                    run["sum_output_tokens"],
                    run["sum_agent_seconds"],
                    run["sum_verifier_seconds"],
                    run["rewind_records"],
                    run["rewind_records_per_trial"],
                    *(run["rewind_bands"][band] for band in BAND_ORDER),
                    *(run["rewind_band_sources"][source] for source in BAND_SOURCE_ORDER),
                    run["compacted_events"],
                    run["fission_groups"],
                    run["fission_branches"],
                ]
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("run_dir", nargs="+", type=Path, help="Harbor job/run directories (lanes)")
    parser.add_argument("--json", action="store_true", help="Emit JSON instead of markdown")
    parser.add_argument("--markdown", type=Path, help="Also write the markdown report to a file")
    parser.add_argument("--csv", type=Path, help="Write per-trial rows as CSV")
    parser.add_argument("--lanes-csv", type=Path, help="Write lane summary rows as CSV")
    args = parser.parse_args()

    runs = [summarize_run(path) for path in args.run_dir]
    if args.csv:
        write_trials_csv(args.csv, runs)
    if args.lanes_csv:
        write_lanes_csv(args.lanes_csv, runs)
    report = markdown_report(runs)
    if args.markdown:
        args.markdown.write_text(report, encoding="utf-8")
    if args.json:
        print(json.dumps(runs, indent=2, sort_keys=True))
    else:
        print(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
