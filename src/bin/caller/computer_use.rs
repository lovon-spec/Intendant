//! Provider-agnostic computer use abstraction.
//!
//! Defines common CU action types and an executor that dispatches them via
//! platform-specific backends (X11, Wayland, macOS). Provider-specific parsing
//! and result formatting live in `provider.rs`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ── Display backend ──────────────────────────────────────────────────────────

/// Display backend for input simulation and screenshot capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DisplayBackend {
    /// X11: xdotool + ImageMagick import. Works with Xvfb and real X11 DEs.
    X11,
    /// Wayland: ydotool + grim. Requires /dev/uinput access. (not yet implemented)
    #[allow(dead_code)]
    Wayland,
    /// macOS: cliclick + screencapture. Requires accessibility permissions.
    MacOS,
}

impl DisplayBackend {
    /// Detect the display backend from environment or config string.
    pub fn from_config(backend: &str) -> Self {
        match backend {
            "x11" => DisplayBackend::X11,
            "wayland" => DisplayBackend::Wayland,
            "macos" => DisplayBackend::MacOS,
            _ => Self::detect(),
        }
    }

    /// Auto-detect the display backend from the environment.
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            return DisplayBackend::MacOS;
        }
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            return DisplayBackend::Wayland;
        }
        DisplayBackend::X11
    }
}

// ── Display target ──────────────────────────────────────────────────────────

/// Cross-platform display target. Replaces raw display numbers with a
/// platform-agnostic enum that distinguishes between agent-managed virtual
/// displays and the user's active session display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayTarget {
    /// A virtual display managed by intendant (Xvfb on Linux, :99+).
    Virtual { id: u32 },
    /// The user's active session display (:0 on Linux X11, primary display
    /// on macOS). Requires explicit grant via the autonomy system.
    UserSession,
}

impl DisplayTarget {
    /// Return the DISPLAY environment variable string for this target.
    ///
    /// - `Virtual { id: 99 }` → `":99"`
    /// - `UserSession` → queries the login session DISPLAY, falls back to `":0"`
    pub fn display_env_string(&self) -> String {
        match self {
            DisplayTarget::Virtual { id } => format!(":{}", id),
            DisplayTarget::UserSession => {
                if cfg!(target_os = "macos") {
                    // macOS doesn't use DISPLAY for the primary display
                    String::new()
                } else {
                    // On Linux, try to find the login session's DISPLAY.
                    // The caller may have overridden DISPLAY for Xvfb, so we
                    // check INTENDANT_USER_DISPLAY first, then fall back to :0.
                    std::env::var("INTENDANT_USER_DISPLAY")
                        .unwrap_or_else(|_| ":0".to_string())
                }
            }
        }
    }

    /// Return the stream name used in frame/recording registries.
    pub fn stream_name(&self) -> String {
        match self {
            DisplayTarget::Virtual { id } => format!("display_{}", id),
            DisplayTarget::UserSession => "display_user_session".to_string(),
        }
    }

    /// Whether this target refers to the user's session display.
    pub fn is_user_session(&self) -> bool {
        matches!(self, DisplayTarget::UserSession)
    }

    /// Convert a raw display ID to a `DisplayTarget`.
    /// `0` maps to `UserSession`, positive values to `Virtual`.
    pub fn from_display_id(id: i32) -> Self {
        if id <= 0 {
            DisplayTarget::UserSession
        } else {
            DisplayTarget::Virtual { id: id as u32 }
        }
    }

    /// Bridge for `Command.display: Option<i32>` (the JSON wire format).
    /// Returns the explicit target if provided, otherwise the given default.
    pub fn from_command_display(display: Option<i32>, default: Self) -> Self {
        match display {
            Some(id) => Self::from_display_id(id),
            None => default,
        }
    }
}

impl std::fmt::Display for DisplayTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayTarget::Virtual { id } => write!(f, ":{}", id),
            DisplayTarget::UserSession => write!(f, "user_session"),
        }
    }
}

// ── Action types ─────────────────────────────────────────────────────────────

/// A single computer-use action, normalized across all providers.
/// Coordinates are always in absolute pixels (Gemini's 0-999 grid is converted
/// at parse time).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CuAction {
    Click {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    DoubleClick {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
    },
    Scroll {
        x: i32,
        y: i32,
        direction: ScrollDirection,
        #[serde(default = "default_scroll_amount")]
        amount: i32,
    },
    MoveMouse {
        x: i32,
        y: i32,
    },
    Drag {
        start_x: i32,
        start_y: i32,
        end_x: i32,
        end_y: i32,
    },
    Screenshot,
    Wait {
        ms: u64,
    },
}

fn default_scroll_amount() -> i32 { 3 }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// xdotool button number.
    fn xdotool_button(self) -> &'static str {
        match self {
            MouseButton::Left => "1",
            MouseButton::Right => "3",
            MouseButton::Middle => "2",
        }
    }

    /// cliclick action prefix for this button.
    fn cliclick_prefix(self) -> &'static str {
        match self {
            MouseButton::Left => "c",
            MouseButton::Right => "rc",
            // cliclick has no middle-click; use triple-click as closest approximation
            MouseButton::Middle => "tc",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    /// xdotool click button for this scroll direction.
    fn xdotool_button(self) -> &'static str {
        match self {
            ScrollDirection::Up => "4",
            ScrollDirection::Down => "5",
            ScrollDirection::Left => "6",
            ScrollDirection::Right => "7",
        }
    }
}

// ── Tool call / result types ─────────────────────────────────────────────────

/// A parsed CU tool call from a provider response.
#[derive(Debug, Clone)]
pub struct CuToolCall {
    /// Provider's native call ID (for routing results back).
    pub call_id: String,
    /// Parsed actions (one for Anthropic/Gemini, possibly many for OpenAI).
    pub actions: Vec<CuAction>,
    /// Provider-specific metadata (safety checks, etc.).
    pub metadata: CuCallMetadata,
}

/// Provider-specific metadata attached to a CU call.
#[derive(Debug, Clone, Default)]
pub struct CuCallMetadata {
    /// OpenAI: pending safety checks that must be acknowledged in the result.
    pub pending_safety_checks: Vec<serde_json::Value>,
    /// Gemini: safety decision string.
    pub safety_decision: Option<String>,
}

/// Result of executing a CU action.
#[derive(Debug)]
pub struct CuActionResult {
    pub success: bool,
    pub screenshot: Option<ScreenshotData>,
    pub error: Option<String>,
}

/// A captured screenshot.
#[derive(Debug, Clone)]
pub struct ScreenshotData {
    pub path: PathBuf,
    pub base64_png: String,
    pub width: u32,
    pub height: u32,
}

// ── Coordinate transforms ────────────────────────────────────────────────────

/// Convert Gemini's normalized 0-999 coordinates to absolute pixels.
pub fn normalized_to_pixels(
    nx: i32,
    ny: i32,
    display_width: u32,
    display_height: u32,
) -> (i32, i32) {
    let px = ((nx as f64 / 999.0) * display_width as f64).round() as i32;
    let py = ((ny as f64 / 999.0) * display_height as f64).round() as i32;
    (px, py)
}

// ── Executor ─────────────────────────────────────────────────────────────────

/// Execute a batch of CU actions on the given display.
///
/// Returns one result per action. A screenshot is automatically captured after
/// the last non-Screenshot action (all providers expect a screenshot in the
/// result).
pub async fn execute_actions(
    actions: &[CuAction],
    target: DisplayTarget,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    action_counter: &mut u64,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> Vec<CuActionResult> {
    // Virtual displays are always Xvfb (X11), so use X11 tooling for them
    // regardless of the host's detected backend. This lets an agent running
    // on a Wayland host capture its own Xvfb virtual displays with `import`.
    let effective_backend = match target {
        DisplayTarget::Virtual { .. } if backend == DisplayBackend::Wayland => DisplayBackend::X11,
        _ => backend,
    };

    match effective_backend {
        DisplayBackend::Wayland => {
            if let Some(session) = lookup_display_session(session_registry, &target).await {
                return execute_via_session(&session, actions, screenshot_dir, action_counter).await;
            }
            return vec![CuActionResult {
                success: false,
                screenshot: None,
                error: Some(no_wayland_session_message(&target)),
            }];
        }
        DisplayBackend::X11 | DisplayBackend::MacOS => {} // handled below
    }
    let display = target.display_env_string();
    let mut results = Vec::with_capacity(actions.len());
    let mut last_screenshot: Option<ScreenshotData> = None;

    for action in actions {
        let result =
            execute_single(action, &display, effective_backend, screenshot_dir, action_counter).await;
        if let Some(ref s) = result.screenshot {
            last_screenshot = Some(s.clone());
        }
        results.push(result);
    }

    // If the last action was not a Screenshot, auto-capture one.
    let needs_auto_screenshot = actions
        .last()
        .is_some_and(|a| !matches!(a, CuAction::Screenshot));
    if needs_auto_screenshot {
        let auto = take_screenshot(&display, effective_backend, screenshot_dir, action_counter).await;
        match auto {
            Ok(s) => {
                last_screenshot = Some(s.clone());
                results.push(CuActionResult {
                    success: true,
                    screenshot: Some(s),
                    error: None,
                });
            }
            Err(e) => {
                results.push(CuActionResult {
                    success: false,
                    screenshot: None,
                    error: Some(e),
                });
            }
        }
    }

    // Attach the final screenshot to the first result if it doesn't have one
    // (convenience for callers that just want the latest screenshot from the batch).
    if let (Some(screenshot), Some(first)) = (last_screenshot, results.first_mut()) {
        if first.screenshot.is_none() {
            first.screenshot = Some(screenshot);
        }
    }

    results
}

/// Get the logical display size for the main display. Cached after first call.
/// Used to map CU model coordinates (which are in a normalized 1024-wide space)
/// to actual logical points for cliclick/xdotool.
///
/// This is a platform-agnostic *fallback* used when no active capture session
/// is available for the target display. Prefer [`target_pixel_size`] for any
/// code path that knows which `DisplayTarget` is being driven — it returns the
/// true stream/display resolution from the live session registry, which on
/// Wayland is the only way to get the portal-granted stream size.
pub fn logical_display_size() -> (u32, u32) {
    use std::sync::OnceLock;
    static SIZE: OnceLock<(u32, u32)> = OnceLock::new();
    *SIZE.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            #[link(name = "CoreGraphics", kind = "framework")]
            extern "C" {
                fn CGMainDisplayID() -> u32;
                fn CGDisplayPixelsWide(display: u32) -> usize;
                fn CGDisplayPixelsHigh(display: u32) -> usize;
            }
            let (w, h) = unsafe {
                let d = CGMainDisplayID();
                (CGDisplayPixelsWide(d) as u32, CGDisplayPixelsHigh(d) as u32)
            };
            if w > 0 && h > 0 { return (w, h); }
        }
        // Fallback: assume 1:1 mapping
        (1024, 768)
    })
}

/// Resolve the reference pixel size for denormalizing 0-1000 model coordinates.
///
/// Returns the resolution that 0-1000 model coordinates should be scaled
/// against so that the resulting pixel clicks land where the model intended.
/// Preference order:
///
/// 1. **Active capture session** for the target (`session.resolution()`) —
///    this matches the screenshot the model is actually looking at, and on
///    Wayland it is the *only* correct reference because the portal's
///    pointer injection accepts coordinates in stream-pixel space, which is
///    whatever the portal granted (often not the compositor resolution).
/// 2. **Platform display enumeration** (xrandr / x11rb on Linux,
///    CoreGraphics on macOS) — used when no session has been created yet.
/// 3. **`logical_display_size()` fallback** — last resort, only correct on
///    macOS.
pub async fn target_pixel_size(
    target: DisplayTarget,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> (u32, u32) {
    if let Some(session) = lookup_display_session(session_registry, &target).await {
        let (w, h) = session.resolution();
        if w > 0 && h > 0 {
            return (w, h);
        }
    }

    #[cfg(target_os = "linux")]
    {
        let display_id = match target {
            DisplayTarget::UserSession => 0,
            DisplayTarget::Virtual { id } => id,
        };
        let displays = crate::display::x11::enumerate_displays().await;
        if let Some(d) = displays.iter().find(|d| d.id == display_id) {
            if d.width > 0 && d.height > 0 {
                return (d.width, d.height);
            }
        }
    }

    logical_display_size()
}

/// Screenshots are resized to logical display size before sending to the model,
/// so model coordinates are already in logical (cliclick/xdotool) space.
/// This function is a no-op but kept as the single place to adjust if needed.
fn scale_coords(x: i32, y: i32) -> (i32, i32) {
    (x, y)
}

/// Execute a single CU action, dispatching to the appropriate backend.
async fn execute_single(
    action: &CuAction,
    display: &str,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    counter: &mut u64,
) -> CuActionResult {
    match action {
        CuAction::Click { x, y, button } => match backend {
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                // Move first so hover-to-reveal UIs register the pointer,
                // then click. Without this, UIs like Element's call controls
                // don't respond because cliclick's c: doesn't hover first.
                run_cliclick(&[
                    &format!("m:{},{}", sx, sy),
                    "w:50",
                    &format!("{}:{},{}", button.cliclick_prefix(), sx, sy),
                ]).await
            }
            _ => {
                run_xdotool(display, &[
                    "mousemove", "--sync", &x.to_string(), &y.to_string(),
                    "click", button.xdotool_button(),
                ]).await
            }
        },
        CuAction::DoubleClick { x, y, .. } => match backend {
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                run_cliclick(&[&format!("dc:{},{}", sx, sy)]).await
            }
            _ => {
                run_xdotool(display, &[
                    "mousemove", "--sync", &x.to_string(), &y.to_string(),
                    "click", "--repeat", "2", "--delay", "50",
                    MouseButton::Left.xdotool_button(),
                ]).await
            }
        },
        CuAction::Type { text } => match backend {
            DisplayBackend::MacOS => {
                // CU models often append \n to Type text expecting Enter.
                // cliclick's t: command types literally, so strip \n and
                // append kp:return as a separate keystroke.
                let has_newline = text.ends_with('\n');
                let clean = text.trim_end_matches('\n');
                let mut args = vec![format!("t:{}", clean)];
                if has_newline {
                    args.push("kp:return".to_string());
                }
                let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                run_cliclick(&refs).await
            }
            _ => {
                run_xdotool(display, &["type", "--clearmodifiers", text]).await
            }
        },
        CuAction::Key { key } => match backend {
            DisplayBackend::MacOS => {
                execute_macos_key(key).await
            }
            _ => {
                run_xdotool(display, &["key", "--clearmodifiers", key]).await
            }
        },
        CuAction::Scroll { x, y, direction, amount } => match backend {
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                execute_macos_scroll(sx, sy, *direction, *amount).await
            }
            _ => {
                let mut result = run_xdotool(display, &[
                    "mousemove", "--sync", &x.to_string(), &y.to_string(),
                ]).await;
                if result.success {
                    let btn = direction.xdotool_button();
                    let amt = (*amount).max(1);
                    result = run_xdotool(display, &[
                        "click", "--repeat", &amt.to_string(), "--delay", "20", btn,
                    ]).await;
                }
                result
            }
        },
        CuAction::MoveMouse { x, y } => match backend {
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                run_cliclick(&[&format!("m:{},{}", sx, sy)]).await
            }
            _ => {
                run_xdotool(display, &[
                    "mousemove", "--sync", &x.to_string(), &y.to_string(),
                ]).await
            }
        },
        CuAction::Drag { start_x, start_y, end_x, end_y } => match backend {
            DisplayBackend::MacOS => {
                let (sx1, sy1) = scale_coords(*start_x, *start_y);
                let (sx2, sy2) = scale_coords(*end_x, *end_y);
                run_cliclick(&[
                    &format!("dd:{},{}", sx1, sy1),
                    &format!("du:{},{}", sx2, sy2),
                ]).await
            }
            _ => {
                run_xdotool(display, &[
                    "mousemove", "--sync", &start_x.to_string(), &start_y.to_string(),
                    "mousedown", "1",
                    "mousemove", "--sync", &end_x.to_string(), &end_y.to_string(),
                    "mouseup", "1",
                ]).await
            }
        },
        CuAction::Screenshot => {
            match take_screenshot(display, backend, screenshot_dir, counter).await {
                Ok(s) => CuActionResult {
                    success: true,
                    screenshot: Some(s),
                    error: None,
                },
                Err(e) => CuActionResult {
                    success: false,
                    screenshot: None,
                    error: Some(e),
                },
            }
        }
        CuAction::Wait { ms } => {
            tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            CuActionResult {
                success: true,
                screenshot: None,
                error: None,
            }
        }
    }
}

// ── X11 backend (xdotool) ───────────────────────────────────────────────────

/// Run an xdotool command on the given display.
async fn run_xdotool(display: &str, args: &[&str]) -> CuActionResult {
    let output = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(args)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        },
        Ok(o) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(String::from_utf8_lossy(&o.stderr).to_string()),
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(format!("xdotool exec error: {}", e)),
        },
    }
}

// ── macOS backend (cliclick + osascript) ─────────────────────────────────────

/// Run a cliclick command with the given action arguments.
async fn run_cliclick(args: &[&str]) -> CuActionResult {
    let output = Command::new("cliclick")
        .args(args)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        },
        Ok(o) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(String::from_utf8_lossy(&o.stderr).to_string()),
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(format!("cliclick exec error (is cliclick installed?): {}", e)),
        },
    }
}

/// Translate an xdotool-style key name to cliclick key press syntax.
///
/// Handles single keys and modifier combos (e.g. "ctrl+c" → "kd:ctrl kp:c ku:ctrl").
fn translate_key_for_cliclick(key: &str) -> Vec<String> {
    // Check for modifier combo (e.g. "ctrl+c", "super+shift+a")
    if key.contains('+') {
        let parts: Vec<&str> = key.split('+').collect();
        if parts.len() >= 2 {
            let modifiers: Vec<&str> = parts[..parts.len() - 1].to_vec();
            let base_key = parts[parts.len() - 1];
            let mut args = Vec::new();
            // Press modifiers down
            for m in &modifiers {
                args.push(format!("kd:{}", translate_single_key(m)));
            }
            // Press the base key — use kp: for special keys, t: for single characters
            let translated = translate_single_key(base_key);
            if translated == base_key && base_key.len() == 1 {
                // Single character (e.g. 'w', 'a') — cliclick kp: doesn't accept these
                args.push(format!("t:{}", base_key));
            } else {
                args.push(format!("kp:{}", translated));
            }
            // Release modifiers in reverse
            for m in modifiers.iter().rev() {
                args.push(format!("ku:{}", translate_single_key(m)));
            }
            return args;
        }
    }
    let translated = translate_single_key(key);
    if translated == key && key.len() == 1 {
        vec![format!("t:{}", key)]
    } else {
        vec![format!("kp:{}", translated)]
    }
}

/// Map a single key name from xdotool convention to cliclick convention.
fn translate_single_key(key: &str) -> &str {
    match key.to_lowercase().as_str() {
        "return" | "enter" | "kp_enter" => "return",
        "tab" => "tab",
        "escape" | "esc" => "esc",
        "space" => "space",
        "backspace" => "delete",
        "delete" => "fwd-delete",
        "up" => "arrow-up",
        "down" => "arrow-down",
        "left" => "arrow-left",
        "right" => "arrow-right",
        "home" => "home",
        "end" => "end",
        "prior" | "page_up" | "pageup" => "page-up",
        "next" | "page_down" | "pagedown" => "page-down",
        "ctrl" | "control" | "control_l" | "control_r" => "ctrl",
        "alt" | "alt_l" | "alt_r" => "alt",
        "shift" | "shift_l" | "shift_r" => "shift",
        "super" | "super_l" | "super_r" | "meta" | "cmd" | "command" => "cmd",
        "f1" => "f1",
        "f2" => "f2",
        "f3" => "f3",
        "f4" => "f4",
        "f5" => "f5",
        "f6" => "f6",
        "f7" => "f7",
        "f8" => "f8",
        "f9" => "f9",
        "f10" => "f10",
        "f11" => "f11",
        "f12" => "f12",
        // cliclick accepts single characters directly
        _ => {
            // Can't return a computed value from a match arm that borrows,
            // so for unrecognized keys, return the input as-is via leak-free path.
            // The caller already owns the key string.
            key
        }
    }
}

/// Execute a key press on macOS via cliclick.
async fn execute_macos_key(key: &str) -> CuActionResult {
    let args = translate_key_for_cliclick(key);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_cliclick(&arg_refs).await
}

/// Execute a scroll action on macOS.
///
/// Moves the mouse to (x, y) via cliclick, then uses osascript to post
/// scroll wheel events via CGEvent.
async fn execute_macos_scroll(
    x: i32,
    y: i32,
    direction: ScrollDirection,
    amount: i32,
) -> CuActionResult {
    // Move mouse to target position first
    let move_result = run_cliclick(&[&format!("m:{},{}", x, y)]).await;
    if !move_result.success {
        return move_result;
    }

    let amt = amount.max(1);
    // CGEvent scroll: positive = up/left, negative = down/right
    let (dy, dx) = match direction {
        ScrollDirection::Up => (amt, 0),
        ScrollDirection::Down => (-amt, 0),
        ScrollDirection::Left => (0, amt),
        ScrollDirection::Right => (0, -amt),
    };

    // Use osascript + ObjC bridge to post a CGEvent scroll wheel event
    let script = format!(
        concat!(
            "use framework \"ApplicationServices\"\n",
            "set e to current application's CGEventCreateScrollWheelEvent(",
            "missing value, 0, 2, {}, {})\n",
            "current application's CGEventPost(0, e)"
        ),
        dy, dx
    );

    let output = Command::new("osascript")
        .args(["-l", "AppleScript", "-e", &script])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        },
        Ok(o) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(String::from_utf8_lossy(&o.stderr).to_string()),
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(format!("osascript exec error: {}", e)),
        },
    }
}

// ── Screenshot capture ──────────────────────────────────────────────────────

/// Capture a screenshot using the appropriate backend tool.
///
/// X11: ImageMagick `import -window root -display :N`.
/// macOS: `screencapture -x` (captures primary display, silent).
async fn take_screenshot(
    display: &str,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    counter: &mut u64,
) -> Result<ScreenshotData, String> {
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_screenshot_{}.png", counter));

    let output = match backend {
        DisplayBackend::MacOS => {
            Command::new("screencapture")
                .args(["-x", &path.to_string_lossy()])
                .output()
                .await
                .map_err(|e| format!("screencapture exec error: {}", e))?
        }
        _ => {
            Command::new("import")
                .args(["-window", "root", "-display", display, &path.to_string_lossy()])
                .output()
                .await
                .map_err(|e| format!("import exec error: {}", e))?
        }
    };

    if !output.status.success() {
        let tool = if backend == DisplayBackend::MacOS {
            "screencapture"
        } else {
            "import"
        };
        return Err(format!(
            "{} failed: {}",
            tool,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Read file, resize to logical display size (so model coordinates = cliclick
    // coordinates), and encode as base64.
    let raw_bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read screenshot: {}", e))?;

    let (raw_w, raw_h) = png_dimensions(&raw_bytes).unwrap_or((0, 0));
    let (logical_w, logical_h) = logical_display_size();

    let bytes = if raw_w > logical_w && logical_w > 0 && logical_h > 0 {
        // Resize to logical display size so model coords = logical coords
        match image::load_from_memory(&raw_bytes) {
            Ok(img) => {
                let resized = img.resize_exact(
                    logical_w,
                    logical_h,
                    image::imageops::FilterType::Triangle,
                );
                let mut buf = std::io::Cursor::new(Vec::new());
                if resized.write_to(&mut buf, image::ImageFormat::Png).is_ok() {
                    buf.into_inner()
                } else {
                    raw_bytes
                }
            }
            Err(_) => raw_bytes,
        }
    } else {
        raw_bytes
    };

    let (width, height) = png_dimensions(&bytes).unwrap_or((raw_w, raw_h));

    use base64::Engine;
    let base64_png = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ScreenshotData {
        path,
        base64_png,
        width,
        height,
    })
}

/// Extract width and height from a PNG file header.
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 {
        return None;
    }
    // PNG IHDR chunk starts at byte 16, width at 16..20, height at 20..24
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((width, height))
}

// ── Wayland: DisplaySession routing ─────────────────────────────────────────

/// Build an actionable error for the "no Wayland session" failure path.
/// The previous message ("No display session for Wayland target") left callers
/// with no hint about what's wrong or how to recover, which caused external
/// agents to retry the same call indefinitely.
fn no_wayland_session_message(target: &DisplayTarget) -> String {
    let granted = std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok();
    match target {
        DisplayTarget::UserSession => {
            if granted {
                "No active display capture session on Wayland. The screen-sharing \
                 portal dialog is either pending approval on the physical display \
                 or was denied. Approve the dialog to enable capture, or target a \
                 virtual Xvfb display (e.g. display_target=\":99\") instead."
                    .to_string()
            } else {
                "No active display capture session on Wayland. User display access \
                 has not been granted — call grant_user_display first, then approve \
                 the screen-sharing portal dialog on the physical display. \
                 Alternatively, target a virtual Xvfb display (e.g. \
                 display_target=\":99\")."
                    .to_string()
            }
        }
        DisplayTarget::Virtual { id } => format!(
            "No virtual display :{id} is active. Start one with \
             `Xvfb :{id} -screen 0 1920x1080x24 &` before taking a screenshot, \
             or target the user session with display_target=\"user_session\"."
        ),
    }
}

/// Look up the `DisplaySession` for the given target from the shared registry.
async fn lookup_display_session(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
    target: &DisplayTarget,
) -> Option<std::sync::Arc<crate::display::DisplaySession>> {
    let registry = session_registry.as_ref()?;
    let display_id = match target {
        DisplayTarget::UserSession => 0,
        DisplayTarget::Virtual { id } => *id,
    };
    registry.read().await.get(display_id)
}

/// Execute CU actions by routing through a `DisplaySession` (WebRTC pipeline).
///
/// Converts CU pixel coordinates to normalised 0.0..1.0 coordinates expected by
/// `InputEvent`, and maps `CuAction` variants to sequences of `InputEvent`
/// injections.
async fn execute_via_session(
    session: &crate::display::DisplaySession,
    actions: &[CuAction],
    screenshot_dir: &std::path::Path,
    action_counter: &mut u64,
) -> Vec<CuActionResult> {
    let (width, height) = session.resolution();
    let mut results = Vec::with_capacity(actions.len());
    let mut needs_auto_screenshot = false;

    for action in actions {
        match action {
            CuAction::Screenshot => {
                let result = take_session_screenshot(session, screenshot_dir, action_counter).await;
                results.push(result);
                needs_auto_screenshot = false;
            }
            CuAction::Click { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let down = session
                    .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let up = session
                    .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                    .await;
                let success = down.is_ok() && up.is_ok();
                let error = if !success {
                    Some("Click injection failed".to_string())
                } else {
                    None
                };
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error,
                });
                needs_auto_screenshot = true;
            }
            CuAction::DoubleClick { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let mut success = true;
                for _ in 0..2 {
                    if session
                        .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                        .await
                        .is_err()
                    {
                        success = false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    if session
                        .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                        .await
                        .is_err()
                    {
                        success = false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some("DoubleClick injection failed".to_string())
                    },
                });
                needs_auto_screenshot = true;
            }
            CuAction::Type { text } => {
                let mut success = true;
                for ch in text.chars() {
                    let code = char_to_dom_code(ch);
                    let shift = ch.is_uppercase() || "!@#$%^&*()_+{}|:\"<>?~".contains(ch);
                    if shift {
                        let _ = session
                            .inject_input(crate::display::InputEvent::KeyDown {
                                code: "ShiftLeft".to_string(),
                                key: "Shift".to_string(),
                                shift: true,
                                ctrl: false,
                                alt: false,
                                meta: false,
                            })
                            .await;
                    }
                    if session
                        .inject_input(crate::display::InputEvent::KeyDown {
                            code: code.to_string(),
                            key: ch.to_string(),
                            shift,
                            ctrl: false,
                            alt: false,
                            meta: false,
                        })
                        .await
                        .is_err()
                    {
                        success = false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    let _ = session
                        .inject_input(crate::display::InputEvent::KeyUp {
                            code: code.to_string(),
                            key: ch.to_string(),
                            shift,
                            ctrl: false,
                            alt: false,
                            meta: false,
                        })
                        .await;
                    if shift {
                        let _ = session
                            .inject_input(crate::display::InputEvent::KeyUp {
                                code: "ShiftLeft".to_string(),
                                key: "Shift".to_string(),
                                shift: false,
                                ctrl: false,
                                alt: false,
                                meta: false,
                            })
                            .await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some("Type injection failed".to_string())
                    },
                });
                needs_auto_screenshot = true;
            }
            CuAction::Key { key } => {
                let code = key_name_to_dom_code(key);
                let down = session
                    .inject_input(crate::display::InputEvent::KeyDown {
                        code: code.to_string(),
                        key: key.clone(),
                        shift: false,
                        ctrl: false,
                        alt: false,
                        meta: false,
                    })
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                let up = session
                    .inject_input(crate::display::InputEvent::KeyUp {
                        code: code.to_string(),
                        key: key.clone(),
                        shift: false,
                        ctrl: false,
                        alt: false,
                        meta: false,
                    })
                    .await;
                let success = down.is_ok() && up.is_ok();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some("Key injection failed".to_string())
                    },
                });
                needs_auto_screenshot = true;
            }
            CuAction::Scroll {
                x,
                y,
                direction,
                amount,
            } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                // Convert ScrollDirection + amount to pixel deltas.
                let amt = (*amount).max(1) as f64;
                let (dx, dy) = match direction {
                    ScrollDirection::Up => (0.0, -amt),
                    ScrollDirection::Down => (0.0, amt),
                    ScrollDirection::Left => (-amt, 0.0),
                    ScrollDirection::Right => (amt, 0.0),
                };
                let r = session
                    .inject_input(crate::display::InputEvent::Scroll {
                        x: nx,
                        y: ny,
                        dx,
                        dy,
                    })
                    .await;
                results.push(CuActionResult {
                    success: r.is_ok(),
                    screenshot: None,
                    error: r.err().map(|e| e.to_string()),
                });
                needs_auto_screenshot = true;
            }
            CuAction::MoveMouse { x, y } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let r = session
                    .inject_input(crate::display::InputEvent::MouseMove { x: nx, y: ny, buttons: 0 })
                    .await;
                results.push(CuActionResult {
                    success: r.is_ok(),
                    screenshot: None,
                    error: r.err().map(|e| e.to_string()),
                });
                needs_auto_screenshot = true;
            }
            CuAction::Drag {
                start_x,
                start_y,
                end_x,
                end_y,
            } => {
                let sx = *start_x as f64 / width as f64;
                let sy = *start_y as f64 / height as f64;
                let ex = *end_x as f64 / width as f64;
                let ey = *end_y as f64 / height as f64;
                // Drag uses left button (0).
                let _ = session
                    .inject_input(crate::display::InputEvent::MouseDown {
                        x: sx,
                        y: sy,
                        b: 0,
                    })
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                // Interpolate intermediate points for smooth drag.
                for i in 1..=5 {
                    let t = i as f64 / 5.0;
                    let mx = sx + (ex - sx) * t;
                    let my = sy + (ey - sy) * t;
                    let _ = session
                        .inject_input(crate::display::InputEvent::MouseMove { x: mx, y: my, buttons: 0 })
                        .await;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                let _ = session
                    .inject_input(crate::display::InputEvent::MouseUp {
                        x: ex,
                        y: ey,
                        b: 0,
                    })
                    .await;
                results.push(CuActionResult {
                    success: true,
                    screenshot: None,
                    error: None,
                });
                needs_auto_screenshot = true;
            }
            CuAction::Wait { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                results.push(CuActionResult {
                    success: true,
                    screenshot: None,
                    error: None,
                });
            }
        }
    }

    // Auto-screenshot after the last non-screenshot action (matches X11 path).
    if needs_auto_screenshot {
        let auto = take_session_screenshot(session, screenshot_dir, action_counter).await;
        if auto.success {
            let screenshot = auto.screenshot.clone();
            results.push(auto);
            // Attach to first result if it has no screenshot (convenience for callers).
            if let (Some(ss), Some(first)) = (screenshot, results.first_mut()) {
                if first.screenshot.is_none() {
                    first.screenshot = Some(ss);
                }
            }
        } else {
            results.push(auto);
        }
    }

    results
}

/// Capture a PNG screenshot from a `DisplaySession`.
async fn take_session_screenshot(
    session: &crate::display::DisplaySession,
    screenshot_dir: &std::path::Path,
    counter: &mut u64,
) -> CuActionResult {
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_screenshot_{}.png", counter));
    match session.screenshot().await {
        Ok(png_bytes) => match std::fs::write(&path, &png_bytes) {
            Ok(_) => {
                let (width, height) = png_dimensions(&png_bytes).unwrap_or((0, 0));
                use base64::Engine;
                let base64_png =
                    base64::engine::general_purpose::STANDARD.encode(&png_bytes);
                CuActionResult {
                    success: true,
                    screenshot: Some(ScreenshotData {
                        path,
                        base64_png,
                        width,
                        height,
                    }),
                    error: None,
                }
            }
            Err(e) => CuActionResult {
                success: false,
                screenshot: None,
                error: Some(format!("Failed to write screenshot: {}", e)),
            },
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(format!("Screenshot failed: {}", e)),
        },
    }
}

/// Map a `MouseButton` to the browser button index used by `InputEvent`.
fn mouse_button_index(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Map a character to a DOM `KeyboardEvent.code` value.
fn char_to_dom_code(ch: char) -> &'static str {
    match ch.to_ascii_lowercase() {
        'a' => "KeyA",
        'b' => "KeyB",
        'c' => "KeyC",
        'd' => "KeyD",
        'e' => "KeyE",
        'f' => "KeyF",
        'g' => "KeyG",
        'h' => "KeyH",
        'i' => "KeyI",
        'j' => "KeyJ",
        'k' => "KeyK",
        'l' => "KeyL",
        'm' => "KeyM",
        'n' => "KeyN",
        'o' => "KeyO",
        'p' => "KeyP",
        'q' => "KeyQ",
        'r' => "KeyR",
        's' => "KeyS",
        't' => "KeyT",
        'u' => "KeyU",
        'v' => "KeyV",
        'w' => "KeyW",
        'x' => "KeyX",
        'y' => "KeyY",
        'z' => "KeyZ",
        '0' | ')' => "Digit0",
        '1' | '!' => "Digit1",
        '2' | '@' => "Digit2",
        '3' | '#' => "Digit3",
        '4' | '$' => "Digit4",
        '5' | '%' => "Digit5",
        '6' | '^' => "Digit6",
        '7' | '&' => "Digit7",
        '8' | '*' => "Digit8",
        '9' | '(' => "Digit9",
        ' ' => "Space",
        '\n' | '\r' => "Enter",
        '\t' => "Tab",
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "BracketLeft",
        ']' | '}' => "BracketRight",
        '\\' | '|' => "Backslash",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Quote",
        '`' | '~' => "Backquote",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        _ => "Unidentified",
    }
}

/// Map a key name (from CU action) to a DOM `KeyboardEvent.code` value.
fn key_name_to_dom_code(key: &str) -> &str {
    match key.to_lowercase().as_str() {
        "enter" | "return" => "Enter",
        "escape" | "esc" => "Escape",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "space" | " " => "Space",
        "arrowup" | "up" => "ArrowUp",
        "arrowdown" | "down" => "ArrowDown",
        "arrowleft" | "left" => "ArrowLeft",
        "arrowright" | "right" => "ArrowRight",
        "delete" => "Delete",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        _ => key, // pass through as-is
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_to_pixels_corners() {
        assert_eq!(normalized_to_pixels(0, 0, 1440, 900), (0, 0));
        assert_eq!(normalized_to_pixels(999, 999, 1440, 900), (1440, 900));
        assert_eq!(normalized_to_pixels(500, 500, 1440, 900), (721, 450));
    }

    #[test]
    fn mouse_button_xdotool() {
        assert_eq!(MouseButton::Left.xdotool_button(), "1");
        assert_eq!(MouseButton::Right.xdotool_button(), "3");
        assert_eq!(MouseButton::Middle.xdotool_button(), "2");
    }

    #[test]
    fn mouse_button_cliclick() {
        assert_eq!(MouseButton::Left.cliclick_prefix(), "c");
        assert_eq!(MouseButton::Right.cliclick_prefix(), "rc");
        assert_eq!(MouseButton::Middle.cliclick_prefix(), "tc");
    }

    #[test]
    fn scroll_direction_xdotool() {
        assert_eq!(ScrollDirection::Up.xdotool_button(), "4");
        assert_eq!(ScrollDirection::Down.xdotool_button(), "5");
    }

    #[test]
    fn no_wayland_session_message_virtual_target_suggests_xvfb() {
        let msg = no_wayland_session_message(&DisplayTarget::Virtual { id: 99 });
        assert!(msg.contains(":99"), "message should mention display number: {}", msg);
        assert!(msg.contains("Xvfb"), "message should suggest Xvfb: {}", msg);
    }

    #[test]
    fn no_wayland_session_message_user_session_mentions_portal() {
        // Clear env first so the test is deterministic.
        std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        let msg = no_wayland_session_message(&DisplayTarget::UserSession);
        assert!(msg.contains("grant_user_display"), "ungranted message: {}", msg);

        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
        let msg = no_wayland_session_message(&DisplayTarget::UserSession);
        assert!(msg.contains("portal"), "granted message should mention portal: {}", msg);
        std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
    }

    #[test]
    fn png_dimensions_valid() {
        // Minimal valid PNG header (8 byte signature + IHDR chunk)
        let mut header = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
            0x00, 0x00, 0x04, 0x00, // width: 1024
            0x00, 0x00, 0x03, 0x00, // height: 768
        ];
        header.extend_from_slice(&[0u8; 8]); // padding
        assert_eq!(png_dimensions(&header), Some((1024, 768)));
    }

    #[test]
    fn cu_action_serde_roundtrip() {
        let action = CuAction::Click {
            x: 100,
            y: 200,
            button: MouseButton::Left,
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: CuAction = serde_json::from_str(&json).unwrap();
        match back {
            CuAction::Click { x, y, button } => {
                assert_eq!(x, 100);
                assert_eq!(y, 200);
                assert!(matches!(button, MouseButton::Left));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn translate_simple_keys() {
        assert_eq!(translate_single_key("Return"), "return");
        assert_eq!(translate_single_key("Tab"), "tab");
        assert_eq!(translate_single_key("Escape"), "esc");
        assert_eq!(translate_single_key("BackSpace"), "delete");
        assert_eq!(translate_single_key("Up"), "arrow-up");
        assert_eq!(translate_single_key("Down"), "arrow-down");
        assert_eq!(translate_single_key("super"), "cmd");
        assert_eq!(translate_single_key("control"), "ctrl");
    }

    #[test]
    fn translate_modifier_combo() {
        // Single chars use t: (type) since cliclick kp: only accepts special key names
        let args = translate_key_for_cliclick("ctrl+c");
        assert_eq!(args, vec!["kd:ctrl", "t:c", "ku:ctrl"]);

        let args = translate_key_for_cliclick("super+shift+a");
        assert_eq!(args, vec!["kd:cmd", "kd:shift", "t:a", "ku:shift", "ku:cmd"]);

        // Special keys still use kp:
        let args = translate_key_for_cliclick("cmd+space");
        assert_eq!(args, vec!["kd:cmd", "kp:space", "ku:cmd"]);
    }

    #[test]
    fn translate_single_key_passthrough() {
        // Unrecognized keys pass through as-is
        assert_eq!(translate_single_key("a"), "a");
        assert_eq!(translate_single_key("z"), "z");
    }

    #[test]
    fn translate_single_key_cmd_variants() {
        // OpenAI CU sends "CMD", Gemini sends "super"/"meta"
        assert_eq!(translate_single_key("CMD"), "cmd");
        assert_eq!(translate_single_key("cmd"), "cmd");
        assert_eq!(translate_single_key("command"), "cmd");
        assert_eq!(translate_single_key("super"), "cmd");
        assert_eq!(translate_single_key("meta"), "cmd");
        assert_eq!(translate_single_key("Meta"), "cmd");
    }

    #[test]
    fn display_target_virtual_env_string() {
        let target = DisplayTarget::Virtual { id: 99 };
        assert_eq!(target.display_env_string(), ":99");
    }

    #[test]
    fn display_target_stream_names() {
        assert_eq!(
            DisplayTarget::Virtual { id: 99 }.stream_name(),
            "display_99"
        );
        assert_eq!(
            DisplayTarget::UserSession.stream_name(),
            "display_user_session"
        );
    }

    #[test]
    fn display_target_is_user_session() {
        assert!(!DisplayTarget::Virtual { id: 99 }.is_user_session());
        assert!(DisplayTarget::UserSession.is_user_session());
    }

    #[test]
    fn display_target_from_display_id() {
        assert_eq!(
            DisplayTarget::from_display_id(99),
            DisplayTarget::Virtual { id: 99 }
        );
        assert_eq!(
            DisplayTarget::from_display_id(0),
            DisplayTarget::UserSession
        );
        assert_eq!(
            DisplayTarget::from_display_id(-1),
            DisplayTarget::UserSession
        );
    }

    #[test]
    fn display_target_from_command_display() {
        let default = DisplayTarget::Virtual { id: 99 };
        assert_eq!(
            DisplayTarget::from_command_display(None, default),
            DisplayTarget::Virtual { id: 99 }
        );
        assert_eq!(
            DisplayTarget::from_command_display(Some(0), default),
            DisplayTarget::UserSession
        );
        assert_eq!(
            DisplayTarget::from_command_display(Some(50), default),
            DisplayTarget::Virtual { id: 50 }
        );
    }

    #[test]
    fn display_target_serde_roundtrip() {
        let virtual_target = DisplayTarget::Virtual { id: 42 };
        let json = serde_json::to_string(&virtual_target).unwrap();
        let back: DisplayTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, virtual_target);

        let session_target = DisplayTarget::UserSession;
        let json = serde_json::to_string(&session_target).unwrap();
        let back: DisplayTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, session_target);
    }

    #[test]
    fn display_target_display_fmt() {
        assert_eq!(format!("{}", DisplayTarget::Virtual { id: 99 }), ":99");
        assert_eq!(
            format!("{}", DisplayTarget::UserSession),
            "user_session"
        );
    }

    #[test]
    fn char_to_dom_code_letters() {
        assert_eq!(char_to_dom_code('a'), "KeyA");
        assert_eq!(char_to_dom_code('A'), "KeyA");
        assert_eq!(char_to_dom_code('z'), "KeyZ");
    }

    #[test]
    fn char_to_dom_code_digits() {
        assert_eq!(char_to_dom_code('0'), "Digit0");
        assert_eq!(char_to_dom_code('9'), "Digit9");
        assert_eq!(char_to_dom_code('!'), "Digit1");
        assert_eq!(char_to_dom_code('@'), "Digit2");
    }

    #[test]
    fn char_to_dom_code_special() {
        assert_eq!(char_to_dom_code(' '), "Space");
        assert_eq!(char_to_dom_code('\n'), "Enter");
        assert_eq!(char_to_dom_code('\t'), "Tab");
        assert_eq!(char_to_dom_code('-'), "Minus");
        assert_eq!(char_to_dom_code('/'), "Slash");
    }

    #[test]
    fn char_to_dom_code_unknown() {
        assert_eq!(char_to_dom_code('\u{2603}'), "Unidentified");
    }

    #[test]
    fn key_name_to_dom_code_known_keys() {
        assert_eq!(key_name_to_dom_code("Enter"), "Enter");
        assert_eq!(key_name_to_dom_code("ENTER"), "Enter");
        assert_eq!(key_name_to_dom_code("return"), "Enter");
        assert_eq!(key_name_to_dom_code("Escape"), "Escape");
        assert_eq!(key_name_to_dom_code("esc"), "Escape");
        assert_eq!(key_name_to_dom_code("Tab"), "Tab");
        assert_eq!(key_name_to_dom_code("Backspace"), "Backspace");
        assert_eq!(key_name_to_dom_code("ArrowUp"), "ArrowUp");
        assert_eq!(key_name_to_dom_code("up"), "ArrowUp");
        assert_eq!(key_name_to_dom_code("F1"), "F1");
        assert_eq!(key_name_to_dom_code("f12"), "F12");
    }

    #[test]
    fn key_name_to_dom_code_passthrough() {
        // Unknown keys pass through as-is
        assert_eq!(key_name_to_dom_code("Meta"), "Meta");
        assert_eq!(key_name_to_dom_code("ctrl+c"), "ctrl+c");
    }

    #[test]
    fn mouse_button_index_values() {
        assert_eq!(mouse_button_index(MouseButton::Left), 0);
        assert_eq!(mouse_button_index(MouseButton::Middle), 1);
        assert_eq!(mouse_button_index(MouseButton::Right), 2);
    }
}
