#!/usr/bin/env python3
"""Starter test for cli/client.py (see cli/SPEC.md). Runs the CLI against a
tiny in-process stub API, so the CLI is tested independently of api/."""
import json
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CLIENT = Path(__file__).resolve().parents[1] / "client.py"
JOBS = {}
_counter = [0]


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

    def do_POST(self):
        if self.path == "/jobs":
            n = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(n) or b"{}")
            _counter[0] += 1
            jid = "job-%d" % _counter[0]
            job = {"id": jid, "op": body.get("op"), "input": body.get("input"),
                   "status": "queued", "result": None, "attempts": 0}
            JOBS[jid] = job
            self._send(201, job)
        elif self.path.startswith("/jobs/") and self.path.endswith("/requeue"):
            jid = self.path[len("/jobs/"):-len("/requeue")]
            job = JOBS.get(jid)
            if job is None:
                self._send(404, {"error": "unknown"})
            elif job["status"] != "error":
                self._send(409, {"error": "not error"})
            else:
                job["status"] = "queued"
                job["result"] = None
                self._send(200, job)
        else:
            self._send(404, {"error": "not found"})

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.startswith("/jobs/"):
            jid = path[len("/jobs/"):]
            if jid in JOBS:
                self._send(200, JOBS[jid])
            else:
                self._send(404, {"error": "unknown"})
        else:
            self._send(404, {"error": "not found"})


def run_cli(*args):
    return subprocess.run([sys.executable, str(CLIENT), *args],
                          capture_output=True, text=True, timeout=30)


def main():
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % srv.server_address[1]
    try:
        # submit -> prints the new id; the stub should now hold that job.
        p = run_cli("submit", base, "sum", "[1, 2, 3]")
        assert p.returncode == 0, "submit exit %s: %s" % (p.returncode, p.stderr.strip())
        jid = p.stdout.strip()
        assert jid in JOBS and JOBS[jid]["op"] == "sum" and JOBS[jid]["input"] == [1, 2, 3], \
            "submit did not create the job correctly: %r / %r" % (jid, JOBS.get(jid))

        # get -> prints the job JSON
        p = run_cli("get", base, jid)
        assert p.returncode == 0, "get exit %s: %s" % (p.returncode, p.stderr.strip())
        got = json.loads(p.stdout)
        assert got["id"] == jid and got["op"] == "sum", got

        # get unknown -> non-zero
        p = run_cli("get", base, "nope")
        assert p.returncode != 0, "get on unknown id should fail"

        # wait on an already-done job -> prints it, exit 0
        JOBS["seeded"] = {"id": "seeded", "op": "sum", "input": [1],
                          "status": "done", "result": 1, "attempts": 1}
        p = run_cli("wait", base, "seeded", "--timeout", "5")
        assert p.returncode == 0, "wait exit %s: %s" % (p.returncode, p.stderr.strip())
        w = json.loads(p.stdout)
        assert w["status"] == "done" and w["result"] == 1, w

        # wait --quiet -> nothing on stdout, same exit code
        p = run_cli("wait", base, "seeded", "--timeout", "5", "--quiet")
        assert p.returncode == 0 and p.stdout == "", (p.returncode, p.stdout)

        # wait on an error job -> prints it, exits non-zero
        JOBS["bad"] = {"id": "bad", "op": "max", "input": [],
                       "status": "error", "result": None, "attempts": 1}
        p = run_cli("wait", base, "bad", "--timeout", "5")
        assert p.returncode != 0, "wait on error job must exit non-zero"
        assert json.loads(p.stdout)["status"] == "error", p.stdout

        # submit-batch: good lines submitted in order, bad lines reported but
        # skipped, exit non-zero because there were failures.
        with tempfile.TemporaryDirectory() as td:
            batch = Path(td) / "batch.jsonl"
            batch.write_text(
                '{"op": "sum", "input": [1]}\n'
                "\n"
                "not json at all\n"
                '{"op": "max", "input": [2, 5]}\n'
                '{"no_op_key": true}\n'
                '{"op": "reverse", "input": "xy"}\n',
                encoding="utf-8")
            before = _counter[0]
            p = run_cli("submit-batch", base, str(batch))
            ids = [ln.strip() for ln in p.stdout.splitlines() if ln.strip()]
            assert len(ids) == 3, "want 3 ids on stdout, got %r" % p.stdout
            assert p.returncode != 0, "batch with bad lines must exit non-zero"
            assert [JOBS[i]["op"] for i in ids] == ["sum", "max", "reverse"], \
                [JOBS.get(i) for i in ids]
            assert _counter[0] == before + 3, "exactly the 3 good lines submitted"

            allgood = Path(td) / "good.jsonl"
            allgood.write_text('{"op": "sum", "input": []}\n', encoding="utf-8")
            p = run_cli("submit-batch", base, str(allgood))
            assert p.returncode == 0, "all-good batch must exit 0: %s" % p.stderr.strip()
            assert len(p.stdout.split()) == 1

        # requeue an error job -> queued, printed, exit 0; non-error -> non-zero
        p = run_cli("requeue", base, "bad")
        assert p.returncode == 0, "requeue exit %s: %s" % (p.returncode, p.stderr.strip())
        rq = json.loads(p.stdout)
        assert rq["status"] == "queued" and rq["result"] is None, rq
        p = run_cli("requeue", base, "seeded")   # done job -> 409 -> non-zero
        assert p.returncode != 0, "requeue on a done job must exit non-zero"
        p = run_cli("requeue", base, "missing")
        assert p.returncode != 0, "requeue on unknown id must exit non-zero"
        print("cli starter test: OK")
    finally:
        srv.shutdown()


if __name__ == "__main__":
    main()
