#!/usr/bin/env python3
"""Starter test for quarantine/quarantine.py (see quarantine/SPEC.md)."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

QUARANTINE = Path(__file__).resolve().parents[1] / "quarantine.py"


def rec(rid, amount, date, email="e@x.io", name="Ann", tags=()):
    return {"id": rid, "name": name, "email": email, "amount": amount,
            "date": date, "tags": sorted(tags)}


# as-of 2026-06-01 -> stale cutoff 2021-06-01.
RECORDS = [
    rec("r1", 100, "2024-01-01"),                                   # clean
    rec("r2", 25000.01, "2024-01-02"),                              # amount_limit
    rec("r3", 25000, "2024-01-03"),                                 # boundary: clean
    rec("r4", 0, "2024-01-04"),                                     # zero_amount
    rec("r5", 1000.01, "2024-01-05", email=None),                   # missing_contact
    rec("r6", 1000, "2024-01-06", email=None),                      # boundary: clean
    rec("r7", 10, "2024-01-07", name="Latest Tester"),              # test_data ("latest")
    rec("r8", 10, "2026-06-02"),                                    # future_date
    rec("r9", 10, "2026-06-01"),                                    # boundary: clean
    rec("r10", 10, "2021-05-31"),                                   # stale_date
    rec("r11", 10, "2021-06-01"),                                   # boundary: clean
    rec("r12", 30000, "2027-01-01", email=None, name="test co"),    # multi-reason
]
EXPECT_CLEAN = ["r1", "r3", "r6", "r9", "r11"]
EXPECT_QUAR = {
    "r2": ["amount_limit"],
    "r4": ["zero_amount"],
    "r5": ["missing_contact"],
    "r7": ["test_data"],
    "r8": ["future_date"],
    "r10": ["stale_date"],
    "r12": ["amount_limit", "future_date", "missing_contact", "test_data"],
}


def run(records, as_of):
    with tempfile.TemporaryDirectory() as td:
        src = Path(td) / "in.jsonl"
        clean = Path(td) / "clean.jsonl"
        quar = Path(td) / "quar.jsonl"
        src.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")
        proc = subprocess.run(
            [sys.executable, str(QUARANTINE), "--as-of", as_of, str(src), str(clean), str(quar)],
            capture_output=True, text=True, timeout=60)
        parse = lambda p: [json.loads(ln) for ln in p.read_text().splitlines() if ln.strip()] \
            if p.exists() else None
        return proc, parse(clean), parse(quar)


def main():
    proc, clean, quar = run(RECORDS, "2026-06-01")
    assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr.strip()}"
    assert clean is not None and quar is not None, "both output files must be written"
    assert [r["id"] for r in clean] == EXPECT_CLEAN, [r["id"] for r in clean]
    assert [q["record"]["id"] for q in quar] == list(EXPECT_QUAR), \
        [q["record"]["id"] for q in quar]
    for q in quar:
        rid = q["record"]["id"]
        assert q["reasons"] == EXPECT_QUAR[rid], (rid, q["reasons"])
        assert q["record"] == RECORDS[[r["id"] for r in RECORDS].index(rid)], \
            "quarantined record must be unchanged"

    # Leap-day as-of: 2024-02-29 minus 5 years -> cutoff 2019-02-28.
    proc, clean, quar = run([rec("s1", 5, "2019-02-27"), rec("s2", 5, "2019-02-28")],
                            "2024-02-29")
    assert proc.returncode == 0, proc.stderr
    assert [q["record"]["id"] for q in quar] == ["s1"], quar
    assert [r["id"] for r in clean] == ["s2"], clean

    # Malformed --as-of is a usage error.
    proc, _, _ = run([rec("t1", 5, "2024-01-01")], "06/01/2026")
    assert proc.returncode != 0, "--as-of must be YYYY-MM-DD"
    print("quarantine starter test: OK")


if __name__ == "__main__":
    main()
