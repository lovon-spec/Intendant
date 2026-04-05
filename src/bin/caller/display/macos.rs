//! macOS display backend using ScreenCaptureKit for frame capture and
//! CoreGraphics CGEvent API for input injection.
//!
//! ScreenCaptureKit callbacks run on a system dispatch queue and deliver
//! `CMSampleBuffer` frames.  We lock the pixel buffer, copy the data into a
//! `Frame`, and send it over a bounded `mpsc` channel (capacity 4, `try_send`,
//! drop on full -- same backpressure policy as the Wayland backend).
//!
//! Input injection uses `CGEvent` for keyboard, mouse, and scroll events.
//! The `CGEventSource` is created with `HIDSystemState` so injected events
//! appear as if they came from physical hardware.

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use crate::error::CallerError;
use async_trait::async_trait;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use screencapturekit::cm::CMTime;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Active capture state: holds the `SCStream` and shutdown flag.
struct CaptureState {
    stream: SCStream,
}

/// macOS screen capture and input injection backend.
///
/// Uses ScreenCaptureKit (SCStream) for high-performance frame capture and
/// CoreGraphics CGEvent for input injection.
pub struct MacOSBackend {
    capture: Mutex<Option<CaptureState>>,
    width: AtomicU32,
    height: AtomicU32,
    shutdown: Arc<AtomicBool>,
    /// Optional target display ID (CGDisplayID).  When `None`, captures the
    /// first available display (backwards-compatible single-monitor behavior).
    target_display_id: Option<u32>,
}

impl MacOSBackend {
    /// Create a new macOS backend.  Resolution is populated from the actual
    /// captured display once `start_capture()` runs.
    pub fn new() -> Self {
        Self {
            capture: Mutex::new(None),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            shutdown: Arc::new(AtomicBool::new(false)),
            target_display_id: None,
        }
    }

    /// Create a backend targeting a specific display by its CGDisplayID.
    pub fn with_display_id(display_id: u32) -> Self {
        Self {
            capture: Mutex::new(None),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            shutdown: Arc::new(AtomicBool::new(false)),
            target_display_id: Some(display_id),
        }
    }
}

/// Enumerate macOS displays via ScreenCaptureKit.
///
/// Returns a `DisplayInfo` per connected display.  The primary display
/// (`CGMainDisplayID()`) gets `id: 0`; additional displays get sequential
/// IDs starting from 1.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    let content = match SCShareableContent::get() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/macos] SCShareableContent::get failed: {e}");
            return Vec::new();
        }
    };

    let main_id = CGDisplay::main().id;
    let mut displays = Vec::new();
    let mut next_id: u32 = 1;

    for sc_display in content.displays() {
        let cg = CGDisplay::new(sc_display.display_id());
        let is_primary = sc_display.display_id() == main_id;
        let id = if is_primary { 0 } else { let id = next_id; next_id += 1; id };
        let width = cg.pixels_wide() as u32;
        let height = cg.pixels_high() as u32;

        // Build a human-readable name. SCDisplay does not expose a name
        // property, so we use the display ID and resolution.
        let name = if is_primary {
            format!("Primary Display ({}x{})", width, height)
        } else {
            format!("Display {} ({}x{})", sc_display.display_id(), width, height)
        };

        displays.push(super::DisplayInfo {
            id,
            platform_id: sc_display.display_id() as u64,
            name,
            width,
            height,
            is_primary,
        });
    }

    // Ensure primary is first.
    displays.sort_by_key(|d| if d.is_primary { 0 } else { 1 });
    displays
}

#[async_trait]
impl DisplayBackend for MacOSBackend {
    async fn start_capture(
        &self,
        fps: u32,
    ) -> Result<mpsc::Receiver<Frame>, CallerError> {
        self.shutdown.store(false, Ordering::SeqCst);

        // Get shareable content (triggers TCC permission prompt on first use).
        let content = SCShareableContent::get()
            .map_err(|e| CallerError::Display(format!("SCShareableContent::get: {e}")))?;

        let display = if let Some(target_id) = self.target_display_id {
            // Find the specific display by CGDisplayID.
            content
                .displays()
                .into_iter()
                .find(|d| d.display_id() == target_id)
                .ok_or_else(|| {
                    CallerError::Display(format!(
                        "display with CGDisplayID {} not found",
                        target_id
                    ))
                })?
        } else {
            // Default: first available display (backwards-compatible).
            content
                .displays()
                .into_iter()
                .next()
                .ok_or_else(|| CallerError::Display("no display found".into()))?
        };

        // Use the *captured* display's CGDisplay for resolution, so input
        // injection targets the same monitor (avoids multi-monitor mismatch
        // when the first SCDisplay is not CGDisplay::main()).
        let cg_display = CGDisplay::new(display.display_id());
        // VP8 (and I420 color space) requires dimensions divisible by 2.
        let width = (cg_display.pixels_wide() as u32) & !1;
        let height = (cg_display.pixels_high() as u32) & !1;
        self.width.store(width, Ordering::SeqCst);
        self.height.store(height, Ordering::SeqCst);

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let frame_interval = CMTime {
            value: 1,
            timescale: fps.max(1) as i32,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        };

        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(true)
            .with_minimum_frame_interval(&frame_interval);

        // Bounded channel: backend drops frames if consumer is slow.
        let (tx, rx) = mpsc::channel::<Frame>(4);

        let shutdown_flag = Arc::clone(&self.shutdown);

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                if of_type != SCStreamOutputType::Screen {
                    return;
                }
                if shutdown_flag.load(Ordering::SeqCst) {
                    return;
                }
                let Some(buffer) = sample.image_buffer() else {
                    return;
                };
                let Ok(guard) = buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
                    return;
                };

                let w = guard.width() as u32;
                let h = guard.height() as u32;
                let stride = guard.bytes_per_row() as u32;
                let pixels = guard.as_slice();

                if pixels.is_empty() {
                    return;
                }

                let frame = Frame {
                    data: pixels.to_vec(),
                    format: FrameFormat::Bgra,
                    width: w,
                    height: h,
                    stride,
                    timestamp: std::time::Instant::now(),
                };

                // Backpressure: drop frame if channel is full.
                let _ = tx.try_send(frame);
            },
            SCStreamOutputType::Screen,
        );

        stream
            .start_capture()
            .map_err(|e| CallerError::Display(format!("start_capture: {e}")))?;

        *self.capture.lock().await = Some(CaptureState { stream });

        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(state) = self.capture.lock().await.take() {
            let _ = state.stream.stop_capture();
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        let width = self.width.load(Ordering::SeqCst) as f64;
        let height = self.height.load(Ordering::SeqCst) as f64;

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| CallerError::Display("failed to create CGEventSource".into()))?;

        match event {
            InputEvent::KeyDown { ref code, shift, ctrl, alt, meta, .. } => {
                if let Some(keycode) = super::macos_keymap::dom_code_to_macos_keycode(code) {
                    let ev = CGEvent::new_keyboard_event(source, keycode, true)
                        .map_err(|()| CallerError::Display("CGEvent keyboard failed".into()))?;
                    let flags = build_modifier_flags(shift, ctrl, alt, meta);
                    ev.set_flags(flags);
                    ev.post(CGEventTapLocation::HID);
                }
            }
            InputEvent::KeyUp { ref code, shift, ctrl, alt, meta, .. } => {
                if let Some(keycode) = super::macos_keymap::dom_code_to_macos_keycode(code) {
                    let ev = CGEvent::new_keyboard_event(source, keycode, false)
                        .map_err(|()| CallerError::Display("CGEvent keyboard failed".into()))?;
                    let flags = build_modifier_flags(shift, ctrl, alt, meta);
                    ev.set_flags(flags);
                    ev.post(CGEventTapLocation::HID);
                }
            }
            InputEvent::MouseMove { x, y, buttons } => {
                let point = CGPoint::new(x * width, y * height);
                let (event_type, button) = if buttons & 1 != 0 {
                    (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                } else if buttons & 2 != 0 {
                    (CGEventType::RightMouseDragged, CGMouseButton::Right)
                } else if buttons & 4 != 0 {
                    (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                } else {
                    (CGEventType::MouseMoved, CGMouseButton::Left)
                };
                let ev = CGEvent::new_mouse_event(source, event_type, point, button)
                    .map_err(|()| CallerError::Display("CGEvent mouse move failed".into()))?;
                ev.post(CGEventTapLocation::HID);
            }
            InputEvent::MouseDown { x, y, b } => {
                let point = CGPoint::new(x * width, y * height);
                let (event_type, button) = mouse_button_down(b);
                let ev = CGEvent::new_mouse_event(source, event_type, point, button)
                    .map_err(|()| CallerError::Display("CGEvent mouse down failed".into()))?;
                if b == 2 {
                    // Middle button needs button number field set
                    ev.set_integer_value_field(
                        core_graphics::event::EventField::MOUSE_EVENT_BUTTON_NUMBER,
                        2,
                    );
                }
                ev.post(CGEventTapLocation::HID);
            }
            InputEvent::MouseUp { x, y, b } => {
                let point = CGPoint::new(x * width, y * height);
                let (event_type, button) = mouse_button_up(b);
                let ev = CGEvent::new_mouse_event(source, event_type, point, button)
                    .map_err(|()| CallerError::Display("CGEvent mouse up failed".into()))?;
                if b == 2 {
                    ev.set_integer_value_field(
                        core_graphics::event::EventField::MOUSE_EVENT_BUTTON_NUMBER,
                        2,
                    );
                }
                ev.post(CGEventTapLocation::HID);
            }
            InputEvent::Scroll { dx, dy, .. } => {
                // CGEvent scroll: positive dy scrolls up (opposite of browser convention)
                let wheel1 = -(dy.round() as i32);
                let wheel2 = dx.round() as i32;
                if wheel1 != 0 || wheel2 != 0 {
                    let ev = CGEvent::new_scroll_event(
                        source,
                        ScrollEventUnit::LINE,
                        2,     // wheel_count
                        wheel1,
                        wheel2,
                        0,
                    )
                    .map_err(|()| CallerError::Display("CGEvent scroll failed".into()))?;
                    ev.post(CGEventTapLocation::HID);
                }
            }
        }
        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::SeqCst),
            self.height.load(Ordering::SeqCst),
        )
    }

    fn kind(&self) -> &'static str {
        "macos"
    }
}

/// Build CGEventFlags from the modifier booleans.
fn build_modifier_flags(shift: bool, ctrl: bool, alt: bool, meta: bool) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if ctrl {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if alt {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if meta {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

/// Map browser mouse button index to macOS event type and CGMouseButton for down events.
fn mouse_button_down(b: u8) -> (CGEventType, CGMouseButton) {
    match b {
        0 => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        1 => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        2 => (CGEventType::RightMouseDown, CGMouseButton::Right),
        _ => (CGEventType::LeftMouseDown, CGMouseButton::Left),
    }
}

/// Map browser mouse button index to macOS event type and CGMouseButton for up events.
fn mouse_button_up(b: u8) -> (CGEventType, CGMouseButton) {
    match b {
        0 => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        1 => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        2 => (CGEventType::RightMouseUp, CGMouseButton::Right),
        _ => (CGEventType::LeftMouseUp, CGMouseButton::Left),
    }
}
