#!/usr/bin/env python3
"""Inject post-completion recall probes into a benchmark session.

See protocol.md in this directory for the measurement rules. Two modes:

managed — drive follow-up turns on a LIVE Intendant-supervised Codex session
    through the web gateway WebSocket (the same `ControlMsg::FollowUp` the
    dashboard sends; cf. tests/skills/codex-fission-e2e/SKILL.md):
        {"action": "follow_up", "session_id": SID, "text": ..., "direct": true}
    The intendant session id is parsed from the launch banner ("Session ID:"
    line) in --console-log, or passed via --session-id. Answers are read from
    the parent codex rollout (the next `task_complete` event's
    `last_agent_message`); per-turn tool usage is collected from the same
    line span. Requires the `websockets` package.

vanilla — resume an ARCHIVED stock-codex session post-hoc, one probe per
    invocation of:
        CODEX_HOME=<archive> codex exec resume --skip-git-repo-check \
            -c sandbox_mode="read-only" -o <answer-file> <session-id> <probe>
    (syntax verified against npm codex 0.133.0: `exec resume` has no
    --sandbox flag — the sandbox is forced via `-c sandbox_mode`; the final
    agent message is captured with -o/--output-last-message). The rollout
    scan supplies per-turn tool stats and doubles as verification that
    resume continued the archived thread.

Examples:
    inject_probes.py managed --gateway ws://172.17.0.2:8901/ws \
        --codex-home <trial>/agent-logs/codex-home \
        --console-log <trial>/agent-logs/intendant-console.log \
        --probes probes/61810_cow.json --out probe_answers.json

    inject_probes.py vanilla --codex-home <trial>/agent-logs/codex-home \
        --codex-bin /opt/homebrew/bin/codex \
        --probes probes/61810_cow.json --out probe_answers.json

Probes file schema (see probe_authoring.md):
    {"task_id": "...", "probes": [{"id": "p1", "fact_class": "...",
      "question": "...", "ground_truth": "..." (optional),
      "ground_truth_pattern": "..." (optional regex)}]}
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rollout_lib  # noqa: E402

PROBE_TEMPLATE = (
    "Recall check (do not run commands, do not edit files, do not re-derive "
    "the answer from the workspace): {question}\n"
    "Answer from your own memory of this session. If you are not certain, "
    "say exactly what you do remember and state clearly that you do not know "
    "the rest."
)

SESSION_ID_RE = re.compile(r"^Session ID: (\S+)", re.MULTILINE)


def load_probes(path: Path) -> dict:
    data = json.loads(path.read_text())
    if "probes" not in data or not isinstance(data["probes"], list):
        raise ValueError(f"{path}: expected an object with a 'probes' list")
    return data


def wrap(question: str) -> str:
    return PROBE_TEMPLATE.format(question=question)


# --------------------------------------------------------------------------
# managed mode
# --------------------------------------------------------------------------


def parse_intendant_session_id(console_log: Path) -> str | None:
    try:
        match = SESSION_ID_RE.search(console_log.read_text(errors="replace"))
    except OSError:
        return None
    return match.group(1) if match else None


def ws_send_follow_up(gateway: str, session_id: str | None, text: str) -> None:
    try:
        import websockets  # type: ignore
    except ImportError as exc:  # pragma: no cover
        raise SystemExit(
            "managed mode needs the 'websockets' package "
            "(pip install websockets into the bench venv)"
        ) from exc

    import asyncio

    async def _send() -> None:
        async with websockets.connect(gateway, open_timeout=30) as ws:
            payload: dict = {"action": "follow_up", "text": text, "direct": True}
            if session_id:
                payload["session_id"] = session_id
            await ws.send(json.dumps(payload))
            # Give the gateway a beat to consume the intent before closing,
            # mirroring the fission-smoke skill's scripted client.
            await asyncio.sleep(2)

    asyncio.run(_send())


def pick_parent_rollout(codex_home: Path) -> Path:
    parents = rollout_lib.parent_rollouts(codex_home)
    completed = [p for p in parents if rollout_lib.task_completes(p)]
    pool = completed or parents
    if not pool:
        raise SystemExit(f"No parent rollout found under {codex_home}/sessions")
    return pool[0]


def wait_for_answer(
    rollout: Path, after_line: int, timeout_s: float, poll_s: float = 2.0
) -> rollout_lib.TaskComplete | None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        completes = rollout_lib.task_completes(rollout, after_line=after_line)
        if completes:
            return completes[0]
        time.sleep(poll_s)
    return None


def run_managed(args: argparse.Namespace) -> list[dict]:
    codex_home = Path(args.codex_home)
    session_id = args.session_id
    if session_id is None and args.console_log:
        session_id = parse_intendant_session_id(Path(args.console_log))
    if session_id is None:
        print(
            "warning: no intendant session id (banner not found); sending "
            "follow_up without session_id (single-session instances accept it)",
            file=sys.stderr,
        )

    rollout = pick_parent_rollout(codex_home)
    print(f"parent rollout: {rollout}", file=sys.stderr)

    results = []
    data = load_probes(Path(args.probes))
    for probe in data["probes"]:
        baseline = rollout_lib.line_count(rollout)
        text = wrap(probe["question"])
        started = time.monotonic()
        ws_send_follow_up(args.gateway, session_id, text)
        answer = wait_for_answer(rollout, baseline, args.turn_timeout)
        latency = time.monotonic() - started
        if answer is None:
            results.append(
                {
                    "id": probe["id"],
                    "question": probe["question"],
                    "answer": None,
                    "error": f"no task_complete within {args.turn_timeout}s",
                    "latency_s": round(latency, 1),
                }
            )
            print(f"probe {probe['id']}: TIMED OUT", file=sys.stderr)
            continue
        stats = rollout_lib.probe_turn_stats(
            rollout, after_line=baseline, before_line=answer.lineno
        )
        results.append(
            {
                "id": probe["id"],
                "question": probe["question"],
                "answer": answer.last_agent_message,
                "latency_s": round(latency, 1),
                "rollout_lines": [baseline + 1, answer.lineno],
                "recovery_tool_calls": stats.recovery_tool_calls,
                "other_tool_calls": stats.other_tool_calls,
                "tainted": stats.tainted,
            }
        )
        print(
            f"probe {probe['id']}: answered in {latency:.0f}s "
            f"(recovery tools: {stats.recovery_tool_calls or 'none'})",
            file=sys.stderr,
        )
    return results


# --------------------------------------------------------------------------
# vanilla mode
# --------------------------------------------------------------------------


def run_vanilla(args: argparse.Namespace) -> list[dict]:
    codex_home = Path(args.codex_home)
    rollout = pick_parent_rollout(codex_home)
    session_id = args.session_id or rollout_lib.session_id_of(rollout)
    if not session_id:
        raise SystemExit(f"Could not determine session id from {rollout}")
    print(f"resuming session {session_id} ({rollout})", file=sys.stderr)

    env = dict(os.environ)
    env["CODEX_HOME"] = str(codex_home)
    env.setdefault("NO_COLOR", "1")

    results = []
    data = load_probes(Path(args.probes))
    out_dir = Path(args.out).resolve().parent
    out_dir.mkdir(parents=True, exist_ok=True)
    for probe in data["probes"]:
        baseline = rollout_lib.line_count(rollout)
        text = wrap(probe["question"])
        answer_file = out_dir / f".probe-{probe['id']}-last-message.txt"
        answer_file.unlink(missing_ok=True)
        cmd = [
            args.codex_bin,
            "exec",
            "resume",
            "--skip-git-repo-check",
            "-c",
            'sandbox_mode="read-only"',
            "-o",
            str(answer_file),
            session_id,
            text,
        ]
        started = time.monotonic()
        proc = subprocess.run(
            cmd,
            env=env,
            capture_output=True,
            text=True,
            timeout=args.turn_timeout,
            cwd=str(codex_home),
        )
        latency = time.monotonic() - started
        answer = None
        if answer_file.is_file():
            answer = answer_file.read_text(errors="replace").strip() or None
            answer_file.unlink(missing_ok=True)
        # Resume may append to the original rollout or open a fresh rollout
        # file for the same thread, depending on codex revision — check both.
        completes = rollout_lib.task_completes(rollout, after_line=baseline)
        turn_rollout, turn_after = rollout, baseline
        if not completes:
            for candidate in rollout_lib.find_rollouts(codex_home):
                if candidate == rollout:
                    continue
                if rollout_lib.session_id_of(candidate) == session_id:
                    # A fresh resume file replays inherited history; only
                    # count the turn from this probe's user message onward.
                    probe_line = 0
                    for msg in rollout_lib.messages(candidate):
                        if msg.role == "user" and probe["question"] in msg.text:
                            probe_line = msg.lineno
                    completes = rollout_lib.task_completes(
                        candidate, after_line=probe_line
                    )
                    turn_rollout, turn_after = candidate, probe_line
                    break
        if answer is None and completes:
            answer = completes[-1].last_agent_message
        entry = {
            "id": probe["id"],
            "question": probe["question"],
            "answer": answer,
            "latency_s": round(latency, 1),
            "exit_code": proc.returncode,
        }
        if completes:
            stats = rollout_lib.probe_turn_stats(
                turn_rollout, after_line=turn_after, before_line=completes[-1].lineno
            )
            entry["rollout_lines"] = [turn_after + 1, completes[-1].lineno]
            entry["other_tool_calls"] = stats.other_tool_calls
            entry["tainted"] = stats.tainted
        if proc.returncode != 0:
            entry["error"] = proc.stderr[-2000:]
            print(
                f"probe {probe['id']}: codex exited {proc.returncode}",
                file=sys.stderr,
            )
        else:
            print(f"probe {probe['id']}: answered in {latency:.0f}s", file=sys.stderr)
        results.append(entry)
    return results


# --------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)

    managed = sub.add_parser("managed", help="probe a live Intendant session")
    managed.add_argument("--gateway", required=True, help="ws://host:port/ws")
    managed.add_argument("--codex-home", required=True)
    managed.add_argument("--console-log", help="intendant console log (Session ID banner)")
    managed.add_argument("--session-id", help="intendant session id (overrides banner)")

    vanilla = sub.add_parser("vanilla", help="probe an archived codex session")
    vanilla.add_argument("--codex-home", required=True)
    vanilla.add_argument("--codex-bin", default="codex")
    vanilla.add_argument("--session-id", help="codex thread id (default: newest rollout)")

    for p in (managed, vanilla):
        p.add_argument("--probes", required=True, help="probes JSON (probe_authoring.md)")
        p.add_argument("--out", required=True, help="answers JSON output path")
        p.add_argument(
            "--turn-timeout",
            type=float,
            default=300.0,
            help="seconds to wait for each probe's answer",
        )

    args = parser.parse_args()
    results = run_managed(args) if args.mode == "managed" else run_vanilla(args)

    data = load_probes(Path(args.probes))
    out = {
        "task_id": data.get("task_id"),
        "mode": args.mode,
        "answers": results,
    }
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(out, indent=2) + "\n")
    print(f"wrote {out_path}", file=sys.stderr)

    if any(r.get("answer") is None for r in results):
        sys.exit(3)


if __name__ == "__main__":
    main()
