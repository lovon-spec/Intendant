#!/usr/bin/env bash
#
# Intendant LAN Access Setup
# Sets up mTLS nginx reverse proxy for secure phone access to intendant --web.
#
# Usage:
#   sudo ./setup-lan.sh                                # Direct: intendant on this machine
#   sudo ./setup-lan.sh --tunnel user@192.168.122.163  # VM: auto-tunnel to guest
#   sudo ./setup-lan.sh --backend 10.0.0.5:8765        # Manual: proxy to another host
#   sudo ./setup-lan.sh --recert                       # Regenerate server cert (IP changed)
#   sudo ./setup-lan.sh --remove                       # Uninstall everything
#
# Security tiers:
#   Trusted LAN  — mTLS (this script)
#   Public WiFi   — WireGuard + mTLS
#   NAT traversal — Tailscale
#
set -euo pipefail

CERT_DIR="/etc/intendant-lan"
NGINX_SITE="intendant-lan"
TUNNEL_SERVICE="intendant-tunnel"
BACKEND="localhost:8765"
HTTPS_PORT=8443
CERT_SERVE_PORT=9999
TUNNEL_TARGET=""
ACTION="setup"
LAN_IP=""
P12_PASS=""

die()   { echo "error: $*" >&2; exit 1; }
info()  { echo ":: $*"; }
warn()  { echo "!! $*" >&2; }

usage() {
    sed -n '3,16p' "$0" | sed 's/^# \?//'
    exit 0
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --backend)  BACKEND="$2"; shift 2 ;;
            --tunnel)   TUNNEL_TARGET="$2"; shift 2 ;;
            --port)     HTTPS_PORT="$2"; shift 2 ;;
            --recert)   ACTION="recert"; shift ;;
            --remove)   ACTION="remove"; shift ;;
            -h|--help)  usage ;;
            *)          die "unknown option: $1" ;;
        esac
    done
}

detect_lan_ip() {
    LAN_IP=$(hostname -I | awk '{print $1}')
    [[ -n "$LAN_IP" ]] || die "could not detect LAN IP"
    info "LAN IP: $LAN_IP"
}

generate_server_cert() {
    info "generating server cert for IP $LAN_IP..."
    openssl genrsa -out "$CERT_DIR/server.key" 2048 2>/dev/null
    openssl req -new \
        -key "$CERT_DIR/server.key" \
        -out "$CERT_DIR/server.csr" \
        -subj "/CN=$LAN_IP" 2>/dev/null
    openssl x509 -req \
        -in "$CERT_DIR/server.csr" \
        -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" -CAcreateserial \
        -days 3650 -out "$CERT_DIR/server.crt" \
        -extfile <(echo "subjectAltName=IP:$LAN_IP") 2>/dev/null
    rm -f "$CERT_DIR/server.csr"
    info "server cert issued for $LAN_IP"
}

recert() {
    [[ $(id -u) -eq 0 ]] || die "run with sudo"
    [[ -f "$CERT_DIR/ca.key" ]] || die "no CA found in $CERT_DIR — run full setup first"

    detect_lan_ip

    local old_ip
    old_ip=$(openssl x509 -in "$CERT_DIR/server.crt" -noout -ext subjectAltName 2>/dev/null \
        | grep -oP 'IP Address:\K[0-9.]+' || echo "unknown")

    if [[ "$old_ip" == "$LAN_IP" ]]; then
        info "server cert already matches $LAN_IP — nothing to do"
        exit 0
    fi

    info "IP changed: $old_ip → $LAN_IP"
    generate_server_cert
    systemctl restart nginx
    info "done — nginx restarted with new cert"
    info "no changes needed on your phone (same CA)"
}

generate_certs() {
    if [[ -f "$CERT_DIR/ca.crt" && -f "$CERT_DIR/client.p12" ]]; then
        P12_PASS=$(cat "$CERT_DIR/p12_password" 2>/dev/null || echo "unknown")
        info "certs already exist in $CERT_DIR (run --remove to regenerate)"
        # Still regenerate server cert if IP changed
        local old_ip
        old_ip=$(openssl x509 -in "$CERT_DIR/server.crt" -noout -ext subjectAltName 2>/dev/null \
            | grep -oP 'IP Address:\K[0-9.]+' || echo "none")
        if [[ "$old_ip" != "$LAN_IP" ]]; then
            warn "IP changed ($old_ip → $LAN_IP) — regenerating server cert"
            generate_server_cert
        fi
        return
    fi

    info "generating certificates..."
    mkdir -p "$CERT_DIR"

    # CA
    openssl genrsa -out "$CERT_DIR/ca.key" 2048 2>/dev/null
    openssl req -x509 -new -nodes \
        -key "$CERT_DIR/ca.key" \
        -days 3650 -out "$CERT_DIR/ca.crt" \
        -subj "/CN=Intendant CA" 2>/dev/null

    # Server cert (SAN required by modern iOS/browsers)
    generate_server_cert

    # Client cert
    openssl genrsa -out "$CERT_DIR/client.key" 2048 2>/dev/null
    openssl req -new \
        -key "$CERT_DIR/client.key" \
        -out "$CERT_DIR/client.csr" \
        -subj "/CN=Intendant Client" 2>/dev/null
    openssl x509 -req \
        -in "$CERT_DIR/client.csr" \
        -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" -CAcreateserial \
        -days 3650 -out "$CERT_DIR/client.crt" 2>/dev/null

    # .p12 for iOS (must have a password)
    P12_PASS=$(head -c 16 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 8)
    echo "$P12_PASS" > "$CERT_DIR/p12_password"
    chmod 600 "$CERT_DIR/p12_password"

    openssl pkcs12 -export \
        -out "$CERT_DIR/client.p12" \
        -inkey "$CERT_DIR/client.key" \
        -in "$CERT_DIR/client.crt" \
        -certfile "$CERT_DIR/ca.crt" \
        -passout "pass:$P12_PASS" 2>/dev/null

    # Cleanup intermediates
    rm -f "$CERT_DIR"/*.csr "$CERT_DIR"/*.srl

    info "certificates generated in $CERT_DIR"
}

install_nginx() {
    if command -v nginx &>/dev/null; then
        info "nginx already installed"
    else
        info "installing nginx..."
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nginx
    fi
}

write_nginx_config() {
    info "configuring nginx..."

    cat > "/etc/nginx/sites-available/$NGINX_SITE" <<NGINX
server {
    listen ${HTTPS_PORT} ssl;

    ssl_certificate     ${CERT_DIR}/server.crt;
    ssl_certificate_key ${CERT_DIR}/server.key;

    # mTLS — only clients with a cert signed by our CA can connect
    ssl_client_certificate ${CERT_DIR}/ca.crt;
    ssl_verify_client on;

    location / {
        proxy_pass http://${BACKEND};
        proxy_http_version 1.1;
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;

        # Keep WebSocket alive (24h)
        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;
    }
}
NGINX

    ln -sf "/etc/nginx/sites-available/$NGINX_SITE" /etc/nginx/sites-enabled/

    nginx -t 2>/dev/null || die "nginx config test failed"
    systemctl restart nginx
    info "nginx listening on https://0.0.0.0:$HTTPS_PORT (mTLS)"
}

setup_tunnel() {
    [[ -z "$TUNNEL_TARGET" ]] && return

    # With nginx handling external access, tunnel only needs localhost
    BACKEND="localhost:8765"

    info "setting up SSH tunnel to $TUNNEL_TARGET..."

    # Determine which user invoked sudo
    local run_user="${SUDO_USER:-root}"
    local run_home
    run_home=$(eval echo "~$run_user")

    # Ensure key-based auth works
    local ssh_key="$run_home/.ssh/id_ed25519"
    if ! sudo -u "$run_user" ssh -o BatchMode=yes -o ConnectTimeout=5 "$TUNNEL_TARGET" true 2>/dev/null; then
        if [[ ! -f "$ssh_key" ]]; then
            info "generating SSH key for $run_user..."
            sudo -u "$run_user" ssh-keygen -t ed25519 -f "$ssh_key" -N "" -q
        fi
        info "copying SSH key to $TUNNEL_TARGET (password prompt)..."
        sudo -u "$run_user" ssh-copy-id -i "${ssh_key}.pub" "$TUNNEL_TARGET"
    fi

    # Check if port 8765 is already in use
    if ss -tlnp | grep -q ":8765 " 2>/dev/null; then
        warn "port 8765 already in use — stop your manual SSH tunnel first"
        warn "(the systemd service will manage the tunnel from now on)"
        info "continuing anyway — service will retry until port is free"
    fi

    cat > "/etc/systemd/system/${TUNNEL_SERVICE}.service" <<SERVICE
[Unit]
Description=Intendant SSH tunnel to VM guest
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${run_user}
ExecStart=/usr/bin/ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -L 127.0.0.1:8765:localhost:8765 ${TUNNEL_TARGET}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE

    systemctl daemon-reload
    systemctl enable --now "$TUNNEL_SERVICE"
    info "tunnel service started (systemctl status $TUNNEL_SERVICE)"
}

print_instructions_ios() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Open Safari → http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Settings → General → VPN & Device Management → Install
    Settings → General → About → Certificate Trust Settings → Enable

  Step 2 — Install client certificate:

    Open Safari → http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Password:  ${P12_PASS}

    Settings → General → VPN & Device Management → Install
EOF
}

print_instructions_android() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Open Chrome → http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Settings → Security → Encryption & Credentials
      → Install a certificate → CA certificate

  Step 2 — Install client certificate:

    Open Chrome → http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Password:  ${P12_PASS}

    Settings → Security → Encryption & Credentials
      → Install a certificate → VPN & app user certificate

    (If .p12 doesn't work, try: http://${LAN_IP}:${CERT_SERVE_PORT}/client.pfx)
EOF
}

print_instructions_firefox() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Option A (import from browser):
      Settings → Privacy & Security → Certificates → View Certificates
      → Authorities tab → Import → select ca.crt
      → Check "Trust this CA to identify websites"

    Option B (download):
      Open http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt
      Firefox may prompt to trust it directly.

  Step 2 — Install client certificate:

    Settings → Privacy & Security → Certificates → View Certificates
    → Your Certificates tab → Import → select client.p12

    Password:  ${P12_PASS}

    (Download from: http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12)
EOF
}

print_instructions_chrome_linux() {
    cat <<EOF

  Step 1 — Install CA certificate (run in terminal):

    certutil -d sql:\$HOME/.pki/nssdb -A -t "C,," \\
      -n "Intendant CA" -i <(curl -s http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt)

    (Install libnss3-tools if certutil is missing: sudo apt install libnss3-tools)

  Step 2 — Install client certificate (run in terminal):

    curl -so /tmp/client.p12 http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12
    pk12util -d sql:\$HOME/.pki/nssdb -i /tmp/client.p12
    rm /tmp/client.p12

    Password:  ${P12_PASS}

  Restart Chrome after importing.
EOF
}

print_instructions_chrome_mac() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Download: http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Double-click → opens Keychain Access → add to "login" keychain
    Find "Intendant CA" → Get Info → Trust → "Always Trust"

  Step 2 — Install client certificate:

    Download: http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Double-click → opens Keychain Access → enter password

    Password:  ${P12_PASS}

  Restart Chrome after importing.
EOF
}

print_instructions_chrome_windows() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Download: http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Double-click → Install Certificate → Local Machine → "Trusted Root Certification Authorities"

    Or via PowerShell (admin):
      certutil.exe -addstore Root ca.crt

  Step 2 — Install client certificate:

    Download: http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Double-click → Import → enter password → place in "Personal"

    Password:  ${P12_PASS}

  Restart Chrome after importing.
EOF
}

serve_certs() {
    local serve_dir
    serve_dir=$(mktemp -d)
    cp "$CERT_DIR/ca.crt" "$serve_dir/"
    cp "$CERT_DIR/client.p12" "$serve_dir/"
    cp "$CERT_DIR/client.p12" "$serve_dir/client.pfx"  # Android compat

    cat <<EOF

════════════════════════════════════════════════════════
  Certificate Installation
════════════════════════════════════════════════════════

  What will you use to access intendant?

    1) iPhone / iPad  (Safari)
    2) Android        (Chrome)
    3) Desktop Firefox
    4) Desktop Chrome / Edge (Linux)
    5) Desktop Chrome / Edge (macOS)
    6) Desktop Chrome / Edge (Windows)
    7) Show all

EOF

    local choice
    read -rp "  Choose [1-7]: " choice
    echo ""

    case "$choice" in
        1) print_instructions_ios ;;
        2) print_instructions_android ;;
        3) print_instructions_firefox ;;
        4) print_instructions_chrome_linux ;;
        5) print_instructions_chrome_mac ;;
        6) print_instructions_chrome_windows ;;
        7|*)
            echo "  ── iPhone / iPad (Safari) ──"
            print_instructions_ios
            echo ""
            echo "  ── Android (Chrome) ──"
            print_instructions_android
            echo ""
            echo "  ── Desktop Firefox ──"
            print_instructions_firefox
            echo ""
            echo "  ── Desktop Chrome/Edge (Linux) ──"
            print_instructions_chrome_linux
            echo ""
            echo "  ── Desktop Chrome/Edge (macOS) ──"
            print_instructions_chrome_mac
            echo ""
            echo "  ── Desktop Chrome/Edge (Windows) ──"
            print_instructions_chrome_windows
            ;;
    esac

    cat <<EOF

  Then open:  https://${LAN_IP}:${HTTPS_PORT}

════════════════════════════════════════════════════════

  Serving certs at http://${LAN_IP}:${CERT_SERVE_PORT}/
  Press Ctrl+C when done.

EOF

    cd "$serve_dir"
    python3 -m http.server "$CERT_SERVE_PORT" --bind 0.0.0.0 2>/dev/null
    rm -rf "$serve_dir"
}

remove_all() {
    info "removing intendant LAN setup..."

    # nginx
    rm -f "/etc/nginx/sites-enabled/$NGINX_SITE"
    rm -f "/etc/nginx/sites-available/$NGINX_SITE"
    if systemctl is-active nginx &>/dev/null; then
        systemctl restart nginx
    fi

    # tunnel service
    if [[ -f "/etc/systemd/system/${TUNNEL_SERVICE}.service" ]]; then
        systemctl disable --now "$TUNNEL_SERVICE" 2>/dev/null || true
        rm -f "/etc/systemd/system/${TUNNEL_SERVICE}.service"
        systemctl daemon-reload
    fi

    # certs
    if [[ -d "$CERT_DIR" ]]; then
        read -rp "Remove certificates in $CERT_DIR? [y/N] " yn
        if [[ "$yn" == [yY] ]]; then
            rm -rf "$CERT_DIR"
            info "certificates removed"
        fi
    fi

    info "done"
}

main() {
    parse_args "$@"

    if [[ "$ACTION" == "remove" ]]; then
        [[ $(id -u) -eq 0 ]] || die "run with sudo"
        remove_all
        exit 0
    fi

    if [[ "$ACTION" == "recert" ]]; then
        recert
        exit 0
    fi

    [[ $(id -u) -eq 0 ]] || die "run with sudo"

    detect_lan_ip
    install_nginx
    [[ -n "$TUNNEL_TARGET" ]] && setup_tunnel
    generate_certs
    write_nginx_config
    serve_certs
}

main "$@"
