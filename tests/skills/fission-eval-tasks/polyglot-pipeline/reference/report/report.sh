#!/usr/bin/env bash
# Reference solution for the report component. See report/SPEC.md.
# Excluded from agent visibility by the SKILL runner.
set -euo pipefail

if [ "$#" -ne 1 ] || [ ! -r "$1" ]; then
  echo "usage: report.sh MERGED.jsonl" >&2
  exit 2
fi

# One slurped jq pass. round2 = 2-decimal rounding; p90 uses the nearest-rank
# index ceil(0.9*n) computed as floor((9n+9)/10); median averages the two
# middles on even counts (no rounding; the grader allows tolerance).
jq -s '
  def round2: (. * 100 | round) / 100;
  . as $r
  | ($r | length) as $n
  | ($r | map(.amount) | sort) as $a
  | {
      count: $n,
      total_amount: ((($a | add) // 0) | round2),
      median_amount:
        (if $n == 0 then null
         elif ($n % 2) == 1 then $a[(($n - 1) / 2) | floor]
         else (($a[($n / 2 | floor) - 1] + $a[($n / 2 | floor)]) / 2)
         end),
      p90_amount:
        (if $n == 0 then null
         else $a[((($n * 9 + 9) / 10) | floor) - 1]
         end),
      by_tag: ($r | reduce (.[].tags[]) as $t ({}; .[$t] = ((.[$t] // 0) + 1))),
      by_month:
        ($r
         | reduce .[] as $rec ({};
             ($rec.date[0:7]) as $m
             | .[$m] = {count: ((.[$m].count // 0) + 1),
                        total: ((.[$m].total // 0) + $rec.amount)})
         | with_entries(.value.total = (.value.total | round2))),
      email_domains:
        ($r
         | map(select(.email != null) | .email | split("@")[1])
         | reduce .[] as $d ({}; .[$d] = ((.[$d] // 0) + 1))),
      top_spenders: ($r | sort_by(.id) | sort_by(-.amount) | .[0:5] | map({id, amount}))
    }
' "$1"
