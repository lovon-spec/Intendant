# api — REST job store

Python 3 stdlib only (e.g. `http.server`, `json`). Stores jobs in memory; it
does **not** compute results (that is the worker's job). It is a queue with a
lifecycle.

## CLI

```
python3 api/server.py --port PORT [--host HOST]
```

- `--port` is required; `--host` defaults to `127.0.0.1`.
- Bind and serve until killed. It is fine (and expected) to print a startup
  line to stderr; stdout is not checked.

## Job

A JSON object with exactly these keys:

```json
{"id": "<non-empty unique string>",
 "op": "<non-empty string>",
 "input": <any JSON, including null>,
 "status": "queued" | "running" | "done" | "error",
 "result": <any JSON, null until set>,
 "attempts": <int, 0 until claimed>}
```

`id` is assigned by the API (any non-empty unique string — a counter or UUID
is fine). New jobs start `queued` with `result` `null` and `attempts` `0`.
Every successful claim increments `attempts` by 1.

## Endpoints

All request/response bodies are JSON. Respond with
`Content-Type: application/json`.

| Method + path | Request body | Success | Errors |
|---|---|---|---|
| `GET /healthz` | — | `200 {"ok": true}` | — |
| `POST /jobs` | `{"op": <str>, "input": <any>}` | `201` with the created job | `400` if the body is not valid JSON, or `op` is missing / not a string / the **empty string**, or the `input` key is **absent** (an explicit `"input": null` is valid) |
| `GET /jobs/{id}` | — | `200` with the job | `404` if no such id |
| `GET /jobs` | — | `200 {"jobs": [<job>, ...]}` — see *Listing* below | `400` for invalid `limit`/`offset` |
| `POST /jobs/{id}/claim` | — | `200` with the job, whose `status` is now `running` and `attempts` incremented | `404` unknown id; `409` if the job's status is not `queued` |
| `POST /jobs/{id}/result` | `{"status": "done"\|"error", "result": <any>}` | `200` with the updated job (`status` and `result` set; a missing `result` key means `null`) | `404` unknown id; `400` if body is not JSON or `status` is not `done`/`error` |
| `POST /jobs/{id}/requeue` | — | `200` with the job back in `queued` with `result` reset to `null` (`attempts` is **preserved**) | `404` unknown id; `409` if the job's status is not `error` |
| `DELETE /jobs/{id}` | — | `200 {"deleted": true, "id": "<id>"}` and the job is gone (a later `GET` is `404`) | `404` unknown id; `409` if the job's status is `queued` or `running` (only terminal jobs are deletable) |

### Listing — `GET /jobs` query parameters

- Jobs are returned in **creation order** (oldest first). Deleted jobs are
  gone; requeued jobs keep their original creation position.
- `status={s}` — keep only jobs whose `status == s`.
- `op={o}` — keep only jobs whose `op == o`. Combines with `status` (AND).
- `offset={n}` — skip the first *n* jobs **after filtering** (default 0).
- `limit={n}` — return at most *n* jobs after the offset (default: no limit).
  `limit=0` is valid and returns `{"jobs": []}`.
- `limit`/`offset` must be base-10 non-negative integers; anything else
  (e.g. `limit=-1`, `limit=abc`) → `400`. Unknown query parameters are
  ignored.

Notes:

- `claim` must be **atomic**: if two requests claim the same `queued` job, one
  returns `200` (and flips it to `running`), the other returns `409`. A simple
  lock around the read-modify-write is enough.
- `result` may be set on a `running` job (the normal path); setting it on an
  already-terminal job is allowed (last write wins) but the worker never does
  that.
- Unknown routes/methods may return `404`/`405`; only the rows above are
  graded.

## Performance

The grader drives a bulk lifecycle (a few hundred jobs created, claimed,
resolved, and listed) and expects it to complete within a generous wall-clock
budget (30 s). Any in-memory dict/list store passes easily; per-request disk
or O(n²) rescans may not.

## Starter test

```
bash api/tests/test_api.sh
```
