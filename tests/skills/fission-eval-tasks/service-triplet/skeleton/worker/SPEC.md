# worker — job processor

Python 3 stdlib only. Two responsibilities: a pure **compute** function (the op
semantics) and a **serve** loop that drives jobs through the API.

## CLI

```
python3 worker/worker.py compute OP INPUT_JSON      # pure: print {"status","result"}
python3 worker/worker.py serve API_URL [--once] [--poll SECONDS]
```

### `compute OP INPUT_JSON`

`INPUT_JSON` is the job's `input`, as a JSON string. If `INPUT_JSON` is the
single character `-`, read the JSON value from **stdin** instead (large
inputs do not fit in argv; a bare `-` is not valid JSON, so the sentinel is
unambiguous). Print exactly one JSON object to stdout:
`{"status": "done"|"error", "result": <any>}`. Exit 0 (also for `"error"`
results — an error *status* is a successful computation). Beyond argv/stdin
and stdout this subcommand performs no I/O, so it is independently testable
without the API.

### `serve API_URL [--once] [--poll SECONDS]`

Loop: `GET {API_URL}/jobs?status=queued`; for a queued job, `POST .../claim`
(skip on `409` — someone else got it); compute its result; `POST .../result`
with `{"status", "result"}` from compute. With `--once`, process at most one
job and exit; otherwise loop forever, sleeping `--poll` seconds (default `0.2`)
when there is nothing to do. Tolerate transient API errors by retrying.

## Op semantics (compute)

Throughout: a JSON boolean is **never** a number — `true`/`false` appearing
where a number is required (in a list, as `min`/`max`, as `by`) makes the
input invalid. "Number" means JSON int or float.

| op | valid input | result |
|---|---|---|
| `sum` | list of numbers (may be empty) | their sum (`0` for `[]`) |
| `max` | **non-empty** list of numbers | the maximum |
| `min` | **non-empty** list of numbers | the minimum |
| `mean` | **non-empty** list of numbers | arithmetic mean (a number; no rounding required — graded with tolerance) |
| `median` | **non-empty** list of numbers | sort ascending: odd count → the middle value; even count → the mean of the two middles (graded with tolerance) |
| `sort_desc` | list of numbers (may be empty) | a new list, sorted descending |
| `dedupe` | list whose elements are **all numbers or all strings** (may be empty) | the list with duplicates removed, keeping the **first** occurrence of each value, original order otherwise. For numbers, numeric equality applies (`1` and `1.0` are duplicates). A mixed or otherwise-typed list is invalid. |
| `reverse` | string | the string reversed |
| `wordcount` | string | integer = number of whitespace-separated tokens (`len(s.split())`) |
| `uppercase` | string | the string upper-cased |
| `histogram` | string | object mapping each whitespace-separated token to its occurrence count (`{}` for an empty/blank string) |
| `clamp` | an object with **exactly** the keys `{"values": <list of numbers>, "min": <number>, "max": <number>}`, where `min <= max` | the values list with each element clamped into `[min, max]`, order preserved |
| `rotate` | an object with **exactly** the keys `{"values": <list of any JSON values>, "by": <integer-valued number>}` (`3` and `3.0` are fine, `3.5` and `true` are not) | the list rotated **right** by `by` positions: with `n = len(values)` and `k = by mod n` (mathematical modulo, so negative `by` rotates left), the result is `values[n-k:] + values[:n-k]`. An empty list yields `[]` for any `by`. |

- On the wrong input type/shape (e.g. `sum` of a string, `max` of `[]`,
  `clamp` with `min > max` or extra/missing keys, `rotate` with a fractional
  `by`), or an **unknown op**, return status `"error"`. The `result` for an
  error is not graded (use `null` or a short message — anything).
- On valid input, return status `"done"` and the result above.

Examples: `compute("sum", [1, 2, 3])` → `("done", 6)`;
`compute("median", [4, 1, 3, 2])` → `("done", 2.5)`;
`compute("dedupe", [3, 1, 3.0, 2, 1])` → `("done", [3, 1, 2])`;
`compute("histogram", "a b a")` → `("done", {"a": 2, "b": 1})`;
`compute("clamp", {"values": [-5, 7, 2], "min": 0, "max": 5})` → `("done", [0, 5, 2])`;
`compute("rotate", {"values": [1, 2, 3, 4], "by": -1})` → `("done", [2, 3, 4, 1])`;
`compute("rotate", {"values": [1, 2, 3, 4], "by": 6})` → `("done", [3, 4, 1, 2])`;
`compute("max", [])` → `("error", ...)`;
`compute("sum", [1, true])` → `("error", ...)`;
`compute("frobnicate", 1)` → `("error", ...)`.

## Performance

`compute` must handle large inputs within a generous budget (the grader runs
`sort_desc` on a 200,000-number list and `histogram` on a ~1 MB string — fed
via the stdin form — each with a 10 s wall-clock budget). Anything
linear/`O(n log n)` passes easily.

## Starter test

```
python3 worker/tests/test_worker.py
```
