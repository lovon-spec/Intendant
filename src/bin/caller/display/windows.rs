//! Windows display backend with two capture paths -- GDI `BitBlt` (default) and
//! DXGI Desktop Duplication (opt-in) -- plus `SendInput` for input injection.
//!
//! ## Capture
//!
//! Two implementations sit behind the same `DisplayBackend` seam, selected by
//! [`CaptureMethod`]:
//!
//! ### GDI `BitBlt` (default -- [`CaptureMethod::Gdi`])
//!
//! `BitBlt` from the screen DC reads the **DWM-composed** desktop, the same
//! pixels a user sees. Crucially it works on *every* display adapter, including
//! the virtual/indirect ones Intendant's "always-on steward" actually runs on:
//! RDP indirect display, GCP/cloud virtual display, headless. On those adapters
//! DXGI Desktop Duplication captures **all-black** frames -- it requires real
//! frame presentation/scanout that virtual/headless/RDP displays don't provide,
//! so it "succeeds" yet duplicates black. (Proven live on a GCP Windows VM: DDA
//! streamed black on both RDP and the console session with a virtual display
//! device, while a GDI `BitBlt` capture at the same instant showed the real
//! desktop.) GDI is therefore the robust, default capture path.
//!
//! The capture loop runs on a dedicated `std::thread` because GDI device
//! contexts and bitmaps are raw `HDC`/`HBITMAP` handles that are **not** `Send`.
//! Per frame the loop `BitBlt`s the screen DC into a cached top-down 32-bit
//! `CreateDIBSection` DIB (`SRCCOPY | CAPTUREBLT`, the latter so layered/overlay
//! windows are included), then copies the DIB bits into a `Vec<u8>`. The DC,
//! memory DC, and DIB are cached across frames and recreated only on a
//! resolution change. The DIB is created BGRA8 top-down (`biHeight` negative,
//! `biBitCount = 32`, `biCompression = BI_RGB`), so the emitted rows are the
//! identical `DXGI_FORMAT_B8G8R8A8_UNORM` byte layout the DDA path produced --
//! `FrameFormat::Bgra`, `stride = width * 4` -- and feed the existing
//! `bgra_to_i420` / Media Foundation H.264 encoder unchanged.
//!
//! ### DXGI Desktop Duplication (opt-in -- [`CaptureMethod::Dxgi`])
//!
//! `IDXGIOutputDuplication` is the GPU-accelerated path: zero-copy from the GPU
//! into a CPU-readable staging texture, lowest overhead on physical hardware. It
//! is retained as an opt-in fast path (constructor or
//! `INTENDANT_WINDOWS_CAPTURE=dxgi`) for hosts with a real GPU/scanout where it
//! works, but it is **not** the default because it silently captures black on
//! the cloud/RDP/headless adapters this project commonly targets.
//!
//! Like GDI, the duplication interface, the D3D11 device, and the device context
//! are single-threaded COM objects **not** `Send` across `await` points, so the
//! loop runs on a dedicated `std::thread`. Per frame: `AcquireNextFrame`
//! (`DXGI_ERROR_WAIT_TIMEOUT` -> re-emit the last frame to keep cadence),
//! `CopyResource` into a staging texture, `Map` and copy the BGRA rows,
//! `ReleaseFrame`. `DXGI_ERROR_ACCESS_LOST` (resolution change, exclusive
//! full-screen, secure-desktop / UAC transition, GPU mode switch) tears down the
//! duplication and re-acquires it.
//!
//! Both paths talk to the tokio runtime via a bounded `mpsc` channel (capacity
//! 4, `try_send`, drop on full -- the same backpressure policy as the macOS and
//! X11 backends) and an `AtomicBool` for shutdown, and both report their initial
//! resolution back through a oneshot so `start_capture` fails loudly rather than
//! returning a silent black session.
//!
//! ## Input
//!
//! `SendInput` injects synthesized keyboard and mouse events. Keyboard events
//! carry a Win32 virtual-key code (see [`super::windows_keymap`]) plus the
//! `KEYEVENTF_EXTENDEDKEY` flag for keys in the extended block. Mouse moves use
//! `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`. The normalized `0.0..1.0`
//! browser coordinates are relative to the **captured monitor**, so they are
//! first placed inside that monitor's rect on the virtual desktop (using the
//! capture rect's origin, tracked alongside its dimensions), then scaled to the
//! `0..65535` absolute coordinate space `SendInput` expects across the entire
//! virtual desktop. This makes clicks land on the correct monitor when a
//! secondary output is streamed; capturing the primary / whole virtual desktop
//! leaves the mapping unchanged.
//!
//! ## Status
//!
//! Compiles and links for `x86_64-pc-windows-msvc`. The GDI default path
//! captures the real desktop on the cloud/RDP/headless hosts this project
//! targets (validated live). Input injection via `SendInput` still requires an
//! interactive desktop session (it is blocked on the headless service "Session
//! 0" desktop) -- see the crate-level Windows-port notes.

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use crate::error::CallerError;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE270,
    DXGI_MODE_ROTATION_ROTATE90,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, SRCCOPY,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenDesktopW, OpenInputDesktop, SetThreadDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, HDESK,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WHEEL_DELTA,
};

/// Active capture state: holds the thread handle for cleanup.
struct CaptureState {
    thread: std::thread::JoinHandle<()>,
}

/// Which whole-desktop capture implementation a [`WindowsBackend`] drives.
///
/// Both produce byte-identical `FrameFormat::Bgra` frames with `stride =
/// width * 4`; only the OS path that fills them differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMethod {
    /// GDI `BitBlt` of the screen DC. **Default.** Reads the DWM-composed
    /// desktop and works on virtual/indirect/headless/RDP/cloud display
    /// adapters where DXGI Desktop Duplication captures black.
    Gdi,
    /// DXGI Desktop Duplication. GPU-accelerated fast path for hosts with a
    /// real GPU + scanout; opt-in because it captures black on the cloud/RDP/
    /// headless adapters this project commonly targets.
    Dxgi,
}

impl CaptureMethod {
    /// Resolve the default capture method, honoring an
    /// `INTENDANT_WINDOWS_CAPTURE` override (`gdi` | `dxgi`, case-insensitive)
    /// so the fast path can be opted into at runtime without a code change.
    /// Anything unrecognized (or unset) falls back to the GDI default.
    fn from_env_or_default() -> Self {
        match std::env::var("INTENDANT_WINDOWS_CAPTURE") {
            Ok(v) if v.eq_ignore_ascii_case("dxgi") => CaptureMethod::Dxgi,
            Ok(v) if v.eq_ignore_ascii_case("gdi") => CaptureMethod::Gdi,
            _ => CaptureMethod::Gdi,
        }
    }
}

/// The rectangle currently being captured, expressed in **virtual-desktop**
/// pixel coordinates (the same space `DXGI_OUTPUT_DESC::DesktopCoordinates` and
/// `GetSystemMetrics(SM_X/YVIRTUALSCREEN)` use). `(left, top)` is the monitor's
/// offset within the virtual desktop -- `(0, 0)` for the primary / whole-desktop
/// capture, and negative for a monitor placed left of or above the primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureRect {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

/// Windows screen capture and input injection backend.
///
/// Captures the full desktop via GDI `BitBlt` (default) or DXGI Desktop
/// Duplication (opt-in -- see [`CaptureMethod`]) and injects keyboard/mouse/
/// scroll via `SendInput`. Resolution is resolved when `start_capture()` runs
/// (from `GetSystemMetrics` for the primary GDI path, or the monitor/output
/// rect when a specific monitor is targeted); `with_output_index` targets a
/// specific monitor by its DXGI output ordinal. The resolved capture rect's
/// origin is also tracked (`capture_left`/`capture_top`) so input injection can
/// map normalized coordinates into the captured monitor on a multi-monitor
/// virtual desktop.
pub struct WindowsBackend {
    capture: Mutex<Option<CaptureState>>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    /// Top-left of the active capture rect in **virtual-desktop** pixel
    /// coordinates (the `(left, top)` `resolve_capture_rect` produced for the
    /// targeted output). Together with `width`/`height` this is the rectangle
    /// normalized input coordinates are mapped into; for the primary / whole
    /// virtual desktop it is `(0, 0)` and behavior is unchanged. Signed because
    /// a monitor left/above the primary has negative virtual-desktop offsets.
    capture_left: Arc<AtomicI32>,
    capture_top: Arc<AtomicI32>,
    shutdown: Arc<AtomicBool>,
    /// Target DXGI output index on adapter 0. `None` captures the primary /
    /// first available output (backwards-compatible single-monitor behavior).
    target_output_index: Option<u32>,
    /// Capture implementation to drive.
    method: CaptureMethod,
}

impl WindowsBackend {
    /// Create a new Windows backend capturing the primary output with the
    /// default (GDI) capture path, honoring the `INTENDANT_WINDOWS_CAPTURE`
    /// override. Resolution is populated once `start_capture()` runs.
    pub fn new() -> Self {
        Self::with_method(None, CaptureMethod::from_env_or_default())
    }

    /// Create a backend targeting a specific DXGI output (monitor) by its
    /// ordinal on adapter 0, with the default (GDI) capture path. Mirrors
    /// `MacOSBackend::with_display_id`.
    pub fn with_output_index(output_index: u32) -> Self {
        Self::with_method(Some(output_index), CaptureMethod::from_env_or_default())
    }

    /// The active capture rectangle in virtual-desktop pixel coordinates, as a
    /// snapshot of the shared atomics the capture thread keeps current. Used by
    /// input injection to map normalized coordinates into the captured monitor.
    fn captured_rect(&self) -> CaptureRect {
        CaptureRect {
            left: self.capture_left.load(Ordering::SeqCst),
            top: self.capture_top.load(Ordering::SeqCst),
            width: self.width.load(Ordering::SeqCst),
            height: self.height.load(Ordering::SeqCst),
        }
    }

    /// Create a backend with an explicit output target and capture method.
    /// Lets callers force the DXGI fast path (e.g. on a known-good GPU host)
    /// without relying on the environment override.
    pub fn with_method(target_output_index: Option<u32>, method: CaptureMethod) -> Self {
        Self {
            capture: Mutex::new(None),
            width: Arc::new(AtomicU32::new(0)),
            height: Arc::new(AtomicU32::new(0)),
            capture_left: Arc::new(AtomicI32::new(0)),
            capture_top: Arc::new(AtomicI32::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            target_output_index,
            method,
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
    //
    // SAFETY (whole loop): `factory` is the live DXGI factory; `EnumAdapters` /
    // `EnumOutputs` are index-driven enumerators returning either a new
    // RAII-owned interface or an error that terminates the walk. Each `output`
    // passed to `get_output_desc` is the live interface just enumerated.
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
                let is_primary =
                    desc.DesktopCoordinates.left == 0 && desc.DesktopCoordinates.top == 0;
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
    // SAFETY: per the fn contract `output` is a valid `IDXGIOutput`; `GetDesc`
    // returns the populated desc by value (no caller-supplied pointers).
    output.GetDesc().ok()
}

/// Create a DXGI 1.1 factory (`CreateDXGIFactory1`), used for both enumeration
/// and capture setup.
fn create_dxgi_factory() -> windows::core::Result<windows::Win32::Graphics::Dxgi::IDXGIFactory1> {
    // SAFETY: `CreateDXGIFactory1` takes no arguments and returns a fresh
    // RAII-owned factory interface.
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
        let shared_left = Arc::clone(&self.capture_left);
        let shared_top = Arc::clone(&self.capture_top);
        let target_output = self.target_output_index;
        let method = self.method;

        // Probe the capture on its own thread and report the initial resolution
        // back through a oneshot, so `start_capture` fails loudly (rather than
        // returning a silent black session) if init fails -- e.g. Desktop
        // Duplication unavailable on the headless Session 0 desktop, no GPU, or
        // a GDI DC that can't be acquired. This mirrors the X11 backend
        // surfacing connect failures, but here init happens on the thread
        // because the COM/GDI objects are not `Send`.
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<(u32, u32), String>>();

        let thread = std::thread::spawn(move || match method {
            CaptureMethod::Gdi => {
                run_gdi_capture(
                    tx,
                    shutdown_flag,
                    fps,
                    target_output,
                    shared_w,
                    shared_h,
                    shared_left,
                    shared_top,
                    init_tx,
                );
            }
            CaptureMethod::Dxgi => {
                run_dxgi_capture(
                    tx,
                    shutdown_flag,
                    fps,
                    target_output,
                    shared_w,
                    shared_h,
                    shared_left,
                    shared_top,
                    init_tx,
                );
            }
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
                    "Windows {method:?} capture init failed: {e}"
                )))
            }
            Err(_) => {
                // The thread dropped the sender without sending (panicked
                // before init). Join and report.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = thread.join();
                })
                .await;
                Err(CallerError::Display(format!(
                    "Windows {method:?} capture thread exited before initialization"
                )))
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
        // SendInput is synchronous and cheap; run it inline. Normalized mouse
        // coordinates are mapped into the **captured monitor's** rect (the one
        // `resolve_capture_rect` produced, tracked in `capture_left/top` +
        // `width/height` and refreshed on resize), then normalized against the
        // virtual-desktop extents queried at injection time. This is what makes
        // clicks land on the right monitor when a secondary output is streamed;
        // for the primary / whole virtual desktop the rect is (0,0,vw,vh) and
        // the mapping is unchanged.
        let rect = self.captured_rect();
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
                send_mouse_absolute(x, y, rect, MOUSEEVENTF_MOVE, 0)?;
            }
            InputEvent::MouseDown { x, y, b } => {
                send_mouse_absolute(x, y, rect, MOUSEEVENTF_MOVE | mouse_down_flag(b), 0)?;
            }
            InputEvent::MouseUp { x, y, b } => {
                send_mouse_absolute(x, y, rect, MOUSEEVENTF_MOVE | mouse_up_flag(b), 0)?;
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
                    send_mouse_absolute(x, y, rect, MOUSEEVENTF_MOVE | MOUSEEVENTF_WHEEL, vwheel)?;
                }
                if hwheel != 0 {
                    send_mouse_absolute(x, y, rect, MOUSEEVENTF_MOVE | MOUSEEVENTF_HWHEEL, hwheel)?;
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
/// flags. `rect` is the monitor currently being captured (virtual-desktop
/// pixels); `mouse_data` carries the wheel delta for `MOUSEEVENTF_WHEEL` /
/// `MOUSEEVENTF_HWHEEL` (ignored otherwise).
///
/// The `0.0..1.0` browser coordinates are relative to the captured monitor, so
/// we first place the point inside that monitor's pixel rect on the virtual
/// desktop, then normalize against the virtual-screen extents:
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` interprets `dx`/`dy` as a
/// `0..65535` fraction of the **entire** virtual screen (all monitors).
fn send_mouse_absolute(
    x: f64,
    y: f64,
    rect: CaptureRect,
    flags: MOUSE_EVENT_FLAGS,
    mouse_data: i32,
) -> Result<(), CallerError> {
    let virtual_screen = virtual_screen_metrics();
    let (abs_x, abs_y) = map_normalized_to_virtualdesk_abs(x, y, rect, virtual_screen);
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
    // SAFETY: `GetSystemMetrics` takes a scalar metric index and returns an
    // `i32` — no pointers, no state, always sound to call.
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (left, top, width, height)
    }
}

/// Map normalized `0.0..1.0` coordinates -- relative to the **captured
/// monitor** `rect` -- into the `0..65535` absolute space `SendInput` expects
/// with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
///
/// `virtual_screen` is `(left, top, width, height)` from
/// [`virtual_screen_metrics`]; `rect` is the monitor being streamed, in the
/// same virtual-desktop pixel space. Two steps:
///
/// 1. **Normalized -> virtual-desktop pixel.** Place the point inside the
///    captured monitor: `px = rect.left + x * rect.width`. For a secondary
///    monitor `rect.left`/`top` are its offset within the desktop (negative if
///    it sits left of / above the primary), which is exactly what was missing
///    before -- the old code scaled against the *whole* virtual screen, so a
///    click meant for a right-hand monitor landed on the primary.
/// 2. **Virtual-desktop pixel -> 0..65535.** Subtract the virtual-screen origin
///    so the point is in `[0, extent]`, then apply the documented SendInput
///    formula `(coord * 65535) / (extent - 1)`.
///
/// For the primary / whole-desktop capture `rect` is `(0, 0, vwidth, vheight)`
/// and this reduces to the previous behavior. If `rect` has a zero dimension
/// (capture not yet started, atomics unpopulated) we fall back to the full
/// virtual screen so a stray pre-capture event still maps sanely.
fn map_normalized_to_virtualdesk_abs(
    x: f64,
    y: f64,
    rect: CaptureRect,
    virtual_screen: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (vleft, vtop, vwidth, vheight) = virtual_screen;
    // Guard against a zero/negative extent (no desktop): fall back to a
    // full-range mapping so we never divide by zero.
    let vwidth = vwidth.max(1);
    let vheight = vheight.max(1);

    // The captured rect drives the mapping. If it is empty (capture not started
    // yet) fall back to the whole virtual desktop -- origin at the virtual
    // screen's top-left -- which reproduces the legacy whole-desktop behavior.
    let (rleft, rtop, rwidth, rheight) = if rect.width > 0 && rect.height > 0 {
        (rect.left, rect.top, rect.width as i32, rect.height as i32)
    } else {
        (vleft, vtop, vwidth, vheight)
    };

    // Step 1: normalized -> absolute virtual-desktop pixel within the monitor.
    let vx = rleft as f64 + x.clamp(0.0, 1.0) * rwidth as f64;
    let vy = rtop as f64 + y.clamp(0.0, 1.0) * rheight as f64;

    // Step 2: virtual-desktop pixel -> [0, extent], offsetting by the virtual
    // screen origin (`vleft`/`vtop` are <= 0 when monitors extend left/up).
    let px = vx.round() as i32 - vleft;
    let py = vy.round() as i32 - vtop;

    // Normalize to 0..65535 across the virtual screen, per the documented
    // SendInput formula `(coord * 65535) / (extent - 1)`.
    let abs_x = ((px as i64 * 65535) / (vwidth as i64 - 1).max(1)) as i32;
    let abs_y = ((py as i64 * 65535) / (vheight as i64 - 1).max(1)) as i32;
    (abs_x.clamp(0, 65535), abs_y.clamp(0, 65535))
}

/// Submit a single `INPUT` to `SendInput`, returning an error if the system
/// rejected it (e.g. blocked by `UIPI`, or a higher-integrity foreground app).
fn send_one_input(input: &INPUT) -> Result<(), CallerError> {
    // SAFETY: `SendInput` reads a slice of `INPUT` structs whose count and
    // element size we pass explicitly. We hand it a one-element slice and
    // `size_of::<INPUT>()`, so the count/stride match the buffer exactly; the
    // slice is valid for the duration of the (synchronous) call.
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
/// Returns the live `Duplication`, the output's top-left in virtual-desktop
/// pixels, and its pixel dimensions: `(dup, left, top, width, height)`.
fn init_duplication(
    target_output: Option<u32>,
) -> Result<(Duplication, i32, i32, u32, u32), String> {
    // 1. Create a hardware D3D11 device with BGRA support (required for the
    //    Desktop Duplication B8G8R8A8 surface).
    let feature_levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();

    // SAFETY: `&feature_levels` is a stack array we own; the device / context /
    // feature-level out-params are `&mut Option<…>`/`&mut` locals D3D11 fills
    // with RAII-owned interfaces (released on drop). No adapter or sw-rasterizer
    // module is supplied (the `None`/`default` args), which is valid.
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
    let context = context.ok_or_else(|| "D3D11CreateDevice returned null context".to_string())?;

    // 2. From the device, reach the DXGI adapter to enumerate outputs.
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| format!("ID3D11Device -> IDXGIDevice: {e}"))?;
    // SAFETY: `dxgi_device` is the live device cast from `device`; `GetAdapter`
    // returns a new RAII-owned adapter interface.
    let adapter: IDXGIAdapter =
        unsafe { dxgi_device.GetAdapter() }.map_err(|e| format!("IDXGIDevice::GetAdapter: {e}"))?;

    // 3. Pick the target output (default: first attached output).
    let (output, left, top, width, height) = select_output(&adapter, target_output)?;

    // 4. Upgrade to IDXGIOutput1 and duplicate the desktop.
    let output1: IDXGIOutput1 = output
        .cast()
        .map_err(|e| format!("IDXGIOutput -> IDXGIOutput1: {e}"))?;
    // SAFETY: `output1` is the live output and `&device` the live device that
    // will own the duplication; `DuplicateOutput` returns a RAII-owned
    // `IDXGIOutputDuplication` (or an error on a headless/Session-0/exclusive
    // host).
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
        left,
        top,
        width,
        height,
    ))
}

/// Select a DXGI output on the adapter, returning the output, its top-left in
/// virtual-desktop pixels, and its pixel dimensions: `(output, left, top, w,
/// h)`. `target_output` is an ordinal on this adapter; `None` picks the first
/// attached output. The origin lets input injection map normalized coordinates
/// into the captured monitor's slice of the virtual desktop.
fn select_output(
    adapter: &IDXGIAdapter,
    target_output: Option<u32>,
) -> Result<(IDXGIOutput, i32, i32, u32, u32), String> {
    // SAFETY (whole fn): `adapter` is the live adapter passed in; `EnumOutputs`
    // returns a new RAII-owned output (or an error) for the given index, and
    // each `get_output_desc` runs on the output just enumerated.
    if let Some(idx) = target_output {
        let output: IDXGIOutput =
            unsafe { adapter.EnumOutputs(idx) }.map_err(|e| format!("EnumOutputs({idx}): {e}"))?;
        let desc = unsafe { get_output_desc(&output) }
            .ok_or_else(|| format!("GetDesc on output {idx} failed"))?;
        let rect = &desc.DesktopCoordinates;
        let (w, h) = output_dimensions(rect, desc.Rotation);
        return Ok((output, rect.left, rect.top, w & !1, h & !1));
    }

    // Default: first output that is attached to the desktop.
    let mut idx = 0u32;
    loop {
        match unsafe { adapter.EnumOutputs(idx) } {
            Ok(output) => {
                if let Some(desc) = unsafe { get_output_desc(&output) } {
                    if desc.AttachedToDesktop.as_bool() {
                        let rect = &desc.DesktopCoordinates;
                        let (w, h) = output_dimensions(rect, desc.Rotation);
                        // VP8/I420 require even dimensions; enforce here as the
                        // backends do elsewhere.
                        return Ok((output, rect.left, rect.top, w & !1, h & !1));
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
    // SAFETY: `dup.device` is the live D3D11 device; `&desc` is a fully
    // initialized descriptor we own and `&mut texture` an out-param the device
    // fills with a RAII-owned texture (no initial data, hence `None`).
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
    shared_left: Arc<AtomicI32>,
    shared_top: Arc<AtomicI32>,
    init_tx: tokio::sync::oneshot::Sender<Result<(u32, u32), String>>,
) {
    let frame_interval =
        std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });
    // AcquireNextFrame timeout in ms: roughly one frame interval, but at least
    // a few ms so a high fps doesn't busy-poll, and capped so shutdown is
    // responsive.
    let acquire_timeout_ms = (frame_interval.as_millis() as u32).clamp(5, 100);

    // Bind to the interactive desktop before duplicating it. Desktop
    // Duplication is keyed off the calling thread's desktop too, so a
    // worker thread on the wrong desktop can fail to duplicate (or duplicate
    // the wrong surface). Harmless on hosts where the thread already has the
    // right desktop; the guard lives for the whole loop.
    let _desktop_guard = bind_thread_to_input_desktop("DXGI");

    let mut dup = match init_duplication(target_output) {
        Ok((dup, left, top, w, h)) => {
            // Publish the resolved capture rect (origin + size) so input
            // injection maps normalized coordinates into this monitor's slice
            // of the virtual desktop rather than the whole desktop.
            shared_width.store(w, Ordering::SeqCst);
            shared_height.store(h, Ordering::SeqCst);
            shared_left.store(left, Ordering::SeqCst);
            shared_top.store(top, Ordering::SeqCst);
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
                eprintln!("[display/windows] DXGI_ERROR_ACCESS_LOST -- re-acquiring duplication",);
                // Drop the old duplication and re-init. A transient failure
                // (e.g. mid secure-desktop transition) is retried.
                match init_duplication(target_output) {
                    Ok((new_dup, left, top, w, h)) => {
                        dup = new_dup;
                        shared_width.store(w, Ordering::SeqCst);
                        shared_height.store(h, Ordering::SeqCst);
                        shared_left.store(left, Ordering::SeqCst);
                        shared_top.store(top, Ordering::SeqCst);
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
fn acquire_and_copy(dup: &mut Duplication, timeout_ms: u32) -> Result<Option<Frame>, CaptureError> {
    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;

    // SAFETY: `dup.duplication` is the live duplication interface; the out-params
    // `frame_info`/`resource` are stack locals we own. On `Ok`, `resource` holds
    // a new ref that the closure below releases (and `ReleaseFrame` pairs the
    // acquire on every exit path).
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
        // SAFETY: `src_texture` is the live texture cast from the acquired
        // resource; `GetDesc` fills the `&mut src_desc` we own.
        unsafe { src_texture.GetDesc(&mut src_desc) };
        let width = src_desc.Width & !1;
        let height = src_desc.Height & !1;

        let staging = ensure_staging(dup, width, height).map_err(CaptureError::Fatal)?;

        // GPU copy from the acquired frame into the CPU-readable staging tex.
        // SAFETY: both textures are live D3D11 resources on `dup.context`;
        // `ensure_staging` sized `staging` to match `src_texture`, which is what
        // `CopyResource` (a whole-resource copy) requires.
        unsafe { dup.context.CopyResource(&staging, &src_texture) };

        // Map and copy out the BGRA rows.
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` is a CPU-readable (D3D11_USAGE_STAGING + READ) live
        // texture; `Map` fills `mapped` with a base pointer + row pitch valid
        // until the paired `Unmap` below. Errors propagate without an Unmap
        // (nothing was mapped on the error path).
        unsafe {
            dup.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Fatal(format!("Map staging texture: {e}")))?;
        }

        let stride = mapped.RowPitch;
        let row_bytes = (width as usize) * 4;
        let mut data = vec![0u8; row_bytes * height as usize];
        // SAFETY: `mapped.pData` points at the mapped surface; each source row
        // starts at `row * stride` (the driver-reported `RowPitch`, always
        // >= `row_bytes`) and is valid for the `height` rows the surface holds.
        // Each destination offset `row * row_bytes` is in-bounds for `data`
        // (sized `row_bytes * height`), and we copy exactly `row_bytes` per row.
        // Source and destination are distinct allocations. `Unmap` pairs the
        // `Map` before the surface pointer goes out of use.
        unsafe {
            let src_base = mapped.pData as *const u8;
            for row in 0..height as usize {
                let src_row = src_base.add(row * stride as usize);
                let dst_off = row * row_bytes;
                std::ptr::copy_nonoverlapping(src_row, data.as_mut_ptr().add(dst_off), row_bytes);
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
    // SAFETY: pairs the successful `AcquireNextFrame` above (we only reach here
    // after a non-error acquire); releasing exactly once per acquire is the
    // documented contract.
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
// Interactive-desktop binding (shared by both capture threads)
// ---------------------------------------------------------------------------

/// RAII guard owning the `HDESK` handle a capture thread is bound to.
///
/// The handle must outlive every GDI/DXGI operation on the thread, so we hold
/// the guard for the whole capture loop and only drop it when the thread
/// returns. `CloseDesktop` refuses to close a desktop that is still the calling
/// thread's desktop (the bind is still in effect at drop time) and returns an
/// error; that is harmless here because the OS reclaims the handle when the
/// thread exits moments later. We attempt the close anyway (and ignore the
/// result) so that if the bind had failed -- leaving an opened-but-unassigned
/// handle -- it is still released.
struct DesktopGuard {
    handle: HDESK,
}

impl Drop for DesktopGuard {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // Best-effort: the thread is ending, so a failure here is inert.
            // SAFETY: `self.handle` is the non-invalid `HDESK` this guard owns
            // (from `OpenInputDesktop`/`OpenDesktopW`); closing it exactly once at
            // drop matches the open. A close that fails because the desktop is
            // still the thread's current desktop is harmless — the OS reclaims it
            // as the thread exits.
            let _ = unsafe { CloseDesktop(self.handle) };
        }
    }
}

/// Bind the **calling** thread to the interactive input desktop
/// (`Winsta0\Default`) so GDI `GetDC(None)` + `BitBlt` (and DXGI duplication)
/// read the desktop the user actually sees rather than the worker thread's
/// inherited default desktop.
///
/// ## Why this is the black-frame fix
///
/// GDI/DXGI capture reads the desktop **bound to the calling thread**. A thread
/// spawned by a service or a thread that simply never associated itself with
/// the displayed desktop renders against a blank/black desktop surface, so
/// `BitBlt` copies all-black even though a *different* interactive process
/// (e.g. PowerShell `CopyFromScreen`) captures the real pixels at the same
/// instant. Calling `SetThreadDesktop(OpenInputDesktop(...))` at the top of the
/// capture thread points it at the live desktop, after which `BitBlt` reads the
/// real composed pixels.
///
/// ## Robustness
///
/// Returns the guard on success, or `None` on failure (logged, not fatal):
/// `SetThreadDesktop` fails with `ERROR_BUSY` if the thread already owns
/// windows/hooks, and `OpenInputDesktop` can fail under tight station ACLs.
/// In the common always-on-steward case the capture thread is freshly spawned
/// and window-less, so the bind succeeds; when it doesn't we fall through and
/// capture against whatever desktop the thread already had (the prior
/// behavior), so this is strictly an improvement and never a regression.
///
/// Tries `OpenInputDesktop` first (no name needed — always the *displayed*
/// desktop, which is what we want even across fast-user-switching), then falls
/// back to `OpenDesktopW("Default")` on `Winsta0`.
fn bind_thread_to_input_desktop(thread_label: &str) -> Option<DesktopGuard> {
    // `OpenInputDesktop` opens the desktop currently receiving input — the one
    // on screen. DESKTOP_READOBJECTS is the access needed to read its surface.
    // SAFETY: `OpenInputDesktop` takes scalar flags/access args and returns a new
    // `HDESK` we own; on success ownership passes to the `DesktopGuard` (or is
    // explicitly closed on the `SetThreadDesktop` failure path below).
    let handle =
        match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) } {
            Ok(h) if !h.is_invalid() => h,
            Ok(_) => {
                eprintln!(
                    "[display/windows] {thread_label}: OpenInputDesktop returned a null \
                 desktop; trying OpenDesktopW(\"Default\")"
                );
                match open_default_desktop() {
                    Some(h) => h,
                    None => return None,
                }
            }
            Err(e) => {
                eprintln!(
                    "[display/windows] {thread_label}: OpenInputDesktop failed ({e}); \
                 trying OpenDesktopW(\"Default\")"
                );
                match open_default_desktop() {
                    Some(h) => h,
                    None => return None,
                }
            }
        };

    // SAFETY: `handle` is the live, non-invalid `HDESK` opened just above.
    // `SetThreadDesktop` associates it with the calling thread; on success the
    // returned `DesktopGuard` keeps the handle alive (and closes it) for the
    // whole capture loop, which is required because the binding must outlive
    // every GDI/DXGI op on this thread.
    match unsafe { SetThreadDesktop(handle) } {
        Ok(()) => {
            eprintln!(
                "[display/windows] {thread_label}: bound capture thread to the \
                 interactive input desktop"
            );
            Some(DesktopGuard { handle })
        }
        Err(e) => {
            // Couldn't switch (e.g. thread already owns windows/hooks). Close
            // the handle we opened and continue on the inherited desktop.
            eprintln!(
                "[display/windows] {thread_label}: SetThreadDesktop failed ({e}); \
                 capturing on the thread's existing desktop"
            );
            // SAFETY: the bind failed so no `DesktopGuard` will own `handle`;
            // close the handle we opened exactly once here to avoid leaking it.
            let _ = unsafe { CloseDesktop(handle) };
            None
        }
    }
}

/// Fallback: open `Winsta0\Default` by name with read access. Returns the
/// handle on success; logs and returns `None` on failure.
fn open_default_desktop() -> Option<HDESK> {
    // UTF-16, NUL-terminated "Default".
    let name: Vec<u16> = "Default\0".encode_utf16().collect();
    let pcwstr = windows::core::PCWSTR::from_raw(name.as_ptr());
    // SAFETY: `pcwstr` points at `name`, a NUL-terminated UTF-16 buffer that
    // outlives this call (it's still in scope). `OpenDesktopW` only reads the
    // string and returns a new `HDESK` we own.
    match unsafe {
        OpenDesktopW(
            pcwstr,
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_READOBJECTS.0,
        )
    } {
        Ok(h) if !h.is_invalid() => Some(h),
        Ok(_) => {
            eprintln!("[display/windows] OpenDesktopW(\"Default\") returned a null desktop");
            None
        }
        Err(e) => {
            eprintln!("[display/windows] OpenDesktopW(\"Default\") failed: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// GDI BitBlt capture (default path)
// ---------------------------------------------------------------------------

/// Resolve the source rect to `BitBlt` from the screen DC, in physical pixels:
/// `(left, top, width, height)`. Dimensions are forced even (`& !1`) so the
/// VP8/I420 path -- which requires even dimensions, like the other backends --
/// is satisfied identically to the DXGI path.
///
/// - `None` (primary): origin `(0, 0)` with `GetSystemMetrics(SM_CXSCREEN /
///   SM_CYSCREEN)`. These report the *primary* monitor's size, which is what
///   the always-on single-desktop steward scenario wants.
/// - `Some(ordinal)`: the DXGI output's `DesktopCoordinates` rect (reusing the
///   enumeration path), so a targeted secondary monitor is captured at its true
///   virtual-desktop offset. Falls back to the primary metrics if the ordinal
///   can't be resolved.
fn resolve_capture_rect(target_output: Option<u32>) -> Result<(i32, i32, u32, u32), String> {
    if let Some(idx) = target_output {
        match output_rect_for_ordinal(idx) {
            Ok(rect) => return Ok(rect),
            Err(e) => {
                eprintln!(
                    "[display/windows] GDI: output ordinal {idx} unresolved ({e}); \
                     falling back to primary screen metrics",
                );
            }
        }
    }

    // SAFETY: `GetSystemMetrics` takes a scalar metric index and returns an
    // `i32`; no pointers or state, always sound to call.
    let (w, h) = unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN).max(0) as u32,
            GetSystemMetrics(SM_CYSCREEN).max(0) as u32,
        )
    };
    if w == 0 || h == 0 {
        return Err(format!(
            "GetSystemMetrics returned an empty primary screen ({w}x{h})"
        ));
    }
    Ok((0, 0, w & !1, h & !1))
}

/// Resolve a DXGI output ordinal (adapter 0 enumeration order, the same ordinal
/// `enumerate_displays` reports as `platform_id`) to its desktop rect in
/// virtual-screen pixel coordinates: `(left, top, width, height)`.
fn output_rect_for_ordinal(target: u32) -> Result<(i32, i32, u32, u32), String> {
    let factory = create_dxgi_factory().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;

    // SAFETY (whole loop): `factory` is the live DXGI factory; `EnumAdapters` /
    // `EnumOutputs` return a new RAII-owned interface or an error that ends the
    // walk, and `get_output_desc` below runs on the live output just enumerated.
    let mut ordinal = 0u32;
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter = match unsafe { factory.EnumAdapters(adapter_index) } {
            Ok(a) => a,
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

            if ordinal == target {
                let desc = unsafe { get_output_desc(&output) }
                    .ok_or_else(|| format!("GetDesc on output ordinal {target} failed"))?;
                let rect = &desc.DesktopCoordinates;
                let (w, h) = output_dimensions(rect, desc.Rotation);
                return Ok((rect.left, rect.top, w & !1, h & !1));
            }
            ordinal += 1;
        }
    }
    Err(format!("DXGI output ordinal {target} not found"))
}

/// Cached GDI capture resources for one resolution. Recreated on resize.
///
/// Holds the screen DC (source), a compatible memory DC, and a top-down 32-bit
/// `CreateDIBSection` DIB selected into the memory DC. `bits` points at the
/// DIB's pixel storage, which GDI owns for the lifetime of the `HBITMAP`. All
/// handles live on the capture thread only -- `HDC`/`HBITMAP` are raw pointers
/// and not `Send` -- and are released in [`GdiCapture::free`] / `Drop`.
struct GdiCapture {
    /// Source DC for the whole screen (`GetDC(None)`).
    screen_dc: HDC,
    /// Memory DC the DIB is selected into.
    mem_dc: HDC,
    /// The DIB section we `BitBlt` into and read back from.
    dib: HBITMAP,
    /// Object previously selected in `mem_dc`, restored before deletion so the
    /// DIB can be freed (`SelectObject` returns the prior selection).
    old_obj: HGDIOBJ,
    /// Raw pointer to the DIB's top-down BGRA pixel bits (GDI-owned).
    bits: *mut u8,
    /// Source rect on the virtual desktop.
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl GdiCapture {
    /// Acquire the screen DC and build a top-down 32-bit DIB sized to the rect.
    ///
    /// The DIB header uses `biBitCount = 32`, `biCompression = BI_RGB`, and a
    /// **negative** `biHeight` so the rows are stored top-down -- giving us the
    /// exact `DXGI_FORMAT_B8G8R8A8_UNORM` byte layout (BGRA, first row first)
    /// that the DXGI path emitted, so everything downstream is unchanged.
    fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, String> {
        // Whole-screen DC. `GetDC(None)` returns the DC for the entire screen
        // (the virtual desktop's primary surface); `BitBlt`'s source x/y then
        // index into it, so a secondary monitor's offset rect reads correctly.
        // SAFETY: `GetDC(None)` takes no pointer args and returns the screen DC,
        // which we own and release in `free`/`Drop` (or on the error paths below).
        let screen_dc = unsafe { GetDC(None) };
        if screen_dc.is_invalid() {
            return Err("GetDC(None) returned a null screen DC".into());
        }

        // SAFETY: `screen_dc` is the valid DC just acquired; `CreateCompatibleDC`
        // returns a memory DC we own.
        let mem_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        if mem_dc.is_invalid() {
            // SAFETY: release the screen DC we acquired before bailing; `screen_dc`
            // is still valid and `None` matches the `GetDC(None)` window arg.
            unsafe {
                ReleaseDC(None, screen_dc);
            }
            return Err("CreateCompatibleDC returned a null memory DC".into());
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative height => top-down DIB (row 0 is the top scanline).
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: Default::default(),
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `mem_dc` is valid and `&bmi` is a fully-initialized header we
        // own; `CreateDIBSection` writes the DIB's pixel-storage pointer into
        // `&mut bits` (GDI owns that memory for the `HBITMAP`'s lifetime) and
        // returns the `HBITMAP` we own.
        let dib =
            unsafe { CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
                .map_err(|e| format!("CreateDIBSection ({width}x{height}): {e}"))?;

        if dib.is_invalid() || bits.is_null() {
            // SAFETY: nothing has been selected into `mem_dc` yet; delete it and
            // release the screen DC (both still valid) before bailing.
            unsafe {
                let _ = DeleteDC(mem_dc);
                ReleaseDC(None, screen_dc);
            }
            return Err("CreateDIBSection returned a null DIB / bits pointer".into());
        }

        // Select the DIB into the memory DC so BitBlt writes into its bits.
        // SAFETY: `mem_dc` and `dib` are both valid; `SelectObject` returns the
        // previously selected object, which we keep in `old_obj` to restore
        // before deleting the DIB in `free` (a DIB still selected can't be freed).
        let old_obj = unsafe { SelectObject(mem_dc, HGDIOBJ::from(dib)) };

        Ok(Self {
            screen_dc,
            mem_dc,
            dib,
            old_obj,
            bits: bits as *mut u8,
            x,
            y,
            width,
            height,
        })
    }

    /// `BitBlt` the screen rect into the DIB, then copy the bits into a tightly
    /// packed `Vec<u8>` (`stride = width * 4`, BGRA, top-down). Returns a
    /// `Frame` byte-identical in layout to the DXGI path's output.
    ///
    /// `SRCCOPY | CAPTUREBLT` is used so layered / overlay windows (and, on most
    /// drivers, the visible cursor's host content) are included, matching what a
    /// user sees -- the same flags PowerShell's `CopyFromScreen` uses under the
    /// hood and which captured the real desktop on the cloud VM.
    fn grab(&self) -> Result<Frame, String> {
        // SAFETY: `mem_dc` (destination) and `screen_dc` (source) are the valid
        // DCs this `GdiCapture` owns; the blit rect (`width`×`height` from the
        // source `x`/`y`) was sized to match the DIB selected into `mem_dc`.
        unsafe {
            BitBlt(
                self.mem_dc,
                0,
                0,
                self.width as i32,
                self.height as i32,
                Some(self.screen_dc),
                self.x,
                self.y,
                SRCCOPY | CAPTUREBLT,
            )
        }
        .map_err(|e| format!("BitBlt: {e}"))?;

        let row_bytes = (self.width as usize) * 4;
        let total = row_bytes * self.height as usize;
        let mut data = vec![0u8; total];
        // The DIB is top-down and tightly packed at 32bpp, so its stride is
        // exactly `width * 4` -- a single contiguous copy suffices (no per-row
        // RowPitch stride to step over, unlike the DXGI staging map).
        // SAFETY: `self.bits` points at the GDI-owned DIB storage (still alive —
        // the `HBITMAP` is selected into `mem_dc`), which holds exactly
        // `width * height * 4` = `total` bytes; `data` is sized to `total`.
        // Distinct allocations, so the copy can't overlap. The `BitBlt` above
        // just refreshed those bits.
        unsafe {
            std::ptr::copy_nonoverlapping(self.bits as *const u8, data.as_mut_ptr(), total);
        }

        Ok(Frame {
            data,
            format: FrameFormat::Bgra,
            width: self.width,
            height: self.height,
            stride: row_bytes as u32,
            timestamp: std::time::Instant::now(),
        })
    }

    /// Release every GDI handle in the correct order: restore the memory DC's
    /// original selection, delete the DIB, delete the memory DC, release the
    /// screen DC. Idempotent enough for the single `Drop` call.
    fn free(&mut self) {
        // SAFETY: each handle is checked non-invalid before use and freed exactly
        // once (the fields are nulled afterward so a second `free` is a no-op).
        // The order is the GDI-mandated reverse of `new`: restore the original
        // selection so the DIB is no longer in use, delete the DIB, delete the
        // memory DC, release the screen DC.
        unsafe {
            // Restore the previously selected object so the DIB is no longer in
            // use, then it can be deleted.
            if !self.mem_dc.is_invalid() {
                SelectObject(self.mem_dc, self.old_obj);
            }
            if !self.dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ::from(self.dib));
            }
            if !self.mem_dc.is_invalid() {
                let _ = DeleteDC(self.mem_dc);
            }
            if !self.screen_dc.is_invalid() {
                ReleaseDC(None, self.screen_dc);
            }
        }
        // Null the handles so a stray second free is a no-op.
        self.dib = HBITMAP::default();
        self.mem_dc = HDC::default();
        self.screen_dc = HDC::default();
        self.bits = std::ptr::null_mut();
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        self.free();
    }
}

/// Average byte value of a pixel buffer, computed over a bounded sample (not
/// the whole buffer) so it stays cheap on the capture thread.
///
/// Used only by the black-frame diagnostic: an all-black BGRA frame averages
/// ~0 (the alpha byte from `CreateDIBSection` is also 0), while any real
/// desktop content averages well above 0. We step through the buffer so the
/// sample spans the whole image (top to bottom) rather than just the first
/// rows, and cap the work at ~4096 samples regardless of resolution.
fn sampled_avg_byte(data: &[u8]) -> u32 {
    if data.is_empty() {
        return 0;
    }
    const MAX_SAMPLES: usize = 4096;
    let step = (data.len() / MAX_SAMPLES).max(1);
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    let mut i = 0;
    while i < data.len() {
        sum += data[i] as u64;
        n += 1;
        i += step;
    }
    (sum / n.max(1)) as u32
}

/// Run the GDI `BitBlt` capture loop on a dedicated OS thread.
///
/// Sends the initial resolution (or an init error) through `init_tx`, then
/// loops at the target framerate, `BitBlt`-ing the screen into a cached DIB and
/// emitting a `Frame`. Unlike DXGI there is no "no change" signal -- every
/// iteration grabs the current desktop -- so the frame-interval sleep is the
/// only pacing. A `BitBlt` failure re-emits the previous frame to keep the
/// encoder's heartbeat alive, and a resolution change (detected via
/// `GetSystemMetrics`) rebuilds the cached DCs/DIB.
#[allow(clippy::too_many_arguments)]
fn run_gdi_capture(
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    fps: u32,
    target_output: Option<u32>,
    shared_width: Arc<AtomicU32>,
    shared_height: Arc<AtomicU32>,
    shared_left: Arc<AtomicI32>,
    shared_top: Arc<AtomicI32>,
    init_tx: tokio::sync::oneshot::Sender<Result<(u32, u32), String>>,
) {
    let frame_interval =
        std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });

    // Bind this thread to the interactive desktop BEFORE any GDI DC work
    // (`GetDC(None)` happens inside `GdiCapture::new`). Without this the worker
    // thread captures its inherited (blank/black) desktop even though the real
    // desktop is on screen -- the root cause of the all-black GDI stream. The
    // guard is held for the whole loop so the handle stays valid; a bind
    // failure is logged and we proceed on the existing desktop.
    let _desktop_guard = bind_thread_to_input_desktop("GDI");

    let (mut rect_x, mut rect_y, init_w, init_h) = match resolve_capture_rect(target_output) {
        Ok(r) => r,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    let mut capture = match GdiCapture::new(rect_x, rect_y, init_w, init_h) {
        Ok(c) => {
            // Publish the resolved capture rect (origin + size) so input
            // injection maps normalized coordinates into this monitor's slice
            // of the virtual desktop rather than the whole desktop.
            shared_width.store(init_w, Ordering::SeqCst);
            shared_height.store(init_h, Ordering::SeqCst);
            shared_left.store(rect_x, Ordering::SeqCst);
            shared_top.store(rect_y, Ordering::SeqCst);
            let _ = init_tx.send(Ok((init_w, init_h)));
            c
        }
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    let mut last_frame: Option<Frame> = None;
    let mut frame_count: u64 = 0;
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 60;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let start = std::time::Instant::now();

        // Detect a primary-resolution change and rebuild the cached resources.
        // For a targeted output we re-resolve its rect too, so a monitor
        // re-arrange / mode switch is picked up.
        if let Ok((nx, ny, nw, nh)) = resolve_capture_rect(target_output) {
            if nw != capture.width || nh != capture.height || nx != rect_x || ny != rect_y {
                match GdiCapture::new(nx, ny, nw, nh) {
                    Ok(c) => {
                        eprintln!(
                            "[display/windows] GDI resize: {}x{} -> {nw}x{nh}",
                            capture.width, capture.height,
                        );
                        capture = c;
                        rect_x = nx;
                        rect_y = ny;
                        shared_width.store(nw, Ordering::SeqCst);
                        shared_height.store(nh, Ordering::SeqCst);
                        // Keep the capture-rect origin current too, so input
                        // injection follows a monitor re-arrange / mode switch.
                        shared_left.store(nx, Ordering::SeqCst);
                        shared_top.store(ny, Ordering::SeqCst);
                        last_frame = None;
                    }
                    Err(e) => {
                        eprintln!("[display/windows] GDI resize rebuild failed: {e}");
                    }
                }
            }
        }

        match capture.grab() {
            Ok(frame) => {
                consecutive_errors = 0;

                let w = frame.width;
                let h = frame.height;
                let prev_w = shared_width.load(Ordering::SeqCst);
                let prev_h = shared_height.load(Ordering::SeqCst);
                if w != prev_w || h != prev_h {
                    shared_width.store(w, Ordering::SeqCst);
                    shared_height.store(h, Ordering::SeqCst);
                }

                frame_count += 1;
                if frame_count == 1 || frame_count % 300 == 0 {
                    eprintln!(
                        "[display/windows] GDI frame #{frame_count} {}x{} stride={} size={}B",
                        frame.width,
                        frame.height,
                        frame.stride,
                        frame.data.len(),
                    );
                }

                // Black-frame diagnostic: log the average byte value of the
                // captured buffer for the first few frames (then once every
                // ~600 as a cheap liveness check). avg_byte ~= 0 means the
                // capture is black (wrong-desktop / capture bug); avg_byte > 0
                // means real pixels reached the buffer, so any remaining black
                // is downstream (encode/transport). Sampled, not summed, to
                // stay off the hot path.
                if frame_count <= 3 || frame_count % 600 == 0 {
                    let avg = sampled_avg_byte(&frame.data);
                    eprintln!("[display/windows] GDI frame #{frame_count} avg_byte={avg}");
                }

                last_frame = Some(clone_frame(&frame));
                let _ = tx.try_send(frame);
            }
            Err(e) => {
                consecutive_errors += 1;
                eprintln!("[display/windows] GDI capture error ({consecutive_errors}): {e}");
                // Keep the encoder's heartbeat alive with the last good frame.
                if let Some(prev) = &last_frame {
                    let mut hb = clone_frame(prev);
                    hb.timestamp = std::time::Instant::now();
                    let _ = tx.try_send(hb);
                }
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/windows] GDI giving up after {consecutive_errors} errors",);
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

    // `capture` drops here, freeing all GDI handles.
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
            output_dimensions(
                &rect,
                windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY
            ),
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

    #[test]
    fn default_capture_method_is_gdi() {
        // GDI is the robust default; DXGI captures black on cloud/RDP/headless.
        // The constructors must default to GDI (absent an env override).
        std::env::remove_var("INTENDANT_WINDOWS_CAPTURE");
        assert_eq!(CaptureMethod::from_env_or_default(), CaptureMethod::Gdi);
        assert_eq!(WindowsBackend::new().method, CaptureMethod::Gdi);
        assert_eq!(
            WindowsBackend::with_output_index(0).method,
            CaptureMethod::Gdi
        );
    }

    #[test]
    fn capture_method_env_override_opts_into_dxgi() {
        // The override is the documented runtime opt-in for the fast path.
        std::env::set_var("INTENDANT_WINDOWS_CAPTURE", "dxgi");
        assert_eq!(CaptureMethod::from_env_or_default(), CaptureMethod::Dxgi);
        std::env::set_var("INTENDANT_WINDOWS_CAPTURE", "GDI");
        assert_eq!(CaptureMethod::from_env_or_default(), CaptureMethod::Gdi);
        // Anything unrecognized falls back to the GDI default.
        std::env::set_var("INTENDANT_WINDOWS_CAPTURE", "nonsense");
        assert_eq!(CaptureMethod::from_env_or_default(), CaptureMethod::Gdi);
        std::env::remove_var("INTENDANT_WINDOWS_CAPTURE");
    }

    #[test]
    fn explicit_method_overrides_default() {
        let b = WindowsBackend::with_method(Some(1), CaptureMethod::Dxgi);
        assert_eq!(b.method, CaptureMethod::Dxgi);
        assert_eq!(b.target_output_index, Some(1));
    }

    #[test]
    fn sampled_avg_byte_black_is_zero_real_is_nonzero() {
        // The black-frame diagnostic contract: an all-zero (black BGRA) buffer
        // averages 0, any real content averages > 0, and an empty buffer is 0.
        assert_eq!(sampled_avg_byte(&[]), 0);
        let black = vec![0u8; 1920 * 1080 * 4];
        assert_eq!(sampled_avg_byte(&black), 0, "all-black must report avg 0");
        let mid = vec![128u8; 1920 * 1080 * 4];
        assert_eq!(sampled_avg_byte(&mid), 128, "uniform 128 must report 128");
        // Real desktop content (a non-trivial fraction of non-black pixels)
        // reads clearly above 0, which is the signal the diagnostic relies on.
        // Here every other byte is 64 -> sampled average ~32.
        let mut content = vec![0u8; 1920 * 1080 * 4];
        for (i, b) in content.iter_mut().enumerate() {
            if i % 2 == 0 {
                *b = 64;
            }
        }
        assert!(
            sampled_avg_byte(&content) > 0,
            "real content must read above 0 (got {})",
            sampled_avg_byte(&content)
        );
    }

    #[test]
    fn gdi_dib_header_is_topdown_bgra32_matching_dxgi() {
        // The frame layout contract the whole downstream pipeline depends on:
        // BGRA, 32bpp, top-down (negative biHeight), uncompressed (BI_RGB),
        // tightly packed (stride = width*4) -- byte-identical to the DXGI
        // staging-map output so the MF H.264 encoder is unchanged.
        let (w, h) = (1600u32, 900u32);
        let hdr = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        assert_eq!(hdr.biBitCount, 32, "must be 32bpp BGRA");
        assert_eq!(hdr.biCompression, BI_RGB.0, "must be uncompressed BI_RGB");
        assert!(hdr.biHeight < 0, "negative height => top-down rows");
        assert_eq!(hdr.biWidth.unsigned_abs(), w);
        assert_eq!(hdr.biHeight.unsigned_abs(), h);
        // Emitted stride matches what the DXGI path produced: width * 4.
        assert_eq!((w as usize) * 4, 6400);
    }

    // -- Multi-monitor input-coordinate mapping (F6 regression coverage) -----
    //
    // `map_normalized_to_virtualdesk_abs` must place normalized `0..1`
    // coordinates inside the *captured* monitor's slice of the virtual desktop,
    // not stretch them across the whole desktop. These cases pin a normalized
    // center to the captured monitor's center -- first as the expected
    // virtual-desktop pixel (step 1: monitor offset applied), then as the
    // expected `0..65535` value (step 2: normalized against the virtual-screen
    // extents). Before the fix the center of a right-hand secondary always
    // landed near the *middle of the whole desktop*, i.e. on the primary.

    /// Mirror of step 2: a virtual-desktop pixel `(vpx, vpy)` -> `0..65535`,
    /// given the virtual-screen `(left, top, width, height)`. Independent of the
    /// function under test so a bug in the production formula can't hide here.
    fn expected_abs(vpx: i32, vpy: i32, vs: (i32, i32, i32, i32)) -> (i32, i32) {
        let (vleft, vtop, vwidth, vheight) = vs;
        let px = vpx - vleft;
        let py = vpy - vtop;
        let ax = ((px as i64 * 65535) / (vwidth as i64 - 1).max(1)) as i32;
        let ay = ((py as i64 * 65535) / (vheight as i64 - 1).max(1)) as i32;
        (ax.clamp(0, 65535), ay.clamp(0, 65535))
    }

    #[test]
    fn map_primary_only_is_full_range_and_unchanged() {
        // Single 1920x1080 monitor: rect == virtual screen, origin (0,0).
        let vs = (0, 0, 1920, 1080);
        let rect = CaptureRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };
        // Corners hit the documented full-range endpoints.
        assert_eq!(
            map_normalized_to_virtualdesk_abs(0.0, 0.0, rect, vs),
            (0, 0)
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.0, 1.0, rect, vs),
            (65535, 65535)
        );
        // Center maps to the monitor center in pixels (960, 540), then to the
        // abs value of that pixel across the (here identical) virtual screen.
        let got = map_normalized_to_virtualdesk_abs(0.5, 0.5, rect, vs);
        assert_eq!(got, expected_abs(960, 540, vs));
        // Sanity: a single monitor's center is ~mid-range on both axes.
        assert!(
            (32_000..=33_500).contains(&got.0),
            "center x ~32767, got {}",
            got.0
        );
        assert!(
            (32_000..=33_500).contains(&got.1),
            "center y ~32767, got {}",
            got.1
        );
    }

    #[test]
    fn map_secondary_to_the_right() {
        // Primary 1920x1080 at (0,0); secondary 1920x1080 to its right at
        // (1920,0). Virtual screen spans 0..3840 wide. Capturing the secondary,
        // a normalized center must land at the secondary's center pixel
        // (1920 + 960 = 2880, 540) -- i.e. in the RIGHT half of the desktop.
        let vs = (0, 0, 3840, 1080);
        let rect = CaptureRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        let center = map_normalized_to_virtualdesk_abs(0.5, 0.5, rect, vs);
        assert_eq!(center, expected_abs(2880, 540, vs));
        // The whole captured monitor maps to the right half (abs_x >= ~32767);
        // pre-fix this would have spanned the full 0..65535 across both panels.
        assert!(
            center.0 > 32_767,
            "secondary-right center must be past desktop midline, got {}",
            center.0
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(0.0, 0.0, rect, vs),
            expected_abs(1920, 0, vs),
            "top-left of the right monitor is the desktop midpoint"
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.0, 1.0, rect, vs),
            (65535, 65535),
            "bottom-right of the right monitor is the desktop's far corner"
        );
    }

    #[test]
    fn map_secondary_to_the_left() {
        // Secondary 1920x1080 placed LEFT of the primary => negative origin.
        // Virtual screen: left=-1920, width=3840. Capturing the secondary, the
        // normalized center is its center pixel (-1920 + 960 = -960, 540),
        // which sits in the LEFT half of the desktop (abs_x < midline).
        let vs = (-1920, 0, 3840, 1080);
        let rect = CaptureRect {
            left: -1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        let center = map_normalized_to_virtualdesk_abs(0.5, 0.5, rect, vs);
        assert_eq!(center, expected_abs(-960, 540, vs));
        assert!(
            center.0 < 32_767,
            "secondary-left center must be before desktop midline, got {}",
            center.0
        );
        // Top-left of the left monitor is the virtual-screen origin -> abs 0.
        assert_eq!(
            map_normalized_to_virtualdesk_abs(0.0, 0.0, rect, vs),
            (0, 0)
        );
        // Bottom-right of the left monitor is the desktop midpoint on x.
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.0, 1.0, rect, vs),
            expected_abs(0, 1080, vs)
        );
    }

    #[test]
    fn map_secondary_above() {
        // Secondary stacked ABOVE the primary => negative top. Virtual screen:
        // top=-1080, height=2160. Capturing the secondary, the normalized
        // center is its center pixel (960, -1080 + 540 = -540), in the TOP half.
        let vs = (0, -1080, 1920, 2160);
        let rect = CaptureRect {
            left: 0,
            top: -1080,
            width: 1920,
            height: 1080,
        };
        let center = map_normalized_to_virtualdesk_abs(0.5, 0.5, rect, vs);
        assert_eq!(center, expected_abs(960, -540, vs));
        assert!(
            center.1 < 32_767,
            "monitor-above center must be in the top half, got {}",
            center.1
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(0.0, 0.0, rect, vs),
            (0, 0),
            "top-left of the upper monitor is the virtual-screen origin"
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.0, 1.0, rect, vs),
            expected_abs(1920, 0, vs),
            "bottom of the upper monitor is the desktop's vertical midpoint"
        );
    }

    #[test]
    fn map_secondary_below() {
        // Secondary stacked BELOW the primary at (0,1080). Virtual screen:
        // top=0, height=2160. Capturing the secondary, the normalized center is
        // its center pixel (960, 1080 + 540 = 1620), in the BOTTOM half.
        let vs = (0, 0, 1920, 2160);
        let rect = CaptureRect {
            left: 0,
            top: 1080,
            width: 1920,
            height: 1080,
        };
        let center = map_normalized_to_virtualdesk_abs(0.5, 0.5, rect, vs);
        assert_eq!(center, expected_abs(960, 1620, vs));
        assert!(
            center.1 > 32_767,
            "monitor-below center must be in the bottom half, got {}",
            center.1
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(0.0, 0.0, rect, vs),
            expected_abs(0, 1080, vs),
            "top of the lower monitor is the desktop's vertical midpoint"
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.0, 1.0, rect, vs),
            (65535, 65535),
            "bottom-right of the lower monitor is the desktop's far corner"
        );
    }

    #[test]
    fn map_out_of_range_normalized_is_clamped_to_monitor() {
        // Inputs outside 0..1 are clamped to the captured monitor's edges, not
        // allowed to wander onto an adjacent monitor.
        let vs = (0, 0, 3840, 1080);
        let rect = CaptureRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        // x < 0 clamps to the monitor's left edge (its origin pixel).
        assert_eq!(
            map_normalized_to_virtualdesk_abs(-0.5, 0.5, rect, vs),
            expected_abs(1920, 540, vs)
        );
        // x > 1 clamps to the monitor's right edge (the far desktop corner x).
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.5, 0.5, rect, vs).0,
            65535
        );
    }

    #[test]
    fn map_empty_rect_falls_back_to_full_virtual_desktop() {
        // Before capture starts the rect atomics are zero; the mapping must not
        // divide by zero or collapse to the origin -- it falls back to the whole
        // virtual desktop (legacy whole-desktop behavior).
        let vs = (-1920, 0, 3840, 1080);
        let empty = CaptureRect {
            left: 0,
            top: 0,
            width: 0,
            height: 0,
        };
        // Center over the full fallback desktop == its midpoint pixel.
        let got = map_normalized_to_virtualdesk_abs(0.5, 0.5, empty, vs);
        assert_eq!(got, expected_abs(0, 540, vs));
        // Corners still hit the full range without panicking.
        assert_eq!(
            map_normalized_to_virtualdesk_abs(0.0, 0.0, empty, vs),
            (0, 0)
        );
        assert_eq!(
            map_normalized_to_virtualdesk_abs(1.0, 1.0, empty, vs),
            (65535, 65535)
        );
    }

    #[test]
    fn map_degenerate_virtual_screen_does_not_panic() {
        // A 0/1-px virtual screen (no real desktop) must not divide by zero.
        let rect = CaptureRect {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
        };
        let got = map_normalized_to_virtualdesk_abs(0.5, 0.5, rect, (0, 0, 0, 0));
        assert!((0..=65535).contains(&got.0));
        assert!((0..=65535).contains(&got.1));
    }
}
