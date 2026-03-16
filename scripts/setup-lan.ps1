#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Intendant LAN Access Setup for Windows hosts with a Linux guest.

.DESCRIPTION
    Sets up port forwarding from the Windows host to a Linux guest (WSL2 or VM)
    running intendant, then runs setup-lan.sh inside the guest to configure
    nginx with mTLS.

.PARAMETER Mode
    Guest type: "wsl" (default) or "vm"

.PARAMETER VmAddress
    IP address of the VM guest (required for -Mode vm)

.PARAMETER VmUser
    SSH user for the VM guest (default: same as current Windows user)

.PARAMETER Port
    HTTPS port for phone access (default: 8443)

.PARAMETER Remove
    Remove all port forwarding rules and firewall rules

.PARAMETER Recert
    Regenerate server cert (IP changed) inside the guest

.PARAMETER Force
    Force regeneration even if IP matches

.EXAMPLE
    .\setup-lan.ps1                                    # WSL2 guest
    .\setup-lan.ps1 -Mode vm -VmAddress 192.168.1.50   # Hyper-V / VirtualBox guest
    .\setup-lan.ps1 -Remove                            # Uninstall everything
    .\setup-lan.ps1 -Recert                            # Regenerate server cert
#>

param(
    [ValidateSet("wsl", "vm")]
    [string]$Mode = "wsl",

    [string]$VmAddress = "",
    [string]$VmUser = $env:USERNAME,

    [int]$Port = 8443,
    [int]$CertPort = 9999,

    [switch]$Remove,
    [switch]$Recert,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$FwRuleName = "Intendant LAN Access"
$ProxyPorts = @($Port, $CertPort)

function Info($msg)  { Write-Host ":: $msg" -ForegroundColor Cyan }
function Warn($msg)  { Write-Host "!! $msg" -ForegroundColor Yellow }
function Die($msg)   { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

function Get-WslIp {
    $ip = wsl hostname -I 2>$null
    if (-not $ip) { Die "could not get WSL IP — is WSL running?" }
    return ($ip.Trim() -split '\s+')[0]
}

function Get-GuestIp {
    if ($Mode -eq "wsl") {
        return Get-WslIp
    } else {
        if (-not $VmAddress) { Die "specify -VmAddress for VM mode" }
        return $VmAddress
    }
}

function Get-HostLanIp {
    $ip = (Get-NetIPAddress -AddressFamily IPv4 |
           Where-Object { $_.InterfaceAlias -notmatch 'Loopback|WSL|vEthernet' -and $_.PrefixOrigin -ne 'WellKnown' } |
           Sort-Object -Property InterfaceMetric |
           Select-Object -First 1).IPAddress
    if (-not $ip) { Die "could not detect LAN IP" }
    return $ip
}

function Add-PortForwarding($guestIp) {
    foreach ($p in $ProxyPorts) {
        # Remove existing rule if any
        netsh interface portproxy delete v4tov4 listenport=$p listenaddress=0.0.0.0 2>$null | Out-Null

        Info "forwarding 0.0.0.0:$p → ${guestIp}:$p"
        netsh interface portproxy add v4tov4 `
            listenport=$p listenaddress=0.0.0.0 `
            connectport=$p connectaddress=$guestIp | Out-Null
    }
}

function Add-FirewallRule {
    # Remove existing rule if any
    Remove-NetFirewallRule -DisplayName $FwRuleName -ErrorAction SilentlyContinue

    Info "adding firewall rule for ports $($ProxyPorts -join ', ')"
    New-NetFirewallRule `
        -DisplayName $FwRuleName `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort $ProxyPorts `
        -Profile Private | Out-Null
}

function Remove-PortForwarding {
    foreach ($p in $ProxyPorts) {
        netsh interface portproxy delete v4tov4 listenport=$p listenaddress=0.0.0.0 2>$null | Out-Null
    }
    Info "port forwarding rules removed"
}

function Remove-FirewallRule {
    Remove-NetFirewallRule -DisplayName $FwRuleName -ErrorAction SilentlyContinue
    Info "firewall rule removed"
}

function Invoke-GuestCommand($cmd) {
    if ($Mode -eq "wsl") {
        wsl bash -c $cmd
    } else {
        ssh "${VmUser}@${VmAddress}" $cmd
    }
}

function Copy-ScriptToGuest {
    $scriptPath = Join-Path $PSScriptRoot "setup-lan.sh"
    if (-not (Test-Path $scriptPath)) {
        Die "setup-lan.sh not found in $PSScriptRoot"
    }

    if ($Mode -eq "wsl") {
        $wslPath = wsl wslpath -a ($scriptPath -replace '\\', '/')
        wsl cp $wslPath /tmp/setup-lan.sh
        wsl chmod +x /tmp/setup-lan.sh
    } else {
        scp $scriptPath "${VmUser}@${VmAddress}:/tmp/setup-lan.sh"
        ssh "${VmUser}@${VmAddress}" "chmod +x /tmp/setup-lan.sh"
    }
}

# ── Main ──

if ($Remove) {
    Info "removing intendant LAN setup..."
    Remove-PortForwarding
    Remove-FirewallRule

    Info "removing guest-side config..."
    try { Invoke-GuestCommand "sudo /tmp/setup-lan.sh --remove" } catch {
        Warn "could not remove guest config (run 'sudo setup-lan.sh --remove' manually)"
    }
    Info "done"
    exit 0
}

if ($Recert) {
    Info "regenerating server cert on guest..."
    $recertArgs = "--recert"
    if ($Force) { $recertArgs += " --force" }
    Invoke-GuestCommand "sudo /tmp/setup-lan.sh $recertArgs"

    if ($Mode -eq "wsl") {
        # WSL IP may have changed — update port forwarding
        $guestIp = Get-WslIp
        Add-PortForwarding $guestIp
        Info "port forwarding updated for WSL IP $guestIp"
    }
    exit 0
}

# ── Full Setup ──

$guestIp = Get-GuestIp
$hostIp = Get-HostLanIp

Info "host LAN IP:  $hostIp"
Info "guest IP:     $guestIp"
Info "mode:         $Mode"
Write-Host ""

# 1. Copy setup-lan.sh to guest
Info "copying setup script to guest..."
Copy-ScriptToGuest

# 2. Run setup-lan.sh inside the guest
Info "running setup on guest..."
Invoke-GuestCommand "sudo /tmp/setup-lan.sh --port $Port"

# 3. Set up Windows port forwarding
Add-PortForwarding $guestIp

# 4. Add firewall rule
Add-FirewallRule

# 5. WSL IP instability warning
if ($Mode -eq "wsl") {
    Write-Host ""
    Warn "WSL2 IP changes on every restart. After rebooting, run:"
    Warn "  .\setup-lan.ps1 -Recert"
    Warn "This updates port forwarding and regenerates the server cert if needed."
    Write-Host ""
    Warn "To make this automatic, create a scheduled task:"
    Warn '  $action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument "-File $PSCommandPath -Recert"'
    Warn '  $trigger = New-ScheduledTaskTrigger -AtLogOn'
    Warn '  Register-ScheduledTask -TaskName "IntendantLAN" -Action $action -Trigger $trigger -RunLevel Highest'
}

Write-Host ""
Info "setup complete!"
Info "phone connects to: https://${hostIp}:${Port}"
Info "cert download at:  http://${hostIp}:${CertPort}/ (while setup-lan.sh serves)"
