//! Tile grid: maps arbitrary screen rectangles to a fixed-size tile
//! coordinate set.
//!
//! The grid is always *square tiles* of [`TileGrid::tile_size_px`]
//! pixels (default 64). The grid covers the full screen — for screens
//! whose dimensions aren't an exact multiple of the tile size, the
//! grid extends one tile past the screen edge and partial tiles at
//! the right/bottom edges are encoded with the same padded dimensions
//! (the encoder pads, the renderer crops).
//!
//! ## Over-detection by design
//!
//! A rect that crosses tile boundaries (which is almost every rect)
//! marks every tile it touches as dirty. A 1-pixel rect at coord
//! `(63, 63)` with `tile_size = 64` marks tile `(0, 0)` as dirty,
//! not tile `(0, 0)` and `(1, 1)` — but a 2-pixel rect crossing the
//! boundary marks all four neighboring tiles. This is intentional:
//! the encoder always works at tile granularity, and the resulting
//! marginal over-encoding is cheaper than per-pixel dirty tracking.
//!
//! ## Bounded by screen, not by infinity
//!
//! Rects with negative coordinates or extending past the screen
//! bounds are clamped to the on-screen area before partitioning.
//! Rects entirely off-screen produce no dirty tiles. Rects with
//! zero area (`is_empty()`) produce no dirty tiles.

use super::super::capture::damage::Rect;
use std::collections::BTreeSet;

/// Identifies one tile in the grid as `(tile_x, tile_y)` (grid coords,
/// not pixel coords). Comparable + ordered for deterministic iteration
/// in tests and trace output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileId {
    pub x: u16,
    pub y: u16,
}

impl TileId {
    pub fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

/// Fixed-size square tile grid covering a screen of given dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileGrid {
    pub tile_size_px: u16,
    /// Number of tiles horizontally. Computed as
    /// `ceil(screen_w / tile_size_px)` so the grid always covers the
    /// full screen.
    pub width_tiles: u16,
    /// Number of tiles vertically. Computed analogously.
    pub height_tiles: u16,
    /// Screen width in pixels — kept for clamping rects to the
    /// on-screen area.
    pub screen_w_px: u32,
    /// Screen height in pixels.
    pub screen_h_px: u32,
}

impl TileGrid {
    /// Construct a grid covering the given screen with the given tile
    /// size. Tile size must be > 0; screen dimensions must be > 0.
    /// Returns `None` for invalid input.
    pub fn new(screen_w_px: u32, screen_h_px: u32, tile_size_px: u16) -> Option<Self> {
        if tile_size_px == 0 || screen_w_px == 0 || screen_h_px == 0 {
            return None;
        }
        let ts = tile_size_px as u32;
        // ceil-div without overflow risk for plausible screen sizes
        // (max 64K × 64K still fits in u32 with 1px tiles).
        let width_tiles = ((screen_w_px + ts - 1) / ts) as u16;
        let height_tiles = ((screen_h_px + ts - 1) / ts) as u16;
        Some(Self {
            tile_size_px,
            width_tiles,
            height_tiles,
            screen_w_px,
            screen_h_px,
        })
    }

    /// Total tile count. Useful for computing dirty fraction.
    pub fn total_tiles(&self) -> usize {
        self.width_tiles as usize * self.height_tiles as usize
    }

    /// Map an arbitrary set of dirty rects into the set of tiles they
    /// overlap. Idempotent and deterministic — same input always
    /// produces the same `BTreeSet`. Off-screen and empty rects
    /// contribute nothing.
    pub fn dirty_tiles(&self, rects: &[Rect]) -> BTreeSet<TileId> {
        let mut out = BTreeSet::new();
        for r in rects {
            self.add_rect_tiles(r, &mut out);
        }
        out
    }

    /// Compute the dirty fraction as `dirty_count / total_tiles`,
    /// in `[0.0, 1.0]`.
    pub fn dirty_fraction(&self, dirty_count: usize) -> f32 {
        let total = self.total_tiles();
        if total == 0 {
            return 0.0;
        }
        let ratio = dirty_count as f32 / total as f32;
        // Clamp because the caller might sum overlapping per-rect
        // counts before passing in (the API doesn't enforce uniqueness).
        ratio.clamp(0.0, 1.0)
    }

    /// Add the tile coordinates of one rect to the output set.
    fn add_rect_tiles(&self, r: &Rect, out: &mut BTreeSet<TileId>) {
        if r.is_empty() {
            return;
        }

        // Clamp to on-screen area. We do this in i64 to avoid
        // signed/unsigned mishaps when a rect has negative coordinates.
        let rx0 = r.x as i64;
        let ry0 = r.y as i64;
        let rx1 = rx0 + r.width as i64;
        let ry1 = ry0 + r.height as i64;
        let sw = self.screen_w_px as i64;
        let sh = self.screen_h_px as i64;

        // Intersect with screen rect [0, sw) × [0, sh).
        let x0 = rx0.max(0);
        let y0 = ry0.max(0);
        let x1 = rx1.min(sw);
        let y1 = ry1.min(sh);

        // Entirely off-screen or zero-area after clamp.
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let ts = self.tile_size_px as i64;
        let tile_x0 = (x0 / ts) as u16;
        let tile_y0 = (y0 / ts) as u16;
        // Inclusive last tile: ceil-div of x1 by ts, minus 1.
        // x1 > x0 ≥ 0 here, so (x1 - 1) / ts is safe.
        let tile_x1 = ((x1 - 1) / ts) as u16;
        let tile_y1 = ((y1 - 1) / ts) as u16;

        for ty in tile_y0..=tile_y1 {
            for tx in tile_x0..=tile_x1 {
                out.insert(TileId::new(tx, ty));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid64(w: u32, h: u32) -> TileGrid {
        TileGrid::new(w, h, 64).expect("valid grid")
    }

    #[test]
    fn invalid_dimensions_return_none() {
        assert!(TileGrid::new(0, 100, 64).is_none());
        assert!(TileGrid::new(100, 0, 64).is_none());
        assert!(TileGrid::new(100, 100, 0).is_none());
    }

    #[test]
    fn grid_dimensions_round_up() {
        // 1920x1080 with 64px tiles → 30 × 17 (1088 covers 1080).
        let g = grid64(1920, 1080);
        assert_eq!(g.width_tiles, 30);
        assert_eq!(g.height_tiles, 17);
        assert_eq!(g.total_tiles(), 510);
        // 1024x768 with 64px tiles → 16 × 12 exact.
        let g = grid64(1024, 768);
        assert_eq!(g.width_tiles, 16);
        assert_eq!(g.height_tiles, 12);
        // 1361x769 (odd) with 64px tiles → 22 × 13 (1408 × 832 cover).
        let g = grid64(1361, 769);
        assert_eq!(g.width_tiles, 22);
        assert_eq!(g.height_tiles, 13);
    }

    #[test]
    fn empty_input_yields_empty_set() {
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[]);
        assert!(dirty.is_empty());
    }

    #[test]
    fn zero_area_rect_yields_empty_set() {
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[
            Rect::new(100, 100, 0, 50),
            Rect::new(100, 100, 50, 0),
            Rect::new(0, 0, 0, 0),
        ]);
        assert!(dirty.is_empty());
    }

    #[test]
    fn aligned_single_tile() {
        // Rect exactly inside tile (0, 0).
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[Rect::new(0, 0, 64, 64)]);
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains(&TileId::new(0, 0)));
    }

    #[test]
    fn rect_crossing_one_boundary_marks_two_tiles() {
        // Rect from (60, 0) to (68, 8) crosses x=64 boundary.
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[Rect::new(60, 0, 8, 8)]);
        assert_eq!(dirty.len(), 2);
        assert!(dirty.contains(&TileId::new(0, 0)));
        assert!(dirty.contains(&TileId::new(1, 0)));
    }

    #[test]
    fn rect_crossing_corner_marks_four_tiles() {
        // Rect spanning the (64, 64) intersection.
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[Rect::new(60, 60, 8, 8)]);
        assert_eq!(dirty.len(), 4);
        assert!(dirty.contains(&TileId::new(0, 0)));
        assert!(dirty.contains(&TileId::new(1, 0)));
        assert!(dirty.contains(&TileId::new(0, 1)));
        assert!(dirty.contains(&TileId::new(1, 1)));
    }

    #[test]
    fn full_screen_rect_marks_every_tile() {
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[Rect::new(0, 0, 1024, 768)]);
        assert_eq!(dirty.len(), g.total_tiles());
    }

    #[test]
    fn rect_at_screen_edge_partial_tile() {
        // Screen 1361×769 with 64px tiles → grid 22×13.
        // Rightmost column of tiles is partial: pixels 1344..1361 (17 wide).
        let g = grid64(1361, 769);
        let dirty = g.dirty_tiles(&[Rect::new(1350, 760, 5, 5)]);
        // 1350 is in tile_x = 21 (1344..1408), 760 in tile_y = 11 (704..768).
        // 765 < 769 still on-screen, so tile_y = 11.
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains(&TileId::new(21, 11)));
    }

    #[test]
    fn rect_extending_off_screen_clamped() {
        // Rect starts on-screen, extends past right edge. Should mark
        // only the on-screen tiles.
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[Rect::new(1000, 0, 200, 8)]);
        // 1000 in tile_x = 15. Clamped right edge is x = 1024 (excl) → tile 15.
        // So only one tile.
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains(&TileId::new(15, 0)));
    }

    #[test]
    fn rect_entirely_off_screen_yields_empty() {
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[
            Rect::new(2000, 0, 100, 100),    // right of screen
            Rect::new(0, 1000, 100, 100),    // below screen
            Rect::new(-200, -200, 100, 100), // top-left of screen
        ]);
        assert!(dirty.is_empty());
    }

    #[test]
    fn rect_with_negative_origin_clamped() {
        // Rect at (-10, -10) with size 80×80 → on-screen portion is
        // (0, 0) to (70, 70). All within tile (0, 0) → 1x1 partial tile.
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[Rect::new(-10, -10, 80, 80)]);
        // Clamped to (0..70) × (0..70). tile_x0=0, tile_x1 = (70-1)/64 = 1.
        // Same for y. So 4 tiles: (0,0), (1,0), (0,1), (1,1).
        assert_eq!(dirty.len(), 4);
    }

    #[test]
    fn deduplication_across_rects() {
        // Two overlapping rects that touch the same tiles should still
        // produce each tile once.
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[
            Rect::new(0, 0, 128, 128), // tiles (0..2, 0..2) = 4
            Rect::new(50, 50, 50, 50), // tiles (0..2, 0..2) subset
        ]);
        assert_eq!(dirty.len(), 4);
    }

    #[test]
    fn dirty_fraction_bounds() {
        let g = grid64(1024, 768);
        let total = g.total_tiles(); // 192
        assert_eq!(g.dirty_fraction(0), 0.0);
        assert!((g.dirty_fraction(total) - 1.0).abs() < f32::EPSILON);
        assert!((g.dirty_fraction(total / 2) - 0.5).abs() < 0.01);
        // Over-count clamps.
        assert_eq!(g.dirty_fraction(total * 3), 1.0);
    }

    #[test]
    fn deterministic_iteration_order() {
        // BTreeSet → ordered by (x, y) lex. Verify this property
        // because trace output relies on it for stable diffs.
        let g = grid64(1024, 768);
        let dirty = g.dirty_tiles(&[
            Rect::new(500, 500, 8, 8), // (7, 7)
            Rect::new(10, 10, 8, 8),   // (0, 0)
            Rect::new(500, 10, 8, 8),  // (7, 0)
        ]);
        let ordered: Vec<_> = dirty.iter().copied().collect();
        assert_eq!(ordered.len(), 3);
        // BTreeSet's Ord on TileId compares by (x, y) — derived order
        // from the field declaration order.
        assert_eq!(ordered[0], TileId::new(0, 0));
        assert_eq!(ordered[1], TileId::new(7, 0));
        assert_eq!(ordered[2], TileId::new(7, 7));
    }
}
