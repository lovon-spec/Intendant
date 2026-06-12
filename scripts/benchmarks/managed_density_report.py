#!/usr/bin/env python3
"""Density-first managed-context evaluation report.

Computes context-density and hygiene metrics from harbor trial dirs (or a
bare codex-sessions dir + intendant session-log dir) produced by managed
(Intendant + lineage-fork Codex) and vanilla Codex benchmark runs.

Inputs per trial:
  - `agent/sessions/**/rollout-*.jsonl`   (codex rollouts)
  - `agent/intendant/context_rewinds/*.json` (managed lane only)
  - `agent/intendant/fission_ledger.json`    (optional)
  - `result.json`                            (optional; reward/cost passthrough)

Metrics:
  M1  Density-throughout: replay the rollout into effective history H_i at
      every model request (token_count event), estimate per-item tokens
      (tiktoken o200k_base; chars/4 fallback), classify stale noise, and
      emit a density series + mean/min/tail + a tier-1 proxy (share of live
      context that is raw tool outputs older than 5 requests).
  M2  Hygiene cadence: pressure band at each rewind record (thresholds
      mirror src/bin/caller/mcp.rs: watch >= 85% of model_context_window,
      high >= window, critical >= model_hard_context_window), rewinds per
      100 requests / per 1M processed tokens, turns-to-prune for big tool
      outputs, hygiene-tool token overhead.
  M3a Re-derivation: duplicate tool calls (canonicalized name+args),
      classified in-context vs post-prune (first occurrence no longer in
      H_i at re-call time); post-prune cases joined against intervening
      rewind-record primers (primer-ignored vs primer-missing). Works on
      vanilla rollouts too, with `compacted` events as the prune boundary.
  M4  Primer chain: fact units from primer sentences + preserve[] entries,
      typed by regex; carry-rate into the next primer; post-rewind
      reference rate; dropped-then-rederived join with M3a; primer token
      growth across the chain.
  Plus: fission summary, compaction/rollback counts, reward/cost (M5)
      passthrough from result.json.

Calibration: per-request plain ratio = est(H_i input side)/reported
last_token_usage.input_tokens, and an offset-corrected ratio that fits one
constant per trial (median residual) for the unobservable system prompt +
tool definitions that rollouts do not persist. Composition analyses are
gated on the offset-corrected median ratio being within +/-15% of 1.0; the
tier-1 proxy (a ratio of estimates) is reported regardless.

CLI:
  managed_density_report.py TRIAL_DIR [TRIAL_DIR...] --out report-dir [--json-only]
  managed_density_report.py --sessions-dir DIR [--intendant-dir DIR] --out report-dir
  managed_density_report.py --self-test

Multi-trial mode emits a cross-trial aggregate table; trial dirs are tagged
managed/vanilla by the presence of an intendant log dir.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import statistics
import sys
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from rollout_replay import (  # noqa: E402
    HYGIENE_TOOL_NAMES,
    OUTPUT_ITEM_TYPES,
    HistoryItem,
    RequestSnapshot,
    RolloutReplay,
    TokenEstimator,
    extract_identifiers,
    is_contextual_user_message,
    is_primer_message,
    normalize_call,
    parse_timestamp,
    replay_rollout,
)

# Thresholds (sources noted; keep in sync with the product).
WATCH_THRESHOLD_PCT = 85.0  # mcp.rs CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT
NOISE_MIN_EST_TOKENS = 500  # stale-noise candidate floor
FRESH_REQUESTS = 3  # outputs newer than this many requests are signal
TIER1_AGE_REQUESTS = 5  # tier-1 proxy: outputs older than this are "old"
PRUNE_TRACK_MIN_TOKENS = 2000  # turns-to-prune tracks outputs above this
CALIBRATION_TOLERANCE = 0.15
TAIL_FRACTION = 0.25  # tail density = mean over the last quarter of requests

CONSUMPTION_KINDS = {"reasoning", "function_call", "custom_tool_call", "local_shell_call"}


# ---------------------------------------------------------------------------
# Trial discovery
# ---------------------------------------------------------------------------


@dataclass
class TrialPaths:
    name: str
    root: Path | None
    sessions_dir: Path
    intendant_dir: Path | None
    result_json: Path | None

    @property
    def lane(self) -> str:
        return "managed" if self.intendant_dir is not None else "vanilla"


def resolve_trial(trial_dir: Path) -> TrialPaths:
    trial_dir = trial_dir.resolve()
    if (trial_dir / "agent" / "sessions").is_dir():
        sessions = trial_dir / "agent" / "sessions"
        intendant = trial_dir / "agent" / "intendant"
    elif (trial_dir / "sessions").is_dir():
        sessions = trial_dir / "sessions"
        intendant = trial_dir / "intendant"
    else:
        raise SystemExit(
            f"{trial_dir}: no agent/sessions or sessions directory (not a trial dir)"
        )
    result = trial_dir / "result.json"
    return TrialPaths(
        name=trial_dir.name,
        root=trial_dir,
        sessions_dir=sessions,
        intendant_dir=intendant if intendant.is_dir() else None,
        result_json=result if result.is_file() else None,
    )


def load_rewind_records(intendant_dir: Path | None) -> list[dict[str, Any]]:
    if intendant_dir is None:
        return []
    records_dir = intendant_dir / "context_rewinds"
    if not records_dir.is_dir():
        return []
    records = []
    for path in sorted(records_dir.glob("*.json")):
        try:
            record = json.loads(path.read_text(encoding="utf-8", errors="replace"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(record, dict) and record.get("record_id"):
            records.append(record)
    records.sort(key=lambda record: str(record.get("created_at") or ""))
    return records


def select_main_rollout(
    sessions_dir: Path, records: list[dict[str, Any]]
) -> tuple[Path, list[Path]]:
    rollouts = sorted(sessions_dir.glob("**/rollout-*.jsonl"))
    if not rollouts:
        raise SystemExit(f"{sessions_dir}: no rollout-*.jsonl files")
    thread_ids = {
        str(record.get("thread_id")) for record in records if record.get("thread_id")
    }
    main = None
    for thread_id in thread_ids:
        for path in rollouts:
            if thread_id and thread_id in path.name:
                main = path
                break
        if main:
            break
    if main is None:
        main = max(rollouts, key=lambda path: path.stat().st_size)
    return main, [path for path in rollouts if path != main]


# ---------------------------------------------------------------------------
# M1: density-throughout
# ---------------------------------------------------------------------------


def _first_reference_request(
    replay: RolloutReplay, candidate: HistoryItem
) -> int | None:
    """Earliest request index at which a later model-consumption item
    (assistant message / reasoning / tool-call arguments) shares an
    identifier with `candidate`. Raw timeline: later items count even if
    they were themselves pruned afterwards (the model demonstrably
    consumed the content)."""
    ids = candidate.identifiers()
    if not ids:
        return None
    for uid in range(candidate.uid + 1, max(replay.items) + 1):
        item = replay.items.get(uid)
        if item is None:
            continue
        kind = item.kind
        is_consumption = kind in CONSUMPTION_KINDS or (
            kind == "message" and (item.role or "").lower() == "assistant"
        )
        if not is_consumption:
            continue
        if ids & item.identifiers():
            return item.birth_request
    return None


def _is_noise_candidate(item: HistoryItem) -> bool:
    if item.est_tokens <= NOISE_MIN_EST_TOKENS:
        return False
    if item.kind in OUTPUT_ITEM_TYPES:
        return True
    if (
        item.kind == "message"
        and (item.role or "").lower() == "user"
        and not is_primer_message(item.payload)
        and not is_contextual_user_message(item.payload)
    ):
        # Pasted command output / large injected blobs.
        return True
    return False


def compute_density(replay: RolloutReplay) -> dict[str, Any]:
    candidates: dict[int, int | None] = {}  # uid -> first reference request
    for item in replay.items.values():
        if _is_noise_candidate(item):
            candidates[item.uid] = _first_reference_request(replay, item)

    rollback_requests = sorted(event.request_index for event in replay.rollbacks)
    series: list[dict[str, Any]] = []
    plain_ratios: list[float] = []
    residuals: list[float] = []
    for snapshot in replay.requests:
        since_rollback = None
        for request_index in rollback_requests:
            if request_index <= snapshot.index:
                since_rollback = snapshot.index - request_index
            else:
                break
        # Server-billed prompt view: input-side effective history minus
        # completed turns' reasoning items (see RolloutReplay.billed_input_uids).
        uids = replay.billed_input_uids(snapshot)
        total_est = sum(replay.items[uid].est_tokens for uid in uids)
        stale_est = 0
        stale_uids: list[int] = []
        tier1_est = 0
        raw_output_est = 0
        for uid in uids:
            item = replay.items[uid]
            if item.kind in OUTPUT_ITEM_TYPES:
                raw_output_est += item.est_tokens
                if snapshot.index - item.birth_request >= TIER1_AGE_REQUESTS:
                    tier1_est += item.est_tokens
            if uid in candidates:
                age = snapshot.index - item.birth_request
                if age < FRESH_REQUESTS:
                    continue  # recent outputs are signal
                first_ref = candidates[uid]
                if first_ref is None or first_ref > snapshot.index:
                    stale_est += item.est_tokens
                    stale_uids.append(uid)
        density = 1.0 - (stale_est / total_est) if total_est > 0 else None
        tier1 = (tier1_est / total_est) if total_est > 0 else None
        entry: dict[str, Any] = {
            "request": snapshot.index,
            "input_tokens": snapshot.input_tokens,
            "total_tokens": snapshot.total_tokens,
            "context_window": snapshot.context_window,
            "est_tokens": total_est,
            "stale_est_tokens": stale_est,
            "stale_uids": stale_uids,
            "raw_output_est_tokens": raw_output_est,
            "density": density,
            "tier1_old_output_share": tier1,
            # cached==0 marks a prompt-prefix reset; in managed runs these
            # are the fork's recovery-turn requests, which sample with a
            # reduced tool surface + an ephemeral (non-persisted) prompt
            # tail, so their calibration ratio is expected to drift.
            "cache_reset": snapshot.cached_input_tokens == 0,
            # Calibration outliers cluster at small values of this: the
            # recovery turns right after a rollback (see module docstring).
            "requests_since_rollback": since_rollback,
        }
        if snapshot.input_tokens:
            plain = total_est / snapshot.input_tokens
            entry["plain_ratio"] = round(plain, 4)
            plain_ratios.append(plain)
            residuals.append(snapshot.input_tokens - total_est)
        series.append(entry)

    fitted_offset = statistics.median(residuals) if residuals else None
    corrected_ratios = []
    if fitted_offset is not None:
        for entry in series:
            if entry.get("input_tokens"):
                corrected = (entry["est_tokens"] + fitted_offset) / entry["input_tokens"]
                entry["corrected_ratio"] = round(corrected, 4)
                corrected_ratios.append(corrected)

    calibration: dict[str, Any] = {
        "estimator": replay.estimator_mode,
        "n_requests_with_usage": len(plain_ratios),
        "plain_ratio_median": round(statistics.median(plain_ratios), 4)
        if plain_ratios
        else None,
        "fitted_offset_tokens": int(fitted_offset) if fitted_offset is not None else None,
        "corrected_ratio_median": round(statistics.median(corrected_ratios), 4)
        if corrected_ratios
        else None,
        "corrected_ratio_p10": round(
            sorted(corrected_ratios)[max(0, len(corrected_ratios) // 10)], 4
        )
        if corrected_ratios
        else None,
        "corrected_ratio_p90": round(
            sorted(corrected_ratios)[min(len(corrected_ratios) - 1, len(corrected_ratios) * 9 // 10)],
            4,
        )
        if corrected_ratios
        else None,
    }
    ok = (
        len(corrected_ratios) >= 3
        and calibration["corrected_ratio_median"] is not None
        and abs(calibration["corrected_ratio_median"] - 1.0) <= CALIBRATION_TOLERANCE
    )
    calibration["within_tolerance"] = ok
    calibration["tolerance"] = CALIBRATION_TOLERANCE
    calibration["n_outlier_requests"] = sum(
        1 for ratio in corrected_ratios if abs(ratio - 1.0) > CALIBRATION_TOLERANCE
    )

    densities = [entry["density"] for entry in series if entry["density"] is not None]
    tier1s = [
        entry["tier1_old_output_share"]
        for entry in series
        if entry["tier1_old_output_share"] is not None
    ]
    tail_n = max(1, int(len(densities) * TAIL_FRACTION)) if densities else 0
    summary = {
        "mean_density": round(statistics.mean(densities), 4) if densities else None,
        "min_density": round(min(densities), 4) if densities else None,
        "tail_density": round(statistics.mean(densities[-tail_n:]), 4)
        if densities
        else None,
        "final_density": round(densities[-1], 4) if densities else None,
        "tier1_mean": round(statistics.mean(tier1s), 4) if tier1s else None,
        "tier1_max": round(max(tier1s), 4) if tier1s else None,
        "tier1_final": round(tier1s[-1], 4) if tier1s else None,
        "density_trusted": ok,
    }
    return {
        "calibration": calibration,
        "summary": summary,
        "series": series,
    }


# ---------------------------------------------------------------------------
# M2: hygiene cadence
# ---------------------------------------------------------------------------


def pressure_band(
    used_tokens: int | None,
    context_window: int | None,
    hard_context_window: int | None,
) -> str:
    """Mirror of mcp.rs context_pressure_snapshot_for: critical >= hard
    window, high >= window, watch >= 85% of window, else ok."""
    if used_tokens is None or not context_window:
        return "unknown"
    if hard_context_window and hard_context_window > 0 and used_tokens >= hard_context_window:
        return "critical"
    if used_tokens >= context_window:
        return "high"
    if used_tokens >= math.floor(context_window * WATCH_THRESHOLD_PCT / 100.0):
        return "watch"
    return "ok"


def _snapshot_band(snapshot: RequestSnapshot) -> str:
    # Product semantics: backend usage compares last_token_usage.total_tokens
    # against the window (main.rs context_rewind_backend_usage_from_rollout_entry).
    used = snapshot.total_tokens if snapshot.total_tokens is not None else snapshot.input_tokens
    return pressure_band(used, snapshot.context_window, snapshot.hard_context_window)


def _last_snapshot_before(
    replay: RolloutReplay, moment: datetime | None
) -> RequestSnapshot | None:
    if moment is None:
        return None
    best = None
    for snapshot in replay.requests:
        if snapshot.timestamp is not None and snapshot.timestamp <= moment:
            best = snapshot
        elif snapshot.timestamp is not None and snapshot.timestamp > moment:
            break
    return best


def compute_hygiene(
    replay: RolloutReplay, records: list[dict[str, Any]]
) -> dict[str, Any]:
    n_requests = len(replay.requests)
    processed_tokens = sum(
        snapshot.input_tokens or 0 for snapshot in replay.requests
    )

    band_histogram = {"ok": 0, "watch": 0, "high": 0, "critical": 0, "unknown": 0}
    per_record = []
    for record in records:
        created = parse_timestamp(record.get("created_at"))
        snapshot = _last_snapshot_before(replay, created)
        band = _snapshot_band(snapshot) if snapshot is not None else "unknown"
        band_histogram[band] += 1
        per_record.append(
            {
                "record_id": record.get("record_id"),
                "created_at": record.get("created_at"),
                "item_id": record.get("item_id"),
                "position": record.get("position"),
                "band": band,
                "used_tokens": snapshot.total_tokens if snapshot else None,
                "context_window": snapshot.context_window if snapshot else None,
                "request_index": snapshot.index if snapshot else None,
                "detached_fission_group_ids": record.get("detached_fission_group_ids")
                or [],
            }
        )

    # Turns-to-prune for big tool outputs.
    tracked = []
    for item in replay.items.values():
        if item.kind in OUTPUT_ITEM_TYPES and item.est_tokens > PRUNE_TRACK_MIN_TOKENS:
            if item.removed_at_request is not None:
                tracked.append(
                    {
                        "uid": item.uid,
                        "est_tokens": item.est_tokens,
                        "requests_to_prune": item.removed_at_request - item.birth_request,
                        "cause": item.removal_cause,
                    }
                )
            else:
                tracked.append(
                    {
                        "uid": item.uid,
                        "est_tokens": item.est_tokens,
                        "requests_to_prune": None,  # never pruned
                        "cause": None,
                    }
                )
    finite = sorted(
        entry["requests_to_prune"]
        for entry in tracked
        if entry["requests_to_prune"] is not None
    )

    def percentile(values: list[int], fraction: float) -> int | None:
        if not values:
            return None
        return values[min(len(values) - 1, int(len(values) * fraction))]

    # Hygiene-tool token overhead inside the billed prompt view.
    def hygiene_uids(snapshot: RequestSnapshot) -> int:
        total = 0
        for uid in replay.billed_input_uids(snapshot):
            item = replay.items[uid]
            name = item.name
            if name is None and item.kind in OUTPUT_ITEM_TYPES:
                for key in ("call_id", "callId"):
                    call_id = item.payload.get(key)
                    if isinstance(call_id, str):
                        name = replay.call_name_by_call_id.get(call_id.strip())
                        if name:
                            break
            if name in HYGIENE_TOOL_NAMES:
                total += item.est_tokens
        return total

    overhead_shares = []
    for snapshot in replay.requests:
        total_est = sum(
            replay.items[uid].est_tokens for uid in replay.billed_input_uids(snapshot)
        )
        if total_est > 0:
            overhead_shares.append(hygiene_uids(snapshot) / total_est)

    return {
        "n_rewind_records": len(records),
        "n_backend_rollbacks": len(replay.rollbacks),
        "rollbacks_by_resolution": _count_by(
            [event.resolved_by for event in replay.rollbacks]
        ),
        "n_requests": n_requests,
        "processed_input_tokens": processed_tokens,
        "rewinds_per_100_requests": round(len(records) / n_requests * 100, 2)
        if n_requests
        else None,
        "rewinds_per_1m_tokens": round(len(records) / processed_tokens * 1_000_000, 2)
        if processed_tokens
        else None,
        "pressure_band_histogram": band_histogram,
        "per_record": per_record,
        "turns_to_prune": {
            "tracked_outputs_gt_2k": len(tracked),
            "pruned": len(finite),
            "pruned_fraction": round(len(finite) / len(tracked), 3) if tracked else None,
            "median_requests": percentile(finite, 0.5),
            "p90_requests": percentile(finite, 0.9),
        },
        "hygiene_tool_overhead_share_mean": round(statistics.mean(overhead_shares), 4)
        if overhead_shares
        else None,
        "request_band_series": [_snapshot_band(s) for s in replay.requests],
    }


def _count_by(values: list[str]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for value in values:
        counts[value] = counts.get(value, 0) + 1
    return counts


# ---------------------------------------------------------------------------
# M3a: re-derivation
# ---------------------------------------------------------------------------


def compute_rederivation(
    replay: RolloutReplay, records: list[dict[str, Any]]
) -> dict[str, Any]:
    output_by_call_id: dict[str, HistoryItem] = {}
    for item in replay.items.values():
        if item.kind in OUTPUT_ITEM_TYPES:
            for key in ("call_id", "callId"):
                call_id = item.payload.get(key)
                if isinstance(call_id, str) and call_id.strip():
                    output_by_call_id.setdefault(call_id.strip(), item)

    first_seen: dict[str, HistoryItem] = {}
    duplicates = []
    n_calls = 0
    for item in sorted(replay.items.values(), key=lambda entry: entry.uid):
        if item.kind not in ("function_call", "custom_tool_call", "local_shell_call"):
            continue
        name = item.name
        if name in HYGIENE_TOOL_NAMES:
            continue  # management tools repeat by design; tracked under M2
        n_calls += 1
        key = normalize_call(name, item.payload.get("arguments") or item.payload.get("action"))
        first = first_seen.get(key)
        if first is None:
            first_seen[key] = item
            continue

        # Classify against the first occurrence's liveness at re-call time.
        pruned_before_recall = (
            first.removed_at_line is not None
            and item.line_no > 0
            and first.removed_at_line < item.line_no
        )
        token_weight = item.est_tokens
        for cid_key in ("call_id", "callId"):
            call_id = item.payload.get(cid_key)
            if isinstance(call_id, str) and call_id.strip() in output_by_call_id:
                token_weight += output_by_call_id[call_id.strip()].est_tokens
                break
        entry: dict[str, Any] = {
            "name": name,
            "key_preview": key[:160],
            "first_uid": first.uid,
            "dup_uid": item.uid,
            "dup_request": item.birth_request,
            "class": "post_prune" if pruned_before_recall else "in_context",
            "prune_cause": first.removal_cause if pruned_before_recall else None,
            "token_weight": token_weight,
        }
        if pruned_before_recall and records:
            # Was the pruned fact available in an intervening primer?
            call_ids = extract_identifiers(item.text())
            first_output = None
            for cid_key in ("call_id", "callId"):
                call_id = first.payload.get(cid_key)
                if isinstance(call_id, str) and call_id.strip() in output_by_call_id:
                    first_output = output_by_call_id[call_id.strip()]
                    break
            fact_ids = call_ids | (first_output.identifiers() if first_output else set())
            recall_at = item.timestamp
            primer_hit = False
            had_primer = False
            for record in records:
                created = parse_timestamp(record.get("created_at"))
                if created is None or recall_at is None:
                    continue
                if created <= recall_at:
                    had_primer = True
                    primer_text = " ".join(
                        [str(record.get("primer") or "")]
                        + [str(entry) for entry in record.get("preserve") or []]
                    )
                    if fact_ids & extract_identifiers(primer_text):
                        primer_hit = True
                        break
            entry["primer_status"] = (
                "primer_ignored"
                if primer_hit
                else ("primer_missing" if had_primer else "no_primer")
            )
        duplicates.append(entry)

    post_prune = [entry for entry in duplicates if entry["class"] == "post_prune"]
    in_context = [entry for entry in duplicates if entry["class"] == "in_context"]
    return {
        "n_calls": n_calls,
        "n_duplicates": len(duplicates),
        "duplicate_token_weight": sum(entry["token_weight"] for entry in duplicates),
        "in_context": {
            "count": len(in_context),
            "token_weight": sum(entry["token_weight"] for entry in in_context),
        },
        "post_prune": {
            "count": len(post_prune),
            "token_weight": sum(entry["token_weight"] for entry in post_prune),
            "by_cause": _count_by(
                [str(entry["prune_cause"]) for entry in post_prune]
            ),
            "primer_status": _count_by(
                [
                    str(entry.get("primer_status"))
                    for entry in post_prune
                    if entry.get("primer_status")
                ]
            ),
        },
        "duplicates": duplicates,
    }


# ---------------------------------------------------------------------------
# M4: primer chain
# ---------------------------------------------------------------------------

_FACT_SPLIT_RE = re.compile(r"(?<=[.!?])\s+|\n+")
_COMMAND_HINT_RE = re.compile(
    r"`[^`]+`|(?:^|\s)(?:\$|cargo |python3? |node |git |make |pip3? |npm |bash |sh )"
)
_NUMBER_HINT_RE = re.compile(r"\b\d[\d,_.]*\b|0x[0-9a-fA-F]+")
_PATH_HINT_RE = re.compile(r"(?:/[\w.+\-]+)+|\b[\w\-]+\.\w{1,5}\b")


def _fact_type(text: str) -> str:
    if _COMMAND_HINT_RE.search(text):
        return "command"
    if _PATH_HINT_RE.search(text):
        return "path"
    if _NUMBER_HINT_RE.search(text):
        return "number"
    return "decision"


def _record_rollback_line(
    replay: RolloutReplay, record: dict[str, Any]
) -> int | None:
    """Locate the thread_rolled_back marker this record produced (anchor id
    + position match; nearest after the record's creation time wins)."""
    item_id = str(record.get("item_id") or "")
    position = str(record.get("position") or "").lower()
    created = parse_timestamp(record.get("created_at"))
    best = None
    for event in replay.rollbacks:
        if item_id and event.anchor_item_id == item_id and (
            not position or (event.anchor_position or "after") == position
        ):
            if created is not None and event.timestamp is not None:
                delta = abs((event.timestamp - created).total_seconds())
                if best is None or delta < best[0]:
                    best = (delta, event.line_no)
            elif best is None:
                best = (float("inf"), event.line_no)
    return best[1] if best else None


def compute_primer_chain(
    replay: RolloutReplay,
    records: list[dict[str, Any]],
    rederivation: dict[str, Any],
    estimator: TokenEstimator,
) -> dict[str, Any]:
    chain = []
    consumption_items = [
        item
        for item in replay.items.values()
        if item.kind in CONSUMPTION_KINDS
        or (item.kind == "message" and (item.role or "").lower() == "assistant")
    ]
    post_prune_dups = [
        entry for entry in rederivation.get("duplicates", []) if entry["class"] == "post_prune"
    ]

    for index, record in enumerate(records):
        primer = str(record.get("primer") or "")
        preserve = [str(entry) for entry in record.get("preserve") or []]
        facts = []
        for sentence in _FACT_SPLIT_RE.split(primer):
            sentence = sentence.strip()
            if len(sentence) >= 15:
                facts.append({"text": sentence, "source": "primer"})
        for entry in preserve:
            entry = entry.strip()
            if entry:
                facts.append({"text": entry, "source": "preserve"})
        for fact in facts:
            fact["type"] = _fact_type(fact["text"])
            fact["ids"] = extract_identifiers(fact["text"], cap=40)

        next_text = ""
        if index + 1 < len(records):
            next_record = records[index + 1]
            next_text = " ".join(
                [str(next_record.get("primer") or "")]
                + [str(entry) for entry in next_record.get("preserve") or []]
            )
        next_ids = extract_identifiers(next_text, cap=2000) if next_text else set()
        rollback_line = _record_rollback_line(replay, record)
        post_items = (
            [item for item in consumption_items if item.line_no > rollback_line]
            if rollback_line is not None
            else []
        )

        n_carried = n_relevant = n_rederived = 0
        fact_rows = []
        for fact in facts:
            ids = fact["ids"]
            carried = None
            if next_text:
                normalized = " ".join(fact["text"].lower().split())
                carried = bool(
                    (ids and len(ids & next_ids) >= max(1, len(ids) // 2))
                    or (normalized and normalized in " ".join(next_text.lower().split()))
                )
                n_carried += 1 if carried else 0
            relevant = None
            if post_items and ids:
                relevant = any(ids & item.identifiers() for item in post_items)
                n_relevant += 1 if relevant else 0
            rederived = bool(
                ids
                and any(
                    ids
                    & extract_identifiers(
                        str(replay.items[dup["dup_uid"]].text()), cap=200
                    )
                    for dup in post_prune_dups
                    if rollback_line is None
                    or replay.items[dup["dup_uid"]].line_no > rollback_line
                )
            )
            n_rederived += 1 if rederived else 0
            fact_rows.append(
                {
                    "type": fact["type"],
                    "source": fact["source"],
                    "carried_to_next": carried,
                    "referenced_after_rewind": relevant,
                    "rederived_post_prune": rederived,
                    "text_preview": fact["text"][:120],
                }
            )

        chain.append(
            {
                "record_id": record.get("record_id"),
                "created_at": record.get("created_at"),
                "rollback_line": rollback_line,
                "primer_tokens": estimator.estimate_text(primer),
                "preserve_entries": len(preserve),
                "n_facts": len(facts),
                "fact_types": _count_by([fact["type"] for fact in facts]),
                "carried_to_next": n_carried if next_text else None,
                "referenced_after_rewind": n_relevant if post_items else None,
                "rederived_post_prune": n_rederived,
                "facts": fact_rows,
            }
        )

    growth = None
    sizes = [entry["primer_tokens"] for entry in chain if entry["primer_tokens"]]
    if len(sizes) >= 2 and sizes[0] > 0:
        growth = round(sizes[-1] / sizes[0], 3)
    return {
        "n_records": len(records),
        "primer_tokens_series": sizes,
        "primer_growth_last_over_first": growth,
        "chain": chain,
    }


# ---------------------------------------------------------------------------
# Extras: fission, M5 passthrough
# ---------------------------------------------------------------------------


def compute_fission(
    trial: TrialPaths, records: list[dict[str, Any]], aux_rollouts: list[Path]
) -> dict[str, Any] | None:
    ledger = None
    if trial.intendant_dir is not None:
        ledger_path = trial.intendant_dir / "fission_ledger.json"
        if ledger_path.is_file():
            try:
                ledger = json.loads(ledger_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                ledger = None
    if ledger is None:
        for record in reversed(records):
            embedded = record.get("fission_ledger")
            if isinstance(embedded, dict):
                ledger = embedded
                break
    detached = sorted(
        {
            group_id
            for record in records
            for group_id in record.get("detached_fission_group_ids") or []
        }
    )
    if ledger is None and not detached and not aux_rollouts:
        return None
    groups = (ledger or {}).get("groups") or []
    branches = [branch for group in groups for branch in group.get("branches") or []]
    return {
        "groups": len(groups),
        "branches": len(branches),
        "branch_status": _count_by(
            [str(branch.get("status")) for branch in branches]
        ),
        "detached_group_ids": detached,
        "auxiliary_rollouts": [path.name for path in aux_rollouts],
    }


def compute_passthrough(trial: TrialPaths) -> dict[str, Any] | None:
    if trial.result_json is None:
        return None
    try:
        result = json.loads(trial.result_json.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    reward = None
    verifier = result.get("verifier_result")
    if isinstance(verifier, dict):
        rewards = verifier.get("rewards")
        if isinstance(rewards, dict):
            reward = rewards.get("reward")
    agent = result.get("agent_result") or {}
    out = {
        "task_name": result.get("task_name"),
        "trial_name": result.get("trial_name"),
        "reward": reward,
        "n_input_tokens": agent.get("n_input_tokens"),
        "n_cache_tokens": agent.get("n_cache_tokens"),
        "n_output_tokens": agent.get("n_output_tokens"),
        "cost_usd": agent.get("cost_usd"),
        "started_at": result.get("started_at"),
        "finished_at": result.get("finished_at"),
    }
    return out


# ---------------------------------------------------------------------------
# Per-trial driver
# ---------------------------------------------------------------------------


def analyze_trial(trial: TrialPaths, estimator: TokenEstimator) -> dict[str, Any]:
    records = load_rewind_records(trial.intendant_dir)
    main_rollout, aux_rollouts = select_main_rollout(trial.sessions_dir, records)
    replay = replay_rollout(main_rollout, estimator)

    density = compute_density(replay)
    hygiene = compute_hygiene(replay, records)
    rederivation = compute_rederivation(replay, records)
    primer_chain = compute_primer_chain(replay, records, rederivation, estimator)
    fission = compute_fission(trial, records, aux_rollouts)
    passthrough = compute_passthrough(trial)

    return {
        "trial": trial.name,
        "lane": trial.lane,
        "rollout": str(main_rollout),
        "n_rollout_lines_items": len(replay.items),
        "n_requests": len(replay.requests),
        "n_compactions": len(replay.compactions),
        "m1_density": density,
        "m2_hygiene": hygiene,
        "m3a_rederivation": rederivation,
        "m4_primer_chain": primer_chain,
        "fission": fission,
        "m5_passthrough": passthrough,
    }


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def maybe_plot(report: dict[str, Any], out_path: Path) -> bool:
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except Exception:
        return False
    series = report["m1_density"]["series"]
    if not series:
        return False
    xs = [entry["request"] for entry in series]
    density = [entry["density"] for entry in series]
    tier1 = [entry["tier1_old_output_share"] for entry in series]
    inputs = [entry["input_tokens"] for entry in series]
    window = next(
        (entry["context_window"] for entry in series if entry.get("context_window")),
        None,
    )

    fig, ax1 = plt.subplots(figsize=(10, 5))
    ax1.plot(xs, density, label="density (1 - stale share)", color="tab:blue")
    ax1.plot(xs, tier1, label="tier-1 old-output share", color="tab:orange")
    ax1.set_xlabel("model request")
    ax1.set_ylabel("share of live context")
    ax1.set_ylim(0, 1.05)
    ax2 = ax1.twinx()
    ax2.plot(xs, inputs, label="reported input tokens", color="tab:gray", alpha=0.5)
    if window:
        ax2.axhline(window, color="tab:red", linestyle=":", alpha=0.6)
        ax2.axhline(
            math.floor(window * WATCH_THRESHOLD_PCT / 100),
            color="tab:red",
            linestyle="--",
            alpha=0.3,
        )
    ax2.set_ylabel("tokens")
    for event_request in {
        entry["request_index"]
        for entry in report["m2_hygiene"]["per_record"]
        if entry.get("request_index") is not None
    }:
        ax1.axvline(event_request, color="tab:green", alpha=0.25)
    lines1, labels1 = ax1.get_legend_handles_labels()
    lines2, labels2 = ax2.get_legend_handles_labels()
    ax1.legend(lines1 + lines2, labels1 + labels2, loc="lower left", fontsize=8)
    ax1.set_title(f"{report['trial']} ({report['lane']})")
    fig.tight_layout()
    fig.savefig(out_path, dpi=120)
    plt.close(fig)
    return True


def fmt(value: Any) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.3f}"
    return str(value)


def render_markdown(reports: list[dict[str, Any]], plots: dict[str, str]) -> str:
    lines = ["# Managed-context density report", ""]
    lines.append(
        "| trial | lane | reward | requests | mean density | min | tail | tier1 mean | "
        "calib (corr p50) | rewinds | dup post-prune | hygiene share |"
    )
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for report in reports:
        m1 = report["m1_density"]["summary"]
        cal = report["m1_density"]["calibration"]
        m2 = report["m2_hygiene"]
        m3 = report["m3a_rederivation"]
        m5 = report.get("m5_passthrough") or {}
        calib = (
            f"{fmt(cal['corrected_ratio_median'])}"
            f"{'' if cal['within_tolerance'] else ' (!)'}"
        )
        lines.append(
            f"| {report['trial']} | {report['lane']} | {fmt(m5.get('reward'))} "
            f"| {report['n_requests']} | {fmt(m1['mean_density'])} | {fmt(m1['min_density'])} "
            f"| {fmt(m1['tail_density'])} | {fmt(m1['tier1_mean'])} | {calib} "
            f"| {m2['n_rewind_records']} | {m3['post_prune']['count']} "
            f"| {fmt(m2['hygiene_tool_overhead_share_mean'])} |"
        )
    lines.append("")

    # Per-lane aggregate when there is more than one trial.
    if len(reports) > 1:
        lines.append("## Cross-trial aggregate (per lane)")
        lines.append("")
        lines.append(
            "| lane | trials | mean density | mean tail | mean tier1 | "
            "rewinds/100req | dup token weight | mean reward | mean cost USD |"
        )
        lines.append("|---|---|---|---|---|---|---|---|---|")
        for lane in ("managed", "vanilla"):
            lane_reports = [report for report in reports if report["lane"] == lane]
            if not lane_reports:
                continue

            def mean_of(getter) -> float | None:
                values = [
                    getter(report) for report in lane_reports if getter(report) is not None
                ]
                return round(statistics.mean(values), 4) if values else None

            lines.append(
                "| {lane} | {n} | {dens} | {tail} | {tier1} | {cad} | {dup} | {rew} | {cost} |".format(
                    lane=lane,
                    n=len(lane_reports),
                    dens=fmt(mean_of(lambda r: r["m1_density"]["summary"]["mean_density"])),
                    tail=fmt(mean_of(lambda r: r["m1_density"]["summary"]["tail_density"])),
                    tier1=fmt(mean_of(lambda r: r["m1_density"]["summary"]["tier1_mean"])),
                    cad=fmt(
                        mean_of(lambda r: r["m2_hygiene"]["rewinds_per_100_requests"])
                    ),
                    dup=fmt(
                        mean_of(
                            lambda r: r["m3a_rederivation"]["post_prune"]["token_weight"]
                        )
                    ),
                    rew=fmt(
                        mean_of(lambda r: (r.get("m5_passthrough") or {}).get("reward"))
                    ),
                    cost=fmt(
                        mean_of(lambda r: (r.get("m5_passthrough") or {}).get("cost_usd"))
                    ),
                )
            )
        lines.append("")

    for report in reports:
        m1 = report["m1_density"]
        m2 = report["m2_hygiene"]
        m3 = report["m3a_rederivation"]
        m4 = report["m4_primer_chain"]
        lines.append(f"## {report['trial']} ({report['lane']})")
        lines.append("")
        cal = m1["calibration"]
        lines.append(
            f"- rollout: `{Path(report['rollout']).name}`; requests: {report['n_requests']}; "
            f"compactions: {report['n_compactions']}; backend rollbacks: {m2['n_backend_rollbacks']} "
            f"{m2['rollbacks_by_resolution'] or ''}"
        )
        lines.append(
            f"- calibration: estimator `{cal['estimator']}`, plain p50 {fmt(cal['plain_ratio_median'])}, "
            f"fitted offset {fmt(cal['fitted_offset_tokens'])} tok, corrected p50 "
            f"{fmt(cal['corrected_ratio_median'])} (p10 {fmt(cal['corrected_ratio_p10'])} / "
            f"p90 {fmt(cal['corrected_ratio_p90'])}) -> "
            f"{'TRUSTED' if cal['within_tolerance'] else 'NOT trusted (tier-1 proxy only)'}"
        )
        summary = m1["summary"]
        lines.append(
            f"- density: mean {fmt(summary['mean_density'])}, min {fmt(summary['min_density'])}, "
            f"tail {fmt(summary['tail_density'])}, final {fmt(summary['final_density'])}; "
            f"tier-1 old-output share: mean {fmt(summary['tier1_mean'])}, "
            f"max {fmt(summary['tier1_max'])}, final {fmt(summary['tier1_final'])}"
        )
        lines.append(
            f"- hygiene: {m2['n_rewind_records']} rewind records "
            f"({m2['rewinds_per_100_requests'] if m2['rewinds_per_100_requests'] is not None else '-'} per 100 req, "
            f"{m2['rewinds_per_1m_tokens'] if m2['rewinds_per_1m_tokens'] is not None else '-'} per 1M tok); "
            f"bands {m2['pressure_band_histogram']}; turns-to-prune "
            f"median {fmt(m2['turns_to_prune']['median_requests'])} / "
            f"p90 {fmt(m2['turns_to_prune']['p90_requests'])} "
            f"(pruned {fmt(m2['turns_to_prune']['pruned_fraction'])} of "
            f"{m2['turns_to_prune']['tracked_outputs_gt_2k']} tracked); "
            f"hygiene-tool share {fmt(m2['hygiene_tool_overhead_share_mean'])}"
        )
        lines.append(
            f"- re-derivation: {m3['n_duplicates']} duplicates of {m3['n_calls']} calls "
            f"({m3['duplicate_token_weight']} tok); in-context {m3['in_context']['count']}, "
            f"post-prune {m3['post_prune']['count']} "
            f"(causes {m3['post_prune']['by_cause'] or '{}'}, "
            f"primer {m3['post_prune']['primer_status'] or '{}'})"
        )
        if m4["n_records"]:
            lines.append(
                f"- primer chain: {m4['n_records']} primers, tokens {m4['primer_tokens_series']}, "
                f"growth {fmt(m4['primer_growth_last_over_first'])}"
            )
            lines.append("")
            lines.append(
                "| record | band | primer tok | facts | types | carried→next | ref'd after | rederived |"
            )
            lines.append("|---|---|---|---|---|---|---|---|")
            band_by_record = {
                entry["record_id"]: entry["band"] for entry in m2["per_record"]
            }
            for entry in m4["chain"]:
                lines.append(
                    f"| {str(entry['record_id'])[:18]} | {band_by_record.get(entry['record_id'], '-')} "
                    f"| {entry['primer_tokens']} | {entry['n_facts']} | {entry['fact_types']} "
                    f"| {fmt(entry['carried_to_next'])} | {fmt(entry['referenced_after_rewind'])} "
                    f"| {entry['rederived_post_prune']} |"
                )
        if report.get("fission"):
            lines.append(f"- fission: {json.dumps(report['fission'])}")
        if report["trial"] in plots:
            lines.append(f"- density plot: `{plots[report['trial']]}`")
        lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Self-test (metric level, on the committed fixtures)
# ---------------------------------------------------------------------------


def self_test() -> None:
    fixtures = Path(__file__).resolve().parent / "fixtures" / "density_replay"
    estimator = TokenEstimator()

    managed = analyze_trial(resolve_trial(fixtures / "managed-trial"), estimator)
    assert managed["lane"] == "managed"
    assert managed["n_requests"] == 8
    assert managed["n_compactions"] == 1

    # M2: the single record lands in the watch band (9000 >= 8500, < 10000).
    m2 = managed["m2_hygiene"]
    assert m2["n_rewind_records"] == 1
    assert m2["pressure_band_histogram"]["watch"] == 1, m2["pressure_band_histogram"]
    assert m2["n_backend_rollbacks"] == 3
    assert m2["rollbacks_by_resolution"] == {"anchor": 2, "num_turns": 1}
    assert m2["rewinds_per_100_requests"] == 12.5

    # M1: the big call_1 output (uid 3) is stale at requests 3 and 4
    # (age >= 3, identifiers never referenced later); the call_2 output is
    # referenced (magic_token_xyz123) so it never goes stale.
    series = managed["m1_density"]["series"]
    stale_by_request = {entry["request"]: entry["stale_uids"] for entry in series}
    assert stale_by_request[0] == [] and stale_by_request[2] == []
    assert stale_by_request[3] == [3], stale_by_request
    assert stale_by_request[4] == [3]
    assert all(stale_by_request[i] == [] for i in (5, 6, 7))
    assert all(
        entry["tier1_old_output_share"] == 0 for entry in series
    ), "no output ages past 5 requests in the managed fixture"
    densities = [entry["density"] for entry in series]
    assert densities[3] < 1.0 and densities[2] == 1.0

    # M3a: call_3 duplicates call_1 (whitespace-only diff) after the
    # compaction pruned call_1 -> post_prune/compaction; the fixture primer
    # does not contain the call's identifiers -> primer_missing.
    m3 = managed["m3a_rederivation"]
    assert m3["n_duplicates"] == 1
    dup = m3["duplicates"][0]
    assert dup["class"] == "post_prune" and dup["prune_cause"] == "compaction"
    assert dup["primer_status"] == "primer_missing"

    # M4: the primer facts referencing magic_token_xyz123 are seen again
    # after the rewind (assistant message m_3).
    m4 = managed["m4_primer_chain"]
    assert m4["n_records"] == 1
    chain_entry = m4["chain"][0]
    assert chain_entry["rollback_line"] is not None
    assert chain_entry["n_facts"] >= 3
    assert chain_entry["referenced_after_rewind"] and chain_entry["referenced_after_rewind"] >= 1

    assert managed["m5_passthrough"]["reward"] == 1.0

    vanilla = analyze_trial(resolve_trial(fixtures / "vanilla-trial"), estimator)
    assert vanilla["lane"] == "vanilla"
    assert vanilla["n_requests"] == 8
    assert vanilla["n_compactions"] == 1
    vseries = vanilla["m1_density"]["series"]
    # The big dump output (uid 2, born request 0) ages into the tier-1
    # proxy at requests 5 and 6 and is compacted away by request 7.
    tier1 = {entry["request"]: entry["tier1_old_output_share"] for entry in vseries}
    assert tier1[4] == 0 and tier1[5] > 0.3 and tier1[6] > 0.3 and tier1[7] == 0, tier1
    vstale = {entry["request"]: entry["stale_uids"] for entry in vseries}
    assert all(vstale[i] == [2] for i in (3, 4, 5, 6)), vstale
    assert vstale[7] == []
    # M3a vanilla lane: duplicate after the compaction boundary.
    vm3 = vanilla["m3a_rederivation"]
    assert vm3["n_duplicates"] == 1
    vdup = vm3["duplicates"][0]
    assert vdup["class"] == "post_prune" and vdup["prune_cause"] == "compaction"
    assert "primer_status" not in vdup  # no rewind records in the vanilla lane

    print("managed_density_report self-test OK")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("trial_dirs", nargs="*", type=Path, help="harbor trial dirs")
    parser.add_argument("--out", type=Path, help="report output directory")
    parser.add_argument(
        "--json-only", action="store_true", help="emit per-trial JSON only"
    )
    parser.add_argument("--no-plot", action="store_true", help="skip matplotlib plots")
    parser.add_argument(
        "--sessions-dir", type=Path, help="bare mode: codex sessions dir"
    )
    parser.add_argument(
        "--intendant-dir", type=Path, help="bare mode: intendant session log dir"
    )
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return

    trials: list[TrialPaths] = []
    if args.sessions_dir:
        intendant = args.intendant_dir
        trials.append(
            TrialPaths(
                name=args.sessions_dir.parent.name or "session",
                root=None,
                sessions_dir=args.sessions_dir,
                intendant_dir=intendant if intendant and intendant.is_dir() else None,
                result_json=None,
            )
        )
    for trial_dir in args.trial_dirs:
        trials.append(resolve_trial(trial_dir))
    if not trials:
        parser.error("no trial dirs given (and no --sessions-dir)")
    if not args.out:
        parser.error("--out is required")

    args.out.mkdir(parents=True, exist_ok=True)
    estimator = TokenEstimator()
    reports = []
    plots: dict[str, str] = {}
    for trial in trials:
        report = analyze_trial(trial, estimator)
        reports.append(report)
        json_path = args.out / f"{trial.name}.json"
        json_path.write_text(json.dumps(report, indent=2, default=str), encoding="utf-8")
        print(f"wrote {json_path}")
        if not args.json_only and not args.no_plot:
            plot_path = args.out / f"{trial.name}.density.png"
            if maybe_plot(report, plot_path):
                plots[trial.name] = plot_path.name
                print(f"wrote {plot_path}")

    if not args.json_only:
        md_path = args.out / "report.md"
        md_path.write_text(render_markdown(reports, plots), encoding="utf-8")
        print(f"wrote {md_path}")
    if len(reports) > 1:
        agg_path = args.out / "aggregate.json"
        agg = {
            "lanes": {
                lane: [report["trial"] for report in reports if report["lane"] == lane]
                for lane in ("managed", "vanilla")
            },
            "trials": [
                {
                    "trial": report["trial"],
                    "lane": report["lane"],
                    "reward": (report.get("m5_passthrough") or {}).get("reward"),
                    "mean_density": report["m1_density"]["summary"]["mean_density"],
                    "tail_density": report["m1_density"]["summary"]["tail_density"],
                    "tier1_mean": report["m1_density"]["summary"]["tier1_mean"],
                    "calibration_ok": report["m1_density"]["calibration"][
                        "within_tolerance"
                    ],
                    "rewinds_per_100_requests": report["m2_hygiene"][
                        "rewinds_per_100_requests"
                    ],
                    "post_prune_duplicates": report["m3a_rederivation"]["post_prune"][
                        "count"
                    ],
                }
                for report in reports
            ],
        }
        agg_path.write_text(json.dumps(agg, indent=2), encoding="utf-8")
        print(f"wrote {agg_path}")


if __name__ == "__main__":
    main()
