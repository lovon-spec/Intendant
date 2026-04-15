#!/usr/bin/env bash
#
# Intendant macOS dependency installer.
#
# Usage:
#   ./setup-macos.sh             # Install all dependencies and build
#   ./setup-macos.sh --check     # Check what's installed without changing anything
#
set -euo pipefail

die()  { echo "error: $*" >&2; exit 1; }
info() { echo ":: $*"; }
warn() { echo "!! $*" >&2; }
ok()   { echo "   ✓ $1"; }
miss() { echo "   ✗ $1 — $2"; }

ACTION="install"
NEEDS_REBOOT=false

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --check) ACTION="check"; shift ;;
            -h|--help) sed -n '3,8p' "$0" | sed 's/^# \?//'; exit 0 ;;
            *)       die "unknown option: $1" ;;
        esac
    done
}

# ── Checks ──────────────────────────────────────────────────────────────────

check_macos() {
    [[ "$(uname)" == "Darwin" ]] || die "this script is for macOS"
}

has_cmd() { command -v "$1" &>/dev/null; }

has_brew_pkg() { brew list --formula "$1" &>/dev/null 2>&1; }

# Check if a named audio device exists in the audio system
has_audio_device() {
    local name="$1"
    system_profiler SPAudioDataType 2>/dev/null | grep -q "$name"
}

# Legacy alias
has_blackhole() { has_audio_device "$1"; }

# ── Dependency definitions ──────────────────────────────────────────────────

# Core deps: needed for basic operation
check_core() {
    local all_ok=true

    echo ""
    echo "Core dependencies:"

    if has_cmd brew; then
        ok "Homebrew"
    else
        miss "Homebrew" "https://brew.sh"
        all_ok=false
    fi

    if has_cmd rustc && has_cmd cargo; then
        ok "Rust toolchain ($(rustc --version 2>/dev/null | cut -d' ' -f2))"
    else
        miss "Rust toolchain" "https://rustup.rs"
        all_ok=false
    fi

    if has_cmd bash; then
        ok "bash"
    else
        miss "bash" "should be pre-installed on macOS"
        all_ok=false
    fi

    # OpenSSL (build-time dep for the openssl-sys crate). Homebrew's
    # openssl@3 provides pkg-config metadata that openssl-sys finds at
    # build time — without it, cargo fails with
    # "Could not find openssl via pkg-config".
    if brew list --formula 2>/dev/null | grep -q '^openssl@3$'; then
        ok "openssl@3 (Homebrew)"
    else
        miss "openssl@3" "brew install openssl@3"
        all_ok=false
    fi

    $all_ok
}

# Computer-use deps: needed for display interaction
check_computer_use() {
    local all_ok=true

    echo ""
    echo "Computer-use dependencies:"

    if has_cmd screencapture; then
        ok "screencapture (built-in)"
    else
        miss "screencapture" "should be pre-installed on macOS"
        all_ok=false
    fi

    if has_cmd cliclick; then
        ok "cliclick"
    else
        miss "cliclick" "brew install cliclick"
        all_ok=false
    fi

    $all_ok
}

# Audio routing deps: needed for spawn_live_audio (voice calls through apps).
# Browser-based voice (Gemini Live / OpenAI Realtime via WebRTC) works without these.
#
# Two modes:
#   1. Vortex Audio (preferred): HAL plugin with direct shm bridge. No system
#      default changes, per-app routing, works in VMs.
#   2. BlackHole (fallback): Virtual loopback via system default switching.
#      Simpler setup but changes system-wide audio defaults during calls.
check_audio() {
    local all_ok=true

    echo ""
    echo "Audio routing dependencies:"

    if has_cmd SwitchAudioSource; then
        ok "SwitchAudioSource"
    else
        miss "SwitchAudioSource" "brew install switchaudio-osx"
        all_ok=false
    fi

    if has_cmd sox; then
        ok "sox"
    else
        miss "sox" "brew install sox"
        all_ok=false
    fi

    # Vortex Audio (preferred)
    if has_audio_device "Vortex Audio"; then
        ok "Vortex Audio (HAL plugin)"
        # Check if it's the default input
        local cur_input
        cur_input="$(SwitchAudioSource -c -t input 2>/dev/null)"
        if [[ "$cur_input" == "Vortex Audio" ]]; then
            ok "Vortex Audio is default input"
        else
            miss "Vortex Audio not default input" "current: $cur_input"
            all_ok=false
        fi
    else
        miss "Vortex Audio" "install Vortex guest tools (scripts/install-vortex-audio.sh)"
        # Fall back to checking BlackHole
        if has_blackhole "BlackHole 2ch"; then
            ok "BlackHole 2ch (fallback)"
        else
            miss "BlackHole 2ch" "brew install --cask blackhole-2ch (reboot required)"
            all_ok=false
        fi
        if has_blackhole "BlackHole 16ch"; then
            ok "BlackHole 16ch (fallback)"
        else
            miss "BlackHole 16ch" "brew install --cask blackhole-16ch (reboot required)"
            all_ok=false
        fi
    fi

    # TCC mic access
    echo ""
    echo "Audio permissions:"
    echo "   ⚠  macOS requires microphone permission for audio input."
    echo "      Launch Intendant.app from Finder (not SSH) and approve"
    echo "      the mic prompt on first run."

    $all_ok
}

# Recording deps
check_recording() {
    local all_ok=true

    echo ""
    echo "Recording dependencies:"

    if has_cmd ffmpeg; then
        ok "ffmpeg"
    else
        miss "ffmpeg" "brew install ffmpeg"
        all_ok=false
    fi

    $all_ok
}

# WASM build deps (required — build.rs auto-rebuilds WASM when source changes)
check_wasm() {
    echo ""
    echo "WASM build dependencies:"

    if has_cmd wasm-pack; then
        ok "wasm-pack"
    else
        miss "wasm-pack" "cargo install wasm-pack"
        info "installing wasm-pack..."
        cargo install wasm-pack
    fi
}

# ── Install ─────────────────────────────────────────────────────────────────

ensure_homebrew() {
    if has_cmd brew; then return; fi
    info "installing Homebrew..."
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    # Add to PATH for this session
    if [[ -f /opt/homebrew/bin/brew ]]; then
        eval "$(/opt/homebrew/bin/brew shellenv)"
    fi
}

ensure_rust() {
    if has_cmd rustc && has_cmd cargo; then return; fi
    info "installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
}

brew_install() {
    local pkg="$1"
    if has_brew_pkg "$pkg" || has_cmd "$pkg"; then return; fi
    info "installing $pkg..."
    brew install "$pkg"
}

install_vortex_audio() {
    if has_audio_device "Vortex Audio"; then return; fi

    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local vortex_tools="$script_dir/../vendor/vortex-guest-tools"
    local pkg="$vortex_tools/VortexGuestTools.pkg"

    if [[ -f "$pkg" ]]; then
        info "installing Vortex guest tools..."
        sudo installer -pkg "$pkg" -target /
        NEEDS_REBOOT=true
    else
        warn "Vortex guest tools not found at $pkg"
        warn "Audio routing will fall back to BlackHole."
        warn "To install Vortex: place VortexGuestTools.pkg in vendor/vortex-guest-tools/"
    fi
}

set_vortex_defaults() {
    if ! has_audio_device "Vortex Audio"; then return; fi
    if ! has_cmd SwitchAudioSource; then return; fi

    local cur_input cur_output
    cur_input="$(SwitchAudioSource -c -t input 2>/dev/null)"
    cur_output="$(SwitchAudioSource -c -t output 2>/dev/null)"

    if [[ "$cur_input" != "Vortex Audio" ]]; then
        info "setting Vortex Audio as default input..."
        SwitchAudioSource -s "Vortex Audio" -t input
    fi
    if [[ "$cur_output" != "Vortex Audio" ]]; then
        info "setting Vortex Audio as default output..."
        SwitchAudioSource -s "Vortex Audio" -t output
    fi
}

install_blackhole() {
    local need_2ch=false need_16ch=false

    has_blackhole "BlackHole 2ch"  || need_2ch=true
    has_blackhole "BlackHole 16ch" || need_16ch=true

    if ! $need_2ch && ! $need_16ch; then return; fi

    $need_2ch  && { info "installing BlackHole 2ch (virtual mic)...";  brew install --cask blackhole-2ch;  }
    $need_16ch && { info "installing BlackHole 16ch (app capture)..."; brew install --cask blackhole-16ch; }

    NEEDS_REBOOT=true
}

build_intendant() {
    info "building intendant (release)..."
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local repo_root="$script_dir/.."

    cd "$repo_root"
    cargo build --release

    local bin_dir="$repo_root/target/release"
    echo ""
    ok "intendant          → $bin_dir/intendant"
    ok "intendant-runtime  → $bin_dir/intendant-runtime"
}

# ── Main ────────────────────────────────────────────────────────────────────

run_check() {
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Intendant macOS Dependency Check"
    echo "════════════════════════════════════════════════════════"

    local core_ok cu_ok audio_ok rec_ok
    check_core         && core_ok=true  || core_ok=false
    check_computer_use && cu_ok=true    || cu_ok=false
    check_audio        && audio_ok=true || audio_ok=false
    check_recording    && rec_ok=true   || rec_ok=false

    check_wasm

    echo ""
    echo "────────────────────────────────────────────────────────"

    if $core_ok && $cu_ok; then
        echo "  Core + computer-use: ready"
    else
        echo "  Core + computer-use: missing dependencies"
    fi

    if $audio_ok; then
        echo "  Audio routing: ready"
    else
        echo "  Audio routing: missing dependencies"
    fi

    if $rec_ok; then
        echo "  Recording: ready"
    else
        echo "  Recording: missing dependencies"
    fi

    echo ""
}

run_install() {
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Intendant macOS Setup"
    echo "════════════════════════════════════════════════════════"

    # Phase 1: Core
    info "checking core dependencies..."
    ensure_homebrew
    ensure_rust

    # Phase 2: Homebrew packages
    info "installing Homebrew packages..."
    brew_install cliclick
    brew_install ffmpeg
    brew_install switchaudio-osx
    brew_install sox

    # Phase 3: Audio routing
    # Try Vortex first (preferred), fall back to BlackHole
    install_vortex_audio
    if ! has_audio_device "Vortex Audio"; then
        install_blackhole
    fi

    # Phase 4: Build
    echo ""
    build_intendant

    # Phase 5: Set audio defaults
    set_vortex_defaults

    # Phase 6: App bundle
    echo ""
    info "building macOS app bundle..."
    if [ -f scripts/bundle-macos.sh ]; then
        bash scripts/bundle-macos.sh
    fi

    # Phase 6: Final status
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Setup complete!"
    echo "════════════════════════════════════════════════════════"
    echo ""

    if $NEEDS_REBOOT; then
        warn "Reboot required before audio routing will work."
        echo "   Audio drivers were installed but need a reboot to load."
        echo "   You may also need to allow the system extension in"
        echo "   System Settings → Privacy & Security."
        echo ""
    fi

    echo "  IMPORTANT: Launch from the macOS GUI for audio to work:"
    echo ""
    echo "    open target/Intendant.app --args --web"
    echo ""
    echo "  macOS requires GUI session for audio input. Do NOT run"
    echo "  from SSH — use the app bundle, Finder, or Terminal.app"
    echo "  inside the VM's display."
    echo ""
    echo "  On first launch, approve the microphone permission prompt."
    echo ""
}

main() {
    parse_args "$@"
    check_macos

    case "$ACTION" in
        check)   run_check ;;
        install) run_install ;;
    esac
}

main "$@"
