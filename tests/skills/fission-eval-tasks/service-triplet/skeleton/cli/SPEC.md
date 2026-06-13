# cli — job client

Python 3 stdlib only (`urllib`, `json`, `argparse`). Talks to the API over the
shared HTTP protocol (see the repo README). It implements five verbs.

## CLI

```
python3 cli/client.py submit       API_URL OP INPUT_JSON
python3 cli/client.py submit-batch API_URL FILE.jsonl
python3 cli/client.py get          API_URL JOB_ID
python3 cli/client.py wait         API_URL JOB_ID [--timeout SECONDS] [--poll SECONDS] [--quiet]
python3 cli/client.py requeue      API_URL JOB_ID
```

`API_URL` is a base URL like `http://127.0.0.1:8080` (no trailing slash).

### `submit API_URL OP INPUT_JSON`

`POST {API_URL}/jobs` with `{"op": OP, "input": <INPUT_JSON parsed as JSON>}`.
On `201`, print **only the new job's `id`** (followed by a newline) to stdout
and exit 0. On any non-201 response, print an error to stderr and exit
non-zero.

### `submit-batch API_URL FILE.jsonl`

`FILE.jsonl` contains one JSON object per line: `{"op": <str>, "input": <any>}`.
Blank/whitespace-only lines are skipped. For each remaining line **in file
order**:

- if the line is not valid JSON, or not an object with a string `op` and an
  `input` key: print an error to stderr, count it as a failure, and
  **continue** with the next line;
- otherwise `POST {API_URL}/jobs`; on `201` print the new job's `id` on its
  own line to stdout (so stdout is exactly the ids of the successfully
  submitted jobs, in input order); on any other response, print an error to
  stderr, count a failure, and continue.

Exit 0 if every line succeeded, non-zero if there was at least one failure
(an unreadable `FILE` is also a failure). 

### `get API_URL JOB_ID`

`GET {API_URL}/jobs/{JOB_ID}`. On `200`, print the job as JSON to stdout and
exit 0. On `404`, print an error to stderr and exit non-zero.

### `wait API_URL JOB_ID [--timeout SECONDS] [--poll SECONDS] [--quiet]`

Poll `GET {API_URL}/jobs/{JOB_ID}` every `--poll` seconds (default `0.1`) until
the job's `status` is `done` or `error`, or `--timeout` seconds (default `10`)
elapse. Then:

- status `done`: print the final job JSON to stdout, exit 0.
- status `error`: print the final job JSON to stdout, exit non-zero.
- timeout: print an error to stderr, exit non-zero.

With `--quiet`, print **nothing to stdout** (errors may still go to stderr);
the exit code alone carries the outcome.

### `requeue API_URL JOB_ID`

`POST {API_URL}/jobs/{JOB_ID}/requeue`. On `200`, print the updated job JSON
to stdout and exit 0. On any other response (`404` unknown, `409` not in
`error` status), print an error to stderr and exit non-zero.

## Output discipline

Printed job JSON must be a single JSON object parseable with `json.loads`;
key order and whitespace do not matter. `submit`/`submit-batch` stdout must
contain the ids only — no banners or extra lines.

## Starter test

```
python3 cli/tests/test_cli.py
```
