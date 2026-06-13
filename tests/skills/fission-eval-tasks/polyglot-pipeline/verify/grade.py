#!/usr/bin/env python3
"""Behavioral grader for polyglot-pipeline.

Generates inputs at check time from a random seed, runs the agent's tools, and
compares their output to the independent oracle in oracle.py. Emits the suite
JSON contract on stdout. Never inspects file/string presence; only behavior.

Components: normalizer, quarantine, dedup, report (1.0 each) + integration
(1.0) => max_total 5.0. Per-component perf scenarios enforce the SPEC budgets.

Usage: grade.py <scratch_workdir> [--seed N]   (scratch is graded in place)
"""
import argparse
import csv
import datetime
import io
import json
import os
import random
import string
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle  # noqa: E402

# The canonical Makefile is the task's own skeleton/Makefile (single source of
# truth); the grader pins it over the agent's copy before the integration run
# so a tampered Makefile can't fake the pipeline wiring.
TASK_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CANONICAL_MAKEFILE = os.path.join(TASK_ROOT, "skeleton", "Makefile")
PYTHON = sys.executable
RUN_TIMEOUT = 30
MAKE_TIMEOUT = 300
# Perf budgets (seconds) per the component SPECs.
PERF_NORMALIZER = (120000, 45)
PERF_DEDUP = (500000, 10)
PERF_REPORT = (60000, 30)


# ----------------------------------------------------------- random builders
def rand_id(rng):
    n = rng.randint(1, 12)
    return "".join(rng.choice(string.ascii_letters + string.digits + "_-")
                   for _ in range(n))


def rand_bad_id(rng):
    return rng.choice(["", "   ", "has space", "ex!cl", "a" * 33, "dot.ted",
                       "tab\tbed", "comma,id", "*star*"])


def rand_name(rng):
    pool = ["Lee, Ann", "Bo", "", "  ", "O'Hara", "Zed   Zee", "mary\tjane",
            "X, Y, Z", "A  B  C", " padded  inner "]
    return rng.choice(pool)


def rand_valid_email(rng):
    user = "".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 6)))
    if rng.random() < 0.3:
        user += "+" + "".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 4)))
    dom = "".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 5)))
    tld = rng.choice(["com", "io", "net", "co.uk"])
    s = "%s@%s.%s" % (user, dom, tld)
    # randomly upper-case some letters; the spec lowercases.
    return "".join(c.upper() if rng.random() < 0.4 else c for c in s)


def rand_invalid_email(rng):
    return rng.choice(["plainstring", "a@@b.com", "@nodomain.com", "nolocal@",
                       "two@at@sign.com", "nodot@domain", "a@.io", "a@io.",
                       "a@b..c", "+tail@ex.io", "++@ex.io"])


def _group_commas(digits):
    if len(digits) <= 3:
        return digits
    out = []
    while len(digits) > 3:
        out.insert(0, digits[-3:])
        digits = digits[:-3]
    out.insert(0, digits)
    return ",".join(out)


def rand_valid_amount(rng):
    whole = rng.choice([rng.randint(0, 9999), rng.randint(0, 999999), 1000000])
    s = str(whole)
    if whole != 1000000 and rng.random() < 0.5:
        if rng.random() < 0.5:
            s = "%s.%02d" % (whole, rng.randint(0, 99))
        else:
            s = "%s.%d" % (whole, rng.randint(0, 9))
    if rng.random() < 0.4:
        intpart, dot, frac = s.partition(".")
        s = _group_commas(intpart) + dot + frac
    if rng.random() < 0.3:
        s = "$" + s
    roll = rng.random()
    if roll < 0.2:
        s = "-" + s
    elif roll < 0.4:
        s = "(" + s + ")"
    return s


def rand_invalid_amount(rng):
    return rng.choice(["12.345", "1.2.3", "$", "", "abc", ".5", "--5",
                       "1,23.45", "12,34", "1234,567", "1.2,3", "12.",
                       "$-12", "1 000", "0x10", "(-5)", "(5", "5)", "()",
                       "1000000.01", "2,000,000", "(1,000,000.50)"])


def rand_valid_date(rng):
    y = rng.randint(1990, 2035)
    m = rng.randint(1, 12)
    d = rng.randint(1, 28)
    roll = rng.random()
    if roll < 0.4:
        return "%04d-%02d-%02d" % (y, m, d)
    if roll < 0.7:
        return "%02d/%02d/%04d" % (m, d, y)
    return "%02d.%02d.%04d" % (d, m, y)


def rand_invalid_date(rng):
    return rng.choice(["2025-13-01", "2025-02-30", "2025/01/01", "1/9/2025",
                       "2025-1-9", "01-09-2025", "Jan 5 2025", "20250101",
                       "2025-00-10", "", "1989-12-31", "2036-01-01",
                       "12/31/1989", "01.01.2036", "31/12/2025", "12.31.2025",
                       "0995-03-03"])


def rand_tags_field(rng):
    pool = ["red", "blue", "green", "vip", "eu", "us", "x9", "y_2", "m", "n"]
    bad = ["m&m", "sp ace", "Tag!", "dot.t", "-lead"]
    k = rng.randint(0, 5)
    chosen = [rng.choice(pool) for _ in range(k)]
    if chosen and rng.random() < 0.5:
        chosen.append(chosen[0].upper())  # dup after lowercasing
    if rng.random() < 0.4:
        chosen.append(rng.choice(bad))  # dropped, not row-rejecting
    if rng.random() < 0.3:
        chosen.append("")
    rng.shuffle(chosen)
    seps = [";", "|"]
    out = ""
    for i, c in enumerate(chosen):
        cell = " %s " % c if rng.random() < 0.3 else c
        out += cell if i == 0 else rng.choice(seps) + cell
    return out


def valid_cells(rng):
    return {
        "id": rand_id(rng),
        "name": rand_name(rng),
        "email": rand_valid_email(rng) if rng.random() < 0.7 else "",
        "amount": rand_valid_amount(rng),
        "date": rand_valid_date(rng),
        "tags": rand_tags_field(rng),
    }


def render_csv(rng, rows, extra_unknown=True):
    """rows: list of cell-dicts. Header order is shuffled per scenario and may
    gain unknown columns (ignored per SPEC)."""
    header = oracle.COLUMNS[:]
    if extra_unknown and rng.random() < 0.5:
        for nm in rng.sample(["junk", "region", "note"], rng.randint(1, 2)):
            header.insert(rng.randint(0, len(header)), nm)
    rng.shuffle(header)
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(header)
    for r in rows:
        w.writerow([r.get(h, "zz" if h not in oracle.COLUMNS else "") for h in header])
    return buf.getvalue()


def render_csv_dup_amount(rng, rows):
    """Header contains `amount` twice: decoy first, real last (last wins)."""
    header = ["date", "id", "amount", "tags", "name", "email", "amount"]
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(header)
    for r in rows:
        w.writerow([r["date"], r["id"], "999999.99", r["tags"], r["name"],
                    r["email"], r["amount"]])
    return buf.getvalue()


# --------------------------------------------------------- scenario builders
def normalizer_scenarios(rng):
    """List of (label, kind, csv_text); kind 'ok' compares output to oracle,
    'malformed' requires a non-zero exit, 'perf' is the budgeted large run."""
    scn = []
    for _ in range(10):
        c = valid_cells(rng)
        c["amount"] = rand_valid_amount(rng) if rng.random() < 0.5 else rand_invalid_amount(rng)
        scn.append(("amount", "ok", render_csv(rng, [c])))
    for _ in range(8):
        c = valid_cells(rng)
        c["date"] = rand_valid_date(rng) if rng.random() < 0.5 else rand_invalid_date(rng)
        scn.append(("date", "ok", render_csv(rng, [c])))
    for _ in range(8):
        c = valid_cells(rng)
        c["email"] = rng.choice([rand_valid_email(rng), "", rand_invalid_email(rng)])
        scn.append(("email", "ok", render_csv(rng, [c])))
    for _ in range(6):
        c = valid_cells(rng)
        c["tags"] = rand_tags_field(rng)
        if rng.random() < 0.4:  # 10-vs-11 distinct boundary
            n = rng.choice([10, 11])
            c["tags"] = "|".join("t%02d" % i for i in range(n))
        scn.append(("tags", "ok", render_csv(rng, [c])))
    for _ in range(4):
        good = valid_cells(rng)
        bad = valid_cells(rng)
        bad["id"] = rand_bad_id(rng)
        scn.append(("id", "ok", render_csv(rng, [good, bad])))
    for _ in range(3):
        good1 = valid_cells(rng)
        blankish = {k: "" for k in oracle.COLUMNS}
        good2 = valid_cells(rng)
        scn.append(("blank_row", "ok", render_csv(rng, [good1, blankish, good2])))
    for _ in range(2):
        rows = [valid_cells(rng) for _ in range(rng.randint(2, 4))]
        scn.append(("dup_header", "ok", render_csv_dup_amount(rng, rows)))
    for _ in range(2):
        missing = rng.choice(oracle.COLUMNS)
        header = [c for c in oracle.COLUMNS if c != missing]
        rng.shuffle(header)
        buf = io.StringIO()
        w = csv.writer(buf)
        w.writerow(header)
        c = valid_cells(rng)
        w.writerow([c.get(h, "") for h in header])
        scn.append(("missing_column", "malformed", buf.getvalue()))
    for _ in range(6):
        rows = []
        for _ in range(rng.randint(3, 8)):
            c = valid_cells(rng)
            roll = rng.random()
            if roll < 0.2:
                c["amount"] = rand_invalid_amount(rng)
            elif roll < 0.35:
                c["date"] = rand_invalid_date(rng)
            elif roll < 0.45:
                c["email"] = rand_invalid_email(rng)
            elif roll < 0.55:
                c["id"] = rand_bad_id(rng)
            rows.append(c)
        scn.append(("fuzz", "ok", render_csv(rng, rows)))
    scn.append(("perf_120k", "perf", None))
    return scn


def perf_normalizer_csv(rng):
    """A 120k-row CSV (mostly valid, light variation), built fast."""
    n_rows, _budget = PERF_NORMALIZER
    header = ["id", "name", "email", "amount", "date", "tags"]
    parts = [",".join(header)]
    dates = ["2024-0%d-1%d" % (rng.randint(1, 9), rng.randint(0, 9)) for _ in range(20)]
    doms = ["ex.io", "mail.net", "corp.co.uk"]
    for i in range(n_rows):
        rid = "p%06d" % i
        amt = "%d.%02d" % (i % 9000, i % 100)
        if i % 17 == 0:
            amt = "12.345"  # reject
        email = "" if i % 5 == 0 else "U%d@%s" % (i, doms[i % 3])
        tags = "a;b" if i % 3 == 0 else "vip|x%d" % (i % 7)
        parts.append("%s,Name %d,%s,%s,%s,%s"
                     % (rid, i, email, amt, dates[i % 20], tags))
    return "\n".join(parts) + "\n"


def make_record(rng, id_pool=None, date_pool=None, domain_pool=None):
    rid = rng.choice(id_pool) if id_pool else rand_id(rng)
    d = rng.choice(date_pool) if date_pool else rand_valid_date_iso(rng)
    tags = sorted(set(rng.choice(["a", "b", "c", "d", "e", "x", "y"])
                      for _ in range(rng.randint(0, 4))))
    if rng.random() < 0.65:
        dom = rng.choice(domain_pool) if domain_pool else \
            rng.choice(["ex.io", "mail.net", "shop.de", "corp.co.uk"])
        email = "%s@%s" % ("".join(rng.choice(string.ascii_lowercase)
                                   for _ in range(rng.randint(1, 5))), dom)
    else:
        email = None
    return {
        "id": rid,
        "name": rng.choice(["Ann", "Bo Lee", "", "Zed Zee", "O'Hara"]),
        "email": email,
        "amount": rand_amount_value(rng),
        "date": d,
        "tags": tags,
    }


def rand_valid_date_iso(rng):
    return "%04d-%02d-%02d" % (rng.randint(2015, 2030), rng.randint(1, 12), rng.randint(1, 28))


def rand_amount_value(rng):
    roll = rng.random()
    if roll < 0.3:
        return rng.randint(-50, 5000)
    if roll < 0.6:
        return round(rng.uniform(-100, 9000), 2)
    return round(rng.uniform(0, 1000), 1)


def malformed_record(rng):
    base = make_record(rng)
    breakers = [
        lambda r: r.update(amount=str(r["amount"])),
        lambda r: r.update(amount=True),
        lambda r: r.update(id=""),
        lambda r: r.update(id=7),
        lambda r: r.pop("date"),
        lambda r: r.update(date="2025/01/01"),
        lambda r: r.update(tags="not-a-list"),
        lambda r: r.update(tags=[1, 2]),
        lambda r: r.update(email=42),
        lambda r: r.pop("name"),
    ]
    rng.choice(breakers)(base)
    return base


def quarantine_scenarios(rng):
    """List of (label, kind, records, as_of). kind 'ok' or 'usage'."""
    scn = []

    def base(amount=100, date="2024-03-03", email="e@x.io", name="Ann"):
        return {"id": rand_id(rng), "name": name, "email": email,
                "amount": amount, "date": date, "tags": []}

    as_of = "2026-06-01"
    # rule probes with boundary values
    scn.append(("amount_limit", "ok",
                [base(25000.01), base(25000), base(-25000.01), base(-25000)], as_of))
    scn.append(("zero", "ok", [base(0), base(0.0), base(0.01)], as_of))
    scn.append(("missing_contact", "ok",
                [base(1000.01, email=None), base(1000, email=None),
                 base(1000.01), base(-2000, email=None)], as_of))
    scn.append(("test_data", "ok",
                [base(name="Latest Tester"), base(name="attestation"),
                 base(name="TEST CO"), base(name="Tes t"), base(name="")], as_of))
    scn.append(("future", "ok",
                [base(date="2026-06-02"), base(date="2026-06-01"),
                 base(date="2030-01-01")], as_of))
    scn.append(("stale", "ok",
                [base(date="2021-05-31"), base(date="2021-06-01"),
                 base(date="2015-01-01")], as_of))
    scn.append(("multi_reason", "ok",
                [base(30000, email=None, name="test co", date="2027-01-01"),
                 base(-26000, date="2020-01-01")], as_of))
    # leap-day cutoffs
    scn.append(("leap_cutoff", "ok",
                [base(date="2019-02-27"), base(date="2019-02-28"),
                 base(date="2019-03-01")], "2024-02-29"))
    scn.append(("nonleap_as_of", "ok",
                [base(date="2022-02-27"), base(date="2022-02-28")], "2027-02-28"))
    # randomized as-of + fuzz batches (order preservation, mixed reasons)
    for _ in range(8):
        ao = rand_valid_date_iso(rng)
        recs = []
        for _ in range(rng.randint(3, 9)):
            r = make_record(rng)
            roll = rng.random()
            if roll < 0.2:
                r["amount"] = rng.choice([0, 25001, -30000, 26000.5])
            elif roll < 0.35:
                r["amount"] = round(rng.uniform(1001, 24000), 2)
                r["email"] = None
            elif roll < 0.5:
                r["name"] = rng.choice(["Test", "latest", "protest co", "fine"])
            recs.append(r)
        scn.append(("fuzz", "ok", recs, ao))
    scn.append(("empty_input", "ok", [], as_of))
    # usage errors
    scn.append(("bad_as_of_format", "usage", [base()], "06/01/2026"))
    scn.append(("bad_as_of_date", "usage", [base()], "2026-13-01"))
    return scn


def dedup_scenarios(rng):
    """List of (label, kind, files, since). kinds: ok | fatal | usage_nofiles
    | usage_badsince | perf."""
    scn = []
    dates = ["2024-12-31", "2025-01-01", "2025-02-01", "2025-02-01", "2025-06-15"]
    for _ in range(2):
        recs = [make_record(rng) for _ in range(rng.randint(2, 6))]
        scn.append(("sort_unique", "ok", [recs], None))
    for _ in range(4):
        pool = ["id%d" % i for i in range(rng.randint(2, 4))]
        f1 = [make_record(rng, pool, dates) for _ in range(rng.randint(2, 4))]
        f2 = [make_record(rng, pool, dates) for _ in range(rng.randint(2, 4))]
        scn.append(("conflict2", "ok", [f1, f2], None))
    # email beats position on date ties
    for _ in range(3):
        rid = rand_id(rng)
        nd = "2025-09-09"
        a = {"id": rid, "name": "with_email", "email": "win@e.io", "amount": 1,
             "date": nd, "tags": ["a"]}
        b = {"id": rid, "name": "later_null", "email": None, "amount": 2,
             "date": nd, "tags": ["b"]}
        older = {"id": rid, "name": "old", "email": None, "amount": 9,
                 "date": "2020-01-01", "tags": ["d"]}
        extra = [make_record(rng) for _ in range(rng.randint(0, 2))]
        scn.append(("tie_email_beats_pos", "ok", [[older, a] + extra, [b]], None))
    # all-null tie -> position; both-email tie -> position
    rid = rand_id(rng)
    nd = "2025-08-08"
    scn.append(("tie_all_null", "ok",
                [[{"id": rid, "name": "n1", "email": None, "amount": 1, "date": nd, "tags": ["x"]}],
                 [{"id": rid, "name": "n2", "email": None, "amount": 2, "date": nd, "tags": ["y"]}]],
                None))
    rid = rand_id(rng)
    scn.append(("tie_both_email", "ok",
                [[{"id": rid, "name": "e1", "email": "a@x.io", "amount": 1, "date": nd, "tags": []}],
                 [{"id": rid, "name": "e2", "email": "b@x.io", "amount": 2, "date": nd, "tags": []}]],
                None))
    # email backfill: winner null, sources ordered by (date, pos)
    for _ in range(3):
        rid = rand_id(rng)
        win = {"id": rid, "name": "w", "email": None, "amount": 5,
               "date": "2025-12-01", "tags": ["w"]}
        s1 = {"id": rid, "name": "s1", "email": "first@e.io", "amount": 1,
              "date": "2025-03-01", "tags": []}
        s2 = {"id": rid, "name": "s2", "email": "second@e.io", "amount": 2,
              "date": rng.choice(["2025-03-01", "2025-05-01"]), "tags": []}
        allnull = {"id": rid, "name": "n", "email": None, "amount": 3,
                   "date": "2025-11-30", "tags": []}
        scn.append(("backfill", "ok", [[s1, s2], [allnull, win]], None))
    # --since: filters before grouping (winners, unions, backfill all change)
    for _ in range(3):
        pool = ["s%d" % i for i in range(rng.randint(2, 4))]
        files = [[make_record(rng, pool, dates) for _ in range(rng.randint(2, 5))]
                 for _ in range(2)]
        scn.append(("since", "ok", files, rng.choice(dates)))
    scn.append(("since_boundary", "ok",
                [[{"id": "b1", "name": "on", "email": None, "amount": 1,
                   "date": "2025-02-01", "tags": ["keep"]},
                  {"id": "b1", "name": "before", "email": "x@y.io", "amount": 2,
                   "date": "2025-01-31", "tags": ["drop"]}]],
                "2025-02-01"))
    # malformed records are skipped silently (and consume no position)
    for _ in range(3):
        pool = ["v%d" % i for i in range(2)]
        f1 = [make_record(rng, pool, dates) for _ in range(2)]
        f1.insert(rng.randint(0, 2), malformed_record(rng))
        f2 = [make_record(rng, pool, dates) for _ in range(2)]
        f2.insert(rng.randint(0, 2), malformed_record(rng))
        scn.append(("skip_malformed", "ok", [f1, f2], None))
    # a line that is not a JSON object is fatal
    scn.append(("fatal_nonobject", "fatal", None, None))
    scn.append(("usage_nofiles", "usage_nofiles", None, None))
    scn.append(("usage_badsince", "usage_badsince", None, None))
    for _ in range(4):
        pool = ["k%d" % i for i in range(rng.randint(2, 5))]
        files = [[make_record(rng, pool, dates) for _ in range(rng.randint(1, 4))]
                 for _ in range(3)]
        scn.append(("three", "ok", files, None))
    scn.append(("perf_500k", "perf", None, None))
    return scn


def perf_dedup_files(rng, td):
    """Two files totalling 500k records, ~60k ids; returns (paths, seq)."""
    n_total, _budget = PERF_DEDUP
    dates = ["2024-%02d-%02d" % (m, d) for m in range(1, 13) for d in (3, 14, 27)]
    doms = ["a.io", "b.net"]
    paths = []
    seq = []
    per_file = n_total // 2
    for fi in range(2):
        p = os.path.join(td, "perf%d.jsonl" % fi)
        with open(p, "w", encoding="utf-8") as fh:
            for i in range(per_file):
                rid = "p%05d" % ((i * 7 + fi * 13) % 60000)
                email = None if i % 3 == 0 else ("u%d@%s" % (i % 50, doms[i % 2]))
                rec = {"id": rid, "name": "N%d" % (i % 1000), "email": email,
                       "amount": (i % 5000) + (0.5 if i % 2 else 0),
                       "date": dates[i % len(dates)], "tags": ["t%d" % (i % 9)]}
                fh.write(json.dumps(rec) + "\n")
                seq.append(rec)
        paths.append(p)
    return paths, seq


def report_scenarios(rng):
    scn = []
    scn.append(("empty", "ok", []))
    for _ in range(2):
        scn.append(("single", "ok", [make_record(rng)]))
    for _ in range(5):
        scn.append(("many", "ok", [make_record(rng) for _ in range(rng.randint(2, 12))]))
    # forced median parities and p90 rank edges
    scn.append(("median_odd", "ok", [make_record(rng) for _ in range(7)]))
    scn.append(("median_even", "ok", [make_record(rng) for _ in range(8)]))
    scn.append(("p90_n10", "ok", [make_record(rng) for _ in range(10)]))
    scn.append(("p90_n11", "ok", [make_record(rng) for _ in range(11)]))
    # forced top_spenders ties (equal amounts -> id tie-break), >5 records
    for _ in range(3):
        amt = rng.choice([100, 250.5, 42])
        recs = [dict(make_record(rng), amount=amt) for _ in range(rng.randint(4, 7))]
        recs += [make_record(rng) for _ in range(rng.randint(1, 4))]
        scn.append(("ties", "ok", recs))
    # heavy tag overlap
    for _ in range(2):
        recs = []
        for _ in range(rng.randint(3, 7)):
            r = make_record(rng)
            r["tags"] = sorted(set(rng.choice(["p", "q", "r"]) for _ in range(rng.randint(1, 3))))
            recs.append(r)
        scn.append(("tags", "ok", recs))
    # month grouping and domain counting emphasis
    months = ["2025-01-10", "2025-01-20", "2025-02-05", "2024-12-31"]
    for _ in range(2):
        recs = [make_record(rng, date_pool=months, domain_pool=["dup.io", "solo.net"])
                for _ in range(rng.randint(4, 9))]
        scn.append(("months_domains", "ok", recs))
    scn.append(("perf_60k", "perf", None))
    return scn


def perf_report_records(rng):
    n, _budget = PERF_REPORT
    dates = ["2025-%02d-15" % m for m in range(1, 13)]
    doms = ["x.io", "y.net", "z.org"]
    recs = []
    for i in range(n):
        recs.append({
            "id": "r%06d" % i,
            "name": "N",
            "email": None if i % 4 == 0 else ("u@%s" % doms[i % 3]),
            "amount": (i % 2000) + (0.25 if i % 2 else 0),
            "date": dates[i % 12],
            "tags": ["t%d" % (i % 6)],
        })
    return recs


# --------------------------------------------------------------- run helpers
def parse_jsonl(text):
    return [json.loads(ln) for ln in text.splitlines() if ln.strip()]


def run_normalizer(workdir, csv_text, timeout=RUN_TIMEOUT):
    """Returns (rc, records|None, elapsed)."""
    script = os.path.join(workdir, "normalizer", "normalize.py")
    with tempfile.TemporaryDirectory() as td:
        ip = os.path.join(td, "in.csv")
        op = os.path.join(td, "out.jsonl")
        with open(ip, "w", encoding="utf-8") as fh:
            fh.write(csv_text)
        t0 = time.monotonic()
        try:
            p = subprocess.run([PYTHON, script, ip, op], cwd=workdir,
                               capture_output=True, text=True, timeout=timeout)
        except (subprocess.TimeoutExpired, OSError):
            return None, None, time.monotonic() - t0
        elapsed = time.monotonic() - t0
        if p.returncode != 0 or not os.path.exists(op):
            return p.returncode, None, elapsed
        try:
            with open(op, encoding="utf-8") as fh:
                return p.returncode, parse_jsonl(fh.read()), elapsed
        except (json.JSONDecodeError, OSError):
            return p.returncode, None, elapsed


def run_quarantine(workdir, records, as_of, timeout=RUN_TIMEOUT):
    """Returns (rc, clean|None, quar|None)."""
    script = os.path.join(workdir, "quarantine", "quarantine.py")
    with tempfile.TemporaryDirectory() as td:
        ip = os.path.join(td, "in.jsonl")
        cp = os.path.join(td, "clean.jsonl")
        qp = os.path.join(td, "quar.jsonl")
        with open(ip, "w", encoding="utf-8") as fh:
            for r in records:
                fh.write(json.dumps(r) + "\n")
        try:
            p = subprocess.run([PYTHON, script, "--as-of", as_of, ip, cp, qp],
                               cwd=workdir, capture_output=True, text=True, timeout=timeout)
        except (subprocess.TimeoutExpired, OSError):
            return None, None, None
        clean = quar = None
        try:
            if os.path.exists(cp):
                with open(cp, encoding="utf-8") as fh:
                    clean = parse_jsonl(fh.read())
            if os.path.exists(qp):
                with open(qp, encoding="utf-8") as fh:
                    quar = parse_jsonl(fh.read())
        except (json.JSONDecodeError, OSError):
            clean = quar = None
        return p.returncode, clean, quar


def dedup_bin(workdir):
    return os.path.join(workdir, "dedup", "target", "release", "dedup")


def run_dedup_argv(workdir, argv, timeout=RUN_TIMEOUT):
    """Returns (rc, records|None, elapsed)."""
    binpath = dedup_bin(workdir)
    if not os.path.exists(binpath):
        return None, None, 0.0
    t0 = time.monotonic()
    try:
        p = subprocess.run([binpath] + argv, capture_output=True, text=True,
                           timeout=timeout)
    except (subprocess.TimeoutExpired, OSError):
        return None, None, time.monotonic() - t0
    elapsed = time.monotonic() - t0
    if p.returncode != 0:
        return p.returncode, None, elapsed
    try:
        return p.returncode, parse_jsonl(p.stdout), elapsed
    except json.JSONDecodeError:
        return p.returncode, None, elapsed


def run_dedup(workdir, files, since=None, timeout=RUN_TIMEOUT):
    with tempfile.TemporaryDirectory() as td:
        paths = []
        for i, recs in enumerate(files):
            p = os.path.join(td, "f%02d.jsonl" % i)
            with open(p, "w", encoding="utf-8") as fh:
                for r in recs:
                    fh.write(json.dumps(r) + "\n")
            paths.append(p)
        argv = (["--since", since] if since else []) + paths
        return run_dedup_argv(workdir, argv, timeout=timeout)


def run_report(workdir, records, timeout=RUN_TIMEOUT):
    """Returns (rc, report|None, elapsed)."""
    script = os.path.join(workdir, "report", "report.sh")
    with tempfile.TemporaryDirectory() as td:
        ip = os.path.join(td, "merged.jsonl")
        with open(ip, "w", encoding="utf-8") as fh:
            for r in records:
                fh.write(json.dumps(r) + "\n")
        t0 = time.monotonic()
        try:
            p = subprocess.run(["bash", script, ip], cwd=workdir,
                               capture_output=True, text=True, timeout=timeout)
        except (subprocess.TimeoutExpired, OSError):
            return None, None, time.monotonic() - t0
        elapsed = time.monotonic() - t0
        if p.returncode != 0:
            return p.returncode, None, elapsed
        try:
            return p.returncode, json.loads(p.stdout), elapsed
        except json.JSONDecodeError:
            return p.returncode, None, elapsed


# ------------------------------------------------------------------- scoring
def score_normalizer(workdir, rng):
    scn = normalizer_scenarios(rng)
    pos_pass = neg_pass = 0
    fails = []
    perf_detail = {}
    for label, kind, csv_text in scn:
        if kind == "perf":
            n_rows, budget = PERF_NORMALIZER
            text = perf_normalizer_csv(rng)
            want = oracle.normalize_csv_text(text)
            rc, got, elapsed = run_normalizer(workdir, text, timeout=budget)
            ok = rc == 0 and got is not None and oracle.json_equal(got, want)
            perf_detail = {"rows": n_rows, "budget_s": budget,
                           "elapsed_s": round(elapsed, 2), "ok": bool(ok)}
            if ok:
                pos_pass += 1
            elif len(fails) < 3:
                fails.append({"rule": label, "perf": perf_detail})
            continue
        if kind == "malformed":
            rc, got, _ = run_normalizer(workdir, csv_text)
            if rc is not None and rc != 0:
                neg_pass += 1
            elif len(fails) < 3:
                fails.append({"rule": label, "want": "non-zero exit", "got_rc": rc})
            continue
        want = oracle.normalize_csv_text(csv_text)
        rc, got, _ = run_normalizer(workdir, csv_text)
        if rc == 0 and got is not None and oracle.json_equal(got, want):
            pos_pass += 1
        elif len(fails) < 3:
            fails.append({"rule": label, "want": want, "got": got, "rc": rc})
    # Negative (non-zero-exit) checks only count once the positive path works:
    # a do-nothing stub that fails everything must not collect them.
    passed = pos_pass + (neg_pass if pos_pass > 0 else 0)
    return passed, len(scn), fails, perf_detail


def score_quarantine(workdir, rng):
    scn = quarantine_scenarios(rng)
    pos_pass = neg_pass = 0
    fails = []
    for label, kind, records, as_of in scn:
        rc, clean, quar = run_quarantine(workdir, records, as_of)
        if kind == "usage":
            if rc is not None and rc != 0:
                neg_pass += 1
            elif len(fails) < 3:
                fails.append({"case": label, "want": "non-zero exit", "got_rc": rc})
            continue
        want_clean, want_quar = oracle.quarantine_records(records, as_of)
        ok = (rc == 0 and clean is not None and quar is not None
              and oracle.json_equal(clean, want_clean)
              and oracle.json_equal(quar, want_quar))
        if ok:
            pos_pass += 1
        elif len(fails) < 3:
            fails.append({"case": label, "as_of": as_of, "rc": rc,
                          "want": {"clean": want_clean, "quarantine": want_quar},
                          "got": {"clean": clean, "quarantine": quar}})
    passed = pos_pass + (neg_pass if pos_pass > 0 else 0)
    return passed, len(scn), fails


def score_dedup(workdir, rng):
    scn = dedup_scenarios(rng)
    pos_pass = neg_pass = 0
    fails = []
    have_bin = os.path.exists(dedup_bin(workdir))
    perf_detail = {}
    for label, kind, files, since in scn:
        if kind == "perf":
            n_total, budget = PERF_DEDUP
            with tempfile.TemporaryDirectory() as td:
                paths, seq = perf_dedup_files(rng, td)
                want = oracle.dedup_records(seq)
                rc, got, elapsed = run_dedup_argv(workdir, paths, timeout=budget)
            ok = rc == 0 and got is not None and oracle.json_equal(got, want)
            perf_detail = {"lines": n_total, "budget_s": budget,
                           "elapsed_s": round(elapsed, 2), "ok": bool(ok)}
            if ok:
                pos_pass += 1
            elif len(fails) < 3:
                fails.append({"case": label, "perf": perf_detail})
            continue
        if kind == "usage_nofiles":
            rc, _, _ = run_dedup_argv(workdir, [])
            if rc is not None and rc != 0:
                neg_pass += 1
            elif len(fails) < 3:
                fails.append({"case": label, "want": "non-zero exit", "got_rc": rc})
            continue
        if kind == "usage_badsince":
            with tempfile.TemporaryDirectory() as td:
                p = os.path.join(td, "x.jsonl")
                open(p, "w").close()
                rc, _, _ = run_dedup_argv(workdir, ["--since", "2025/01/01", p])
            if rc is not None and rc != 0:
                neg_pass += 1
            elif len(fails) < 3:
                fails.append({"case": label, "want": "non-zero exit", "got_rc": rc})
            continue
        if kind == "fatal":
            with tempfile.TemporaryDirectory() as td:
                p = os.path.join(td, "bad.jsonl")
                with open(p, "w", encoding="utf-8") as fh:
                    fh.write(json.dumps(make_record(rng)) + "\n")
                    fh.write("[1, 2, 3]\n")
                rc, _, _ = run_dedup_argv(workdir, [p])
            if rc is not None and rc != 0:
                neg_pass += 1
            elif len(fails) < 3:
                fails.append({"case": label, "want": "non-zero exit", "got_rc": rc})
            continue
        seq = [r for f in files for r in f]
        want = oracle.dedup_records(seq, since=since)
        rc, got, _ = run_dedup(workdir, files, since=since)
        if rc == 0 and got is not None and oracle.json_equal(got, want):
            pos_pass += 1
        elif len(fails) < 3:
            fails.append({"case": label, "since": since, "want": want,
                          "got": got, "rc": rc})
    passed = pos_pass + (neg_pass if pos_pass > 0 else 0)
    return passed, len(scn), fails, have_bin, perf_detail


def score_report(workdir, rng):
    scn = report_scenarios(rng)
    passed = 0
    fails = []
    perf_detail = {}
    for label, kind, records in scn:
        if kind == "perf":
            n, budget = PERF_REPORT
            recs = perf_report_records(rng)
            want = oracle.report_records(recs)
            rc, got, elapsed = run_report(workdir, recs, timeout=budget)
            ok = rc == 0 and got is not None and oracle.report_equal(got, want)
            perf_detail = {"lines": n, "budget_s": budget,
                           "elapsed_s": round(elapsed, 2), "ok": bool(ok)}
            if ok:
                passed += 1
            elif len(fails) < 3:
                fails.append({"case": label, "perf": perf_detail})
            continue
        want = oracle.report_records(records)
        rc, got, _ = run_report(workdir, records)
        if rc == 0 and got is not None and oracle.report_equal(got, want):
            passed += 1
        elif len(fails) < 3:
            fails.append({"case": label, "want": want, "got": got, "rc": rc})
    return passed, len(scn), fails, perf_detail


def score_integration(workdir, rng):
    """Run the real `make pipeline` (with a grader-pinned Makefile) on freshly
    generated CSVs; score the four pipeline stages 1/4 each vs the oracle."""
    detail = {"stages": {}}
    # Pin the Makefile so a tampered one can't fake the wiring.
    if os.path.exists(CANONICAL_MAKEFILE):
        with open(CANONICAL_MAKEFILE) as fh:
            mk = fh.read()
        with open(os.path.join(workdir, "Makefile"), "w") as fh:
            fh.write(mk)
    raw = os.path.join(workdir, ".grade_raw")
    out = os.path.join(workdir, ".grade_out")
    import shutil
    for d in (raw, out):
        if os.path.isdir(d):
            shutil.rmtree(d)
    os.makedirs(raw, exist_ok=True)
    as_of = "%04d-%02d-%02d" % (rng.randint(2024, 2028), rng.randint(1, 12),
                                rng.randint(1, 28))
    detail["as_of"] = as_of
    # Several CSVs with sort-stable names; oracle must mirror sorted glob order.
    names = ["east", "north", "south", "west"][:rng.randint(2, 4)]
    csv_by_name = {}
    for nm in names:
        rows = [valid_cells(rng) for _ in range(rng.randint(3, 7))]
        # sprinkle rejects + quarantine triggers + duplicate ids across files
        if rng.random() < 0.6:
            rows[0]["amount"] = rand_invalid_amount(rng)
        if rng.random() < 0.6:
            rows[-1]["amount"] = rng.choice(["26,000", "0", "30000"])
        if rng.random() < 0.5:
            rows[rng.randrange(len(rows))]["name"] = "Test Unit"
        csv_by_name[nm] = render_csv(rng, rows)
        with open(os.path.join(raw, nm + ".csv"), "w", encoding="utf-8") as fh:
            fh.write(csv_by_name[nm])

    try:
        p = subprocess.run(["make", "pipeline", "RAW=" + raw, "OUT=" + out,
                            "AS_OF=" + as_of],
                           cwd=workdir, capture_output=True, text=True, timeout=MAKE_TIMEOUT)
        detail["make_rc"] = p.returncode
        if p.returncode != 0:
            detail["make_stderr"] = p.stderr[-600:]
    except (subprocess.TimeoutExpired, OSError) as e:
        detail["make_error"] = str(e)[:200]

    # Oracle end-to-end in sorted filename order.
    sorted_names = sorted(names)
    oracle_norm = {nm: oracle.normalize_csv_text(csv_by_name[nm]) for nm in sorted_names}
    oracle_clean, oracle_quar = {}, {}
    for nm in sorted_names:
        oracle_clean[nm], oracle_quar[nm] = oracle.quarantine_records(
            oracle_norm[nm], as_of)
    seq = [r for nm in sorted_names for r in oracle_clean[nm]]
    oracle_merged = oracle.dedup_records(seq)
    oracle_report = oracle.report_records(oracle_merged)

    def read_jsonl(path):
        if not os.path.exists(path):
            return None
        try:
            with open(path, encoding="utf-8") as fh:
                return parse_jsonl(fh.read())
        except (OSError, json.JSONDecodeError):
            return None

    # Stage 1: normalized/*.jsonl per file (fraction of files matching).
    norm_ok = 0
    for nm in sorted_names:
        got = read_jsonl(os.path.join(out, "normalized", nm + ".jsonl"))
        if got is not None and oracle.json_equal(got, oracle_norm[nm]):
            norm_ok += 1
    stage_norm = norm_ok / len(sorted_names)
    detail["stages"]["normalized"] = {"matched_files": norm_ok, "files": len(sorted_names)}

    # Stage 2: clean/ + quarantine/ pairs per file.
    screen_ok = 0
    for nm in sorted_names:
        got_clean = read_jsonl(os.path.join(out, "clean", nm + ".jsonl"))
        got_quar = read_jsonl(os.path.join(out, "quarantine", nm + ".jsonl"))
        if (got_clean is not None and got_quar is not None
                and oracle.json_equal(got_clean, oracle_clean[nm])
                and oracle.json_equal(got_quar, oracle_quar[nm])):
            screen_ok += 1
    stage_screen = screen_ok / len(sorted_names)
    detail["stages"]["screened"] = {"matched_files": screen_ok, "files": len(sorted_names)}

    # Stage 3: merged.jsonl.
    merged_got = read_jsonl(os.path.join(out, "merged.jsonl"))
    stage_merged = 1.0 if (merged_got is not None
                           and oracle.json_equal(merged_got, oracle_merged)) else 0.0
    detail["stages"]["merged"] = bool(stage_merged)

    # Stage 4: report.json.
    report_got = None
    rpath = os.path.join(out, "report.json")
    if os.path.exists(rpath):
        try:
            with open(rpath, encoding="utf-8") as fh:
                report_got = json.load(fh)
        except (OSError, json.JSONDecodeError):
            report_got = None
    stage_report = 1.0 if (report_got is not None
                           and oracle.report_equal(report_got, oracle_report)) else 0.0
    detail["stages"]["report"] = bool(stage_report)

    integration = round((stage_norm + stage_screen + stage_merged + stage_report) / 4.0, 4)
    for d in (raw, out):
        shutil.rmtree(d, ignore_errors=True)
    return integration, detail


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workdir")
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()
    seed = args.seed if args.seed is not None else random.randrange(1, 2**31)

    workdir = os.path.abspath(args.workdir)
    # Independent RNG streams per component so the seed->inputs map is stable
    # even if one battery's size changes.
    rng_n = random.Random(seed ^ 0x11111111)
    rng_q = random.Random(seed ^ 0x55555555)
    rng_d = random.Random(seed ^ 0x22222222)
    rng_r = random.Random(seed ^ 0x33333333)
    rng_i = random.Random(seed ^ 0x44444444)

    n_pass, n_tot, n_fail, n_perf = score_normalizer(workdir, rng_n)
    q_pass, q_tot, q_fail = score_quarantine(workdir, rng_q)
    d_pass, d_tot, d_fail, have_bin, d_perf = score_dedup(workdir, rng_d)
    r_pass, r_tot, r_fail, r_perf = score_report(workdir, rng_r)
    integration, idetail = score_integration(workdir, rng_i)

    comp = {
        "normalizer": round(n_pass / n_tot, 4),
        "quarantine": round(q_pass / q_tot, 4),
        "dedup": round(d_pass / d_tot, 4),
        "report": round(r_pass / r_tot, 4),
    }
    total = round(sum(comp.values()) + integration, 4)
    result = {
        "task": "polyglot-pipeline",
        "seed": seed,
        "component_scores": comp,
        "integration": integration,
        "total": total,
        "max_total": 5.0,
        "details": {
            "normalizer": {"passed": n_pass, "total": n_tot, "perf": n_perf,
                           "fails": n_fail},
            "quarantine": {"passed": q_pass, "total": q_tot, "fails": q_fail},
            "dedup": {"passed": d_pass, "total": d_tot, "built": have_bin,
                      "perf": d_perf, "fails": d_fail},
            "report": {"passed": r_pass, "total": r_tot, "perf": r_perf,
                       "fails": r_fail},
            "integration": idetail,
        },
    }
    print(json.dumps(result, indent=2, default=str))


if __name__ == "__main__":
    main()
