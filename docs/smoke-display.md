# Display smoke recipe

Canonical end-to-end smoke for the display paths:

- **Local DisplaySlot** (§1–5): macOS Mac viewing its own display via
  the in-process WebRTC pipe. H.264 by default (WKWebView's hardware
  VideoToolbox path); single-RID. Run this after any change to
  `src/bin/caller/display/`, `src/bin/caller/web_gateway.rs`'s
  display/authority code, or `static/app.html`'s `DisplaySlot` /
  `pendingAuthorityStates` / `set_on_display_input_authority_change`.
- **Federated path** (§6–§8): browser → host coturn TURN → peer
  daemon → peer's encoder. VP8 single-RID floor (§6); F-1 visibility
  + Take/Release chip (§7); F-2 input wiring + F-3 cross-primary
  arbitration (§8). Run §6 after any change to `src/bin/caller/peer/`,
  `static/app.html`'s `PeerDisplayConnection`, or peer-side encoder
  pool / layer policy. Run §7–§8 after any change to the
  `display_input_authority` / `control` / `pointer` data-channel
  wiring or the federated input authorizer predicate.

**Pass = every signal in §3 (local), §6.2 (federated baseline), §7.2
(federated authority chip), and §8.2 (federated input + handover)
matches verbatim. Anything else is a regression in the implicated
phase.**

## 1. Setup

Macs (the local target this recipe validates):

```sh
# Build + install /Applications/Intendant.app from the worktree
./scripts/bundle-macos.sh

# Clean restart so server-side log starts empty
killall Intendant 2>/dev/null
killall intendant-bin 2>/dev/null
sleep 2
> ~/.intendant/app-backend.log
open -b com.intendant.app
sleep 4

# Verify daemon up
grep -E 'Web TUI:|ICE-TCP candidates advertise' ~/.intendant/app-backend.log
# Expect:
#   [web_gateway] ICE-TCP candidates advertise port 8765
#   Web TUI: http://0.0.0.0:8765
```

Two browsers:

- Browser **A**: the WKWebView inside Intendant.app. Right-click the
  display thumb → Inspect Element to attach Safari Web Inspector.
- Browser **B**: any second browser at `http://localhost:8765/`. Chrome
  works, Safari works.

## 2. Steps

### 2.1 Grant the local display

In Intendant.app's status bar, click **`your display off`** (top of the
window). Toggle flips to `on`.

### 2.2 Capture browser A's bootstrap signals

Right-click the new display thumbnail → **Inspect Element** → **Console**
tab. Run:

```js
JSON.stringify({
  chip: document.getElementById('ds-authority-0').outerHTML,
  codecs: Array.from(document.querySelectorAll('#display-canvas-0 video')).map(v => ({
    hasStream: !!v.srcObject, paused: v.paused, vw: v.videoWidth, vh: v.videoHeight
  })),
})
```

### 2.3 Take + Release in browser A only

```js
document.getElementById('ds-take-0').click();
// chip should flip to "Input: you"
document.getElementById('ds-release-0').click();
// chip should flip back to "Input: shared"
```

### 2.4 Two-browser handoff

Open browser B at `http://localhost:8765/`. In B's console:

```js
// Bootstrap chip — should reflect A's current authority
JSON.stringify({chip: document.getElementById('ds-authority-0')?.outerHTML});
```

Then in B:

```js
document.getElementById('ds-take-0').click();
```

Re-query A's chip — should flip to `other`. Reverse: A takes, then check
B's chip flips to `other`.

### 2.5 Holder closes

While B holds (`Input: you` in B), close B's tab. A's chip should snap
back to `unclaimed` / `Input: shared` within ~1s of WS close.

### 2.6 Cleanup

```sh
killall Intendant 2>/dev/null
killall intendant-bin 2>/dev/null
```

## 3. Expected signals

### 3.1 Browser console (DevTools)

After WS connect (logged once per `DisplaySlot` lifetime). Exact
codec depends on browser default order — WKWebView prefers H.264,
Chrome prefers VP8:

```
[DisplaySlot 0] offer first codec: <H264 or VP8>/90000
[DisplaySlot 0] answer negotiated codec: <same>/90000; (no a=simulcast)
```

**#58**: local DisplaySlot is single-RID (`a=simulcast:recv f` only),
so the answer is plain sendonly with no `a=simulcast:send` directive.
WKWebView gets hardware-accelerated H.264 via VideoToolbox; Chrome
gets one software VP8 encoder. Multi-encoding simulcast (`f;h;q`) is
NOT expected on this path post-#58 — that signature would indicate a
regression to the pre-#58 unconditional 3-encoder demand.

### 3.2 Authority chip (browser A and browser B)

| State | `class` | `textContent` |
|---|---|---|
| Bootstrap, no holder | `display-input-authority unclaimed` | `Input: shared` |
| This browser holds | `display-input-authority you` | `Input: you` |
| Other browser holds | `display-input-authority other` | `Input: another viewer` |

Stuck-at-empty (`class="display-input-authority"`, hidden via
`display:none`) → 5c.2 bootstrap regression. Compare to
`web_gateway::compute_bootstrap_authority_snapshots` and the deferred
post-`log_replay` send.

### 3.3 Server-side activity log (Intendant.app's Log tab)

```
User display access granted (display_id: 0)
Display :0 ready
Display :0 in use      ← on Take
Display :0 released    ← on Release / WS close from holder
```

The lifecycle markers (`in use` / `released`) come from the legacy
`AppEvent::DisplayTaken` / `DisplayReleased` path. They must NOT fire
on transient disconnects (`disconnect({userInitiated:false})` from
capture_lost or ICE retry) — phase 5c spec.

### 3.4 Server-side metrics log (`~/.intendant/app-backend.log`)

```
grep '\[display/metrics\]' ~/.intendant/app-backend.log | tail -5
```

Expect:

```
[display/metrics] id=0 capture=N.Nfps encode≈capture drops=cap:0/enc:0/peer:0 peers=2 latency_avg=~20ms res=WxH
```

- `peers=2` while both browsers viewing; `peers=1` after one closes.
- `encode ≈ capture` — single-RID single-encoding (post-#58 default).
  An integer factor (e.g. `encode ≈ 3 × capture`) on this path is a
  regression to the multi-encoding default, OR — if intentional — the
  signature of an opt-in multi-RID configuration that should be called
  out explicitly by the change adopting it.
- `drops=peer:0` is the only drop counter that matters here (`enc:N`
  during burst is fine; `peer:N` is back-pressure).
- `latency_avg ≤ 50ms` for an idle desktop on macOS / VideoToolbox.

## 4. Out of scope

- **Federated display** (peer-to-peer): separate operational path
  with its own smoke coverage.
- **Loss shaping / TWCC capacity adaptation**: covered by separate
  baseline tasks #50 (4d.3a) and #48 (4d.3b). Re-running those needs
  netem on Linux or Network Link Conditioner on macOS, deliberately
  excluded from this smoke to keep it fast.
- **Authority across federation**: deny-by-default per
  `build_federated_input_authorizer() → || false`. Federated authority
  is a future slice on top of phase 5c local authority.

## 5. Failure triage cheatsheet

| Symptom | Most likely culprit |
|---|---|
| Browser B's chip stuck `unknown` (hidden) on connect | `display_input_authority_state` sent before `log_replay` recreates the slot — see `web_gateway.rs` deferred-snapshot logic |
| Chip flips to `you` but no `Display :0 in use` log | `_enterInteractive` not invoked — check `setAuthority('you')` promotion when `_takeControlPending` |
| Chip stays `you` after another browser takes | `setAuthority` not handling `'you' → 'other'` exit-interactive transition |
| `Display :0 released` fires on transient disconnect | `disconnect()` invoked with `userInitiated:true` from a non-user-close path — check capture_lost / ICE retry handlers |
| `a=simulcast:send` missing from answer | Per-peer Rtc codec negotiation broken (phase 2 / 3), or rtc 0.9 SDP-sanitizer regression |
| `peers=N` doesn't match browser count | Encoder lifecycle leak — see `display/encode/pool.rs` refcounting |

## 6. Federated path baseline

Distinct from §1–5 above (local DisplaySlot). The federated path —
browser → host coturn TURN → peer daemon → peer's encoder — is the
operational target for cross-host display viewing. Baseline as of
#67 / #70:

**Federated baseline = VP8 single-RID floor over TURN relay.**

- **Codec**: VP8, pinned in `PeerDisplayConnection.connect()`
  via `setCodecPreferences` (#67). Distinct from local DisplaySlot's
  H.264 default (#58); federation has no hardware-accel argument (peer
  is libx264 software anyway) and the H.264 path is unusable in the
  reference smoke topology — see "H.264 status" below.
- **Layer**: `q` only. Peer's layer-policy starts with all three layers
  paused (`PauseLayer(f)`, `PauseLayer(h)`, `PauseLayer(q)`) and
  resumes only `q` after Connected. Browser SIZE = 1/4 native capture
  (e.g. 224×150 from 896×600, or 340×192 from 1360×768).
- **Wire**: TURN UDP relay forced via `iceTransportPolicy=relay` when
  `[webrtc].ice_servers` lists a turn:/turns: URL (#45). rtc 0.9 does
  not drive DTLS over ICE-TCP (#41–#44), so direct paths stall at
  `dtlsState=connecting`; relay is the verified-working path.
- **SDP**: single PT (107 = VP8/90000), no `a=simulcast:send`, no
  `a=rid:`, single SSRC, single `cname:display-<sessionhash>` (the
  20-digit federated form, distinct from local DisplaySlot's
  autoincrement `display-N` form).

### 6.1 Federated smoke recipe

Mac primary + Debian/X11 peer over the existing SSH tunnel topology:

```sh
# Peer (SIGTERM, never -9 — SIGKILL skips X11 SHM detach)
ssh -J user@<jump> vm@<peer> 'pkill -TERM -f target/release/intendant; sleep 3'
ssh -J user@<jump> vm@<peer> 'cd /home/vm/projects/intendant && setsid -f env DISPLAY=:0 \
  ./target/release/intendant --web --no-tui --no-presence </dev/null >/tmp/intendant.out 2>&1'

# Mac primary
killall Intendant 2>/dev/null; killall intendant-bin 2>/dev/null; sleep 2
> ~/.intendant/app-backend.log
open -b com.intendant.app

# Verify peer reachable + tunnel listening on a non-loopback addr
# (browsers silently drop remote loopback ICE candidates)
lsof -nP -iTCP:<tunnel-port> -sTCP:LISTEN

# Re-add peer with browser_tcp_via_url set to a NON-loopback URL the
# browser's machine can dial (Mac LAN IP, not 127.0.0.1)
curl -s -X POST -H 'Content-Type: application/json' http://127.0.0.1:8765/api/peers -d '{
  "card_url":"http://<mac-lan-ip>:<tunnel-port>/.well-known/agent-card.json",
  "via_urls":["ws://<mac-lan-ip>:<tunnel-port>/ws"],
  "browser_tcp_via_url":"ws://<mac-lan-ip>:<tunnel-port>/ws"
}'

# Persistent grant (script must NOT exit — keeps capture alive)
( python3 -c '
import asyncio, json, websockets
async def main():
    async with websockets.connect("ws://<mac-lan-ip>:<tunnel-port>/ws") as ws:
        await ws.send(json.dumps({"action": "grant_user_display"}))
        try:
            while True: await asyncio.wait_for(ws.recv(), timeout=900)
        except: pass
asyncio.run(main())
' >/tmp/grant.log 2>&1 ) & disown
```

In Intendant.app: Settings → Network → expand peer row → click
**View display**.

### 6.2 Federated expected signals

Browser-side (Web Inspector → Console — install monkey-patch BEFORE
clicking View display so SDP gets captured):

```js
window.__cap = {pcs:[], offers:[], answers:[]};
const __O = window.RTCPeerConnection;
window.RTCPeerConnection = function(...a){
  const pc = new __O(...a); window.__cap.pcs.push(pc);
  const co = pc.createOffer.bind(pc);
  pc.createOffer = async function(...x){ const o = await co(...x); window.__cap.offers.push(o.sdp); return o; };
  const sr = pc.setRemoteDescription.bind(pc);
  pc.setRemoteDescription = async function(d){ if (d?.type === 'answer') window.__cap.answers.push(d.sdp); return sr(d); };
  return pc;
};
window.RTCPeerConnection.prototype = __O.prototype;
```

After 30s of streaming, expected shape:

| Signal | Expected |
|---|---|
| `window.__cap.offers[0]` m=video | `m=video 9 UDP/TLS/RTP/SAVPF 107` (VP8 only, no H.264 PTs) |
| `window.__cap.answers[0]` | `a=rtpmap:107 VP8/90000`, no `a=simulcast:send`, no `a=rid:`, single `a=ssrc:<id>` |
| `pc.iceConnectionState` | `connected` |
| inbound-rtp[0].framesDecoded | advancing every getStats poll |
| inbound-rtp[0].keyFramesDecoded | > 0 |
| inbound-rtp[0].frameWidth | = 1/4 of peer's display res |
| inbound-rtp[0].pliCount | ~0 (small VP8 IDRs survive without retransmits) |
| inbound-rtp[0].nackCount | ~0 |
| video.videoWidth | matches inbound-rtp.frameWidth, readyState=4 |

Peer-side (`/tmp/intendant.out`):

```
[display/x11] XShm available, using shared memory capture <W>x<H>
[layer-policy] PauseLayer(SimulcastRid("f"))
[layer-policy] PauseLayer(SimulcastRid("h"))
[layer-policy] PauseLayer(SimulcastRid("q"))
[display/webrtc] peer <session-hash-id>: ICE-TCP enabled on <ip>:<port>
[display/webrtc] connection: Connected
[layer-policy] ResumeLayer(SimulcastRid("q"))
[twcc-health] reported=N received=N lost=0 loss_fraction=0.0000 ...
```

`PauseLayer(f|h|q)` then `ResumeLayer(q)` only is the load-bearing
signal — multi-layer simulcast would show `ResumeLayer(f)` and/or
`ResumeLayer(h)` too. **No `[encoder/pool] h264:*` lines in the
session** — H.264 should not spawn for federation; if it does see #71.

### 6.3 H.264 status on federation (do not enable)

H.264 over the federated path is currently broken end-to-end in the
reference smoke topology. Diagnosed in #65 + #67:

- Topology: browser → host coturn at `192.168.1.223:3478` → Debian
  UTM peer at `192.168.65.2:8765`, all on one MacBook. Per-packet
  loss measured at 13–22% on this purely-local path (anomalous;
  pending investigation in #69 — likely virtio-net config / coturn
  buffer / MTU).
- libx264 IDR sizes at 1360×768 → ~349 KB ≈ 291 RTP packets.
- P(complete IDR delivery) at 13–22% loss = (0.78)^291 ≈ 1.5e-30.
  Effectively impossible to reassemble even one IDR.
- Result: browser sees packets flowing (e.g. 80 MB / 30 s) but
  framesDecoded stays at 0 indefinitely; PLI storm (~30/sec) as the
  decoder begs for keyframes that can never complete.

The Bug-B SPS/PPS guarantee (`63facd5`) remains correct H.264 hygiene
for paths that DO negotiate H.264 (macOS-to-macOS local DisplaySlot)
but does not by itself unblock federation. VP8 single-RID floor (this
section) is the working federated baseline until either (a) the local
TURN/virtio loss is fixed at the network layer (#69) or (b) the H.264
encoder produces small enough IDRs to survive the loss (#69 mitigation).

## 7. Federated input authority chip — F-1.3c smoke

**Goal.** Verify the browser-side authority chip + Take/Release
buttons render and round-trip through the peer's authority registry.
F-1 is visibility-only — input is still deny-by-default
(`build_federated_input_authorizer() -> false`), so this smoke does
NOT exercise actual mouse/keyboard injection.

**Prereq.** §6 federated path is up (peer added with non-loopback
`browser_tcp_via_url`, display granted, video stream rendering in
the peer-display panel).

### 7.1 Steps

1. With the federated peer-display panel open and connected:
   verify the chip in the panel's controls row resolves from
   hidden (the `unknown` initial state) to grey **"Input: shared"**
   within ~1s of `ontrack` firing. The transition is driven by the
   peer's per-subscriber fanout sending the initial personalized
   snapshot via `Command::SendAuthorityState` — F-1.2's pending
   queue absorbs the case where the snapshot arrives before the
   `display_input_authority` data channel finishes negotiating.
2. Click **Take Control** in the controls row.
3. Verify within ~1s: chip flips to green **"Input: you"**, button
   row swaps to **Release** (Take Control hidden).
4. Click **Release**.
5. Verify within ~1s: chip flips back to grey **"Input: shared"**,
   button row swaps back to **Take Control** (Release hidden).

### 7.2 Expected signals

In the browser console (`Develop → ... → Inspect`), filter for
`webrtc-peer`:

- `authority data channel open` — the `display_input_authority`
  data channel negotiated successfully. Without this, the chip
  stays at `unknown` and the buttons are non-functional.
- `sent display_input_authority_request (display_id=N)` — emitted
  by `_sendAuthorityFrame` on the Take Control click.
- `authority granted: you` — emitted by `setPeerAuthorityState`
  when the peer's broadcast confirms the grant.
- `sent display_input_authority_release (display_id=N)` — emitted
  on the Release click.

A second federated browser (or a second tab from the same primary)
attached to the same peer-display will see the chip transition to
yellow **"Input: another viewer"** when this browser holds. F-1's
fanout broadcasts personalized state to every subscribed peer.

### 7.3 Failure modes

- **Chip stuck at hidden / `unknown`.** The data channel didn't
  open. Most likely the peer is on an old build that lacks F-1
  (`apply_grant_input_authority_federated` etc.); check
  `git_sha` on the peer-row badge in Settings → Network.
- **Click on Take Control does nothing — chip and button stay.**
  Check `_sendAuthorityFrame` warn in console (`dropped — authority
  channel not open`). If channel is `connecting`, the snapshot
  would also be queued — wait for the chip to resolve to "Input:
  shared" first, then click. Click handler is bound at
  `attachToDom` time on the elements present at that moment; if a
  daemons-list re-render destroyed and rebuilt the panel,
  `attachToDom` re-binds, so the second-or-later panel build still
  works.
- **Chip flips to "you" briefly then snaps back to "shared".** A
  second federated subscriber on the same peer raced and took
  authority. Last-writer-wins. Open both browsers' consoles to
  see who clicked first.

### 7.4 Out of scope (handled in §8)

- Actual keyboard / mouse input on the federated path — §8.1.1.
- Close-while-holding teardown — §8.1.2.
- Cross-primary handover (two Intendant.app instances) — §8.1.3.
- Clipboard sync over federated path: separate channel, not yet
  wired for federation.

## 8. Federated input + multi-viewer arbitration — F-2 / F-3 smoke

**Goal.** Verify (1) browser → peer real input injection over the
federated `control` / `pointer` data channels; (2) close-while-holding
cleans up authority + listeners; (3) cross-primary handover arbitrates
last-writer-wins across two distinct Intendant.app instances viewing
the same peer.

**Prereq.** §6 federated path up (peer added with non-loopback
`browser_tcp_via_url`, display granted, video stream rendering). §7
chip + Take/Release smoke passing — F-2 inherits that wiring and only
flips the input authorizer's predicate.

### 8.1 Steps

#### 8.1.1 Single viewer, real input (F-2 acceptance items 1-4)

1. With the panel up and chip resolved to `Input: shared`, click
   **Take Control**. Chip → `Input: you`. Console emits
   `entered interactive mode — input listeners installed`.
2. Move the mouse into the video area. The peer's X11 cursor mirrors
   motion (visible directly on the X11 guest, or via SSH'd `xdotool
   getmouselocation`). Each motion sends an `mm` frame on the
   `pointer` channel.
3. Click on a visible xterm inside the stream — its X11 focus follows.
4. Type letters / digits into the xterm via keyboard. Characters
   appear in the xterm, one per `kd` / `ku` pair on the `control`
   channel. Modifier-bearing chords (Cmd+letter, Ctrl+letter) require
   the dashboard's `_heldModifiers` set, exhaustively populated for
   modifier keys (see §8.5 for the regular-key gap → #79).
5. Click **Release**. Chip → `Input: shared`. Console emits
   `exited interactive mode — input listeners removed`. Subsequent
   browser input is silently dropped at the peer's predicate.

#### 8.1.2 Close-while-holding (F-2 acceptance item 5)

1. Take Control again (chip → `you`).
2. Close the browser tab / `cmd+W` the panel / quit the Intendant.app
   instance — any of the three tears the WebRtcPeer down.
3. The peer's authority registry release fires on WebRtcPeer drop
   (`by_display.remove(&display_id)`), not on data-channel close.
   Within ~1s, any other viewer's chip flips to `Input: shared`.
4. No leftover `xdotool` invocations from the closed browser's path
   (modulo §8.5's stuck-key edge for held regular keys → #79).

#### 8.1.3 Two primaries / cross-primary handover (F-3 acceptance items 6-7)

1. Launch a second Intendant.app: `open -n -a Intendant`. Auto-discovery
   picks a free `--web` port (e.g. 57014; do NOT pin to the historical
   49575 — auto-discover reuses whatever's free). Verify both alive:
   `pgrep -fa intendant-bin` shows two `--web N` lines.
2. Add the same peer to the second primary via curl, OR via its
   dashboard's Add Peer form. Same `card_url` / `via_urls` /
   `browser_tcp_via_url` as the first. Persistent grant (§6.1) keeps
   capture alive for both subscribers — `peers=2` in
   `[display/metrics]` confirms both attached.

   ```sh
   curl -s -X POST -H 'Content-Type: application/json' \
     http://127.0.0.1:<port2>/api/peers -d '{
       "card_url":"http://<mac-lan-ip>:<tunnel-port>/.well-known/agent-card.json",
       "via_urls":["ws://<mac-lan-ip>:<tunnel-port>/ws"],
       "browser_tcp_via_url":"ws://<mac-lan-ip>:<tunnel-port>/ws"
     }'
   ```
3. In each primary: Settings → Network → expand peer row → click
   **View display**.
4. With first primary holding, second primary's chip = `Input:
   another viewer`, **Take Control** button visible.
5. From second primary's Web Inspector console:
   `document.querySelector('.take-control-btn[data-host-id="intendant:host"]').click()`.
   Console emits `sent display_input_authority_request (display_id=0)`
   → `authority granted: you` → `entered interactive mode — input
   listeners installed`. Chip flips to `Input: you`.
6. First primary's console emits `exited interactive mode — input
   listeners removed`. Chip flips to `Input: another viewer`.
7. From first primary: click Take Control. First primary returns to
   `Input: you`; second primary demotes to `Input: another viewer`.
   Bidirectional arbitration confirmed.

### 8.2 Expected signals (browser console, per peer-display panel)

Filter `webrtc-peer`:

| Event | Console line(s), in order |
|---|---|
| All 3 channels open | `authority data channel open` → `control data channel open` → `pointer data channel open` |
| Take Control | `sent display_input_authority_request (display_id=N)` → `authority granted: you` → `entered interactive mode — input listeners installed` |
| Release | `sent display_input_authority_release (display_id=N)` → `authority released` → `exited interactive mode — input listeners removed` |
| Demoted by another viewer | `authority changed: other` → `exited interactive mode — input listeners removed` |
| Promoted from other | `authority granted: you` → `entered interactive mode — input listeners installed` |

Chip class + textContent vocabulary (matches §3.2 / §7):

| Class | Text |
|---|---|
| `display-input-authority you` | `Input: you` |
| `display-input-authority other` | `Input: another viewer` |
| `display-input-authority unclaimed` | `Input: shared` |

### 8.3 Two-Intendant.app harness setup

Both `Intendant.app` instances share `com.intendant.app` bundle ID. Implications when driving them:

- `cmd+\`` cycles only within one process — the second primary cannot
  be raised from the first that way.
- `open -n -a Intendant` spawns a fresh process; the new daemon
  auto-discovers a free `--web` port. Find each via
  `lsof -iTCP -sTCP:LISTEN -P -n | grep intendant-bin` or
  `pgrep -fa intendant-bin`.
- To switch between the two main windows: Mission Control (Ctrl+Up),
  Dock right-click → window list, or open both Web Inspectors and
  address them by their distinct titles ("Web Inspector — backend"
  for both, but each one's coordinates differ).
- Tunnel: same `ssh -L *:18765:<peer>:8765 -o GatewayPorts=yes`
  serves both primaries — peer's URL uses the Mac's LAN IP, not
  loopback, so any browser on the host can reach it.

### 8.4 Synthetic-event caveats (test-driver artifacts only)

When driving from a CDP-style harness (Playwright,
`mcp__claude-in-chrome`, etc.):

- WKWebView silently rejects `setPointerCapture` from synthetic
  PointerEvents. Real OS-level `mousedown` / `mouseup` works fine,
  as does Web Inspector console JS. Inject native clicks via the
  driver's primitives, or call into the dashboard's already-bound
  handlers via console expressions — do NOT dispatch synthetic
  PointerEvents from outside the page.
- Synthetic click y-coordinates inside Intendant.app's WKWebView
  carry a `-142px` offset relative to the screenshot frame
  (titlebar + status bar geometry). Documented in
  `project_macos_smoke_quirks.md`.

### 8.5 Failure modes / known caveats

- **Stuck X11 auto-repeat after closing tab while a key was held.**
  Closing the panel mid-keypress can drop the `keyup`. X11 then
  auto-repeats the held key indefinitely. Recovery from peer:
  `ssh ... 'DISPLAY=:0 xdotool keyup <keyname>'`. Root cause:
  browser's `_heldModifiers` set tracks only modifiers, not regular
  keys, so a dropped `ku` for a letter never gets a synthetic
  release-on-close. Tracked as #79; mirror local DisplaySlot's
  exhaustive `_held` set + emit `release-all` on tab-close.
- **Take Control fires but chip doesn't flip.** Console warn
  `dropped — authority channel not open`. Wait for §7.2 channel-open
  signals (chip resolves to `Input: shared`) before clicking.
- **Take Control click missed entirely (no console line).** The
  click handler is bound at `attachToDom` time; if a daemons-list
  re-render rebuilt the panel WHILE this connection held authority,
  F-2 follow-up at `5f6a1dd` (commit) re-binds — but if you're on a
  pre-`5f6a1dd` build, `attachToDom`'s old `interactive=true` guard
  could leave listeners detached. Verify worktree HEAD includes
  `5f6a1dd` before triaging.
- **One primary's panel stays black while authority arbitrates
  fine.** Black means a separate WebRTC issue (codec / ICE), not an
  authority bug — chip + button transitions still arbitrate
  correctly. Re-View display to reset that primary's
  RTCPeerConnection if stuck. If `[encoder/pool] h264:*` lines
  appear in `/tmp/intendant.out` during federation, see #71 (stale
  H.264 spawn).
- **`peers=N` in `[display/metrics]` doesn't match the number of
  open primaries.** Confirms #61 / encoder lifecycle leak signature.
  Re-check via `pgrep -fa intendant-bin` and the peer's
  `/tmp/intendant.out` for capture-thread duplication.

### 8.6 Out of scope (handled elsewhere)

- Per-RID layer policy + TWCC capacity adaptation: §6 baseline +
  #48 / #51.
- H.264 federation (broken in reference topology): §6.3 + #69.
- Stuck-key proper fix: #79.
- Federated clipboard sync: separate channel, not in F-2 / F-3
  scope.
