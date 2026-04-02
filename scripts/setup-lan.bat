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
powershell -ExecutionPolicy Bypass -NoProfile -File "%PS1%" -ScriptDir "%~dp0" %*
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

# -- Port forwarding & firewall --

function Add-PortForwarding {
    $ports = @($script:Port, $script:CertPort)
    foreach ($p in $ports) {
        netsh interface portproxy delete v4tov4 listenport=$p listenaddress=0.0.0.0 2>$null | Out-Null
        Info "forwarding 0.0.0.0:$p -> $($script:GuestIp):$p"
        netsh interface portproxy add v4tov4 `
            listenport=$p listenaddress=0.0.0.0 `
            connectport=$p connectaddress=$script:GuestIp | Out-Null
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
    foreach ($p in @($script:Port, $script:CertPort)) {
        netsh interface portproxy delete v4tov4 listenport=$p listenaddress=0.0.0.0 2>$null | Out-Null
    }
    Info "port forwarding rules removed"
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

function Find-LocalGuestScript {
    # $PSScriptRoot is %TEMP% (bat copies itself there as .ps1).
    # $ScriptDir is the bat's real directory, passed via -ScriptDir from the batch preamble.
    $dirs = @($PSScriptRoot, $PWD.Path)
    if ($ScriptDir) { $dirs = @($ScriptDir) + $dirs }
    foreach ($dir in $dirs) {
        $candidate = Join-Path $dir "setup-lan.sh"
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

function Copy-AndRunOnGuest($setupArgs) {
    # Temporarily allow stderr from ssh/scp without crashing (host key warnings, etc.)
    $ErrorActionPreference = "Continue"

    $scriptPath = Find-LocalGuestScript
    $ghUrl = "https://raw.githubusercontent.com/lovon-spec/intendant/main/scripts/setup-lan.sh"
    $runCmd = "sudo /tmp/setup-lan.sh $setupArgs"

    if ($script:Mode -eq "wsl") {
        if ($scriptPath) {
            $wslPath = wsl wslpath -a ($scriptPath -replace '\\', '/')
            wsl cp $wslPath /tmp/setup-lan.sh
        } else {
            wsl curl -sfL $ghUrl -o /tmp/setup-lan.sh
            if ($LASTEXITCODE -ne 0) { Die "failed to download setup-lan.sh in WSL" }
        }
        wsl bash -c "chmod +x /tmp/setup-lan.sh && $runCmd"
    } else {
        $sshArgs = Get-SshArgs
        if ($scriptPath) {
            $scpPort = if ($script:SshPort -ne 22) { @("-P", $script:SshPort) } else { @() }
            scp @scpPort $scriptPath "$($script:VmUser)@$($script:SshHost):/tmp/setup-lan.sh"
        }
        # Single SSH session: download (if needed) + chmod + run
        $remoteCmd = ""
        if (-not $scriptPath) {
            $remoteCmd = "curl -sfL '$ghUrl' -o /tmp/setup-lan.sh && "
        }
        $remoteCmd += "chmod +x /tmp/setup-lan.sh && $runCmd"
        ssh @sshArgs $remoteCmd
    }
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
    Write-Host "  Intendant LAN Access Setup" -ForegroundColor Cyan
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
            Write-Host "    Open VirtualBox > select your VM > Settings > Network."
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
            Write-Host ""
            Info "VirtualBox NAT mode: connections go through port forwarding"
            Write-Host ""
            Write-Host "  Before continuing, set up port forwarding in VirtualBox:" -ForegroundColor Yellow
            Write-Host ""
            Write-Host "    1. Open VirtualBox > select your VM > Settings > Network"
            Write-Host "    2. Expand 'Advanced' > click 'Port Forwarding'"
            Write-Host "    3. Add these rules (click the + icon):"
            Write-Host ""
            Write-Host "       Name      Host IP      Host Port   Guest Port" -ForegroundColor Cyan
            Write-Host "       SSH       127.0.0.1    2222        22"
            Write-Host "       HTTPS     127.0.0.1    8443        8443"
            Write-Host "       Cert      127.0.0.1    9999        9999"
            Write-Host ""
            Write-Host "    4. Click OK, then OK again to save."
            Write-Host ""

            $ready = Ask "Press Enter when done (or 'q' to quit)" ""
            if ($ready -eq "q") { exit 0 }

            $script:SshHost = "127.0.0.1"
            $sshPortInput = Ask "SSH host port from step 3 above" "2222"
            $script:SshPort = [int]$sshPortInput
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
                Write-Host "    - VirtualBox port forwarding is configured (see above)"
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

    # Set up port forwarding FIRST -- setup-lan.sh will start a temporary
    # HTTP server for cert download, which needs to be reachable from the phone
    Info "setting up Windows port forwarding..."
    Add-PortForwarding
    Add-FirewallRule

    Info "running setup on guest (follow the prompts there)..."
    Copy-AndRunOnGuest "--port $($script:Port) --lan-ip $hostIp"

    # Save config for future --recert / --remove
    Save-Config
    Info "config saved to $($script:ConfigPath)"

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
        Copy-AndRunOnGuest "--remove"
    } catch {
        Warn "could not remove guest config (run 'sudo setup-lan.sh --remove' manually in the guest)"
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

    $recertArgs = "--recert"
    if ($Force) { $recertArgs += " --force" }

    Info "regenerating server cert on guest..."
    Copy-AndRunOnGuest $recertArgs

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
