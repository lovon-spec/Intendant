#!/usr/bin/env bash
# JSONL -> summary report. See report/SPEC.md for the contract.
set -euo pipefail

if [ "$#" -ne 1 ] || [ ! -r "$1" ]; then
  echo "usage: report.sh MERGED.jsonl" >&2
  exit 2
fi

# TODO: implement per report/SPEC.md
echo "report.sh: not implemented" >&2
exit 2
