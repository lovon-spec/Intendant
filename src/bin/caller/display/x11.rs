//! X11 display backend using XShm for frame capture and xdotool for input
//! injection.
//!
//! The XShm capture loop runs on a dedicated `std::thread` (the X11 connection
//! is not `Send` across await points).  Communication with the tokio runtime is
//! via a bounded `mpsc` channel for frames and an `AtomicBool` for shutdown
//! signaling.
//!
//! If the XShm extension is unavailable, the backend falls back to `XGetImage`
//! (slower but always works).

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use crate::error::CallerError;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use x11rb::connection::Connection;

/// Active capture state: holds the thread handle for cleanup.
struct CaptureState {
    thread: std::thread::JoinHandle<()>,
}

/// X11 screen capture and input injection backend.
///
/// Uses `x11rb` with the XShm extension for fast full-screen capture and
/// shells out to `xdotool` for keyboard/mouse/scroll input injection (same
/// approach as the existing `computer_use.rs` X11 backend).
pub struct X11Backend {
    capture: Mutex<Option<CaptureState>>,
    width: AtomicU32,
    height: AtomicU32,
    shutdown: Arc<AtomicBool>,
    display: String,
}

impl X11Backend {
    /// Create a new X11 backend.
    ///
    /// Connects to the X server (using `DISPLAY` env var), queries the root
    /// window dimensions, and caches them.  The connection is dropped after
    /// setup -- the capture thread creates its own connection.
    pub fn new() -> Result<Self, CallerError> {
        let display_str = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());

        // Probe the display to get resolution.
        let (conn, screen_num) = x11rb::connect(Some(&display_str))
            .map_err(|e| CallerError::Display(format!("X11 connect: {e}")))?;

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        // VP8 requires even dimensions.
        let width = (screen.width_in_pixels as u32) & !1;
        let height = (screen.height_in_pixels as u32) & !1;

        Ok(Self {
            capture: Mutex::new(None),
            width: AtomicU32::new(width),
            height: AtomicU32::new(height),
            shutdown: Arc::new(AtomicBool::new(false)),
            display: display_str,
        })
    }

    /// Create a backend targeting a specific X11 display string (e.g. ":0", ":99").
    #[allow(dead_code)]
    pub fn with_display(display_str: &str) -> Result<Self, CallerError> {
        let (conn, screen_num) = x11rb::connect(Some(display_str))
            .map_err(|e| CallerError::Display(format!("X11 connect to {display_str}: {e}")))?;

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let width = (screen.width_in_pixels as u32) & !1;
        let height = (screen.height_in_pixels as u32) & !1;

        Ok(Self {
            capture: Mutex::new(None),
            width: AtomicU32::new(width),
            height: AtomicU32::new(height),
            shutdown: Arc::new(AtomicBool::new(false)),
            display: display_str.to_string(),
        })
    }
}

/// Enumerate X11 displays using xrandr.
///
/// Parses `xrandr --query` output to find connected monitors.  The primary
/// monitor gets `id: 0`; additional monitors get sequential IDs from 1.
/// Falls back to the root window dimensions if xrandr is unavailable.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    let display_str = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());

    // Try xrandr first for multi-monitor enumeration.
    if let Ok(output) = tokio::process::Command::new("xrandr")
        .arg("--query")
        .env("DISPLAY", &display_str)
        .output()
        .await
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let displays = parse_xrandr_output(&text);
            if !displays.is_empty() {
                return displays;
            }
        }
    }

    // Fallback: use x11rb to get the root window size (single display).
    if let Ok((conn, screen_num)) = x11rb::connect(Some(&display_str)) {
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let width = (screen.width_in_pixels as u32) & !1;
        let height = (screen.height_in_pixels as u32) & !1;
        return vec![super::DisplayInfo {
            id: 0,
            platform_id: screen_num as u64,
            name: format!("X11 Screen {} ({}x{})", screen_num, width, height),
            width,
            height,
            is_primary: true,
        }];
    }

    Vec::new()
}

/// Parse xrandr --query output into a list of `DisplayInfo`.
///
/// Looks for lines like:
///   HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
///   DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
fn parse_xrandr_output(text: &str) -> Vec<super::DisplayInfo> {
    let mut displays = Vec::new();
    let mut next_id: u32 = 1;

    for line in text.lines() {
        // Match " connected " lines that include a mode+offset pattern.
        if !line.contains(" connected ") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let output_name = parts[0];
        let is_primary = parts.iter().any(|p| *p == "primary");

        // Find the resolution+offset token: "WxH+X+Y".
        let mode_token = parts.iter().find(|p| {
            let s = **p;
            s.contains('x') && s.contains('+')
        });
        let (width, height) = if let Some(tok) = mode_token {
            parse_mode_token(tok)
        } else {
            continue; // Connected but no active mode — skip.
        };

        if width == 0 || height == 0 {
            continue;
        }

        let id = if is_primary { 0 } else { let id = next_id; next_id += 1; id };

        displays.push(super::DisplayInfo {
            id,
            platform_id: id as u64,
            name: format!("{} ({}x{})", output_name, width, height),
            width,
            height,
            is_primary,
        });
    }

    // Ensure primary is first.
    displays.sort_by_key(|d| if d.is_primary { 0 } else { 1 });
    displays
}

/// Parse "WxH+X+Y" into (width, height).
fn parse_mode_token(tok: &str) -> (u32, u32) {
    // "1920x1080+0+0" → split on 'x' → "1920", "1080+0+0" → split on '+' → "1080"
    let x_pos = match tok.find('x') {
        Some(p) => p,
        None => return (0, 0),
    };
    let w_str = &tok[..x_pos];
    let rest = &tok[x_pos + 1..];
    let h_str = rest.split('+').next().unwrap_or("0");
    let w = w_str.parse::<u32>().unwrap_or(0);
    let h = h_str.parse::<u32>().unwrap_or(0);
    (w, h)
}

#[async_trait]
impl DisplayBackend for X11Backend {
    async fn start_capture(
        &self,
        fps: u32,
    ) -> Result<mpsc::Receiver<Frame>, CallerError> {
        self.shutdown.store(false, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<Frame>(4);
        let shutdown_flag = Arc::clone(&self.shutdown);
        let display_str = self.display.clone();
        let width = self.width.load(Ordering::SeqCst);
        let height = self.height.load(Ordering::SeqCst);

        let thread = std::thread::spawn(move || {
            run_x11_capture(display_str, tx, shutdown_flag, fps, width, height);
        });

        *self.capture.lock().await = Some(CaptureState { thread });
        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(state) = self.capture.lock().await.take() {
            let _ = state.thread.join();
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        let width = self.width.load(Ordering::SeqCst) as f64;
        let height = self.height.load(Ordering::SeqCst) as f64;
        let display = &self.display;

        match event {
            InputEvent::KeyDown { ref code, .. } => {
                if let Some(key_name) = dom_code_to_xdotool_key(code) {
                    run_xdotool(display, &["keydown", key_name]).await?;
                }
            }
            InputEvent::KeyUp { ref code, .. } => {
                if let Some(key_name) = dom_code_to_xdotool_key(code) {
                    run_xdotool(display, &["keyup", key_name]).await?;
                }
            }
            InputEvent::MouseMove { x, y, .. } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                let sx = px.to_string();
                let sy = py.to_string();
                run_xdotool(display, &["mousemove", "--screen", "0", &sx, &sy]).await?;
            }
            InputEvent::MouseDown { x, y, b } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                let sx = px.to_string();
                let sy = py.to_string();
                // Browser button: 0=left, 1=middle, 2=right
                // xdotool button: 1=left, 2=middle, 3=right
                let button = match b {
                    0 => "1",
                    1 => "2",
                    2 => "3",
                    _ => "1",
                };
                run_xdotool(
                    display,
                    &["mousemove", "--screen", "0", &sx, &sy, "mousedown", button],
                )
                .await?;
            }
            InputEvent::MouseUp { x, y, b } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                let sx = px.to_string();
                let sy = py.to_string();
                let button = match b {
                    0 => "1",
                    1 => "2",
                    2 => "3",
                    _ => "1",
                };
                run_xdotool(
                    display,
                    &["mousemove", "--screen", "0", &sx, &sy, "mouseup", button],
                )
                .await?;
            }
            InputEvent::Scroll { x, y, dx, dy } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                let sx = px.to_string();
                let sy = py.to_string();
                // Move to position first.
                run_xdotool(display, &["mousemove", "--screen", "0", &sx, &sy]).await?;
                // Vertical scroll: xdotool button 4=up, 5=down.
                if dy.abs() > f64::EPSILON {
                    let steps = dy.abs().round().max(1.0) as u32;
                    let button = if dy < 0.0 { "4" } else { "5" };
                    let steps_str = steps.to_string();
                    run_xdotool(
                        display,
                        &["click", "--repeat", &steps_str, "--delay", "20", button],
                    )
                    .await?;
                }
                // Horizontal scroll: xdotool button 6=left, 7=right.
                if dx.abs() > f64::EPSILON {
                    let steps = dx.abs().round().max(1.0) as u32;
                    let button = if dx < 0.0 { "6" } else { "7" };
                    let steps_str = steps.to_string();
                    run_xdotool(
                        display,
                        &["click", "--repeat", &steps_str, "--delay", "20", button],
                    )
                    .await?;
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
        "x11"
    }
}

// ---------------------------------------------------------------------------
// xdotool helper
// ---------------------------------------------------------------------------

/// Run an xdotool command on the given display.
async fn run_xdotool(display: &str, args: &[&str]) -> Result<(), CallerError> {
    let output = tokio::process::Command::new("xdotool")
        .env("DISPLAY", display)
        .args(args)
        .output()
        .await
        .map_err(|e| CallerError::Display(format!("xdotool exec: {e}")))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(CallerError::Display(format!(
            "xdotool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

// ---------------------------------------------------------------------------
// DOM code -> xdotool key name mapping
// ---------------------------------------------------------------------------

/// Map a DOM `KeyboardEvent.code` to an xdotool key name (X11 keysym name).
///
/// xdotool accepts X11 keysym names as documented in `<X11/keysymdef.h>`.
/// This is a physical key mapping -- the same limitation as `keymap.rs`
/// (non-US layouts will produce incorrect character output).
fn dom_code_to_xdotool_key(code: &str) -> Option<&'static str> {
    Some(match code {
        // Row 0 -- Escape + Function keys
        "Escape" => "Escape",
        "F1" => "F1",
        "F2" => "F2",
        "F3" => "F3",
        "F4" => "F4",
        "F5" => "F5",
        "F6" => "F6",
        "F7" => "F7",
        "F8" => "F8",
        "F9" => "F9",
        "F10" => "F10",
        "F11" => "F11",
        "F12" => "F12",

        // Row 1 -- Digits
        "Backquote" => "grave",
        "Digit1" => "1",
        "Digit2" => "2",
        "Digit3" => "3",
        "Digit4" => "4",
        "Digit5" => "5",
        "Digit6" => "6",
        "Digit7" => "7",
        "Digit8" => "8",
        "Digit9" => "9",
        "Digit0" => "0",
        "Minus" => "minus",
        "Equal" => "equal",
        "Backspace" => "BackSpace",

        // Row 2 -- QWERTY
        "Tab" => "Tab",
        "KeyQ" => "q",
        "KeyW" => "w",
        "KeyE" => "e",
        "KeyR" => "r",
        "KeyT" => "t",
        "KeyY" => "y",
        "KeyU" => "u",
        "KeyI" => "i",
        "KeyO" => "o",
        "KeyP" => "p",
        "BracketLeft" => "bracketleft",
        "BracketRight" => "bracketright",
        "Backslash" => "backslash",

        // Row 3 -- ASDF
        "CapsLock" => "Caps_Lock",
        "KeyA" => "a",
        "KeyS" => "s",
        "KeyD" => "d",
        "KeyF" => "f",
        "KeyG" => "g",
        "KeyH" => "h",
        "KeyJ" => "j",
        "KeyK" => "k",
        "KeyL" => "l",
        "Semicolon" => "semicolon",
        "Quote" => "apostrophe",
        "Enter" => "Return",

        // Row 4 -- ZXCV
        "ShiftLeft" => "Shift_L",
        "KeyZ" => "z",
        "KeyX" => "x",
        "KeyC" => "c",
        "KeyV" => "v",
        "KeyB" => "b",
        "KeyN" => "n",
        "KeyM" => "m",
        "Comma" => "comma",
        "Period" => "period",
        "Slash" => "slash",
        "ShiftRight" => "Shift_R",

        // Row 5 -- Bottom
        "ControlLeft" => "Control_L",
        "MetaLeft" => "Super_L",
        "AltLeft" => "Alt_L",
        "Space" => "space",
        "AltRight" => "Alt_R",
        "MetaRight" => "Super_R",
        "ControlRight" => "Control_R",

        // Navigation cluster
        "PrintScreen" => "Print",
        "ScrollLock" => "Scroll_Lock",
        "Pause" => "Pause",
        "Insert" => "Insert",
        "Home" => "Home",
        "PageUp" => "Prior",
        "Delete" => "Delete",
        "End" => "End",
        "PageDown" => "Next",

        // Arrow keys
        "ArrowUp" => "Up",
        "ArrowLeft" => "Left",
        "ArrowDown" => "Down",
        "ArrowRight" => "Right",

        // Numpad
        "NumLock" => "Num_Lock",
        "NumpadDivide" => "KP_Divide",
        "NumpadMultiply" => "KP_Multiply",
        "NumpadSubtract" => "KP_Subtract",
        "Numpad7" => "KP_7",
        "Numpad8" => "KP_8",
        "Numpad9" => "KP_9",
        "NumpadAdd" => "KP_Add",
        "Numpad4" => "KP_4",
        "Numpad5" => "KP_5",
        "Numpad6" => "KP_6",
        "Numpad1" => "KP_1",
        "Numpad2" => "KP_2",
        "Numpad3" => "KP_3",
        "NumpadEnter" => "KP_Enter",
        "Numpad0" => "KP_0",
        "NumpadDecimal" => "KP_Decimal",

        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// X11 capture thread
// ---------------------------------------------------------------------------

/// Run the X11 capture loop on a dedicated OS thread.
///
/// Connects to the X server, sets up XShm (or falls back to XGetImage),
/// and loops at the target framerate sending frames via `try_send()`.
fn run_x11_capture(
    display_str: String,
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    fps: u32,
    width: u32,
    height: u32,
) {
    use x11rb::connection::Connection;
    use x11rb::protocol::shm;

    let frame_interval = std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });

    let (conn, screen_num) = match x11rb::connect(Some(&display_str)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/x11] X11 connect failed: {e}");
            return;
        }
    };

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let depth = screen.root_depth;

    // Try to use XShm for fast capture.
    use x11rb::connection::RequestConnection;
    let shm_available = conn
        .extension_information(shm::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();

    if shm_available {
        eprintln!("[display/x11] XShm available, using shared memory capture {}x{}", width, height);
        run_shm_capture(&conn, root, depth, width, height, &tx, &shutdown, frame_interval);
    } else {
        eprintln!("[display/x11] XShm unavailable, falling back to XGetImage {}x{}", width, height);
        run_getimage_capture(&conn, root, depth, width, height, &tx, &shutdown, frame_interval);
    }
}

/// XShm-based capture loop.
fn run_shm_capture(
    conn: &impl x11rb::connection::Connection,
    root: u32,
    _depth: u8,
    width: u32,
    height: u32,
    tx: &mpsc::Sender<Frame>,
    shutdown: &Arc<AtomicBool>,
    frame_interval: std::time::Duration,
) {
    use x11rb::protocol::shm::ConnectionExt as ShmConnectionExt;
    use x11rb::protocol::xproto::ImageFormat;

    // Allocate shared memory segment.
    // 4 bytes per pixel (BGRA), full screen.
    let seg_size = (width as usize) * (height as usize) * 4;

    let shm_id = unsafe {
        libc::shmget(
            libc::IPC_PRIVATE,
            seg_size,
            libc::IPC_CREAT | 0o600,
        )
    };
    if shm_id < 0 {
        eprintln!("[display/x11] shmget failed: {}", std::io::Error::last_os_error());
        return;
    }

    let shm_addr = unsafe { libc::shmat(shm_id, std::ptr::null(), 0) };
    if shm_addr == (-1isize) as *mut libc::c_void {
        eprintln!("[display/x11] shmat failed: {}", std::io::Error::last_os_error());
        unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };
        return;
    }

    // Mark for removal on last detach (cleanup even if we crash).
    unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };

    // Attach to X server.
    let seg = conn.generate_id().unwrap();
    let attach_ok = conn
        .shm_attach(seg, shm_id as u32, false)
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_some();
    if !attach_ok {
        eprintln!("[display/x11] ShmAttach failed, falling back to XGetImage");
        unsafe { libc::shmdt(shm_addr) };
        run_getimage_capture(conn, root, _depth, width, height, tx, shutdown, frame_interval);
        return;
    }

    let mut frame_count: u64 = 0;
    // Track consecutive capture errors to detect hotplug / display loss.
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 30;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let start = std::time::Instant::now();

        let cookie = match conn.shm_get_image(
            root,
            0,
            0,
            width as u16,
            height as u16,
            0xFFFFFFFF, // plane mask: all planes
            ImageFormat::Z_PIXMAP.into(),
            seg,
            0, // offset into shm segment
        ) {
            Ok(c) => c,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] ShmGetImage request failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] ShmGetImage request failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        match cookie.reply() {
            Ok(reply) => {
                consecutive_errors = 0;
                // X11 ZPixmap at depth 24/32 is BGRA (or BGRx).
                // For ShmGetImage the data is written into the shm segment
                // tightly packed at width*4 for depth 24/32.
                let stride = width * 4;
                let data_len = stride as usize * height as usize;

                let data = unsafe {
                    std::slice::from_raw_parts(shm_addr as *const u8, data_len)
                };

                let frame = Frame {
                    data: data.to_vec(),
                    format: FrameFormat::Bgra,
                    width,
                    height,
                    stride,
                    timestamp: std::time::Instant::now(),
                };

                frame_count += 1;
                if frame_count == 1 || frame_count % 300 == 0 {
                    eprintln!(
                        "[display/x11] shm frame #{} {}x{} stride={} size={}B depth={}",
                        frame_count, width, height, stride, data_len, reply.depth
                    );
                }

                let _ = tx.try_send(frame);
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] ShmGetImage reply failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] ShmGetImage reply failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    // Cleanup: detach from X server and shared memory.
    let _ = conn.shm_detach(seg);
    let _ = conn.flush();
    unsafe { libc::shmdt(shm_addr) };
}

/// Fallback XGetImage-based capture loop (no shared memory).
fn run_getimage_capture(
    conn: &impl x11rb::connection::Connection,
    root: u32,
    _depth: u8,
    width: u32,
    height: u32,
    tx: &mpsc::Sender<Frame>,
    shutdown: &Arc<AtomicBool>,
    frame_interval: std::time::Duration,
) {
    use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

    let mut frame_count: u64 = 0;
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 30;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let start = std::time::Instant::now();

        let cookie = match conn.get_image(
            ImageFormat::Z_PIXMAP,
            root,
            0,
            0,
            width as u16,
            height as u16,
            0xFFFFFFFF, // plane mask
        ) {
            Ok(c) => c,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] GetImage request failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] GetImage request failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        match cookie.reply() {
            Ok(reply) => {
                consecutive_errors = 0;
                let data = reply.data;
                // For ZPixmap, bytes_per_line can include padding.
                // x11rb's GetImageReply doesn't expose bytes_per_line directly,
                // but the data is tightly packed for the returned visual format.
                let stride = width * 4;

                let frame = Frame {
                    data,
                    format: FrameFormat::Bgra,
                    width,
                    height,
                    stride,
                    timestamp: std::time::Instant::now(),
                };

                frame_count += 1;
                if frame_count == 1 || frame_count % 300 == 0 {
                    eprintln!(
                        "[display/x11] getimage frame #{} {}x{} stride={} size={}B",
                        frame_count, width, height, stride, frame.data.len()
                    );
                }

                let _ = tx.try_send(frame);
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] GetImage reply failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] GetImage failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_code_to_xdotool_key_letters() {
        assert_eq!(dom_code_to_xdotool_key("KeyA"), Some("a"));
        assert_eq!(dom_code_to_xdotool_key("KeyZ"), Some("z"));
        assert_eq!(dom_code_to_xdotool_key("KeyM"), Some("m"));
    }

    #[test]
    fn dom_code_to_xdotool_key_digits() {
        assert_eq!(dom_code_to_xdotool_key("Digit1"), Some("1"));
        assert_eq!(dom_code_to_xdotool_key("Digit0"), Some("0"));
    }

    #[test]
    fn dom_code_to_xdotool_key_function_keys() {
        assert_eq!(dom_code_to_xdotool_key("F1"), Some("F1"));
        assert_eq!(dom_code_to_xdotool_key("F12"), Some("F12"));
    }

    #[test]
    fn dom_code_to_xdotool_key_modifiers() {
        assert_eq!(dom_code_to_xdotool_key("ShiftLeft"), Some("Shift_L"));
        assert_eq!(dom_code_to_xdotool_key("ShiftRight"), Some("Shift_R"));
        assert_eq!(dom_code_to_xdotool_key("ControlLeft"), Some("Control_L"));
        assert_eq!(dom_code_to_xdotool_key("ControlRight"), Some("Control_R"));
        assert_eq!(dom_code_to_xdotool_key("AltLeft"), Some("Alt_L"));
        assert_eq!(dom_code_to_xdotool_key("AltRight"), Some("Alt_R"));
        assert_eq!(dom_code_to_xdotool_key("MetaLeft"), Some("Super_L"));
        assert_eq!(dom_code_to_xdotool_key("MetaRight"), Some("Super_R"));
    }

    #[test]
    fn dom_code_to_xdotool_key_special() {
        assert_eq!(dom_code_to_xdotool_key("Escape"), Some("Escape"));
        assert_eq!(dom_code_to_xdotool_key("Enter"), Some("Return"));
        assert_eq!(dom_code_to_xdotool_key("Backspace"), Some("BackSpace"));
        assert_eq!(dom_code_to_xdotool_key("Tab"), Some("Tab"));
        assert_eq!(dom_code_to_xdotool_key("Space"), Some("space"));
        assert_eq!(dom_code_to_xdotool_key("CapsLock"), Some("Caps_Lock"));
    }

    #[test]
    fn dom_code_to_xdotool_key_navigation() {
        assert_eq!(dom_code_to_xdotool_key("ArrowUp"), Some("Up"));
        assert_eq!(dom_code_to_xdotool_key("ArrowDown"), Some("Down"));
        assert_eq!(dom_code_to_xdotool_key("ArrowLeft"), Some("Left"));
        assert_eq!(dom_code_to_xdotool_key("ArrowRight"), Some("Right"));
        assert_eq!(dom_code_to_xdotool_key("Insert"), Some("Insert"));
        assert_eq!(dom_code_to_xdotool_key("Delete"), Some("Delete"));
        assert_eq!(dom_code_to_xdotool_key("Home"), Some("Home"));
        assert_eq!(dom_code_to_xdotool_key("End"), Some("End"));
        assert_eq!(dom_code_to_xdotool_key("PageUp"), Some("Prior"));
        assert_eq!(dom_code_to_xdotool_key("PageDown"), Some("Next"));
    }

    #[test]
    fn dom_code_to_xdotool_key_numpad() {
        assert_eq!(dom_code_to_xdotool_key("NumLock"), Some("Num_Lock"));
        assert_eq!(dom_code_to_xdotool_key("Numpad0"), Some("KP_0"));
        assert_eq!(dom_code_to_xdotool_key("Numpad5"), Some("KP_5"));
        assert_eq!(dom_code_to_xdotool_key("NumpadEnter"), Some("KP_Enter"));
        assert_eq!(dom_code_to_xdotool_key("NumpadAdd"), Some("KP_Add"));
        assert_eq!(dom_code_to_xdotool_key("NumpadDecimal"), Some("KP_Decimal"));
    }

    #[test]
    fn dom_code_to_xdotool_key_unknown() {
        assert_eq!(dom_code_to_xdotool_key("BogusKey"), None);
        assert_eq!(dom_code_to_xdotool_key(""), None);
    }

    #[test]
    fn parse_xrandr_single_monitor() {
        let output = "\
Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00    59.94
";
        let displays = parse_xrandr_output(output);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, 0);
        assert!(displays[0].is_primary);
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].height, 1080);
        assert!(displays[0].name.contains("HDMI-1"));
    }

    #[test]
    fn parse_xrandr_multi_monitor() {
        let output = "\
Screen 0: minimum 8 x 8, current 4480 x 1440, maximum 32767 x 32767
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00    59.94
DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440     59.95*+  74.97
";
        let displays = parse_xrandr_output(output);
        assert_eq!(displays.len(), 2);
        // Primary first
        assert_eq!(displays[0].id, 0);
        assert!(displays[0].is_primary);
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].height, 1080);
        // Secondary
        assert_eq!(displays[1].id, 1);
        assert!(!displays[1].is_primary);
        assert_eq!(displays[1].width, 2560);
        assert_eq!(displays[1].height, 1440);
        assert!(displays[1].name.contains("DP-1"));
    }

    #[test]
    fn parse_xrandr_disconnected_ignored() {
        let output = "\
Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
DP-1 disconnected (normal left inverted right x axis y axis)
";
        let displays = parse_xrandr_output(output);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, 0);
    }

    #[test]
    fn parse_mode_token_basic() {
        assert_eq!(parse_mode_token("1920x1080+0+0"), (1920, 1080));
        assert_eq!(parse_mode_token("2560x1440+1920+0"), (2560, 1440));
    }

    #[test]
    fn parse_mode_token_invalid() {
        assert_eq!(parse_mode_token("primary"), (0, 0));
        assert_eq!(parse_mode_token(""), (0, 0));
    }
}
