//! Map DOM `KeyboardEvent.code` to Win32 Virtual-Key codes (`VK_*`).
//!
//! Phase 1: physical key semantics only -- this maps physical key positions,
//! not character output. Non-US layouts will produce incorrect characters for
//! text entry. This mirrors the evdev keymap in `keymap.rs` and the CGKeyCode
//! keymap in `macos_keymap.rs`, but targets Win32 virtual-key codes from
//! `<winuser.h>` (the `VK_*` constants).
//!
//! Virtual-key codes are what `SendInput` consumes when `KEYEVENTF_SCANCODE`
//! is **not** set: the `ki.wVk` field carries the VK and Windows derives the
//! scan code via `MapVirtualKey`. The injection side ([`super::windows`]) sets
//! `KEYEVENTF_EXTENDEDKEY` for the keys that live in the extended block of the
//! keyboard (arrows, navigation cluster, right-hand modifiers, numpad
//! divide/enter) so that, e.g., the right Ctrl is distinguishable from the
//! left and the arrow keys are not confused with the numpad. The set of
//! extended keys is reported by [`is_extended_key`].

/// Returns the Win32 virtual-key code for the given DOM `KeyboardEvent.code`
/// value, or `None` if the code is unrecognised.
///
/// Values are the numeric `VK_*` constants from `<winuser.h>`. Letters and
/// digits map to their ASCII values (`'A'`..=`'Z'`, `'0'`..=`'9'`), which is
/// the documented Win32 convention -- there are no dedicated `VK_A` macros.
pub fn dom_code_to_vk(code: &str) -> Option<u16> {
    Some(match code {
        // Row 0 -- Escape + Function keys
        "Escape" => 0x1B,    // VK_ESCAPE
        "F1" => 0x70,        // VK_F1
        "F2" => 0x71,        // VK_F2
        "F3" => 0x72,        // VK_F3
        "F4" => 0x73,        // VK_F4
        "F5" => 0x74,        // VK_F5
        "F6" => 0x75,        // VK_F6
        "F7" => 0x76,        // VK_F7
        "F8" => 0x77,        // VK_F8
        "F9" => 0x78,        // VK_F9
        "F10" => 0x79,       // VK_F10
        "F11" => 0x7A,       // VK_F11
        "F12" => 0x7B,       // VK_F12

        // Row 1 -- Digits and top-row punctuation
        "Backquote" => 0xC0, // VK_OEM_3
        "Digit1" => 0x31,    // '1'
        "Digit2" => 0x32,    // '2'
        "Digit3" => 0x33,    // '3'
        "Digit4" => 0x34,    // '4'
        "Digit5" => 0x35,    // '5'
        "Digit6" => 0x36,    // '6'
        "Digit7" => 0x37,    // '7'
        "Digit8" => 0x38,    // '8'
        "Digit9" => 0x39,    // '9'
        "Digit0" => 0x30,    // '0'
        "Minus" => 0xBD,     // VK_OEM_MINUS
        "Equal" => 0xBB,     // VK_OEM_PLUS
        "Backspace" => 0x08, // VK_BACK

        // Row 2 -- QWERTY
        "Tab" => 0x09,       // VK_TAB
        "KeyQ" => 0x51,      // 'Q'
        "KeyW" => 0x57,      // 'W'
        "KeyE" => 0x45,      // 'E'
        "KeyR" => 0x52,      // 'R'
        "KeyT" => 0x54,      // 'T'
        "KeyY" => 0x59,      // 'Y'
        "KeyU" => 0x55,      // 'U'
        "KeyI" => 0x49,      // 'I'
        "KeyO" => 0x4F,      // 'O'
        "KeyP" => 0x50,      // 'P'
        "BracketLeft" => 0xDB,  // VK_OEM_4
        "BracketRight" => 0xDD, // VK_OEM_6
        "Backslash" => 0xDC,    // VK_OEM_5

        // Row 3 -- ASDF
        "CapsLock" => 0x14,  // VK_CAPITAL
        "KeyA" => 0x41,      // 'A'
        "KeyS" => 0x53,      // 'S'
        "KeyD" => 0x44,      // 'D'
        "KeyF" => 0x46,      // 'F'
        "KeyG" => 0x47,      // 'G'
        "KeyH" => 0x48,      // 'H'
        "KeyJ" => 0x4A,      // 'J'
        "KeyK" => 0x4B,      // 'K'
        "KeyL" => 0x4C,      // 'L'
        "Semicolon" => 0xBA, // VK_OEM_1
        "Quote" => 0xDE,     // VK_OEM_7
        "Enter" => 0x0D,     // VK_RETURN

        // Row 4 -- ZXCV
        "ShiftLeft" => 0xA0, // VK_LSHIFT
        "KeyZ" => 0x5A,      // 'Z'
        "KeyX" => 0x58,      // 'X'
        "KeyC" => 0x43,      // 'C'
        "KeyV" => 0x56,      // 'V'
        "KeyB" => 0x42,      // 'B'
        "KeyN" => 0x4E,      // 'N'
        "KeyM" => 0x4D,      // 'M'
        "Comma" => 0xBC,     // VK_OEM_COMMA
        "Period" => 0xBE,    // VK_OEM_PERIOD
        "Slash" => 0xBF,     // VK_OEM_2
        "ShiftRight" => 0xA1, // VK_RSHIFT

        // Row 5 -- Bottom (modifiers + space)
        "ControlLeft" => 0xA2,  // VK_LCONTROL
        "MetaLeft" => 0x5B,     // VK_LWIN
        "AltLeft" => 0xA4,      // VK_LMENU
        "Space" => 0x20,        // VK_SPACE
        "AltRight" => 0xA5,     // VK_RMENU
        "MetaRight" => 0x5C,    // VK_RWIN
        "ContextMenu" => 0x5D,  // VK_APPS (the "menu" key)
        "ControlRight" => 0xA3, // VK_RCONTROL

        // Navigation cluster
        "PrintScreen" => 0x2C, // VK_SNAPSHOT
        "ScrollLock" => 0x91,  // VK_SCROLL
        "Pause" => 0x13,       // VK_PAUSE
        "Insert" => 0x2D,      // VK_INSERT
        "Home" => 0x24,        // VK_HOME
        "PageUp" => 0x21,      // VK_PRIOR
        "Delete" => 0x2E,      // VK_DELETE
        "End" => 0x23,         // VK_END
        "PageDown" => 0x22,    // VK_NEXT

        // Arrow keys
        "ArrowUp" => 0x26,    // VK_UP
        "ArrowLeft" => 0x25,  // VK_LEFT
        "ArrowDown" => 0x28,  // VK_DOWN
        "ArrowRight" => 0x27, // VK_RIGHT

        // Numpad
        "NumLock" => 0x90,        // VK_NUMLOCK
        "NumpadDivide" => 0x6F,   // VK_DIVIDE
        "NumpadMultiply" => 0x6A, // VK_MULTIPLY
        "NumpadSubtract" => 0x6D, // VK_SUBTRACT
        "Numpad7" => 0x67,        // VK_NUMPAD7
        "Numpad8" => 0x68,        // VK_NUMPAD8
        "Numpad9" => 0x69,        // VK_NUMPAD9
        "NumpadAdd" => 0x6B,      // VK_ADD
        "Numpad4" => 0x64,        // VK_NUMPAD4
        "Numpad5" => 0x65,        // VK_NUMPAD5
        "Numpad6" => 0x66,        // VK_NUMPAD6
        "Numpad1" => 0x61,        // VK_NUMPAD1
        "Numpad2" => 0x62,        // VK_NUMPAD2
        "Numpad3" => 0x63,        // VK_NUMPAD3
        "NumpadEnter" => 0x0D,    // VK_RETURN (extended; see is_extended_key)
        "Numpad0" => 0x60,        // VK_NUMPAD0
        "NumpadDecimal" => 0x6E,  // VK_DECIMAL

        _ => return None,
    })
}

/// Whether the given DOM `KeyboardEvent.code` corresponds to a Win32
/// "extended" key, which `SendInput` distinguishes via the
/// `KEYEVENTF_EXTENDEDKEY` flag.
///
/// The extended keys are the ones added to the original IBM PC/AT keyboard
/// when the 101/102-key layout introduced a duplicate navigation cluster and
/// dedicated arrow keys to the right of the main block. Without the flag, the
/// VK alone is ambiguous between, e.g., the right and left Ctrl, or the arrow
/// `Up` and numpad `8` (when NumLock is off). Reference: the "Extended-Key
/// Flag" section of the Win32 keyboard input docs.
///
/// We key this off the DOM `code` (physical key position) rather than the VK
/// so that the two keys sharing `VK_RETURN` -- main `Enter` (not extended) and
/// `NumpadEnter` (extended) -- and the two sharing `VK_CONTROL`/`VK_MENU`
/// resolve correctly. This is exactly why injection passes the original
/// `code` string here instead of inferring from the mapped VK.
pub fn is_extended_key(code: &str) -> bool {
    matches!(
        code,
        // Right-hand modifiers (left variants are NOT extended).
        "ControlRight"
            | "AltRight"
            // Navigation cluster (the dedicated block, not the numpad).
            | "Insert"
            | "Delete"
            | "Home"
            | "End"
            | "PageUp"
            | "PageDown"
            // Dedicated arrow keys.
            | "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
            // Numpad keys that are extended: divide and enter.
            | "NumpadDivide"
            | "NumpadEnter"
            // NumLock and the Windows/Menu keys are extended.
            | "NumLock"
            | "MetaLeft"
            | "MetaRight"
            | "ContextMenu"
            // PrintScreen is extended.
            | "PrintScreen"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_keys() {
        // Letters map to ASCII uppercase values per the Win32 convention.
        assert_eq!(dom_code_to_vk("KeyA"), Some(0x41));
        assert_eq!(dom_code_to_vk("KeyZ"), Some(0x5A));
        assert_eq!(dom_code_to_vk("KeyM"), Some(0x4D));
        assert_eq!(dom_code_to_vk("KeyA"), Some(b'A' as u16));
        assert_eq!(dom_code_to_vk("KeyQ"), Some(b'Q' as u16));
    }

    #[test]
    fn digit_keys() {
        // Digits map to ASCII '0'..='9' values.
        assert_eq!(dom_code_to_vk("Digit1"), Some(0x31));
        assert_eq!(dom_code_to_vk("Digit0"), Some(0x30));
        assert_eq!(dom_code_to_vk("Digit9"), Some(b'9' as u16));
    }

    #[test]
    fn function_keys() {
        assert_eq!(dom_code_to_vk("F1"), Some(0x70));
        assert_eq!(dom_code_to_vk("F10"), Some(0x79));
        assert_eq!(dom_code_to_vk("F11"), Some(0x7A));
        assert_eq!(dom_code_to_vk("F12"), Some(0x7B));
    }

    #[test]
    fn modifiers() {
        assert_eq!(dom_code_to_vk("ShiftLeft"), Some(0xA0));
        assert_eq!(dom_code_to_vk("ShiftRight"), Some(0xA1));
        assert_eq!(dom_code_to_vk("ControlLeft"), Some(0xA2));
        assert_eq!(dom_code_to_vk("ControlRight"), Some(0xA3));
        assert_eq!(dom_code_to_vk("AltLeft"), Some(0xA4));
        assert_eq!(dom_code_to_vk("AltRight"), Some(0xA5));
        assert_eq!(dom_code_to_vk("MetaLeft"), Some(0x5B));
        assert_eq!(dom_code_to_vk("MetaRight"), Some(0x5C));
    }

    #[test]
    fn special_keys() {
        assert_eq!(dom_code_to_vk("Escape"), Some(0x1B));
        assert_eq!(dom_code_to_vk("Enter"), Some(0x0D));
        assert_eq!(dom_code_to_vk("Backspace"), Some(0x08));
        assert_eq!(dom_code_to_vk("Tab"), Some(0x09));
        assert_eq!(dom_code_to_vk("Space"), Some(0x20));
        assert_eq!(dom_code_to_vk("CapsLock"), Some(0x14));
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(dom_code_to_vk("ArrowUp"), Some(0x26));
        assert_eq!(dom_code_to_vk("ArrowDown"), Some(0x28));
        assert_eq!(dom_code_to_vk("ArrowLeft"), Some(0x25));
        assert_eq!(dom_code_to_vk("ArrowRight"), Some(0x27));
        assert_eq!(dom_code_to_vk("Home"), Some(0x24));
        assert_eq!(dom_code_to_vk("End"), Some(0x23));
        assert_eq!(dom_code_to_vk("PageUp"), Some(0x21));
        assert_eq!(dom_code_to_vk("PageDown"), Some(0x22));
        assert_eq!(dom_code_to_vk("Insert"), Some(0x2D));
        assert_eq!(dom_code_to_vk("Delete"), Some(0x2E));
    }

    #[test]
    fn punctuation_keys() {
        assert_eq!(dom_code_to_vk("Minus"), Some(0xBD));
        assert_eq!(dom_code_to_vk("Equal"), Some(0xBB));
        assert_eq!(dom_code_to_vk("BracketLeft"), Some(0xDB));
        assert_eq!(dom_code_to_vk("BracketRight"), Some(0xDD));
        assert_eq!(dom_code_to_vk("Backslash"), Some(0xDC));
        assert_eq!(dom_code_to_vk("Semicolon"), Some(0xBA));
        assert_eq!(dom_code_to_vk("Quote"), Some(0xDE));
        assert_eq!(dom_code_to_vk("Backquote"), Some(0xC0));
        assert_eq!(dom_code_to_vk("Comma"), Some(0xBC));
        assert_eq!(dom_code_to_vk("Period"), Some(0xBE));
        assert_eq!(dom_code_to_vk("Slash"), Some(0xBF));
    }

    #[test]
    fn numpad_keys() {
        assert_eq!(dom_code_to_vk("NumLock"), Some(0x90));
        assert_eq!(dom_code_to_vk("Numpad0"), Some(0x60));
        assert_eq!(dom_code_to_vk("Numpad5"), Some(0x65));
        assert_eq!(dom_code_to_vk("Numpad9"), Some(0x69));
        assert_eq!(dom_code_to_vk("NumpadEnter"), Some(0x0D));
        assert_eq!(dom_code_to_vk("NumpadAdd"), Some(0x6B));
        assert_eq!(dom_code_to_vk("NumpadSubtract"), Some(0x6D));
        assert_eq!(dom_code_to_vk("NumpadMultiply"), Some(0x6A));
        assert_eq!(dom_code_to_vk("NumpadDivide"), Some(0x6F));
        assert_eq!(dom_code_to_vk("NumpadDecimal"), Some(0x6E));
    }

    #[test]
    fn misc_keys() {
        assert_eq!(dom_code_to_vk("PrintScreen"), Some(0x2C));
        assert_eq!(dom_code_to_vk("ScrollLock"), Some(0x91));
        assert_eq!(dom_code_to_vk("Pause"), Some(0x13));
        assert_eq!(dom_code_to_vk("ContextMenu"), Some(0x5D));
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(dom_code_to_vk("BogusKey"), None);
        assert_eq!(dom_code_to_vk(""), None);
    }

    #[test]
    fn extended_keys_flagged() {
        // Right-hand modifiers are extended; left-hand are not.
        assert!(is_extended_key("ControlRight"));
        assert!(is_extended_key("AltRight"));
        assert!(!is_extended_key("ControlLeft"));
        assert!(!is_extended_key("AltLeft"));
        assert!(!is_extended_key("ShiftLeft"));
        assert!(!is_extended_key("ShiftRight"));

        // Dedicated navigation + arrows are extended.
        assert!(is_extended_key("ArrowUp"));
        assert!(is_extended_key("ArrowLeft"));
        assert!(is_extended_key("Home"));
        assert!(is_extended_key("End"));
        assert!(is_extended_key("Insert"));
        assert!(is_extended_key("Delete"));
        assert!(is_extended_key("PageUp"));
        assert!(is_extended_key("PageDown"));

        // Numpad enter/divide are extended; the rest of the numpad is not.
        assert!(is_extended_key("NumpadEnter"));
        assert!(is_extended_key("NumpadDivide"));
        assert!(!is_extended_key("NumpadAdd"));
        assert!(!is_extended_key("Numpad5"));

        // Windows + menu keys are extended.
        assert!(is_extended_key("MetaLeft"));
        assert!(is_extended_key("MetaRight"));
        assert!(is_extended_key("ContextMenu"));

        // Ordinary keys are not extended.
        assert!(!is_extended_key("KeyA"));
        assert!(!is_extended_key("Enter"));
        assert!(!is_extended_key("Space"));
        assert!(!is_extended_key("BogusKey"));
    }
}
