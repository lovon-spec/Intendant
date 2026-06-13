#!/usr/bin/env python3
"""Reference REST job store (agent-facing solution). See api/SPEC.md.
Excluded from agent visibility by the SKILL runner."""
import argparse
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

JOBS = {}           # id -> job (dict preserves creation order; deletes drop out)
LOCK = threading.Lock()
COUNTER = [0]
_UINT = re.compile(r"^[0-9]+$")


def new_job(op, value):
    with LOCK:
        COUNTER[0] += 1
        jid = "j%d" % COUNTER[0]
        job = {"id": jid, "op": op, "input": value, "status": "queued",
               "result": None, "attempts": 0}
        JOBS[jid] = job
        return dict(job)


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

    def _body(self):
        n = int(self.headers.get("Content-Length", 0) or 0)
        raw = self.rfile.read(n) if n else b""
        return json.loads(raw) if raw else None

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/healthz":
            return self._send(200, {"ok": True})
        if u.path == "/jobs":
            q = parse_qs(u.query)
            status = q.get("status", [None])[0]
            op = q.get("op", [None])[0]
            offset_raw = q.get("offset", [None])[0]
            limit_raw = q.get("limit", [None])[0]
            for raw in (offset_raw, limit_raw):
                if raw is not None and not _UINT.match(raw):
                    return self._send(400, {"error": "limit/offset must be non-negative integers"})
            offset = int(offset_raw) if offset_raw is not None else 0
            limit = int(limit_raw) if limit_raw is not None else None
            with LOCK:
                jobs = [dict(j) for j in JOBS.values()
                        if (status is None or j["status"] == status)
                        and (op is None or j["op"] == op)]
            jobs = jobs[offset:]
            if limit is not None:
                jobs = jobs[:limit]
            return self._send(200, {"jobs": jobs})
        if u.path.startswith("/jobs/"):
            jid = u.path[len("/jobs/"):]
            with LOCK:
                job = JOBS.get(jid)
                snap = dict(job) if job else None
            return self._send(200, snap) if snap else self._send(404, {"error": "unknown job"})
        return self._send(404, {"error": "not found"})

    def do_POST(self):
        u = urlparse(self.path)
        path = u.path
        if path == "/jobs":
            try:
                body = self._body()
            except json.JSONDecodeError:
                return self._send(400, {"error": "invalid json"})
            if not isinstance(body, dict) or not isinstance(body.get("op"), str) \
                    or body["op"] == "" or "input" not in body:
                return self._send(400, {"error": "op (non-empty string) and input are required"})
            return self._send(201, new_job(body["op"], body["input"]))
        if path.startswith("/jobs/") and path.endswith("/claim"):
            jid = path[len("/jobs/"):-len("/claim")]
            with LOCK:
                job = JOBS.get(jid)
                if job is None:
                    return self._send(404, {"error": "unknown job"})
                if job["status"] != "queued":
                    return self._send(409, {"error": "not queued"})
                job["status"] = "running"
                job["attempts"] += 1
                snap = dict(job)
            return self._send(200, snap)
        if path.startswith("/jobs/") and path.endswith("/result"):
            jid = path[len("/jobs/"):-len("/result")]
            try:
                body = self._body()
            except json.JSONDecodeError:
                return self._send(400, {"error": "invalid json"})
            if not isinstance(body, dict) or body.get("status") not in ("done", "error"):
                return self._send(400, {"error": "status must be done or error"})
            with LOCK:
                job = JOBS.get(jid)
                if job is None:
                    return self._send(404, {"error": "unknown job"})
                job["status"] = body["status"]
                job["result"] = body.get("result")
                snap = dict(job)
            return self._send(200, snap)
        if path.startswith("/jobs/") and path.endswith("/requeue"):
            jid = path[len("/jobs/"):-len("/requeue")]
            with LOCK:
                job = JOBS.get(jid)
                if job is None:
                    return self._send(404, {"error": "unknown job"})
                if job["status"] != "error":
                    return self._send(409, {"error": "only error jobs can be requeued"})
                job["status"] = "queued"
                job["result"] = None
                snap = dict(job)
            return self._send(200, snap)
        return self._send(404, {"error": "not found"})

    def do_DELETE(self):
        u = urlparse(self.path)
        if u.path.startswith("/jobs/"):
            jid = u.path[len("/jobs/"):]
            with LOCK:
                job = JOBS.get(jid)
                if job is None:
                    return self._send(404, {"error": "unknown job"})
                if job["status"] not in ("done", "error"):
                    return self._send(409, {"error": "only terminal jobs are deletable"})
                del JOBS[jid]
            return self._send(200, {"deleted": True, "id": jid})
        return self._send(404, {"error": "not found"})


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv[1:])
    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    print("jobline api on %s:%d" % (args.host, args.port), file=sys.stderr)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
