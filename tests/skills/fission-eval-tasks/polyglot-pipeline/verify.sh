#!/usr/bin/env bash
# Behavioral verifier for the polyglot-pipeline task.
#
#   verify.sh <workdir> [--seed N]
#
# <workdir> is the agent's repo (a copy of skeleton/ they implemented). It is
# graded on a scratch COPY (never mutated). All build/run noise goes to stderr;
# stdout is exactly one JSON object: {task, seed, component_scores, integration,
# total, max_total, details}. See README.md for the contract.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

WORKDIR=""
SEED_ARG=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --seed) SEED_ARG=(--seed "$2"); shift 2 ;;
    --seed=*) SEED_ARG=(--seed "${1#*=}"); shift ;;
    *) WORKDIR="$1"; shift ;;
  esac
done

if [ -z "$WORKDIR" ] || [ ! -d "$WORKDIR" ]; then
  echo "usage: verify.sh <workdir> [--seed N]" >&2
  exit 64
fi
WORKDIR=$(cd "$WORKDIR" && pwd)

command -v python3 >/dev/null || { echo "python3 required" >&2; exit 70; }
command -v jq      >/dev/null || echo "warning: jq not found; report component will score 0" >&2

# Scratch copy: exclude VCS / agent-state / build dirs so grading is hermetic
# and side-effect free on the agent's tree.
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/polyglot-verify.XXXXXX")
trap 'rm -rf "$SCRATCH"' EXIT
( cd "$WORKDIR" && tar --exclude='./.git' --exclude='./.intendant' \
      --exclude='*/target' --exclude='./.grade_raw' --exclude='./.grade_out' \
      -cf - . ) | ( cd "$SCRATCH" && tar -xf - )

# Build the dedup tool (best effort; failure just zeroes the dedup component
# and the merged/report integration stages).
if command -v cargo >/dev/null && [ -f "$SCRATCH/dedup/Cargo.toml" ]; then
  ( cd "$SCRATCH/dedup" && cargo build --release --quiet ) >&2 \
    || echo "warning: dedup failed to build" >&2
fi

# Guard the array expansion: bash 3.2 (macOS) errors on "${empty[@]}" under set -u.
if [ "${#SEED_ARG[@]}" -gt 0 ]; then
  exec python3 "$HERE/verify/grade.py" "$SCRATCH" "${SEED_ARG[@]}"
fi
exec python3 "$HERE/verify/grade.py" "$SCRATCH"
