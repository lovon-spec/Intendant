#!/usr/bin/env python3
"""Independent oracle for service-triplet op semantics + metrics aggregation +
JSON comparison.

A second implementation of worker/SPEC.md and the metrics/SPEC.md aggregates
(the agent-facing reference/ solutions are a third). Defines truth; never runs
the agent's code. Pure stdlib.
"""


def _is_number(x):
    return isinstance(x, (int, float)) and not isinstance(x, bool)


def _num_list(x):
    return isinstance(x, list) and all(_is_number(e) for e in x)


def _str_list(x):
    return isinstance(x, list) and all(isinstance(e, str) for e in x)


def compute(op, value):
    """Return (status, result). Mirrors worker/SPEC.md exactly."""
    if op == "sum":
        if _num_list(value):
            return "done", sum(value)
        return "error", None
    if op == "max":
        if _num_list(value) and len(value) > 0:
            return "done", max(value)
        return "error", None
    if op == "min":
        if _num_list(value) and len(value) > 0:
            return "done", min(value)
        return "error", None
    if op == "mean":
        if _num_list(value) and len(value) > 0:
            return "done", sum(value) / len(value)
        return "error", None
    if op == "median":
        if _num_list(value) and len(value) > 0:
            s = sorted(value)
            n = len(s)
            if n % 2 == 1:
                return "done", s[(n - 1) // 2]
            return "done", (s[n // 2 - 1] + s[n // 2]) / 2.0
        return "error", None
    if op == "sort_desc":
        if _num_list(value):
            return "done", sorted(value, reverse=True)
        return "error", None
    if op == "dedupe":
        if _num_list(value) or _str_list(value):
            seen = set()
            out = []
            for e in value:
                if e not in seen:  # exact numeric equality: 1 == 1.0 dedupes
                    seen.add(e)
                    out.append(e)
            return "done", out
        return "error", None
    if op == "reverse":
        if isinstance(value, str):
            return "done", value[::-1]
        return "error", None
    if op == "wordcount":
        if isinstance(value, str):
            return "done", len(value.split())
        return "error", None
    if op == "uppercase":
        if isinstance(value, str):
            return "done", value.upper()
        return "error", None
    if op == "histogram":
        if isinstance(value, str):
            counts = {}
            for tok in value.split():
                counts[tok] = counts.get(tok, 0) + 1
            return "done", counts
        return "error", None
    if op == "clamp":
        if (isinstance(value, dict) and set(value) == {"values", "min", "max"}
                and _num_list(value["values"]) and _is_number(value["min"])
                and _is_number(value["max"]) and value["min"] <= value["max"]):
            lo, hi = value["min"], value["max"]
            return "done", [min(max(e, lo), hi) for e in value["values"]]
        return "error", None
    if op == "rotate":
        if (isinstance(value, dict) and set(value) == {"values", "by"}
                and isinstance(value["values"], list) and _is_number(value["by"])
                and float(value["by"]).is_integer()):
            vals = value["values"]
            n = len(vals)
            if n == 0:
                return "done", []
            k = int(value["by"]) % n
            return "done", vals[n - k:] + vals[:n - k]
        return "error", None
    return "error", None


# ----------------------------------------------------- metrics aggregation
def metrics_summary(jobs):
    """Expected GET /summary body for a job list. Mirrors metrics/SPEC.md."""
    by_status, by_op = {}, {}
    done = 0
    nums = []
    for j in jobs:
        by_status[j["status"]] = by_status.get(j["status"], 0) + 1
        by_op[j["op"]] = by_op.get(j["op"], 0) + 1
        if j["status"] == "done":
            done += 1
            if _is_number(j.get("result")):
                nums.append(j["result"])
    total = len(jobs)
    return {
        "total": total,
        "by_status": by_status,
        "by_op": by_op,
        "done_ratio": (done / total) if total else None,
        "numeric_result_stats": (
            {"count": len(nums), "sum": sum(nums), "min": min(nums), "max": max(nums)}
            if nums else None),
    }


def metrics_op_summary(jobs, op):
    """Expected GET /ops/{op} body. Mirrors metrics/SPEC.md."""
    by_status = {}
    total = 0
    for j in jobs:
        if j["op"] == op:
            total += 1
            by_status[j["status"]] = by_status.get(j["status"], 0) + 1
    return {"op": op, "total": total, "by_status": by_status}


# --------------------------------------------------------- comparison helpers
def _num_close(a, b, tol=1e-9):
    return _is_number(a) and _is_number(b) and abs(a - b) <= tol + 1e-9 * max(abs(a), abs(b))


def json_equal(a, b, tol=1e-9):
    if a is None or b is None:
        return a is None and b is None
    if _num_close(a, b, tol):
        return True
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    if isinstance(a, dict) and isinstance(b, dict):
        return set(a) == set(b) and all(json_equal(a[k], b[k], tol) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(json_equal(x, y, tol) for x, y in zip(a, b))
    return a == b
