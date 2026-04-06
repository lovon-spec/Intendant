#!/usr/bin/env bash
#
# intendant-ctl — Send a command to a running intendant instance via control socket.
#
# Usage:
#   intendant-ctl send '{"action":"start_task","task":"do something"}'
#   intendant-ctl status
#   intendant-ctl approve <id>
#   intendant-ctl follow "additional instructions"
#   intendant-ctl stream              # stream all events
#   intendant-ctl socket              # print the socket path
#
set -euo pipefail

find_socket() {
    local sock
    sock=$(ls /tmp/intendant-*.sock 2>/dev/null | head -1)
    if [[ -z "$sock" ]]; then
        echo "error: no intendant control socket found. Is intendant running with --control-socket?" >&2
        exit 1
    fi
    echo "$sock"
}

cmd="${1:-help}"
shift || true

case "$cmd" in
    socket)
        find_socket
        ;;
    send)
        SOCK=$(find_socket)
        echo "$1" | socat - UNIX-CONNECT:"$SOCK"
        ;;
    status)
        SOCK=$(find_socket)
        echo '{"action":"status"}' | socat - UNIX-CONNECT:"$SOCK"
        ;;
    approve)
        SOCK=$(find_socket)
        echo "{\"action\":\"approve\",\"id\":$1}" | socat - UNIX-CONNECT:"$SOCK"
        ;;
    deny)
        SOCK=$(find_socket)
        echo "{\"action\":\"deny\",\"id\":$1}" | socat - UNIX-CONNECT:"$SOCK"
        ;;
    follow)
        SOCK=$(find_socket)
        MSG=$(python3 -c "import json; print(json.dumps({'action':'follow_up','text':'$*'}))")
        echo "$MSG" | socat - UNIX-CONNECT:"$SOCK"
        ;;
    start)
        SOCK=$(find_socket)
        TASK="$*"
        MSG=$(python3 -c "import json,sys; print(json.dumps({'action':'start_task','task':sys.argv[1]}))" "$TASK")
        echo "$MSG" | socat - UNIX-CONNECT:"$SOCK"
        ;;
    stream)
        SOCK=$(find_socket)
        socat - UNIX-CONNECT:"$SOCK"
        ;;
    help|--help|-h)
        sed -n '3,9p' "$0" | sed 's/^# \?//'
        ;;
    *)
        echo "Unknown command: $cmd" >&2
        exit 1
        ;;
esac
