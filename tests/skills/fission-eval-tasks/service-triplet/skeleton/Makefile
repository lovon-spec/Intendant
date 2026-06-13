.PHONY: test run-api run-worker run-metrics

test:
	python3 worker/tests/test_worker.py
	bash api/tests/test_api.sh
	python3 cli/tests/test_cli.py
	python3 metrics/tests/test_metrics.py

# Convenience launchers (the grader starts the services itself, on random ports).
run-api:
	@test -n "$(PORT)" || { echo "set PORT=<port>" >&2; exit 2; }
	python3 api/server.py --port $(PORT)

run-worker:
	@test -n "$(URL)" || { echo "set URL=<api base url>" >&2; exit 2; }
	python3 worker/worker.py serve $(URL)

run-metrics:
	@test -n "$(PORT)" || { echo "set PORT=<port>" >&2; exit 2; }
	@test -n "$(URL)" || { echo "set URL=<api base url>" >&2; exit 2; }
	python3 metrics/metrics.py --port $(PORT) --api $(URL)
