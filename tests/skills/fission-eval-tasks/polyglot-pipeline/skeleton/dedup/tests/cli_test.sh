#!/usr/bin/env bash
# Starter test for the dedup binary (see dedup/SPEC.md).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DEDUP_DIR=$(dirname "$HERE")
(cd "$DEDUP_DIR" && cargo build --release --quiet)
BIN="$DEDUP_DIR/target/release/dedup"

TD=$(mktemp -d)
trap 'rm -rf "$TD"' EXIT

cat > "$TD/a.jsonl" <<'EOF'
{"id":"x1","name":"A","email":null,"amount":10,"date":"2025-01-01","tags":["t0"]}
{"id":"y2","name":"B","email":"b@e.com","amount":5.5,"date":"2025-03-01","tags":["a","z"]}
{"id":"m1","name":"M","email":null,"amount":"not-a-number","date":"2025-01-01","tags":[]}
{"id":"z3","name":"Zo","email":"old@z.io","amount":1,"date":"2025-01-01","tags":["q"]}
EOF
cat > "$TD/b.jsonl" <<'EOF'
{"id":"x1","name":"A2","email":"a2@e.com","amount":20,"date":"2025-02-01","tags":["t2"]}
{"id":"x1","name":"A3","email":null,"amount":30,"date":"2025-02-01","tags":["t3"]}
{"id":"w0","name":"W","email":null,"amount":1,"date":"2024-12-31","tags":[]}
{"id":"z3","name":"Zn","email":null,"amount":2,"date":"2025-05-05","tags":["p"]}
EOF
# m1 has a non-number amount -> skipped silently (no position, no tags).
# x1: newest date 2025-02-01 ties between A2 and A3; A2 has a non-null email,
#     so A2 wins (the email rule beats the position rule). Tags union the group.
# z3: the winner (Zn, 2025-05-05) has a null email -> backfilled from Zo.
cat > "$TD/expected.jsonl" <<'EOF'
{"id":"w0","name":"W","email":null,"amount":1,"date":"2024-12-31","tags":[]}
{"id":"x1","name":"A2","email":"a2@e.com","amount":20,"date":"2025-02-01","tags":["t0","t2","t3"]}
{"id":"y2","name":"B","email":"b@e.com","amount":5.5,"date":"2025-03-01","tags":["a","z"]}
{"id":"z3","name":"Zn","email":"old@z.io","amount":2,"date":"2025-05-05","tags":["p","q"]}
EOF
# --since 2025-01-15 drops w0, x1's first record (its t0 never unions in), and
# z3's old record (so there is no backfill source left: email stays null).
cat > "$TD/expected_since.jsonl" <<'EOF'
{"id":"x1","name":"A2","email":"a2@e.com","amount":20,"date":"2025-02-01","tags":["t2","t3"]}
{"id":"y2","name":"B","email":"b@e.com","amount":5.5,"date":"2025-03-01","tags":["a","z"]}
{"id":"z3","name":"Zn","email":null,"amount":2,"date":"2025-05-05","tags":["p"]}
EOF

"$BIN" "$TD/a.jsonl" "$TD/b.jsonl" > "$TD/got.jsonl"
"$BIN" --since 2025-01-15 "$TD/a.jsonl" "$TD/b.jsonl" > "$TD/got_since.jsonl"

python3 - "$TD/got.jsonl" "$TD/expected.jsonl" <<'PY'
import json, sys
got = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
want = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
assert got == want, "dedup output mismatch\nGOT:  %r\nWANT: %r" % (got, want)
PY
python3 - "$TD/got_since.jsonl" "$TD/expected_since.jsonl" <<'PY'
import json, sys
got = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
want = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
assert got == want, "dedup --since mismatch\nGOT:  %r\nWANT: %r" % (got, want)
PY

# usage errors
if "$BIN" > /dev/null 2>&1; then echo "no-args must exit non-zero" >&2; exit 1; fi
if "$BIN" --since 2025/01/01 "$TD/a.jsonl" > /dev/null 2>&1; then
  echo "bad --since must exit non-zero" >&2; exit 1
fi
echo "dedup starter test: OK"
