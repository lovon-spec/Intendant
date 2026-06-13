#!/usr/bin/env bash
# Behavioral verifier for the service-triplet task.
#
#   verify.sh <workdir> [--seed N]
#
# Grades a scratch COPY of <workdir> (never mutated). stdout is exactly one JSON
# object: {task, seed, component_scores:{api,worker,cli,metrics}, integration,
# total, max_total:5.0, details}. All other output goes to stderr. See
# README.md for the contract.
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

SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/triplet-verify.XXXXXX")
trap 'rm -rf "$SCRATCH"' EXIT
( cd "$WORKDIR" && tar --exclude='./.git' --exclude='./.intendant' \
      --exclude='*/__pycache__' -cf - . ) | ( cd "$SCRATCH" && tar -xf - )

# Guard the array expansion: bash 3.2 (macOS) errors on "${empty[@]}" under set -u.
if [ "${#SEED_ARG[@]}" -gt 0 ]; then
  exec python3 "$HERE/verify/grade.py" "$SCRATCH" "${SEED_ARG[@]}"
fi
exec python3 "$HERE/verify/grade.py" "$SCRATCH"
