#!/usr/bin/env bash
# Starter test for api/server.py (see api/SPEC.md). Starts the server on a free
# port, drives it over HTTP, checks the job lifecycle, requeue/delete, and the
# listing (creation order, filters, pagination).
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SERVER="$(dirname "$HERE")/server.py"

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
python3 "$SERVER" --port "$PORT" --host 127.0.0.1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT
BASE="http://127.0.0.1:$PORT"

python3 - "$BASE" <<'PY'
import json, sys, time, urllib.error, urllib.request

base = sys.argv[1]

def req(method, path, body=None, expect=None, raw=None):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    r = urllib.request.Request(base + path, data=data, method=method,
                               headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=5) as resp:
            code, payload = resp.status, resp.read()
    except urllib.error.HTTPError as e:
        code, payload = e.code, e.read()
    if expect is not None:
        assert code == expect, "%s %s -> %s (want %s): %s" % (method, path, code, expect, payload[:200])
    try:
        return code, json.loads(payload) if payload else None
    except json.JSONDecodeError:
        return code, None

# wait for startup
for _ in range(50):
    try:
        if req("GET", "/healthz")[0] == 200:
            break
    except Exception:
        pass
    time.sleep(0.1)
else:
    raise SystemExit("api never became healthy")

# create
_, job = req("POST", "/jobs", {"op": "sum", "input": [1, 2, 3]}, expect=201)
assert job["status"] == "queued" and job["result"] is None and job["op"] == "sum", job
assert job["attempts"] == 0, job
jid = job["id"]
assert isinstance(jid, str) and jid, job

# input may be null, but the key must be present; op must be a non-empty string
req("POST", "/jobs", {"op": "noop", "input": None}, expect=201)
req("POST", "/jobs", {"op": "sum"}, expect=400)
req("POST", "/jobs", {"op": "", "input": 1}, expect=400)
req("POST", "/jobs", {"op": 7, "input": 1}, expect=400)
req("POST", "/jobs", raw=b"not json{", expect=400)

# get
_, got = req("GET", "/jobs/%s" % jid, expect=200)
assert got["input"] == [1, 2, 3], got
req("GET", "/jobs/does-not-exist", expect=404)

# claim once -> running, attempts 1; claim again -> 409
_, claimed = req("POST", "/jobs/%s/claim" % jid, expect=200)
assert claimed["status"] == "running" and claimed["attempts"] == 1, claimed
req("POST", "/jobs/%s/claim" % jid, expect=409)
req("POST", "/jobs/nope/claim", expect=404)

# delete refuses non-terminal jobs
req("DELETE", "/jobs/%s" % jid, expect=409)

# result
_, done = req("POST", "/jobs/%s/result" % jid, {"status": "done", "result": 6}, expect=200)
assert done["status"] == "done" and done["result"] == 6, done
req("POST", "/jobs/nope/result", {"status": "done", "result": 1}, expect=404)
req("POST", "/jobs/%s/result" % jid, {"status": "bogus", "result": 1}, expect=400)

# requeue: only error jobs; attempts preserved, result reset
req("POST", "/jobs/%s/requeue" % jid, expect=409)        # done -> 409
_, ejob = req("POST", "/jobs", {"op": "max", "input": []}, expect=201)
eid = ejob["id"]
req("POST", "/jobs/%s/claim" % eid, expect=200)
req("POST", "/jobs/%s/result" % eid, {"status": "error", "result": None}, expect=200)
_, requeued = req("POST", "/jobs/%s/requeue" % eid, expect=200)
assert requeued["status"] == "queued" and requeued["result"] is None, requeued
assert requeued["attempts"] == 1, requeued
_, c2 = req("POST", "/jobs/%s/claim" % eid, expect=200)
assert c2["attempts"] == 2, c2
req("POST", "/jobs/nope/requeue", expect=404)

# delete a terminal job; it is then gone
req("POST", "/jobs/%s/result" % eid, {"status": "error", "result": None}, expect=200)
_, deleted = req("DELETE", "/jobs/%s" % eid, expect=200)
assert deleted == {"deleted": True, "id": eid}, deleted
req("GET", "/jobs/%s" % eid, expect=404)
req("DELETE", "/jobs/%s" % eid, expect=404)

# listing: creation order, filters, pagination
ids = []
for i in range(5):
    _, j = req("POST", "/jobs", {"op": "batch", "input": i}, expect=201)
    ids.append(j["id"])
_, lst = req("GET", "/jobs?op=batch", expect=200)
assert [j["id"] for j in lst["jobs"]] == ids, (ids, lst)
_, page = req("GET", "/jobs?op=batch&offset=1&limit=2", expect=200)
assert [j["id"] for j in page["jobs"]] == ids[1:3], page
_, zero = req("GET", "/jobs?op=batch&limit=0", expect=200)
assert zero["jobs"] == [], zero
_, beyond = req("GET", "/jobs?op=batch&offset=99", expect=200)
assert beyond["jobs"] == [], beyond
req("GET", "/jobs?limit=-1", expect=400)
req("GET", "/jobs?offset=abc", expect=400)
_, q = req("GET", "/jobs?status=queued&op=batch", expect=200)
assert len(q["jobs"]) == 5, q
_, dl = req("GET", "/jobs?status=done", expect=200)
assert any(j["id"] == jid for j in dl["jobs"]), dl
print("api starter test: OK")
PY
