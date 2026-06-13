# jobline

A small job-processing system: a REST **API** that stores jobs, a **worker**
that computes their results, a **CLI** client that submits and polls jobs, and
a read-only **metrics** service that aggregates the job store. Everything is
Python 3 standard library — no third-party packages, no network beyond
localhost.

## Components

The system is four components. **They are independent**: each lives in its own
directory with its own authoritative spec and its own starter tests (currently
failing), and each can be built and tested on its own. They communicate only
through the shared HTTP protocol and op semantics below — no component imports
another's source.

| Directory | Tool | Role |
|---|---|---|
| `api/` | `server.py` | REST job store (queue + lifecycle + listing/pagination). Computes nothing. |
| `worker/` | `worker.py` | Claims queued jobs, computes results, submits them back. Owns the op semantics. |
| `cli/` | `client.py` | Client: submit / submit-batch / get / wait / requeue. |
| `metrics/` | `metrics.py` | Read-only aggregation service over the API (its own HTTP endpoint). |

## Shared contract — HTTP protocol

A **job** is a JSON object:
`{"id", "op", "input", "status", "result", "attempts"}` where `status` is one
of `queued`, `running`, `done`, `error`; `result` is `null` until set; and
`attempts` counts successful claims (0 for a new job). `id` is a non-empty
unique string the API assigns. The API serves these endpoints (all bodies
JSON; `Content-Type: application/json`):

| Method + path | Body | Success | Errors |
|---|---|---|---|
| `GET /healthz` | — | `200 {"ok": true}` | — |
| `POST /jobs` | `{"op": str, "input": any}` | `201` job (status `queued`, result `null`, attempts `0`) | `400` if not JSON / `op` missing, non-string or empty / `input` key absent |
| `GET /jobs/{id}` | — | `200` job | `404` unknown id |
| `GET /jobs?status=&op=&offset=&limit=` | — | `200 {"jobs": [job, ...]}` in creation order, filtered then sliced | `400` invalid `limit`/`offset` |
| `POST /jobs/{id}/claim` | — | `200` job now `running`, attempts+1 | `404` unknown; `409` if not currently `queued` |
| `POST /jobs/{id}/result` | `{"status": "done"\|"error", "result": any}` | `200` updated job | `404` unknown; `400` bad body |
| `POST /jobs/{id}/requeue` | — | `200` job back to `queued`, result `null`, attempts preserved | `404` unknown; `409` if not `error` |
| `DELETE /jobs/{id}` | — | `200 {"deleted": true, "id": id}` | `404` unknown; `409` if `queued`/`running` |

The `claim` step is atomic: two workers racing to claim the same job — exactly
one gets `200`, the other `409`. See `api/SPEC.md` for the full listing
(filter/pagination) semantics.

## Shared contract — op semantics (defined by the worker)

`op` + `input` → `result`. See `worker/SPEC.md` for the exact rules. Summary:
`sum`/`max`/`min`/`mean`/`median`/`sort_desc`/`dedupe` take a list
(numbers; `dedupe` also accepts all-strings); `reverse`/`wordcount`/
`uppercase`/`histogram` take a string; `clamp`/`rotate` take a small object.
JSON booleans are never numbers. Invalid input (or an unknown op) yields
status `error`.

## Shared contract — CLI verbs

See `cli/SPEC.md`. `submit <url> <op> <input_json>` prints the new job id;
`submit-batch <url> <file.jsonl>` submits one job per line and prints the ids;
`get <url> <id>` prints the job JSON; `wait <url> <id>` polls until the job is
`done`/`error` and prints the final job JSON (`--quiet` suppresses stdout);
`requeue <url> <id>` puts an `error` job back in the queue.

## Shared contract — metrics

See `metrics/SPEC.md`. `GET /summary` returns
`{total, by_status, by_op, done_ratio, numeric_result_stats}` computed from a
fresh `GET {api}/jobs` snapshot; `GET /ops/{op}` returns the per-op breakdown;
API unreachable → `503`.

## Make targets

```
make test      # run all four components' starter tests
make run-api PORT=8080                        # convenience: start the API
make run-worker URL=http://127.0.0.1:8080     # convenience: start the worker loop
make run-metrics PORT=9090 URL=http://127.0.0.1:8080   # convenience: start metrics
```

The end-to-end flow (API + worker + metrics + CLI together) is exercised by
the grader, which starts the live services on random ports and drives them
with generated jobs.
