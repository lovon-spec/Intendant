/// Map DOM KeyboardEvent.code to macOS virtual keycode (CGKeyCode).
///
/// Phase 1: physical key semantics only -- this maps physical key positions,
/// not character output. Non-US layouts will produce incorrect characters
/// for text entry. This mirrors the evdev keymap in `keymap.rs` but targets
/// macOS virtual keycodes from Carbon's Events.h (kVK_* constants).

/// Returns the macOS virtual keycode for the given DOM `KeyboardEvent.code`
/// value, or `None` if the code is unrecognised.
pub fn dom_code_to_macos_keycode(code: &str) -> Option<u16> {
    // Virtual keycodes from Carbon HIToolbox Events.h
    // Reference: core_graphics::event::KeyCode constants
    Some(match code {
        // Row 0 -- Escape + Function keys
        "Escape" => 0x35,
        "F1" => 0x7A,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6D,
        "F11" => 0x67,
        "F12" => 0x6F,

        // Row 1 -- Digits
        "Backquote" => 0x32,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit5" => 0x17,
        "Digit6" => 0x16,
        "Digit7" => 0x1A,
        "Digit8" => 0x1C,
        "Digit9" => 0x19,
        "Digit0" => 0x1D,
        "Minus" => 0x1B,
        "Equal" => 0x18,
        "Backspace" => 0x33,

        // Row 2 -- QWERTY
        "Tab" => 0x30,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyT" => 0x11,
        "KeyY" => 0x10,
        "KeyU" => 0x20,
        "KeyI" => 0x22,
        "KeyO" => 0x1F,
        "KeyP" => 0x23,
        "BracketLeft" => 0x21,
        "BracketRight" => 0x1E,
        "Backslash" => 0x2A,

        // Row 3 -- ASDF
        "CapsLock" => 0x39,
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyG" => 0x05,
        "KeyH" => 0x04,
        "KeyJ" => 0x26,
        "KeyK" => 0x28,
        "KeyL" => 0x25,
        "Semicolon" => 0x29,
        "Quote" => 0x27,
        "Enter" => 0x24,

        // Row 4 -- ZXCV
        "ShiftLeft" => 0x38,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "KeyB" => 0x0B,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        "Comma" => 0x2B,
        "Period" => 0x2F,
        "Slash" => 0x2C,
        "ShiftRight" => 0x3C,

        // Row 5 -- Bottom
        "ControlLeft" => 0x3B,
        "AltLeft" => 0x3A,  // Option
        "MetaLeft" => 0x37, // Command
        "Space" => 0x31,
        "MetaRight" => 0x36, // Right Command
        "AltRight" => 0x3D,  // Right Option
        "ControlRight" => 0x3E,

        // Navigation cluster
        "Insert" => 0x72, // Help key on Mac
        "Home" => 0x73,
        "PageUp" => 0x74,
        "Delete" => 0x75, // Forward Delete
        "End" => 0x77,
        "PageDown" => 0x79,

        // Arrow keys
        "ArrowUp" => 0x7E,
        "ArrowLeft" => 0x7B,
        "ArrowDown" => 0x7D,
        "ArrowRight" => 0x7C,

        // Numpad
        "NumLock" => 0x47, // Clear on Mac
        "NumpadDivide" => 0x4B,
        "NumpadMultiply" => 0x43,
        "NumpadSubtract" => 0x4E,
        "Numpad7" => 0x59,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,
        "NumpadAdd" => 0x45,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "NumpadEnter" => 0x4C,
        "Numpad0" => 0x52,
        "NumpadDecimal" => 0x41,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_keys() {
        assert_eq!(dom_code_to_macos_keycode("KeyA"), Some(0x00));
        assert_eq!(dom_code_to_macos_keycode("KeyZ"), Some(0x06));
        assert_eq!(dom_code_to_macos_keycode("KeyM"), Some(0x2E));
    }

    #[test]
    fn digit_keys() {
        assert_eq!(dom_code_to_macos_keycode("Digit1"), Some(0x12));
        assert_eq!(dom_code_to_macos_keycode("Digit0"), Some(0x1D));
    }

    #[test]
    fn function_keys() {
        assert_eq!(dom_code_to_macos_keycode("F1"), Some(0x7A));
        assert_eq!(dom_code_to_macos_keycode("F10"), Some(0x6D));
        assert_eq!(dom_code_to_macos_keycode("F11"), Some(0x67));
        assert_eq!(dom_code_to_macos_keycode("F12"), Some(0x6F));
    }

    #[test]
    fn modifiers() {
        assert_eq!(dom_code_to_macos_keycode("ShiftLeft"), Some(0x38));
        assert_eq!(dom_code_to_macos_keycode("ShiftRight"), Some(0x3C));
        assert_eq!(dom_code_to_macos_keycode("ControlLeft"), Some(0x3B));
        assert_eq!(dom_code_to_macos_keycode("ControlRight"), Some(0x3E));
        assert_eq!(dom_code_to_macos_keycode("AltLeft"), Some(0x3A));
        assert_eq!(dom_code_to_macos_keycode("AltRight"), Some(0x3D));
        assert_eq!(dom_code_to_macos_keycode("MetaLeft"), Some(0x37));
        assert_eq!(dom_code_to_macos_keycode("MetaRight"), Some(0x36));
    }

    #[test]
    fn special_keys() {
        assert_eq!(dom_code_to_macos_keycode("Escape"), Some(0x35));
        assert_eq!(dom_code_to_macos_keycode("Enter"), Some(0x24));
        assert_eq!(dom_code_to_macos_keycode("Backspace"), Some(0x33));
        assert_eq!(dom_code_to_macos_keycode("Tab"), Some(0x30));
        assert_eq!(dom_code_to_macos_keycode("Space"), Some(0x31));
        assert_eq!(dom_code_to_macos_keycode("CapsLock"), Some(0x39));
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(dom_code_to_macos_keycode("ArrowUp"), Some(0x7E));
        assert_eq!(dom_code_to_macos_keycode("ArrowDown"), Some(0x7D));
        assert_eq!(dom_code_to_macos_keycode("ArrowLeft"), Some(0x7B));
        assert_eq!(dom_code_to_macos_keycode("ArrowRight"), Some(0x7C));
        assert_eq!(dom_code_to_macos_keycode("Home"), Some(0x73));
        assert_eq!(dom_code_to_macos_keycode("End"), Some(0x77));
        assert_eq!(dom_code_to_macos_keycode("PageUp"), Some(0x74));
        assert_eq!(dom_code_to_macos_keycode("PageDown"), Some(0x79));
        assert_eq!(dom_code_to_macos_keycode("Delete"), Some(0x75));
    }

    #[test]
    fn punctuation_keys() {
        assert_eq!(dom_code_to_macos_keycode("Minus"), Some(0x1B));
        assert_eq!(dom_code_to_macos_keycode("Equal"), Some(0x18));
        assert_eq!(dom_code_to_macos_keycode("BracketLeft"), Some(0x21));
        assert_eq!(dom_code_to_macos_keycode("BracketRight"), Some(0x1E));
        assert_eq!(dom_code_to_macos_keycode("Backslash"), Some(0x2A));
        assert_eq!(dom_code_to_macos_keycode("Semicolon"), Some(0x29));
        assert_eq!(dom_code_to_macos_keycode("Quote"), Some(0x27));
        assert_eq!(dom_code_to_macos_keycode("Backquote"), Some(0x32));
        assert_eq!(dom_code_to_macos_keycode("Comma"), Some(0x2B));
        assert_eq!(dom_code_to_macos_keycode("Period"), Some(0x2F));
        assert_eq!(dom_code_to_macos_keycode("Slash"), Some(0x2C));
    }

    #[test]
    fn numpad_keys() {
        assert_eq!(dom_code_to_macos_keycode("NumLock"), Some(0x47));
        assert_eq!(dom_code_to_macos_keycode("Numpad0"), Some(0x52));
        assert_eq!(dom_code_to_macos_keycode("Numpad5"), Some(0x57));
        assert_eq!(dom_code_to_macos_keycode("NumpadEnter"), Some(0x4C));
        assert_eq!(dom_code_to_macos_keycode("NumpadAdd"), Some(0x45));
        assert_eq!(dom_code_to_macos_keycode("NumpadDecimal"), Some(0x41));
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(dom_code_to_macos_keycode("BogusKey"), None);
        assert_eq!(dom_code_to_macos_keycode(""), None);
    }
}
