# metrics — read-only aggregation service

Python 3 stdlib only. A separate HTTP service that aggregates the job store
**on demand**: every metrics request fetches the current job list from the API
(`GET {api}/jobs`, unpaginated) and computes its answer from that fresh
snapshot. It holds no state of its own, never mutates the API, and computes no
job results.

## CLI

```
python3 metrics/metrics.py --port PORT --api API_URL [--host HOST]
```

- `--port` and `--api` are required; `--host` defaults to `127.0.0.1`.
- Serve until killed. A startup line on stderr is fine; stdout is not checked.

## Endpoints

All responses are JSON (`Content-Type: application/json`).

### `GET /healthz`

`200 {"ok": true}` — always, **independent of whether the API is reachable**
(the metrics process itself is healthy).

### `GET /summary`

Fetch `GET {api}/jobs`; on success, `200` with:

```json
{
  "total": <int — number of jobs>,
  "by_status": {"<status>": <int>, ...},
  "by_op": {"<op>": <int>, ...},
  "done_ratio": <number — done count / total; null when total is 0>,
  "numeric_result_stats": {"count": <int>, "sum": <number>,
                            "min": <number>, "max": <number>}
}
```

- `by_status` / `by_op` contain only keys that actually appear (no
  zero-count entries; `{}` when there are no jobs).
- `done_ratio` is a plain JSON number (graded with a small tolerance);
  `null` when there are no jobs.
- `numeric_result_stats` aggregates over jobs whose `status` is `"done"`
  **and** whose `result` is a JSON number (booleans are not numbers). It is
  `null` when no such job exists.

### `GET /ops/{op}`

Fetch `GET {api}/jobs`; on success, `200` with:

```json
{"op": "<op>", "total": <int>, "by_status": {"<status>": <int>, ...}}
```

counting only jobs whose `op` equals `{op}` exactly. An op that never
appears yields `{"op": "...", "total": 0, "by_status": {}}` (still `200`).

### API failure

If the API fetch fails (connection refused/timeout, non-200, or a body that
is not valid JSON), `/summary` and `/ops/{op}` respond
`503 {"error": "<any message>"}`. `/healthz` is unaffected.

### Anything else

`404` (body unspecified).

## Starter test

```
python3 metrics/tests/test_metrics.py
```
