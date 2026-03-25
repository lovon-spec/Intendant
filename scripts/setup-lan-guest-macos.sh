#!/usr/bin/env bash
#
# Intendant LAN Access Setup for macOS
# Sets up mTLS nginx reverse proxy for secure phone access to intendant --web.
#
# Usage:
#   ./setup-lan-guest-macos.sh                  # Interactive setup
#   ./setup-lan-guest-macos.sh --recert         # Regenerate server cert (IP changed)
#   ./setup-lan-guest-macos.sh --remove         # Uninstall everything
#
# This is the macOS equivalent of setup-lan.sh (for Linux). Run it on the
# machine where intendant is running. For macOS host → Linux guest setups,
# use setup-lan-macos.sh instead.
#
set -euo pipefail

CERT_DIR="$HOME/.intendant/lan-certs"
NGINX_CONF_DIR="$(brew --prefix 2>/dev/null || echo /opt/homebrew)/etc/nginx/servers"
NGINX_CONF="$NGINX_CONF_DIR/intendant-lan.conf"
BACKEND="127.0.0.1:8765"
HTTPS_PORT=8443
CERT_SERVE_PORT=9999
ACTION="setup"
FORCE=false
LAN_IP=""
P12_PASS=""

die()  { echo "error: $*" >&2; exit 1; }
info() { echo ":: $*"; }
warn() { echo "!! $*" >&2; }

usage() {
    sed -n '3,13p' "$0" | sed 's/^# \?//'
    exit 0
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --port)    HTTPS_PORT="$2"; shift 2 ;;
            --backend) BACKEND="$2"; shift 2 ;;
            --recert)  ACTION="recert"; shift ;;
            --force)   FORCE=true; shift ;;
            --remove)  ACTION="remove"; shift ;;
            -h|--help) usage ;;
            *)         die "unknown option: $1" ;;
        esac
    done
}

# ── Platform checks ──────────────────────────────────────────────────────────

check_macos() {
    [[ "$(uname)" == "Darwin" ]] || die "this script is for macOS — use setup-lan.sh on Linux"
}

check_homebrew() {
    command -v brew &>/dev/null || die "Homebrew is required — install from https://brew.sh"
}

# ── Network detection ────────────────────────────────────────────────────────

detect_lan_ip() {
    if [[ -n "$LAN_IP" ]]; then
        info "LAN IP: $LAN_IP (override)"
        return
    fi
    # Get the default interface, then its IP
    local iface
    iface=$(route -n get default 2>/dev/null | awk '/interface:/ {print $2}')
    if [[ -n "$iface" ]]; then
        LAN_IP=$(ipconfig getifaddr "$iface" 2>/dev/null || true)
    fi
    [[ -n "$LAN_IP" ]] || die "could not detect LAN IP — pass it with --lan-ip"
    info "LAN IP: $LAN_IP"
}

# ── Certificate generation ───────────────────────────────────────────────────

generate_server_cert() {
    info "generating server cert for IP $LAN_IP..."
    openssl genrsa -out "$CERT_DIR/server.key" 2048 2>/dev/null
    openssl req -new \
        -key "$CERT_DIR/server.key" \
        -out "$CERT_DIR/server.csr" \
        -subj "/CN=$LAN_IP" 2>/dev/null

    # iOS requires: <=825 days, serverAuth EKU, CA:FALSE, SAN
    openssl x509 -req \
        -in "$CERT_DIR/server.csr" \
        -CA "$CERT_DIR/ca.crt" -CAkey "$CERT_DIR/ca.key" -CAcreateserial \
        -days 825 -out "$CERT_DIR/server.crt" \
        -extfile <(cat <<EXTFILE
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=IP:$LAN_IP
EXTFILE
        ) 2>/dev/null

    rm -f "$CERT_DIR/server.csr"
    info "server cert issued for $LAN_IP (valid 825 days)"
}

generate_certs() {
    if [[ -f "$CERT_DIR/ca.crt" && -f "$CERT_DIR/client.p12" ]]; then
        P12_PASS=$(cat "$CERT_DIR/p12_password" 2>/dev/null || echo "unknown")
        info "certs already exist in $CERT_DIR (run --remove to regenerate)"
        # Regenerate server cert if IP changed
        local old_ip
        old_ip=$(openssl x509 -in "$CERT_DIR/server.crt" -noout -text 2>/dev/null \
            | grep -o 'IP Address:[0-9.]*' | head -1 | cut -d: -f2 || echo "none")
        if [[ "$old_ip" != "$LAN_IP" ]]; then
            warn "IP changed ($old_ip -> $LAN_IP) — regenerating server cert"
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
        -subj "/CN=Intendant CA ($LAN_IP)" 2>/dev/null

    # Server cert (SAN required by modern iOS/browsers)
    generate_server_cert

    # Client cert
    openssl genrsa -out "$CERT_DIR/client.key" 2048 2>/dev/null
    openssl req -new \
        -key "$CERT_DIR/client.key" \
        -out "$CERT_DIR/client.csr" \
        -subj "/CN=Intendant Client ($LAN_IP)" 2>/dev/null
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

# ── nginx (Homebrew) ─────────────────────────────────────────────────────────

install_nginx() {
    if command -v nginx &>/dev/null; then
        info "nginx already installed"
    else
        info "installing nginx..."
        brew install nginx
    fi
}

write_nginx_config() {
    info "configuring nginx..."
    mkdir -p "$NGINX_CONF_DIR"

    cat > "$NGINX_CONF" <<NGINX
map \$http_upgrade \$connection_upgrade {
    default upgrade;
    '' close;
}

server {
    listen ${HTTPS_PORT} ssl;

    ssl_certificate     ${CERT_DIR}/server.crt;
    ssl_certificate_key ${CERT_DIR}/server.key;

    # mTLS — clients with a cert signed by our CA get access.
    ssl_client_certificate ${CERT_DIR}/ca.crt;
    ssl_verify_client optional;

    # Block requests without a valid client cert (except WebSocket upgrades)
    set \$auth_ok 0;
    if (\$ssl_client_verify = SUCCESS) { set \$auth_ok 1; }
    if (\$http_upgrade = websocket)    { set \$auth_ok 1; }
    if (\$auth_ok = 0) { return 403; }

    location / {
        proxy_pass http://${BACKEND};
        proxy_http_version 1.1;
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection \$connection_upgrade;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;

        # Keep WebSocket alive (24h)
        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;

        # Custom error page when intendant isn't running
        proxy_intercept_errors on;
        error_page 502 =502 @intendant_down;
    }

    location @intendant_down {
        default_type text/html;
        return 502 '<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Intendant</title><style>body{background:#1e1e2e;color:#cdd6f4;font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0}div{text-align:center}.status{font-size:1.5em;margin-bottom:.5em;color:#f9e2af}p{color:#a6adc8}</style></head><body><div><div class="status">Waiting for intendant</div><p>The agent is not running yet. Start it with:<br><code style="color:#89b4fa">intendant --web</code></p><script>setTimeout(()=>location.reload(),3000)</script></div></body></html>';
    }
}
NGINX

    # Verify config
    nginx -t 2>/dev/null || die "nginx config test failed"
    brew services restart nginx
    info "nginx listening on https://0.0.0.0:$HTTPS_PORT (mTLS)"
}

# ── Certificate serving & instructions ───────────────────────────────────────

print_instructions_ios() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Open Safari -> http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Settings -> General -> VPN & Device Management -> Install
    Settings -> General -> About -> Certificate Trust Settings -> Enable

  Step 2 — Install client certificate:

    Open Safari -> http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Password:  ${P12_PASS}

    Settings -> General -> VPN & Device Management -> Install
EOF
}

print_instructions_android() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Open Chrome -> http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Settings -> Security -> Encryption & Credentials
      -> Install a certificate -> CA certificate

  Step 2 — Install client certificate:

    Open Chrome -> http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Password:  ${P12_PASS}

    Settings -> Security -> Encryption & Credentials
      -> Install a certificate -> VPN & app user certificate
EOF
}

print_instructions_mac() {
    cat <<EOF

  Step 1 — Install CA certificate:

    Download: http://${LAN_IP}:${CERT_SERVE_PORT}/ca.crt

    Double-click -> opens Keychain Access -> add to "login" keychain
    Find "Intendant CA" -> Get Info -> Trust -> "Always Trust"

  Step 2 — Install client certificate:

    Download: http://${LAN_IP}:${CERT_SERVE_PORT}/client.p12

    Double-click -> opens Keychain Access -> enter password

    Password:  ${P12_PASS}

  Restart your browser after importing.
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
    3) Mac / Desktop  (Chrome / Safari)
    4) Show all

EOF

    local choice
    read -rp "  Choose [1-4]: " choice
    echo ""

    case "$choice" in
        1) print_instructions_ios ;;
        2) print_instructions_android ;;
        3) print_instructions_mac ;;
        4|*)
            echo "  -- iPhone / iPad (Safari) --"
            print_instructions_ios
            echo ""
            echo "  -- Android (Chrome) --"
            print_instructions_android
            echo ""
            echo "  -- Mac / Desktop --"
            print_instructions_mac
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

# ── Recert ───────────────────────────────────────────────────────────────────

recert() {
    [[ -f "$CERT_DIR/ca.key" ]] || die "no CA found in $CERT_DIR — run full setup first"

    detect_lan_ip

    local old_ip
    old_ip=$(openssl x509 -in "$CERT_DIR/server.crt" -noout -text 2>/dev/null \
        | grep -o 'IP Address:[0-9.]*' | head -1 | cut -d: -f2 || echo "unknown")

    if [[ "$old_ip" == "$LAN_IP" ]] && ! $FORCE; then
        info "server cert already matches $LAN_IP — nothing to do (use --force to regenerate)"
        exit 0
    fi

    info "IP changed: $old_ip -> $LAN_IP"
    generate_server_cert
    brew services restart nginx
    info "done — nginx restarted with new cert"
    info "no changes needed on your phone (same CA)"
}

# ── Remove ───────────────────────────────────────────────────────────────────

remove_all() {
    info "removing intendant LAN setup..."

    # nginx config
    if [[ -f "$NGINX_CONF" ]]; then
        rm -f "$NGINX_CONF"
        if brew services list | grep -q 'nginx.*started'; then
            brew services restart nginx
        fi
        info "nginx config removed"
    fi

    # certs
    if [[ -d "$CERT_DIR" ]]; then
        local yn
        read -rp "Remove certificates in $CERT_DIR? [y/N] " yn
        if [[ "$yn" == [yY] ]]; then
            rm -rf "$CERT_DIR"
            info "certificates removed"
        fi
    fi

    info "done"
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    parse_args "$@"
    check_macos
    check_homebrew

    case "$ACTION" in
        remove) remove_all; exit 0 ;;
        recert) recert; exit 0 ;;
    esac

    # Full setup
    detect_lan_ip
    install_nginx
    generate_certs
    write_nginx_config
    serve_certs
}

main "$@"
