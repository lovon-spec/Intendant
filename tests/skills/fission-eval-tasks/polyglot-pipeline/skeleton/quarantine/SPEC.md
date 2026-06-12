# quarantine — business-rule validation stage

Python 3 standard library only. Sits between the normalizer and dedup in the
pipeline: takes normalized JSONL, applies row-local business rules, and splits
the stream into **clean** records and **quarantined** records with reason
codes. It never modifies a record.

## CLI

```
python3 quarantine/quarantine.py --as-of YYYY-MM-DD INPUT.jsonl CLEAN.jsonl QUAR.jsonl
```

- `--as-of` is **required** and must be a valid `YYYY-MM-DD` calendar date;
  a missing/malformed `--as-of`, wrong argument count, or unreadable input →
  exit non-zero, write nothing.
- Otherwise exit 0 — including when every record lands on one side. **Both**
  output files are always written (possibly empty).
- Empty/whitespace-only input lines are skipped. Input lines are otherwise
  trusted to be records in the repo README schema (the normalizer produced
  them).

## Routing

Evaluate **all** rules below for each record:

- zero violations → append the record **unchanged** to `CLEAN.jsonl` (one
  JSON object per line);
- one or more violations → append to `QUAR.jsonl` as
  `{"record": <the record, unchanged>, "reasons": [<all violated codes,
  sorted ascending>]}`.

Input order is preserved within each file.

## Rules (code — condition that quarantines)

| code | condition |
|---|---|
| `amount_limit` | `abs(amount) > 25000` |
| `zero_amount` | `amount == 0` (i.e. `0`, `0.0`, `-0.0`) |
| `missing_contact` | `amount > 1000` **and** `email` is `null` (only positive amounts trigger this) |
| `test_data` | the lowercased `name` contains the substring `"test"` |
| `future_date` | `date` > the `--as-of` date (ISO strings compare correctly) |
| `stale_date` | `date` < the cutoff, where the cutoff is the `--as-of` date moved back exactly **5 years**; if `--as-of` is Feb 29 and the target year is not a leap year, the cutoff is Feb 28 of that year. A record dated exactly on the cutoff is **not** stale. |

Notes:

- Codes in `reasons` are sorted ascending (plain byte order) and contain no
  duplicates.
- Rules are independent; a record can violate several at once (e.g. a
  30,000-amount record with `null` email gets
  `["amount_limit", "missing_contact"]`).

## Starter test

```
python3 quarantine/tests/test_quarantine.py
```
