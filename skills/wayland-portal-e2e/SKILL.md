---
name: wayland-portal-e2e
description: Use when running Intendant Wayland display-capture E2E tests against a remote GNOME Wayland VM where the XDG Desktop Portal screen-sharing dialog must be approved programmatically or through a remote desktop window. Covers GNOME Remote Desktop setup, FreeRDP wrapping for Computer Use, portal approval, verification, cleanup, and safety boundaries.
---

# Wayland Portal E2E Approval

Use this skill for Intendant Wayland display-capture smoke tests when the target is a remote GNOME Wayland VM and the XDG Desktop Portal prompt must be approved without the human sitting at that VM.

This is test harness infrastructure, not a product consent bypass. It makes the real GNOME portal dialog reachable through a remote desktop window so an authorized operator can approve it.

## Why This Exists

GNOME's portal approval is session/compositor policy. Passwordless `sudo`, direct D-Bus calls, `wtype`, and shell-level input helpers do not grant screen-sharing consent. In the current Intendant Wayland path, capture uses a RemoteDesktop/ScreenCast portal session and the portal can re-prompt when a fresh Intendant instance starts. Treat that as expected security behavior unless the code is changed to use a restorable ScreenCast-only flow for view-only capture.

The repeatable route is:

1. Start GNOME Remote Desktop on the Wayland VM.
2. Open that RDP session from macOS through a local app bundle Computer Use can see.
3. Trigger Intendant's display grant.
4. Approve the real portal dialog inside the RDP session.

## Safety Boundary

- Get explicit user authorization before approving a screen-sharing or remote-interaction portal prompt.
- Use this only for test VMs or displays the user has identified as safe to share.
- Do not treat `sudo` as portal consent. GNOME portal approval is compositor/session policy, not Unix file permission.
- Use temporary RDP credentials. Do not commit or paste them into logs.
- Avoid changing global macOS Accessibility settings unless the user explicitly asks.
- Do not rely on saved screen coordinates without first inspecting the current RDP window; GNOME dialog layout can shift with resolution and theme.

## Preconditions

- SSH reaches the Wayland VM, commonly with `-J user@<jump-host>`.
- The target session is GNOME Wayland with `grdctl`, `xdg-desktop-portal`, `xdg-desktop-portal-gnome`, PipeWire, and WirePlumber running.
- The target user can run passwordless `sudo` if a temporary `/dev/uinput` helper is needed.
- Local machine can run Homebrew `freerdp` or already has `sdl-freerdp`.
- Intendant target daemon is running in the graphical session environment, or with:

```sh
XDG_RUNTIME_DIR=/run/user/1000
WAYLAND_DISPLAY=wayland-0
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
XDG_SESSION_TYPE=wayland
```

## Remote Headed Browser Validation

For Station/dashboard browser validation on a remote GNOME Wayland host, do not
assume a non-login SSH shell has the graphical environment or the Rustup
toolchain on `PATH`. On `user@192.168.1.206`, non-login SSH has historically
resolved `/usr/bin/cargo` / Rust 1.85 unless Rustup is prepended.

The dashboard validation helper imports the active graphical session variables
from `systemctl --user show-environment` for Linux `--headed` runs when
`DISPLAY` / `WAYLAND_DISPLAY` are absent. Keep the Rust `PATH` fix explicit and
use a throwaway port:

```sh
ssh user@192.168.1.206 '
  set -euo pipefail
  cd /home/user/projects/intendant-station-mainline-123e28c
  export PATH="$HOME/.cargo/bin:$PATH"

  # Rebuild explicitly instead if target/release/intendant is stale.
  [ -x target/release/intendant ] || cargo build --release -p intendant

  node scripts/validate-dashboard.cjs \
    --launch-dashboard \
    --port 8898 \
    --dashboard-binary target/release/intendant \
    --dashboard-arg --no-presence \
    --headed \
    --enable-gpu \
    --station-probe dock-hidden
'
```

For older helper versions, or when manually launching Chromium, import the
graphical session variables before running the validation:

```sh
while IFS= read -r line; do
  case "$line" in
    DISPLAY=*|WAYLAND_DISPLAY=*|XDG_RUNTIME_DIR=*|XDG_SESSION_TYPE=*|\
DBUS_SESSION_BUS_ADDRESS=*|XAUTHORITY=*|XDG_CURRENT_DESKTOP=*|DESKTOP_SESSION=*)
      export "$line"
      ;;
  esac
done < <(systemctl --user show-environment)
```

Check ports before launching and never reuse the protected local `8765` session
or kill unrelated Intendant instances:

```sh
ss -ltnp | grep -E ':(8898|8899|8900)\b' || true
pgrep -af intendant || true
```

If Chromium fails before CDP with `Missing X server or $DISPLAY`, the run is
still missing the graphical session environment. If CDP starts and the helper
returns a Station renderer failure such as `Station initializing` with a
`0x0` canvas, the remote headed/GPU browser path is working and the remaining
failure is a Station renderer/readiness condition, not an SSH/X11/ozone setup
problem.

## Preferred Flow

1. Enable GNOME Remote Desktop on the Wayland VM:

```sh
RDP_PASS="intendant-$(LC_ALL=C tr -dc A-Za-z0-9 </dev/urandom | head -c 12)"
ssh -J user@<jump-host> user@<wayland-vm> "
  set -euo pipefail
  export XDG_RUNTIME_DIR=/run/user/1000
  export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
  mkdir -p ~/.local/share/intendant-grd
  if [ ! -f ~/.local/share/intendant-grd/rdp.crt ] || [ ! -f ~/.local/share/intendant-grd/rdp.key ]; then
    openssl req -x509 -newkey rsa:2048 -nodes \
      -keyout ~/.local/share/intendant-grd/rdp.key \
      -out ~/.local/share/intendant-grd/rdp.crt \
      -subj '/CN=intendant-wayland-rdp' -days 30 >/dev/null 2>&1
    chmod 600 ~/.local/share/intendant-grd/rdp.key
  fi
  grdctl rdp set-tls-cert ~/.local/share/intendant-grd/rdp.crt
  grdctl rdp set-tls-key ~/.local/share/intendant-grd/rdp.key
  grdctl rdp set-credentials intendant '$RDP_PASS'
  grdctl rdp disable-view-only
  grdctl rdp enable
  systemctl --user enable --now gnome-remote-desktop.service
  grdctl status --show-credentials
"
```

2. Tunnel RDP to the local machine:

```sh
ssh -N -L 127.0.0.1:33890:<wayland-vm>:3389 user@<jump-host>
```

3. If Computer Use cannot attach to bare `sdl-freerdp`, create a signed local app wrapper with the actual FreeRDP binary inside the bundle. Reuse an existing `/Applications/IntendantFreeRDP.app` if it is already current. A shell wrapper is not enough; the visible SDL window may remain associated with `/opt/homebrew/bin/sdl-freerdp`.

```sh
brew install freerdp  # if needed
# Skip this rebuild if /Applications/IntendantFreeRDP.app already exists
# and its bundled sdl-freerdp.bin is current.
rm -rf /Applications/IntendantFreeRDP.app
mkdir -p /Applications/IntendantFreeRDP.app/Contents/MacOS
cp /opt/homebrew/bin/sdl-freerdp /Applications/IntendantFreeRDP.app/Contents/MacOS/sdl-freerdp.bin
```

Create `/Applications/IntendantFreeRDP.app/Contents/Info.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.intendant.freerdp</string>
  <key>CFBundleName</key><string>IntendantFreeRDP</string>
  <key>CFBundleDisplayName</key><string>IntendantFreeRDP</string>
  <key>CFBundleExecutable</key><string>IntendantFreeRDP</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict>
</plist>
```

Build the launcher:

```sh
cat >/tmp/intendant-freerdp-launcher.c <<'C'
#include <limits.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char exe[PATH_MAX];
    uint32_t len = sizeof(exe);
    if (_NSGetExecutablePath(exe, &len) != 0) return 127;
    char *slash = strrchr(exe, '/');
    if (!slash) return 127;
    *slash = '\0';
    char bin[PATH_MAX];
    snprintf(bin, sizeof(bin), "%s/sdl-freerdp.bin", exe);
    const char *pass = getenv("INTENDANT_RDP_PASS");
    if (!pass || !*pass) pass = "";
    char pass_arg[256];
    snprintf(pass_arg, sizeof(pass_arg), "/p:%s", pass);
    char *const argv[] = {
        (char *)bin,
        "/v:127.0.0.1:33890",
        "/u:intendant",
        pass_arg,
        "/cert:ignore",
        "/size:1280x800",
        "/dynamic-resolution",
        "+auto-reconnect",
        NULL,
    };
    execv(bin, argv);
    perror("execv sdl-freerdp.bin");
    return 127;
}
C
cc -O2 -Wall -o /Applications/IntendantFreeRDP.app/Contents/MacOS/IntendantFreeRDP /tmp/intendant-freerdp-launcher.c
xattr -cr /Applications/IntendantFreeRDP.app || true
codesign --force --deep --sign - /Applications/IntendantFreeRDP.app
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/IntendantFreeRDP.app
launchctl setenv INTENDANT_RDP_PASS "$RDP_PASS"
open -a IntendantFreeRDP
```

4. Confirm Computer Use can attach:

```text
Use Computer Use `list_apps`; it should show:
IntendantFreeRDP — com.intendant.freerdp

Then call `get_app_state` with app `IntendantFreeRDP`.
```

5. Trigger the Intendant display grant after the RDP window is visible:

```py
import asyncio, json, websockets

async def main():
    async with websockets.connect("ws://127.0.0.1:<tunnel-port>/ws") as ws:
        await ws.send(json.dumps({"action": "grant_user_display", "display_id": 0, "granted": True}))
        await ws.send(json.dumps({"action": "set_diagnostics_visual_marker", "display_id": 0, "enabled": True}))

asyncio.run(main())
```

6. In the FreeRDP window, approve the GNOME portal:

- Toggle **Allow Remote Interaction** on if input testing is needed.
- Select **Display**.
- Click **Share**.

7. Verify the Wayland portal succeeded:

```sh
ssh -J user@<jump-host> user@<wayland-vm> 'tail -120 /tmp/intendant-wayland.out'
```

Look for a successful portal/PipeWire stream start, then verify the dashboard display actually renders.

## Known Traps

- `wtype` may fail with `Compositor does not support the virtual keyboard protocol` on GNOME.
- `grim` may fail with `compositor doesn't support wlr-screencopy-unstable-v1`; GNOME is not wlroots.
- GNOME Shell screenshot D-Bus calls may return `Access denied`.
- A wrapper app that simply shells out to `/opt/homebrew/bin/sdl-freerdp` may appear in LaunchServices but still leave Computer Use unable to attach to the SDL child window.
- If the portal timeout is too short, remote approval becomes flaky. Current Intendant builds intentionally allow a generous Wayland portal approval window; older builds may time out before a remote operator can click the dialog.
- If the portal dialog disappears but the daemon still times out, check portal and session logs before repeatedly clicking:

```sh
journalctl --user -u xdg-desktop-portal -u xdg-desktop-portal-gnome -u gnome-remote-desktop --since "20 minutes ago" --no-pager
```

## Cleanup

When the run is done:

```sh
pkill -f 'IntendantFreeRDP|sdl-freerdp' || true
pkill -f '127.0.0.1:33890:.*:3389' || true
ssh -J user@<jump-host> user@<wayland-vm> '
  export XDG_RUNTIME_DIR=/run/user/1000
  export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
  grdctl rdp disable
  grdctl rdp clear-credentials
'
```

Keep the app wrapper if repeated Wayland portal E2E testing is expected; delete `/Applications/IntendantFreeRDP.app` if the machine should be left pristine.
