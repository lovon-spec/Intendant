#!/usr/bin/env bash
#
# Intendant LAN Access Setup for macOS hosts with a UTM Linux guest.
#
# Usage:
#   ./setup-lan-macos.sh              # Interactive setup wizard
#   ./setup-lan-macos.sh --remove     # Uninstall everything
#   ./setup-lan-macos.sh --recert     # Regenerate server cert (IP changed)
#
set -euo pipefail

LAUNCHD_LABEL="com.intendant.tunnel"
SETUP_SCRIPT_NAME="setup-lan.sh"

REAL_USER="${SUDO_USER:-$(whoami)}"
REAL_HOME=$(eval echo "~$REAL_USER")
CONFIG_FILE="$REAL_HOME/.intendant-lan.conf"
LAUNCHD_PLIST="$REAL_HOME/Library/LaunchAgents/$LAUNCHD_LABEL.plist"

ACTION="setup"
FORCE=false

# State — populated by wizard or loaded from config
VM_IP=""
VM_USER=""
HTTPS_PORT=8443
CERT_PORT=9999
NET_MODE=""     # "shared" or "bridged"
LAN_IFACE=""
LAN_IP=""

die()   { echo "error: $*" >&2; exit 1; }
info()  { echo ":: $*"; }
warn()  { echo "!! $*" >&2; }

usage() {
    sed -n '3,9p' "$0" | sed 's/^# \?//'
    exit 0
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --remove)  ACTION="remove"; shift ;;
            --recert)  ACTION="recert"; shift ;;
            --force)   FORCE=true; shift ;;
            -h|--help) usage ;;
            *)         die "unknown option: $1" ;;
        esac
    done
}

# ── Platform checks ──

check_macos() {
    [[ "$(uname)" == "Darwin" ]] || die "this script is for macOS — use setup-lan.sh on Linux"
}

# ── Run commands as the real user (not root) ──

as_user() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        sudo -u "$SUDO_USER" -- "$@"
    else
        "$@"
    fi
}

# ── Config persistence ──

save_config() {
    cat > "$CONFIG_FILE" <<CFG
VM_IP="$VM_IP"
VM_USER="$VM_USER"
HTTPS_PORT=$HTTPS_PORT
NET_MODE="$NET_MODE"
LAN_IFACE="$LAN_IFACE"
CFG
    chown "$REAL_USER" "$CONFIG_FILE" 2>/dev/null || true
}

load_config() {
    [[ -f "$CONFIG_FILE" ]] || return 1
    # shellcheck disable=SC1090
    source "$CONFIG_FILE"
    return 0
}

# ── Network detection ──

detect_lan_iface() {
    LAN_IFACE=$(route -n get default 2>/dev/null | awk '/interface:/ {print $2}')
    [[ -n "$LAN_IFACE" ]] || die "could not detect default network interface"
    info "LAN interface: $LAN_IFACE"
}

detect_lan_ip() {
    LAN_IP=$(ipconfig getifaddr "$LAN_IFACE" 2>/dev/null || true)
    [[ -n "$LAN_IP" ]] || die "could not detect IP for $LAN_IFACE"
    info "LAN IP: $LAN_IP"
}

# ── SSH helpers ──

test_ssh() {
    as_user ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 \
        "${VM_USER}@${VM_IP}" "echo ok" >/dev/null
}

run_on_guest() {
    as_user ssh -o StrictHostKeyChecking=accept-new "${VM_USER}@${VM_IP}" "$1"
}

copy_to_guest() {
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local script_path="$script_dir/$SETUP_SCRIPT_NAME"
    [[ -f "$script_path" ]] || die "$SETUP_SCRIPT_NAME not found in $script_dir"

    as_user scp -o StrictHostKeyChecking=accept-new "$script_path" "${VM_USER}@${VM_IP}:/tmp/$SETUP_SCRIPT_NAME"
    run_on_guest "chmod +x /tmp/$SETUP_SCRIPT_NAME"
}

ensure_ssh_keys() {
    if as_user ssh -o BatchMode=yes -o ConnectTimeout=3 \
            "${VM_USER}@${VM_IP}" "true" &>/dev/null; then
        info "SSH key auth already working"
        return
    fi

    info "SSH key auth required for the tunnel service"

    local key_file="$REAL_HOME/.ssh/id_ed25519"
    if [[ ! -f "$key_file" ]]; then
        info "generating SSH key..."
        as_user ssh-keygen -t ed25519 -f "$key_file" -N "" -q
    fi

    info "copying key to VM (enter password one last time)..."
    as_user ssh-copy-id -o StrictHostKeyChecking=accept-new "${VM_USER}@${VM_IP}"
    info "SSH keys configured"
}

# ── SSH tunnel via launchd ──

setup_tunnel() {
    info "setting up SSH tunnel service..."

    as_user mkdir -p "$REAL_HOME/Library/LaunchAgents"

    # Unload existing if present
    as_user launchctl unload "$LAUNCHD_PLIST" 2>/dev/null || true

    cat > "$LAUNCHD_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LAUNCHD_LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/ssh</string>
        <string>-N</string>
        <string>-o</string>
        <string>ExitOnForwardFailure=yes</string>
        <string>-o</string>
        <string>ServerAliveInterval=15</string>
        <string>-o</string>
        <string>ServerAliveCountMax=3</string>
        <string>-L</string>
        <string>0.0.0.0:${HTTPS_PORT}:localhost:${HTTPS_PORT}</string>
        <string>-L</string>
        <string>0.0.0.0:${CERT_PORT}:localhost:${CERT_PORT}</string>
        <string>${VM_USER}@${VM_IP}</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>${REAL_HOME}/.intendant-tunnel.log</string>
</dict>
</plist>
PLIST
    chown "$REAL_USER" "$LAUNCHD_PLIST" 2>/dev/null || true

    as_user launchctl load "$LAUNCHD_PLIST"
    info "tunnel service started — forwarding 0.0.0.0:{$HTTPS_PORT,$CERT_PORT} → VM"
}

remove_tunnel() {
    if [[ -f "$LAUNCHD_PLIST" ]]; then
        as_user launchctl unload "$LAUNCHD_PLIST" 2>/dev/null || true
        rm -f "$LAUNCHD_PLIST"
        info "tunnel service removed"
    else
        info "no tunnel service found"
    fi
    rm -f "$REAL_HOME/.intendant-tunnel.log"
}

# ── Interactive wizard ──

ask() {
    local prompt="$1" default="${2:-}"
    local suffix=""
    [[ -n "$default" ]] && suffix=" [$default]"
    local answer
    read -rp "  $prompt$suffix: " answer
    echo "${answer:-$default}"
}

ask_choice() {
    local prompt="$1"; shift
    local options=("$@")

    echo ""
    echo "  $prompt"
    echo ""
    for i in "${!options[@]}"; do
        echo "    $((i + 1))) ${options[$i]}"
    done
    echo ""

    while true; do
        local choice
        read -rp "  Choose [1-${#options[@]}]: " choice
        if [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#options[@]} )); then
            return $((choice - 1))
        fi
        echo "  Invalid choice, try again."
    done
}

run_wizard() {
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Intendant LAN Access Setup (macOS → UTM)"
    echo "════════════════════════════════════════════════════════"

    # Step 1: Network mode
    ask_choice "How is your UTM VM networked?" \
        "Shared Network (NAT — default UTM setting)" \
        "Bridged — VM has its own LAN IP"
    local net_choice=$?

    if [[ "$net_choice" -eq 0 ]]; then
        NET_MODE="shared"
    else
        NET_MODE="bridged"
    fi

    # Step 2: VM details
    echo ""
    VM_IP=$(ask "VM IP address")
    [[ -n "$VM_IP" ]] || die "IP address is required"

    VM_USER=$(ask "SSH username on the VM" "$REAL_USER")

    # Step 3: Test SSH
    info "testing SSH connection to ${VM_USER}@${VM_IP}..."
    if ! test_ssh; then
        warn "could not connect via SSH"
        echo ""
        echo "  Make sure:"
        echo "    - The VM is running"
        echo "    - SSH server is installed: sudo apt install openssh-server"
        echo "    - You can SSH manually: ssh ${VM_USER}@${VM_IP}"
        echo ""
        echo "  If you were prompted for a password above and it was rejected,"
        echo "  consider setting up SSH keys:"
        echo "    ssh-copy-id ${VM_USER}@${VM_IP}"
        echo ""
        local retry
        retry=$(ask "Try again after fixing? (y/n)" "y")
        if [[ "$retry" == "y" ]]; then
            if ! test_ssh; then die "still cannot connect"; fi
        else
            die "SSH connection required"
        fi
    fi
    info "SSH connection OK"

    # SSH keys — required for shared (tunnel service), optional for bridged
    if [[ "$NET_MODE" == "shared" ]]; then
        ensure_ssh_keys
    else
        if ! as_user ssh -o BatchMode=yes -o ConnectTimeout=3 \
                "${VM_USER}@${VM_IP}" "true" &>/dev/null; then
            echo ""
            echo "  Tip: set up SSH keys to avoid repeated password prompts:"
            echo "    ssh-copy-id ${VM_USER}@${VM_IP}"
            echo ""
            local setup_keys
            setup_keys=$(ask "Set up SSH keys now? (y/n)" "y")
            if [[ "$setup_keys" == "y" ]]; then
                local key_file="$REAL_HOME/.ssh/id_ed25519"
                if [[ ! -f "$key_file" ]]; then
                    info "generating SSH key..."
                    as_user ssh-keygen -t ed25519 -f "$key_file" -N "" -q
                fi
                info "copying key to VM (enter password one last time)..."
                as_user ssh-copy-id -o StrictHostKeyChecking=accept-new "${VM_USER}@${VM_IP}"
                info "SSH keys configured — no more password prompts"
            fi
        fi
    fi

    # Step 4: Port
    echo ""
    HTTPS_PORT=$(ask "HTTPS port for phone access" "8443")

    # Step 5: Detect host network
    detect_lan_iface
    detect_lan_ip

    # Step 6: Confirm
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Setup Summary"
    echo "════════════════════════════════════════════════════════"
    echo ""
    echo "  Network mode:  $NET_MODE"
    echo "  VM address:    ${VM_USER}@${VM_IP}"
    echo "  Host LAN IP:   $LAN_IP"
    echo "  HTTPS port:    $HTTPS_PORT"
    if [[ "$NET_MODE" == "shared" ]]; then
        echo "  SSH tunnel:    0.0.0.0:{$HTTPS_PORT,$CERT_PORT} → VM (launchd)"
    fi
    echo "  Phone URL:     https://${LAN_IP}:${HTTPS_PORT}"
    echo ""

    local confirm
    confirm=$(ask "Proceed with setup? (y/n)" "y")
    [[ "$confirm" == "y" ]] || exit 0

    # Step 7: Execute
    echo ""

    # SSH tunnel for shared networking (before guest setup so cert port is reachable)
    if [[ "$NET_MODE" == "shared" ]]; then
        setup_tunnel
    fi

    # UTM shared networking often lacks IPv6 routing — tell apt to use IPv4
    if [[ "$NET_MODE" == "shared" ]]; then
        info "configuring apt to prefer IPv4 on VM..."
        run_on_guest "echo 'Acquire::ForceIPv4 \"true\";' | sudo tee /etc/apt/apt.conf.d/99force-ipv4 >/dev/null"
    fi

    info "copying setup script to VM..."
    copy_to_guest

    info "running setup on VM..."
    local guest_args="--port $HTTPS_PORT"
    [[ "$NET_MODE" == "shared" ]] && guest_args="$guest_args --lan-ip $LAN_IP"
    run_on_guest "sudo /tmp/$SETUP_SCRIPT_NAME $guest_args"

    save_config
    info "config saved to $CONFIG_FILE"

    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Setup complete!"
    echo "════════════════════════════════════════════════════════"
    echo ""
    echo "  Phone connects to: https://${LAN_IP}:${HTTPS_PORT}"
    echo ""
    if [[ "$NET_MODE" == "shared" ]]; then
        info "SSH tunnel survives reboot (launchd service)"
        info "  Logs: ~/.intendant-tunnel.log"
    fi
}

# ── Maintenance ──

run_recert() {
    load_config || die "no saved config found — run the setup wizard first"

    detect_lan_iface
    detect_lan_ip

    if [[ "$NET_MODE" == "shared" ]]; then
        info "restarting SSH tunnel..."
        setup_tunnel
    fi

    local recert_args="--recert"
    $FORCE && recert_args="$recert_args --force"
    [[ "$NET_MODE" == "shared" ]] && recert_args="$recert_args --lan-ip $LAN_IP"

    info "regenerating server cert on VM..."
    run_on_guest "sudo /tmp/$SETUP_SCRIPT_NAME $recert_args"

    echo ""
    info "done — phone connects to: https://${LAN_IP}:${HTTPS_PORT}"
}

run_remove() {
    if ! load_config; then
        warn "no saved config found — using defaults"
        HTTPS_PORT=8443
        NET_MODE="shared"
    fi

    info "removing intendant LAN setup..."

    if [[ "$NET_MODE" == "shared" ]]; then
        remove_tunnel
    fi

    info "removing VM-side config..."
    run_on_guest "sudo /tmp/$SETUP_SCRIPT_NAME --remove" 2>/dev/null || \
        warn "could not remove VM config — run 'sudo setup-lan.sh --remove' manually in the VM"

    rm -f "$CONFIG_FILE"
    info "done"
}

# ── Main ──

main() {
    parse_args "$@"
    check_macos

    case "$ACTION" in
        setup)  run_wizard ;;
        recert) run_recert ;;
        remove) run_remove ;;
    esac
}

main "$@"
