# Tile-Based Display Streaming — Design Doc (#82, D-0)

Status: design only. No implementation has started. D-1 is gated on review of this document.

## Problem statement

Whole-frame VP8 at full resolution fails the §9 visual-freshness acceptance bar regardless of bitrate. Reference runs (smoke-display.md §9.5):

| Path | p50 | p95 | Max | fps | Result |
|---|---|---|---|---|---|
| VP8-q baseline (small) | 50 ms | 68 ms | 446 ms | 20.28 | **Pass** |
| VP8-f full-res @ 2.5 Mbps | 34 ms | 5048 ms | 38385 ms | 0.68 | Fail |
| VP8-f full-res @ 800 kbps | 35 ms | 1083 ms | 1136 ms | 2.85 | Fail |

The peer encoder is not the bottleneck (continues 27-29fps with zero drops in both failure cases). The wire is not catastrophically lossy (peer→coturn measured continuous). The bottleneck is the data model: a video codec encodes every pixel of every frame, even when 90-95% of the frame is identical to the previous one. Bytes spent re-encoding the static background are bytes not spent encoding the small fast-changing region the user actually cares about.

Dirty-region streaming inverts the model: encode only what changed, leave everything else alone. RFB/VNC has done this for decades; modern derivatives (Spice, RDP, Citrix HDX) layer dirty-region updates on top of video for high-motion full-screen content. This design specifies how to add the same architecture to Intendant alongside the existing VP8 path, without disrupting federation, auth, or input.

## Architecture

```
PEER                                          BROWSER
====                                          =======
Capture (X11/Wayland/macOS)
  produces: (frame, Vec<DirtyRect>)
       │
       ▼
DamageBackend trait
  → poll() → Vec<Rect>       (XDamage / macOS dirty / Wayland damage)
       │
       ▼
SyntheticDirtySources                        ┌──────────────────┐
  + cursor old/new positions                 │ TileCompositor   │
  + diagnostic marker tile                   │  Canvas (native) │
  + (optional) compositor frame-diff backup  │  TileMap[id→seq] │
       │                                     │  ChunkAssembler  │
       ▼                                     └──────────────────┘
TileGrid partition                                    ▲
  → Vec<Rect> → Set<TileId>                          │
       │                                              │
       ▼                                              │
TilePolicy                                            │
  case A: dirty fraction < threshold ──┐              │
  case B: dirty fraction ≥ threshold ──┤              │
       │                               │              │
       ▼                               ▼              │
TileEncoder                    VP8-q encoder          │
 raw_bgra / rle_bgra v1        (existing path)        │
       │                               │              │
       ▼                               ▼              │
TileTransport ─── reliable+ordered ──── DC:control ───┤
  + chunking                ─── reliable+ordered ──── DC:snapshot ──┤
  + backpressure-aware drop ─── unordered+lossy ───── DC:tiles ─────┤
                                       video track ───────────────► (HTMLVideoElement, hidden in tile mode)
       ▲
       │
DC:control inbound ◄── subscribe / snapshot_request / gap_report ──
                                                                     │
DC:authority/control/pointer (existing) ◄── input ─────────────────  │
                                                                     │
                                                              (browser)
```

Federation transport unchanged. Tile data channels are negotiated on the same `RTCPeerConnection` as today's video track, audio track, and existing `authority`/`control`/`pointer` data channels. The existing VP8-q video track stays as the proven fallback.

## Wire format

Single binary framing on the data channels. Little-endian. Versioned at the frame header.

```
+------+------+--------+-----------+
| u8   | u8   | u16    | varies    |
| ver  | type | flags  | body      |
+------+------+--------+-----------+
ver = 0x01
```

| code | name              | channel        | direction          |
|------|-------------------|----------------|--------------------|
| 0x01 | `SnapshotChunk`   | snapshot       | peer → browser     |
| 0x02 | `TileUpdate`      | tiles          | peer → browser     |
| 0x03 | `Resize`          | control        | peer → browser     |
| 0x04 | `EpochAdvance`    | control        | peer → browser     |
| 0x05 | `FallbackToVideo` | control        | peer → browser     |
| 0x06 | `FallbackToTile`  | control        | peer → browser     |
| 0x07 | `CursorState`     | control        | peer → browser     |
| 0x10 | `Subscribe`       | control        | browser → peer     |
| 0x11 | `SnapshotRequest` | control        | browser → peer     |
| 0x12 | `GapReport`       | control        | browser → peer     |
| 0xFF | `Error`           | control        | bidirectional      |

### Chunking — `SnapshotChunk` and `TileUpdate`

A logical snapshot or tile-update can exceed SCTP datachannel message limits. Practical safe ceilings: 16 KB universal, 32 KB target, 64 KB hard cap. Each on-wire message is bounded by `MAX_DATACHANNEL_MESSAGE_SIZE = 32 * 1024` bytes (constant, configurable, conservative default). Chunking is mandatory for snapshots, recommended for tile updates.

```
SnapshotChunk:
  u32 epoch
  u32 snapshot_id          // identifies one logical snapshot across chunks
  u16 chunk_index          // 0..chunk_count-1
  u16 chunk_count          // total chunks for this snapshot
  u16 grid_w_tiles         // present in chunk 0 only; ignored otherwise
  u16 grid_h_tiles         // present in chunk 0 only
  u16 tile_size_px         // present in chunk 0 only
  u32 record_count_in_chunk
  TileRecord[record_count_in_chunk]:
    u16 tile_x
    u16 tile_y
    u8  encoding           // 0=raw_bgra, 1=rle_bgra; 2/3 reserved for D-4
    u32 payload_len
    u8[payload_len] payload
```

Browser buffers chunks by `snapshot_id`. When all `chunk_count` chunks have arrived (snapshot channel is reliable+ordered, so this is monotonic), browser applies them as one atomic snapshot, clears the buffer entry, and bumps `last_snapshot_id`. A new `snapshot_id` arriving while another is still incomplete invalidates the older buffer.

```
TileUpdate:
  u32 epoch
  u32 seq                  // monotonic within epoch; gaps mean drops
  u16 record_count
  TileRecord[record_count]
```

Per-message size cap is enforced at packing time on the peer: if the next tile would push the message over `MAX_DATACHANNEL_MESSAGE_SIZE`, emit the current message and start a new one with `seq` incremented. Per-tile staleness logic on the browser handles split frames correctly (each TileRecord supersedes per (tile_x, tile_y, seq)).

### Other frames

```
Resize: u32 new_epoch, u16 new_grid_w, u16 new_grid_h, u16 new_tile_size_px
EpochAdvance: u32 new_epoch
FallbackToVideo / FallbackToTile: u32 new_epoch
CursorState: u32 epoch, u32 seq, i32 x_px, i32 y_px, u8 visible
Subscribe: u32 client_id
SnapshotRequest: u32 epoch, u8 reason  (0=startup, 1=resize, 2=gap, 3=manual)
GapReport: u32 epoch, u32 last_seen_seq, u32 expected_seq
Error: u16 code, u16 msg_len, u8[msg_len] msg
```

`CursorState` is sent on the control (reliable) channel because cursor position is small, latency-sensitive, and superseded — but we send the latest, not a history. See "Cursor handling" below for why this is a separate frame and not just synthetic dirty tiles.

## Datachannel reliability tiers

Three channels, each opened at PC setup time. Picked deliberately, not "everything reliable".

```js
// Browser side, in PeerDisplayConnection.connect():
const dc_control  = pc.createDataChannel('tile-control',  { ordered: true,  maxRetransmits: null });
const dc_snapshot = pc.createDataChannel('tile-snapshot', { ordered: true,  maxRetransmits: null });
const dc_tiles    = pc.createDataChannel('tile-deltas',   { ordered: false, maxRetransmits: 0 });
```

Reasoning:

- **`tile-control`** — reliable, ordered. Subscribe, SnapshotRequest, Resize, EpochAdvance, Fallback transitions, CursorState, Error. Low rate. Loss of any of these breaks invariants.
- **`tile-snapshot`** — reliable, ordered. SnapshotChunk frames. Each snapshot is one atomic state-of-the-world; partial delivery is unrecoverable. Reliable + ordered is the only sane choice. Backpressure on this channel manifests as snapshot delivery delay; recovery sends the next snapshot.
- **`tile-deltas`** — unordered, no retransmits. TileUpdate frames. **Each TileUpdate is supersedable per-tile by any later update for the same (tile_x, tile_y).** If a frame is dropped, the affected tiles keep their previous content for ~33ms longer until the next update for those tiles arrives. Sequence numbers carry within-epoch ordering for the per-tile staleness check.

WebKit compatibility: `{ ordered: false, maxRetransmits: 0 }` is universally supported in all major browsers in 2026, including WebKit. If field testing surfaces a WebKit edge case, the documented fallback is `ordered: true, maxRetransmits: 0` (still unreliable but ordered). The architecture tolerates either. Acceptable HOL blocking impact is mitigated by the per-tile supersedability and the periodic snapshot.

## Backpressure and send-queue policy

**Non-negotiable: tile traffic must never starve input or control.** The three datachannels share the underlying SCTP association. Even though SCTP arbitrates between streams fairly, a sustained tile-delta flood can saturate the outbound network buffer and back-pressure all channels.

Peer-side `TileTransport` monitors `bufferedAmount` on each channel via the `RTCDataChannel` API equivalent in str0m, and applies a priority drop policy:

```rust
const TILE_DELTAS_HIGH_WATERMARK_BYTES: usize = 256 * 1024;  // start dropping
const TILE_DELTAS_LOW_WATERMARK_BYTES:  usize =  64 * 1024;  // resume sending
const TILE_SNAPSHOT_HIGH_WATERMARK_BYTES: usize = 1024 * 1024;  // delay next snapshot

enum TileSendDecision {
    Send,
    DropDelta,        // tile-deltas only; counter increments for diag
    DelaySnapshot,    // tile-snapshot only; defer until below low watermark
}

impl TileTransport {
    fn decide_tile_update(&self) -> TileSendDecision {
        let buffered = self.dc_tiles.buffered_amount();
        if buffered >= TILE_DELTAS_HIGH_WATERMARK_BYTES {
            self.dropping_deltas = true;
            return TileSendDecision::DropDelta;
        }
        if self.dropping_deltas && buffered <= TILE_DELTAS_LOW_WATERMARK_BYTES {
            self.dropping_deltas = false;
        }
        if self.dropping_deltas { TileSendDecision::DropDelta } else { TileSendDecision::Send }
    }
    // similar for snapshot, but DelaySnapshot rather than drop
}
```

Control channel is never throttled. CursorState is on control, so cursor updates also remain prompt under tile-channel pressure. Input traveling on the existing `authority`/`control`/`pointer` data channels is unaffected by tile transport state.

**Telemetry:** drop counts (per-tick and cumulative), watermark transitions, and current `bufferedAmount` per channel are exported as metrics so D-5 can verify backpressure behavior in measured workloads.

## Sequence + epoch semantics

**Epoch** changes when the world changes shape: resize, mode-fallback transition, source-side reset. Browser drops every TileUpdate with `epoch < current_epoch` and triggers a SnapshotRequest for any TileUpdate with `epoch > current_epoch`.

**Seq** is monotonic per epoch. Per-tile staleness check on the browser:

```
TileMap : Map<TileId, AppliedState>
AppliedState { epoch: u32, seq: u32 }

on TileUpdate(epoch, seq, records):
  if epoch < current_epoch: drop
  if epoch > current_epoch: send GapReport, buffer this update
  for record in records:
    let id = (record.tile_x, record.tile_y)
    let prev = TileMap.get(id)
    if prev.is_some() && prev.seq >= seq: continue   // tile-stale, drop this record
    decode and blit record.payload at (id.x*tile_size, id.y*tile_size)
    TileMap[id] = { epoch, seq }
```

Snapshot completion resets every tile to `(epoch, snapshot_seq)`. Subsequent updates with `seq <= snapshot_seq` for any tile are dropped.

## Source-side modules (Rust)

```
src/bin/caller/display/
├── capture/damage.rs           NEW   trait DamageBackend, capability enum
├── capture/x11_damage.rs       NEW   X11 XDamage impl
├── capture/wayland_damage.rs   NEW   stub returning DamageCapability::None initially
├── capture/macos_damage.rs     NEW   stub returning DamageCapability::None initially
├── tile/grid.rs                NEW   damage rects → tile id set
├── tile/synthetic_dirty.rs     NEW   cursor + marker dirty injection
├── tile/encode.rs              NEW   raw_bgra / rle_bgra encoders, dispatch
├── tile/policy.rs              NEW   fallback decision + hysteresis
├── tile/transport.rs           NEW   wire-format + chunking + backpressure
├── tile/snapshot.rs            NEW   periodic snapshot generator + ringbuffer
└── webrtc.rs                   MODIFY add tile data channels alongside video track
```

Trait sketches:

```rust
// capture/damage.rs
pub trait DamageBackend: Send {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Rect>, DamageError>;
    fn capability(&self) -> DamageCapability;
}

pub enum DamageCapability {
    OsLevel,        // X11 XDamage, etc. — trustworthy dirty info
    FrameDiff,      // tile-hash fallback when OS doesn't report damage (D-4 work)
    None,           // no damage info at all → force full snapshot every tick
}

// tile/synthetic_dirty.rs
pub struct SyntheticDirtySources {
    last_cursor: (i32, i32),
    marker_enabled: bool,
    marker_tile: TileId,
}

impl SyntheticDirtySources {
    /// Returns rects that must be marked dirty regardless of OS damage.
    /// Called every tick; consumes any cursor/marker state changes since
    /// last call.
    pub fn collect(
        &mut self,
        new_cursor: Option<(i32, i32)>,
        marker_changed: bool,
    ) -> Vec<Rect> { ... }
}

// tile/transport.rs
pub struct TileTransport {
    dc_control:  Box<dyn DataChannel>,
    dc_snapshot: Box<dyn DataChannel>,
    dc_tiles:    Box<dyn DataChannel>,
    chunker: SnapshotChunker,
    backpressure: BackpressureMonitor,
    metrics: TransportMetrics,
}
// All send paths go through methods that consult bufferedAmount before
// emitting. No direct dc.send() from outside this module.
```

## Browser-side compositor (JS)

`TileCompositor` class in `static/app.html`, owned by `PeerDisplayConnection` alongside (not instead of) the existing `<video>` element.

```js
class TileCompositor {
    constructor(container, { tileSize, gridW, gridH }) {
        this.canvas = document.createElement('canvas');
        this.canvas.className = 'peer-display-canvas';
        this.canvas.width  = tileSize * gridW;
        this.canvas.height = tileSize * gridH;
        container.appendChild(this.canvas);
        this.ctx = this.canvas.getContext('2d', { alpha: false });
        this.tileSize = tileSize;
        this.gridW = gridW;
        this.gridH = gridH;
        this.epoch = 0;
        this.tileMap = new Map();              // tileId → { epoch, seq }
        this.lastAppliedSnapshotId = -1;
        this.snapshotChunkBuffers = new Map(); // snapshot_id → { chunks: Map<idx, ArrayBuffer>, expected: count }
    }

    onSnapshotChunk(frame) {
        const buf = this.snapshotChunkBuffers.get(frame.snapshot_id) ??
                    { chunks: new Map(), expected: frame.chunk_count, epoch: frame.epoch,
                      grid_w: frame.grid_w_tiles, grid_h: frame.grid_h_tiles, tile_size: frame.tile_size_px };
        buf.chunks.set(frame.chunk_index, frame.records);
        this.snapshotChunkBuffers.set(frame.snapshot_id, buf);
        if (buf.chunks.size === buf.expected) {
            this._applySnapshot(frame.snapshot_id, buf);
        }
    }

    _applySnapshot(snapshot_id, buf) {
        if (snapshot_id <= this.lastAppliedSnapshotId) return;          // older snapshot, ignore
        if (buf.grid_w !== this.gridW || buf.grid_h !== this.gridH || buf.tile_size !== this.tileSize) {
            this.canvas.width  = buf.tile_size * buf.grid_w;
            this.canvas.height = buf.tile_size * buf.grid_h;
            this.gridW = buf.grid_w; this.gridH = buf.grid_h; this.tileSize = buf.tile_size;
        }
        this.epoch = buf.epoch;
        this.tileMap.clear();
        // Apply records in chunk-index order so any positional dependencies are deterministic.
        for (const idx of [...buf.chunks.keys()].sort((a, b) => a - b)) {
            for (const r of buf.chunks.get(idx)) {
                this._applyRecord(r, buf.epoch, /*snapshot_seq*/ 0);
            }
        }
        this.lastAppliedSnapshotId = snapshot_id;
        this.snapshotChunkBuffers.delete(snapshot_id);
    }

    onTileUpdate(frame) {
        if (frame.epoch < this.epoch) return;
        if (frame.epoch > this.epoch) { this._requestSnapshot('gap'); return; }
        for (const r of frame.records) {
            const key = (r.tile_y << 16) | r.tile_x;
            const prev = this.tileMap.get(key);
            if (prev && prev.seq >= frame.seq) continue;
            this._applyRecord(r, frame.epoch, frame.seq);
        }
    }

    _applyRecord(r, epoch, seq) {
        const px = r.tile_x * this.tileSize;
        const py = r.tile_y * this.tileSize;
        if (r.encoding === 0) {                                         // raw_bgra
            const id = new ImageData(new Uint8ClampedArray(r.payload), this.tileSize, this.tileSize);
            this.ctx.putImageData(id, px, py);
        } else if (r.encoding === 1) {                                  // rle_bgra
            const id = decodeRleBgraToImageData(r.payload, this.tileSize);
            this.ctx.putImageData(id, px, py);
        }
        // encodings 2 (webp), 3 (vp8_intra) deferred to D-4
        const key = (r.tile_y << 16) | r.tile_x;
        this.tileMap.set(key, { epoch, seq });
    }

    onResize({ epoch, grid_w, grid_h, tile_size }) { ... }
    onCursorState({ x_px, y_px, visible }) { ... }   // optional overlay; see below
    onFallbackToVideo() { this.canvas.style.display = 'none'; }
    onFallbackToTile() { this.canvas.style.display = 'block'; }
}
```

WebP and VP8-intra branches are deliberately omitted from D-3. Async decode paths (`createImageBitmap`) introduce ordering complications on top of the per-tile staleness check; defer them to D-4 once the synchronous-decode happy path is proven.

## Cursor handling

OS damage may not include cursor movement (X server typically renders the cursor as a hardware overlay; XDamage doesn't fire on hw-cursor moves). Two paths considered, both supported by the design:

**Path A: cursor as separate browser overlay (preferred, shipped in D-3c3).** Peer sends `CursorState` frames on the control channel with `(x_px, y_px, visible)`. Browser maintains a small absolutely-positioned cursor sprite over the canvas. No tile dirtying needed for cursor motion. Matches RFB rich-cursor pseudo-encoding and RDP cursor shadow extension.

**Path B: cursor as synthetic dirty rects (fallback / marker precedent).** `SyntheticDirtySources` injects two dirty rects per cursor move (old position area + new position area), each one tile-size square centered on the cursor. The compositor draws cursor as part of the captured frame (the X server's normal compositing path). This remains useful as a generalized synthetic-dirty hook, but hardware-cursor behavior on the peer made Path A the cleaner D-3 choice.

For D-3, use Path A for cursor freshness and keep `SyntheticDirtySources` for non-OS-damage sources such as the diagnostic marker. D-4 builds on the same cursor overlay instead of introducing a second cursor path.

## Diagnostic marker handling

The visual-freshness marker is rendered into the captured frame BEFORE encoding. With OS damage, the marker draw doesn't trigger XDamage (it's a draw into our captured-frame buffer, not into the X server's framebuffer). `SyntheticDirtySources` injects a synthetic dirty rect for the marker tile every time the marker value changes. Trivial — marker value increments are driven from the daemon, so the dirty injection is a one-line call adjacent to the increment.

`?diag=1` sampler on the browser side reads marker value from the canvas using `requestAnimationFrame` instead of `requestVideoFrameCallback`. Same percentile triple metric, same #80 acceptance bar. Confirmed compatible because both APIs deliver per-frame callbacks at vsync rate.

## Recovery semantics

| Trigger | Action |
|---|---|
| Browser subscribes for the first time | Server sends snapshot (chunked). Browser allocates canvas. |
| Browser detects epoch-advance | Browser awaits next snapshot for new epoch (server emits one immediately on epoch change). |
| Browser detects gap (`expected_seq` skipped > `GAP_REPORT_THRESHOLD = 10` within an epoch) | Browser emits `GapReport`. Server decides: small gap (≤ ringbuffer depth) → resend missing TileUpdate from per-tile ringbuffer; large gap → snapshot. |
| Periodic snapshot | Server emits snapshot every `SNAPSHOT_PERIOD = 30s` regardless. Bounds time-to-recover from any silent corruption. |
| Resize at source | Server sends `Resize` (control) → bumps epoch → sends new snapshot (chunked). Browser drops in-flight TileUpdates with old epoch. |
| `FallbackToVideo` | Server stops emitting tile updates. Browser hides canvas, shows VP8-q `<video>`. Server keeps tile channel open; periodic snapshot continues but at reduced rate (`SNAPSHOT_PERIOD_VIDEO_MODE = 60s`) so resume-to-tiles is fast. |
| `FallbackToTile` | Server emits new snapshot to seed canvas, then resumes TileUpdates. Browser hides `<video>`, shows canvas. |

### Bounded recovery

Snapshots and gap-recovery sends are not free. Bounds:

```
SNAPSHOT_PERIOD                = 30 seconds (tile mode)
SNAPSHOT_PERIOD_VIDEO_MODE     = 60 seconds (fallback mode)
SNAPSHOT_MAX_BYTES             = 4 MiB     (cap; if encoded snapshot exceeds, server logs warning and emits anyway — this scenario means whole-screen high-entropy content, which would be fallback-to-video territory)
SNAPSHOT_MIN_INTERVAL          = 2 seconds (rate limit on demand snapshots; a flood of GapReport from a broken client cannot trigger more frequent snapshots)
GAP_RINGBUFFER_DEPTH           = 250 ms of TileUpdates per channel
GAP_RINGBUFFER_MAX_BYTES       = 8 MiB
```

The ringbuffer is per-tile, not per-frame: keys are `TileId`, value is a small VecDeque of recent `(seq, encoded_payload)`. Bounded by total bytes; if a tile's recent payload would push the ringbuffer over budget, oldest entries are evicted regardless of age.

Snapshot send rate is throttled by `SNAPSHOT_MIN_INTERVAL` and gated by `tile-snapshot` channel `bufferedAmount`. A demand snapshot that arrives while one is already in flight (chunks still draining) is collapsed: the in-flight one continues, the new one is queued at most one-deep. Multiple SnapshotRequests within the throttle window from the same client get one snapshot in response.

This is the must-not-recreate-the-failure-in-a-different-costume guarantee: the recovery mechanism cannot itself become the source of a flood that breaks the channel it's trying to recover.

## Fallback to VP8 — semantics, threshold, anti-flap

**Fallback target is VP8-q, not VP8-f.** The proven baseline. Specifically: the existing single-RID-q federation video path that today's smoke runs already exercise. No new encoder negotiation; the video track is already up at PC setup time, encoder is gated on whether peers are subscribed (which they always are in practice).

**Resolution mismatch:** the canvas (tile mode) is at native peer display resolution. The VP8-q fallback video is at q-resolution (smaller than native). Behavior on fallback transition:

- Browser hides canvas, shows `<video>` element.
- The `<video>` element is sized to match the canvas's CSS layout box (CSS `width: 100%`), so visually the video is upscaled to fill the same area the canvas occupied. Slight perceived blur during fallback; acceptable because fallback is the high-motion case.
- Coordinate mapping for input: input events on either canvas or `<video>` produce the same normalized `(x, y) ∈ [0, 1]` from `event.offsetX / element.clientWidth`. Peer-side scales normalized coordinates by its actual display resolution. Same authority/active-controller logic, no changes.

**Threshold + anti-flap:**

```rust
// tile/policy.rs constants
HISTORY_K              = 8        // rolling window, ~270ms at 30fps
ENTER_VIDEO_THRESHOLD  = 0.25     // dirty fraction to switch tile→video
EXIT_VIDEO_THRESHOLD   = 0.15     // dirty fraction to switch video→tile (hysteresis gap)
MIN_DWELL_MS           = 500      // min time in any state before re-eval

fn evaluate(&mut self, dirty_fraction: f32) -> TileMode {
    self.history.push_back(dirty_fraction);
    if self.history.len() > HISTORY_K { self.history.pop_front(); }
    let avg = self.history.iter().sum::<f32>() / self.history.len() as f32;
    let dwell_ok = self.last_transition.elapsed() >= Duration::from_millis(MIN_DWELL_MS);
    let next = match self.state {
        TileMode::Tiles if dwell_ok && avg >= ENTER_VIDEO_THRESHOLD => TileMode::Video,
        TileMode::Video if dwell_ok && avg <= EXIT_VIDEO_THRESHOLD  => TileMode::Tiles,
        _ => self.state,
    };
    if next != self.state { self.state = next; self.last_transition = Instant::now(); }
    self.state
}
```

Rolling-average over per-tick `dirty_fraction = dirty_tile_count / total_tile_count`. Hysteresis (25% to enter, 15% to exit) prevents flapping at the boundary. `MIN_DWELL_MS = 500` caps mode-switch frequency at 2/sec.

Threshold values are starting hypotheses. D-5 measures actual transition frequency under realistic workloads (terminal scroll, browser scroll, video playback, cursor-only motion, idle desktop) and tunes them.

## Federation, auth, input — unchanged

Tile data channels ride the same `RTCPeerConnection` as today's federation pipeline. The federation transport (`PeerOp::WebRtcSignal`) carries any added SDP for the new channels transparently. Active-controller logic, per-display input authority, the existing `authority`/`control`/`pointer` data channels — all unmodified by this design.

## Platform scope

D-1 ships X11 only on the source side. The `DamageBackend` trait + `DamageCapability` enum + capability gating in `TilePolicy` mean Wayland and macOS slot in later without architectural change.

| Platform | D-1 status | Path forward |
|---|---|---|
| X11 | XDamage-driven (real OS damage) | First-class |
| Wayland (PipeWire) | Stub: `DamageCapability::None` → forces full snapshot every tick | D-4+ adds PipeWire damage metadata if exposed in our capture path |
| macOS (ScreenCaptureKit) | Stub: `DamageCapability::None` → forces full snapshot every tick | D-4+ spike: does ScreenCaptureKit expose dirty rects to clients? If yes, OsLevel impl. If no, FrameDiff impl. |
| Frame-diff fallback (any platform without OS damage) | Not in D-1 | D-4 adds a tile-hash diff backend for `DamageCapability::FrameDiff` |

`DamageCapability::None` is well-defined behavior: every tick, dirty fraction is treated as 1.0 → policy always returns `Video` → tile mode never engages, fallback video is used. This degrades gracefully — non-X11 platforms keep the existing VP8 path until their backend lands.

## Implementation slices

| slice | scope | exit criteria |
|---|---|---|
| **D-0** | This document, reviewed and locked. | Sign-off. Threshold values + slice boundaries + must-fix points agreed. |
| **D-1** | `display/capture/damage.rs` trait + X11 XDamage backend + `tile/grid.rs` + `tile/synthetic_dirty.rs` skeleton + unit tests. **Trace-only** — log per-tick `dirty_tile_count` and damage capability. No transport, no browser, no encoding. | `cargo test display::tile::*` passes. Trace output during xdotool sweep on peer X session shows non-trivial varying `dirty_tile_count` per tick + cursor-position synthetic dirty injection. Damage capability reports `OsLevel`. |
| **D-2** | `TileCompositor` class in `static/app.html` driven by a synthetic test stream (same wire format, generated by `static/diag/synthetic-tiles.js`). Renders 24×14 grid, applies snapshot-with-chunking + tile updates + epoch advance + resize. Raw BGRA encoding only. ChunkAssembler logic exercised. | Loading dashboard with `?tile-test=1` shows a synthetic moving-cursor + scrolling-text region rendering correctly at 30fps. Snapshot-after-resize works visibly. Visual-freshness sampler with marker rendered into the synthetic stream reports steady-state percentile triple comparable to today's video path. |
| **D-3** | Real peer tile stream over real WebRTC datachannels: `tile/transport.rs` wire-format encode + chunking + `webrtc.rs` data-channel registration + `display/mod.rs` integration of damage→grid→encode→transport. **Raw BGRA + RLE encodings only** (no WebP, no VP8-intra). Periodic 30s snapshot. `TilePolicy` always returns `Tiles` (no fallback yet). Cursor handled via `CursorState` overlay (Path A). | View display from primary's dashboard against peer; canvas renders peer's actual desktop with cursor + window updates. ICE/DTLS/datachannel state stable for ≥5 min. No tile-channel errors or unbounded peer CPU on idle desktop. |
| **D-4** | `tile/policy.rs` enabled (real fallback) + `FallbackToVideo`/`FallbackToTile` control messages + browser canvas/video toggle + WebP encoding (entropy-dispatched) + `GapReport` + ringbuffer recovery + explicit backpressure metrics/watermarks + Wayland/macOS frame-diff backend. Optional VP8-intra encoding gated behind a feature flag if browser support proves out. | Switching workloads on the peer (idle ↔ window drag ↔ video playback) triggers visible mode transitions; transitions stay below 1/sec under normal load. Snapshot recovery works after simulated channel drop. WebP encoded tiles render correctly when entropy threshold trips. |
| **D-5** | Visual-freshness comparison runs (q baseline / VP8-f baseline / tile-mode at full res) using the §9 harness. Use the `INTENDANT_DIAG=1` env hook + the run-script pattern from this session. Tune `ENTER_VIDEO_THRESHOLD`, `EXIT_VIDEO_THRESHOLD`, `MIN_DWELL_MS`, `MAX_DATACHANNEL_MESSAGE_SIZE`, watermark constants based on D-4 measurements. | Tile-mode passes #80 at full resolution: p50 ≤ 200ms, p95 ≤ 500ms, no freeze >1s, fps ≥ 15. **Tile-mode beats VP8-q on subjective usability (cursor latency, scroll latency, text editor responsiveness) while preserving VP8-q as the fallback baseline** — no regression to authority/input/federation work. smoke-display.md §9.5 updated with the comparison table. |

## Post-D5 Product Hardening

D-5 proved the X11 tile stream can satisfy the full-resolution freshness
bar. The remaining work is product hardening: make the path robust across
platforms and failure modes before treating tiles as the default display
experience everywhere.

| track | scope | success signal |
|---|---|---|
| **W-0/W-1: Wayland compatibility** | Run tile mode on the GNOME Wayland peer. Native Wayland should use the frame-diff damage path first; do not accidentally select XDamage from Xwayland. Portal approval may remain manual for the smoke. | Wayland peer reaches live full-resolution tile rendering after portal approval, or produces a clear portal/permission blocker with logs. |
| **M-0: macOS compatibility** | Decide whether ScreenCaptureKit exposes useful dirty rects. If not, use the same frame-diff fallback as Wayland. | macOS local DisplaySlot can run tile mode without losing the existing H.264 baseline. |
| **Defaulting and policy** | Decide when the dashboard should prefer tile canvas vs VP8-q video. Preserve VP8-q as fallback for high-motion or unsupported platforms. | Normal desktop work opens in full-resolution tile mode; high-motion content falls back without flapping. |
| **Observability** | Surface tile mode, dirty fraction, update bytes, dropped superseded deltas, snapshot sends, and fallback transitions in display metrics. | Operators can explain stutter or fallback from logs/metrics without attaching a debugger. |
| **Recovery drills** | Exercise snapshot throttling, GapReport replay, data-channel close/reopen, resize, and peer disconnect. | Corrupt or missing tiles self-heal within the bounded recovery window; no runaway snapshot loop. |
| **UX polish** | Make the canvas/video switch visually seamless; keep input coordinate mapping identical across modes; avoid diagnostic marker leaks when `?diag=1` is off. | A user cannot tell which rendering path is active except via debug metrics. |

## Open questions for review

1. **Tile size.** 64×64 vs 32×32 vs 128×128. Smaller = finer granularity but more per-frame overhead (more TileRecords, more decode calls). Larger = coarser (small dirty regions over-include surrounding pixels). 64 feels right; D-5 can re-tune if measurements suggest.
2. **Encoding heuristic for D-4.** Entropy-based dispatch (raw/RLE/WebP) vs always-RLE vs WebP-everywhere. D-3 measurements (CPU + bytes per encoding type) inform the D-4 dispatch policy.
3. **Datachannel reliability under WebKit edge cases.** Plan accepts `ordered: true, maxRetransmits: 0` as fallback if the unordered variant misbehaves on WebKit. Field testing required in D-3.
4. **Snapshot timing on `FallbackToTile` resume.** Send snapshot immediately on transition vs wait for next periodic. Immediate is more responsive but bursts the snapshot channel right after resume. Probably immediate with rate-limit honored (snapshot won't send if one was sent within `SNAPSHOT_MIN_INTERVAL`).
5. **macOS dirty-rect availability.** Whether ScreenCaptureKit exposes dirty rects to its clients in current macOS — needs a small spike before D-4 starts. Out of scope for D-1 (X11-only).
6. **Mixed-mode rendering** (tiles for static area + video for high-motion sub-region of the same frame) — interesting later optimization, deliberately out of scope for D-0 through D-5. Potential D-6 if there's still p99 latency to chase after D-5 lands.
7. **Compression of small tiles via combined encoding** (e.g. a frame's worth of TileRecords run through a single deflate stream for cross-tile redundancy). Not in v1; the per-tile encoding cost is bounded and the cross-tile redundancy is typically low for desktop content.

End of D-0 (revised). Pause for review per request. No code touched.
