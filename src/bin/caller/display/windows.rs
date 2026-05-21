//! Windows display backend using DXGI Desktop Duplication for frame capture
//! and the `SendInput` API for input injection.
//!
//! ## Capture
//!
//! DXGI Desktop Duplication (`IDXGIOutputDuplication`) is the modern,
//! GPU-accelerated path for whole-desktop capture on Windows 8+. The duplication
//! interface, the D3D11 device, and the device context are all single-threaded
//! COM objects that are **not** `Send` across `await` points, so -- exactly like
//! the X11 backend's XShm connection -- the capture loop runs on a dedicated
//! `std::thread`. It communicates with the tokio runtime via a bounded `mpsc`
//! channel (capacity 4, `try_send`, drop on full -- the same backpressure
//! policy as the macOS and X11 backends) and an `AtomicBool` for shutdown.
//!
//! Per-frame the loop:
//! 1. `AcquireNextFrame` with a timeout derived from the target fps. A
//!    `DXGI_ERROR_WAIT_TIMEOUT` simply means the desktop did not change -- we
//!    re-emit the previous frame so the encoder keeps a live cadence (matching
//!    the always-on heartbeat the rest of the pipeline expects).
//! 2. `CopyResource` the acquired GPU texture into a CPU-readable staging
//!    texture (`D3D11_USAGE_STAGING` + `CPU_ACCESS_READ`).
//! 3. `Map` the staging texture and copy the BGRA rows into a `Vec<u8>`
//!    (per-frame heap copy, same as macOS).
//! 4. `ReleaseFrame` and send the `Frame`.
//!
//! `DXGI_ERROR_ACCESS_LOST` (resolution change, full-screen app taking
//! exclusive ownership, secure-desktop / UAC transition, GPU mode switch) tears
//! down the duplication and re-acquires it on the next iteration.
//!
//! Desktop Duplication produces `DXGI_FORMAT_B8G8R8A8_UNORM`, i.e. BGRA8, so we
//! tag frames `FrameFormat::Bgra` and feed the existing `bgra_to_i420`
//! converter unchanged.
//!
//! ## Input
//!
//! `SendInput` injects synthesized keyboard and mouse events. Keyboard events
//! carry a Win32 virtual-key code (see [`super::windows_keymap`]) plus the
//! `KEYEVENTF_EXTENDEDKEY` flag for keys in the extended block. Mouse moves use
//! `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, with the normalized
//! `0.0..1.0` browser coordinates scaled to the `0..65535` absolute coordinate
//! space that `SendInput` expects across the entire virtual desktop.
//!
//! ## Status
//!
//! Compiles and links for `x86_64-pc-windows-msvc`. Live capture requires an
//! interactive desktop session (Desktop Duplication is unavailable on the
//! headless / service / disconnected-RDP "Session 0" desktop), so end-to-end
//! frame delivery and input injection are pending validation on an interactive
//! Windows host -- see the crate-level Windows-port notes.

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use crate::error::CallerError;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE270,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSEINPUT, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WHEEL_DELTA,
};

/// Active capture state: holds the thread handle for cleanup.
struct CaptureState {
    thread: std::thread::JoinHandle<()>,
}

/// Windows screen capture and input injection backend.
///
/// Uses DXGI Desktop Duplication for GPU-accelerated full-desktop capture and
/// the `SendInput` API for keyboard/mouse/scroll injection. Resolution is
/// resolved when `start_capture()` runs (from the duplicated output's mode
/// description); `with_output_index` targets a specific monitor by its DXGI
/// output ordinal.
pub struct WindowsBackend {
    capture: Mutex<Option<CaptureState>>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    /// Target DXGI output index on adapter 0. `None` captures the first
    /// available output (backwards-compatible single-monitor behavior).
    target_output_index: Option<u32>,
}

impl WindowsBackend {
    /// Create a new Windows backend capturing the first available output.
    /// Resolution is populated once `start_capture()` runs.
    pub fn new() -> Self {
        Self {
            capture: Mutex::new(None),
            width: Arc::new(AtomicU32::new(0)),
            height: Arc::new(AtomicU32::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            target_output_index: None,
        }
    }

    /// Create a backend targeting a specific DXGI output (monitor) by its
    /// ordinal on adapter 0. Mirrors `MacOSBackend::with_display_id`.
    pub fn with_output_index(output_index: u32) -> Self {
        Self {
            capture: Mutex::new(None),
            width: Arc::new(AtomicU32::new(0)),
            height: Arc::new(AtomicU32::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            target_output_index: Some(output_index),
        }
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Width/height of a `RECT`, with rotation applied so the reported dimensions
/// match the pixels the duplication actually delivers.
fn output_dimensions(desktop: &RECT, rotation: DXGI_MODE_ROTATION) -> (u32, u32) {
    let raw_w = (desktop.right - desktop.left).max(0) as u32;
    let raw_h = (desktop.bottom - desktop.top).max(0) as u32;
    // For 90/270 rotation the duplicated surface is transposed relative to the
    // desktop rect. Report the surface dimensions.
    if rotation == DXGI_MODE_ROTATION_ROTATE90 || rotation == DXGI_MODE_ROTATION_ROTATE270 {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    }
}

/// Enumerate Windows displays via DXGI output enumeration.
///
/// Walks adapter 0's outputs (`IDXGIAdapter::EnumOutputs`) and reports a
/// `DisplayInfo` per attached output. The primary output (the one whose
/// desktop rect contains the origin `(0,0)`) gets `id: 0`; additional outputs
/// get sequential IDs from 1. `platform_id` carries the DXGI output ordinal so
/// the backend factory can target a specific monitor.
///
/// Returns an empty `Vec` on failure; the caller ([`super::enumerate_displays`])
/// supplies a default fallback entry.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    // DXGI enumeration touches COM objects that are not `Send`; run it on a
    // blocking thread and hop the plain-data result back.
    tokio::task::spawn_blocking(enumerate_displays_blocking)
        .await
        .unwrap_or_default()
}

fn enumerate_displays_blocking() -> Vec<super::DisplayInfo> {
    let factory = match create_dxgi_factory() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[display/windows] CreateDXGIFactory1 failed: {e}");
            return Vec::new();
        }
    };

    let mut displays = Vec::new();
    let mut next_id: u32 = 1;
    let mut output_ordinal: u32 = 0;

    // Adapter loop. In practice a single adapter drives every monitor on a
    // typical machine, but a multi-GPU host can spread outputs across
    // adapters, so we walk every adapter and keep a global output ordinal.
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter = match unsafe { factory.EnumAdapters(adapter_index) } {
            Ok(a) => a,
            // DXGI_ERROR_NOT_FOUND terminates adapter enumeration.
            Err(_) => break,
        };
        adapter_index += 1;

        let mut output_index = 0u32;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };
            output_index += 1;

            let desc = match unsafe { get_output_desc(&output) } {
                Some(d) => d,
                None => {
                    output_ordinal += 1;
                    continue;
                }
            };

            // AttachedToDesktop == FALSE means the output exists but is not
            // part of the desktop (e.g. a disconnected port). Skip it but
            // still advance the ordinal so a targeted index stays stable.
            if desc.AttachedToDesktop.as_bool() {
                let (width, height) = output_dimensions(&desc.DesktopCoordinates, desc.Rotation);
                let is_primary = desc.DesktopCoordinates.left == 0
                    && desc.DesktopCoordinates.top == 0;
                let id = if is_primary {
                    0
                } else {
                    let id = next_id;
                    next_id += 1;
                    id
                };
                let name = device_name_from_desc(&desc);
                displays.push(super::DisplayInfo {
                    id,
                    platform_id: output_ordinal as u64,
                    name: format!("{} ({}x{})", name, width, height),
                    width,
                    height,
                    is_primary,
                });
            }
            output_ordinal += 1;
        }
    }

    // Ensure primary is first.
    displays.sort_by_key(|d| if d.is_primary { 0 } else { 1 });
    displays
}

/// Decode the UTF-16 `DeviceName` from a `DXGI_OUTPUT_DESC` into a `String`.
fn device_name_from_desc(desc: &DXGI_OUTPUT_DESC) -> String {
    let end = desc
        .DeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.DeviceName.len());
    String::from_utf16_lossy(&desc.DeviceName[..end])
}

/// `GetDesc` wrapper that returns `None` on failure.
///
/// In `windows` 0.59 `IDXGIOutput::GetDesc` takes no arguments and returns the
/// populated `DXGI_OUTPUT_DESC` directly as a `Result`.
///
/// # Safety
/// `output` must be a valid `IDXGIOutput`.
unsafe fn get_output_desc(output: &IDXGIOutput) -> Option<DXGI_OUTPUT_DESC> {
    output.GetDesc().ok()
}

/// Create a DXGI 1.1 factory (`CreateDXGIFactory1`), used for both enumeration
/// and capture setup.
fn create_dxgi_factory() -> windows::core::Result<windows::Win32::Graphics::Dxgi::IDXGIFactory1>
{
    unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1() }
}

#[async_trait]
impl DisplayBackend for WindowsBackend {
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
        // Defensive teardown of any prior capture before starting a new one,
        // matching the x11/macos backends: a double-start would otherwise leak
        // the previous duplication thread (its JoinHandle dropped, but the
        // thread kept alive by the shared `shutdown` AtomicBool never being
        // flipped). `stop_capture` is idempotent when nothing is running.
        self.stop_capture().await;

        self.shutdown.store(false, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<Frame>(4);
        let shutdown_flag = Arc::clone(&self.shutdown);
        let shared_w = Arc::clone(&self.width);
        let shared_h = Arc::clone(&self.height);
        let target_output = self.target_output_index;

        // Probe the duplication on the capture thread and report the initial
        // resolution back through a oneshot, so `start_capture` fails loudly
        // (rather than returning a silent black session) if Desktop
        // Duplication is unavailable -- e.g. running on the headless Session 0
        // desktop, no GPU, or DDA disabled. This mirrors the X11 backend
        // surfacing connect failures, but here the device init happens on the
        // thread because the COM objects are not `Send`.
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<(u32, u32), String>>();

        let thread = std::thread::spawn(move || {
            run_dxgi_capture(
                tx,
                shutdown_flag,
                fps,
                target_output,
                shared_w,
                shared_h,
                init_tx,
            );
        });

        match init_rx.await {
            Ok(Ok((w, h))) => {
                self.width.store(w, Ordering::SeqCst);
                self.height.store(h, Ordering::SeqCst);
                *self.capture.lock().await = Some(CaptureState { thread });
                Ok(rx)
            }
            Ok(Err(e)) => {
                // Init failed; the thread has already returned. Join it to
                // avoid a detached-thread warning, then surface the error.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = thread.join();
                })
                .await;
                Err(CallerError::Display(format!(
                    "DXGI Desktop Duplication init failed: {e}"
                )))
            }
            Err(_) => {
                // The thread dropped the sender without sending (panicked
                // before init). Join and report.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = thread.join();
                })
                .await;
                Err(CallerError::Display(
                    "DXGI capture thread exited before initialization".into(),
                ))
            }
        }
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(state) = self.capture.lock().await.take() {
            // `JoinHandle::join` is blocking; pushing it onto the blocking
            // pool keeps the executor thread free (same rationale as the X11
            // backend -- a blocking join on an executor thread stalls the
            // WebSocket pump that delivers UserDisplayRevoked).
            let _ = tokio::task::spawn_blocking(move || {
                let _ = state.thread.join();
            })
            .await;
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        // SendInput is synchronous and cheap; run it inline. The normalized
        // mouse coordinates are scaled against the virtual-desktop extents
        // queried at injection time (so they track resolution / monitor-layout
        // changes without restarting capture).
        match event {
            InputEvent::KeyDown { ref code, .. } => {
                inject_key(code, false)?;
            }
            InputEvent::KeyUp { ref code, .. } => {
                inject_key(code, true)?;
            }
            InputEvent::MouseMove { x, y, buttons } => {
                // A pure move; button-held drags are expressed by the same
                // MOUSEEVENTF_MOVE absolute reposition (Windows tracks button
                // state from the preceding down event), so `buttons` is
                // advisory only and does not change the flags.
                let _ = buttons;
                send_mouse_absolute(x, y, MOUSEEVENTF_MOVE, 0)?;
            }
            InputEvent::MouseDown { x, y, b } => {
                send_mouse_absolute(x, y, MOUSEEVENTF_MOVE | mouse_down_flag(b), 0)?;
            }
            InputEvent::MouseUp { x, y, b } => {
                send_mouse_absolute(x, y, MOUSEEVENTF_MOVE | mouse_up_flag(b), 0)?;
            }
            InputEvent::Scroll { x, y, dx, dy } => {
                // One WHEEL_DELTA (120) per unit line. Browser convention:
                // positive dy scrolls the content down, which on Windows is a
                // negative wheel delta (wheel-forward / away-from-user is
                // positive and scrolls up). Horizontal: positive dx scrolls
                // right, which is a positive HWHEEL delta.
                let vwheel = -(dy.round() as i32) * WHEEL_DELTA as i32;
                let hwheel = (dx.round() as i32) * WHEEL_DELTA as i32;
                if vwheel != 0 {
                    send_mouse_absolute(x, y, MOUSEEVENTF_MOVE | MOUSEEVENTF_WHEEL, vwheel)?;
                }
                if hwheel != 0 {
                    send_mouse_absolute(x, y, MOUSEEVENTF_MOVE | MOUSEEVENTF_HWHEEL, hwheel)?;
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
        "windows"
    }
}

// ---------------------------------------------------------------------------
// Input injection helpers
// ---------------------------------------------------------------------------

/// Map a browser mouse-button index to the `MOUSEEVENTF_*DOWN` flag.
/// Browser: 0 = left, 1 = middle, 2 = right.
fn mouse_down_flag(b: u8) -> MOUSE_EVENT_FLAGS {
    match b {
        0 => MOUSEEVENTF_LEFTDOWN,
        1 => MOUSEEVENTF_MIDDLEDOWN,
        2 => MOUSEEVENTF_RIGHTDOWN,
        _ => MOUSEEVENTF_LEFTDOWN,
    }
}

/// Map a browser mouse-button index to the `MOUSEEVENTF_*UP` flag.
fn mouse_up_flag(b: u8) -> MOUSE_EVENT_FLAGS {
    match b {
        0 => MOUSEEVENTF_LEFTUP,
        1 => MOUSEEVENTF_MIDDLEUP,
        2 => MOUSEEVENTF_RIGHTUP,
        _ => MOUSEEVENTF_LEFTUP,
    }
}

/// Inject a single keyboard event (down or up) for a DOM `code`.
///
/// Unknown codes are silently ignored (matching the x11/macos backends, which
/// no-op when the keymap returns `None`). The `KEYEVENTF_EXTENDEDKEY` flag is
/// applied for keys in the extended block so the right-hand modifiers, arrows,
/// navigation cluster, and numpad enter/divide resolve to the correct physical
/// key.
fn inject_key(code: &str, key_up: bool) -> Result<(), CallerError> {
    let Some(vk) = super::windows_keymap::dom_code_to_vk(code) else {
        return Ok(());
    };

    let mut flags = KEYBD_EVENT_FLAGS(0);
    if super::windows_keymap::is_extended_key(code) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_one_input(&input)
}

/// Send a mouse event at the given normalized coordinates with the supplied
/// flags. `mouse_data` carries the wheel delta for `MOUSEEVENTF_WHEEL` /
/// `MOUSEEVENTF_HWHEEL` (ignored otherwise).
///
/// The `0.0..1.0` browser coordinates are mapped onto the virtual desktop:
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` interprets `dx`/`dy` as a
/// `0..65535` fraction of the **entire** virtual screen (all monitors), so we
/// first place the point inside the captured monitor's pixel rect, then
/// normalize that against the virtual-screen extents.
fn send_mouse_absolute(
    x: f64,
    y: f64,
    flags: MOUSE_EVENT_FLAGS,
    mouse_data: i32,
) -> Result<(), CallerError> {
    let (abs_x, abs_y) = normalized_to_virtual_abs(x, y);
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: abs_x,
                dy: abs_y,
                mouseData: mouse_data as u32,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_one_input(&input)
}

/// Read the virtual-screen extents (origin + size across all monitors).
/// Returns `(left, top, width, height)` in physical pixels.
fn virtual_screen_metrics() -> (i32, i32, i32, i32) {
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (left, top, width, height)
    }
}

/// Convert normalized `0.0..1.0` coordinates (relative to the captured
/// monitor, whose top-left we treat as the virtual-screen origin for the
/// single-monitor case) into the `0..65535` absolute space `SendInput`
/// expects with `MOUSEEVENTF_VIRTUALDESK`.
///
/// For the common single-monitor / primary-output case the captured rect is
/// the whole virtual screen with origin `(0,0)`, so this reduces to
/// `coord * 65535 / (extent - 1)`. For a secondary monitor the normalized
/// coordinate is relative to that monitor and would need its offset added; the
/// current backend captures the primary output, where the offset is zero.
fn normalized_to_virtual_abs(x: f64, y: f64) -> (i32, i32) {
    let (vleft, vtop, vwidth, vheight) = virtual_screen_metrics();
    // Guard against a zero/negative extent (no desktop): fall back to a
    // full-range mapping so we never divide by zero.
    let vwidth = if vwidth > 1 { vwidth } else { 1 };
    let vheight = if vheight > 1 { vheight } else { 1 };

    // Pixel position within the virtual desktop. The captured primary output
    // sits at (0,0); `vleft`/`vtop` are typically <= 0 when secondary
    // monitors extend left/up, so subtract them to land in the [0, extent]
    // range SendInput normalizes against.
    let px = (x.clamp(0.0, 1.0) * vwidth as f64) as i32 - vleft;
    let py = (y.clamp(0.0, 1.0) * vheight as f64) as i32 - vtop;

    // Normalize to 0..65535 across the virtual screen, per the documented
    // SendInput formula `(coord * 65535) / (extent - 1)`.
    let abs_x = ((px as i64 * 65535) / (vwidth as i64 - 1).max(1)) as i32;
    let abs_y = ((py as i64 * 65535) / (vheight as i64 - 1).max(1)) as i32;
    (abs_x.clamp(0, 65535), abs_y.clamp(0, 65535))
}

/// Submit a single `INPUT` to `SendInput`, returning an error if the system
/// rejected it (e.g. blocked by `UIPI`, or a higher-integrity foreground app).
fn send_one_input(input: &INPUT) -> Result<(), CallerError> {
    let sent = unsafe { SendInput(&[*input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(CallerError::Display(format!(
            "SendInput injected {sent}/1 events (blocked by UIPI or a \
             higher-integrity foreground window?)"
        )))
    }
}

// ---------------------------------------------------------------------------
// DXGI Desktop Duplication capture thread
// ---------------------------------------------------------------------------

/// Owns the live duplication objects for one acquisition session. Recreated on
/// `DXGI_ERROR_ACCESS_LOST`.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    /// Reusable CPU-readable staging texture. Reallocated only when the source
    /// dimensions change.
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

/// Initialize D3D11 + DXGI Desktop Duplication for the requested output.
///
/// Returns the live `Duplication` plus the output's pixel dimensions.
fn init_duplication(target_output: Option<u32>) -> Result<(Duplication, u32, u32), String> {
    // 1. Create a hardware D3D11 device with BGRA support (required for the
    //    Desktop Duplication B8G8R8A8 surface).
    let feature_levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();

    let hr = unsafe {
        D3D11CreateDevice(
            None, // default adapter
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(), // no software rasterizer module
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
    };
    hr.map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    let device = device.ok_or_else(|| "D3D11CreateDevice returned null device".to_string())?;
    let context =
        context.ok_or_else(|| "D3D11CreateDevice returned null context".to_string())?;

    // 2. From the device, reach the DXGI adapter to enumerate outputs.
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| format!("ID3D11Device -> IDXGIDevice: {e}"))?;
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }
        .map_err(|e| format!("IDXGIDevice::GetAdapter: {e}"))?;

    // 3. Pick the target output (default: first attached output).
    let (output, width, height) = select_output(&adapter, target_output)?;

    // 4. Upgrade to IDXGIOutput1 and duplicate the desktop.
    let output1: IDXGIOutput1 = output
        .cast()
        .map_err(|e| format!("IDXGIOutput -> IDXGIOutput1: {e}"))?;
    let duplication = unsafe { output1.DuplicateOutput(&device) }.map_err(|e| {
        format!(
            "IDXGIOutput1::DuplicateOutput: {e} (Desktop Duplication is \
             unavailable on a headless/Session-0 desktop, under exclusive \
             full-screen, or when another process already holds the \
             duplication)"
        )
    })?;

    Ok((
        Duplication {
            device,
            context,
            duplication,
            staging: None,
            width,
            height,
        },
        width,
        height,
    ))
}

/// Select a DXGI output on the adapter, returning the output plus its pixel
/// dimensions. `target_output` is an ordinal on this adapter; `None` picks the
/// first attached output.
fn select_output(
    adapter: &IDXGIAdapter,
    target_output: Option<u32>,
) -> Result<(IDXGIOutput, u32, u32), String> {
    if let Some(idx) = target_output {
        let output: IDXGIOutput = unsafe { adapter.EnumOutputs(idx) }
            .map_err(|e| format!("EnumOutputs({idx}): {e}"))?;
        let desc = unsafe { get_output_desc(&output) }
            .ok_or_else(|| format!("GetDesc on output {idx} failed"))?;
        let (w, h) = output_dimensions(&desc.DesktopCoordinates, desc.Rotation);
        return Ok((output, w & !1, h & !1));
    }

    // Default: first output that is attached to the desktop.
    let mut idx = 0u32;
    loop {
        match unsafe { adapter.EnumOutputs(idx) } {
            Ok(output) => {
                if let Some(desc) = unsafe { get_output_desc(&output) } {
                    if desc.AttachedToDesktop.as_bool() {
                        let (w, h) =
                            output_dimensions(&desc.DesktopCoordinates, desc.Rotation);
                        // VP8/I420 require even dimensions; enforce here as the
                        // backends do elsewhere.
                        return Ok((output, w & !1, h & !1));
                    }
                }
                idx += 1;
            }
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => {
                return Err("no DXGI output attached to the desktop".into());
            }
            Err(e) => return Err(format!("EnumOutputs({idx}): {e}")),
        }
    }
}

/// Ensure `dup.staging` is a CPU-readable staging texture matching `(w, h)`,
/// (re)allocating it if absent or stale.
fn ensure_staging(dup: &mut Duplication, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
    if let Some(existing) = &dup.staging {
        if dup.width == w && dup.height == h {
            return Ok(existing.clone());
        }
    }

    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe {
        dup.device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| format!("CreateTexture2D (staging {w}x{h}): {e}"))?;
    }
    let texture = texture.ok_or_else(|| "CreateTexture2D returned null".to_string())?;
    dup.staging = Some(texture.clone());
    dup.width = w;
    dup.height = h;
    Ok(texture)
}

/// Run the DXGI Desktop Duplication capture loop on a dedicated OS thread.
///
/// Sends the initial resolution (or an init error) through `init_tx`, then
/// loops at the target framerate. Handles `DXGI_ERROR_WAIT_TIMEOUT` (no change
/// -- re-emit the last frame to keep cadence) and `DXGI_ERROR_ACCESS_LOST`
/// (re-acquire the duplication).
#[allow(clippy::too_many_arguments)]
fn run_dxgi_capture(
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    fps: u32,
    target_output: Option<u32>,
    shared_width: Arc<AtomicU32>,
    shared_height: Arc<AtomicU32>,
    init_tx: tokio::sync::oneshot::Sender<Result<(u32, u32), String>>,
) {
    let frame_interval =
        std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });
    // AcquireNextFrame timeout in ms: roughly one frame interval, but at least
    // a few ms so a high fps doesn't busy-poll, and capped so shutdown is
    // responsive.
    let acquire_timeout_ms = (frame_interval.as_millis() as u32).clamp(5, 100);

    let mut dup = match init_duplication(target_output) {
        Ok((dup, w, h)) => {
            let _ = init_tx.send(Ok((w, h)));
            dup
        }
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    // Last successfully captured frame, re-emitted on WAIT_TIMEOUT so the
    // downstream encoder keeps a live heartbeat on a static desktop.
    let mut last_frame: Option<Frame> = None;
    let mut frame_count: u64 = 0;
    // Consecutive hard errors before giving up (display gone, device removed).
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 60;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let start = std::time::Instant::now();

        match acquire_and_copy(&mut dup, acquire_timeout_ms) {
            Ok(Some(frame)) => {
                consecutive_errors = 0;
                // Track resolution changes so inject_input's coordinate math
                // (and the session's reported resolution) stay current.
                let w = frame.width;
                let h = frame.height;
                let prev_w = shared_width.load(Ordering::SeqCst);
                let prev_h = shared_height.load(Ordering::SeqCst);
                if w != prev_w || h != prev_h {
                    shared_width.store(w, Ordering::SeqCst);
                    shared_height.store(h, Ordering::SeqCst);
                    eprintln!(
                        "[display/windows] frame resize detected: {prev_w}x{prev_h} -> {w}x{h}",
                    );
                }

                frame_count += 1;
                if frame_count == 1 || frame_count % 300 == 0 {
                    eprintln!(
                        "[display/windows] frame #{frame_count} {}x{} stride={} size={}B",
                        frame.width,
                        frame.height,
                        frame.stride,
                        frame.data.len(),
                    );
                }

                // Cache a clone for heartbeat re-emission, then send.
                last_frame = Some(clone_frame(&frame));
                let _ = tx.try_send(frame);
            }
            Ok(None) => {
                // WAIT_TIMEOUT: desktop unchanged. Re-emit the last frame with
                // a fresh timestamp so the encoder's freshness clock advances.
                if let Some(prev) = &last_frame {
                    let mut hb = clone_frame(prev);
                    hb.timestamp = std::time::Instant::now();
                    let _ = tx.try_send(hb);
                }
            }
            Err(CaptureError::AccessLost) => {
                eprintln!(
                    "[display/windows] DXGI_ERROR_ACCESS_LOST -- re-acquiring duplication",
                );
                // Drop the old duplication and re-init. A transient failure
                // (e.g. mid secure-desktop transition) is retried.
                match init_duplication(target_output) {
                    Ok((new_dup, w, h)) => {
                        dup = new_dup;
                        shared_width.store(w, Ordering::SeqCst);
                        shared_height.store(h, Ordering::SeqCst);
                        consecutive_errors = 0;
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        eprintln!(
                            "[display/windows] re-acquire failed ({consecutive_errors}): {e}",
                        );
                        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                            eprintln!(
                                "[display/windows] giving up after {consecutive_errors} \
                                 re-acquire failures",
                            );
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
            Err(CaptureError::Fatal(e)) => {
                consecutive_errors += 1;
                eprintln!("[display/windows] capture error ({consecutive_errors}): {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!(
                        "[display/windows] giving up after {consecutive_errors} capture errors",
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    // The duplication / device drop here releases all COM references.
}

/// Capture-loop error discriminator.
enum CaptureError {
    /// `DXGI_ERROR_ACCESS_LOST` -- the duplication must be recreated.
    AccessLost,
    /// Any other failure.
    Fatal(String),
}

/// Acquire one frame and copy it into a CPU-side `Frame`.
///
/// Returns `Ok(None)` on `DXGI_ERROR_WAIT_TIMEOUT` (no desktop change within
/// the timeout). Always pairs a successful `AcquireNextFrame` with
/// `ReleaseFrame`.
fn acquire_and_copy(
    dup: &mut Duplication,
    timeout_ms: u32,
) -> Result<Option<Frame>, CaptureError> {
    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;

    let acquire = unsafe {
        dup.duplication
            .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
    };

    if let Err(e) = acquire {
        return if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
            Ok(None)
        } else if e.code() == DXGI_ERROR_ACCESS_LOST {
            Err(CaptureError::AccessLost)
        } else {
            Err(CaptureError::Fatal(format!("AcquireNextFrame: {e}")))
        };
    }

    // Ensure ReleaseFrame runs on every path out of here once acquired.
    let result = (|| {
        let resource = resource
            .ok_or_else(|| CaptureError::Fatal("AcquireNextFrame returned null resource".into()))?;
        let src_texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|e| CaptureError::Fatal(format!("IDXGIResource -> ID3D11Texture2D: {e}")))?;

        // Read the source dimensions from its desc; the output rect can lag a
        // resolution change by a frame, so trust the texture.
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src_texture.GetDesc(&mut src_desc) };
        let width = src_desc.Width & !1;
        let height = src_desc.Height & !1;

        let staging = ensure_staging(dup, width, height).map_err(CaptureError::Fatal)?;

        // GPU copy from the acquired frame into the CPU-readable staging tex.
        unsafe { dup.context.CopyResource(&staging, &src_texture) };

        // Map and copy out the BGRA rows.
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            dup.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Fatal(format!("Map staging texture: {e}")))?;
        }

        let stride = mapped.RowPitch;
        let row_bytes = (width as usize) * 4;
        let mut data = vec![0u8; row_bytes * height as usize];
        unsafe {
            let src_base = mapped.pData as *const u8;
            for row in 0..height as usize {
                let src_row = src_base.add(row * stride as usize);
                let dst_off = row * row_bytes;
                std::ptr::copy_nonoverlapping(
                    src_row,
                    data.as_mut_ptr().add(dst_off),
                    row_bytes,
                );
            }
            dup.context.Unmap(&staging, 0);
        }

        Ok(Frame {
            data,
            format: FrameFormat::Bgra,
            width,
            height,
            // We copy into a tightly packed buffer, so the emitted stride is
            // width*4 regardless of the source RowPitch padding.
            stride: row_bytes as u32,
            timestamp: std::time::Instant::now(),
        })
    })();

    // Release the acquired frame unconditionally.
    let _ = unsafe { dup.duplication.ReleaseFrame() };

    result.map(Some)
}

/// Shallow-copy a `Frame` (clones the pixel `Vec`). Used to cache the last
/// frame for heartbeat re-emission on `WAIT_TIMEOUT`.
fn clone_frame(f: &Frame) -> Frame {
    Frame {
        data: f.data.clone(),
        format: f.format,
        width: f.width,
        height: f.height,
        stride: f.stride,
        timestamp: f.timestamp,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_dimensions_no_rotation() {
        let rect = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert_eq!(
            output_dimensions(&rect, windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY),
            (1920, 1080)
        );
    }

    #[test]
    fn output_dimensions_rotated_transposes() {
        let rect = RECT {
            left: 0,
            top: 0,
            right: 1080,
            bottom: 1920,
        };
        // A 90-degree rotated portrait panel reports a transposed surface.
        assert_eq!(
            output_dimensions(&rect, DXGI_MODE_ROTATION_ROTATE90),
            (1920, 1080)
        );
        assert_eq!(
            output_dimensions(&rect, DXGI_MODE_ROTATION_ROTATE270),
            (1920, 1080)
        );
    }

    #[test]
    fn mouse_button_flags_map_browser_indices() {
        assert_eq!(mouse_down_flag(0), MOUSEEVENTF_LEFTDOWN);
        assert_eq!(mouse_down_flag(1), MOUSEEVENTF_MIDDLEDOWN);
        assert_eq!(mouse_down_flag(2), MOUSEEVENTF_RIGHTDOWN);
        assert_eq!(mouse_down_flag(7), MOUSEEVENTF_LEFTDOWN); // unknown -> left
        assert_eq!(mouse_up_flag(0), MOUSEEVENTF_LEFTUP);
        assert_eq!(mouse_up_flag(1), MOUSEEVENTF_MIDDLEUP);
        assert_eq!(mouse_up_flag(2), MOUSEEVENTF_RIGHTUP);
    }
}
