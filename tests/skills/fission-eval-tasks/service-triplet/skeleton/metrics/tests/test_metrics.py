#!/usr/bin/env python3
"""Starter test for metrics/metrics.py (see metrics/SPEC.md). Runs the metrics
service against a tiny in-process stub API, so it is tested independently."""
import json
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

METRICS = Path(__file__).resolve().parents[1] / "metrics.py"

JOBS = [
    {"id": "j1", "op": "sum", "input": [1, 2, 3], "status": "done", "result": 6, "attempts": 1},
    {"id": "j2", "op": "max", "input": [], "status": "error", "result": None, "attempts": 1},
    {"id": "j3", "op": "reverse", "input": "ab", "status": "queued", "result": None, "attempts": 0},
    {"id": "j4", "op": "sum", "input": [4.5], "status": "done", "result": 4.5, "attempts": 1},
    {"id": "j5", "op": "uppercase", "input": "ab", "status": "done", "result": "AB", "attempts": 1},
    {"id": "j6", "op": "sum", "input": [1], "status": "running", "result": None, "attempts": 1},
]


class StubApi(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == "/healthz":
            body = json.dumps({"ok": True}).encode()
        elif path == "/jobs":
            body = json.dumps({"jobs": JOBS}).encode()
        else:
            body = json.dumps({"error": "not found"}).encode()
            self.send_response(404)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def get(url, timeout=5):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"null")
        except json.JSONDecodeError:
            return e.code, None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_health(base):
    for _ in range(60):
        try:
            if get(base + "/healthz")[0] == 200:
                return True
        except Exception:
            pass
        time.sleep(0.1)
    return False


def main():
    srv = ThreadingHTTPServer(("127.0.0.1", 0), StubApi)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    api = "http://127.0.0.1:%d" % srv.server_address[1]
    port = free_port()
    proc = subprocess.Popen([sys.executable, str(METRICS), "--port", str(port),
                             "--api", api],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    try:
        assert wait_health(base), "metrics never became healthy"

        code, s = get(base + "/summary")
        assert code == 200, (code, s)
        assert s["total"] == 6, s
        assert s["by_status"] == {"done": 3, "error": 1, "queued": 1, "running": 1}, s
        assert s["by_op"] == {"sum": 3, "max": 1, "reverse": 1, "uppercase": 1}, s
        assert abs(s["done_ratio"] - 0.5) < 1e-9, s
        nrs = s["numeric_result_stats"]
        assert nrs["count"] == 2 and abs(nrs["sum"] - 10.5) < 1e-9, nrs
        assert nrs["min"] == 4.5 and nrs["max"] == 6, nrs

        code, o = get(base + "/ops/sum")
        assert code == 200 and o == {"op": "sum", "total": 3,
                                     "by_status": {"done": 2, "running": 1}}, o
        code, o = get(base + "/ops/zzz")
        assert code == 200 and o == {"op": "zzz", "total": 0, "by_status": {}}, o

        # freshness: metrics must re-fetch on demand
        JOBS.append({"id": "j7", "op": "sum", "input": [], "status": "queued",
                     "result": None, "attempts": 0})
        code, s = get(base + "/summary")
        assert code == 200 and s["total"] == 7, s

        code, _ = get(base + "/nope")
        assert code == 404, code
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        srv.shutdown()

    # API unreachable -> 503 from /summary, /healthz still 200.
    dead_api = "http://127.0.0.1:%d" % free_port()
    port = free_port()
    proc = subprocess.Popen([sys.executable, str(METRICS), "--port", str(port),
                             "--api", dead_api],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % port
    try:
        assert wait_health(base), "metrics /healthz must not depend on the API"
        code, body = get(base + "/summary", timeout=10)
        assert code == 503 and isinstance(body, dict) and "error" in body, (code, body)
        code, body = get(base + "/ops/sum", timeout=10)
        assert code == 503, (code, body)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
    print("metrics starter test: OK")


if __name__ == "__main__":
    main()
