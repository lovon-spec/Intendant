//! X11 XDamage-based [`DamageBackend`] implementation.
//!
//! Subscribes to root-window damage with `ReportLevel::BoundingBox` —
//! one event per non-empty transition carrying the bounding box of
//! the damaged region. After polling we issue `damage_subtract` to
//! clear the accumulated region so the next mutation re-fires.
//!
//! ## Why BoundingBox and not RawRectangles
//!
//! `BoundingBox` gives one event per dirty interval at the cost of
//! over-detection (the bbox covers everything between the two extremes
//! of damage). For D-1 trace-only that's the right granularity:
//! coarse but cheap, easy to verify visually during an `xdotool` sweep.
//! D-3 may switch to `RawRectangles` if the over-detection costs too
//! many tile re-encodes per frame — that decision is empirical and
//! belongs to the integration slice, not D-1.
//!
//! ## Why this connection is independent of the XShm capture connection
//!
//! XDamage subscriptions are per-X11-connection. Sharing a connection
//! between the capture thread and a damage observer would mean
//! interleaving XShm + Damage events on one event queue, which
//! complicates both code paths. Each backend opening its own connection
//! is the documented pattern, and the X server happily accepts multiple
//! Damage subscriptions on the same drawable.

use super::damage::{DamageBackend, DamageCapability, DamageError, Rect};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt, ReportLevel};
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesExt};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// X11 XDamage-backed damage tracker. One per display connection.
pub struct X11DamageBackend {
    conn: RustConnection,
    damage_id: damage::Damage,
    geometry: (u32, u32),
}

impl X11DamageBackend {
    /// Connect to the given DISPLAY string and set up an XDamage
    /// subscription on the root window. Returns
    /// [`DamageError::ExtensionMissing`] if either DAMAGE or XFIXES is
    /// unavailable on the X server — caller should fall back to a
    /// [`super::damage::NullDamageBackend`] in that case.
    pub fn new(display_str: &str) -> Result<Self, DamageError> {
        let (conn, screen_num) = x11rb::connect(Some(display_str))
            .map_err(|e| DamageError::Connect(e.to_string()))?;

        // Verify both required extensions are present BEFORE making any
        // protocol-specific calls. This is the explicit-degradation
        // requirement: if XDamage is unavailable we want a clean
        // ExtensionMissing error, not a vague "unknown opcode" message
        // from a downstream call.
        let damage_present = conn
            .extension_information(damage::X11_EXTENSION_NAME)
            .map_err(|e| DamageError::Setup(format!("query DAMAGE: {e}")))?
            .is_some();
        if !damage_present {
            return Err(DamageError::ExtensionMissing("DAMAGE"));
        }

        let xfixes_present = conn
            .extension_information(xfixes::X11_EXTENSION_NAME)
            .map_err(|e| DamageError::Setup(format!("query XFIXES: {e}")))?
            .is_some();
        if !xfixes_present {
            return Err(DamageError::ExtensionMissing("XFIXES"));
        }

        // Negotiate version. XDamage 1.1 + XFixes 5.0 are floor versions
        // shipped by every Xorg in active use. If the server is older
        // than 1.1, the QueryVersion call returns the highest the server
        // supports; we don't currently use any 1.1-specific features so
        // we accept whatever comes back.
        conn.damage_query_version(1, 1)
            .map_err(|e| DamageError::Setup(format!("damage_query_version: {e}")))?
            .reply()
            .map_err(|e| DamageError::Setup(format!("damage_query_version reply: {e}")))?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| DamageError::Setup(format!("xfixes_query_version: {e}")))?
            .reply()
            .map_err(|e| DamageError::Setup(format!("xfixes_query_version reply: {e}")))?;

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;
        let geometry = (
            screen.width_in_pixels as u32,
            screen.height_in_pixels as u32,
        );

        let damage_id = conn
            .generate_id()
            .map_err(|e| DamageError::Setup(format!("generate_id: {e}")))?;
        conn.damage_create(damage_id, root, ReportLevel::BOUNDING_BOX)
            .map_err(|e| DamageError::Setup(format!("damage_create: {e}")))?
            .check()
            .map_err(|e| DamageError::Setup(format!("damage_create check: {e}")))?;

        // Flush so the subscription is live before the first poll.
        let _ = conn.flush();

        Ok(Self { conn, damage_id, geometry })
    }
}

impl DamageBackend for X11DamageBackend {
    fn poll_damage(&mut self) -> Result<Vec<Rect>, DamageError> {
        let mut rects = Vec::new();

        // Drain every queued event without blocking. With ReportLevel
        // BoundingBox we expect one DamageNotify per damaged interval;
        // if the consumer polls slowly we may see several queued.
        while let Some(event) = self
            .conn
            .poll_for_event()
            .map_err(|e| DamageError::Poll(format!("poll_for_event: {e}")))?
        {
            if let Event::DamageNotify(ev) = event {
                let r = Rect {
                    x: ev.area.x as i32,
                    y: ev.area.y as i32,
                    width: ev.area.width as u32,
                    height: ev.area.height as u32,
                };
                if !r.is_empty() {
                    rects.push(r);
                }
            }
            // Other event types (visibility, expose, etc.) may also be
            // delivered to this connection — we silently drop them.
        }

        // Acknowledge: subtract the entire damage region so future
        // mutations re-fire DamageNotify. The two `0` args mean:
        // parts=0 → subtract the entire region; repair=0 → discard
        // what was subtracted (we already extracted bboxes above).
        if !rects.is_empty() {
            self.conn
                .damage_subtract(self.damage_id, 0u32, 0u32)
                .map_err(|e| DamageError::Poll(format!("damage_subtract: {e}")))?
                .ignore_error();
            // Flush so the subtract is sent to the server before our
            // next poll; without this the next batch can race.
            let _ = self.conn.flush();
        }

        Ok(rects)
    }

    fn capability(&self) -> DamageCapability {
        DamageCapability::OsLevel
    }

    fn screen_geometry(&self) -> (u32, u32) {
        self.geometry
    }
}

// No tests in this module: X11DamageBackend requires a live X server
// to construct. Integration verification is the example binary
// `examples/damage-trace.rs` (deployed and run on the X11 peer).
