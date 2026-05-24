<#
.SYNOPSIS
    Intendant Windows dependency installer.

.DESCRIPTION
    Idempotent setup for building and running Intendant on Windows
    (x86_64-pc-windows-msvc). The Windows counterpart to setup-linux.sh
    and setup-macos.sh.

    Installs or verifies the build toolchain (rustup, Visual Studio 2022
    Build Tools C++ workload, NASM, git), optional WASM tooling (wasm-pack),
    and the runtime dependencies the agent shells out to (ffmpeg, Media
    Foundation). Package-managed installs use the selected PackageManager
    policy so re-runs are no-ops and the script does not silently bootstrap a
    package manager unless Chocolatey is explicitly selected. VB-CABLE remains
    a manual, optional voice step.

    Run from an elevated (Administrator) PowerShell -- Visual Studio Build
    Tools, Chocolatey installs, machine PATH repair, and the Windows Server
    Media Foundation feature may require it.

.PARAMETER Check
    Report what is installed without changing anything.

.PARAMETER SkipWasm
    Skip the wasm-pack install. The build.rs auto-rebuilds WASM but degrades
    gracefully when wasm-pack is absent, so the dashboard still works from
    the pre-compiled artifacts checked into static/wasm-web/.

.PARAMETER NoBuild
    Install dependencies but skip the cargo build at the end.

.PARAMETER PackageManager
    Package install policy: Auto, Winget, Chocolatey, or None. Auto uses an
    already-installed winget first, then Chocolatey. It does not install
    Chocolatey by surprise. Use -PackageManager Chocolatey to preserve the old
    one-command bootstrap behavior when Chocolatey is missing.

.PARAMETER SkipSmoke
    Skip the post-build runtime hello smoke check.

.EXAMPLE
    .\setup-windows.ps1
    Install dependencies with an existing winget/choco and build.

.EXAMPLE
    .\setup-windows.ps1 -PackageManager Chocolatey
    Install Chocolatey if needed, install dependencies, and build.

.EXAMPLE
    .\setup-windows.ps1 -Check
    Check what is installed without changing anything.
#>
[CmdletBinding()]
param(
    [switch]$Check,
    [switch]$SkipWasm,
    [switch]$NoBuild,
    [ValidateSet("Auto", "Winget", "Chocolatey", "None")]
    [string]$PackageManager = "Auto",
    [switch]$SkipSmoke
)

$ErrorActionPreference = "Stop"

# Repo root is the parent of the scripts/ directory this file lives in.
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RepoRoot  = Split-Path -Parent $ScriptDir

# Track whether a manual follow-up (VB-CABLE) is still outstanding so the
# final summary can surface it.
$script:NeedsManual = @()
$script:PackageProvider = $null

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

function Show-PackageManagerStatus {
    Write-Host ""
    Write-Host "Package installer policy:"
    Write-Host "   Selected policy: $PackageManager"
    if (Test-Cmd winget) {
        Ok "winget available"
    } else {
        Warn "   - winget -- not found"
    }
    if (Test-Cmd choco) {
        Ok "Chocolatey available"
    } else {
        Warn "   - Chocolatey -- not found"
    }
    if ($PackageManager -eq "Auto") {
        Write-Host "   Auto uses an existing winget first, then Chocolatey. It does not install Chocolatey automatically."
    } elseif ($PackageManager -eq "Chocolatey") {
        Write-Host "   Chocolatey mode will install Chocolatey if it is missing."
    } elseif ($PackageManager -eq "None") {
        Write-Host "   None mode never installs package-managed dependencies; missing build deps must already be present or be installed manually."
    }
}

function Resolve-PackageProvider {
    switch ($PackageManager) {
        "Winget" {
            if (-not (Test-Cmd winget)) {
                Die "PackageManager=Winget selected, but winget is not on PATH. Install App Installer/winget or rerun with -PackageManager Chocolatey/None."
            }
            return "Winget"
        }
        "Chocolatey" {
            Install-Chocolatey
            return "Chocolatey"
        }
        "None" {
            return "None"
        }
        default {
            if (Test-Cmd winget) { return "Winget" }
            if (Test-Cmd choco) { return "Chocolatey" }
            Warn "No supported package manager found. Continuing without package-managed installs."
            Warn "Install winget/choco, install missing dependencies manually, or rerun with -PackageManager Chocolatey to bootstrap Chocolatey."
            return "None"
        }
    }
}

# Ensure `nasm` is callable. The choco `nasm` package installs nasm.exe into a
# fixed directory (C:\Program Files\NASM) and amends the *machine* PATH -- but
# that change only lands in newly-spawned shells, so the very shell that ran the
# install (and a fresh-machine `-Check`) can have nasm.exe on disk yet not on
# PATH. If `nasm` doesn't resolve, locate the install dir and prepend it to this
# session's PATH. Install mode can also persist that repair (machine if elevated,
# else user); check mode keeps the fix process-local so `-Check` stays read-only.
# Returns $true if nasm is callable afterward.
function Resolve-Nasm {
    param([switch]$Persist)

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

    if (-not $Persist) {
        return (Test-Cmd nasm)
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
        Miss "Visual Studio 2022 C++ build tools" "install VS 2022 Build Tools with the C++ workload"
        $allOk = $false
    }

    # NASM is required to build the `ring` crypto crate on windows-msvc. It is
    # commonly installed (C:\Program Files\NASM) but absent from PATH; resolve
    # that case before declaring it missing.
    if (Resolve-Nasm) {
        Ok "NASM ($((nasm -v) -replace '^NASM version\s+(\S+).*', '$1'))"
    } else {
        Miss "NASM" "install NASM and ensure nasm.exe is on PATH (required by the ring crate)"
        $allOk = $false
    }

    if (Test-Cmd git) {
        Ok "git"
    } else {
        Miss "git" "install Git for Windows"
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
        Miss "ffmpeg" "install Gyan.FFmpeg/ffmpeg (audio bridge only; not needed to build)"
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

function Install-WingetPkg {
    param(
        [string]$Name,
        [string]$Id,
        [string]$ProbeCmd = "",
        [string]$Override = ""
    )
    if ($ProbeCmd -and (Test-Cmd $ProbeCmd)) {
        Info "$Name already present ($ProbeCmd found), skipping"
        return
    }
    Info "installing $Name via winget ($Id)..."
    $args = @(
        "install",
        "--id", $Id,
        "--exact",
        "--source", "winget",
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--silent"
    )
    if ($Override) {
        $args += @("--override", $Override)
    }
    & winget @args
    if ($LASTEXITCODE -ne 0) { Die "winget install $Name ($Id) failed (exit $LASTEXITCODE)" }
    Update-SessionPath
}

function Install-ManagedPackage {
    param(
        [string]$Name,
        [string]$ProbeCmd,
        [string]$WingetId,
        [string]$ChocoPackage,
        [string]$ManualHint,
        [string]$WingetOverride = ""
    )

    if ($ProbeCmd -and (Test-Cmd $ProbeCmd)) {
        Info "$Name already present ($ProbeCmd found), skipping"
        return
    }

    switch ($script:PackageProvider) {
        "Winget" {
            Install-WingetPkg -Name $Name -Id $WingetId -ProbeCmd $ProbeCmd -Override $WingetOverride
        }
        "Chocolatey" {
            Install-ChocoPkg -Package $ChocoPackage -ProbeCmd $ProbeCmd
        }
        "None" {
            Die "$Name is missing and PackageManager=$PackageManager will not install package-managed dependencies. $ManualHint"
        }
        default {
            Die "internal error: package provider has not been resolved"
        }
    }
}

function Install-VsCppWorkload {
    if (Test-VsCppWorkload) {
        Info "Visual Studio 2022 C++ build tools already present, skipping"
        return
    }

    switch ($script:PackageProvider) {
        "Winget" {
            Install-WingetPkg `
                -Name "Visual Studio 2022 Build Tools C++ workload" `
                -Id "Microsoft.VisualStudio.2022.BuildTools" `
                -Override "--wait --quiet --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
        }
        "Chocolatey" {
            Install-ChocoPkg -Package "visualstudio2022-workload-vctools"
        }
        "None" {
            Die "Visual Studio 2022 C++ build tools are missing and PackageManager=$PackageManager will not install them. Install Visual Studio 2022 Build Tools with the C++ workload, then rerun this script."
        }
        default {
            Die "internal error: package provider has not been resolved"
        }
    }

    if (-not (Test-VsCppWorkload)) {
        Die "Visual Studio 2022 C++ workload was not detected after install. Open a new Administrator PowerShell or inspect the Visual Studio Installer."
    }
}

function Install-FfmpegToolchain {
    if ((Test-Cmd ffmpeg) -and (Test-Cmd ffplay)) {
        Info "ffmpeg toolchain already present (ffmpeg/ffplay found), skipping"
        return
    }

    $manualFfmpeg = "Install a full ffmpeg distribution that includes ffmpeg.exe and ffplay.exe for the optional Windows voice audio bridge."

    switch ($script:PackageProvider) {
        "Winget" {
            Info "installing optional ffmpeg toolchain via winget (Gyan.FFmpeg)..."
            & winget install --id "Gyan.FFmpeg" --exact --source "winget" --accept-package-agreements --accept-source-agreements --silent
            if ($LASTEXITCODE -ne 0) {
                Warn "winget install Gyan.FFmpeg failed (exit $LASTEXITCODE); continuing because voice audio is optional."
                $script:NeedsManual += $manualFfmpeg
                return
            }
            Update-SessionPath
        }
        "Chocolatey" {
            Info "installing optional ffmpeg toolchain via Chocolatey..."
            & choco install -y ffmpeg
            if ($LASTEXITCODE -ne 0) {
                Warn "choco install ffmpeg failed (exit $LASTEXITCODE); continuing because voice audio is optional."
                $script:NeedsManual += $manualFfmpeg
                return
            }
            Update-SessionPath
        }
        "None" {
            Warn "ffmpeg/ffplay are missing and PackageManager=$PackageManager will not install them. Voice audio will be unavailable until they are installed manually."
            $script:NeedsManual += $manualFfmpeg
            return
        }
        default {
            Die "internal error: package provider has not been resolved"
        }
    }

    if (-not ((Test-Cmd ffmpeg) -and (Test-Cmd ffplay))) {
        Warn "ffmpeg package install completed, but ffmpeg.exe and ffplay.exe were not both detected on PATH. Voice audio will remain unavailable until both are present."
        $script:NeedsManual += $manualFfmpeg
    }
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
    Test-PostBuildSmoke -BinDir $binDir
}

function Test-PostBuildSmoke {
    param([string]$BinDir)

    $intendant = Join-Path $BinDir "intendant.exe"
    $runtime = Join-Path $BinDir "intendant-runtime.exe"

    Write-Host ""
    Write-Host "Post-build smoke check:"
    if (-not (Test-Path -LiteralPath $intendant)) {
        Die "expected binary missing: $intendant"
    }
    if (-not (Test-Path -LiteralPath $runtime)) {
        Die "expected binary missing: $runtime"
    }
    Ok "release binaries exist"

    if ($SkipSmoke) {
        Info "skipping runtime smoke check (-SkipSmoke)"
        return
    }

    Info "running intendant-runtime hello check..."
    $marker = "intendant-runtime-smoke"
    $payload = '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo intendant-runtime-smoke"}]}'
    $output = $payload | & $runtime 2>&1
    $exit = $LASTEXITCODE
    $text = ($output | Out-String)
    if ($exit -ne 0) {
        Die "runtime smoke check failed (exit $exit): $text"
    }
    if ($text -match $marker) {
        Ok "runtime hello protocol succeeded"
    } else {
        Warn "runtime smoke check exited successfully but did not echo the expected marker"
    }
}

function Show-CompletionSummary {
    param([bool]$Built)

    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Green
    Write-Host "  Setup complete" -ForegroundColor Green
    Write-Host "========================================================" -ForegroundColor Green
    Write-Host ""

    if ($script:NeedsManual.Count -gt 0) {
        Warn "Manual steps still required:"
        foreach ($step in $script:NeedsManual) {
            Write-Host "   - $step" -ForegroundColor Yellow
        }
        Write-Host ""
    }

    $binDir = Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\release"
    Write-Host "  Status:" -ForegroundColor White
    if ($Built) {
        Ok "Build ready: $binDir\intendant.exe"
    } else {
        Warn "   - Build skipped (-NoBuild); run cargo build --release --target x86_64-pc-windows-msvc when ready."
    }
    Ok "Web/display path: run from an interactive desktop session, then open the dashboard."

    if ((Test-Cmd ffmpeg) -and (Test-Cmd ffplay) -and (Test-VbCable)) {
        Ok "Voice prerequisites detected: ffmpeg/ffplay and VB-CABLE are present (Windows voice bridge still needs end-to-end validation)."
    } else {
        Warn "   - Voice optional: needs ffmpeg/ffplay plus manual VB-CABLE setup; Windows voice bridge is still pending end-to-end validation."
    }
    Warn "   - LAN setup: native 'intendant lan' is unsupported on Windows; use scripts\setup-lan.bat with WSL/a Linux guest or your own reverse proxy."

    Write-Host ""
    Write-Host "  Add an API key, then run intendant:" -ForegroundColor White
    Write-Host ""
    Write-Host "    cd `"$RepoRoot`""
    Write-Host "    notepad .env        # add OPENAI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY"
    Write-Host "    .\target\x86_64-pc-windows-msvc\release\intendant.exe `"your task here`""
    Write-Host ""
    Write-Host "  Other modes:" -ForegroundColor White
    Write-Host "    .\target\x86_64-pc-windows-msvc\release\intendant.exe --web"
    Write-Host "    .\target\x86_64-pc-windows-msvc\release\intendant.exe --direct `"task`""
    Write-Host "    .\target\x86_64-pc-windows-msvc\release\intendant.exe --no-tui `"task`""
    Write-Host ""
}

# ── Modes ──────────────────────────────────────────────────────────────────

function Run-Check {
    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Intendant Windows Dependency Check" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan

    Show-PackageManagerStatus

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
        Write-Host "  Build toolchain (required):   MISSING dependencies (run without -Check to install, or install manually)" -ForegroundColor Red
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
        Die "this script must run as Administrator (Visual Studio Build Tools, Chocolatey installs, machine PATH repair, and Server-Media-Foundation may require it). Right-click PowerShell -> Run as administrator."
    }

    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Intendant Windows Setup" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan

    # Phase 1: Package install policy
    Show-PackageManagerStatus
    $script:PackageProvider = Resolve-PackageProvider
    Info "package-managed installs: $($script:PackageProvider)"
    Update-SessionPath

    # Phase 2: build toolchain via the selected package policy
    Info "installing build toolchain packages..."
    Install-ManagedPackage `
        -Name "git" `
        -ProbeCmd "git" `
        -WingetId "Git.Git" `
        -ChocoPackage "git" `
        -ManualHint "Install Git for Windows and ensure git.exe is on PATH."
    # NASM is required to assemble the `ring` crate on windows-msvc. The choco
    # package amends the *machine* PATH; the winget package can behave the same.
    # Resolve-Nasm repairs the current session before the cargo build below.
    if (Resolve-Nasm -Persist) {
        Info "NASM already present"
    } else {
        Install-ManagedPackage `
            -Name "NASM" `
            -ProbeCmd "nasm" `
            -WingetId "NASM.NASM" `
            -ChocoPackage "nasm" `
            -ManualHint "Install NASM, then add the directory containing nasm.exe to PATH."
        if (-not (Resolve-Nasm -Persist)) {
            Die "NASM not callable after install -- nasm.exe was not found in C:\Program Files\NASM or the package-manager shim dir"
        }
    }
    # The C++ workload provides cl.exe / link.exe / the Windows SDK -- required
    # even for `cargo check`. No reliable bare-command probe (cl.exe lives off
    # PATH outside a Developer prompt), so gate on the package + vswhere.
    Install-VsCppWorkload

    # Phase 3: Rust (rustup, MSVC host)
    Write-Host ""
    Install-Rust

    # Phase 4: wasm-pack (optional)
    Write-Host ""
    Install-WasmPack

    # Phase 5: runtime deps
    Write-Host ""
    Info "installing runtime dependencies..."
    Install-FfmpegToolchain
    Install-MediaFoundation
    Show-VbCableInstructions

    # Phase 6: build
    $built = $false
    if ($NoBuild) {
        Write-Host ""
        Info "skipping build (-NoBuild)"
    } else {
        Write-Host ""
        Build-Intendant
        $built = $true
    }

    # Phase 7: summary
    Show-CompletionSummary -Built $built
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
