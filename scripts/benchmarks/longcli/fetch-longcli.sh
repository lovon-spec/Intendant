#!/usr/bin/env bash
# Fetch LongCLI-Bench (arXiv 2602.14337) at the pinned commit.
#
# The checkout is ~213 MB (tasks_long_cli/ alone is ~180 MB), so it is NOT
# vendored into this repo. This script produces a reproducible checkout at a
# caller-chosen path; the tb agents in this directory are then run against it
# via PYTHONPATH + `tb run --dataset-path <checkout>/tasks_long_cli`.
#
# Usage:
#   ./fetch-longcli.sh [DEST_DIR]        # default: $HOME/longcli-bench
#
# LongCLI-Bench vendors its own `terminal_bench` 0.2.18 fork (the `tb` CLI
# comes from this checkout, NOT from upstream terminal-bench / harbor).
set -euo pipefail

LONGCLI_REPO="https://github.com/finyorko/longcli-bench"
LONGCLI_COMMIT="e20364ba3eb4c083f582843cdd4e2d5fe3b5a729"  # 2026-05-25, post task-cleanup

DEST="${1:-$HOME/longcli-bench}"

if [ -e "$DEST/.git" ]; then
    echo "Existing checkout at $DEST — fetching and pinning to $LONGCLI_COMMIT"
    git -C "$DEST" fetch origin
else
    git clone "$LONGCLI_REPO" "$DEST"
fi

git -C "$DEST" checkout --detach "$LONGCLI_COMMIT"

echo
echo "LongCLI-Bench pinned at:"
git -C "$DEST" log -1 --format='  %H%n  %cI  %s'
echo
echo "Next steps (see README.md / RUN-COMMANDS.md in this directory):"
echo "  python3 -m venv ~/longcli-venv && ~/longcli-venv/bin/pip install -e $DEST"
echo "  docker build -f $DEST/longcli_dockerImage/Dockerfile.make-pytest-base -t tb/make-pytest:v0 $DEST/longcli_dockerImage"
echo "  docker build -f $DEST/longcli_dockerImage/Dockerfile.c-env-base -t tb/c-env:v0 $DEST/longcli_dockerImage"
