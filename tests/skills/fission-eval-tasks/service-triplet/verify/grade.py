#!/usr/bin/env python3
"""Behavioral grader for service-triplet.

Five independent measures — worker (pure compute vs oracle, incl. perf
budgets), api (driven over raw HTTP, incl. requeue/delete/listing semantics
and a bulk perf check), cli (against a conforming reference server), metrics
(against the reference server, incl. freshness and API-down behavior), and a
live integration (agent api + worker + metrics, random ports, generated
payloads, driven through the agent's own cli). Scored against the independent
oracle. Emits the suite JSON contract on stdout: component_scores for
api/worker/cli/metrics (1.0 each) + integration (1.0) => max_total 5.0.
Behavior only; never inspects file/string presence.

Negative checks (specific 4xx/5xx/non-zero-exit outcomes) only count once the
battery's positive path passes, so a do-nothing stub collects nothing.

Usage: grade.py <workdir> [--seed N]
"""
import argparse
import json
import os
import random
import socket
import string
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle          # noqa: E402
from ref_api import RefApi  # noqa: E402

PYTHON = sys.executable
LIST_OPS_ANY = ["sum", "sort_desc", "dedupe"]
LIST_OPS_NONEMPTY = ["max", "min", "mean", "median"]
STRING_OPS = ["reverse", "wordcount", "uppercase", "histogram"]
OBJECT_OPS = ["clamp", "rotate"]
ALL_OPS = LIST_OPS_ANY + LIST_OPS_NONEMPTY + STRING_OPS + OBJECT_OPS
PERF_SORT = (200000, 10)       # elements, seconds
PERF_HISTOGRAM = (150000, 10)  # tokens (~1 MB), seconds
PERF_BULK_JOBS = (250, 30)     # jobs, seconds


# ------------------------------------------------------------- http client
def http(method, url, body=None, timeout=5, raw=None):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            payload = resp.read()
            code = resp.status
    except urllib.error.HTTPError as e:
        payload, code = e.read(), e.code
    except (urllib.error.URLError, OSError, ConnectionError):
        return None, None
    try:
        return code, (json.loads(payload) if payload else None)
    except json.JSONDecodeError:
        return code, None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_healthy(base, tries=60, delay=0.1):
    for _ in range(tries):
        code, body = http("GET", base + "/healthz")
        if code == 200 and isinstance(body, dict) and body.get("ok") is True:
            return True
        time.sleep(delay)
    return False


def start_api(workdir, port):
    return subprocess.Popen(
        [PYTHON, os.path.join(workdir, "api", "server.py"), "--port", str(port),
         "--host", "127.0.0.1"],
        cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def start_metrics(workdir, port, api_base):
    return subprocess.Popen(
        [PYTHON, os.path.join(workdir, "metrics", "metrics.py"), "--port", str(port),
         "--api", api_base],
        cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def stop(proc):
    if proc and proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


# ------------------------------------------------------------- generators
def rand_numbers(rng, allow_empty=True, n_max=6):
    lo = 0 if allow_empty else 1
    n = rng.randint(lo, n_max)
    out = []
    for _ in range(n):
        if rng.random() < 0.6:
            out.append(rng.randint(-50, 50))
        else:
            out.append(round(rng.uniform(-50, 50), 2))
    return out


def rand_string(rng):
    words = rng.randint(0, 6)
    pool = ["".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 5)))
            for _ in range(max(1, words // 2))]
    toks = [rng.choice(pool) for _ in range(words)]  # repeats exercise histogram
    sep = rng.choice([" ", "  ", " \t ", "   "])
    s = sep.join(toks)
    if rng.random() < 0.3:
        s = "  " + s + "  "
    return s


def gen_valid_job(rng):
    op = rng.choice(ALL_OPS)
    if op in LIST_OPS_ANY or op in LIST_OPS_NONEMPTY:
        value = rand_numbers(rng, allow_empty=(op in LIST_OPS_ANY))
        if op == "dedupe":
            if rng.random() < 0.4:
                value = [rng.choice(["a", "b", "c", "aa"]) for _ in range(rng.randint(0, 6))]
            elif value and rng.random() < 0.6:
                value = value + [float(rng.choice(value)), rng.choice(value)]
                rng.shuffle(value)
    elif op in STRING_OPS:
        value = rand_string(rng)
    elif op == "clamp":
        a, b = sorted([rng.randint(-20, 20), rng.randint(-20, 20)])
        value = {"values": rand_numbers(rng), "min": a, "max": b}
    else:  # rotate
        by = rng.choice([rng.randint(-9, 9), float(rng.randint(-4, 4)),
                         rng.randint(100, 10**6)])
        vals = [rng.choice([1, 2.5, "x", "y", None, True, [1], {"k": 1}])
                for _ in range(rng.randint(0, 6))]
        value = {"values": vals, "by": by}
    return op, value


def gen_invalid_job(rng):
    return rng.choice([
        ("sum", "not-a-list"),
        ("sum", [1, True, 3]),       # booleans are not numbers
        ("max", []),
        ("min", []),
        ("mean", []),
        ("median", []),
        ("sort_desc", [1, "two", 3]),
        ("dedupe", [1, "a"]),
        ("dedupe", [True, False]),
        ("dedupe", {"values": [1]}),
        ("reverse", 5),
        ("wordcount", [1, 2]),
        ("uppercase", 99),
        ("histogram", ["a", "b"]),
        ("clamp", {"values": [1], "min": 5, "max": 0}),
        ("clamp", {"values": [1], "min": 0}),
        ("clamp", {"values": [1], "min": 0, "max": 5, "extra": 1}),
        ("clamp", {"values": [1, True], "min": 0, "max": 5}),
        ("clamp", [1, 2, 3]),
        ("rotate", {"values": [1], "by": 2.5}),
        ("rotate", {"values": [1], "by": True}),
        ("rotate", {"values": "xs", "by": 1}),
        ("rotate", {"values": [1]}),
        ("rotate", 7),
        ("frobnicate", 1),
        ("", "x"),
    ])


def gen_jobs(rng, n, invalid_ratio=0.3):
    jobs = []
    for _ in range(n):
        jobs.append(gen_invalid_job(rng) if rng.random() < invalid_ratio else gen_valid_job(rng))
    return jobs


def gen_invalid_submittable(rng):
    """An invalid-compute job that the API will still accept (op non-empty):
    integration jobs flow through POST /jobs, and the v2 API correctly rejects
    an empty op with 400, so that case belongs to the api battery instead."""
    while True:
        op, value = gen_invalid_job(rng)
        if op:
            return op, value


# --------------------------------------------------------------- batteries
def run_compute(workdir, op, value, timeout=20, via_stdin=False):
    worker = os.path.join(workdir, "worker", "worker.py")
    try:
        if via_stdin:
            p = subprocess.run([PYTHON, worker, "compute", op, "-"],
                               input=json.dumps(value), cwd=workdir,
                               capture_output=True, text=True, timeout=timeout)
        else:
            p = subprocess.run([PYTHON, worker, "compute", op, json.dumps(value)],
                               cwd=workdir, capture_output=True, text=True,
                               timeout=timeout)
        return json.loads(p.stdout) if p.returncode == 0 else None
    except (subprocess.TimeoutExpired, OSError, json.JSONDecodeError):
        return None


def battery_worker(workdir, rng):
    cases = []
    for op, value in gen_jobs(rng, 44, invalid_ratio=0.4):
        cases.append((op, value, False))
    for _ in range(3):  # the '-' stdin form on ordinary inputs
        op, value = gen_valid_job(rng)
        cases.append((op, value, True))
    done_pass = error_pass = 0
    fails = []
    for op, value, via_stdin in cases:
        got = run_compute(workdir, op, value, via_stdin=via_stdin)
        want_status, want_result = oracle.compute(op, value)
        ok = False
        if isinstance(got, dict):
            if want_status == "error":
                ok = got.get("status") == "error"
            else:
                ok = got.get("status") == "done" and oracle.json_equal(got.get("result"), want_result)
        if ok:
            if want_status == "error":
                error_pass += 1
            else:
                done_pass += 1
        elif len(fails) < 4:
            fails.append({"op": op, "stdin": via_stdin, "input": value,
                          "want": [want_status, want_result], "got": got})
    # Must-error cases only count once at least one done-case computes
    # correctly: an always-"error" stub implements nothing and scores 0.
    passed = done_pass + (error_pass if done_pass > 0 else 0)

    perf_detail = {}
    # perf 1: sort_desc on 200k numbers (stdin form), 10 s budget
    n, budget = PERF_SORT
    big = [((i * 31) % 100000) - 50000 + (0.5 if i % 2 else 0) for i in range(n)]
    rng.shuffle(big)
    t0 = time.monotonic()
    got = run_compute(workdir, "sort_desc", big, timeout=budget, via_stdin=True)
    el1 = time.monotonic() - t0
    _, want = oracle.compute("sort_desc", big)
    ok1 = isinstance(got, dict) and got.get("status") == "done" \
        and oracle.json_equal(got.get("result"), want)
    perf_detail["sort_desc"] = {"n": n, "budget_s": budget,
                                "elapsed_s": round(el1, 2), "ok": bool(ok1)}
    if ok1:
        passed += 1
    elif len(fails) < 5:
        fails.append({"op": "sort_desc", "perf": perf_detail["sort_desc"]})
    # perf 2: histogram over ~1 MB of tokens (stdin form), 10 s budget
    n, budget = PERF_HISTOGRAM
    toks = ["w%d" % (i % 500) for i in range(n)]
    rng.shuffle(toks)
    text = " ".join(toks)
    t0 = time.monotonic()
    got = run_compute(workdir, "histogram", text, timeout=budget, via_stdin=True)
    el2 = time.monotonic() - t0
    _, want = oracle.compute("histogram", text)
    ok2 = isinstance(got, dict) and got.get("status") == "done" \
        and oracle.json_equal(got.get("result"), want)
    perf_detail["histogram"] = {"tokens": n, "budget_s": budget,
                                "elapsed_s": round(el2, 2), "ok": bool(ok2)}
    if ok2:
        passed += 1
    elif len(fails) < 6:
        fails.append({"op": "histogram", "perf": perf_detail["histogram"]})

    total = len(cases) + 2
    return passed, total, fails, perf_detail


def battery_api(workdir, rng):
    port = free_port()
    proc = start_api(workdir, port)
    base = "http://127.0.0.1:%d" % port
    checks = []          # (name, ok, negative)

    def add(name, cond, negative=False):
        checks.append((name, bool(cond), negative))

    perf_detail = {}
    try:
        healthy = wait_healthy(base)
        add("healthz", healthy)
        if not healthy:
            return 0, 32, [{"fatal": "api did not become healthy"}], perf_detail

        # create + round-trip on generated jobs (attempts must start at 0)
        ok_create = True
        ids = []
        for op, value in [gen_valid_job(rng) for _ in range(4)]:
            code, job = http("POST", base + "/jobs", {"op": op, "input": value})
            good = (code == 201 and isinstance(job, dict) and job.get("status") == "queued"
                    and job.get("result") is None and job.get("op") == op
                    and oracle.json_equal(job.get("input"), value)
                    and isinstance(job.get("id"), str) and job.get("id")
                    and job.get("attempts") == 0)
            ok_create = ok_create and good
            if good:
                ids.append((job["id"], op, value))
        add("create_queued", ok_create)

        code, job = http("POST", base + "/jobs", {"op": "noop", "input": None})
        add("create_input_null", code == 201 and isinstance(job, dict)
            and job.get("input") is None)

        ok_get = all(
            (lambda j: j is not None and oracle.json_equal(j.get("input"), value))(
                http("GET", "%s/jobs/%s" % (base, jid))[1])
            for jid, op, value in ids) and bool(ids)
        add("get_roundtrip", ok_get)

        add("post_missing_input", http("POST", base + "/jobs", {"op": "x"})[0] == 400,
            negative=True)
        add("post_empty_op", http("POST", base + "/jobs", {"op": "", "input": 1})[0] == 400,
            negative=True)
        add("post_op_nonstring", http("POST", base + "/jobs", {"op": 7, "input": 1})[0] == 400,
            negative=True)
        add("post_bad_json", http("POST", base + "/jobs", raw=b"not json{")[0] == 400,
            negative=True)
        add("get_unknown_404", http("GET", base + "/jobs/nope-xyz")[0] == 404,
            negative=True)

        # claim lifecycle on a fresh job
        code, job = http("POST", base + "/jobs", {"op": "sum", "input": [1, 2]})
        jid = job["id"] if isinstance(job, dict) else None
        c1, claimed = http("POST", "%s/jobs/%s/claim" % (base, jid))
        add("claim_running", c1 == 200 and isinstance(claimed, dict)
            and claimed.get("status") == "running" and claimed.get("attempts") == 1)
        c2, _ = http("POST", "%s/jobs/%s/claim" % (base, jid))
        add("claim_conflict_409", c2 == 409, negative=True)
        add("claim_unknown_404", http("POST", base + "/jobs/nope/claim")[0] == 404,
            negative=True)

        # status filters reflect lifecycle
        _, q_run = http("GET", base + "/jobs?status=running")
        add("filter_running", isinstance(q_run, dict)
            and any(j.get("id") == jid for j in q_run.get("jobs", [])))
        _, q_queued = http("GET", base + "/jobs?status=queued")
        add("filter_queued_excludes_running",
            isinstance(q_queued, dict)
            and all(j.get("id") != jid for j in q_queued.get("jobs", [])))

        # result
        rc, done = http("POST", "%s/jobs/%s/result" % (base, jid),
                        {"status": "done", "result": 3})
        add("result_done", rc == 200 and isinstance(done, dict)
            and done.get("status") == "done" and oracle.json_equal(done.get("result"), 3))
        add("result_unknown_404",
            http("POST", base + "/jobs/nope/result", {"status": "done", "result": 1})[0] == 404,
            negative=True)
        add("result_bad_status_400",
            http("POST", "%s/jobs/%s/result" % (base, jid),
                 {"status": "bogus", "result": 1})[0] == 400,
            negative=True)

        # requeue lifecycle: error -> queued (result reset, attempts preserved)
        _, ej = http("POST", base + "/jobs", {"op": "max", "input": []})
        eid = ej["id"] if isinstance(ej, dict) else None
        http("POST", "%s/jobs/%s/claim" % (base, eid))
        http("POST", "%s/jobs/%s/result" % (base, eid), {"status": "error", "result": "boom"})
        rqc, rq = http("POST", "%s/jobs/%s/requeue" % (base, eid))
        add("requeue_error_to_queued", rqc == 200 and isinstance(rq, dict)
            and rq.get("status") == "queued" and rq.get("result") is None
            and rq.get("attempts") == 1)
        c3, reclaimed = http("POST", "%s/jobs/%s/claim" % (base, eid))
        add("requeue_then_reclaim_attempts", c3 == 200 and isinstance(reclaimed, dict)
            and reclaimed.get("attempts") == 2)
        add("requeue_non_error_409",
            http("POST", "%s/jobs/%s/requeue" % (base, jid))[0] == 409,  # jid is done
            negative=True)
        add("requeue_unknown_404", http("POST", base + "/jobs/nope/requeue")[0] == 404,
            negative=True)

        # delete: only terminal jobs; deleted jobs are gone
        http("POST", "%s/jobs/%s/result" % (base, eid), {"status": "error", "result": None})
        dc, dbody = http("DELETE", "%s/jobs/%s" % (base, eid))
        gone = http("GET", "%s/jobs/%s" % (base, eid))[0] == 404
        add("delete_terminal_then_404", dc == 200 and isinstance(dbody, dict)
            and dbody.get("deleted") is True and dbody.get("id") == eid and gone)
        _, qj = http("POST", base + "/jobs", {"op": "del-probe", "input": 0})
        qid = qj["id"] if isinstance(qj, dict) else None
        add("delete_nonterminal_409", http("DELETE", "%s/jobs/%s" % (base, qid))[0] == 409,
            negative=True)
        add("delete_unknown_404", http("DELETE", base + "/jobs/nope")[0] == 404,
            negative=True)

        # listing semantics on a dedicated op marker
        marker = "lst%d" % rng.randint(1000, 9999)
        mids = []
        for i in range(6):
            _, j = http("POST", base + "/jobs", {"op": marker, "input": i})
            mids.append(j.get("id") if isinstance(j, dict) else None)
        _, lst = http("GET", base + "/jobs?op=" + marker)
        add("list_creation_order", isinstance(lst, dict)
            and [j.get("id") for j in lst.get("jobs", [])] == mids)
        _, page = http("GET", base + "/jobs?op=%s&offset=2&limit=3" % marker)
        add("page_offset_limit", isinstance(page, dict)
            and [j.get("id") for j in page.get("jobs", [])] == mids[2:5])
        _, zero = http("GET", base + "/jobs?op=%s&limit=0" % marker)
        add("page_limit_zero", isinstance(zero, dict) and zero.get("jobs") == [])
        _, beyond = http("GET", base + "/jobs?op=%s&offset=50" % marker)
        add("page_offset_beyond", isinstance(beyond, dict) and beyond.get("jobs") == [])
        add("page_invalid_400", http("GET", base + "/jobs?limit=-1")[0] == 400
            and http("GET", base + "/jobs?offset=abc")[0] == 400
            and http("GET", base + "/jobs?limit=1.5")[0] == 400,
            negative=True)
        # combined status+op filter
        http("POST", "%s/jobs/%s/claim" % (base, mids[0]))
        _, comb = http("GET", base + "/jobs?op=%s&status=queued" % marker)
        add("filter_combined", isinstance(comb, dict)
            and [j.get("id") for j in comb.get("jobs", [])] == mids[1:])

        # bulk perf: create/claim/resolve/list a few hundred jobs in budget
        n_bulk, budget = PERF_BULK_JOBS
        bulk_marker = "blk%d" % rng.randint(1000, 9999)
        t0 = time.monotonic()
        bulk_ok = True
        bulk_ids = []
        for i in range(n_bulk):
            code, j = http("POST", base + "/jobs", {"op": bulk_marker, "input": i})
            if code != 201 or not isinstance(j, dict):
                bulk_ok = False
                break
            bulk_ids.append(j["id"])
        if bulk_ok:
            for jid2 in bulk_ids:
                if http("POST", "%s/jobs/%s/claim" % (base, jid2))[0] != 200:
                    bulk_ok = False
                    break
            for jid2 in (bulk_ids if bulk_ok else []):
                if http("POST", "%s/jobs/%s/result" % (base, jid2),
                        {"status": "done", "result": 1})[0] != 200:
                    bulk_ok = False
                    break
        if bulk_ok:
            _, blst = http("GET", base + "/jobs?op=%s&status=done" % bulk_marker, timeout=15)
            bulk_ok = isinstance(blst, dict) and len(blst.get("jobs", [])) == n_bulk \
                and [j.get("id") for j in blst["jobs"]] == bulk_ids
        elapsed = time.monotonic() - t0
        within = elapsed <= budget
        perf_detail = {"jobs": n_bulk, "budget_s": budget,
                       "elapsed_s": round(elapsed, 2), "ok": bool(bulk_ok and within)}
        add("perf_bulk", bulk_ok and within)
    finally:
        stop(proc)

    positives_ok = any(ok for name, ok, neg in checks if not neg and name != "healthz")
    passed = sum(1 for _n, ok, neg in checks if ok and (not neg or positives_ok))
    fails = [n for n, ok, neg in checks if not ok]
    return passed, len(checks), ([{"failed_checks": fails}] if fails else []), perf_detail


def battery_cli(workdir, rng):
    ref = RefApi()
    base = ref.start()
    client = os.path.join(workdir, "cli", "client.py")
    checks = []

    def run(*args, timeout=20):
        try:
            return subprocess.run([PYTHON, client, *args], cwd=workdir,
                                  capture_output=True, text=True, timeout=timeout)
        except (subprocess.TimeoutExpired, OSError):
            return None

    def add(name, cond):
        checks.append((name, bool(cond)))

    try:
        # submit: several generated jobs; cli must POST and print the new id
        ok_submit = True
        for op, value in [gen_valid_job(rng) for _ in range(3)]:
            p = run("submit", base, op, json.dumps(value))
            if p is None or p.returncode != 0:
                ok_submit = False
                continue
            jid = p.stdout.strip().splitlines()[-1].strip() if p.stdout.strip() else ""
            job = ref.jobs.get(jid)
            if not (job and job["op"] == op and oracle.json_equal(job["input"], value)):
                ok_submit = False
        add("submit_creates_job", ok_submit)

        # submit that the server rejects (empty op -> 400) must fail. GATED.
        p = run("submit", base, "", "1")
        add("submit_rejected_on_400",
            ok_submit and p is not None and p.returncode != 0 and not p.stdout.strip())

        # get: seed a job, cli get prints it
        op, value = gen_valid_job(rng)
        jid = ref.seed(op, value)
        p = run("get", base, jid)
        ok_get = False
        if p is not None and p.returncode == 0:
            try:
                got = json.loads(p.stdout)
                ok_get = got.get("id") == jid and oracle.json_equal(got.get("input"), value)
            except json.JSONDecodeError:
                ok_get = False
        add("get_prints_job", ok_get)

        # get unknown -> non-zero exit. GATED on the positive get working, so a
        # do-nothing stub (which "fails" everything) gets no credit here.
        p = run("get", base, "nope-unknown")
        add("get_unknown_rejected", ok_get and p is not None and p.returncode != 0)

        # wait on a done job -> exit 0, prints it
        jid = ref.seed("sum", [1, 2], status="done", result=3, attempts=1)
        p = run("wait", base, jid, "--timeout", "5")
        ok_wait = False
        if p is not None and p.returncode == 0:
            try:
                w = json.loads(p.stdout)
                ok_wait = w.get("status") == "done" and oracle.json_equal(w.get("result"), 3)
            except json.JSONDecodeError:
                ok_wait = False
        add("wait_done_exit0", ok_wait)

        # wait --quiet: nothing on stdout, exit code still 0. GATED.
        p = run("wait", base, jid, "--timeout", "5", "--quiet")
        add("wait_quiet_silent", ok_wait and p is not None and p.returncode == 0
            and p.stdout == "")

        # wait on an error job -> non-zero exit. GATED on wait_done working.
        jid = ref.seed("max", [], status="error", result=None, attempts=1)
        p = run("wait", base, jid, "--timeout", "5")
        add("wait_error_rejected", ok_wait and p is not None and p.returncode != 0)

        # submit-batch: all-valid file -> ids in order, exit 0
        batch_jobs = [gen_valid_job(rng) for _ in range(4)]
        ok_batch = False
        with tempfile.TemporaryDirectory() as td:
            bpath = os.path.join(td, "batch.jsonl")
            with open(bpath, "w", encoding="utf-8") as fh:
                for op, value in batch_jobs:
                    fh.write(json.dumps({"op": op, "input": value}) + "\n")
            before = set(ref.jobs)
            p = run("submit-batch", base, bpath)
            if p is not None and p.returncode == 0:
                out_ids = [ln.strip() for ln in p.stdout.splitlines() if ln.strip()]
                created = [ref.jobs[i] for i in out_ids if i in ref.jobs]
                ok_batch = (len(out_ids) == 4 and len(created) == 4
                            and len(set(ref.jobs) - before) == 4
                            and all(c["op"] == op and oracle.json_equal(c["input"], value)
                                    for c, (op, value) in zip(created, batch_jobs)))
            add("batch_all_valid", ok_batch)

            # submit-batch with broken lines: continues, reports failure. GATED.
            b2 = os.path.join(td, "batch2.jsonl")
            good2 = [gen_valid_job(rng) for _ in range(2)]
            with open(b2, "w", encoding="utf-8") as fh:
                fh.write(json.dumps({"op": good2[0][0], "input": good2[0][1]}) + "\n")
                fh.write("this is not json\n")
                fh.write(json.dumps({"no_op": True}) + "\n")
                fh.write(json.dumps({"op": good2[1][0], "input": good2[1][1]}) + "\n")
            before = set(ref.jobs)
            p = run("submit-batch", base, b2)
            ok_b2 = False
            if p is not None and p.returncode != 0:
                out_ids = [ln.strip() for ln in p.stdout.splitlines() if ln.strip()]
                ok_b2 = (len(out_ids) == 2 and len(set(ref.jobs) - before) == 2
                         and all(i in ref.jobs for i in out_ids))
            add("batch_continues_on_bad_lines", ok_batch and ok_b2)

            # unreadable file -> non-zero. GATED.
            p = run("submit-batch", base, os.path.join(td, "missing.jsonl"))
            add("batch_missing_file_rejected",
                ok_batch and p is not None and p.returncode != 0)

        # requeue an error job -> queued, printed; non-error -> non-zero. GATED.
        eid = ref.seed("max", [], status="error", result=None, attempts=2)
        p = run("requeue", base, eid)
        ok_rq = False
        if p is not None and p.returncode == 0:
            try:
                rq = json.loads(p.stdout)
                ok_rq = (rq.get("status") == "queued" and rq.get("result") is None
                         and ref.jobs[eid]["status"] == "queued"
                         and ref.jobs[eid]["attempts"] == 2)
            except (json.JSONDecodeError, KeyError):
                ok_rq = False
        add("requeue_error_job", ok_rq)
        did = ref.seed("sum", [1], status="done", result=1, attempts=1)
        p = run("requeue", base, did)
        add("requeue_non_error_rejected", ok_rq and p is not None and p.returncode != 0)
    finally:
        ref.stop()

    passed = sum(1 for _n, c in checks if c)
    fails = [n for n, c in checks if not c]
    return passed, len(checks), [{"failed_checks": fails}] if fails else []


def seed_population(ref, rng, n):
    """Seed the reference API with a varied population; returns nothing (the
    grader reads ref.snapshot() for truth)."""
    statuses = ["queued", "running", "done", "error"]
    for _ in range(n):
        op, value = gen_valid_job(rng)
        st = rng.choice(statuses)
        result = None
        attempts = 0
        if st == "done":
            attempts = 1
            roll = rng.random()
            if roll < 0.5:
                result = round(rng.uniform(-100, 100), 2)
            elif roll < 0.65:
                result = rng.randint(-50, 50)
            elif roll < 0.8:
                result = rng.choice(["DONE", "ok", ""])
            elif roll < 0.9:
                result = rng.choice([True, False])  # bools are NOT numbers
            else:
                result = rng.choice([[1, 2], {"k": 1}, None])
        elif st == "error":
            attempts = 1
        elif st == "running":
            attempts = 1
        ref.seed(op, value, status=st, result=result, attempts=attempts)


def battery_metrics(workdir, rng):
    checks = []

    def add(name, cond):
        checks.append((name, bool(cond)))

    ref = RefApi()
    api_base = ref.start()
    seed_population(ref, rng, rng.randint(18, 30))
    port = free_port()
    proc = start_metrics(workdir, port, api_base)
    base = "http://127.0.0.1:%d" % port
    summary_ok = False
    try:
        healthy = wait_healthy(base)
        add("healthz", healthy)

        code, got = http("GET", base + "/summary", timeout=10)
        want = oracle.metrics_summary(ref.snapshot())
        summary_ok = code == 200 and oracle.json_equal(got, want, tol=1e-6)
        add("summary_matches", summary_ok)

        ops_present = sorted({j["op"] for j in ref.snapshot()})
        probe_op = rng.choice(ops_present) if ops_present else "sum"
        code, got = http("GET", base + "/ops/" + probe_op, timeout=10)
        want = oracle.metrics_op_summary(ref.snapshot(), probe_op)
        add("ops_seen_matches", code == 200 and oracle.json_equal(got, want, tol=1e-6))

        code, got = http("GET", base + "/ops/never-seen-op", timeout=10)
        add("ops_unseen_zero", code == 200 and oracle.json_equal(
            got, {"op": "never-seen-op", "total": 0, "by_status": {}}))

        # freshness: the service must re-fetch per request
        seed_population(ref, rng, 4)
        ref.seed("fresh-op", [1], status="done", result=42.5, attempts=1)
        code, got = http("GET", base + "/summary", timeout=10)
        want = oracle.metrics_summary(ref.snapshot())
        add("summary_freshness", code == 200 and oracle.json_equal(got, want, tol=1e-6))
        code, got = http("GET", base + "/ops/fresh-op", timeout=10)
        want = oracle.metrics_op_summary(ref.snapshot(), "fresh-op")
        add("ops_freshness", code == 200 and oracle.json_equal(got, want, tol=1e-6))

        code, _ = http("GET", base + "/nope", timeout=10)
        add("unknown_path_404", summary_ok and code == 404)  # gated negative
    finally:
        stop(proc)
        ref.stop()

    # API-down behavior: healthz stays 200, summary/ops are 503. The 503
    # checks are gated on the positive summary path.
    dead_api = "http://127.0.0.1:%d" % free_port()
    port = free_port()
    proc = start_metrics(workdir, port, dead_api)
    base = "http://127.0.0.1:%d" % port
    try:
        healthy = wait_healthy(base)
        add("healthz_independent_of_api", healthy)
        code, body = http("GET", base + "/summary", timeout=12)
        add("dead_api_summary_503", summary_ok and code == 503
            and isinstance(body, dict) and "error" in body)
        code, _ = http("GET", base + "/ops/sum", timeout=12)
        add("dead_api_ops_503", summary_ok and code == 503)
    finally:
        stop(proc)

    passed = sum(1 for _n, c in checks if c)
    fails = [n for n, c in checks if not c]
    return passed, len(checks), [{"failed_checks": fails}] if fails else []


def integration(workdir, rng):
    detail = {}
    api_port = free_port()
    api = start_api(workdir, api_port)
    base = "http://127.0.0.1:%d" % api_port
    client = os.path.join(workdir, "cli", "client.py")
    worker = None
    metrics_proc = None
    mbase = None

    singles = [gen_valid_job(rng) for _ in range(4)] + \
        [gen_invalid_submittable(rng) for _ in range(3)]
    rng.shuffle(singles)
    batch = [gen_valid_job(rng) for _ in range(4)]
    n_jobs = len(singles) + len(batch)
    extra_names = ["requeue_ok", "requeue_reprocessed", "metrics_summary_matches",
                   "metrics_op_matches"]
    extras = {k: False for k in extra_names}
    correct = 0
    try:
        if not wait_healthy(base):
            detail["error"] = "api not healthy"
            return 0.0, detail
        worker = subprocess.Popen(
            [PYTHON, os.path.join(workdir, "worker", "worker.py"), "serve", base],
            cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        metrics_port = free_port()
        metrics_proc = start_metrics(workdir, metrics_port, base)
        mbase = "http://127.0.0.1:%d" % metrics_port

        def cli(*args, timeout=20):
            try:
                return subprocess.run([PYTHON, client, *args], cwd=workdir,
                                      capture_output=True, text=True, timeout=timeout)
            except (subprocess.TimeoutExpired, OSError):
                return None

        # Submit everything FIRST (worker processes concurrently), then wait
        # under a shared deadline so a dead worker bounds total grading time.
        submitted = []  # (op, value, jid|None)
        for op, value in singles:
            jid = None
            p = cli("submit", base, op, json.dumps(value))
            if p is not None and p.returncode == 0 and p.stdout.strip():
                jid = p.stdout.strip().splitlines()[-1].strip()
            submitted.append((op, value, jid))
        with tempfile.TemporaryDirectory() as td:
            bpath = os.path.join(td, "batch.jsonl")
            with open(bpath, "w", encoding="utf-8") as fh:
                for op, value in batch:
                    fh.write(json.dumps({"op": op, "input": value}) + "\n")
            p = cli("submit-batch", base, bpath)
            bids = []
            if p is not None and p.stdout.strip():
                bids = [ln.strip() for ln in p.stdout.splitlines() if ln.strip()]
            for (op, value), jid in zip(batch, bids + [None] * len(batch)):
                submitted.append((op, value, jid))

        deadline = time.time() + 25.0
        finals = []
        for op, value, jid in submitted:
            want_status, want_result = oracle.compute(op, value)
            final = None
            if jid is not None:
                per = max(1.0, deadline - time.time())
                p = cli("wait", base, jid, "--timeout", "%.1f" % per, timeout=per + 10)
                if p is not None and p.stdout.strip():
                    try:
                        final = json.loads(p.stdout)
                    except json.JSONDecodeError:
                        final = None
                if final is None:  # fall back to a direct GET
                    _, final = http("GET", "%s/jobs/%s" % (base, jid))
            ok = isinstance(final, dict) and final.get("status") == want_status
            if ok and want_status == "done":
                ok = oracle.json_equal(final.get("result"), want_result)
            finals.append((op, value, jid, final, bool(ok)))
        correct = sum(1 for *_x, ok in finals if ok)
        detail["jobs"] = n_jobs
        detail["correct"] = correct

        # requeue flow: an error job goes back through the worker to error again
        err = next(((op, value, jid) for op, value, jid, final, ok in finals
                    if ok and isinstance(final, dict) and final.get("status") == "error"),
                   None)
        if err is not None:
            _op, _value, jid = err
            p = cli("requeue", base, jid)
            if p is not None and p.returncode == 0:
                try:
                    rq = json.loads(p.stdout)
                    extras["requeue_ok"] = rq.get("status") == "queued" and rq.get("result") is None
                except json.JSONDecodeError:
                    extras["requeue_ok"] = False
            p = cli("wait", base, jid, "--timeout", "10", timeout=20)
            _, refetched = http("GET", "%s/jobs/%s" % (base, jid))
            extras["requeue_reprocessed"] = (isinstance(refetched, dict)
                                             and refetched.get("status") == "error"
                                             and isinstance(refetched.get("attempts"), int)
                                             and refetched.get("attempts") >= 2)
        # metrics cross-check against the API's actual job list
        _, listing = http("GET", base + "/jobs", timeout=10)
        if isinstance(listing, dict) and isinstance(listing.get("jobs"), list) and mbase:
            jobs_now = listing["jobs"]
            code, got = http("GET", mbase + "/summary", timeout=10)
            want = oracle.metrics_summary(jobs_now)
            extras["metrics_summary_matches"] = code == 200 and oracle.json_equal(
                got, want, tol=1e-6)
            ops_now = sorted({j.get("op") for j in jobs_now if isinstance(j, dict)})
            if ops_now:
                probe = rng.choice(ops_now)
                code, got = http("GET", mbase + "/ops/" + probe, timeout=10)
                want = oracle.metrics_op_summary(jobs_now, probe)
                extras["metrics_op_matches"] = code == 200 and oracle.json_equal(
                    got, want, tol=1e-6)
        detail["extras"] = dict(extras)
    finally:
        stop(metrics_proc)
        stop(worker)
        stop(api)
    units = n_jobs + len(extra_names)
    score = (correct + sum(1 for v in extras.values() if v)) / units
    return round(score, 4), detail


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workdir")
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()
    seed = args.seed if args.seed is not None else random.randrange(1, 2**31)
    workdir = os.path.abspath(args.workdir)

    rng_w = random.Random(seed ^ 0xA1A1A1)
    rng_a = random.Random(seed ^ 0xB2B2B2)
    rng_c = random.Random(seed ^ 0xC3C3C3)
    rng_m = random.Random(seed ^ 0xE5E5E5)
    rng_i = random.Random(seed ^ 0xD4D4D4)

    w_pass, w_tot, w_fail, w_perf = battery_worker(workdir, rng_w)
    a_pass, a_tot, a_fail, a_perf = battery_api(workdir, rng_a)
    c_pass, c_tot, c_fail = battery_cli(workdir, rng_c)
    m_pass, m_tot, m_fail = battery_metrics(workdir, rng_m)
    integ, idetail = integration(workdir, rng_i)

    comp = {
        "api": round(a_pass / a_tot, 4),
        "worker": round(w_pass / w_tot, 4),
        "cli": round(c_pass / c_tot, 4),
        "metrics": round(m_pass / m_tot, 4),
    }
    total = round(sum(comp.values()) + integ, 4)
    print(json.dumps({
        "task": "service-triplet",
        "seed": seed,
        "component_scores": comp,
        "integration": integ,
        "total": total,
        "max_total": 5.0,
        "details": {
            "api": {"passed": a_pass, "total": a_tot, "perf": a_perf, "fails": a_fail},
            "worker": {"passed": w_pass, "total": w_tot, "perf": w_perf, "fails": w_fail},
            "cli": {"passed": c_pass, "total": c_tot, "fails": c_fail},
            "metrics": {"passed": m_pass, "total": m_tot, "fails": m_fail},
            "integration": idetail,
        },
    }, indent=2, default=str))


if __name__ == "__main__":
    main()
