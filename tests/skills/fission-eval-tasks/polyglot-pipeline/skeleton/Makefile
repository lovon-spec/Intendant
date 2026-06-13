DEDUP_BIN := dedup/target/release/dedup

.PHONY: build test pipeline

build:
	cd dedup && cargo build --release --quiet

test: build
	python3 normalizer/tests/test_normalize.py
	python3 quarantine/tests/test_quarantine.py
	bash dedup/tests/cli_test.sh
	bash report/tests/test_report.sh

# Usage: make pipeline RAW=<dir with *.csv> OUT=<output dir> AS_OF=<YYYY-MM-DD>
pipeline: build
	@test -n "$(RAW)" || { echo "set RAW=<dir of csv files>" >&2; exit 2; }
	@test -n "$(OUT)" || { echo "set OUT=<output dir>" >&2; exit 2; }
	@test -n "$(AS_OF)" || { echo "set AS_OF=<YYYY-MM-DD>" >&2; exit 2; }
	mkdir -p $(OUT)/normalized $(OUT)/clean $(OUT)/quarantine
	set -e; for f in $(RAW)/*.csv; do \
		base=$$(basename $$f .csv); \
		python3 normalizer/normalize.py $$f $(OUT)/normalized/$$base.jsonl; \
	done
	set -e; for f in $(OUT)/normalized/*.jsonl; do \
		base=$$(basename $$f .jsonl); \
		python3 quarantine/quarantine.py --as-of $(AS_OF) $$f \
			$(OUT)/clean/$$base.jsonl $(OUT)/quarantine/$$base.jsonl; \
	done
	$(DEDUP_BIN) $(OUT)/clean/*.jsonl > $(OUT)/merged.jsonl
	bash report/report.sh $(OUT)/merged.jsonl > $(OUT)/report.json
