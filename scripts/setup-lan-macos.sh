#!/usr/bin/env bash
#
# Intendant LAN Access Setup for macOS hosts with a VM guest (Linux or macOS).
# Supports multiple VMs — each gets its own tunnel, ports, and certificates.
#
# Usage:
#   ./setup-lan-macos.sh              # Interactive setup wizard
#   ./setup-lan-macos.sh --list       # Show configured VMs
#   ./setup-lan-macos.sh --recert     # Regenerate server cert (IP changed)
#   ./setup-lan-macos.sh --remove     # Remove a VM's LAN setup
#
set -euo pipefail

SETUP_SCRIPT_NAME="setup-lan.sh"
GUEST_OS="linux"

REAL_USER="${SUDO_USER:-$(whoami)}"
REAL_HOME=$(eval echo "~$REAL_USER")
CONFIG_DIR="$REAL_HOME/.intendant-lan"

ACTION="setup"
FORCE=false

# Instance state — populated by wizard, select_instance, or load_config
INSTANCE_SLUG=""
INSTANCE_NAME=""
VM_IP=""
VM_USER=""
HTTPS_PORT=8443
CERT_PORT=9999
NET_MODE=""
LAN_IFACE=""
LAN_IP=""

die()   { echo "error: $*" >&2; exit 1; }
info()  { echo ":: $*"; }
warn()  { echo "!! $*" >&2; }

usage() {
    sed -n '3,11p' "$0" | sed 's/^# \?//'
    exit 0
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --list)    ACTION="list"; shift ;;
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

set_guest_script() {
    if [[ "$GUEST_OS" == "macos" ]]; then
        SETUP_SCRIPT_NAME="setup-lan-guest-macos.sh"
    else
        SETUP_SCRIPT_NAME="setup-lan.sh"
    fi
}

# ── Instance management ──

ip_to_slug() {
    echo "$1" | tr '.' '-'
}

instance_config() { echo "$CONFIG_DIR/$INSTANCE_SLUG.conf"; }
instance_label()  { echo "com.intendant.tunnel.$INSTANCE_SLUG"; }
instance_plist()  { echo "$REAL_HOME/Library/LaunchAgents/$(instance_label).plist"; }

next_free_port() {
    local base=$1 field=$2
    local used=()
    if [[ -d "$CONFIG_DIR" ]]; then
        for conf in "$CONFIG_DIR"/*.conf; do
            [[ -f "$conf" ]] || continue
            # Don't count the current instance's own port during reconfiguration
            [[ -n "$INSTANCE_SLUG" && "$conf" == "$CONFIG_DIR/$INSTANCE_SLUG.conf" ]] && continue
            local port
            port=$(grep "^${field}=" "$conf" 2>/dev/null | cut -d= -f2)
            [[ -n "$port" ]] && used+=("$port")
        done
    fi
    if [[ ${#used[@]} -eq 0 ]]; then
        echo "$base"
        return
    fi
    local candidate=$base
    while printf '%s\n' "${used[@]}" | grep -qx "$candidate"; do
        candidate=$((candidate + 1))
    done
    echo "$candidate"
}

select_instance() {
    local configs=()
    if [[ -d "$CONFIG_DIR" ]]; then
        for conf in "$CONFIG_DIR"/*.conf; do
            [[ -f "$conf" ]] || continue
            configs+=("$conf")
        done
    fi

    if [[ ${#configs[@]} -eq 0 ]]; then
        die "no VMs configured — run the setup wizard first"
    fi

    if [[ ${#configs[@]} -eq 1 ]]; then
        INSTANCE_SLUG=$(basename "${configs[0]}" .conf)
        local _name _ip
        _name=$(grep '^INSTANCE_NAME=' "${configs[0]}" 2>/dev/null | cut -d'"' -f2)
        _ip=$(grep '^VM_IP=' "${configs[0]}" 2>/dev/null | cut -d'"' -f2)
        local display="$_ip"
        [[ -n "$_name" ]] && display="$_name ($_ip)"
        info "using $display"
        return
    fi

    echo ""
    echo "  Which VM?"
    echo ""
    local i=0
    for conf in "${configs[@]}"; do
        i=$((i + 1))
        local _name _ip _os _port
        _name=$(grep '^INSTANCE_NAME=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _ip=$(grep '^VM_IP=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _os=$(grep '^GUEST_OS=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _port=$(grep '^HTTPS_PORT=' "$conf" 2>/dev/null | cut -d= -f2)
        local display="$_ip"
        [[ -n "$_name" ]] && display="$_name ($_ip)"
        echo "    $i) $display — $_os, port $_port"
    done
    echo ""

    while true; do
        local choice
        read -rp "  Choose [1-${#configs[@]}]: " choice
        if [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#configs[@]} )); then
            INSTANCE_SLUG=$(basename "${configs[$((choice - 1))]}" .conf)
            return
        fi
        echo "  Invalid choice, try again."
    done
}

# ── Legacy migration ──

migrate_legacy() {
    local old="$REAL_HOME/.intendant-lan.conf"
    [[ -f "$old" ]] || return 0

    info "migrating existing LAN config to multi-VM format..."
    mkdir -p "$CONFIG_DIR"
    chown "$REAL_USER" "$CONFIG_DIR" 2>/dev/null || true

    # shellcheck disable=SC1090
    source "$old"

    # Fill in fields that didn't exist in the old format
    CERT_PORT="${CERT_PORT:-9999}"
    INSTANCE_NAME="${INSTANCE_NAME:-}"
    GUEST_OS="${GUEST_OS:-linux}"
    INSTANCE_SLUG=$(ip_to_slug "$VM_IP")
    set_guest_script

    save_config

    # Migrate launchd plist (old format used un-slugged label)
    local old_plist="$REAL_HOME/Library/LaunchAgents/com.intendant.tunnel.plist"
    if [[ -f "$old_plist" ]]; then
        as_user launchctl unload "$old_plist" 2>/dev/null || true
        rm -f "$old_plist"
        if [[ "$NET_MODE" == "shared" ]]; then
            setup_tunnel
        fi
    fi

    rm -f "$old" "$REAL_HOME/.intendant-tunnel.log"
    info "migrated → $CONFIG_DIR/$INSTANCE_SLUG.conf"

    # Reset state so it doesn't leak into subsequent operations
    INSTANCE_SLUG=""
    VM_IP=""
    VM_USER=""
    HTTPS_PORT=8443
    CERT_PORT=9999
    NET_MODE=""
    INSTANCE_NAME=""
    GUEST_OS="linux"
    SETUP_SCRIPT_NAME="setup-lan.sh"
}

# ── Config persistence ──

save_config() {
    mkdir -p "$CONFIG_DIR"
    chown "$REAL_USER" "$CONFIG_DIR" 2>/dev/null || true
    cat > "$(instance_config)" <<CFG
VM_IP="$VM_IP"
VM_USER="$VM_USER"
HTTPS_PORT=$HTTPS_PORT
CERT_PORT=$CERT_PORT
NET_MODE="$NET_MODE"
LAN_IFACE="$LAN_IFACE"
GUEST_OS="$GUEST_OS"
INSTANCE_NAME="$INSTANCE_NAME"
CFG
    chown "$REAL_USER" "$(instance_config)" 2>/dev/null || true
}

load_config() {
    local conf
    conf="$(instance_config)"
    [[ -f "$conf" ]] || return 1
    # shellcheck disable=SC1090
    source "$conf"
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

run_guest_script() {
    local args="$1"
    if [[ "$GUEST_OS" == "linux" ]]; then
        run_on_guest "sudo /tmp/$SETUP_SCRIPT_NAME $args"
    else
        # Homebrew isn't in PATH for SSH sessions (macOS only sources .zprofile for login shells)
        run_on_guest "export PATH=/opt/homebrew/bin:/usr/local/bin:\$PATH; /tmp/$SETUP_SCRIPT_NAME $args"
    fi
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
    as_user ssh-copy-id -o StrictHostKeyChecking=accept-new "${VM_USER}@${VM_IP}" >/dev/null
    info "SSH keys configured"
}

# ── SSH tunnel via launchd ──

setup_tunnel() {
    info "setting up SSH tunnel service..."

    local label plist
    label=$(instance_label)
    plist=$(instance_plist)

    mkdir -p "$CONFIG_DIR"
    as_user mkdir -p "$REAL_HOME/Library/LaunchAgents"

    # Unload existing if present
    as_user launchctl unload "$plist" 2>/dev/null || true

    cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$label</string>
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
    <string>${CONFIG_DIR}/${INSTANCE_SLUG}.log</string>
</dict>
</plist>
PLIST
    chown "$REAL_USER" "$plist" 2>/dev/null || true

    as_user launchctl load "$plist"
    info "tunnel service started — forwarding 0.0.0.0:{$HTTPS_PORT,$CERT_PORT} → VM"
}

remove_tunnel() {
    local plist
    plist=$(instance_plist)
    if [[ -f "$plist" ]]; then
        as_user launchctl unload "$plist" 2>/dev/null || true
        rm -f "$plist"
        info "tunnel service removed"
    else
        info "no tunnel service found"
    fi
    rm -f "$CONFIG_DIR/$INSTANCE_SLUG.log"
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

print_existing() {
    [[ -d "$CONFIG_DIR" ]] || return 0
    local found=false
    for conf in "$CONFIG_DIR"/*.conf; do
        [[ -f "$conf" ]] || continue
        if ! $found; then
            echo ""
            echo "  Already configured:"
            found=true
        fi
        local _name _ip _os _port
        _name=$(grep '^INSTANCE_NAME=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _ip=$(grep '^VM_IP=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _os=$(grep '^GUEST_OS=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _port=$(grep '^HTTPS_PORT=' "$conf" 2>/dev/null | cut -d= -f2)
        local display="$_ip"
        [[ -n "$_name" ]] && display="$_name ($_ip)"
        echo "    • $display — $_os, port $_port"
    done
}

run_wizard() {
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Intendant LAN Access Setup (macOS Host)"
    echo "════════════════════════════════════════════════════════"

    print_existing

    # Step 1: Guest OS
    local os_choice=0
    ask_choice "What OS is your VM running?" \
        "Linux (Debian/Ubuntu)" \
        "macOS" \
        || os_choice=$?
    if [[ "$os_choice" -eq 0 ]]; then
        GUEST_OS="linux"
    else
        GUEST_OS="macos"
    fi
    set_guest_script

    # Step 2: Network mode
    local net_choice=0
    ask_choice "How is your VM networked?" \
        "Shared Network (NAT — default VM setting)" \
        "Bridged — VM has its own LAN IP" \
        || net_choice=$?
    if [[ "$net_choice" -eq 0 ]]; then
        NET_MODE="shared"
    else
        NET_MODE="bridged"
    fi

    # Step 3: VM details
    echo ""
    VM_IP=$(ask "VM IP address")
    [[ -n "$VM_IP" ]] || die "IP address is required"

    # Check for existing config with this IP
    INSTANCE_SLUG=$(ip_to_slug "$VM_IP")
    local is_reconfig=false
    local default_user="$REAL_USER"
    if [[ -f "$(instance_config)" ]]; then
        local _name _os _port
        _name=$(grep '^INSTANCE_NAME=' "$(instance_config)" 2>/dev/null | cut -d'"' -f2)
        _os=$(grep '^GUEST_OS=' "$(instance_config)" 2>/dev/null | cut -d'"' -f2)
        _port=$(grep '^HTTPS_PORT=' "$(instance_config)" 2>/dev/null | cut -d= -f2)
        local display="$VM_IP"
        [[ -n "$_name" ]] && display="$_name ($VM_IP)"
        warn "already configured: $display — $_os, port $_port"
        local replace
        replace=$(ask "Reconfigure this VM? (y/n)" "y")
        [[ "$replace" == "y" ]] || die "setup cancelled"
        is_reconfig=true
        # Load defaults from existing config (ports, name, user)
        default_user=$(grep '^VM_USER=' "$(instance_config)" 2>/dev/null | cut -d'"' -f2)
        default_user="${default_user:-$REAL_USER}"
        HTTPS_PORT=$(grep '^HTTPS_PORT=' "$(instance_config)" 2>/dev/null | cut -d= -f2)
        HTTPS_PORT="${HTTPS_PORT:-8443}"
        CERT_PORT=$(grep '^CERT_PORT=' "$(instance_config)" 2>/dev/null | cut -d= -f2)
        CERT_PORT="${CERT_PORT:-9999}"
        INSTANCE_NAME=$(grep '^INSTANCE_NAME=' "$(instance_config)" 2>/dev/null | cut -d'"' -f2)
    fi

    VM_USER=$(ask "SSH username on the VM" "$default_user")

    # Step 4: Test SSH
    info "testing SSH connection to ${VM_USER}@${VM_IP}..."
    if ! test_ssh; then
        warn "could not connect via SSH"
        echo ""
        echo "  Make sure:"
        echo "    - The VM is running"
        if [[ "$GUEST_OS" == "linux" ]]; then
            echo "    - SSH server is installed: sudo apt install openssh-server"
        else
            echo "    - Remote Login is enabled: System Settings → General → Sharing → Remote Login"
        fi
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
                as_user ssh-copy-id -o StrictHostKeyChecking=accept-new "${VM_USER}@${VM_IP}" >/dev/null
                info "SSH keys configured — no more password prompts"
            fi
        fi
    fi

    # Step 5: Port
    echo ""
    if ! $is_reconfig; then
        local suggested_https
        suggested_https=$(next_free_port 8443 HTTPS_PORT)
        HTTPS_PORT=$(ask "HTTPS port for phone access" "$suggested_https")
        CERT_PORT=$(next_free_port 9999 CERT_PORT)
    else
        HTTPS_PORT=$(ask "HTTPS port for phone access" "$HTTPS_PORT")
    fi

    # Step 6: Name
    INSTANCE_NAME=$(ask "Name for this VM (optional)" "${INSTANCE_NAME:-}")

    # Step 7: Detect host network
    detect_lan_iface
    detect_lan_ip

    # Step 8: Confirm
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Setup Summary"
    echo "════════════════════════════════════════════════════════"
    echo ""
    [[ -n "$INSTANCE_NAME" ]] && echo "  Name:          $INSTANCE_NAME"
    echo "  Guest OS:      $GUEST_OS"
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

    # Step 9: Execute
    echo ""

    # SSH tunnel for shared networking (before guest setup so cert port is reachable)
    if [[ "$NET_MODE" == "shared" ]]; then
        setup_tunnel
    fi

    # UTM shared networking often lacks IPv6 routing — tell apt to use IPv4
    if [[ "$NET_MODE" == "shared" && "$GUEST_OS" == "linux" ]]; then
        info "configuring apt to prefer IPv4 on VM..."
        run_on_guest "echo 'Acquire::ForceIPv4 \"true\";' | sudo tee /etc/apt/apt.conf.d/99force-ipv4 >/dev/null"
    fi

    # Save config early — the guest setup ends with an interactive cert server
    # (Ctrl+C to stop), which causes a non-zero exit that would skip save_config
    # under set -e if we waited until after run_on_guest.
    save_config
    info "config saved to $(instance_config)"

    info "copying setup script to VM..."
    copy_to_guest

    info "running setup on VM..."
    local guest_args="--port $HTTPS_PORT"
    [[ "$NET_MODE" == "shared" ]] && guest_args="$guest_args --lan-ip $LAN_IP"
    local guest_ok=true
    run_guest_script "$guest_args" || guest_ok=false

    echo ""
    if $guest_ok; then
        echo "════════════════════════════════════════════════════════"
        echo "  Setup complete!"
        echo "════════════════════════════════════════════════════════"
        echo ""
        echo "  Phone connects to: https://${LAN_IP}:${HTTPS_PORT}"
        echo ""
        if [[ "$NET_MODE" == "shared" ]]; then
            info "SSH tunnel survives reboot (launchd service)"
            info "  Logs: $CONFIG_DIR/$INSTANCE_SLUG.log"
        fi
    else
        echo "════════════════════════════════════════════════════════"
        echo "  Host setup complete — VM setup failed"
        echo "════════════════════════════════════════════════════════"
        echo ""
        echo "  The SSH tunnel and config are in place, but the"
        echo "  VM-side setup did not complete. SSH in and run:"
        if [[ "$GUEST_OS" == "linux" ]]; then
            echo "    sudo /tmp/$SETUP_SCRIPT_NAME $guest_args"
        else
            echo "    /tmp/$SETUP_SCRIPT_NAME $guest_args"
        fi
        echo ""
        if [[ "$NET_MODE" == "shared" ]]; then
            info "SSH tunnel is running — Logs: $CONFIG_DIR/$INSTANCE_SLUG.log"
        fi
    fi
}

# ── Maintenance ──

run_list() {
    if [[ ! -d "$CONFIG_DIR" ]] || ! ls "$CONFIG_DIR"/*.conf &>/dev/null; then
        info "no VMs configured"
        return
    fi

    echo ""
    echo "  Configured VMs:"
    echo ""
    for conf in "$CONFIG_DIR"/*.conf; do
        [[ -f "$conf" ]] || continue
        local _name _ip _os _port _net
        _name=$(grep '^INSTANCE_NAME=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _ip=$(grep '^VM_IP=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _os=$(grep '^GUEST_OS=' "$conf" 2>/dev/null | cut -d'"' -f2)
        _port=$(grep '^HTTPS_PORT=' "$conf" 2>/dev/null | cut -d= -f2)
        _net=$(grep '^NET_MODE=' "$conf" 2>/dev/null | cut -d'"' -f2)
        local display="$_ip"
        [[ -n "$_name" ]] && display="$_name ($_ip)"
        echo "    • $display — $_os, $_net, port $_port"
    done
    echo ""
}

run_recert() {
    select_instance
    load_config || die "could not load config"
    set_guest_script

    detect_lan_iface
    detect_lan_ip

    if [[ "$NET_MODE" == "shared" ]]; then
        info "restarting SSH tunnel..."
        setup_tunnel
    fi

    local recert_args="--recert"
    $FORCE && recert_args="$recert_args --force"
    [[ "$NET_MODE" == "shared" ]] && recert_args="$recert_args --lan-ip $LAN_IP"

    info "copying setup script to VM..."
    copy_to_guest

    info "regenerating server cert on VM..."
    run_guest_script "$recert_args"

    echo ""
    info "done — phone connects to: https://${LAN_IP}:${HTTPS_PORT}"
}

run_remove() {
    select_instance
    if ! load_config; then
        warn "could not load config — removing local setup only"
        remove_tunnel
        rm -f "$(instance_config)"
        info "done (VM-side config may need manual removal)"
        return
    fi
    set_guest_script

    local display="$VM_IP"
    [[ -n "$INSTANCE_NAME" ]] && display="$INSTANCE_NAME ($VM_IP)"
    info "removing LAN setup for $display..."

    if [[ "$NET_MODE" == "shared" ]]; then
        remove_tunnel
    fi

    info "removing VM-side config..."
    if ! run_guest_script "--remove" 2>/dev/null; then
        local manual_cmd="$SETUP_SCRIPT_NAME --remove"
        [[ "$GUEST_OS" == "linux" ]] && manual_cmd="sudo $manual_cmd"
        warn "could not remove VM config — run '$manual_cmd' manually in the VM"
    fi

    rm -f "$(instance_config)"
    info "done"
}

# ── Main ──

main() {
    parse_args "$@"
    check_macos
    migrate_legacy

    case "$ACTION" in
        setup)  run_wizard ;;
        list)   run_list ;;
        recert) run_recert ;;
        remove) run_remove ;;
    esac
}

main "$@"
