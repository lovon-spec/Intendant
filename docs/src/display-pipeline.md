# Display Pipeline

Intendant gives agents graphical displays they can see and interact with. The
display pipeline streams a display's frames to one or more browsers over a
custom **WebRTC** transport with low latency, and routes remote keyboard/mouse
input back the other way. A parallel **tile-streaming** path replaces whole-frame
video with dirty-region updates on capable platforms.

This chapter covers the post-redesign architecture: one capture backend per
display fanning out to a shared multi-codec **encoder pool**, with each browser
peer driven by its own sans-I/O `rtc` peer connection. For sharing a *remote*
peer's display across machines, see [Peer Federation](./peer-federation.md). For
the Windows-specific backends, see [Windows Support](./windows-support.md).

## Overview

A `DisplaySession` (`src/bin/caller/display/mod.rs`) owns one display's whole
pipeline: the platform capture backend, a broadcast fan-out of raw frames, the
encoder pool, the set of connected WebRTC peers, the clipboard monitor, and the
tile-streaming bridge. The high-level data flow:

```
                    ┌──────────────────────────────────────────────┐
                    │              DisplaySession                    │
                    │                                                │
[CaptureBackend] ─mpsc(4)─▶ capture bridge ─broadcast(16)─▶ pool-feed bridge
  (X11/Wayland/             │  (Arc<Frame>)                   (BGRA→I420)
   macOS/Windows)           ▼                                      │
                       latest_frame                                ▼
                       (RwLock)                          ┌──────────────────┐
                            │                            │   EncoderPool    │
                            │                            │  baseline + sim- │
                            ▼                            │  ulcast + on-    │
                    tile-stream bridge                   │  demand encoders │
                    (XDamage / frame-diff)               └────────┬─────────┘
                            │                                      │ broadcast(16)
                            │ data channels                       │ Arc<EncodedFrame>
                            ▼                                      ▼
                    ┌───────────────────────────────────────────────────────┐
                    │  per-peer WebRtcPeer driver task (one per browser)      │
                    │  rtc PeerConnection + sockets · picks codec/layer ·     │
                    │  packetizes RTP · pumps UDP/TCP · TWCC tap              │
                    └───────────────────────────────────────────────────────┘
                            │ encrypted media + input data channels
                            ▼
                          browser
```

Two kinds of displays go through the same lifecycle:

- **Virtual displays** — on Linux, Xvfb displays (`:99`, `:100`, …) launched
  lazily when the agent first runs a graphical command. There is no Xvfb
  analogue on macOS or Windows.
- **User-session displays** — the user's real desktop (`:0` on Linux, the native
  display on macOS/Windows), opt-in via the `DisplayControl` autonomy category.

### Backpressure rules

Every stage is bounded and lossy by design — slow consumers drop frames rather
than back-pressuring the capture backend (which would degrade every other
viewer):

| Stage | Channel | Drop policy |
|---|---|---|
| Capture backend → tokio | `mpsc(4)` | backend drops on full (`try_send`) |
| Capture bridge → encoders | `broadcast(16)` | lagging subscribers skip |
| Pool I420 input | `broadcast(4)` | slow encoder sees `Lagged`, skips ahead |
| Encoder → per-peer | `broadcast(16)` | slow peer skips, counted as `encode_drops` |
| Per-peer encoded queue | `mpsc(8)` | dropped, counted in `peer_drops` |
| `latest_frame` | `RwLock<Option<…>>` | always overwritten, latest-wins |

## The Encoder Pool

The redesign's centerpiece (`display/encode/pool.rs`). The pre-pool design used
**one encoder per session** with the codec locked to the *first* peer's offer:
every later viewer had to accept that codec or its WebRTC offer failed outright.
The naive fix — one encoder per peer — is what transcoding gateways do, and it
scales badly (N× CPU; hardware encoders hit their session limit at ~5-8 viewers
and silently fall back to software). The pool instead follows the production SFU
pattern: **a small shared bank of encoders that all peers consume, with per-peer
packetization at the edge.**

Each `EncoderPool` holds two kinds of encoders:

### Always-on (baseline) encoders

Constructed eagerly at pool creation so any browser can subscribe instantly. The
baseline codec (`BASELINE_CODEC`) is platform-dependent:

- **macOS / Linux**: **VP8** simulcast — up to three layers at full / half /
  quarter resolution (`LayerSpec::vp8_simulcast`, RIDs `f` / `h` / `q`). VP8 is
  the universal codec — Safari, Firefox, Chrome, and Edge all decode it
  reliably, and it has a long track record on screen content. libvpx has no host
  dependency that can be absent, so a construction failure at startup is treated
  as fatal (panic).
- **Windows**: a single full-resolution **H.264** layer via the Media Foundation
  software encoder. VP8/libvpx is gated off Windows (it needs a C toolchain plus
  vpx headers), so H.264 — also universally decodable by WebRTC browsers — is the
  baseline there. H.264 is not simulcast in the pool, so the always-on bank is a
  single layer. A Windows baseline construction failure **degrades** (logs and
  leaves an empty bank, dashboard stays up) rather than panicking, because the
  pool is built eagerly at `--web` startup and a panic would take the whole
  daemon down.

"Always-on" means the encoder *threads* are spawned eagerly; it does **not** mean
every layer emits frames. Which layers actually encode is governed by a
demand-bound and capacity policy. By default:

- a local single-RID viewer demands `f` only → only the full layer emits;
- a federated single-encoding viewer demands `q` only → only the quarter layer
  emits;
- an opt-in multi-RID viewer whose offer carries `a=simulcast:recv f;h;q`
  demands all three — the experimental adaptive-bandwidth path.

A paused VP8 encoder thread blocks in `blocking_recv` at negligible cost; the
active layer costs roughly 5% of a core.

### On-demand encoders

Spawned when the first peer that needs them joins, torn down when the last such
peer leaves. Today: **H.264** (and declared but not-yet-wired VP9 / AV1). These
exist for browsers that prefer or only support a non-VP8 codec. On-demand
encoders are refcounted by viewer count and released deterministically via a
`PoolLease` RAII handle whose `Drop` decrements the refcount synchronously (so it
works from any context, including teardown). A per-slot `generation` token guards
against a stale lease decrementing a replaced slot after a resize.

### Layer policy coordinator

A single coordinator task per display (`display/aggregator.rs`) is the sole owner
of `pool.pause_layer` / `pool.resume_layer` decisions. Three policies **vote**,
and the coordinator composes their votes **by intersection — pause wins; resume
requires every active policy to agree**:

- **Presence policy** — pauses all layers after the display sits at zero peers
  for a 5 s debounce (absorbs browser refreshes and federation reconnect blips),
  resumes on the first peer.
- **Aggregate-TWCC policy** — per-peer cascaded loss. On sustained packet loss it
  pauses the top layer first, then the middle, reversing on recovery. This is the
  actionable signal source on the current `rtc` 0.9 + WKWebView stack.
- **Per-RID RR policy** — per-`(peer, RID)` `fraction_lost` off receiver reports.
  Currently inert (`rtc` 0.9 doesn't populate the RR accumulator) but kept warm
  for future stacks.

This replaced an earlier design that ran each policy as an independent task
writing to the pool, which produced opposite actions when one policy had signal
and another defaulted to "wanted."

### PLI coalescing and keyframe-first

When N viewers request a keyframe (PLI/FIR) at nearly the same time, a naive path
fires N keyframe requests at the encoder — a 2-3× publisher-side bandwidth
amplifier. A `KeyframeCoalescer` dedupes requests per `(codec, rid)` within a
50 ms window. Separately, every per-peer forwarder enforces **keyframe-first**: a
peer that joins mid-stream drops P-frames and requests a keyframe until it has
seen one, so the decoder never renders garbage.

## Per-Peer WebRTC Driver (`rtc`-rs, sans-I/O)

Each browser connection is a `WebRtcPeer` (`display/webrtc.rs`) that runs its own
tokio **driver task**. The driver holds a sans-I/O
[`rtc` crate](https://crates.io/crates/rtc) (rtc-rs, pinned to `=0.9.0`)
`RTCPeerConnection` plus its own UDP/TCP sockets, and pumps everything in a single
`select!` loop:

1. inbound UDP/TCP datagrams → `peer.handle_read(...)`
2. encoded frames from the pool fan-out → packetize → `writer.write(...)`
3. commands from the public handle (ICE candidates, clipboard, authority state,
   shutdown) → data-channel writes

After every input the driver drains the connection's pending writes, reads, and
events, and uses `poll_timeout` / `handle_timeout` to drive timers.

Because `rtc` is **sans-I/O** — it owns no sockets and no clock — the application
owns two responsibilities the library would otherwise hide:

- **Socket pumping.** The driver task is the only code that can write RTP for its
  peer, which is exactly why per-peer codec/layer selection and packetization
  live *inside* the driver rather than in a separate forwarder module (a separate
  task can't reach the driver's RTC state). `display/forward.rs` is now reduced to
  SDP/codec-preference helpers.
- **TWCC tapping.** `rtc` 0.9 consumes inbound TWCC (transport-wide congestion
  control) feedback internally and never surfaces it to the application, and its
  remote-inbound stats accumulator stays at zero. The only place to observe the
  signal without patching `rtc` is inside its interceptor chain.
  `display/twcc_tap.rs` wires a passive `TwccTapInterceptor` as the outermost
  interceptor: it downcasts each inbound RTCP packet, projects a compact event
  onto a channel, and forwards the original unchanged. A health aggregator turns
  that event stream into a 1 s `TwccHealth` snapshot the layer-policy coordinator
  reads. (The driver also derives a recent send-bitrate estimate from
  `bytes_sent` deltas, because `rtc` 0.9's `available_outgoing_bitrate` field is
  never written.)

### Codec negotiation

On offer, the browser's SDP is parsed for supported codecs. The pool subscribes
the peer to *every* codec it can decode and the driver picks one at frame time;
H.264 in particular is matched on its full `fmtp` identity (profile-level-id +
packetization-mode), not just the codec name, because browser negotiation
discriminates parameter sets. Each `EncodedFrame` carries a `PayloadSpec` so the
driver can verify a frame matches the peer-negotiated RTP codec before
packetizing. Different peers can land on different payload types for the same
codec; the driver rewrites the PT, SSRC, sequence numbers, and timestamps per
peer.

### ICE-TCP multiplexing

When a browser can't reach the agent's UDP candidates — typically because the
agent is inside a NAT'd VM with only the dashboard port forwarded — the pipeline
falls back to **ICE-TCP multiplexed onto the same HTTP port** that serves the
dashboard, so no extra port-forward is needed.

A shared `TcpPeerRegistry` is created once at gateway startup. Each peer
pre-generates its ICE ufrag and registers it. The gateway's accept loop peeks the
first bytes of every TCP connection to tell HTTP vs. WebSocket vs. STUN-framed
traffic apart; STUN-framed connections are read one RFC 4571 frame at a time, the
USERNAME's target-ufrag half is extracted, and the connection is handed to the
matching peer's driver.

The advertised TCP candidate's IP is derived from the browser's `Host:` header —
whatever non-loopback IP the browser used to load the dashboard is what the
server advertises:

- **Via a routable IP** (e.g. `http://192.168.1.42:8765`) — ICE-TCP works
  automatically.
- **Via `http://localhost:8765`** — ICE-TCP does **not** work: Firefox and Chrome
  filter remote loopback candidates as an anti-rebinding mitigation. Bind the
  port-forward on all interfaces and access via the host's LAN IP instead.

For VirtualBox NAT users:

```
VBoxManage modifyvm <vm> --natpf1 delete intendant
VBoxManage modifyvm <vm> --natpf1 "intendant,tcp,0.0.0.0,8765,,8765"
```

Then access the dashboard at `http://<host-LAN-IP>:8765`.

That plain-HTTP LAN URL is sufficient for the display transport itself, but not
for browser APIs that require a secure context. Use HTTPS/mTLS, native `--tls`
with a trusted cert, or the macOS app wrapper when the same dashboard session
also needs Station WebGPU, microphone/camera, browser screen capture, or stricter
clipboard APIs. See
[Web Dashboard: Secure Browser Contexts](./web-dashboard.md#secure-browser-contexts).

### TURN relay candidates

On a host with no inbound reachability at all (e.g. a cloud container behind NAT
with no inbound UDP), even srflx candidates don't pair. The server-side `rtc`
peer can allocate **its own** relay on the configured coturn and trickle a
`typ relay` candidate, so media bounces through coturn from both ends. This reuses
`rtc` 0.9's sans-I/O TURN client and is off the setup critical path: the SDP
answer returns with host/srflx/ICE-TCP candidates immediately, and the relay
candidate is trickled later only if the allocation succeeds — an unreachable TURN
server costs zero added latency.

## Tile Streaming (Dirty-Region)

Whole-frame VP8 spends bytes re-encoding the static background even when 90-95% of
the frame is unchanged, which fails the visual-freshness bar at full resolution
regardless of bitrate. Tile streaming inverts the model — **encode only what
changed** — the way VNC/RFB and its derivatives (Spice, RDP, Citrix HDX) have for
decades. The implementation lives in `display/tile/`; the full design rationale
is in [`docs/design-tile-streaming.md`](https://github.com/lovon-spec/intendant/blob/main/docs/design-tile-streaming.md).

### Data channels

Three browser-created data channels ride the **same** `RTCPeerConnection` as the
video track, chosen deliberately rather than "everything reliable":

| Channel | Reliability | Carries |
|---|---|---|
| `tile-control` | reliable, ordered | Subscribe, SnapshotRequest, Resize, EpochAdvance, fallback transitions, CursorState, Error |
| `tile-snapshot` | reliable, ordered | `SnapshotChunk` frames (one atomic state-of-the-world; partial delivery is unrecoverable) |
| `tile-deltas` | unordered, no retransmits | `TileUpdate` frames — each is supersedable per-tile, so a dropped frame just leaves those tiles stale until the next update |

The wire format is a versioned little-endian binary framing. Each on-wire message
is capped at `MAX_DATACHANNEL_MESSAGE_SIZE = 32 KiB` (the conservative SCTP
datachannel ceiling); snapshots and large tile-updates are chunked, and the
browser reassembles by `snapshot_id`. `epoch` changes when the world changes shape
(resize, fallback transition); `seq` is monotonic within an epoch and drives a
per-tile staleness check.

### Tiles, damage, and fallback

- **Tiles** are **64×64 px** (`TILE_STREAM_TILE_SIZE_PX`).
- **Damage** comes from a `DamageBackend`. On **X11**, XDamage
  (`display/capture/x11_damage.rs`) reports real OS-level dirty rects
  (`ReportLevel::BoundingBox`). On non-X11 platforms there is a CPU-bound
  **frame-diff fallback** (`display/capture/frame_diff.rs`) that hashes every tile
  and emits the ones whose hash changed; where neither is available the capability
  reports `None` and the policy forces video mode, so the platform keeps the
  proven VP8 path until its damage backend lands.
- **Tile ↔ video fallback policy** (`display/tile/policy.rs`) switches to
  whole-frame video on high-motion content and back to tiles when motion subsides,
  with hysteresis to prevent flapping: **enter video at 25% dirty fraction
  (`ENTER_VIDEO_THRESHOLD`), exit at 15% (`EXIT_VIDEO_THRESHOLD`)**, over a
  rolling window of 8 samples (`HISTORY_K`), with a **500 ms minimum dwell**
  (`MIN_DWELL`) capping switches at 2/sec. The fallback target is the proven VP8-q
  video track, which stays up the whole time.
- **Cursor** is sent as a separate `CursorState` frame on the control channel and
  drawn as a browser overlay sprite (Path A). X11 typically renders the cursor as
  a hardware overlay that does *not* fire XDamage, so dirtying tiles for cursor
  motion wouldn't work anyway.

### Recovery and backpressure

Recovery is bounded so it can never become the flood it's recovering from. A
periodic snapshot bounds time-to-recover from any silent corruption (30 s in tile
mode, 60 s in video-fallback mode). Browser `GapReport`s are satisfied from a
bounded per-tile replay buffer (`display/tile/recovery.rs`) when the whole missing
sequence range is still present, otherwise a fresh (rate-limited) snapshot is
sent. Tile-delta sends are gated by a backpressure monitor
(`display/tile/backpressure.rs`) watching each channel's buffered amount; the
control channel is never throttled, so input and cursor stay prompt under tile
pressure.

## Per-Platform Backend Matrix

| Platform | Capture | Input injection | Hardware H.264 encode | Clipboard |
|---|---|---|---|---|
| **Linux / X11** | XShm via `x11rb` (`display/x11.rs`), `XGetImage` fallback | `xdotool` | ffmpeg VA-API, `libx264` software fallback (`encode/h264_linux.rs`) | `xclip` |
| **Linux / Wayland** | PipeWire via XDG Desktop Portal `ScreenCast` (`display/wayland.rs`) | XDG Desktop Portal `RemoteDesktop` `notify_*` D-Bus methods (via `ashpd`) | same as X11 | `wl-copy` / `wl-paste` |
| **macOS** | ScreenCaptureKit (`display/macos.rs`) | `cliclick` | VideoToolbox, zero-copy from SCK frames (`encode/h264_macos.rs`) | `pbcopy` / `pbpaste`, `osascript` for images |
| **Windows** | GDI `BitBlt` default, DXGI Desktop Duplication opt-in via `INTENDANT_WINDOWS_CAPTURE=dxgi` (`display/windows.rs`) | Win32 `SendInput` | Media Foundation software MFT, NVENC where available (`encode/h264_windows.rs`) — this is the **baseline** codec on Windows | `arboard` crate (in-process Win32) |

VP8 (`encode/vp8.rs`, libvpx) is the always-on baseline on macOS/Linux and is
gated off Windows entirely. See [Windows Support](./windows-support.md) for the
Windows backends in detail.

`DisplayBackend` auto-detects the active backend at runtime: on Linux it checks
`WAYLAND_DISPLAY` before falling back to `DISPLAY`; macOS and Windows always use
the native backend.

> **Browser input is physical-key-only (Phase 1).** Injected key events use the
> DOM `code` field (physical key position), not `key`. Non-US keyboard layouts
> therefore produce incorrect character output until a future phase adds
> character-level injection. A blur/focus reset of modifier state guards against
> stuck modifier keys.

## Bidirectional Clipboard

The `ClipboardMonitor` (`display/clipboard.rs`) polls the system clipboard every
500 ms and syncs changes over a WebRTC data channel in both directions. It
supports text and images (PNG, capped at 5 MB). On copy in the browser, content is
pushed to the display's clipboard; on copy in the display, content is pushed to
the browser. The per-platform read/write tools are listed in the matrix above.

## Multi-Monitor

Each display gets a stable `display_id` (0 is always the primary). `DisplayInfo`
carries the intendant-stable id plus the native `platform_id` (CGDisplayID on
macOS, X11 screen number, PipeWire `node_id` on Wayland, DXGI output on Windows).
The pipeline supports enumeration, a dashboard display picker, per-display metrics,
dynamic resize (encoders are transparently recreated at the new dimensions via the
pool's layer factory, with a `DisplayResize` event emitted), and hotplug.

On **Wayland**, the portal does not expose true enumeration before a session is
opened, so `enumerate_displays` returns a placeholder; `enumerate_displays_with_sessions`
patches each entry with the live capture resolution from the session registry so
the agent's click coordinates match the screenshot it receives.

## Recording

Display recording runs in parallel with WebRTC streaming via ffmpeg
(`recording.rs`): `x11grab` input on Linux, `avfoundation` on macOS. Recordings
are segmented into MP4 files (default 60 s) for efficient seeking; the dashboard
provides a player with timeline, seeking, and speed control.

```toml
[recording]
enabled = true
framerate = 15
segment_duration_secs = 60
quality = "medium"   # "low" (CRF 35), "medium" (CRF 28), "high" (CRF 20)
```

## Frame Registry

The `FrameRegistry` stores high-quality JPEG frames captured from displays and
browser cameras in the session directory (`<session>/frames/`, metadata in
`frames.jsonl`). Frames serve two purposes: **context for CU models**
(`auto_attach_display_frames()` grabs the latest frame per stream for the agent's
next turn) and **presence inspection** (`inspect_frame` / `inspect_frames` tools).
Each frame carries a `sent_to_live` flag so the browser-side live model never sees
a frame twice.

## WebRTC Configuration

STUN/TURN servers are configured via `[webrtc]` in `intendant.toml` (empty by
default — local-only, no STUN/TURN):

```toml
[webrtc]
federation_allow_h264 = false   # see Peer Federation; VP8-pinned over relays by default

[[webrtc.ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[webrtc.ice_servers]]
urls = ["turn:turn.example.com:3478"]
username = "user"
credential = "pass"
```

`federation_allow_h264` governs only the cross-machine federated path (see
[Peer Federation](./peer-federation.md)); the local same-machine display path is
unaffected.

## Display Metrics

Per-display atomic counters feed the dashboard Stats tab: capture FPS, encode FPS,
**encode freshness** (how stale emitted frames are at output time — not encoder
speed; an idle desktop driven only by the periodic snapshot can show seconds),
capture/encode/peer drop counts, peer count, resolution, and a full set of
tile-stream counters (dirty rects, dirty tiles, delta/snapshot FPS and kbps).
Rates are computed over the elapsed window and counters reset on read.

## Known Limitations

- **Physical-key-only input** breaks non-US keyboard layouts (Phase 1).
- **Tile streaming is X11-first.** Wayland and macOS use the CPU-bound frame-diff
  damage backend (or force video mode where unavailable); only X11 has true
  OS-level damage.
- **Wayland enumeration is portal-limited** — true multi-monitor identity before
  a session opens is not available.
- **`rtc` 0.9 doesn't surface TWCC or populate RR stats**, hence the interceptor
  tap and the `bytes_sent`-delta bitrate estimate; per-RID RR-driven layer policy
  is inert on this stack.
- **No virtual-display equivalent on macOS or Windows** — capture targets the real
  session only.

## See Also

- [Peer Federation](./peer-federation.md) — sharing a *remote* peer's display
  across machines, LAN/mTLS, and the `[webrtc]` federation knobs
- [Windows Support](./windows-support.md) — the Windows capture/input/encode
  backends in detail
- [Computer Use & Live Audio](./computer-use-and-audio.md) — input and voice
