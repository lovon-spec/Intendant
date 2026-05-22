<#
.SYNOPSIS
    Intendant Windows dependency installer.

.DESCRIPTION
    Idempotent setup for building and running Intendant on Windows
    (x86_64-pc-windows-msvc). The Windows counterpart to setup-linux.sh
    and setup-macos.sh.

    Installs the build toolchain (rustup, Visual Studio 2022 Build Tools
    C++ workload, NASM, git), optional WASM tooling (wasm-pack), and the
    runtime dependencies the agent shells out to (ffmpeg, Media Foundation).
    Prefers Chocolatey for installs with existence checks so re-runs are
    no-ops, and clearly calls out the one manual step it cannot automate
    (the VB-CABLE virtual audio cable).

    Run from an elevated (Administrator) PowerShell — Chocolatey installs,
    the Visual Studio Build Tools workload, and the Windows Server
    Media Foundation feature all require it.

.PARAMETER Check
    Report what is installed without changing anything.

.PARAMETER SkipWasm
    Skip the wasm-pack install. The build.rs auto-rebuilds WASM but degrades
    gracefully when wasm-pack is absent, so the dashboard still works from
    the pre-compiled artifacts checked into static/wasm-web/.

.PARAMETER NoBuild
    Install dependencies but skip the cargo build at the end.

.EXAMPLE
    .\setup-windows.ps1
    Install all dependencies and build.

.EXAMPLE
    .\setup-windows.ps1 -Check
    Check what is installed without changing anything.
#>
[CmdletBinding()]
param(
    [switch]$Check,
    [switch]$SkipWasm,
    [switch]$NoBuild
)

$ErrorActionPreference = "Stop"

# Repo root is the parent of the scripts/ directory this file lives in.
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RepoRoot  = Split-Path -Parent $ScriptDir

# Track whether a manual follow-up (VB-CABLE) is still outstanding so the
# final summary can surface it.
$script:NeedsManual = @()

# ── Helpers ──────────────────────────────────────────────────────────────

function Info($msg) { Write-Host ":: $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "!! $msg" -ForegroundColor Yellow }
function Die($msg)  { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }
function Ok($msg)   { Write-Host "   + $msg" -ForegroundColor Green }
function Miss($name, $hint) { Write-Host "   - $name -- $hint" -ForegroundColor Red }

function Test-Cmd($name) {
    return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($id)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# Pull machine + user PATH (and CARGO_HOME/RUSTUP_HOME) back into the current
# process so freshly-installed tools resolve without restarting the shell.
function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $user    = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = (@($machine, $user) | Where-Object { $_ }) -join ";"

    foreach ($var in "CARGO_HOME", "RUSTUP_HOME") {
        $val = [Environment]::GetEnvironmentVariable($var, "Machine")
        if (-not $val) { $val = [Environment]::GetEnvironmentVariable($var, "User") }
        if ($val) { Set-Item "env:$var" $val }
    }

    # rustup installs cargo into %CARGO_HOME%\bin (default %USERPROFILE%\.cargo\bin),
    # which a freshly-set machine PATH may not include yet this session.
    $cargoBin = if ($env:CARGO_HOME) { Join-Path $env:CARGO_HOME "bin" } `
                else { Join-Path $env:USERPROFILE ".cargo\bin" }
    if ((Test-Path $cargoBin) -and ($env:Path -notlike "*$cargoBin*")) {
        $env:Path = "$env:Path;$cargoBin"
    }
}

# Is a Chocolatey package installed? `choco list` is local-only by default on
# 2.x (the pre-2.0 `--local-only` flag was *removed* and now errors), so probe
# the modern form only.
function Test-ChocoPkg($name) {
    if (-not (Test-Cmd choco)) { return $false }
    $listed = & choco list --exact $name 2>$null
    return [bool]($listed | Select-String -SimpleMatch -Quiet $name)
}

# Ensure `nasm` is callable. The choco `nasm` package installs nasm.exe into a
# fixed directory (C:\Program Files\NASM) and amends the *machine* PATH -- but
# that change only lands in newly-spawned shells, so the very shell that ran the
# install (and a fresh-machine `-Check`) can have nasm.exe on disk yet not on
# PATH. If `nasm` doesn't resolve, locate the install dir, prepend it to this
# session's PATH, persist it (machine if elevated, else user), and re-probe.
# Returns $true if nasm is callable afterward.
function Resolve-Nasm {
    if (Test-Cmd nasm) { return $true }

    $candidates = @(
        (Join-Path $env:ProgramFiles "NASM"),
        (Join-Path ${env:ProgramFiles(x86)} "NASM"),
        (Join-Path $env:ProgramData "chocolatey\bin")  # choco shim dir
    ) | Where-Object { $_ } | Select-Object -Unique

    $nasmDir = $candidates | Where-Object {
        Test-Path -LiteralPath (Join-Path $_ "nasm.exe")
    } | Select-Object -First 1

    if (-not $nasmDir) { return $false }

    # Prepend to this process's PATH for an immediate re-probe.
    if (($env:Path -split ';') -notcontains $nasmDir) {
        $env:Path = "$nasmDir;$env:Path"
    }

    # Persist so future shells (and the cargo build below) resolve nasm without
    # this dance. Machine scope needs admin; fall back to user scope otherwise.
    $scope = if (Test-Admin) { "Machine" } else { "User" }
    $persisted = [Environment]::GetEnvironmentVariable("Path", $scope)
    if (($persisted -split ';') -notcontains $nasmDir) {
        $newPath = if ($persisted) { "$persisted;$nasmDir" } else { $nasmDir }
        try {
            [Environment]::SetEnvironmentVariable("Path", $newPath, $scope)
            Info "added $nasmDir to the $scope PATH (NASM was installed but not on PATH)"
        } catch {
            Warn "could not persist $nasmDir to the $scope PATH: $($_.Exception.Message)"
        }
    }

    return (Test-Cmd nasm)
}

# Whether the Server-Media-Foundation feature is needed (Windows Server only)
# and whether it is installed. Client SKUs ship Media Foundation built in.
function Test-MediaFoundation {
    # `Get-WindowsFeature` exists only on Server SKUs (ServerManager module).
    if (-not (Test-Cmd Get-WindowsFeature)) {
        return [pscustomobject]@{ IsServer = $false; Installed = $true }
    }
    $feature = Get-WindowsFeature -Name Server-Media-Foundation -ErrorAction SilentlyContinue
    return [pscustomobject]@{
        IsServer  = $true
        Installed = [bool]($feature -and $feature.Installed)
    }
}

# ── Checks ─────────────────────────────────────────────────────────────────

function Check-Core {
    $allOk = $true
    Write-Host ""
    Write-Host "Build toolchain (required to build):"

    # Chocolatey is only the *installer* this script uses, not a build input --
    # a machine with rustup/MSVC/NASM/git installed by other means builds fine
    # without it. Report it, but do NOT count its absence as a build failure.
    if (Test-Cmd choco) {
        Ok "Chocolatey (installer)"
    } else {
        Warn "   - Chocolatey -- not installed (only needed to auto-install the deps below; https://chocolatey.org/install)"
    }

    if ((Test-Cmd rustc) -and (Test-Cmd cargo)) {
        $ver = (rustc --version) -replace '^rustc\s+(\S+).*', '$1'
        Ok "Rust toolchain ($ver)"
        $rustcHost = (rustc -vV | Select-String '^host:' ) -replace 'host:\s*', ''
        if ($rustcHost -match 'pc-windows-msvc') {
            Ok "default host: $rustcHost"
        } else {
            Miss "default host is $rustcHost" "expected x86_64-pc-windows-msvc (rustup set default-host x86_64-pc-windows-msvc)"
            $allOk = $false
        }
    } else {
        Miss "Rust toolchain" "https://rustup.rs"
        $allOk = $false
    }

    # cl.exe (MSVC compiler) ships with the VS 2022 Build Tools C++ workload.
    # It is normally only on PATH inside a Developer prompt, so also probe for
    # the workload via the Visual Studio locator (vswhere).
    if (Test-Cmd cl) {
        Ok "MSVC C++ compiler (cl.exe on PATH)"
    } elseif (Test-VsCppWorkload) {
        Ok "Visual Studio 2022 C++ build tools (VC++ workload installed)"
    } else {
        Miss "Visual Studio 2022 C++ build tools" "choco install visualstudio2022-workload-vctools"
        $allOk = $false
    }

    # NASM is required to build the `ring` crypto crate on windows-msvc. It is
    # commonly installed (C:\Program Files\NASM) but absent from PATH; resolve
    # that case before declaring it missing.
    if (Resolve-Nasm) {
        Ok "NASM ($((nasm -v) -replace '^NASM version\s+(\S+).*', '$1'))"
    } else {
        Miss "NASM" "choco install nasm (required by the ring crate; must be on PATH)"
        $allOk = $false
    }

    if (Test-Cmd git) {
        Ok "git"
    } else {
        Miss "git" "choco install git"
        $allOk = $false
    }

    # Shell sanity. The runtime shells out via cmd/PowerShell (never bash on
    # Windows), both of which ship with the OS -- so this is effectively always
    # OK, but verify rather than assume.
    if ((Test-Cmd powershell) -or (Test-Cmd cmd)) {
        Ok "Windows shell (powershell/cmd)"
    } else {
        Miss "Windows shell" "neither powershell nor cmd resolved -- PATH is badly broken"
        $allOk = $false
    }

    return $allOk
}

# Locate a VS 2022 install carrying the VC++ tools component via vswhere,
# which Visual Studio Installer drops at a fixed path. Lets `-Check` and the
# installer recognize an already-present workload even when cl.exe isn't on
# the current PATH (it normally only is inside a Developer prompt).
function Test-VsCppWorkload {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $false }
    $found = & $vswhere -products * -latest `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null
    return [bool]$found
}

function Check-Wasm {
    Write-Host ""
    Write-Host "WASM build tools (optional):"

    if (Test-Cmd wasm-pack) {
        Ok "wasm-pack ($((wasm-pack --version) -replace '^wasm-pack\s+(\S+).*', '$1'))"
        return $true
    }
    Miss "wasm-pack" "cargo install wasm-pack (optional -- build.rs degrades gracefully without it)"
    return $false
}

function Check-Runtime {
    $allOk = $true
    Write-Host ""
    Write-Host "Runtime dependencies (optional -- not required to build):"

    # ffmpeg/ffplay back the audio bridge (no PulseAudio/CoreAudio on Windows).
    if (Test-Cmd ffmpeg) {
        Ok "ffmpeg (on PATH)"
    } else {
        Miss "ffmpeg" "choco install ffmpeg (audio bridge only; not needed to build)"
        $allOk = $false
    }
    if (Test-Cmd ffplay) {
        Ok "ffplay (on PATH)"
    } else {
        Miss "ffplay" "bundled with the ffmpeg package; ensure the full ffmpeg build is on PATH"
        $allOk = $false
    }

    # Media Foundation: built into client SKUs; Server needs the feature.
    $mf = Test-MediaFoundation
    if (-not $mf.IsServer) {
        Ok "Media Foundation (built into Windows client SKU)"
    } elseif ($mf.Installed) {
        Ok "Media Foundation (Server-Media-Foundation feature installed)"
    } else {
        Miss "Media Foundation" "Install-WindowsFeature Server-Media-Foundation (Windows Server only; software H264 encoder)"
        $allOk = $false
    }

    # VB-CABLE: no package/CLI installer; surface its presence either way. A
    # missing cable is a manual follow-up for voice, NOT a build/toolchain
    # failure -- it must never gate the -Check exit code.
    if (Test-VbCable) {
        Ok "VB-CABLE virtual audio device"
    } else {
        Miss "VB-CABLE" "manual install for voice only -- https://vb-audio.com/Cable/ (not needed to build)"
        $allOk = $false
    }

    return $allOk
}

# Detect the VB-CABLE virtual audio device by its driver/device name. The
# Windows analogue of macOS BlackHole / Linux PulseAudio null sinks: a
# loopback the WASAPI audio bridge plays into and captures from.
function Test-VbCable {
    try {
        $dev = Get-CimInstance -ClassName Win32_SoundDevice -ErrorAction SilentlyContinue |
               Where-Object { $_.Name -match 'VB-Audio|CABLE' }
        return [bool]$dev
    } catch {
        return $false
    }
}

# ── Install ──────────────────────────────────────────────────────────────

function Install-Chocolatey {
    if (Test-Cmd choco) { return }
    Info "installing Chocolatey..."
    Set-ExecutionPolicy Bypass -Scope Process -Force
    [System.Net.ServicePointManager]::SecurityProtocol = `
        [System.Net.ServicePointManager]::SecurityProtocol -bor 3072  # TLS 1.2
    Invoke-Expression ((New-Object System.Net.WebClient).DownloadString(
        'https://community.chocolatey.org/install.ps1'))
    Update-SessionPath
    if (-not (Test-Cmd choco)) {
        Die "Chocolatey install did not put choco on PATH -- open a new shell and re-run"
    }
}

# Install a Chocolatey package if neither the package nor a probe command is
# already present. Idempotent: re-runs are no-ops.
function Install-ChocoPkg {
    param(
        [string]$Package,
        [string]$ProbeCmd = ""
    )
    if ($ProbeCmd -and (Test-Cmd $ProbeCmd)) {
        Info "$Package already present ($ProbeCmd found), skipping"
        return
    }
    # If the choco package is recorded as installed but the probe command still
    # doesn't resolve, the package landed but its dir isn't on this session's
    # PATH yet. Refresh the session PATH and re-probe before trusting it --
    # otherwise we'd skip an install whose tool is unusable (the NASM-off-PATH
    # trap). If it still won't resolve, fall through and (re)install.
    if (Test-ChocoPkg $Package) {
        if (-not $ProbeCmd) {
            Info "$Package already installed (choco), skipping"
            return
        }
        Update-SessionPath
        if (Test-Cmd $ProbeCmd) {
            Info "$Package already installed (choco), skipping"
            return
        }
        Warn "$Package recorded as installed but '$ProbeCmd' is not on PATH -- reinstalling to repair"
    }
    Info "installing $Package..."
    & choco install -y $Package
    if ($LASTEXITCODE -ne 0) { Die "choco install $Package failed (exit $LASTEXITCODE)" }
    Update-SessionPath
}

function Install-Rust {
    if ((Test-Cmd rustc) -and (Test-Cmd cargo)) {
        Info "Rust toolchain already installed"
    } else {
        Info "installing Rust toolchain via rustup..."
        $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
        [System.Net.ServicePointManager]::SecurityProtocol = `
            [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
        Invoke-WebRequest "https://win.rustup.rs/x86_64" -OutFile $rustupInit
        # --default-host pins the MSVC ABI target (vs the gnu ABI), matching
        # the windows-sys / windows / arboard crates the build links against.
        & $rustupInit -y --default-host x86_64-pc-windows-msvc --profile default
        if ($LASTEXITCODE -ne 0) { Die "rustup-init failed (exit $LASTEXITCODE)" }
        Update-SessionPath
    }

    if (-not (Test-Cmd cargo)) {
        Die "cargo not found after rustup install -- open a new shell and re-run, or check PATH"
    }

    # Ensure the default host is the MSVC ABI even if rustup was pre-installed
    # with the gnu host. Idempotent.
    $rustcHost = (rustc -vV | Select-String '^host:') -replace 'host:\s*', ''
    if ($rustcHost -notmatch 'pc-windows-msvc') {
        Info "setting rustup default host to x86_64-pc-windows-msvc (was $rustcHost)..."
        & rustup set default-host x86_64-pc-windows-msvc
    }
}

function Install-WasmPack {
    if ($SkipWasm) {
        Info "skipping wasm-pack (-SkipWasm)"
        return
    }
    if (Test-Cmd wasm-pack) {
        Info "wasm-pack already installed"
        return
    }
    Info "installing wasm-pack (cargo install -- this may take a few minutes)..."
    & cargo install wasm-pack
    if ($LASTEXITCODE -ne 0) {
        Warn "cargo install wasm-pack failed -- WASM rebuilds will be skipped."
        Warn "The dashboard still works from the pre-compiled static/wasm-web/ artifacts."
        return
    }
    Update-SessionPath
}

# Enable Media Foundation on Windows Server (client SKUs have it built in).
function Install-MediaFoundation {
    $mf = Test-MediaFoundation
    if (-not $mf.IsServer) {
        Info "Media Foundation built into this Windows client SKU -- nothing to install"
        return
    }
    if ($mf.Installed) {
        Info "Server-Media-Foundation feature already installed"
        return
    }
    Info "installing Server-Media-Foundation feature (Windows Server)..."
    Install-WindowsFeature -Name Server-Media-Foundation | Out-Null
    Ok "Server-Media-Foundation installed"
}

# VB-CABLE has no silent/Chocolatey installer (the vendor ships a manual
# zip + signed driver). Surface clear instructions; this is the macOS
# BlackHole / Linux PulseAudio-module equivalent the script can't automate.
function Show-VbCableInstructions {
    if (Test-VbCable) {
        Ok "VB-CABLE already installed"
        return
    }
    Warn "VB-CABLE virtual audio cable is NOT installed (manual step)."
    Write-Host ""
    Write-Host "  The voice audio bridge plays TTS into a virtual output and captures" -ForegroundColor White
    Write-Host "  mic audio from a virtual input -- the Windows analogue of BlackHole" -ForegroundColor White
    Write-Host "  on macOS / PulseAudio null sinks on Linux. Install VB-CABLE manually:" -ForegroundColor White
    Write-Host ""
    Write-Host "    1. Download:  https://vb-audio.com/Cable/" -ForegroundColor White
    Write-Host "    2. Unzip and run VBCABLE_Setup_x64.exe as Administrator." -ForegroundColor White
    Write-Host "    3. Reboot when prompted (the driver loads on next boot)." -ForegroundColor White
    Write-Host "    4. Set 'CABLE Input (VB-Audio Virtual Cable)' as the DEFAULT" -ForegroundColor White
    Write-Host "       PLAYBACK device (Sound settings) so the bridge can route audio." -ForegroundColor White
    Write-Host ""
    $script:NeedsManual += "Install VB-CABLE and set it as the default playback device (https://vb-audio.com/Cable/)."
}

function Build-Intendant {
    Info "building intendant (release)..."
    Push-Location $RepoRoot
    try {
        & cargo build --release --target x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { Die "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }

    $binDir = Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\release"
    Write-Host ""
    Ok "intendant          -> $binDir\intendant.exe"
    Ok "intendant-runtime  -> $binDir\intendant-runtime.exe"
}

# ── Modes ──────────────────────────────────────────────────────────────────

function Run-Check {
    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Intendant Windows Dependency Check" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan

    # Only the build toolchain gates the exit code. WASM tooling and the runtime
    # deps (ffmpeg, Media Foundation, VB-CABLE) are optional/runtime/manual --
    # a missing VB-CABLE is not a toolchain failure and must not fail -Check.
    # Coerce to a single boolean: a function returns its whole output stream, so
    # guard the exit-code decision against any stray emitted value by taking the
    # last element (the `return $allOk`) and casting.
    $coreOk    = [bool](@(Check-Core)[-1])
    $null      = Check-Wasm
    $null      = Check-Runtime

    Write-Host ""
    Write-Host "--------------------------------------------------------"

    if ($coreOk) {
        Write-Host "  Build toolchain (required):   ready" -ForegroundColor Green
    } else {
        Write-Host "  Build toolchain (required):   MISSING dependencies (run without -Check to install)" -ForegroundColor Red
    }
    Write-Host "  Optional/runtime deps:        see notes above (wasm-pack, ffmpeg, VB-CABLE -- not required to build)"
    Write-Host ""

    # Return a nonzero exit code iff a *required build* dependency is missing or
    # unusable, so CI/automation can't read a false pass. Optional and runtime
    # deps never affect the exit code.
    if (-not $coreOk) {
        Write-Host "Required build dependencies are missing. Re-run without -Check to install them." -ForegroundColor Red
        exit 1
    }
    exit 0
}

function Run-Install {
    if (-not (Test-Admin)) {
        Die "this script must run as Administrator (Chocolatey, the VS Build Tools workload, and Server-Media-Foundation all require it). Right-click PowerShell -> Run as administrator."
    }

    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Intendant Windows Setup" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan

    # Phase 1: Chocolatey
    Info "checking Chocolatey..."
    Install-Chocolatey
    Update-SessionPath

    # Phase 2: build toolchain via Chocolatey
    Info "installing build toolchain packages..."
    Install-ChocoPkg -Package "git" -ProbeCmd "git"
    # NASM is required to assemble the `ring` crate on windows-msvc. The choco
    # package amends the *machine* PATH, which this shell won't see until
    # Resolve-Nasm prepends the install dir (and persists it) so the cargo build
    # below can find nasm.exe.
    Install-ChocoPkg -Package "nasm" -ProbeCmd "nasm"
    if (-not (Resolve-Nasm)) {
        Die "NASM not callable after install -- nasm.exe was not found in C:\Program Files\NASM or the choco shim dir"
    }
    # The C++ workload provides cl.exe / link.exe / the Windows SDK -- required
    # even for `cargo check`. No reliable bare-command probe (cl.exe lives off
    # PATH outside a Developer prompt), so gate on the package + vswhere.
    if (Test-VsCppWorkload) {
        Info "Visual Studio 2022 C++ build tools already present, skipping"
    } else {
        Install-ChocoPkg -Package "visualstudio2022-workload-vctools"
    }

    # Phase 3: Rust (rustup, MSVC host)
    Write-Host ""
    Install-Rust

    # Phase 4: wasm-pack (optional)
    Write-Host ""
    Install-WasmPack

    # Phase 5: runtime deps
    Write-Host ""
    Info "installing runtime dependencies..."
    Install-ChocoPkg -Package "ffmpeg" -ProbeCmd "ffmpeg"
    Install-MediaFoundation
    Show-VbCableInstructions

    # Phase 6: build
    if ($NoBuild) {
        Write-Host ""
        Info "skipping build (-NoBuild)"
    } else {
        Write-Host ""
        Build-Intendant
    }

    # Phase 7: summary
    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Green
    Write-Host "  Setup complete!" -ForegroundColor Green
    Write-Host "========================================================" -ForegroundColor Green
    Write-Host ""

    if ($script:NeedsManual.Count -gt 0) {
        Warn "Manual steps still required:"
        foreach ($step in $script:NeedsManual) {
            Write-Host "   - $step" -ForegroundColor Yellow
        }
        Write-Host ""
    }

    Write-Host "  Add an API key, then run intendant:" -ForegroundColor White
    Write-Host ""
    Write-Host "    cd `"$RepoRoot`""
    Write-Host "    notepad .env        # add OPENAI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY"
    Write-Host "    .\target\x86_64-pc-windows-msvc\release\intendant.exe `"your task here`""
    Write-Host ""
    Write-Host "  Other modes:" -ForegroundColor White
    Write-Host "    intendant.exe --web              # Web dashboard"
    Write-Host "    intendant.exe --direct `"task`"     # Single-agent"
    Write-Host "    intendant.exe --no-tui `"task`"     # Headless"
    Write-Host ""
    Write-Host "  NOTE: live display capture, input injection, and the voice audio"
    Write-Host "  bridge require an interactive desktop session (not a headless"
    Write-Host "  service / Session 0). See docs: Windows Support."
    Write-Host ""
}

# ── Main ─────────────────────────────────────────────────────────────────

if (-not ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)) {
    Die "this script is for Windows"
}

if ($Check) {
    Run-Check
} else {
    Run-Install
}
