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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CuAction {
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
    },
    DoubleClick {
        x: i32,
        y: i32,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
) -> Vec<CuActionResult> {
    match backend {
        DisplayBackend::Wayland => {
            return vec![CuActionResult {
                success: false,
                screenshot: None,
                error: Some("Wayland backend not yet implemented".to_string()),
            }];
        }
        DisplayBackend::X11 | DisplayBackend::MacOS => {} // handled below
    }
    let display = target.display_env_string();
    let mut results = Vec::with_capacity(actions.len());
    let mut last_screenshot: Option<ScreenshotData> = None;

    for action in actions {
        let result =
            execute_single(action, &display, backend, screenshot_dir, action_counter).await;
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
        let auto = take_screenshot(&display, backend, screenshot_dir, action_counter).await;
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
                run_cliclick(&[&format!("{}:{},{}", button.cliclick_prefix(), x, y)]).await
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
                run_cliclick(&[&format!("dc:{},{}", x, y)]).await
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
                run_cliclick(&[&format!("t:{}", text)]).await
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
                execute_macos_scroll(*x, *y, *direction, *amount).await
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
                run_cliclick(&[&format!("m:{},{}", x, y)]).await
            }
            _ => {
                run_xdotool(display, &[
                    "mousemove", "--sync", &x.to_string(), &y.to_string(),
                ]).await
            }
        },
        CuAction::Drag { start_x, start_y, end_x, end_y } => match backend {
            DisplayBackend::MacOS => {
                run_cliclick(&[
                    &format!("dd:{},{}", start_x, start_y),
                    &format!("du:{},{}", end_x, end_y),
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
            // Press the base key
            args.push(format!("kp:{}", translate_single_key(base_key)));
            // Release modifiers in reverse
            for m in modifiers.iter().rev() {
                args.push(format!("ku:{}", translate_single_key(m)));
            }
            return args;
        }
    }
    vec![format!("kp:{}", translate_single_key(key))]
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
        "super" | "super_l" | "super_r" | "meta" => "cmd",
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

    // Read file and encode as base64
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read screenshot: {}", e))?;

    // Get dimensions from the PNG header (first 24 bytes: signature + IHDR)
    let (width, height) = png_dimensions(&bytes).unwrap_or((0, 0));

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
        let args = translate_key_for_cliclick("ctrl+c");
        assert_eq!(args, vec!["kd:ctrl", "kp:c", "ku:ctrl"]);

        let args = translate_key_for_cliclick("super+shift+a");
        assert_eq!(args, vec!["kd:cmd", "kd:shift", "kp:a", "ku:shift", "ku:cmd"]);
    }

    #[test]
    fn translate_single_key_passthrough() {
        // Unrecognized keys pass through as-is
        assert_eq!(translate_single_key("a"), "a");
        assert_eq!(translate_single_key("z"), "z");
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
}
