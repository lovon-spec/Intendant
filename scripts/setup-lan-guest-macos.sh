#!/usr/bin/env bash
#
# Intendant LAN Access Setup (macOS daemon side)
#
# This script is now a thin shim. The real implementation lives in the
# intendant binary as `intendant lan <action>`. The shim forwards the
# same flags and positional args so existing invocations — including
# from the host orchestrator (scripts/setup-lan-macos.sh) — keep
# working unchanged.
#
# Usage:
#   ./setup-lan-guest-macos.sh                      # Full setup
#   ./setup-lan-guest-macos.sh --recert             # Regenerate server cert
#   ./setup-lan-guest-macos.sh --remove             # Tear everything down
#   ./setup-lan-guest-macos.sh --name mac-work      # Label this host
#
# Or use the native subcommand directly:
#   intendant lan setup [flags]
#   intendant lan recert
#   intendant lan remove
#
set -euo pipefail

find_intendant() {
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local project_bin="$script_dir/../target/release/intendant"
    if [[ -x "$project_bin" ]]; then
        echo "$project_bin"
        return 0
    fi
    if command -v intendant &>/dev/null; then
        command -v intendant
        return 0
    fi
    echo "error: intendant binary not found." >&2
    echo "       build it first: cargo build --release" >&2
    echo "       or install it on PATH before running this script." >&2
    exit 1
}

INTENDANT="$(find_intendant)"

action="setup"
args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --recert)           action="recert"; shift ;;
        --remove)           action="remove"; shift ;;
        --stop-cert-server) action="remove"; shift ;;
        --tunnel|--backend) args+=("--backend" "$2"); shift 2 ;;
        -h|--help)
            "$INTENDANT" lan --help
            exit 0
            ;;
        *)
            args+=("$1"); shift
            ;;
    esac
done

exec "$INTENDANT" lan "$action" "${args[@]}"
