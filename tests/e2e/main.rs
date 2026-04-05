//! E2E integration tests for intendant.
//!
//! These tests spawn the real `intendant` binary as a subprocess and interact
//! with it over WebSocket / control socket.  They require a built binary
//! (`cargo build --release` or `cargo build`) and may need a display backend
//! for display-related tests.
//!
//! Run:
//!   cargo test --test e2e -- --nocapture

mod webrtc_signaling;
