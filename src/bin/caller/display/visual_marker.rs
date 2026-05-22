//! Diagnostic visual marker for federated display freshness measurement.
//!
//! Stamps a 32-bit value into the top-left corner of an I420 Y plane as an
//! 8×4 grid of 16×16-pixel binary tiles (128×64 px total). The browser-side
//! sampler reads the same tiles via `requestVideoFrameCallback` + canvas
//! `getImageData`, decodes the value, and tracks transitions to measure
//! visual freshness (effective fps, freeze intervals) without depending on
//! getStats packet counters that proved misleading on task #81.
//!
//! ## Encoding
//!
//! - 32 tiles total: bit `b ∈ 0..32` lives at row `b / COLS`, col `b % COLS`,
//!   with bit 0 at the top-left tile and bit 31 at the bottom-right.
//! - Each tile is `TILE_PX × TILE_PX` luma pixels. 16-px squares align with
//!   VP8/H.264 16×16 macroblock boundaries, which keeps the binary
//!   transitions on quantizer-friendly seams.
//! - High bit → `LUMA_HIGH` (235), low bit → `LUMA_LOW` (16). The 16/235
//!   limited-range pair leaves headroom on both sides for in-loop filter
//!   ringing without crossing the decode-side threshold of 128.
//!
//! Only the Y plane is stamped; U/V are left untouched. The tile pixels
//! decode as monochrome on the browser side regardless of chroma — the
//! sampler reads RGB and computes luminance, so chroma drift doesn't
//! affect classification.
//!
//! ## Boundary handling
//!
//! [`stamp_y_plane`] is a no-op when `width < MARKER_W` or `height < MARKER_H`.
//! That keeps tiny offscreen / fallback displays (e.g. 64×64 placeholder
//! during a portal-startup transition) from panicking, at the cost of no
//! marker until the display reaches the minimum size.
//!
//! ## Why the Y plane and not BGRA pre-conversion
//!
//! The capture pipeline owns BGRA frames behind `Arc<Frame>` (shared with
//! the FrameRegistry and the broadcast subscribers); mutating in place
//! would require a clone or a refactor. The I420 buffer is freshly
//! produced by [`crate::display::encode::bgra_to_i420`] and immediately
//! moved into the bridge's `latest_i420` cache — modifying it before the
//! `Arc::new` is the cheapest correct hook point.
//!
//! ## Compatibility with downscale layers
//!
//! Simulcast layer pool also calls `downscale_i420`. The marker pixels
//! survive box-downscale to f / h / q resolutions because the tile area
//! (16×16) is much larger than the downscale ratio's source-pixel
//! footprint per dest-pixel. At quarter resolution (q layer), each tile
//! becomes a 4×4 source patch averaged into a single dest pixel, which
//! still resolves to ~16 or ~235 luma cleanly.

/// Edge length of one binary tile in luma pixels. Aligns with VP8 / H.264
/// 16×16 macroblock boundaries so a tile's quantization-friendly seam
/// lands on a coding-block boundary.
pub const TILE_PX: usize = 16;

/// Number of tile columns. 8 cols × 4 rows = 32 tiles = 32 bits.
pub const COLS: usize = 8;

/// Number of tile rows.
pub const ROWS: usize = 4;

/// Marker patch width in luma pixels.
pub const MARKER_W: usize = TILE_PX * COLS; // 128

/// Marker patch height in luma pixels.
pub const MARKER_H: usize = TILE_PX * ROWS; // 64

/// Luma value written for a `0` bit. Limited-range black with deblock-
/// filter headroom; survives codec ringing without crossing the
/// decode-side classification threshold.
pub const LUMA_LOW: u8 = 16;

/// Luma value written for a `1` bit. Limited-range white symmetric to
/// [`LUMA_LOW`].
pub const LUMA_HIGH: u8 = 235;

/// Browser-side classification threshold. Any sampled luma `>= THRESHOLD`
/// decodes as a `1` bit; below decodes as `0`. The Rust decoder
/// [`decode_y_plane`] uses the same threshold for symmetry with the
/// expected JS path, even though it's reading uncompressed source pixels.
pub const THRESHOLD: u8 = 128;

/// Stamp the lower 32 bits of `value` into the top-left of `y_plane`.
///
/// `y_plane` is the Y component of an I420 buffer, row-major, no padding,
/// dimensions `width × height`. Out-of-range buffers (smaller than the
/// marker patch) are silently skipped; oversized buffers are stamped only
/// in the top-left [`MARKER_W`] × [`MARKER_H`] region.
///
/// Bit `b` (0 = LSB) lands at tile (row = b / COLS, col = b % COLS),
/// covering luma pixels `(col*TILE_PX..(col+1)*TILE_PX, row*TILE_PX..(row+1)*TILE_PX)`.
///
/// # Panics
///
/// Does not panic on small or short slices — explicit bounds check
/// ensures graceful no-op when `y_plane.len() < width * MARKER_H` or the
/// declared `width`/`height` are smaller than the marker patch.
pub fn stamp_y_plane(y_plane: &mut [u8], width: usize, height: usize, value: u32) {
    if width < MARKER_W || height < MARKER_H {
        return;
    }
    if y_plane.len() < width * MARKER_H {
        // Caller passed a slice shorter than the declared dimensions
        // imply (e.g. truncated buffer). Skip rather than write past
        // the end.
        return;
    }
    for row in 0..ROWS {
        for col in 0..COLS {
            let bit_idx = row * COLS + col;
            let bit = (value >> bit_idx) & 1;
            let luma = if bit == 1 { LUMA_HIGH } else { LUMA_LOW };
            for ty in 0..TILE_PX {
                let py = row * TILE_PX + ty;
                let row_start = py * width + col * TILE_PX;
                y_plane[row_start..row_start + TILE_PX].fill(luma);
            }
        }
    }
}

/// Decode the marker by sampling each tile's center pixel and
/// thresholding at [`THRESHOLD`]. Returns `Some(value)` when the buffer
/// is large enough to hold the full marker, `None` otherwise.
///
/// Mirror of [`stamp_y_plane`] for round-trip tests and for any
/// future Rust-side replay tool that needs to read the marker out of
/// an I420 buffer (e.g. analyzing recorded captures).
pub fn decode_y_plane(y_plane: &[u8], width: usize, height: usize) -> Option<u32> {
    if width < MARKER_W || height < MARKER_H {
        return None;
    }
    if y_plane.len() < width * MARKER_H {
        return None;
    }
    let mut value: u32 = 0;
    for row in 0..ROWS {
        for col in 0..COLS {
            let bit_idx = row * COLS + col;
            // Sample the tile center to dodge any 1-px misalignment
            // from downscale or fractional-luma rounding at edges.
            let cx = col * TILE_PX + TILE_PX / 2;
            let cy = row * TILE_PX + TILE_PX / 2;
            let sample = y_plane[cy * width + cx];
            if sample >= THRESHOLD {
                value |= 1 << bit_idx;
            }
        }
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc_y(width: usize, height: usize) -> Vec<u8> {
        vec![64u8; width * height] // mid-gray background
    }

    #[test]
    fn round_trip_zero() {
        let mut y = alloc_y(MARKER_W, MARKER_H);
        stamp_y_plane(&mut y, MARKER_W, MARKER_H, 0);
        assert_eq!(decode_y_plane(&y, MARKER_W, MARKER_H), Some(0));
    }

    #[test]
    fn round_trip_all_bits() {
        let mut y = alloc_y(MARKER_W, MARKER_H);
        stamp_y_plane(&mut y, MARKER_W, MARKER_H, 0xFFFF_FFFF);
        assert_eq!(decode_y_plane(&y, MARKER_W, MARKER_H), Some(0xFFFF_FFFF));
    }

    #[test]
    fn round_trip_arbitrary() {
        for value in [
            1u32,
            0xDEAD_BEEF,
            0x1234_5678,
            0xFFFF_0000,
            0x0000_FFFF,
            0xAAAA_AAAA,
            0x5555_5555,
            42,
            (u32::MAX / 7),
        ] {
            let mut y = alloc_y(MARKER_W, MARKER_H);
            stamp_y_plane(&mut y, MARKER_W, MARKER_H, value);
            assert_eq!(
                decode_y_plane(&y, MARKER_W, MARKER_H),
                Some(value),
                "round-trip failed for value 0x{value:08x}"
            );
        }
    }

    #[test]
    fn oversized_buffer_only_top_left_touched() {
        let w = 1360;
        let h = 768;
        let mut y = alloc_y(w, h);
        stamp_y_plane(&mut y, w, h, 0xCAFE_BABE);
        assert_eq!(decode_y_plane(&y, w, h), Some(0xCAFE_BABE));
        // Pixels outside the marker patch must be unchanged.
        let untouched_lum = 64u8;
        // Far corner.
        assert_eq!(y[(h - 1) * w + (w - 1)], untouched_lum);
        // Just past the marker's right edge, on its top row (row 0, col MARKER_W).
        assert_eq!(y[MARKER_W], untouched_lum);
        // Just past the marker's bottom edge, on its left column (row MARKER_H, col 0).
        assert_eq!(y[MARKER_H * w], untouched_lum);
        // One row before that, just past the right edge: also untouched.
        assert_eq!(y[(MARKER_H - 1) * w + MARKER_W], untouched_lum);
    }

    #[test]
    fn too_small_is_noop() {
        // Smaller than marker → stamp must not panic and must not write.
        let small_w = MARKER_W - 1;
        let small_h = MARKER_H;
        let mut y = alloc_y(small_w, small_h);
        let untouched = y.clone();
        stamp_y_plane(&mut y, small_w, small_h, 0x1234_5678);
        assert_eq!(y, untouched, "stamp must be no-op when width too small");
        assert_eq!(decode_y_plane(&y, small_w, small_h), None);

        let small_h2 = MARKER_H - 1;
        let mut y2 = alloc_y(MARKER_W, small_h2);
        let untouched2 = y2.clone();
        stamp_y_plane(&mut y2, MARKER_W, small_h2, 0x1234_5678);
        assert_eq!(y2, untouched2, "stamp must be no-op when height too small");
    }

    #[test]
    fn truncated_slice_is_noop() {
        // Slice shorter than declared dims → no panic, no write.
        let w = MARKER_W;
        let h = MARKER_H;
        let mut y = vec![64u8; w * (h - 1)]; // one row short
        let untouched = y.clone();
        stamp_y_plane(&mut y, w, h, 0xDEAD_BEEF);
        assert_eq!(y, untouched);
    }

    #[test]
    fn quantization_tolerance_decode_with_noise() {
        // Stamp + add ±60 luma noise (well within the 16/235 ↔ 128
        // headroom). Decode must still return the original value. This
        // mimics the codec quantization + ringing the marker survives
        // on a real wire.
        let mut y = alloc_y(MARKER_W, MARKER_H);
        let value = 0x1357_9BDFu32;
        stamp_y_plane(&mut y, MARKER_W, MARKER_H, value);
        // Apply deterministic noise: bias + alternate per pixel.
        for (i, px) in y.iter_mut().enumerate() {
            let noise: i16 = if i % 2 == 0 { 60 } else { -60 };
            let new = (*px as i16 + noise).clamp(0, 255) as u8;
            *px = new;
        }
        assert_eq!(decode_y_plane(&y, MARKER_W, MARKER_H), Some(value));
    }

    #[test]
    fn bit_layout_is_lsb_first_top_left() {
        // Bit 0 (LSB) must end up at tile (row=0, col=0), bit 31 at
        // tile (row=ROWS-1, col=COLS-1). This locks the wire format —
        // browser sampler must agree on the same orientation.
        let mut y = alloc_y(MARKER_W, MARKER_H);
        stamp_y_plane(&mut y, MARKER_W, MARKER_H, 0x0000_0001);
        // Bit 0 = top-left tile center should be HIGH.
        assert!(y[(TILE_PX / 2) * MARKER_W + (TILE_PX / 2)] >= THRESHOLD);
        // All other bits = 0, so e.g. bit 1's tile center should be LOW.
        let bit1_cx = TILE_PX + TILE_PX / 2; // col 1, x center
        let bit1_cy = TILE_PX / 2;
        assert!(y[bit1_cy * MARKER_W + bit1_cx] < THRESHOLD);

        let mut y2 = alloc_y(MARKER_W, MARKER_H);
        stamp_y_plane(&mut y2, MARKER_W, MARKER_H, 1u32 << 31);
        // Bit 31 = bottom-right tile (row 3, col 7).
        let cx = (COLS - 1) * TILE_PX + TILE_PX / 2;
        let cy = (ROWS - 1) * TILE_PX + TILE_PX / 2;
        assert!(y2[cy * MARKER_W + cx] >= THRESHOLD);
    }
}
