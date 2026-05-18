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
//! - [`backpressure`] — D-4c event-driven watermarks for supersedable
//!   tile-delta frames.
//! - [`recovery`] — D-4d bounded replay buffer for recent tile
//!   updates used by gap recovery.
//!
//! D-3 wires these into the encode + transport path; D-1 stays
//! consumable only via direct calls (and the trace-only example
//! binary `examples/damage-trace.rs`).

pub mod backpressure;
pub mod encode;
pub mod grid;
pub mod policy;
pub mod recovery;
pub mod synthetic_dirty;
pub mod transport;
