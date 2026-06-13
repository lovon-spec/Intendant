#!/usr/bin/env python3
"""Reference CLI client (agent-facing solution). See cli/SPEC.md.
Excluded from agent visibility by the SKILL runner."""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request


def _http(method, url, body=None, timeout=5):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"null")
        except json.JSONDecodeError:
            return e.code, None
    except (urllib.error.URLError, OSError, json.JSONDecodeError):
        return None, None


def cmd_submit(args):
    code, job = _http("POST", args.url.rstrip("/") + "/jobs",
                      {"op": args.op, "input": json.loads(args.input_json)})
    if code != 201 or not isinstance(job, dict) or "id" not in job:
        print("submit failed (HTTP %s): %s" % (code, job), file=sys.stderr)
        return 1
    print(job["id"])
    return 0


def cmd_submit_batch(args):
    base = args.url.rstrip("/")
    try:
        with open(args.file, encoding="utf-8") as fh:
            lines = fh.read().splitlines()
    except OSError as e:
        print("cannot read %s: %s" % (args.file, e), file=sys.stderr)
        return 1
    failures = 0
    for i, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            print("line %d: invalid JSON" % i, file=sys.stderr)
            failures += 1
            continue
        if not isinstance(obj, dict) or not isinstance(obj.get("op"), str) \
                or "input" not in obj:
            print("line %d: need an object with string op and input" % i, file=sys.stderr)
            failures += 1
            continue
        code, job = _http("POST", base + "/jobs",
                          {"op": obj["op"], "input": obj["input"]})
        if code == 201 and isinstance(job, dict) and "id" in job:
            print(job["id"])
        else:
            print("line %d: submit failed (HTTP %s)" % (i, code), file=sys.stderr)
            failures += 1
    return 0 if failures == 0 else 1


def cmd_get(args):
    code, job = _http("GET", "%s/jobs/%s" % (args.url.rstrip("/"), args.job_id))
    if code != 200 or job is None:
        print("get failed (HTTP %s)" % code, file=sys.stderr)
        return 1
    print(json.dumps(job))
    return 0


def cmd_wait(args):
    base = args.url.rstrip("/")
    deadline = time.time() + args.timeout
    while time.time() < deadline:
        code, job = _http("GET", "%s/jobs/%s" % (base, args.job_id))
        if code == 200 and isinstance(job, dict) and job.get("status") in ("done", "error"):
            if not args.quiet:
                print(json.dumps(job))
            return 0 if job["status"] == "done" else 1
        time.sleep(args.poll)
    print("wait timed out after %ss" % args.timeout, file=sys.stderr)
    return 1


def cmd_requeue(args):
    code, job = _http("POST", "%s/jobs/%s/requeue" % (args.url.rstrip("/"), args.job_id))
    if code != 200 or not isinstance(job, dict):
        print("requeue failed (HTTP %s): %s" % (code, job), file=sys.stderr)
        return 1
    print(json.dumps(job))
    return 0


def main(argv):
    ap = argparse.ArgumentParser(prog="client.py")
    sub = ap.add_subparsers(dest="verb", required=True)
    s = sub.add_parser("submit"); s.add_argument("url"); s.add_argument("op"); s.add_argument("input_json")
    b = sub.add_parser("submit-batch"); b.add_argument("url"); b.add_argument("file")
    g = sub.add_parser("get"); g.add_argument("url"); g.add_argument("job_id")
    w = sub.add_parser("wait"); w.add_argument("url"); w.add_argument("job_id")
    w.add_argument("--timeout", type=float, default=10.0)
    w.add_argument("--poll", type=float, default=0.1)
    w.add_argument("--quiet", action="store_true")
    r = sub.add_parser("requeue"); r.add_argument("url"); r.add_argument("job_id")
    args = ap.parse_args(argv[1:])
    return {"submit": cmd_submit, "submit-batch": cmd_submit_batch, "get": cmd_get,
            "wait": cmd_wait, "requeue": cmd_requeue}[args.verb](args)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
