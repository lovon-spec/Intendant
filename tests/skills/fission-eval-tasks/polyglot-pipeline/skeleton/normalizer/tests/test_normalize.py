#!/usr/bin/env python3
"""Starter test for normalizer/normalize.py (see normalizer/SPEC.md)."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

NORMALIZE = Path(__file__).resolve().parents[1] / "normalize.py"

# Header notes: unknown column ("junk") is ignored; "amount" appears twice and
# the LAST occurrence is the one that counts (the first holds decoys).
INPUT_CSV = """\
date,id,junk,tags,amount,name,email,amount
2025-01-15 ,a1,zz,"Red; blue|RED",999,"Lee,   Ann",ANN+News@Example.COM,"($1,234.50)"
01/09/2025, b2 ,,,x,Bo,,-$12
,,,,,,,
2025-03-05,c3,,z,1,Cy,cy@ex.io,"1,23.45"
2025-03-06,bad id!,,x,1,Bad,b@x.io,5
31.12.1990,d4,,VIP|m&m;vip,0,"  Dee  Dee ",x@y.io,0.75
2025-04-01,e5,,solo,1,Eve,eve@nodot,9
2025-04-02,f6,,,1,Fay,+x@a.b,9
1989-12-31,g7,,,1,Old,o@x.io,9
2025-05-01,h8,,,1,Huge,h@x.io,"2,000,000"
2025-05-02,i9,,,1,Neg,n@x.io,(-5)
2025-05-03,j10,,t1|t2|t3|t4|t5|t6|t7|t8|t9|t10|t11,1,Many,m@x.io,7
2035-12-31,k11,,t01;t02;t03;t04;t05;t06;t07;t08;t09;t10,1,Kay,K@EX.io,33
"""

EXPECTED = [
    {"id": "a1", "name": "Lee, Ann", "email": "ann@example.com",
     "amount": -1234.5, "date": "2025-01-15", "tags": ["blue", "red"]},
    {"id": "b2", "name": "Bo", "email": None,
     "amount": -12, "date": "2025-01-09", "tags": []},
    {"id": "d4", "name": "Dee Dee", "email": "x@y.io",
     "amount": 0.75, "date": "1990-12-31", "tags": ["vip"]},
    {"id": "k11", "name": "Kay", "email": "k@ex.io",
     "amount": 33, "date": "2035-12-31",
     "tags": ["t01", "t02", "t03", "t04", "t05", "t06", "t07", "t08", "t09", "t10"]},
]


def run(src_text):
    with tempfile.TemporaryDirectory() as td:
        src = Path(td) / "in.csv"
        dst = Path(td) / "out.jsonl"
        src.write_text(src_text, encoding="utf-8")
        proc = subprocess.run([sys.executable, str(NORMALIZE), str(src), str(dst)],
                              capture_output=True, text=True, timeout=60)
        got = None
        if dst.exists():
            lines = [ln for ln in dst.read_text(encoding="utf-8").splitlines() if ln.strip()]
            got = [json.loads(ln) for ln in lines]
        return proc, got


def main():
    proc, got = run(INPUT_CSV)
    assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr.strip()}"
    assert got == EXPECTED, (
        "output mismatch\n--- got ---\n%s\n--- want ---\n%s"
        % (json.dumps(got, indent=1), json.dumps(EXPECTED, indent=1)))

    # A header missing a required column is a malformed file: non-zero exit.
    proc, _ = run("date,id,tags,amount,name\n2025-01-01,q1,,5,Q\n")
    assert proc.returncode != 0, "missing 'email' column must be a non-zero exit"
    print("normalizer starter test: OK")


if __name__ == "__main__":
    main()
