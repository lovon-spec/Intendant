#!/usr/bin/env python3
"""Reference quarantine stage (agent-facing solution). See quarantine/SPEC.md.
Excluded from agent visibility by the SKILL runner."""
import datetime
import json
import re
import sys

_ISO = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")


def parse_iso(s):
    if not _ISO.match(s):
        return None
    try:
        return datetime.date(int(s[:4]), int(s[5:7]), int(s[8:10]))
    except ValueError:
        return None


def stale_cutoff(as_of):
    target_year = as_of.year - 5
    if as_of.month == 2 and as_of.day == 29:
        try:
            return datetime.date(target_year, 2, 29).isoformat()
        except ValueError:
            return datetime.date(target_year, 2, 28).isoformat()
    return datetime.date(target_year, as_of.month, as_of.day).isoformat()


def reasons_for(rec, as_of_iso, cutoff_iso):
    out = []
    amount = rec["amount"]
    if abs(amount) > 25000:
        out.append("amount_limit")
    if amount == 0:
        out.append("zero_amount")
    if amount > 1000 and rec["email"] is None:
        out.append("missing_contact")
    if "test" in rec["name"].lower():
        out.append("test_data")
    if rec["date"] > as_of_iso:
        out.append("future_date")
    if rec["date"] < cutoff_iso:
        out.append("stale_date")
    return sorted(out)


def main(argv):
    if len(argv) != 6 or argv[1] != "--as-of":
        print("usage: quarantine.py --as-of YYYY-MM-DD INPUT.jsonl CLEAN.jsonl QUAR.jsonl",
              file=sys.stderr)
        return 2
    as_of = parse_iso(argv[2])
    if as_of is None:
        print("--as-of must be a valid YYYY-MM-DD date", file=sys.stderr)
        return 2
    as_of_iso = as_of.isoformat()
    cutoff_iso = stale_cutoff(as_of)
    try:
        fin = open(argv[3], encoding="utf-8")
    except OSError as e:
        print("cannot read input: %s" % e, file=sys.stderr)
        return 1
    with fin, open(argv[4], "w", encoding="utf-8") as fclean, \
            open(argv[5], "w", encoding="utf-8") as fquar:
        for line in fin:
            if not line.strip():
                continue
            rec = json.loads(line)
            reasons = reasons_for(rec, as_of_iso, cutoff_iso)
            if reasons:
                fquar.write(json.dumps({"record": rec, "reasons": reasons}) + "\n")
            else:
                fclean.write(json.dumps(rec) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
