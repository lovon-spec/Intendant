#!/usr/bin/env python3
"""Reference metrics service (agent-facing solution). See metrics/SPEC.md.
Excluded from agent visibility by the SKILL runner."""
import argparse
import json
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def _is_number(x):
    return isinstance(x, (int, float)) and not isinstance(x, bool)


def fetch_jobs(api_base):
    """Return the current job list, or None on any API failure."""
    try:
        with urllib.request.urlopen(api_base + "/jobs", timeout=5) as resp:
            if resp.status != 200:
                return None
            body = json.loads(resp.read())
    except (urllib.error.URLError, urllib.error.HTTPError, OSError,
            json.JSONDecodeError, ValueError):
        return None
    if not isinstance(body, dict) or not isinstance(body.get("jobs"), list):
        return None
    return body["jobs"]


def summarize(jobs):
    by_status, by_op = {}, {}
    done = 0
    nums = []
    for j in jobs:
        st, op = j.get("status"), j.get("op")
        by_status[st] = by_status.get(st, 0) + 1
        by_op[op] = by_op.get(op, 0) + 1
        if st == "done":
            done += 1
            if _is_number(j.get("result")):
                nums.append(j["result"])
    total = len(jobs)
    stats = None
    if nums:
        stats = {"count": len(nums), "sum": sum(nums),
                 "min": min(nums), "max": max(nums)}
    return {
        "total": total,
        "by_status": by_status,
        "by_op": by_op,
        "done_ratio": (done / total) if total else None,
        "numeric_result_stats": stats,
    }


def op_summary(jobs, op):
    mine = [j for j in jobs if j.get("op") == op]
    by_status = {}
    for j in mine:
        st = j.get("status")
        by_status[st] = by_status.get(st, 0) + 1
    return {"op": op, "total": len(mine), "by_status": by_status}


def make_handler(api_base):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def _send(self, code, obj):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            path = self.path.split("?", 1)[0]
            if path == "/healthz":
                return self._send(200, {"ok": True})
            if path == "/summary":
                jobs = fetch_jobs(api_base)
                if jobs is None:
                    return self._send(503, {"error": "api unreachable"})
                return self._send(200, summarize(jobs))
            if path.startswith("/ops/"):
                op = path[len("/ops/"):]
                jobs = fetch_jobs(api_base)
                if jobs is None:
                    return self._send(503, {"error": "api unreachable"})
                return self._send(200, op_summary(jobs, op))
            return self._send(404, {"error": "not found"})

    return Handler


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--api", required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv[1:])
    srv = ThreadingHTTPServer((args.host, args.port),
                              make_handler(args.api.rstrip("/")))
    print("jobline metrics on %s:%d (api %s)" % (args.host, args.port, args.api),
          file=sys.stderr)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
