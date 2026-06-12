#!/usr/bin/env python3
"""Reference normalizer (agent-facing solution). See normalizer/SPEC.md.

Excluded from agent visibility by the SKILL runner (lives under reference/,
which is never copied into the agent's workdir)."""
import csv
import datetime
import json
import re
import sys

COLUMNS = ("id", "name", "email", "amount", "date", "tags")
_ID = re.compile(r"^[A-Za-z0-9_-]{1,32}$")
_AMOUNT = re.compile(r"^[0-9]+(\.[0-9]{1,2})?$")
_INT_COMMAS = re.compile(r"^[0-9]{1,3}(,[0-9]{3})+$")
_ISO = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
_US = re.compile(r"^[0-9]{2}/[0-9]{2}/[0-9]{4}$")
_EU = re.compile(r"^[0-9]{2}\.[0-9]{2}\.[0-9]{4}$")
_TAG = re.compile(r"^[a-z0-9_]+$")
_WS = re.compile(r"\s+")

DATE_MIN, DATE_MAX = "1990-01-01", "2035-12-31"


def conv_amount(s):
    neg = False
    if len(s) >= 2 and s.startswith("(") and s.endswith(")"):
        neg = True
        s = s[1:-1]
    if s.startswith("-"):
        if neg:
            return None
        neg = True
        s = s[1:]
    if s.startswith("$"):
        s = s[1:]
    if "," in s:
        head, dot, tail = s.partition(".")
        if "," in tail:
            return None
        if not _INT_COMMAS.match(head):
            return None
        s = head.replace(",", "") + dot + tail
    if not _AMOUNT.match(s):
        return None
    val = float(s)
    if neg:
        val = -val
    if abs(val) > 1000000:
        return None
    return val


def conv_date(s):
    if _ISO.match(s):
        y, m, d = int(s[:4]), int(s[5:7]), int(s[8:10])
    elif _US.match(s):
        m, d, y = int(s[:2]), int(s[3:5]), int(s[6:10])
    elif _EU.match(s):
        d, m, y = int(s[:2]), int(s[3:5]), int(s[6:10])
    else:
        return None
    try:
        iso = datetime.date(y, m, d).isoformat()
    except ValueError:
        return None
    if not (DATE_MIN <= iso <= DATE_MAX):
        return None
    return iso


def conv_email(s):
    if not s:
        return True, None
    s = s.lower()
    if s.count("@") != 1:
        return False, None
    local, _, domain = s.partition("@")
    if not local or not domain:
        return False, None
    if "+" in local:
        local = local.split("+", 1)[0]
        if not local:
            return False, None
    if "." not in domain:
        return False, None
    if any(not label for label in domain.split(".")):
        return False, None
    return True, local + "@" + domain


def conv_tags(s):
    out = set()
    for piece in re.split(r"[;|]", s):
        piece = piece.strip().lower()
        if piece and _TAG.match(piece):
            out.add(piece)
    if len(out) > 10:
        return None
    return sorted(out)


def normalize(reader, sink):
    rows = iter(reader)
    try:
        header = [h.strip().lower() for h in next(rows)]
    except StopIteration:
        return False
    pos = {}
    for i, h in enumerate(header):
        if h in COLUMNS:
            pos[h] = i  # later duplicates overwrite: LAST occurrence wins
    if any(c not in pos for c in COLUMNS):
        return False

    def get(row, name):
        i = pos[name]
        return row[i] if i < len(row) else ""

    for row in rows:
        fields = {c: get(row, c) for c in COLUMNS}
        if all(not v.strip() for v in fields.values()):
            continue
        fields = {c: v.strip() for c, v in fields.items()}
        if not _ID.match(fields["id"]):
            continue
        name = _WS.sub(" ", fields["name"])
        ok, email = conv_email(fields["email"])
        if not ok:
            continue
        amount = conv_amount(fields["amount"])
        if amount is None:
            continue
        date = conv_date(fields["date"])
        if date is None:
            continue
        tags = conv_tags(fields["tags"])
        if tags is None:
            continue
        rec = {
            "id": fields["id"],
            "name": name,
            "email": email,
            "amount": amount,
            "date": date,
            "tags": tags,
        }
        sink.write(json.dumps(rec) + "\n")
    return True


def main(argv):
    if len(argv) != 3:
        print("usage: normalize.py INPUT.csv OUTPUT.jsonl", file=sys.stderr)
        return 2
    try:
        fin = open(argv[1], newline="", encoding="utf-8")
    except OSError as e:
        print("cannot read input: %s" % e, file=sys.stderr)
        return 1
    with fin:
        rows = csv.reader(fin)
        try:
            fout = open(argv[2], "w", encoding="utf-8")
        except OSError as e:
            print("cannot write output: %s" % e, file=sys.stderr)
            return 1
        with fout:
            ok = normalize(rows, fout)
    if not ok:
        print("malformed header: need columns %s" % ", ".join(COLUMNS), file=sys.stderr)
        import os
        os.unlink(argv[2])
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
