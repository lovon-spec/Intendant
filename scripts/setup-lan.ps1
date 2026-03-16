#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Intendant LAN Access Setup for Windows hosts with a Linux guest.

.DESCRIPTION
    Interactive setup that configures port forwarding from the Windows host
    to a Linux guest (WSL2 or VM) running intendant, then runs setup-lan.sh
    inside the guest to configure nginx with mTLS.

    Run without parameters for the interactive wizard.
    Use -Remove, -Recert, or -Force for maintenance operations.

.EXAMPLE
    .\setup-lan.ps1              # Interactive setup wizard
    .\setup-lan.ps1 -Remove      # Uninstall everything
    .\setup-lan.ps1 -Recert      # Regenerate server cert (IP changed)
#>

param(
    [switch]$Remove,
    [switch]$Recert,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$FwRuleName = "Intendant LAN Access"

# State — populated by wizard or loaded from config
$script:Mode = ""
$script:GuestIp = ""
$script:VmUser = ""
$script:VmAddress = ""
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

# ── Config persistence ──

function Save-Config {
    @{
        Mode      = $script:Mode
        VmAddress = $script:VmAddress
        VmUser    = $script:VmUser
        Port      = $script:Port
    } | ConvertTo-Json | Set-Content $script:ConfigPath
}

function Load-Config {
    if (Test-Path $script:ConfigPath) {
        $cfg = Get-Content $script:ConfigPath | ConvertFrom-Json
        $script:Mode      = $cfg.Mode
        $script:VmAddress = $cfg.VmAddress
        $script:VmUser    = $cfg.VmUser
        $script:Port      = $cfg.Port
        return $true
    }
    return $false
}

# ── Network helpers ──

function Get-WslIp {
    $ip = wsl hostname -I 2>$null
    if (-not $ip) { Die "could not get WSL IP — is WSL running? Start it with: wsl" }
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

# ── Port forwarding & firewall ──

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

# ── Guest communication ──

function Invoke-GuestCommand($cmd) {
    if ($script:Mode -eq "wsl") {
        wsl bash -c $cmd
    } else {
        ssh "$($script:VmUser)@$($script:VmAddress)" $cmd
    }
}

function Copy-ScriptToGuest {
    $scriptPath = Join-Path $PSScriptRoot "setup-lan.sh"
    if (-not (Test-Path $scriptPath)) {
        Die "setup-lan.sh not found in $PSScriptRoot"
    }

    if ($script:Mode -eq "wsl") {
        $wslPath = wsl wslpath -a ($scriptPath -replace '\\', '/')
        wsl cp $wslPath /tmp/setup-lan.sh
        wsl chmod +x /tmp/setup-lan.sh
    } else {
        scp $scriptPath "$($script:VmUser)@$($script:VmAddress):/tmp/setup-lan.sh"
        ssh "$($script:VmUser)@$($script:VmAddress)" "chmod +x /tmp/setup-lan.sh"
    }
}

function Test-GuestSsh {
    try {
        ssh -o BatchMode=yes -o ConnectTimeout=5 "$($script:VmUser)@$($script:VmAddress)" "echo ok" 2>$null | Out-Null
        return $true
    } catch {
        return $false
    }
}

# ── Interactive wizard ──

function Run-Wizard {
    Write-Host ""
    Write-Host "════════════════════════════════════════════════════════" -ForegroundColor Cyan
    Write-Host "  Intendant LAN Access Setup" -ForegroundColor Cyan
    Write-Host "════════════════════════════════════════════════════════" -ForegroundColor Cyan

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

        Write-Host ""
        $script:VmAddress = Ask "Guest IP address"
        if (-not $script:VmAddress) { Die "IP address is required" }

        $script:VmUser = Ask "SSH username on the guest" $env:USERNAME

        Info "testing SSH connection to $($script:VmUser)@$($script:VmAddress)..."
        if (-not (Test-GuestSsh)) {
            Warn "could not connect via SSH"
            Write-Host ""
            Write-Host "  Make sure:" -ForegroundColor Yellow
            Write-Host "    - The VM is running"
            Write-Host "    - SSH server is installed: sudo apt install openssh-server"
            Write-Host "    - You can SSH manually: ssh $($script:VmUser)@$($script:VmAddress)"
            Write-Host ""
            $retry = Ask "Try again after fixing? (y/n)" "y"
            if ($retry -eq "y") {
                if (-not (Test-GuestSsh)) { Die "still cannot connect" }
            } else {
                Die "SSH connection required"
            }
        }
        Info "SSH connection OK"

        $script:GuestIp = $script:VmAddress
    }

    # Step 2: Port
    Write-Host ""
    $portInput = Ask "HTTPS port for phone access" "8443"
    $script:Port = [int]$portInput

    # Step 3: Detect host IP
    $hostIp = Get-HostLanIp

    # Step 4: Confirm
    Write-Host ""
    Write-Host "════════════════════════════════════════════════════════" -ForegroundColor Cyan
    Write-Host "  Setup Summary" -ForegroundColor Cyan
    Write-Host "════════════════════════════════════════════════════════" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  Guest type:    $($script:Mode)"
    Write-Host "  Guest address: $($script:GuestIp)"
    Write-Host "  Host LAN IP:   $hostIp"
    Write-Host "  HTTPS port:    $($script:Port)"
    Write-Host "  Phone URL:     https://${hostIp}:$($script:Port)"
    Write-Host ""

    $confirm = Ask "Proceed with setup? (y/n)" "y"
    if ($confirm -ne "y") { exit 0 }

    # Step 5: Execute
    Write-Host ""

    # Set up port forwarding FIRST — setup-lan.sh will start a temporary
    # HTTP server for cert download, which needs to be reachable from the phone
    Info "setting up Windows port forwarding..."
    Add-PortForwarding
    Add-FirewallRule

    Info "copying setup script to guest..."
    Copy-ScriptToGuest

    Info "running setup on guest (follow the prompts there)..."
    Invoke-GuestCommand "sudo /tmp/setup-lan.sh --port $($script:Port)"

    # Save config for future --recert / --remove
    Save-Config
    Info "config saved to $($script:ConfigPath)"

    # WSL warning
    if ($script:Mode -eq "wsl") {
        Write-Host ""
        Warn "WSL2's IP changes on every restart. After rebooting, run:"
        Warn "  .\setup-lan.ps1 -Recert"
        Write-Host ""
        $autoTask = Ask "Create a scheduled task to do this automatically at login? (y/n)" "y"
        if ($autoTask -eq "y") {
            $action = New-ScheduledTaskAction -Execute "powershell.exe" `
                -Argument "-WindowStyle Hidden -File `"$PSCommandPath`" -Recert"
            $trigger = New-ScheduledTaskTrigger -AtLogOn
            Register-ScheduledTask -TaskName "IntendantLAN" -Action $action `
                -Trigger $trigger -RunLevel Highest -Force | Out-Null
            Info "scheduled task 'IntendantLAN' created"
        }
    }

    Write-Host ""
    Write-Host "════════════════════════════════════════════════════════" -ForegroundColor Green
    Write-Host "  Setup complete!" -ForegroundColor Green
    Write-Host "════════════════════════════════════════════════════════" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Phone connects to: https://${hostIp}:$($script:Port)"
    Write-Host ""
}

# ── Maintenance commands ──

function Run-Remove {
    if (-not (Load-Config)) {
        Warn "no saved config found — using defaults"
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
        Invoke-GuestCommand "sudo /tmp/setup-lan.sh --remove"
    } catch {
        Warn "could not remove guest config (run 'sudo setup-lan.sh --remove' manually in the guest)"
    }

    Remove-Item $script:ConfigPath -ErrorAction SilentlyContinue
    Info "done"
}

function Run-Recert {
    if (-not (Load-Config)) {
        Die "no saved config found — run the setup wizard first"
    }

    Resolve-GuestIp

    Info "guest IP: $($script:GuestIp)"

    # Update port forwarding (especially important for WSL2)
    Add-PortForwarding
    Info "port forwarding updated"

    $recertArgs = "--recert"
    if ($Force) { $recertArgs += " --force" }

    Info "regenerating server cert on guest..."
    Invoke-GuestCommand "sudo /tmp/setup-lan.sh $recertArgs"

    $hostIp = Get-HostLanIp
    Write-Host ""
    Info "done — phone connects to: https://${hostIp}:$($script:Port)"
}

# ── Main ──

if ($Remove) {
    Run-Remove
} elseif ($Recert) {
    Run-Recert
} else {
    Run-Wizard
}
