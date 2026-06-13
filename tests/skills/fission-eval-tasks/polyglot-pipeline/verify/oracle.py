#!/usr/bin/env python3
"""Independent reference implementation of the polyglot-pipeline specs, used by
the grader as the oracle. This is deliberately a SECOND implementation (the
agent-facing reference/ solution is a third); when two independent
implementations agree on randomly generated inputs, both are almost certainly
correct. Nothing here ever runs the agent's code — it only defines truth.

Pure stdlib. See each component's SPEC.md for the contract these encode.
"""
import csv
import io
import re
from datetime import date as _date

AMOUNT_RE = re.compile(r"^[0-9]+(\.[0-9]{1,2})?$")
COMMA_INT_RE = re.compile(r"^[0-9]{1,3}(,[0-9]{3})+$")
ID_RE = re.compile(r"^[A-Za-z0-9_-]{1,32}$")
ISO_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
US_RE = re.compile(r"^[0-9]{2}/[0-9]{2}/[0-9]{4}$")
EU_RE = re.compile(r"^[0-9]{2}\.[0-9]{2}\.[0-9]{4}$")
TAG_RE = re.compile(r"^[a-z0-9_]+$")
COLUMNS = ["id", "name", "email", "amount", "date", "tags"]
AMOUNT_CAP = 1000000
DATE_LO, DATE_HI = "1990-01-01", "2035-12-31"


# ---------------------------------------------------------------- normalizer
def parse_amount(raw):
    """Return a number, or None to reject. Mirrors normalizer SPEC step 6."""
    s = raw
    neg = False
    if len(s) >= 2 and s[0] == "(" and s[-1] == ")":
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
        if "." in s:
            intpart, frac = s.split(".", 1)
            if "," in frac:
                return None
        else:
            intpart = s
        if not COMMA_INT_RE.match(intpart):
            return None
        s = s.replace(",", "")
    if not AMOUNT_RE.match(s):
        return None
    val = float(s)
    if neg:
        val = -val
    if abs(val) > AMOUNT_CAP:
        return None
    # Collapse -0.0 to 0.0 and integral floats stay numerically equal to ints.
    return val + 0.0


def parse_date(raw):
    """Return 'YYYY-MM-DD' or None to reject. Mirrors SPEC step 7."""
    if ISO_RE.match(raw):
        y, m, d = int(raw[0:4]), int(raw[5:7]), int(raw[8:10])
    elif US_RE.match(raw):
        m, d, y = int(raw[0:2]), int(raw[3:5]), int(raw[6:10])
    elif EU_RE.match(raw):
        d, m, y = int(raw[0:2]), int(raw[3:5]), int(raw[6:10])
    else:
        return None
    try:
        iso = _date(y, m, d).isoformat()
    except ValueError:
        return None
    if iso < DATE_LO or iso > DATE_HI:
        return None
    return iso


def parse_email(raw):
    """Return (ok, value). value is None for empty (-> JSON null)."""
    if raw == "":
        return True, None
    low = raw.lower()
    at = low.find("@")
    if low.count("@") != 1 or at <= 0 or at >= len(low) - 1:
        return False, None
    local, domain = low[:at], low[at + 1:]
    plus = local.find("+")
    if plus != -1:
        local = local[:plus]
    if local == "":
        return False, None
    labels = domain.split(".")
    if len(labels) < 2 or any(lab == "" for lab in labels):
        return False, None
    return True, "%s@%s" % (local, domain)


def parse_tags(raw):
    """Return sorted distinct valid tags, or None when the row must be
    rejected (more than 10 distinct valid tags). Mirrors SPEC step 8."""
    parts = re.split(r"[;|]", raw)
    keep = set()
    for p in parts:
        p = p.strip().lower()
        if p and TAG_RE.match(p):
            keep.add(p)
    if len(keep) > 10:
        return None
    return sorted(keep)


def collapse_ws(s):
    return re.sub(r"\s+", " ", s)


def map_header(header_cells):
    """Return {column: index} honoring last-duplicate-wins, or None when a
    required column is missing."""
    idx = {}
    for i, h in enumerate(header_cells):
        h = h.strip().lower()
        if h in COLUMNS:
            idx[h] = i
    if any(c not in idx for c in COLUMNS):
        return None
    return idx


def normalize_rows(rows):
    """rows: list of header+data lists already parsed from CSV. Returns the
    list of accepted record dicts in input order, or None for a malformed
    header (the tool must exit non-zero)."""
    if not rows:
        return None
    idx = map_header(rows[0])
    if idx is None:
        return None
    out = []
    for raw_row in rows[1:]:
        def cell(name):
            i = idx[name]
            if i >= len(raw_row):
                return ""
            return raw_row[i]

        cells = {name: cell(name) for name in COLUMNS}
        # 1. blank row (raw, pre-trim)
        if all(v.strip() == "" for v in cells.values()):
            continue
        # 2. trim
        cells = {k: v.strip() for k, v in cells.items()}
        # 3. id
        if not ID_RE.match(cells["id"]):
            continue
        # 5. email
        ok, email = parse_email(cells["email"])
        if not ok:
            continue
        # 6. amount
        amount = parse_amount(cells["amount"])
        if amount is None:
            continue
        # 7. date
        d = parse_date(cells["date"])
        if d is None:
            continue
        # 8. tags
        tags = parse_tags(cells["tags"])
        if tags is None:
            continue
        out.append({
            "id": cells["id"],
            "name": collapse_ws(cells["name"]),
            "email": email,
            "amount": amount,
            "date": d,
            "tags": tags,
        })
    return out


def normalize_csv_text(text):
    rows = list(csv.reader(io.StringIO(text)))
    return normalize_rows(rows)


# ---------------------------------------------------------------- quarantine
def quarantine_cutoff(as_of_iso):
    """as_of shifted back 5 years; Feb 29 falls back to Feb 28 when the target
    year is not a leap year."""
    y, m, d = int(as_of_iso[0:4]), int(as_of_iso[5:7]), int(as_of_iso[8:10])
    ty = y - 5
    if m == 2 and d == 29:
        try:
            return _date(ty, 2, 29).isoformat()
        except ValueError:
            return _date(ty, 2, 28).isoformat()
    return _date(ty, m, d).isoformat()


def quarantine_reasons(rec, as_of_iso):
    cutoff = quarantine_cutoff(as_of_iso)
    reasons = set()
    if abs(rec["amount"]) > 25000:
        reasons.add("amount_limit")
    if rec["amount"] == 0:
        reasons.add("zero_amount")
    if rec["amount"] > 1000 and rec["email"] is None:
        reasons.add("missing_contact")
    if "test" in rec["name"].lower():
        reasons.add("test_data")
    if rec["date"] > as_of_iso:
        reasons.add("future_date")
    if rec["date"] < cutoff:
        reasons.add("stale_date")
    return sorted(reasons)


def quarantine_records(records, as_of_iso):
    """Returns (clean_records, quarantine_entries) in input order."""
    clean, quar = [], []
    for rec in records:
        reasons = quarantine_reasons(rec, as_of_iso)
        if reasons:
            quar.append({"record": rec, "reasons": reasons})
        else:
            clean.append(rec)
    return clean, quar


# --------------------------------------------------------------------- dedup
def record_conforms(rec):
    """Mirrors dedup/SPEC.md 'Record validation'."""
    if not isinstance(rec, dict):
        return False
    if not isinstance(rec.get("id"), str) or rec["id"] == "":
        return False
    if not isinstance(rec.get("name"), str):
        return False
    if not (rec.get("email") is None or isinstance(rec.get("email"), str)):
        return False
    amt = rec.get("amount")
    if isinstance(amt, bool) or not isinstance(amt, (int, float)):
        return False
    if not isinstance(rec.get("date"), str) or not ISO_RE.match(rec["date"]):
        return False
    tags = rec.get("tags")
    if not isinstance(tags, list) or not all(isinstance(t, str) for t in tags):
        return False
    return True


def dedup_records(seq, since=None):
    """seq: list of parsed JSON values in global order (files in arg order,
    lines in file order). Non-conforming records are skipped; `since` filters
    by date before positions are assigned. Returns merged list sorted by id.
    Mirrors dedup/SPEC.md."""
    survivors = []
    for rec in seq:
        if not record_conforms(rec):
            continue
        if since is not None and rec["date"] < since:
            continue
        survivors.append(rec)
    groups = {}  # id -> list of (position, record)
    for pos, rec in enumerate(survivors):
        groups.setdefault(rec["id"], []).append((pos, rec))
    out = []
    for rid, members in groups.items():
        # winner: (date, email-non-null, position) lexicographic max.
        winner = max(members,
                     key=lambda pr: (pr[1]["date"], pr[1]["email"] is not None, pr[0]))[1]
        union = set()
        for _pos, rec in members:
            union.update(rec.get("tags", []))
        email = winner["email"]
        if email is None:
            with_email = [(pr[1]["date"], pr[0], pr[1]["email"])
                          for pr in members if pr[1]["email"] is not None]
            if with_email:
                email = max(with_email)[2]
        merged = dict(winner)
        merged["email"] = email
        merged["tags"] = sorted(union)
        out.append(merged)
    out.sort(key=lambda r: r["id"])
    return out


# -------------------------------------------------------------------- report
def _median(sorted_amounts):
    n = len(sorted_amounts)
    if n == 0:
        return None
    if n % 2 == 1:
        return sorted_amounts[(n - 1) // 2]
    return (sorted_amounts[n // 2 - 1] + sorted_amounts[n // 2]) / 2.0


def _p90(sorted_amounts):
    n = len(sorted_amounts)
    if n == 0:
        return None
    rank = (9 * n + 9) // 10  # ceil(0.9 * n) for integer n
    return sorted_amounts[rank - 1]


def report_records(records):
    """Mirrors report/SPEC.md."""
    count = len(records)
    total = round(sum(r["amount"] for r in records) + 0.0, 2)
    amounts = sorted(r["amount"] for r in records)
    by_tag = {}
    for r in records:
        for t in set(r.get("tags", [])):
            by_tag[t] = by_tag.get(t, 0) + 1
    by_month = {}
    for r in records:
        m = r["date"][0:7]
        slot = by_month.setdefault(m, {"count": 0, "total": 0.0})
        slot["count"] += 1
        slot["total"] += r["amount"]
    for slot in by_month.values():
        slot["total"] = round(slot["total"] + 0.0, 2)
    domains = {}
    for r in records:
        if r.get("email") is not None:
            dom = r["email"].split("@", 1)[1]
            domains[dom] = domains.get(dom, 0) + 1
    ranked = sorted(records, key=lambda r: (-r["amount"], r["id"]))
    top = [{"id": r["id"], "amount": r["amount"]} for r in ranked[:5]]
    return {
        "count": count,
        "total_amount": total,
        "median_amount": _median(amounts),
        "p90_amount": _p90(amounts),
        "by_tag": by_tag,
        "by_month": by_month,
        "email_domains": domains,
        "top_spenders": top,
    }


# --------------------------------------------------------- comparison helpers
def _num_close(a, b, tol=1e-6):
    return isinstance(a, (int, float)) and isinstance(b, (int, float)) \
        and not isinstance(a, bool) and not isinstance(b, bool) \
        and abs(a - b) <= tol


def json_equal(a, b, tol=1e-6):
    """Structural JSON equality with numeric tolerance (so 42 == 42.0 and
    float dust is ignored). Lists are order-sensitive (the specs fix order)."""
    if _num_close(a, b, tol):
        return True
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    if isinstance(a, dict) and isinstance(b, dict):
        if set(a) != set(b):
            return False
        return all(json_equal(a[k], b[k], tol) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(json_equal(x, y, tol) for x, y in zip(a, b))
    return a == b


def _money_equal(got, want):
    """2-dp money compare: both rounded to 2 dp, then equal."""
    try:
        return round(float(got) + 0.0, 2) == round(float(want) + 0.0, 2)
    except (TypeError, ValueError):
        return False


def _opt_num_equal(got, want, tol=1e-6):
    if want is None or got is None:
        return got is None and want is None
    return _num_close(got, want, tol)


def report_equal(got, want):
    """report-specific compare: total_amount and by_month totals rounded to
    2 dp; median/p90 with tolerance (or both null); everything else via
    json_equal."""
    if not isinstance(got, dict) or set(got) != set(want):
        return False
    if got.get("count") != want.get("count"):
        return False
    if not _money_equal(got.get("total_amount"), want.get("total_amount")):
        return False
    if not _opt_num_equal(got.get("median_amount"), want.get("median_amount")):
        return False
    if not _opt_num_equal(got.get("p90_amount"), want.get("p90_amount")):
        return False
    if not json_equal(got.get("by_tag"), want.get("by_tag")):
        return False
    gm, wm = got.get("by_month"), want.get("by_month")
    if not isinstance(gm, dict) or set(gm) != set(wm):
        return False
    for month, wslot in wm.items():
        gslot = gm[month]
        if not isinstance(gslot, dict) or set(gslot) != {"count", "total"}:
            return False
        if gslot.get("count") != wslot["count"]:
            return False
        if not _money_equal(gslot.get("total"), wslot["total"]):
            return False
    if not json_equal(got.get("email_domains"), want.get("email_domains")):
        return False
    if not json_equal(got.get("top_spenders"), want.get("top_spenders")):
        return False
    return True
