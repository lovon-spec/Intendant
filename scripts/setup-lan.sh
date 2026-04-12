#!/usr/bin/env bash
#
# Intendant LAN Access Setup (Linux daemon side)
#
# This script is now a thin shim. The real implementation lives in the
# intendant binary as `intendant lan <action>`. The shim forwards the
# same flags and positional args, so existing invocations — including
# from the macOS host orchestrator (scripts/setup-lan-macos.sh) — keep
# working unchanged.
#
# Usage:
#   sudo ./setup-lan.sh                      # Full setup
#   sudo ./setup-lan.sh --recert             # Regenerate server cert
#   sudo ./setup-lan.sh --remove             # Tear everything down
#   sudo ./setup-lan.sh --name mac-work      # Label this host
#
# Or use the native subcommand directly:
#   sudo intendant lan setup [flags]
#   sudo intendant lan recert
#   sudo intendant lan remove
#
set -euo pipefail

# Find the intendant binary: prefer the project's release build, then
# fall back to PATH. Don't force PATH-only, because setup-lan-macos.sh
# copies this shim to /tmp on the target VM where only $PATH is usable.
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

# Parse the flags the old bash script supported and translate them to
# the new subcommand form. Only a few flags changed shape:
#   --recert / --remove / --stop-cert-server → subcommand names
#   everything else → passed through verbatim
action="setup"
args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --recert)           action="recert"; shift ;;
        --remove)           action="remove"; shift ;;
        --stop-cert-server) action="remove"; shift ;;  # closest equivalent
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
