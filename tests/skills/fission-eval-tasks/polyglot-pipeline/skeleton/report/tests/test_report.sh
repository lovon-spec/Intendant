#!/usr/bin/env bash
# Starter test for report/report.sh (see report/SPEC.md).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPORT="$(dirname "$HERE")/report.sh"

TD=$(mktemp -d)
trap 'rm -rf "$TD"' EXIT

cat > "$TD/merged.jsonl" <<'EOF'
{"id":"a","name":"A","email":null,"amount":100,"date":"2025-01-15","tags":["vip","eu"]}
{"id":"b","name":"B","email":"x@ex.io","amount":100,"date":"2025-01-20","tags":["eu"]}
{"id":"c","name":"C","email":"y@ex.io","amount":50.5,"date":"2025-02-03","tags":[]}
{"id":"d","name":"D","email":"z@other.net","amount":75,"date":"2025-02-10","tags":["vip"]}
{"id":"e","name":"E","email":"q@ex.io","amount":-25.25,"date":"2025-03-01","tags":["eu"]}
{"id":"f","name":"F","email":null,"amount":10,"date":"2025-03-05","tags":[]}
EOF

# count 6; total 310.25; sorted amounts [-25.25,10,50.5,75,100,100]:
# median (50.5+75)/2 = 62.75; p90 rank ceil(5.4)=6 -> 100.
cat > "$TD/expected.json" <<'EOF'
{"count":6,"total_amount":310.25,"median_amount":62.75,"p90_amount":100,
 "by_tag":{"vip":2,"eu":3},
 "by_month":{"2025-01":{"count":2,"total":200},"2025-02":{"count":2,"total":125.5},
             "2025-03":{"count":2,"total":-15.25}},
 "email_domains":{"ex.io":3,"other.net":1},
 "top_spenders":[{"id":"a","amount":100},{"id":"b","amount":100},
                 {"id":"d","amount":75},{"id":"c","amount":50.5},
                 {"id":"f","amount":10}]}
EOF

bash "$REPORT" "$TD/merged.jsonl" > "$TD/got.json"

python3 - "$TD/got.json" "$TD/expected.json" <<'PY'
import json, sys
got, want = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
def close(a, b):
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) \
            and not isinstance(a, bool) and not isinstance(b, bool):
        return abs(a - b) <= 1e-6
    if isinstance(a, dict) and isinstance(b, dict):
        return set(a) == set(b) and all(close(a[k], b[k]) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(close(x, y) for x, y in zip(a, b))
    return a == b
assert close(got, want), "report mismatch\nGOT:  %s\nWANT: %s" % (
    json.dumps(got, sort_keys=True), json.dumps(want, sort_keys=True))
PY

# Empty input case.
: > "$TD/empty.jsonl"
bash "$REPORT" "$TD/empty.jsonl" > "$TD/got_empty.json"
python3 - "$TD/got_empty.json" <<'PY'
import json, sys
g = json.load(open(sys.argv[1]))
want = {"count": 0, "total_amount": 0, "median_amount": None, "p90_amount": None,
        "by_tag": {}, "by_month": {}, "email_domains": {}, "top_spenders": []}
assert g == want, "empty mismatch: %r" % g
PY
echo "report starter test: OK"
