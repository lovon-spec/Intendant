//! Synthetic dirty-rect sources for things OS damage doesn't cover.
//!
//! Two cases require explicit dirty injection on top of OS damage:
//!
//! 1. **Cursor moves.** Many X servers render the cursor as a hardware
//!    overlay; XDamage doesn't fire on hw-cursor moves. The compositor
//!    sees a moved cursor but the underlying framebuffer didn't change
//!    from the damage system's perspective. Without injection, the
//!    cursor appears stuck at its last damage-reported position from
//!    the browser's view. **Verified during D-1 smoke**: a 10s
//!    `xdotool mousemove` sweep on the smoke peer (Debian/X11 in UTM)
//!    produced zero DamageNotify events. This injection is therefore
//!    not optional for cursor freshness — it's the only path that
//!    reports the cursor's position to the tile pipeline.
//! 2. **Visual-freshness diagnostic marker.** The marker is drawn
//!    INTO the captured frame buffer by Intendant code (not by the X
//!    server), so XDamage doesn't see it. The marker tile must be
//!    explicitly dirtied every time the marker value changes.
//!
//! D-1 ships the data structure and call surface. The actual wiring
//! (where the cursor poll happens, how marker changes are notified)
//! lands in D-3 when the integration with the capture pipeline starts.
//! For D-1 the example binary `examples/damage-trace.rs` exercises
//! both paths in trace mode.
//!
//! ## Cursor injection strategy
//!
//! For each cursor move, two synthetic dirty rects are emitted:
//! the rect around the OLD cursor position (so the area the cursor
//! left gets re-rendered without the cursor) and the rect around the
//! NEW cursor position (so the area the cursor now occupies gets
//! re-rendered with it). Both are sized
//! [`SyntheticDirtySources::cursor_radius_px`] in each direction
//! (square box, default 32px → 64px wide), which over-covers any
//! reasonable cursor sprite.
//!
//! D-4 may switch to a separate browser-side cursor sprite (Path A in
//! the design doc); when that lands, this synthetic-cursor injection
//! is disabled and `CursorState` frames carry the position instead.

use super::super::capture::damage::Rect;
use super::grid::{TileGrid, TileId};
use std::collections::BTreeSet;

/// Centralized injector for dirty rects that don't come from OS damage.
///
/// Owned by the per-display capture orchestrator (D-3). Stateless across
/// configuration but holds the most recent cursor position so it can
/// emit the leave-area rect on the next move.
pub struct SyntheticDirtySources {
    /// Last reported cursor position (`None` until first report).
    last_cursor: Option<(i32, i32)>,
    /// Half-side of the synthetic dirty box around the cursor, in pixels.
    /// Default 32 → 64×64 box (one full tile worth at default tile size).
    cursor_radius_px: u32,
    /// Whether the visual-freshness marker is currently enabled. When
    /// `true`, [`SyntheticDirtySources::marker_changed`] returns the
    /// marker tile rect; when `false`, returns nothing.
    marker_enabled: bool,
    /// Top-left pixel of the marker on screen. The marker tile is
    /// `tile_size_px` square starting at this point.
    marker_origin: (u32, u32),
    /// Side of the marker tile in pixels.
    marker_size_px: u32,
}

impl SyntheticDirtySources {
    /// Construct with default cursor radius (32px) and marker disabled.
    pub fn new() -> Self {
        Self {
            last_cursor: None,
            cursor_radius_px: 32,
            marker_enabled: false,
            marker_origin: (0, 0),
            marker_size_px: 64,
        }
    }

    /// Override the cursor synthetic-dirty radius. `radius_px` is the
    /// half-side of the dirty box. Default 32 (= 64×64 box).
    pub fn with_cursor_radius(mut self, radius_px: u32) -> Self {
        self.cursor_radius_px = radius_px;
        self
    }

    /// Configure the visual-freshness marker tile location and size.
    /// The marker is initially disabled regardless of this call —
    /// use [`Self::set_marker_enabled`] to flip it.
    pub fn with_marker(mut self, origin_px: (u32, u32), size_px: u32) -> Self {
        self.marker_origin = origin_px;
        self.marker_size_px = size_px;
        self
    }

    /// Enable/disable marker injection. Mirrors the daemon's
    /// `SetDiagnosticsVisualMarker` ControlMsg state. When disabled,
    /// `marker_changed()` returns nothing regardless of input.
    pub fn set_marker_enabled(&mut self, enabled: bool) {
        self.marker_enabled = enabled;
    }

    /// Notify of a cursor position. Returns the dirty rects for this
    /// move (one for the leave-area, one for the new-area), or empty
    /// if the position hasn't changed since the last report. The first
    /// report after construction returns just the new-area rect (no
    /// leave-area because there's no previous position).
    pub fn cursor_moved(&mut self, new_pos: (i32, i32)) -> Vec<Rect> {
        let mut rects = Vec::with_capacity(2);
        if let Some(old) = self.last_cursor {
            if old == new_pos {
                return rects; // no movement, no dirty
            }
            rects.push(self.cursor_box(old));
        }
        rects.push(self.cursor_box(new_pos));
        self.last_cursor = Some(new_pos);
        rects
    }

    /// Reset the cursor tracking state (e.g. on display resize). The
    /// next `cursor_moved` after a reset will only emit the new-area
    /// rect, identical to first-call behavior.
    pub fn reset_cursor(&mut self) {
        self.last_cursor = None;
    }

    /// Notify that the visual-freshness marker value changed. Returns
    /// the marker tile rect if the marker is enabled, empty otherwise.
    pub fn marker_changed(&self) -> Vec<Rect> {
        if !self.marker_enabled {
            return Vec::new();
        }
        vec![Rect::new(
            self.marker_origin.0 as i32,
            self.marker_origin.1 as i32,
            self.marker_size_px,
            self.marker_size_px,
        )]
    }

    /// Convenience: collect every synthetic dirty source in one call.
    /// `cursor_pos` is the most recent observed cursor position; pass
    /// `None` if no cursor poll happened this tick. `marker_changed`
    /// is `true` if the marker value has been updated since the last
    /// collect; the caller is responsible for tracking that state
    /// (typically by watching `visual_marker_value` for changes).
    ///
    /// Returns the union of cursor-move rects and marker-tile rect.
    /// Order is cursor-leave, cursor-new, marker.
    pub fn collect(&mut self, cursor_pos: Option<(i32, i32)>, marker_changed: bool) -> Vec<Rect> {
        let mut out = Vec::new();
        if let Some(pos) = cursor_pos {
            out.extend(self.cursor_moved(pos));
        }
        if marker_changed {
            out.extend(self.marker_changed());
        }
        out
    }

    /// Convenience for testing/tracing: collect synthetic rects AND
    /// the tiles they map to under the given grid. Returns
    /// `(rects, tile_set)`.
    pub fn collect_into_tiles(
        &mut self,
        grid: &TileGrid,
        cursor_pos: Option<(i32, i32)>,
        marker_changed: bool,
    ) -> (Vec<Rect>, BTreeSet<TileId>) {
        let rects = self.collect(cursor_pos, marker_changed);
        let tiles = grid.dirty_tiles(&rects);
        (rects, tiles)
    }

    fn cursor_box(&self, (cx, cy): (i32, i32)) -> Rect {
        let r = self.cursor_radius_px as i32;
        let side = (self.cursor_radius_px * 2) as u32;
        Rect::new(cx - r, cy - r, side, side)
    }
}

impl Default for SyntheticDirtySources {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_cursor_move_emits_only_new_area() {
        let mut s = SyntheticDirtySources::new();
        let rects = s.cursor_moved((100, 100));
        assert_eq!(rects.len(), 1);
        // 32-radius default: box is 64×64 centered on (100, 100) = (68, 68)..(132, 132)
        assert_eq!(rects[0], Rect::new(68, 68, 64, 64));
    }

    #[test]
    fn subsequent_cursor_move_emits_leave_and_new() {
        let mut s = SyntheticDirtySources::new();
        let _ = s.cursor_moved((100, 100));
        let rects = s.cursor_moved((200, 150));
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], Rect::new(68, 68, 64, 64)); // leave
        assert_eq!(rects[1], Rect::new(168, 118, 64, 64)); // new
    }

    #[test]
    fn cursor_no_movement_yields_no_dirty() {
        let mut s = SyntheticDirtySources::new();
        let _ = s.cursor_moved((100, 100));
        let rects = s.cursor_moved((100, 100));
        assert!(rects.is_empty());
    }

    #[test]
    fn cursor_reset_clears_history() {
        let mut s = SyntheticDirtySources::new();
        let _ = s.cursor_moved((100, 100));
        s.reset_cursor();
        let rects = s.cursor_moved((150, 150));
        // After reset, behaves like a first move — only new-area.
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], Rect::new(118, 118, 64, 64));
    }

    #[test]
    fn cursor_radius_override_resizes_box() {
        let mut s = SyntheticDirtySources::new().with_cursor_radius(16);
        let rects = s.cursor_moved((100, 100));
        // 16-radius: 32×32 box = (84, 84)..(116, 116)
        assert_eq!(rects[0], Rect::new(84, 84, 32, 32));
    }

    #[test]
    fn cursor_near_top_left_emits_negative_coords() {
        // Synthetic source doesn't clamp — TileGrid handles that.
        // Verifies the cursor box is emitted as-is so the partition
        // step gets the math.
        let mut s = SyntheticDirtySources::new();
        let rects = s.cursor_moved((10, 10));
        assert_eq!(rects[0], Rect::new(-22, -22, 64, 64));
    }

    #[test]
    fn marker_disabled_yields_no_rect() {
        let s = SyntheticDirtySources::new().with_marker((0, 0), 64);
        // marker_enabled defaults to false even after with_marker
        assert!(s.marker_changed().is_empty());
    }

    #[test]
    fn marker_enabled_yields_marker_tile_rect() {
        let mut s = SyntheticDirtySources::new().with_marker((128, 64), 64);
        s.set_marker_enabled(true);
        let rects = s.marker_changed();
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], Rect::new(128, 64, 64, 64));
    }

    #[test]
    fn collect_unions_cursor_and_marker() {
        let mut s = SyntheticDirtySources::new().with_marker((0, 0), 64);
        s.set_marker_enabled(true);
        let rects = s.collect(Some((100, 100)), true);
        // First cursor move = 1 rect; marker = 1 rect; total 2.
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn collect_no_cursor_no_marker_change_yields_empty() {
        let mut s = SyntheticDirtySources::new();
        let rects = s.collect(None, false);
        assert!(rects.is_empty());
    }

    #[test]
    fn collect_into_tiles_partitions_correctly() {
        let mut s = SyntheticDirtySources::new().with_marker((0, 0), 64);
        s.set_marker_enabled(true);
        let g = TileGrid::new(1024, 768, 64).unwrap();
        let (_rects, tiles) = s.collect_into_tiles(&g, Some((100, 100)), true);
        // Cursor box (68, 68)..(132, 132): tiles (1, 1), (2, 1), (1, 2), (2, 2)
        // Marker tile at (0, 0)..(64, 64): tile (0, 0)
        // First move so no leave rect.
        assert_eq!(tiles.len(), 5);
        assert!(tiles.contains(&TileId::new(0, 0)));
        assert!(tiles.contains(&TileId::new(1, 1)));
        assert!(tiles.contains(&TileId::new(2, 1)));
        assert!(tiles.contains(&TileId::new(1, 2)));
        assert!(tiles.contains(&TileId::new(2, 2)));
    }
}
