#!/usr/bin/env bash
#
# Intendant macOS dependency installer.
#
# Usage:
#   ./setup-macos.sh             # Install all dependencies and build
#   ./setup-macos.sh --check     # Check what's installed without changing anything
#   ./setup-macos.sh --all       # Also install audio routing deps (BlackHole, sox, etc.)
#
set -euo pipefail

die()  { echo "error: $*" >&2; exit 1; }
info() { echo ":: $*"; }
warn() { echo "!! $*" >&2; }
ok()   { echo "   ✓ $1"; }
miss() { echo "   ✗ $1 — $2"; }

ACTION="install"
INCLUDE_AUDIO_ROUTING=false

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --check) ACTION="check"; shift ;;
            --all)   INCLUDE_AUDIO_ROUTING=true; shift ;;
            -h|--help) sed -n '3,9p' "$0" | sed 's/^# \?//'; exit 0 ;;
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
has_brew_cask() { brew list --cask "$1" &>/dev/null 2>&1; }

# Check if a BlackHole device exists in the audio system
has_blackhole() {
    local name="$1"
    system_profiler SPAudioDataType 2>/dev/null | grep -q "$name"
}

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

# Audio routing deps: only needed for spawn_live_audio (voice calls through
# third-party apps like WhatsApp). Browser-based voice works without these.
check_audio() {
    local all_ok=true

    echo ""
    echo "Audio routing dependencies (optional — for voice calls through apps):"

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

    if has_blackhole "BlackHole 2ch"; then
        ok "BlackHole 2ch"
    else
        miss "BlackHole 2ch" "see below"
        all_ok=false
    fi

    if has_blackhole "BlackHole 16ch"; then
        ok "BlackHole 16ch"
    else
        miss "BlackHole 16ch" "see below"
        all_ok=false
    fi

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

# WASM build deps (optional)
check_wasm() {
    echo ""
    echo "WASM build dependencies (optional):"

    if has_cmd wasm-pack; then
        ok "wasm-pack"
    else
        miss "wasm-pack" "cargo install wasm-pack"
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

install_blackhole() {
    # BlackHole requires a kernel extension — can't be silently installed.
    # Check if already present; if not, give clear instructions.
    local need_2ch=false need_16ch=false

    has_blackhole "BlackHole 2ch"  || need_2ch=true
    has_blackhole "BlackHole 16ch" || need_16ch=true

    if ! $need_2ch && ! $need_16ch; then return; fi

    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  BlackHole Virtual Audio Driver"
    echo "════════════════════════════════════════════════════════"
    echo ""
    echo "  BlackHole requires a system extension and cannot be"
    echo "  installed fully automatically. You need both:"
    echo ""
    $need_2ch  && echo "    - BlackHole 2ch  (virtual mic for model output)"
    $need_16ch && echo "    - BlackHole 16ch (virtual speaker for app capture)"
    echo ""
    echo "  Install from: https://github.com/ExistentialAudio/BlackHole"
    echo ""
    echo "  After installing, you may need to:"
    echo "    1. Allow the system extension in System Settings → Privacy & Security"
    echo "    2. Restart your Mac"
    echo ""

    # Try Homebrew cask as a convenience — this may or may not work
    # depending on BlackHole's current Homebrew status
    local try_brew
    read -rp "  Attempt install via Homebrew? (y/n) [y]: " try_brew
    try_brew="${try_brew:-y}"

    if [[ "$try_brew" == "y" ]]; then
        $need_2ch  && (brew install --cask blackhole-2ch  2>/dev/null && ok "BlackHole 2ch installed" || warn "Homebrew cask not available — install manually")
        $need_16ch && (brew install --cask blackhole-16ch 2>/dev/null && ok "BlackHole 16ch installed" || warn "Homebrew cask not available — install manually")
    fi
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
    check_core       && core_ok=true  || core_ok=false
    check_computer_use && cu_ok=true  || cu_ok=false

    if $INCLUDE_AUDIO_ROUTING; then
        check_audio    && audio_ok=true || audio_ok=false
    fi
    check_recording && rec_ok=true  || rec_ok=false

    check_wasm

    echo ""
    echo "────────────────────────────────────────────────────────"

    if $core_ok && $cu_ok; then
        echo "  Core + computer-use: ready"
    else
        echo "  Core + computer-use: missing dependencies"
    fi

    if ${rec_ok:-false}; then
        echo "  Recording: ready"
    else
        echo "  Recording: missing dependencies"
    fi

    if $INCLUDE_AUDIO_ROUTING; then
        if ${audio_ok:-false}; then
            echo "  Audio routing: ready"
        else
            echo "  Audio routing: missing dependencies"
        fi
    else
        echo "  Audio routing: skipped (use --all to include)"
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

    # Phase 3: Audio routing (optional — only for voice calls through apps)
    if $INCLUDE_AUDIO_ROUTING; then
        brew_install switchaudio-osx
        brew_install sox
        install_blackhole
    fi

    # Phase 4: Build
    echo ""
    build_intendant

    # Phase 5: Final status
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Setup complete!"
    echo "════════════════════════════════════════════════════════"
    echo ""
    echo "  Run intendant:"
    echo "    ./target/release/intendant \"your task\""
    echo ""

    if ! $INCLUDE_AUDIO_ROUTING; then
        echo "  Audio routing (voice calls through apps) was skipped."
        echo "  Browser-based voice works without it."
        echo "  Run with --all to install audio routing deps later."
        echo ""
    fi
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
