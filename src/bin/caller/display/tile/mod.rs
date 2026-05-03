//! Tile-based display streaming (#82, D-1).
//!
//! Library scaffolding only in D-1 — no transport, no encoder, no
//! browser code, no integration with the existing capture pipeline.
//! See `docs/design-tile-streaming.md` for the full architecture.
//!
//! D-1 contents:
//! - [`grid::TileGrid`] — partitions arbitrary screen rects into a
//!   fixed-size grid of tile coordinates.
//! - [`synthetic_dirty::SyntheticDirtySources`] — injects synthetic
//!   dirty rects for things OS damage doesn't cover (cursor moves,
//!   the visual-freshness diagnostic marker).
//! - [`transport`] — D-3a binary wire-frame encode/decode helpers for
//!   snapshot chunks, tile updates, and control frames.
//!
//! D-3 wires these into the encode + transport path; D-1 stays
//! consumable only via direct calls (and the trace-only example
//! binary `examples/damage-trace.rs`).

pub mod grid;
pub mod encode;
pub mod policy;
pub mod synthetic_dirty;
pub mod transport;
