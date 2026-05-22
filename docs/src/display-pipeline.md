# Display Pipeline

Intendant provides agents with graphical displays they can see and interact with. The display pipeline uses WebRTC for low-latency video streaming from the agent's display to the browser, with remote input flowing back via data channels.

## Overview

```
[CaptureBackend] → encode (VP8/H264) → WebRTC video track → browser
  browser input  → WebRTC data channel → input injection on display
```

The pipeline handles two types of displays:

- **Virtual displays** — Xvfb-managed displays (`:99`, `:100`, etc.) created on demand when the agent runs graphical applications
- **User session displays** — the user's real desktop (`:0` on Linux, native on macOS), opt-in via the DisplayControl autonomy category

Both go through the same lifecycle: capture, encode, stream, and record.

## Platform Capture Backends

### X11 (Linux)

Uses `x11rb` with XShm (shared memory) for zero-copy frame capture from X11 displays. xdotool handles keyboard and mouse input injection.

### Wayland (Linux)

Uses PipeWire with DMA-BUF for zero-copy capture directly from the compositor. ydotool handles input injection.

### macOS

Uses ScreenCaptureKit for capture and cliclick for input injection.

### Windows

Captures the DWM-composed desktop via GDI `BitBlt` by default — the path that works on virtual / RDP / cloud / headless adapters — with DXGI Desktop Duplication available as an opt-in GPU fast path (`INTENDANT_WINDOWS_CAPTURE=dxgi`) on hosts with real scanout. `SendInput` handles keyboard and mouse injection, and encode targets Media Foundation H264. See [Windows Support](./windows-support.md) for details and current maturity.

### Display Detection

The `DisplayBackend` enum auto-detects the available backend at runtime. On Linux, it checks for Wayland (`WAYLAND_DISPLAY`) before falling back to X11 (`DISPLAY`). On macOS and Windows, the native backend is always used.

## Video Encoding

Two codecs are supported with automatic negotiation:

### VP8

Always available as a software fallback. Uses libvpx for encoding. Lower latency but higher CPU usage at high resolutions.

### H264

Hardware-accelerated encoding when available:

- **macOS**: VideoToolbox (zero-copy from ScreenCaptureKit frames)
- **Linux**: ffmpeg with VA-API (hardware) or libx264 (software fallback)

H264 fmtp parameters (profile-level-id, packetization-mode) are parsed from SDP and matched during codec negotiation. Multi-slice frame assembly handles H264 NAL unit fragmentation.

### Codec Negotiation

On WebRTC session setup, the browser's SDP offer is parsed for supported codecs. H264 is preferred when the server has hardware encoding available; VP8 is used as fallback. Multi-peer scenarios support per-peer codec renegotiation.

## WebRTC Signaling

The browser and server exchange SDP offers/answers and ICE candidates over WebSocket:

```json
// Browser → Server
{"t": "display_offer", "display_id": "...", "sdp": "..."}
{"t": "display_ice", "display_id": "...", "candidate": "..."}

// Server → Browser
{"t": "display_answer", "display_id": "...", "sdp": "..."}
{"t": "display_ice", "display_id": "...", "candidate": "..."}
```

STUN/TURN servers are configurable via `[webrtc]` in `intendant.toml`:

```toml
[webrtc]
[[webrtc.ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[webrtc.ice_servers]]
urls = ["turn:turn.example.com:3478"]
username = "user"
credential = "pass"
```

## ICE-TCP for NAT'd / Tunneled Deployments

When the browser can't reach the agent's UDP host candidates — typically because the agent is inside a NAT'd VM (VirtualBox NAT mode, Hyper-V, etc.) with only the dashboard port forwarded — the pipeline falls back to ICE-TCP. The server multiplexes ICE-TCP onto the same HTTP port that serves the dashboard, so no extra port forwarding is needed beyond what the dashboard already requires.

The advertised TCP candidate's IP is derived from the browser's `Host:` HTTP header: whatever non-loopback IP the browser used to load the dashboard is what the server advertises in SDP as its ICE-TCP host candidate. This means:

- **Accessing via a routable IP** (e.g. `http://192.168.1.42:8765`) — video works over ICE-TCP automatically. The browser sees a non-loopback remote candidate and forms a TCP candidate pair.
- **Accessing via `http://localhost:8765`** — video does **not** work over ICE-TCP. Firefox (and Chrome) filter remote loopback candidates as a security mitigation. The workaround is to bind the port-forward on all interfaces and access via the host's LAN IP instead of localhost.

For VirtualBox NAT users:
```
VBoxManage modifyvm <vm> --natpf1 delete intendant
VBoxManage modifyvm <vm> --natpf1 "intendant,tcp,0.0.0.0,8765,,8765"
```
Then access the dashboard at `http://<host-LAN-IP>:8765`. Find the host's LAN IP with `ipconfig` (Windows) or `ip addr` (Linux/macOS).

## Multi-Monitor Support

Each physical or virtual display gets a stable `display_id` based on its output name or display number. The pipeline supports:

- **Display enumeration** — lists all available displays with resolution and position
- **Display picker** — UI in the web dashboard status bar for selecting which display to view
- **Per-display metrics** — frame rate, encode time, and bandwidth tracked per display
- **Dynamic resize** — captures adapt when displays change resolution
- **Hotplug detection** — new displays are detected and old ones cleaned up

## Bidirectional Clipboard

Clipboard content is synced between the browser and the agent's display via WebRTC data channels. Supports both text and images (PNG). On copy in the browser, content is sent to the display's clipboard (xclip on Linux, pbcopy on macOS). On copy in the display, content is sent to the browser.

## Remote Input

Keyboard and mouse events from the browser flow back through WebRTC data channels:

- **Keyboard**: Key press/release events with modifier state tracking, mapped to platform-specific keycodes
- **Mouse**: Move, click, scroll, and drag events with coordinate translation
- **Modifier recovery**: Blur/focus events reset modifier state to prevent stuck keys

## Virtual Display Management

On Linux, Xvfb displays are auto-launched lazily on the first command that needs a display (`execAsAgent`, `captureScreen`). The flow:

1. Check if a display is already accessible → skip
2. Command batch needs a display? No → skip
3. Launch Xvfb, preferring `:99` for a predictable port
4. Emit `DisplayReady` event → triggers recording and WebRTC streaming
5. On drop, kill Xvfb via `XvfbGuard`

Orphaned Xvfb processes from previous sessions are detected (via `/proc/<pid>/cmdline`) and reclaimed.

On macOS, the native display is always accessible — no virtual display needed.

## Recording

Display recording runs in parallel with WebRTC streaming via ffmpeg:

- **Linux**: `x11grab` input from X11 displays
- **macOS**: `avfoundation` input

Recordings are segmented into MP4 files (configurable duration, default 60s) for efficient seeking and replay. The web dashboard provides a recording player with timeline, seeking, and speed control.

```toml
[recording]
enabled = true
framerate = 15
segment_duration_secs = 60
quality = "medium"   # "low" (CRF 35), "medium" (CRF 28), "high" (CRF 20)
```

## Frame Registry

The `FrameRegistry` stores high-quality JPEG frames captured from displays and browser cameras. Frames are stored in the session directory (`<session>/frames/`) with metadata in `frames.jsonl`.

Frames serve two purposes:
- **Context for CU models** — `auto_attach_display_frames()` grabs the latest frame per display stream for the agent's next turn
- **Presence inspection** — the `inspect_frame` and `inspect_frames` tools let the presence layer examine what's on screen

Each frame has a `sent_to_live` flag tracking whether the browser-side live model has already seen it, preventing redundant sends.

## Display Metrics

Per-display metrics are tracked and surfaced in the web dashboard Stats tab:

- Frame capture rate (FPS)
- Encode latency (ms per frame)
- WebRTC bandwidth (bytes/sec)
- ICE connection state
- Codec in use (VP8 or H264)
