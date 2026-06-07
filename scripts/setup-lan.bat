<# : setup-lan.bat -- Double-click to run Intendant LAN Access Setup
@echo off
setlocal
cd /d "%~dp0"
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo Requesting administrator privileges...
    powershell -Command "Start-Process cmd.exe -Verb RunAs -ArgumentList '/c cd /d \"%~dp0\" && \"%~f0\" %*'"
    exit /b
)
set "PS1=%TEMP%\setup-lan-%RANDOM%%RANDOM%.ps1"
copy /y "%~f0" "%PS1%" >nul || (echo Failed to copy script to temp & pause & exit /b 1)
set "SDIR=%~dp0"
if "%SDIR:~-1%"=="\" set "SDIR=%SDIR:~0,-1%"
powershell -ExecutionPolicy Bypass -NoProfile -File "%PS1%" -ScriptDir "%SDIR%" %*
del "%PS1%" >nul 2>&1
pause
exit /b
#>

param(
    [string]$ScriptDir = "",
    [switch]$Remove,
    [switch]$Recert,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$FwRuleName = "Intendant LAN Access"

# State -- populated by wizard or loaded from config
$script:Mode = ""
$script:GuestIp = ""
$script:VmUser = ""
$script:VmAddress = ""
$script:SshHost = ""
$script:SshPort = 22
$script:Port = 8443
$script:CertPort = 9999
$script:IsVBoxNat = $false
$script:VmName = ""
$script:ConfigPath = Join-Path $env:USERPROFILE ".intendant-lan.json"

function Info($msg)  { Write-Host ":: $msg" -ForegroundColor Cyan }
function Warn($msg)  { Write-Host "!! $msg" -ForegroundColor Yellow }
function Die($msg)   { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

function Ask($prompt, $default) {
    $suffix = if ($default) { " [$default]" } else { "" }
    $answer = Read-Host "  $prompt$suffix"
    if (-not $answer -and $default) { return $default }
    return $answer
}

function Ask-Choice($prompt, $options) {
    Write-Host ""
    Write-Host "  $prompt" -ForegroundColor White
    Write-Host ""
    for ($i = 0; $i -lt $options.Count; $i++) {
        Write-Host "    $($i + 1)) $($options[$i])"
    }
    Write-Host ""
    while ($true) {
        $choice = Read-Host "  Choose [1-$($options.Count)]"
        $idx = [int]$choice - 1
        if ($idx -ge 0 -and $idx -lt $options.Count) { return $idx }
        Write-Host "  Invalid choice, try again." -ForegroundColor Red
    }
}

# -- Config persistence --

function Save-Config {
    @{
        Mode      = $script:Mode
        VmAddress = $script:VmAddress
        VmUser    = $script:VmUser
        SshHost   = $script:SshHost
        SshPort   = $script:SshPort
        Port      = $script:Port
        IsVBoxNat = $script:IsVBoxNat
        VmName    = $script:VmName
    } | ConvertTo-Json | Set-Content $script:ConfigPath
}

function Load-Config {
    if (Test-Path $script:ConfigPath) {
        $cfg = Get-Content $script:ConfigPath | ConvertFrom-Json
        $script:Mode      = $cfg.Mode
        $script:VmAddress = $cfg.VmAddress
        $script:VmUser    = $cfg.VmUser
        $script:SshHost   = if ($cfg.SshHost) { $cfg.SshHost } else { $cfg.VmAddress }
        $script:SshPort   = if ($cfg.SshPort) { $cfg.SshPort } else { 22 }
        $script:Port      = $cfg.Port
        $script:IsVBoxNat = if ($cfg.IsVBoxNat) { $cfg.IsVBoxNat } else { $false }
        $script:VmName    = if ($cfg.VmName) { $cfg.VmName } else { "" }
        return $true
    }
    return $false
}

# -- Network helpers --

function Get-WslIp {
    $ip = wsl hostname -I 2>$null
    if (-not $ip) { Die "could not get WSL IP -- is WSL running? Start it with: wsl" }
    return ($ip.Trim() -split '\s+')[0]
}

function Get-HostLanIp {
    $ip = (Get-NetIPAddress -AddressFamily IPv4 |
           Where-Object { $_.InterfaceAlias -notmatch 'Loopback|WSL|vEthernet' -and $_.PrefixOrigin -ne 'WellKnown' } |
           Sort-Object -Property InterfaceMetric |
           Select-Object -First 1).IPAddress
    if (-not $ip) { Die "could not detect LAN IP" }
    return $ip
}

function Resolve-GuestIp {
    if ($script:Mode -eq "wsl") {
        $script:GuestIp = Get-WslIp
    } else {
        $script:GuestIp = $script:VmAddress
    }
}

# -- VirtualBox helpers --

function Find-VBoxManage {
    $candidates = @(
        "C:\Program Files\Oracle\VirtualBox\VBoxManage.exe"
    )
    $fromPath = Get-Command VBoxManage -ErrorAction SilentlyContinue
    if ($fromPath) { $candidates += $fromPath.Source }
    foreach ($p in $candidates) {
        if (Test-Path $p) { return $p }
    }
    return $null
}

# -- Port forwarding & firewall --

function Add-PortForwarding {
    if ($script:IsVBoxNat) {
        # VirtualBox NAT: configure rules directly via VBoxManage.
        # Bind to 0.0.0.0 explicitly so browsers on the LAN can reach
        # the guest *and* so the browser's Host: header contains a real
        # IP (not "localhost"). This matters for WebRTC ICE-TCP: Firefox
        # filters remote 127.0.0.1 candidates, so if the user accesses
        # via localhost the video stream can't establish a TCP path.
        # With 0.0.0.0 binding the user accesses via the host's LAN IP
        # and the dashboard advertises that IP as the ICE-TCP candidate.
        $vbm = Find-VBoxManage
        if (-not $vbm) { Die "VBoxManage not found -- is VirtualBox installed?" }
        $rules = @(
            @{ Name = "HTTPS"; HP = $script:Port;     GP = $script:Port }
            @{ Name = "Cert";  HP = $script:CertPort; GP = $script:CertPort }
        )
        foreach ($r in $rules) {
            & $vbm controlvm $script:VmName natpf1 delete $r.Name 2>$null | Out-Null
            Info "VBox NAT: 0.0.0.0:$($r.HP) -> guest:$($r.GP)"
            & $vbm controlvm $script:VmName natpf1 "$($r.Name),tcp,0.0.0.0,$($r.HP),,$($r.GP)"
        }
    } else {
        # WSL/Hyper-V/Bridged: use netsh portproxy
        $ports = @($script:Port, $script:CertPort)
        foreach ($p in $ports) {
            netsh interface portproxy delete v4tov4 listenport=$p listenaddress=0.0.0.0 2>$null | Out-Null
            Info "forwarding 0.0.0.0:$p -> $($script:GuestIp):$p"
            netsh interface portproxy add v4tov4 `
                listenport=$p listenaddress=0.0.0.0 `
                connectport=$p connectaddress=$script:GuestIp | Out-Null
        }
    }
}

function Add-FirewallRule {
    $ports = @($script:Port, $script:CertPort)
    Remove-NetFirewallRule -DisplayName $FwRuleName -ErrorAction SilentlyContinue
    Info "adding firewall rule for ports $($ports -join ', ')"
    New-NetFirewallRule `
        -DisplayName $FwRuleName `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort $ports `
        -Profile Private | Out-Null
}

function Remove-PortForwarding {
    if ($script:IsVBoxNat) {
        $vbm = Find-VBoxManage
        if ($vbm -and $script:VmName) {
            foreach ($name in @("HTTPS", "Cert")) {
                & $vbm controlvm $script:VmName natpf1 delete $name 2>$null | Out-Null
            }
            Info "VBox NAT forwarding rules removed"
        } else {
            Warn "could not find VBoxManage or VM name -- remove VBox NAT rules manually"
        }
    } else {
        foreach ($p in @($script:Port, $script:CertPort)) {
            netsh interface portproxy delete v4tov4 listenport=$p listenaddress=0.0.0.0 2>$null | Out-Null
        }
        Info "port forwarding rules removed"
    }
}

function Remove-FirewallRule {
    Remove-NetFirewallRule -DisplayName $FwRuleName -ErrorAction SilentlyContinue
    Info "firewall rule removed"
}

# -- Guest communication --

function Get-SshArgs {
    $args = @()
    if ($script:SshPort -ne 22) { $args += @("-p", $script:SshPort) }
    $args += "$($script:VmUser)@$($script:SshHost)"
    return $args
}

function Invoke-GuestCommand($cmd) {
    if ($script:Mode -eq "wsl") {
        wsl bash -c $cmd
    } else {
        $sshArgs = Get-SshArgs
        ssh @sshArgs $cmd
    }
}

# Cached absolute path to the `intendant` binary on the guest, resolved
# once by Resolve-GuestIntendantPath and reused across subsequent calls.
$script:RemoteIntendant = ""

function Resolve-GuestIntendantPath {
    if ($script:RemoteIntendant) { return $script:RemoteIntendant }

    $ErrorActionPreference = "Continue"
    $resolved = Invoke-GuestCommand "command -v intendant 2>/dev/null || true"
    $ErrorActionPreference = "Stop"
    $resolved = ($resolved -join "`n").Trim()

    if (-not $resolved) {
        Write-Host ""
        Write-Host "error: 'intendant' is not on the guest user's PATH" -ForegroundColor Red
        Write-Host ""
        Write-Host "Build and install it on the guest before re-running this script:"
        if ($script:Mode -eq "wsl") {
            Write-Host "  wsl"
            Write-Host "  cd ~/path/to/intendant && cargo build --release"
            Write-Host "  sudo ln -sf `$(pwd)/target/release/intendant /usr/local/bin/intendant"
        } else {
            Write-Host "  ssh $($script:VmUser)@$($script:SshHost)"
            Write-Host "  cd ~/path/to/intendant && cargo build --release"
            Write-Host "  sudo ln -sf `$(pwd)/target/release/intendant /usr/local/bin/intendant"
        }
        Write-Host ""
        Die "intendant not found on guest"
    }
    $script:RemoteIntendant = $resolved
    Info "remote intendant: $($script:RemoteIntendant)"
    return $script:RemoteIntendant
}

# Build a shell-quoted string of args so they survive a round-trip
# through the guest's shell. PowerShell's argument passing through
# `wsl bash -c` and `ssh host cmd` is notoriously fragile.
function Quote-GuestArg($s) {
    # Single-quote the argument, escape any embedded single quotes.
    return "'" + ($s -replace "'", "'\''") + "'"
}

# Run `intendant lan <action> [args]` on the guest. Linux guest means
# sudo is required (backend writes to /etc); we only support Linux
# guests on the Windows orchestrator.
function Invoke-IntendantLan {
    param(
        [string]$Action,
        [string[]]$LanArgs = @()
    )

    $ErrorActionPreference = "Continue"

    $intendant = Resolve-GuestIntendantPath
    $quoted = ($LanArgs | ForEach-Object { Quote-GuestArg $_ }) -join " "
    $cmd = "sudo $intendant lan $Action $quoted"

    Invoke-GuestCommand $cmd
}

function Test-GuestSsh {
    try {
        $portArgs = if ($script:SshPort -ne 22) { @("-p", $script:SshPort) } else { @() }
        $result = ssh -o ConnectTimeout=5 @portArgs "$($script:VmUser)@$($script:SshHost)" "echo ok" 2>&1
        return ($LASTEXITCODE -eq 0)
    } catch {
        return $false
    }
}

# -- Interactive wizard --

function Run-Wizard {
    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Intendant LAN Access Setup  (v2)" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan

    # Step 1: Guest type
    $guestChoice = Ask-Choice "Where is intendant running?" @(
        "WSL2 (Windows Subsystem for Linux)"
        "Hyper-V virtual machine"
        "VirtualBox virtual machine"
        "Other VM / remote Linux machine (accessible via SSH)"
    )

    if ($guestChoice -eq 0) {
        $script:Mode = "wsl"

        # Check WSL is available
        try { $null = wsl --status 2>$null } catch {
            Die "WSL does not appear to be installed. Install it with: wsl --install"
        }

        $script:GuestIp = Get-WslIp
        Info "detected WSL IP: $($script:GuestIp)"
    } else {
        $script:Mode = "vm"
        $isVirtualBoxNat = $false

        # VirtualBox NAT needs special handling
        if ($guestChoice -eq 2) {
            Write-Host ""
            Write-Host "  How to check:" -ForegroundColor Yellow
            Write-Host "    Open VirtualBox `> select your VM `> Settings `> Network."
            Write-Host "    'Attached to' shows your network mode."
            Write-Host ""
            $netChoice = Ask-Choice "What network mode is the VM using?" @(
                "NAT (default -- you haven't changed network settings)"
                "Bridged Adapter (the VM has its own IP on your network)"
                "Not sure"
            )
            if ($netChoice -eq 2) {
                Write-Host ""
                Write-Host "  If you never changed the VirtualBox network settings," -ForegroundColor Yellow
                Write-Host "  it's almost certainly NAT. Choosing NAT for you." -ForegroundColor Yellow
                $netChoice = 0
            }
            $isVirtualBoxNat = ($netChoice -eq 0)
        }

        if ($isVirtualBoxNat) {
            $script:IsVBoxNat = $true

            # Find VBoxManage
            $vbm = Find-VBoxManage
            if (-not $vbm) {
                Die "could not find VBoxManage.exe -- is VirtualBox installed in the default location?"
            }

            # Detect running VMs
            $vmLines = (& $vbm list runningvms 2>$null) -split "`n" | Where-Object { $_ -match '^"(.+)"\s+\{' }
            $vmNames = @()
            foreach ($line in $vmLines) {
                if ($line -match '^"(.+)"\s+\{') { $vmNames += $Matches[1] }
            }

            if ($vmNames.Count -eq 0) {
                Die "no running VirtualBox VMs found -- start the VM first"
            } elseif ($vmNames.Count -eq 1) {
                $script:VmName = $vmNames[0]
                Info "detected VM: $($script:VmName)"
            } else {
                $vmChoice = Ask-Choice "Which VM is running intendant?" $vmNames
                $script:VmName = $vmNames[$vmChoice]
            }

            # Ensure SSH port forwarding exists (localhost only)
            $vmInfo = & $vbm showvminfo $script:VmName --machinereadable 2>$null
            $sshRule = $vmInfo | Select-String 'Forwarding.*"SSH'
            if ($sshRule -and $sshRule -match ',(\d+),,22"') {
                $script:SshPort = [int]$Matches[1]
                Info "SSH rule found: 127.0.0.1:$($script:SshPort) -> guest:22"
            } else {
                $sshPortInput = Ask "SSH host port (for 127.0.0.1 -> guest:22)" "2222"
                $script:SshPort = [int]$sshPortInput
                Info "adding VBox NAT SSH rule..."
                & $vbm controlvm $script:VmName natpf1 "SSH,tcp,127.0.0.1,$($script:SshPort),,22"
            }

            $script:SshHost = "127.0.0.1"
            $script:VmAddress = "127.0.0.1"
            $script:GuestIp = "127.0.0.1"
        } else {
            Write-Host ""
            $script:VmAddress = Ask "Guest IP address"
            if (-not $script:VmAddress) { Die "IP address is required" }
            $script:SshHost = $script:VmAddress
            $script:SshPort = 22
            $script:GuestIp = $script:VmAddress
        }

        $script:VmUser = Ask "SSH username on the guest" $env:USERNAME

        $sshDisplay = if ($script:SshPort -ne 22) { "$($script:SshHost):$($script:SshPort)" } else { $script:SshHost }
        Info "testing SSH connection to $($script:VmUser)@$sshDisplay..."
        if (-not (Test-GuestSsh)) {
            Warn "could not connect via SSH"
            Write-Host ""
            Write-Host "  Make sure:" -ForegroundColor Yellow
            Write-Host "    - The VM is running"
            Write-Host "    - SSH server is installed: sudo apt install openssh-server"
            if ($isVirtualBoxNat) {
                Write-Host "    - You can SSH manually: ssh -p $($script:SshPort) $($script:VmUser)@$($script:SshHost)"
            } else {
                Write-Host "    - You can SSH manually: ssh $($script:VmUser)@$($script:SshHost)"
            }
            Write-Host ""
            $retry = Ask "Try again after fixing? (y/n)" "y"
            if ($retry -eq "y") {
                if (-not (Test-GuestSsh)) { Die "still cannot connect" }
            } else {
                Die "SSH connection required"
            }
        }
        Info "SSH connection OK"
    }

    # Step 2: Port
    Write-Host ""
    $portInput = Ask "HTTPS port for phone access" "8443"
    $script:Port = [int]$portInput

    # Step 3: Detect host IP
    $hostIp = Get-HostLanIp

    # Step 4: Confirm
    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Setup Summary" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  Guest type:    $($script:Mode)"
    Write-Host "  Guest address: $($script:GuestIp)"
    if ($script:SshPort -ne 22) {
        Write-Host "  SSH via:       $($script:SshHost):$($script:SshPort)"
    }
    Write-Host "  Host LAN IP:   $hostIp"
    Write-Host "  HTTPS port:    $($script:Port)"
    Write-Host "  Phone URL:     https://${hostIp}:$($script:Port)"
    Write-Host ""

    $confirm = Ask "Proceed with setup? (y/n)" "y"
    if ($confirm -ne "y") { exit 0 }

    # Step 5: Execute
    Write-Host ""

    # Set up port forwarding FIRST -- the cert distribution server
    # started below needs to be reachable from the phone on the host
    # interface, not just the guest's localhost.
    Info "setting up port forwarding..."
    Add-PortForwarding
    Add-FirewallRule

    Info "running setup on guest..."
    # --no-serve-certs: the guest-side `intendant lan setup` returns
    # once the certs + nginx config are in place. We run the cert
    # distribution server ourselves below so we can display the p12
    # password in the Windows console (the guest's SSH stdout isn't
    # always visible, especially for WSL flows).
    Invoke-IntendantLan "setup" @(
        "--port", "$($script:Port)"
        "--lan-ip", "$hostIp"
        "--cert-port", "$($script:CertPort)"
        "--no-serve-certs"
    )

    # Save config for future -Recert / -Remove
    Save-Config
    Info "config saved to $($script:ConfigPath)"

    # --- Certificate installation (strict pairing) ---
    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host "  Certificate Installation" -ForegroundColor Cyan
    Write-Host "========================================================" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  Strict enrollment will now start in this console."
    Write-Host "  Open the printed HTTPS enrollment URL in the client browser,"
    Write-Host "  copy the browser-observed server certificate SHA-256 fingerprint,"
    Write-Host "  and paste it into the prompt before any secret is revealed."
    Write-Host ""
    Write-Host "  Press Ctrl+C after the client has downloaded and installed ca.crt"
    Write-Host "  and client.p12."
    Write-Host ""
    Invoke-IntendantLan "serve-certs" @(
        "--lan-ip", "$hostIp"
        "--cert-port", "$($script:CertPort)"
        "--port", "$($script:Port)"
    )

    # WSL warning
    if ($script:Mode -eq "wsl") {
        Write-Host ""
        Warn "WSL2's IP changes on every restart. After rebooting, run:"
        Warn "  .\setup-lan.bat -Recert"
        Write-Host ""
        $autoTask = Ask "Create a scheduled task to do this automatically at login? (y/n)" "y"
        if ($autoTask -eq "y") {
            $action = New-ScheduledTaskAction -Execute "powershell.exe" `
                -Argument "-WindowStyle Hidden -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Recert"
            $trigger = New-ScheduledTaskTrigger -AtLogOn
            Register-ScheduledTask -TaskName "IntendantLAN" -Action $action `
                -Trigger $trigger -RunLevel Highest -Force | Out-Null
            Info "scheduled task 'IntendantLAN' created"
        }
    }

    Write-Host ""
    Write-Host "========================================================" -ForegroundColor Green
    Write-Host "  Setup complete!" -ForegroundColor Green
    Write-Host "========================================================" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Phone connects to: https://${hostIp}:$($script:Port)"
    Write-Host ""
}

# -- Maintenance commands --

function Run-Remove {
    if (-not (Load-Config)) {
        Warn "no saved config found -- using defaults"
        $script:Port = 8443
    }

    Info "removing intendant LAN setup..."
    Remove-PortForwarding
    Remove-FirewallRule

    # Remove scheduled task if it exists
    Unregister-ScheduledTask -TaskName "IntendantLAN" -Confirm:$false -ErrorAction SilentlyContinue

    Info "removing guest-side config..."
    try {
        if ($script:Mode -eq "wsl") {
            Resolve-GuestIp
        }
        Invoke-IntendantLan "remove"
    } catch {
        Warn "could not remove guest config (run 'sudo intendant lan remove' manually in the guest)"
    }

    Remove-Item $script:ConfigPath -ErrorAction SilentlyContinue
    Info "done"
}

function Run-Recert {
    if (-not (Load-Config)) {
        Die "no saved config found -- run the setup wizard first"
    }

    Resolve-GuestIp

    Info "guest IP: $($script:GuestIp)"

    # Update port forwarding (especially important for WSL2)
    Add-PortForwarding
    Info "port forwarding updated"

    $recertArgs = @()
    if ($Force) { $recertArgs += "--force" }

    Info "regenerating server cert on guest..."
    Invoke-IntendantLan "recert" $recertArgs

    $hostIp = Get-HostLanIp
    Write-Host ""
    Info "done -- phone connects to: https://${hostIp}:$($script:Port)"
}

# -- Main --

if ($Remove) {
    Run-Remove
} elseif ($Recert) {
    Run-Recert
} else {
    Run-Wizard
}
