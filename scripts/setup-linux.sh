#!/usr/bin/env bash
#
# Intendant Linux dependency installer.
#
# Works on a fresh Debian 13 / Ubuntu installation with nothing pre-installed.
# Handles sudo access, system packages, Rust, wasm-pack, and the full build.
#
# Usage:
#   ./setup-linux.sh              # Install all dependencies and build
#   ./setup-linux.sh --check      # Check what's installed without changing anything
#   ./setup-linux.sh --no-build   # Install dependencies but skip compilation
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/.."

ACTION="install"
SKIP_BUILD=false

# ── Helpers ────────────────────────────────────────────────────────────────

die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
ok()   { printf '   \033[1;32m+\033[0m %s\n' "$1"; }
miss() { printf '   \033[1;31m-\033[0m %s -- %s\n' "$1" "$2"; }

has_cmd() { command -v "$1" &>/dev/null; }

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --check)    ACTION="check"; shift ;;
            --no-build) SKIP_BUILD=true; shift ;;
            -h|--help)  sed -n '3,12p' "$0" | sed 's/^# \?//'; exit 0 ;;
            *)          die "unknown option: $1" ;;
        esac
    done
}

# ── Distro detection ──────────────────────────────────────────────────────

detect_distro() {
    if [[ ! -f /etc/os-release ]]; then
        die "cannot detect distribution (no /etc/os-release)"
    fi
    # shellcheck disable=SC1091
    . /etc/os-release
    DISTRO_ID="${ID:-unknown}"
    DISTRO_NAME="${PRETTY_NAME:-$DISTRO_ID}"

    case "$DISTRO_ID" in
        debian|ubuntu|linuxmint|pop)
            info "detected $DISTRO_NAME"
            ;;
        *)
            warn "unsupported distribution: $DISTRO_NAME"
            warn "this script targets Debian/Ubuntu -- packages may differ"
            ;;
    esac
}

# ── Sudo ──────────────────────────────────────────────────────────────────

ensure_sudo() {
    # Already root -- nothing to check.
    if [[ $EUID -eq 0 ]]; then
        return
    fi

    if has_cmd sudo && sudo -n true 2>/dev/null; then
        return
    fi

    if has_cmd sudo; then
        # sudo exists but the user cannot run it passwordlessly.
        # Try with a password prompt -- if they're in the sudo group this works.
        if sudo true 2>/dev/null; then
            return
        fi
    fi

    echo ""
    echo "  Your user ($USER) does not have sudo access."
    echo ""
    echo "  Ask root to run:"
    echo ""
    echo "    usermod -aG sudo $USER"
    echo ""
    echo "  Then log out and back in, and re-run this script."
    echo ""
    exit 1
}

# ── System packages ───────────────────────────────────────────────────────

# All apt packages needed for building and running intendant.
APT_PACKAGES=(
    # Build essentials
    build-essential
    pkg-config
    git
    curl
    mold

    # Rust build dep for vpx-encode (ffi-generate needs libclang)
    libclang-dev

    # Rust build dep for openssl-sys (pkg-config finds libssl + headers)
    libssl-dev

    # PNG encoding/decoding (libpng)
    libpng-dev

    # VP8 encoding (libvpx)
    libvpx-dev

    # PipeWire development headers (Wayland capture)
    libpipewire-0.3-dev

    # X11 capture (x11rb links against libxcb)
    libxcb1-dev
    libxcb-shm0-dev
    libxcb-randr0-dev

    # X11 input injection
    xdotool

    # Display detection (xdpyinfo)
    x11-utils

    # ImageMagick (X11 screenshots for computer use)
    imagemagick

    # Video recording
    ffmpeg

    # XDG utilities (desktop portal interaction)
    xdg-utils

    # Virtual display + VNC (runtime, not build deps)
    xvfb
    x11vnc

    # Audio routing (PulseAudio tools for virtual audio bridge)
    pulseaudio-utils

    # AT-SPI accessibility (optional, used by test automation)
    libatspi2.0-dev
)

check_apt_packages() {
    local all_ok=true

    echo ""
    info "system packages (apt):"

    for pkg in "${APT_PACKAGES[@]}"; do
        if dpkg -s "$pkg" &>/dev/null; then
            ok "$pkg"
        else
            miss "$pkg" "apt install $pkg"
            all_ok=false
        fi
    done

    $all_ok
}

install_apt_packages() {
    local missing=()

    for pkg in "${APT_PACKAGES[@]}"; do
        if ! dpkg -s "$pkg" &>/dev/null; then
            missing+=("$pkg")
        fi
    done

    if [[ ${#missing[@]} -eq 0 ]]; then
        info "all system packages already installed"
        return
    fi

    info "installing ${#missing[@]} system packages..."
    sudo apt-get update -qq
    sudo apt-get install -y -qq "${missing[@]}"
}

# ── Rust ──────────────────────────────────────────────────────────────────

check_rust() {
    echo ""
    info "Rust toolchain:"

    if has_cmd rustc && has_cmd cargo; then
        ok "rustc $(rustc --version 2>/dev/null | cut -d' ' -f2)"
        ok "cargo $(cargo --version 2>/dev/null | cut -d' ' -f2)"
        return 0
    fi

    miss "Rust toolchain" "https://rustup.rs"
    return 1
}

install_rust() {
    if has_cmd rustc && has_cmd cargo; then
        info "Rust toolchain already installed"
        return
    fi

    info "installing Rust toolchain via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

    # Source cargo env for this session.
    # shellcheck disable=SC1091
    if [[ -f "$HOME/.cargo/env" ]]; then
        source "$HOME/.cargo/env"
    fi

    if ! has_cmd cargo; then
        die "cargo not found after rustup install -- check your PATH"
    fi

    ok "rustc $(rustc --version 2>/dev/null | cut -d' ' -f2)"
}

# ── wasm-pack ─────────────────────────────────────────────────────────────

check_wasm_pack() {
    echo ""
    info "WASM build tools:"

    if has_cmd wasm-pack; then
        ok "wasm-pack $(wasm-pack --version 2>/dev/null | cut -d' ' -f2)"
        return 0
    fi

    miss "wasm-pack" "cargo install wasm-pack"
    return 1
}

install_wasm_pack() {
    if has_cmd wasm-pack; then
        info "wasm-pack already installed"
        return
    fi

    info "installing wasm-pack (this may take a minute)..."
    cargo install wasm-pack
    ok "wasm-pack installed"
}

# ── Display session detection ─────────────────────────────────────────────

detect_display() {
    echo ""
    info "display environment:"

    local session_type="${XDG_SESSION_TYPE:-}"
    local wayland_display="${WAYLAND_DISPLAY:-}"
    local x_display="${DISPLAY:-}"

    if [[ -n "$wayland_display" ]]; then
        ok "Wayland session detected (WAYLAND_DISPLAY=$wayland_display)"
        ok "display backend: Wayland (portal + PipeWire capture)"
    elif [[ -n "$x_display" ]]; then
        ok "X11 session detected (DISPLAY=$x_display)"
        ok "display backend: X11 (XShm capture + xdotool input)"
    elif [[ "$session_type" == "wayland" ]]; then
        ok "Wayland session type detected (XDG_SESSION_TYPE=$session_type)"
        ok "display backend: Wayland (portal + PipeWire capture)"
    elif [[ "$session_type" == "x11" ]]; then
        ok "X11 session type detected (XDG_SESSION_TYPE=$session_type)"
        ok "display backend: X11 (XShm capture + xdotool input)"
    else
        warn "no display session detected (headless)"
        warn "intendant will auto-launch Xvfb for virtual displays"
        warn "for a desktop session, run this script from a graphical terminal"
    fi
}

# ── .env template ─────────────────────────────────────────────────────────

check_dotenv() {
    echo ""
    info "configuration:"

    if [[ -f "$REPO_ROOT/.env" ]]; then
        ok ".env file exists"

        # Check for placeholder keys.
        local has_real_key=false
        while IFS= read -r line; do
            case "$line" in
                \#*|"") continue ;;
                *_API_KEY=sk-*|*_API_KEY=AI*) has_real_key=true ;;
            esac
        done < "$REPO_ROOT/.env"

        if ! $has_real_key; then
            warn ".env contains only placeholder keys -- add a real API key before running"
        fi
        return 0
    fi

    miss ".env" "no API keys configured"
    return 1
}

create_dotenv() {
    if [[ -f "$REPO_ROOT/.env" ]]; then
        info ".env already exists, not overwriting"
        return
    fi

    info "creating .env template..."
    cat > "$REPO_ROOT/.env" << 'DOTENV'
# Intendant API keys — uncomment and fill in at least one.
#
# OPENAI_API_KEY=sk-your-key-here
# ANTHROPIC_API_KEY=sk-ant-your-key-here
# GEMINI_API_KEY=your-key-here
#
# Optional: default provider and model
# PROVIDER=openai
# MODEL_NAME=gpt-4.1
#
# Optional: OpenAI reasoning controls
# REASONING_EFFORT=medium
# REASONING_SUMMARY=auto
# STRUCTURED_OUTPUT=true
DOTENV
    ok ".env template created -- edit it to add your API key(s)"
}

# ── Build ─────────────────────────────────────────────────────────────────

build_wasm() {
    local wasm_dir="$REPO_ROOT/static/wasm-web"

    # WASM artifacts are pre-compiled in the repo. Only rebuild if they're
    # missing (e.g. fresh clone without LFS, or artifacts were deleted).
    if [[ -f "$wasm_dir/presence_web_bg.wasm" && -f "$wasm_dir/presence_web.js" ]]; then
        info "WASM artifacts already present, skipping rebuild"
        return
    fi

    info "building presence-web WASM..."
    (cd "$REPO_ROOT/crates/presence-web" && \
        wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web)
    ok "WASM build complete"
}

build_intendant() {
    info "building intendant (release)..."
    (cd "$REPO_ROOT" && cargo build --release)

    local bin_dir="$REPO_ROOT/target/release"

    # Symlink into /usr/local/bin so `command -v intendant` works for
    # downstream tools (e.g. setup-lan.bat invoking `intendant lan` over
    # SSH on this guest).
    info "linking intendant into /usr/local/bin..."
    sudo ln -sf "$bin_dir/intendant" /usr/local/bin/intendant
    sudo ln -sf "$bin_dir/intendant-runtime" /usr/local/bin/intendant-runtime

    echo ""
    ok "intendant          -> $bin_dir/intendant"
    ok "intendant-runtime  -> $bin_dir/intendant-runtime"
    ok "symlinked          -> /usr/local/bin/{intendant,intendant-runtime}"
}

# ── Check mode ────────────────────────────────────────────────────────────

run_check() {
    echo ""
    echo "================================================================"
    echo "  Intendant Linux Dependency Check"
    echo "================================================================"

    detect_distro

    local apt_ok rust_ok wasm_ok env_ok

    check_apt_packages && apt_ok=true || apt_ok=false
    check_rust         && rust_ok=true || rust_ok=false
    check_wasm_pack    && wasm_ok=true || wasm_ok=false
    check_dotenv       && env_ok=true || env_ok=false

    detect_display

    echo ""
    echo "----------------------------------------------------------------"

    if $apt_ok; then
        echo "  System packages:  ready"
    else
        echo "  System packages:  missing (run without --check to install)"
    fi

    if $rust_ok; then
        echo "  Rust toolchain:   ready"
    else
        echo "  Rust toolchain:   missing"
    fi

    if $wasm_ok; then
        echo "  WASM tools:       ready"
    else
        echo "  WASM tools:       missing"
    fi

    if $env_ok; then
        echo "  API keys:         configured"
    else
        echo "  API keys:         not configured"
    fi

    echo ""
}

# ── Install mode ──────────────────────────────────────────────────────────

run_install() {
    echo ""
    echo "================================================================"
    echo "  Intendant Linux Setup"
    echo "================================================================"

    detect_distro

    # Phase 1: sudo
    info "checking sudo access..."
    ensure_sudo
    ok "sudo access confirmed"

    # Phase 2: system packages
    echo ""
    info "checking system packages..."
    install_apt_packages
    ok "system packages ready"

    # Phase 3: Rust
    echo ""
    install_rust

    # Phase 4: wasm-pack
    echo ""
    install_wasm_pack

    # Phase 5: .env
    echo ""
    create_dotenv

    # Phase 6: display detection
    detect_display

    # Phase 7: build
    if $SKIP_BUILD; then
        echo ""
        info "skipping build (--no-build)"
    else
        echo ""
        build_wasm
        echo ""
        build_intendant
    fi

    # Done
    echo ""
    echo "================================================================"
    echo "  Setup complete!"
    echo "================================================================"
    echo ""
    echo "  Next steps:"
    echo ""

    if [[ ! -f "$REPO_ROOT/.env" ]] || ! grep -q '^[^#].*_API_KEY=' "$REPO_ROOT/.env" 2>/dev/null; then
        echo "  1. Add an API key to .env:"
        echo ""
        echo "       cd $REPO_ROOT"
        echo "       nano .env"
        echo ""
        echo "  2. Run intendant:"
    else
        echo "  Run intendant:"
    fi

    echo ""
    echo "       cd $REPO_ROOT"
    echo "       ./target/release/intendant \"your task here\""
    echo ""
    echo "  Other modes:"
    echo ""
    echo "       ./target/release/intendant --web             # Web dashboard"
    echo "       ./target/release/intendant --direct \"task\"   # Single-agent"
    echo "       ./target/release/intendant --no-tui \"task\"   # Headless"
    echo ""
}

# ── Main ──────────────────────────────────────────────────────────────────

main() {
    parse_args "$@"

    [[ "$(uname)" == "Linux" ]] || die "this script is for Linux"

    case "$ACTION" in
        check)   run_check ;;
        install) run_install ;;
    esac
}

main "$@"
