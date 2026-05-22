# Windows Support

Intendant builds and runs on Windows. This page covers the supported target,
how to set up a development machine, the per-OS backend architecture, and the
known limitations of the Windows port.

> **Maturity.** The Windows backends compile and link cleanly for
> `x86_64-pc-windows-msvc` and mirror the structure of the X11/macOS backends.
> Several runtime paths (live display capture, input injection, the voice audio
> bridge) require an interactive desktop session and are pending end-to-end
> validation on an interactive Windows host. Where a capability is not yet wired
> up it is called out under [Known Limitations](#known-limitations) — the port
> never panics or silently no-ops; unsupported paths return a clear error.

## Supported Target

| | |
|---|---|
| **Target triple** | `x86_64-pc-windows-msvc` |
| **ABI** | MSVC (not the GNU ABI) |
| **Minimum OS** | Windows 10 / Windows 11 (client), Windows Server 2019+ |

The MSVC ABI is required: the Windows-only crates (`windows`, `windows-sys`,
`arboard`, `clipboard-win`) link against the Microsoft C runtime and the
Windows SDK import libraries. The `x86_64-pc-windows-gnu` target is not
supported.

## Building and Running

The fastest path is the setup script, which is the Windows counterpart to
`scripts/setup-linux.sh` and `scripts/setup-macos.sh`:

```powershell
# From an elevated (Administrator) PowerShell, in the repo root:
powershell -ExecutionPolicy Bypass -File .\scripts\setup-windows.ps1

# Or just check what's already installed without changing anything:
powershell -ExecutionPolicy Bypass -File .\scripts\setup-windows.ps1 -Check
```

`-Check` separates **required-to-build** dependencies from optional/runtime
ones and sets its **exit code accordingly**: it exits **nonzero** if any
required build dependency is missing or unusable, and **0** when they are all
present — so CI and automation can gate on it. A missing optional dependency
(wasm-pack, ffmpeg, VB-CABLE) is reported but never fails the check.

`setup-windows.ps1` installs (idempotently, via Chocolatey where sensible).

**Required to build** (a missing/unusable one fails `-Check`):

- **rustup** with the default host set to `x86_64-pc-windows-msvc`
- **Visual Studio 2022 Build Tools** with the C++ workload
  (`visualstudio2022-workload-vctools`) — provides `cl.exe`, `link.exe`, and
  the Windows SDK. Required even for `cargo check`.
- **NASM** — required to assemble the `ring` crypto crate on windows-msvc. The
  Chocolatey package installs it to `C:\Program Files\NASM` and amends the
  *machine* `PATH`, which a freshly-spawned shell may not yet see; the script
  detects that case, adds the install directory to `PATH` (persisting it), and
  re-probes — so `-Check` recognizes NASM even when it isn't on the current
  `PATH`.
- **git**

**Optional / runtime / manual** (reported, but never fail `-Check`):

- **wasm-pack** (optional) — for rebuilding the presence-web WASM bundle.
- **ffmpeg** — the voice audio bridge shells out to `ffmpeg`/`ffplay`.
- **Media Foundation** — built into Windows client SKUs; on Windows Server the
  script enables the `Server-Media-Foundation` feature.

It then runs `cargo build --release --target x86_64-pc-windows-msvc`, producing:

- `target\x86_64-pc-windows-msvc\release\intendant.exe`
- `target\x86_64-pc-windows-msvc\release\intendant-runtime.exe`

One step the script **cannot** automate is the **VB-CABLE** virtual audio cable
(the vendor ships a manual installer, not a package). The script prints
instructions and flags it in the final summary. See
[Audio](#audio-ffmpeg--vb-cable-wasapi-bridge) below.

Manual build, if you already have the toolchain:

```powershell
rustup set default-host x86_64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc
```

Provide an API key via a `.env` file or environment variables exactly as on the
other platforms (see [Getting Started](./getting-started.md)), then:

```powershell
.\target\x86_64-pc-windows-msvc\release\intendant.exe "your task here"
.\target\x86_64-pc-windows-msvc\release\intendant.exe --web
```

## Per-OS Backend Architecture

Intendant prefers platform-agnostic code; where the OS forces a difference, the
Windows implementation slots in behind the same trait or `cfg` gate the X11,
Wayland, and macOS backends use. The Windows-specific backends are:

### Capture — DXGI Desktop Duplication

`display/windows.rs` captures the desktop via **DXGI Desktop Duplication**
(`IDXGIOutputDuplication`), the GPU-accelerated whole-desktop capture path on
Windows 8+. The duplication interface, the Direct3D 11 device, and the device
context are single-threaded COM objects that are not `Send` across `await`, so
— exactly like the X11 backend's XShm connection — the capture loop runs on a
dedicated `std::thread` and feeds the tokio runtime over a bounded `mpsc`
channel (the same drop-on-full backpressure policy as the macOS and X11
backends). Acquired GPU textures are copied into a CPU-readable staging texture,
mapped, and emitted as BGRA frames into the existing `bgra_to_i420` converter.
`DXGI_ERROR_ACCESS_LOST` (resolution change, full-screen exclusive app,
secure-desktop/UAC transition, GPU mode switch) tears down and re-acquires the
duplication on the next iteration.

### Input — SendInput

Keyboard and mouse injection uses the Win32 **`SendInput`** API. Keyboard events
carry a Win32 virtual-key code (mapped in `display/windows_keymap.rs`) plus the
`KEYEVENTF_EXTENDEDKEY` flag for keys in the extended block. Mouse moves use
`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, scaling the browser's
normalized `0.0..1.0` coordinates into the `0..65535` absolute coordinate space
that spans the entire virtual desktop.

### Clipboard — arboard

Bidirectional clipboard sync uses the **`arboard`** crate (which wraps the Win32
clipboard API via `clipboard-win`), providing text and RGBA image get/set. This
is the one platform where the clipboard is handled in-process: the macOS and
Linux arms shell out to `pbcopy` / `wl-copy` / `xclip`.

### Encode — Media Foundation H264

Windows video encode targets **Media Foundation H264** (with NVENC as a hardware
path where available) rather than VP8/libvpx. The libvpx-backed VP8 encoder and
the OpenSSL FFI crates are gated **off** Windows in `Cargo.toml`
(`cfg(not(target_os = "windows"))`), so the MSVC build never tries to compile
the `env-libvpx-sys` or `openssl-sys` C-FFI crates; the VP8 code paths are
themselves `cfg`'d off Windows. See
[Known Limitations](#known-limitations) for the current encode status.

### Audio — ffmpeg + VB-CABLE WASAPI bridge

There is no PulseAudio (Linux) or CoreAudio/BlackHole (macOS) on Windows. The
voice audio bridge instead routes through **VB-CABLE**, a virtual audio cable,
over WASAPI, shelling out to **`ffmpeg`/`ffplay`** to move audio in and out. VB-
CABLE is the Windows analogue of BlackHole on macOS / a PulseAudio null sink on
Linux: install it, then set **`CABLE Input (VB-Audio Virtual Cable)`** as the
default playback device so the bridge can play synthesized speech into the cable
and capture microphone audio from it.

### Process and Network Introspection

`platform.rs` provides the Windows implementations of the cross-platform
process and network helpers:

- **Process liveness** — `OpenProcess` (query-only) + `GetExitCodeProcess`,
  since Windows has no `kill(pid, 0)` equivalent.
- **Process command line** — `NtQueryInformationProcess` with the
  `ProcessCommandLineInformation` class, falling back to
  `QueryFullProcessImageNameW` (executable path only) when the full command line
  is unavailable.
- **Routable local addresses** — the **`if-addrs`** crate (wrapping
  `GetAdaptersAddresses`) backs `lan::routable_local_addrs`, which feeds the
  web-gateway advertise URLs and WebRTC ICE host-candidate gathering. The Unix
  path keeps its direct `getifaddrs(3)` walk.

## Known Limitations

These are tracked deferrals, not bugs. Each degrades with a clear error rather
than a panic or silent no-op.

- **Interactive session required for live capture/input/audio.** DXGI Desktop
  Duplication, `SendInput`, and the WASAPI audio bridge all need an interactive
  desktop session. They do **not** work on the headless / service /
  disconnected-RDP "Session 0" desktop, where Desktop Duplication is
  unavailable. End-to-end frame delivery, input injection, and voice are pending
  validation on an interactive Windows host.
- **Hardware-accelerated encode.** The Media Foundation H264 backend (and any
  NVENC fast path) is the planned Windows encoder; until it lands, the encode
  selection path on Windows falls through exactly as it does when no hardware
  H264 encoder is available on Linux/macOS. VP8/libvpx is intentionally not
  built on Windows.
- **`intendant lan` is gated off Windows.** The mTLS + nginx LAN-access
  subsystem depends on OpenSSL plus apt/brew and systemd/launchd service
  management, none of which apply on Windows; the Windows `LanBackend` returns
  `"intendant lan is not supported on Windows"`. To expose the dashboard to
  other devices from a Windows host, use the `scripts/setup-lan.bat` orchestrator
  (which drives `intendant lan` on a Linux guest over SSH/WSL), or front the
  dashboard with your own reverse proxy.
- **No virtual-display equivalent.** There is no Windows analogue of Xvfb, so
  the lazily-launched virtual displays the Linux pipeline uses do not exist on
  Windows. Capture targets the real interactive desktop only.
- **Landlock sandboxing is Linux-only.** The `--sandbox` filesystem-restriction
  flag has no effect on Windows (it is a Linux LSM feature).

## See Also

- [Getting Started](./getting-started.md) — building, the `.env` file, and run modes
- [Display Pipeline](./display-pipeline.md) — capture/encode/WebRTC architecture
- [Computer Use & Live Audio](./computer-use-and-audio.md) — input and voice
