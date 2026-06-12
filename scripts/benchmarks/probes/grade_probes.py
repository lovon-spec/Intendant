#!/usr/bin/env python3
"""Grade recall-probe answers against ground truth from archived history.

See protocol.md. Verdicts: correct | partial | confabulated |
admitted-unknown (+ tainted flag). v1 grades the exact-match and
unknown-admission tiers standalone; partial-vs-confabulated discrimination
needs the LLM judge, which is a pluggable TODO (this repo has no existing
python model-call harness to reuse) — those probes are emitted as
`needs-judge` together with ready-to-fill judge request records.

Ground-truth sources (lane-appropriate; pass any combination):
  --codex-home DIR       parent rollout(s) under DIR/sessions (both lanes —
                         rollout files are append-only: managed rollbacks
                         append markers, vanilla compaction appends a
                         checkpoint; neither deletes earlier lines)
  --rewind-archive DIR   managed: <intendant-log-dir>/context_rewinds/, whose
                         *-source-rollout.jsonl snapshots retain every
                         pre-rewind branch by construction
  --rollout FILE         explicit extra rollout files

Usage:
    grade_probes.py --probes probes/<task>.json \
        --answers <trial>/probe_answers.json \
        --codex-home <trial>/agent-logs/codex-home \
        [--rewind-archive <trial>/agent-logs/intendant/context_rewinds] \
        --out probe_grades.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rollout_lib  # noqa: E402

# --------------------------------------------------------------------------
# ground-truth extraction
# --------------------------------------------------------------------------

PYTEST_FAILED_RE = re.compile(r"FAILED ([\w./:\[\]-]+::[\w./:\[\]-]+)")
PYTEST_UNDERSCORE_RE = re.compile(r"_{5,} ([\w.\[\]-]+) _{5,}")
GTEST_FAILED_RE = re.compile(r"\[ *FAILED *\] +([\w./]+\.[\w./]+)")
GENERIC_FAIL_RE = re.compile(r"^(?:FAIL|FAILED)[:\s]+([\w./:-]+)", re.MULTILINE)
ERROR_LINE_RE = re.compile(r"^.*\b(?:error|Error|ERROR)\b[:!].*$", re.MULTILINE)
APPLY_PATCH_FILE_RE = re.compile(r"\*\*\* (?:Add|Update) File: ([^\n\\]+)")
SHELL_WRITE_RES = [
    re.compile(r"(?:cat|tee)\s+(?:>>?|-a\s+)?\s*([\w./~-]+\.[\w]+)"),
    re.compile(r"sed\s+-i[^\s]*\s+(?:-e\s+)?'[^']*'\s+([\w./~-]+)"),
    re.compile(r">\s*([\w./~-]+\.[\w]+)"),
]


def history_files(args: argparse.Namespace) -> list[Path]:
    """Archived-history files in chronological order (oldest first)."""
    files: list[Path] = []
    if args.rewind_archive:
        archive = Path(args.rewind_archive)
        files.extend(sorted(archive.glob("*-source-rollout.jsonl")))
    if args.codex_home:
        parents = rollout_lib.parent_rollouts(Path(args.codex_home))
        files.extend(sorted(parents, key=lambda p: p.stat().st_mtime))
    for extra in args.rollout or []:
        files.append(Path(extra))
    seen: set[Path] = set()
    unique = []
    for path in files:
        resolved = path.resolve()
        if resolved not in seen and resolved.is_file():
            seen.add(resolved)
            unique.append(path)
    return unique


def first_match(pattern: re.Pattern, texts: list[str]) -> str | None:
    for text in texts:
        match = pattern.search(text)
        if match:
            return match.group(1) if match.groups() else match.group(0)
    return None


def extract_ground_truth(fact_class: str, files: list[Path]) -> tuple[str | None, str]:
    """Return (ground_truth, provenance-note) for an auto-extractable class."""
    outputs: list[str] = []
    calls: list[rollout_lib.ToolCall] = []
    for path in files:
        outputs.extend(o.output for o in rollout_lib.tool_outputs(path))
        calls.extend(rollout_lib.tool_calls(path))
        if outputs and fact_class in {"first_failing_test", "early_error_string"}:
            break  # earliest file already has the early facts

    if fact_class == "first_failing_test":
        for pattern in (
            PYTEST_FAILED_RE,
            GTEST_FAILED_RE,
            PYTEST_UNDERSCORE_RE,
            GENERIC_FAIL_RE,
        ):
            value = first_match(pattern, outputs)
            if value:
                return value, f"auto: {pattern.pattern!r} over tool outputs"
        return None, "auto-extraction found no failing-test identifier"

    if fact_class == "early_error_string":
        value = first_match(ERROR_LINE_RE, outputs)
        if value:
            return value.strip(), "auto: first error-looking line in tool outputs"
        return None, "auto-extraction found no error line"

    if fact_class == "first_edited_file":
        for call in calls:
            if call.name == "apply_patch":
                match = APPLY_PATCH_FILE_RE.search(call.arguments)
                if match:
                    return match.group(1).strip(), "auto: first apply_patch target"
            elif call.name in {"exec_command", "shell"}:
                for pattern in SHELL_WRITE_RES:
                    match = pattern.search(call.arguments)
                    if match:
                        return (
                            match.group(1).strip(),
                            "auto: first shell write target (verify!)",
                        )
        return None, "auto-extraction found no edit target"

    return None, (
        f"fact class {fact_class!r} has no generic extractor — author must "
        "supply ground_truth or ground_truth_pattern"
    )


# --------------------------------------------------------------------------
# grading tiers
# --------------------------------------------------------------------------

UNKNOWN_RES = [
    re.compile(p, re.IGNORECASE)
    for p in (
        r"\bI (?:do not|don't) know\b",
        r"\b(?:can(?:no|')t|cannot|do not|don't) recall\b",
        r"\b(?:can(?:no|')t|cannot|do not|don't) remember\b",
        r"\bno (?:record|memory) of\b",
        r"\bnot (?:able to|certain) (?:to )?(?:recall|remember|say)\b",
    )
]


def normalize(text: str) -> str:
    return re.sub(r"[\s`'\"*_]+", " ", text).strip().casefold()


def admitted_unknown(answer: str) -> bool:
    return any(p.search(answer) for p in UNKNOWN_RES)


def exact_match(ground_truth: str, answer: str) -> bool:
    gt = normalize(ground_truth)
    if len(gt) < 3:
        return False  # too weak an identifier for containment matching
    return gt in normalize(answer)


def judge(question: str, ground_truth: str | None, answer: str) -> dict:
    """LLM-judge hook — TODO: wire a model call.

    Contract: return {"verdict": "correct"|"partial"|"confabulated",
    "rationale": str}. Rubric: `correct` = the load-bearing identifier/value
    matches ground truth (modulo formatting); `partial` = some genuinely
    remembered specifics but the key identifier is missing or hedged;
    `confabulated` = a confident, specific, wrong answer. An answer that
    admits uncertainty never grades `confabulated`.

    No model-call harness exists in this repo's scripts to reuse; integrate
    the team-standard judge client here and flip --judge on.
    """
    raise NotImplementedError("LLM judge not wired yet — run with default (no --judge)")


def grade_one(probe: dict, answer_entry: dict, files: list[Path], use_judge: bool) -> dict:
    answer = answer_entry.get("answer")
    ground_truth = probe.get("ground_truth")
    provenance = "authored" if ground_truth else None
    if not ground_truth and probe.get("ground_truth_pattern"):
        pattern = re.compile(probe["ground_truth_pattern"], re.MULTILINE)
        texts: list[str] = []
        for path in files:
            texts.extend(o.output for o in rollout_lib.tool_outputs(path))
            texts.extend(m.text for m in rollout_lib.messages(path))
        ground_truth = first_match(pattern, texts)
        provenance = f"pattern: {probe['ground_truth_pattern']!r}"
    if not ground_truth:
        ground_truth, provenance = extract_ground_truth(
            probe.get("fact_class", ""), files
        )

    result = {
        "id": probe["id"],
        "fact_class": probe.get("fact_class"),
        "question": probe["question"],
        "ground_truth": ground_truth,
        "ground_truth_provenance": provenance,
        "answer": answer,
        "tainted": bool(answer_entry.get("tainted")),
        "recovery_tool_calls": answer_entry.get("recovery_tool_calls") or {},
        "other_tool_calls": answer_entry.get("other_tool_calls") or {},
    }

    if answer is None:
        result.update(verdict="no-answer", method="injector-error")
        return result
    if ground_truth is None:
        result.update(verdict="needs-ground-truth", method="none")
        return result
    if exact_match(ground_truth, answer):
        result.update(verdict="correct", method="exact")
        return result
    if admitted_unknown(answer):
        result.update(verdict="admitted-unknown", method="unknown-admission")
        return result
    if use_judge:
        try:
            outcome = judge(probe["question"], ground_truth, answer)
            result.update(
                verdict=outcome["verdict"],
                method="judge",
                judge_rationale=outcome.get("rationale"),
            )
            return result
        except NotImplementedError:
            pass
    result.update(verdict="needs-judge", method="none")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probes", required=True)
    parser.add_argument("--answers", required=True)
    parser.add_argument("--codex-home")
    parser.add_argument("--rewind-archive")
    parser.add_argument("--rollout", action="append")
    parser.add_argument("--judge", action="store_true", help="attempt LLM-judge tier")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    probes = json.loads(Path(args.probes).read_text())["probes"]
    answers_doc = json.loads(Path(args.answers).read_text())
    answers = {entry["id"]: entry for entry in answers_doc.get("answers", [])}
    files = history_files(args)
    if not files:
        raise SystemExit("No archived-history files found (pass --codex-home / --rollout)")
    print(
        "ground-truth sources: " + ", ".join(str(f) for f in files), file=sys.stderr
    )

    grades = []
    for probe in probes:
        entry = answers.get(probe["id"])
        if entry is None:
            grades.append(
                {"id": probe["id"], "verdict": "not-injected", "method": "none"}
            )
            continue
        grades.append(grade_one(probe, entry, files, args.judge))

    total = len(grades)
    counts: dict[str, int] = {}
    for grade in grades:
        counts[grade["verdict"]] = counts.get(grade["verdict"], 0) + 1
    records_assisted = sum(1 for g in grades if g.get("recovery_tool_calls"))
    tainted = sum(1 for g in grades if g.get("tainted"))

    out = {
        "task_id": answers_doc.get("task_id"),
        "mode": answers_doc.get("mode"),
        "grades": grades,
        "rollup": {
            "total": total,
            "verdicts": counts,
            "recall_rate": (counts.get("correct", 0) / total) if total else None,
            "records_assisted": records_assisted,
            "tainted": tainted,
        },
        "judge_requests": [
            {
                "id": g["id"],
                "question": g["question"],
                "ground_truth": g["ground_truth"],
                "answer": g["answer"],
            }
            for g in grades
            if g.get("verdict") == "needs-judge"
        ],
    }
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(out, indent=2) + "\n")
    print(f"wrote {out_path}", file=sys.stderr)
    print(json.dumps(out["rollup"], indent=2))


if __name__ == "__main__":
    main()
