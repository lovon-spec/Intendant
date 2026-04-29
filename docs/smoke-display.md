# Local DisplaySlot smoke recipe

Canonical end-to-end smoke for the local display path: capture → VP8
or H.264 (single-RID by default) → WebRTC → browser, plus the
per-display input-authority chip. Run this after any change to
`src/bin/caller/display/`, `src/bin/caller/web_gateway.rs`'s
display/authority code, or `static/app.html`'s `DisplaySlot` /
`pendingAuthorityStates` / `set_on_display_input_authority_change`.

This is the local path only. Federated display (peer-to-peer over
`PeerOp::WebRtcSignal`) is out of scope here — it has its own
operational path with separate smoke coverage.

**Pass = every signal in §3 matches verbatim. Anything else is a
regression in 5a.1 / 5c / 5c.1 / 5c.2.**

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
