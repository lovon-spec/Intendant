#!/usr/bin/env python3
"""Starter test for worker compute (see worker/SPEC.md)."""
import json
import subprocess
import sys
from pathlib import Path

WORKER = Path(__file__).resolve().parents[1] / "worker.py"


def compute(op, input_value, via_stdin=False):
    if via_stdin:
        argv = [sys.executable, str(WORKER), "compute", op, "-"]
        p = subprocess.run(argv, input=json.dumps(input_value),
                           capture_output=True, text=True, timeout=30)
    else:
        argv = [sys.executable, str(WORKER), "compute", op, json.dumps(input_value)]
        p = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    assert p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr.strip())
    return json.loads(p.stdout)


CASES = [
    ("sum", [1, 2, 3], 6),
    ("sum", [], 0),
    ("max", [4, 9, 2], 9),
    ("min", [4, 9, 2], 2),
    ("mean", [1, 2, 3, 4], 2.5),
    ("median", [3, 1, 2], 2),
    ("median", [4, 1, 3, 2], 2.5),
    ("sort_desc", [3, 1, 2], [3, 2, 1]),
    ("dedupe", [3, 1, 3.0, 2, 1], [3, 1, 2]),
    ("dedupe", ["b", "a", "b"], ["b", "a"]),
    ("dedupe", [], []),
    ("reverse", "abc", "cba"),
    ("wordcount", "  a  b c ", 3),
    ("uppercase", "aBc", "ABC"),
    ("histogram", "a b a", {"a": 2, "b": 1}),
    ("histogram", "   ", {}),
    ("clamp", {"values": [-5, 7, 2], "min": 0, "max": 5}, [0, 5, 2]),
    ("rotate", {"values": [1, 2, 3, 4], "by": 1}, [4, 1, 2, 3]),
    ("rotate", {"values": [1, 2, 3, 4], "by": -1}, [2, 3, 4, 1]),
    ("rotate", {"values": [1, 2, 3, 4], "by": 6}, [3, 4, 1, 2]),
    ("rotate", {"values": [], "by": 5}, []),
    ("rotate", {"values": ["x", True, None], "by": 4.0}, [None, "x", True]),
]

ERROR_CASES = [
    ("max", []), ("min", []), ("mean", []), ("median", []),
    ("sum", "nope"), ("sum", [1, True, 3]), ("sort_desc", [1, "two"]),
    ("dedupe", [1, "a"]), ("dedupe", [True]), ("reverse", 5),
    ("wordcount", [1, 2]), ("uppercase", 99), ("histogram", 7),
    ("clamp", {"values": [1], "min": 5, "max": 0}),
    ("clamp", {"values": [1], "min": 0}),
    ("clamp", {"values": [1], "min": 0, "max": 5, "extra": 1}),
    ("clamp", {"values": [1, True], "min": 0, "max": 5}),
    ("rotate", {"values": [1], "by": 2.5}),
    ("rotate", {"values": [1], "by": True}),
    ("rotate", {"values": "xs", "by": 1}),
    ("frobnicate", 1),
]


def close(a, b, tol=1e-9):
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) \
            and not isinstance(a, bool) and not isinstance(b, bool):
        return abs(a - b) <= tol
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(close(x, y, tol) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return set(a) == set(b) and all(close(a[k], b[k], tol) for k in a)
    return a == b


def main():
    for op, inp, want in CASES:
        got = compute(op, inp)
        assert got.get("status") == "done" and close(got.get("result"), want), \
            "compute(%r, %r) = %r, want done/%r" % (op, inp, got, want)
    # the '-' sentinel reads the input JSON from stdin (for large inputs)
    got = compute("sum", [1, 2, 3, 4], via_stdin=True)
    assert got.get("status") == "done" and close(got.get("result"), 10), got

    # error paths: status must be "error"; result is not checked.
    for op, inp in ERROR_CASES:
        got = compute(op, inp)
        assert got.get("status") == "error", \
            "compute(%r, %r) = %r, want status error" % (op, inp, got)
    print("worker starter test: OK")


if __name__ == "__main__":
    main()
