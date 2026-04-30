# Federated input authority — design

## Goal

Make federated peer displays controllable from the federated browser
with the same authority semantics as local displays — replacing the
current `build_federated_input_authorizer() → || false` deny-by-default
closure. Input authority arbitration on the peer is unified across
local WS holders and federated WebRTC holders, with explicit
provenance and no shape-based inference.

## Architectural principles

- **Peer remains the source of truth** for who holds each of its
  displays. Input ultimately injects there; authority lives there.
- **One shared authority broker/registry on the peer** handles both
  local WS holders (existing 5a/5c) and federated WebRTC holders
  (new). Same arbitration rules across both provenance kinds.
- **Local 5a/5c keeps its existing WS-based control path unchanged.**
  Local browser still sends `request_display_input_authority` /
  `release_display_input_authority` over the gateway WebSocket and
  receives `display_input_authority_state` over the same.
- **Federated path adds new WebRTC data-channel messages** for
  authority — distinct from local's WS path because federated browsers
  have no WS to the peer, only the WebRTC connection from
  `PeerDisplayConnection`. This is a NEW protocol surface, not a
  duplicate of any existing one.
- **Federated input reuses the existing `control` / `pointer`
  channels** with raw `InputEvent` JSON. The peer's existing
  `display/webrtc.rs::handle_message` parser dispatches on
  `control | pointer` as `InputEvent` and calls an `input_handler`
  closure; that closure's predicate is what changes from
  deny-by-default to a registry lookup. No new `display_input`
  channel, no new wire format for input.
- **Browser-side guard is UX only; peer-side gate is the security
  boundary.** Federated input that arrives without authority is
  silently dropped at the peer's input handler regardless of what the
  browser thinks.
- **Primary stays signaling relay only.** No PeerOp authority broker
  in this slice. Authority + input flow direct browser↔peer over the
  WebRTC data channels.

## Why federated authority can't ride local's WS path

Local 5a/5c assumes the browser has a WebSocket directly to the
gateway whose display it wants to control. A federated browser's
WebSocket is to its OWN primary's gateway — not to the peer's gateway.
Routing federated authority through the primary's WS would require
either:

1. Primary becoming an authority broker (mediates request → forwards
   to peer → relays state back) — adds latency, splits source of truth.
2. Federated browser opening a second WS directly to the peer —
   duplicates connection, complicates auth.

The WebRTC connection between the federated browser and the peer
(relayed via TURN UDP) is already present, low-latency, authenticated
by ICE/DTLS, and is the only direct browser↔peer transport. Putting
federated authority there keeps the primary as a pure signaling relay
and gives the peer first-class identity for the requesting browser.

## Wire — federated authority data-channel messages

Sent over a new dedicated authority data channel on the federated
`PeerDisplayConnection` (created at `connect()` time alongside the
existing recvonly video transceiver and the new `control` / `pointer`
input channels added in F-2).

Channel name: `display_input_authority`.

**Browser → peer:**

```json
{ "t": "display_input_authority_request", "display_id": 0 }
{ "t": "display_input_authority_release", "display_id": 0 }
```

**Peer → browser** (personalized per recipient — each federated
WebRtcPeer's authority data channel sees its OWN view of "you" vs
"other"):

```json
{ "t": "display_input_authority_state", "display_id": 0, "state": "you" | "other" | "unclaimed" }
```

State strings deliberately match the local 5c protocol so the chip
rendering code on the federated peer-display panel reuses the same CSS
classes (`display-input-authority you|other|unclaimed`) and the same
state-machine logic. Wire-message shape is new (different message-type
prefix `display_input_authority_*`); the state vocabulary is
identical.

The authority data channel is **separate from the input data channels
(`control` / `pointer`) added in F-2**. Authority messages are parsed
on the authority channel; input events on the input channels. No
overloading.

## Wire — federated input events (F-2, reuses existing `control` / `pointer` shape)

Federated input events flow on `control` and `pointer` data channels
on the federated `PeerDisplayConnection`. The wire format is the
existing raw `InputEvent` JSON used by local DisplaySlot today —
parsed by `display/webrtc.rs::handle_message` as `InputEvent`, e.g.:

```json
{ "t": "kd", ... }     // key-down
{ "t": "mm", ... }     // mouse-move
{ "t": "md", ... }     // mouse-down
```

(See existing `InputEvent` serialization for the full vocabulary.)

The peer's existing `input_handler` closure — currently deny-by-default
for federated peers — becomes a registry-lookup predicate (see "Input
gate" below). No new channel name, no new message type, no wrapper
envelope.

## State on each side

### Peer (single source of truth)

The existing per-display authority registry's `holder` field type
changes from a flat `Option<connection_id: String>` to an explicit
holder-identity enum with provenance:

```rust
enum DisplayInputHolder {
    LocalWs { connection_id: String },
    FederatedWebRtc { peer_id: PeerId, session_id: String },
}

struct DisplayInputAuthority {
    by_display: RwLock<HashMap<u32, DisplayInputHolder>>,
}
```

- `LocalWs::connection_id` — the WebSocket connection id from the
  local gateway, exactly what the existing 5a/5c registry stores
  today.
- `FederatedWebRtc::peer_id` — the federation `PeerId` of the
  requesting primary (e.g. `intendant:nicks-mac`). Provides
  multi-primary scoping for future per-primary policies.
- `FederatedWebRtc::session_id` — the federated `PeerDisplayConnection`
  session id, which uniquely identifies one browser tab's view
  (multiple tabs from the same primary get distinct session ids).

Combined `(peer_id, session_id)` uniquely identifies one federated
browser holder. **Provenance is explicit, not inferred from string
shape.**

**Absence = unclaimed.** The map does NOT carry an `Option`. A
display with no entry in `by_display` is unclaimed; release is
implemented as `by_display.remove(&display_id)`. This matches the
existing 5a/5c semantics and keeps cleanup logic simple — there's no
second representation of "nobody holds it."

The arbitration rule (last-taker-wins, demote prior holder via state
update) is provenance-agnostic and matches existing 5a/5c.

### Peer-side input gate (F-2)

The federated input handler — same code path that currently calls
`build_federated_input_authorizer() → || false` — instead consults the
registry:

> Holder for `display_id` is `Some(DisplayInputHolder::FederatedWebRtc { peer_id, session_id })`
> matching this WebRtcPeer's identity?

Match → inject. No match → drop silently. Same shape as the local path
checks `LocalWs::connection_id`.

### Federated browser

`PeerDisplayConnection` adds a `peerAuthorityState` field per
peer-display, set from inbound `display_input_authority_state` messages
on the authority data channel. Drives the chip + Take/Release buttons
on the peer-display panel.

A `_takeControlPending` flag mirrors the local `DisplaySlot` pattern —
set on Take Control click, cleared when the peer's
`display_input_authority_state` reflects "you" (or some bounded
timeout).

### Primary

Unchanged. Signals WebRTC, doesn't touch authority. Doesn't proxy any
authority messages.

## Server→browser send path for authority state

Mirrors the existing `Command::SendClipboard` pattern in
`display/webrtc.rs`:

- New driver command: `Command::SendAuthorityState { display_id, state }`.
- Public method on `WebRtcPeer`: `send_authority_state(display_id, state) -> Result<bool>`,
  same shape as `send_clipboard()`.
- Driver writes JSON to the `display_input_authority` data channel
  when the channel is open.
- **Initial snapshot on channel open**: when the authority data
  channel transitions to `open`, the driver immediately sends the
  current per-display state for every display this WebRtcPeer is
  subscribed to. Without this bootstrap, a federated browser that
  opens a peer-display panel could miss the initial state and stall
  the chip at "unknown" indefinitely. Local 5c has the same bootstrap
  problem and handles it via a deferred-snapshot send after `log_replay`;
  the federated path uses channel-open as the bootstrap edge.

The authority registry's broadcast-to-subscribers code (today
iterates local WS subscribers for a display) extends to also iterate
federated WebRtcPeers subscribed to that display, calling
`peer.send_authority_state(display_id, state)` on each one's
authority data channel. State personalization: each WebRtcPeer's
broadcast computes `state` from "is this peer's `(peer_id, session_id)`
the current holder?" → `you` if yes, `other` if someone else holds,
`unclaimed` if nobody holds.

## Composition with local 5a/5c

Zero behavioral changes to local 5a/5c. The peer's authority registry's
arbitration treats `LocalWs` and `FederatedWebRtc` holders identically:

- **Local browser holds, federated browser takes** → peer's
  last-taker-wins fires, prior `LocalWs` holder gets
  `display_input_authority_state: "other"` over its existing WS, new
  `FederatedWebRtc` holder gets `display_input_authority_state: "you"`
  over the new authority data channel.
- **Federated holds, local takes** → symmetric.
- **Federated holder's WebRtcPeer disconnects** → existing
  peer-disconnect cleanup path (`remove_peer`) fires; registry
  releases authority via `by_display.remove(&display_id)`; `state:
  "unclaimed"` broadcast to all remaining viewers across both
  transports. **WebRtcPeer drop is the authoritative release edge —
  data-channel close is not.** Match local semantics where
  WS-disconnect, not control-channel close, is the release trigger.
- **Two federated browsers from same primary, different tabs** →
  distinct session ids, only one holds at a time.
- **Two federated browsers from different primaries** → peer doesn't
  conceptualize "primary" — sees two distinct `FederatedWebRtc`
  holders with different `peer_id` values. Same arbitration.

## Browser UX on the federated peer-display panel

Mirror the local DisplaySlot panel structure, scoped to the
peer-display container:

- **Authority chip** rendering: identical CSS classes
  (`display-input-authority unclaimed | you | other`), identical
  state-string mapping. **"Input: another viewer" for any non-you
  state**, regardless of whether the holder is local or federated.
  Privacy-preserving for slice F-1; can be expanded later. Chip is
  always rendered once the peer-display panel exists; it shows
  `unknown` (hidden) before the first `display_input_authority_state`
  arrives, then shows `unclaimed` ("Input: shared") after the
  channel-open initial snapshot.
- **Take Control / Release** buttons, identical UX semantics.
- **Cursor / keyboard capture** when chip = "you" — same
  `_enterInteractive` pattern as DisplaySlot, captures pointer + key
  events on the video element, serializes to raw `InputEvent` JSON,
  sends on the existing `control` / `pointer` data channels (F-2).

The internal `holder_kind` distinction is carried in the registry but
not surfaced in the UI for slice F-1.

## Preventing unauthorized data-channel input on the peer

Replace `build_federated_input_authorizer() → || false` with a real
predicate that consults the registry. The federated input handler
knows its requesting WebRtcPeer's `(peer_id, session_id)` and asks
the registry:

> `by_display.get(&display_id) == Some(DisplayInputHolder::FederatedWebRtc { peer_id: req_peer_id, session_id: req_session_id })`?

Match → inject. No match → drop silently (no error response — input
is fire-and-forget). The browser's "if state != 'you' don't send"
check is UX courtesy only.

## Tests / smoke

**Unit (peer-side authority registry):**

- `DisplayInputHolder::FederatedWebRtc { peer_id, session_id }`
  arbitrates same as `LocalWs`.
- Take/release/handover cross-provenance (Local takes from Federated,
  Federated takes from Local).
- Federated holder auto-released when its WebRtcPeer drops via
  `remove_peer`; entry removed from `by_display`.
- Unclaimed represented as map absence; no `Option` in the value type.

**Unit (input gate, F-2):**

- Federated input dropped when registry has no entry for `display_id`.
- Federated input dropped when holder is wrong
  `FederatedWebRtc { peer_id, session_id }`.
- Federated input dropped when holder is `LocalWs`.
- Federated input accepted when holder matches request's
  `(peer_id, session_id)`.

**Integration:**

- Two federated PeerDisplayConnections, only one holds, handover
  broadcasts personalized `state` to each.
- Mixed local + federated competition, last-writer-wins.
- Authority data channel bootstrap: panel opens → channel opens →
  initial state arrives → chip renders.

**Smoke (extension to `docs/smoke-display.md`, new §7):**

- Open peer-display panel: chip starts `unknown` briefly, then flips
  to `unclaimed` ("Input: shared") after channel-open snapshot.
- Click Take Control → chip flips to `you`; second federated browser's
  chip flips to `other` ("Input: another viewer").
- (F-2) Move mouse over peer video → peer's `xdotool mousemove` fires
  (verify via peer log or backend trace).
- Click Release → chip flips to `unclaimed`; second browser's chip
  flips to `unclaimed`; second browser can take.
- Two-primary handover: two Intendant.app instances, both viewing
  same peer; last-to-take wins.
- Federated browser closes while holding → other browser's chip flips
  to `unclaimed` within ~1s (WebRtcPeer drop edge).

## Slice boundaries

| Slice | Scope | Ships |
|---|---|---|
| **F-1** | Authority data channel (`display_input_authority`) on federated `PeerDisplayConnection`. Peer registers federated WebRtcPeer's authority data channel as a state-broadcast target. `Command::SendAuthorityState` + `WebRtcPeer::send_authority_state` + driver write. Initial-snapshot-on-channel-open bootstrap. Browser UI: chip + Take/Release buttons, no input sending. Peer registry refactored to `HashMap<u32, DisplayInputHolder>` with explicit `LocalWs` / `FederatedWebRtc` provenance. Arbitration unified across both holder kinds. **Input still deny-by-default; `build_federated_input_authorizer` unchanged.** | View + visible-authority baseline; UX-complete except input itself |
| **F-2** | `control` / `pointer` data channels on federated `PeerDisplayConnection`. Replace `build_federated_input_authorizer()` with real registry predicate. Browser sends raw `InputEvent` JSON when chip = "you". | Federated control end-to-end |
| **F-3** | Cross-primary handover smoke + smoke-display.md §7. | Documented + verified |

Each slice independently shippable. F-1 is the largest (registry
refactor + new data channel + chip UX) but adds zero attack surface
(deny-by-default still applies). F-2 flips the gate. F-3 is
documentation + smoke validation.

## Out of scope

- Video codec / transport (federated VP8 baseline stays as-is).
- TURN / loss work (#69 deferred).
- Connection flapping (#72 deferred).
- Per-primary multi-operator-identity authority policies (current
  model: any operator on a primary that holds = same identity from
  peer's perspective; multi-operator scoping is a future slice).
- Authority recording / replay.
- Clipboard / file transfer over federated path (clipboard already
  has a separate channel design).
