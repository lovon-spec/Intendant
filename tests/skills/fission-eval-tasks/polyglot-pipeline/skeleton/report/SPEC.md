# report — JSONL → summary report

A `bash` + `jq` script. No other interpreters.

## CLI

```
bash report/report.sh MERGED.jsonl     # JSON report on stdout
```

- One argument: a JSONL file of records (the schema in the repo README).
- Empty/whitespace-only lines are ignored.
- Print exactly one JSON object to stdout. Exit 0 on success; exit non-zero
  on a missing/unreadable argument.
- The input may be empty (zero records) — see the empty case below.

## Report shape

```json
{
  "count": <int>,
  "total_amount": <number>,
  "median_amount": <number or null>,
  "p90_amount": <number or null>,
  "by_tag": { "<tag>": <int>, ... },
  "by_month": { "YYYY-MM": {"count": <int>, "total": <number>}, ... },
  "email_domains": { "<domain>": <int>, ... },
  "top_spenders": [ {"id": "<id>", "amount": <number>}, ... ]
}
```

Field rules:

- `count` — total record count.
- `total_amount` — sum of every record's `amount`, rounded to 2 decimal
  places. A whole number is still emitted as a number (e.g. `42` or `42.5`,
  never `"42"`).
- `median_amount` — the median of the amounts: sort ascending; odd count →
  the middle value; even count → the mean of the two middle values. No
  rounding is required (the grader allows a small numeric tolerance). `null`
  when there are no records.
- `p90_amount` — the 90th-percentile amount by the **nearest-rank** method:
  with the amounts sorted ascending, the element at 1-indexed position
  `ceil(0.9 * count)`. (count 1 → that element; count 10 → the 9th; count 11
  → the 10th.) `null` when there are no records.
- `by_tag` — for every tag value that appears in any record's `tags` array,
  the number of records whose `tags` contain it. Tags appearing zero times
  are absent. (Object key order does not matter.)
- `by_month` — group records by the `YYYY-MM` prefix of their `date`; for
  each month present: `count` = records that month, `total` = sum of their
  amounts rounded to 2 decimals. Months with no records are absent.
- `email_domains` — over records whose `email` is non-null: count records
  per domain, where the domain is everything after the `@`. Records with
  `null` email are excluded. `{}` if none.
- `top_spenders` — the records with the highest `amount`, as `{id, amount}`
  objects, sorted by `amount` **descending**; break ties by `id` **ascending**
  (byte order). Include at most **5**. If there are fewer than 5 records,
  include all of them. Omit nothing else.

### Empty input

Exactly:

```json
{"count": 0, "total_amount": 0, "median_amount": null, "p90_amount": null,
 "by_tag": {}, "by_month": {}, "email_domains": {}, "top_spenders": []}
```

## Performance budget

A 60,000-line input must report in under **30 seconds** (the grader enforces
this on a generated input). A single `jq` pass (e.g. slurp + one filter) fits
easily; invoking `jq` once per record does not.

## Starter test

```
bash report/tests/test_report.sh
```
