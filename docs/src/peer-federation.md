# Peer Federation

Intendant can federate with **other autonomous agent daemons as equals** — other
Intendants, A2A-speaking peers, OpenClaw gateways, MCP-server-shaped peers. A
federated peer is a sibling, not a subordinate: the two daemons exchange events,
delegate tasks by capability, and — between Intendants — share each other's
displays across machines.

This chapter covers what federation is and how it differs from external agents,
the Agent Card and discovery model, the peer actor/registry/coordinator layer,
the transport stack (native WebSocket, multi-URL probing, cert pinning), the
cross-machine display path, and the LAN/TLS setup. For the local display pipeline
those federated displays plug into, see [Display Pipeline](./display-pipeline.md).

## Federation vs. External Agents

These are two orthogonal relationships, and they compose:

| | **Peer federation** (`src/bin/caller/peer/`) | **External agents** (`external_agent`) |
|---|---|---|
| Relationship | Peer / peer (A2A-shaped) | Master / worker (ACP-shaped) |
| Mental model | "I federate with a peer daemon" | "I spawn a process and give it a task" |
| Right for | OpenClaw, Hermes, Letta, **another Intendant** | Codex, Claude Code, Aider, goose |
| Lifecycle | Connect to an already-running daemon | Spawn and supervise a child process |

A peer Intendant can itself supervise a Codex subprocess via its own
`external_agent` layer while being driven from this side as a `peer` — the two
layers don't know about each other.

## Agent Card and Discovery

Every Intendant daemon serves an **Agent Card** at
`/.well-known/agent-card.json` (`peer/card.rs`). The card is the single source of
truth for *who this peer is, what it can do, how to reach it, and how to
authenticate*:

```json
{
  "id": { "kind": "intendant", "label": "nicks-mac" },
  "label": "nicks-mac",
  "version": "0.x.y",
  "git_sha": "abc1234",
  "transports": [
    { "type": "intendant-ws", "url": "ws://192.168.1.42:8765/ws" },
    { "type": "intendant-ws", "url": "wss://node.tail-abcd.ts.net:8443/ws" }
  ],
  "capabilities": [
    { "kind": "display" },
    { "kind": "computer-use" },
    { "kind": "voice" }
  ],
  "auth": { "transport": { "scheme": "none" } }
}
```

Key fields:

- **`id`** — a stable opaque `PeerId` (`peer/id.rs`). `id.kind()` is the source of
  truth for the daemon kind (`intendant`, `a2a`, `openclaw`, `mcp`, …); there is
  no separate `kind` field, by design.
- **`transports`** — one or more addresses **in preference order** (highest first).
  A single Intendant typically advertises its native WebSocket *and*, once
  shipped, an MCP/A2A endpoint, all in one card. Transport kinds:
  `intendant-ws` (native), `a2a`, `mcp` (with a nested transport kind), and
  `openclaw-ws` (with a role).
- **`capabilities`** — what the peer *offers* as services: `display`, `voice`,
  `phone`, `computer-use`, `knowledge`, `recording`, `task-delegation`,
  `message-relay`, or a string-tagged `custom:<name>`. The coordinator routes work
  by matching against this list.
- **`auth`** — what the peer *requires* of inbound connections (see
  [Authentication](#authentication) below).

A peer that advertises something an older build doesn't recognize (a future
transport, capability, or auth scheme) deserializes that one position to an
`Unknown` fallback variant rather than failing the whole card parse; the registry
then filters `Unknown` out when picking a transport.

### Advertised endpoints — `--advertise-url`

A daemon's card lists the URLs *peers should try*. By default the gateway
auto-detects its listener URL, but a NAT'd / tunneled / multi-homed daemon must
advertise what's actually reachable:

```bash
intendant --web --advertise-url ws://192.168.1.42:8765/ws \
                --advertise-url wss://node.tail-abcd.ts.net:8443/ws
```

`--advertise-url` is repeatable; each occurrence appends one URL in preference
order. When non-empty, the CLI list replaces both the `[server.advertise]` config
value and the auto-detected URL (operator wins). The same list also seeds the
**primary-relay TCP fallback** for cross-machine display (see below).

## The Peer Actor / Registry / Coordinator Model

```
            ┌───────────────────────────────────────────────┐
            │                   Coordinator                  │   capability-based
            │   TaskRequest{ required_capabilities } ──► pick │   routing
            └──────────────────────┬────────────────────────┘
                                   │ delegate_task
            ┌──────────────────────▼────────────────────────┐
            │                  PeerRegistry                  │   HashMap<PeerId, PeerHandle>
            │   add_peer: fetch card → pick transport → spawn │
            └──────────────────────┬────────────────────────┘
                                   │ one per peer
            ┌──────────────────────▼────────────────────────┐
            │   PeerHandle  ◀──watch── ConnectionState        │
            │      │ commands (mpsc)        events (broadcast) │
            │      ▼                                           │
            │   per-peer actor task                           │
            │   connect → main-loop → reconnect (backoff)     │
            │      │ owns                                      │
            │      ▼                                           │
            │   Box<dyn PeerTransport>  ───────► the wire      │
            └─────────────────────────────────────────────────┘
```

- **`PeerTransport`** (`peer/traits.rs`) is the single transport trait. It accepts
  the sender side of an `mpsc::Sender<PeerEvent>` at construction and pushes
  events as they arrive off the wire — there's no "take the stream once"
  awkwardness. Outbound work is a transport-neutral `PeerOp` envelope
  (`SendMessage`, `DelegateTask`, `CancelTask`, `QueryTaskStatus`,
  `InvokeCapability`, `ResolveApproval`, `WebRtcSignal`); a `TransportFeatures`
  struct declares which verbs a transport class supports.
- **The per-peer actor** (`peer/actor.rs`) owns the transport by value and runs a
  `connect → main-loop → reconnect` state machine with **indefinite exponential
  backoff** (500 ms initial, 30 s cap, jitter, reset on every successful connect).
  Inbound events fan out **durable-first**: to a bounded log sink (must not drop —
  if it's slow the actor pauses, transitively back-pressuring the wire), then to a
  lossy broadcast for UI subscribers. Commands are only processed while
  `Connected`; ones that arrive mid-reconnect wait in the bounded command channel.
- **`PeerRegistry`** (`peer/registry.rs`) owns the `HashMap<PeerId, PeerHandle>`.
  `add_peer` fetches the card from `/.well-known/agent-card.json`, picks the first
  supported `TransportSpec`, constructs the transport, and spawns the actor. If no
  supported transport is advertised it fails cleanly with `PeerError::CardFetch`.
- **`Coordinator`** (`peer/coordinator.rs`) sits on top and does **capability-based
  routing**: given a `TaskRequest` with `required_capabilities`, it picks the first
  eligible peer — one that is `Connected` *and* whose card advertises every
  required capability — in lexicographic `PeerId` order (deterministic, so
  idempotent retries route to the same peer) and delegates via the handle.

## Transports

Phase 1 ships the native Intendant↔Intendant transport. A2A, OpenClaw, and
MCP-as-peer transports slot in as sibling modules behind the same `PeerTransport`
trait.

### Native WebSocket — `IntendantWsTransport`

Speaks Intendant's own `/ws` protocol — the highest-fidelity path between
Intendants. The full `AppEvent` stream is upcast into the lean transport-neutral
`PeerEvent` vocabulary by `peer/upcast.rs` (there is deliberately no
`Native(AppEvent)` escape hatch). HTTP(S) base URLs for card discovery are derived
from the WebSocket URL (`ws://…/ws` → `http://…`).

### Multi-URL probing — `MultiTransport`

When a card advertises several reachable addresses (LAN IP, Tailscale tailnet IP,
port-forwarded WAN URL), `MultiTransport` (`peer/transport/multi.rs`) walks the
candidates **in card order** and uses the first whose `connect()` succeeds. Every
reconnect re-walks the whole list from the top, so if a more-preferred path comes
back online while running on a fallback, the next reconnect picks it up. Before any
candidate connects, `features()` reports the *union* of all candidates' features
so coordinator-level checks don't prematurely reject an op a candidate could
support once connected.

### Cert pinning over mTLS — `PinnedMutualTls`

`peer/transport/pinning.rs` provides a custom rustls `ServerCertVerifier` that
accepts a presented server cert **iff its SHA-256 fingerprint matches one of the
operator-supplied pinned values**. This is defense in depth on top of (or instead
of) plain mTLS: mTLS alone trusts every cert a trusted CA signed, so a CA
compromise or a leaked wildcard lets an attacker impersonate the peer. Pinning the
exact expected cert (or a rotation set) closes that gap.

The pinned peer advertises its fingerprints in its card under
`auth.transport = PinnedMutualTls { server_cert_fingerprints }`; connecting daemons
build a `PinnedFingerprintVerifier` and use it for **both** the WebSocket connect
and the agent-card HTTP fetch. Fingerprints are lowercase or uppercase hex, with
optional `:` separators (the OpenSSL format). Pinning replaces only the cert-path
check — the TLS handshake **signature** is still verified normally, so an attacker
who steals the cert bytes but not the private key still fails the handshake.

## Cross-Machine Display

Federated Intendants can share each other's displays in the browser. The defining
property:

> **The primary is a signaling middleman only — encoded video flows
> browser ↔ peer directly, never through the primary.** A primary-relay TCP path
> exists strictly as a fallback when no direct path can be formed.

```
                signaling (SDP/ICE)
   browser ───────────────────────────► primary ───────────────► peer daemon
      │         ws (/ws + PeerOp::WebRtcSignal)   IntendantWs        │
      │                                                              │
      │              direct encrypted media (WebRTC, via TURN)       │
      └──────────────────────────────────────────────────────────────┘
                          (primary not in this path)

   ── fallback when no direct path forms ──
   browser ──RFC4571/STUN-framed TCP──► primary ──relays bytes──► peer daemon
```

### Signaling

WebRTC signaling is carried over the federation transport as
`PeerOp::WebRtcSignal` (primary → peer) and `PeerEvent::WebRtcSignal`
(peer → primary), both scoped by `{ display_id, session_id }` (`peer/event.rs`).
The **browser is the offerer** — mirroring the local browser→daemon flow:

- **Primary → peer** carries the browser's `Offer` and trickled `IceCandidate`s.
  The `Offer` may include `advertise_tcp_via_url` (the URL the operator typed into
  "Add Peer"), which the peer uses to derive its ICE-TCP host candidate and
  register against its own `TcpPeerRegistry`.
- **Peer → primary** carries the peer's `Answer` and trickled `IceCandidate`s.

`session_id` is a browser-generated UUID, so multiple sessions to the same display
don't collide and a stale tab can't interfere with a fresh one. Unknown signal
kinds parse to an ignored `Unknown` variant for forward compatibility.

### Federated Browser Workspaces

Browser workspaces are the browser-specific sibling of shared displays: they
represent a concrete browser surface that an agent can control through CDP,
Playwright, Agent Browser, or a streamed-display fallback. The local registry
models `placement = local | peer` and carries the target `peer_id`, but remote
peer placement intentionally fails closed until the federation transport has a
first-class browser-workspace operation.

The intended federation rule is the same one used for display input authority:
the peer that owns the browser process is the source of truth for leases. If two
agents on one primary try to access a browser workspace hosted by another peer,
or if agents on multiple peers race for the same remote browser, the owning peer
serializes `acquire_browser_workspace` and rejects the second holder unless the
caller uses an explicit force-takeover. Local same-machine workspaces can use
CDP/Playwright semantics for low-latency automation; cross-machine users can
fall back to the display/shared-view streaming path when local browser control is
not possible.

### Direct media and the TCP-relay fallback

Once signaled, the browser forms a **direct** WebRTC media path to the peer,
typically through TURN: when a TURN server is configured in `[webrtc].ice_servers`
the federated path pins the browser to `iceTransportPolicy: 'relay'` and both ends
can allocate on the configured coturn (without a TURN server the policy is left at
its default). When no direct path can be formed, a **primary-relay TCP fallback**
kicks in (`display/webrtc.rs` `TcpRelayRegistry`):

1. As the peer's `Answer` flows back through the primary, the primary parses the
   peer's ICE ufrag and resolves the peer's advertised URL to a `SocketAddr`,
   registering `(ufrag → addr)` in a `TcpRelayRegistry`.
2. The primary injects a relay TCP candidate (pointing at its own HTTP port) into
   the Answer SDP alongside the peer's direct candidates.
3. If the browser ends up using that candidate, the connection lands on the
   primary's HTTP port with the peer's ufrag in its first STUN USERNAME. The
   primary finds no local match in `TcpPeerRegistry` but a hit in
   `TcpRelayRegistry`, dials the peer, re-frames the peeked first frame, and
   shuttles bytes bidirectionally between browser and peer.

The relay multiplexes onto the same HTTP port as the dashboard (the same accept-loop
peek that distinguishes HTTP / WebSocket / local ICE-TCP grows a relay branch), so
it needs no extra port-forwarding.

### Federation codec policy — `federation_allow_h264`

H.264 over a lossy TURN-relayed path is fragile: a full-resolution 2.5 Mbps stream
produces a seed IDR of hundreds of RTP packets, and at ~17% loss the probability of
reassembling every packet is effectively zero, so the stream never bootstraps. By
default federation therefore **pins VP8** in the browser:

```toml
[webrtc]
federation_allow_h264 = false   # default: VP8-pinned over relays
```

Setting `federation_allow_h264 = true` lets the federated path negotiate the
peer's H.264. To survive lossy relays, that H.264 uses a dedicated **loss-resilient
shape**: a quarter-resolution layer at a capped bitrate (`LayerSpec::single_federated`,
RID `fed`, ~250 kbps — a small ~17-packet IDR with ~24% intact-arrival odds),
combined with periodic IDRs and same-SSRC NACK retransmission. The federated H.264
encoder keys a *distinct* pool slot (`EncoderId { H264, fed }`) so it can never be
handed a full-resolution H.264 encoder a local viewer spawned, or vice versa. The
local, same-machine display path is unaffected by this flag and uses the full
pipeline from [Display Pipeline](./display-pipeline.md).

A transport must support relaying `WebRtcSignal` frames (`TransportFeatures::webrtc_signal`)
for the federated display path to work; the dashboard hides the "View display"
affordance for peers whose transport can't carry it.

### Input authority on federated displays

The peer remains the **single source of truth** for who holds each of its displays.
A unified authority registry on the peer arbitrates both local WebSocket holders
and federated WebRTC holders with the same last-taker-wins rules, distinguishing
provenance explicitly (`LocalWs` vs. `FederatedWebRtc`, never inferred from string
shape). Federated authority requests/state ride a dedicated
`display_input_authority` data channel on the federated connection; federated input
events reuse the existing `control` / `pointer` channels with raw `InputEvent` JSON.
The **peer-side gate is the security boundary** — input arriving without authority
is dropped silently at the peer regardless of what the browser believes; the
browser-side check is UX only. The full protocol is in
[`docs/design-federated-input-authority.md`](https://github.com/lovon-spec/intendant/blob/main/docs/design-federated-input-authority.md).

## LAN Access and TLS

Two independent mechanisms expose the dashboard securely; they can be used
together or separately.

### Native HTTPS/WSS — `--tls`

`web_tls.rs` serves the `--web` dashboard over HTTPS/WSS directly, with no proxy,
on **every platform including Windows**. It's pure-Rust (`rustls` + `rcgen`, both
on the `ring` crypto provider — no OpenSSL anywhere in the tree). The gateway's
accept loop peeks the first bytes of each connection and, on seeing a TLS
ClientHello, wraps the socket in a `TlsAcceptor` before handing the decrypted
stream to the existing HTTP/WebSocket handling.

```bash
intendant --web --tls                          # auto self-signed cert
intendant --web --tls-cert chain.pem --tls-key key.pem   # explicit PEM (implies --tls)
```

`--tls-cert` / `--tls-key` must be supplied together; supplying either implies
`--tls`.

Native HTTPS/WSS is also the direct way to make a remote dashboard origin a
browser secure context when you need Station WebGPU, microphone/camera, browser
screen capture, or stricter clipboard APIs. Use a trusted certificate; merely
clicking through a self-signed certificate warning is not a reliable way to
unlock these browser APIs. See
[Web Dashboard: Secure Browser Contexts](./web-dashboard.md#secure-browser-contexts).

### mTLS reverse proxy — `intendant lan`

`src/bin/caller/lan/` ports the old `setup-lan.sh` script to Rust. `intendant lan
setup` stands up an **mTLS nginx reverse proxy** in front of `intendant --web` so
LAN clients (phones, tablets, other boxes) reach the dashboard over HTTPS
authenticated by a **client certificate**. Cert generation is pure-Rust (`rcgen` +
RustCrypto `rsa` + `p12-keystore`); new cert material uses RSA-2048 with SHA-256
signatures so Apple configuration-profile certificate payloads match Apple's
documented compatibility path. The proxy/service plumbing (nginx config,
apt/brew installs, systemd/launchd) is Unix-specific. Subcommands:

| Command | Action |
|---|---|
| `intendant lan setup` | Generate CA + server/client certs, render nginx config, start the proxy + strict HTTPS enrollment server |
| `intendant lan recert` | Re-issue certs |
| `intendant lan remove` | Tear down the proxy and config |
| `intendant lan list` | List issued client certs |
| `intendant lan serve-certs` | Run strict HTTPS enrollment for importing `ca.crt`, client `.p12`/`.pfx`, or Apple `.mobileconfig` onto devices |

```bash
intendant lan setup --name nicks-mac --https-port 8443
```

`intendant lan` is **gated off Windows** — the nginx/systemd/apt machinery doesn't
apply there. Cert *generation* is cross-platform, so a Windows daemon can still use
`--tls` for native HTTPS and `read_server_cert_fingerprint` to publish a pinned
fingerprint; to put it behind an mTLS proxy, front it with your own reverse proxy.
See [Windows Support](./windows-support.md).

Enrollment is not a plain unauthenticated download. The temporary
`serve-certs` endpoint runs HTTPS with the LAN server certificate. The CLI does
not print the expected server fingerprint or the enrollment secret at startup;
the operator first copies the SHA-256 fingerprint observed in the browser's
certificate UI into the CLI. Only a match reveals a one-time secret, and only a
browser that redeems that secret can download the CA, client certificate, or
Apple configuration profile. The page detects the browser only to put the most
likely install path first; all artifacts remain gated by the terminal-paired
browser session.

The mTLS proxy also solves browser secure-context requirements for LAN clients
once the CA/client identity are installed. That matters for Station's WebGPU
renderer, microphone/camera, browser screen capture, and stricter clipboard APIs;
plain `http://<LAN-IP>:8765` does not expose those features.

### How auth maps to the Agent Card

The card's `auth` field tells connecting peers what to send. Construct it via the
`AuthRequirements` helpers:

| Helper | `transport` | `application` | Use when |
|---|---|---|---|
| `none()` | `None` | — | Trusted network: loopback, tailnet, LAN behind a firewall (the phase-1 default) |
| `mutual_tls()` | `MutualTls` | — | Trusted-LAN federation behind `intendant lan setup` |
| `bearer(hint)` | `None` | `Bearer` | Over an already-secured transport (e.g. a WireGuard tailnet) |
| `mutual_tls_and_bearer(hint)` | `MutualTls` | `Bearer` | WAN exposure — even a TLS-verification CVE still needs a valid bearer |

Auth is layered: a wire-layer `TransportAuth` (`None` / `MutualTls` /
`PinnedMutualTls`) satisfied during the TLS handshake, plus an optional per-request
`ApplicationAuth` (a bearer token, with the actual secret referenced by `hint`
rather than embedded in the card). A `bearer` `hint` is a human-readable reference
like `"intendant.toml [peer.foo] bearer_token"` so the registry can locate the
secret without leaking it onto the wire.

## See Also

- [Display Pipeline](./display-pipeline.md) — the local capture/encode/WebRTC
  pipeline that federated displays plug into, and the `[webrtc]` config
- [Windows Support](./windows-support.md) — why `intendant lan` is gated off
  Windows and what to use instead
- [`docs/design-federated-input-authority.md`](https://github.com/lovon-spec/intendant/blob/main/docs/design-federated-input-authority.md)
  — the full federated input-authority protocol
