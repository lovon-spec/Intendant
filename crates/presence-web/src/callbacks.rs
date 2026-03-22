//! JavaScript callback bridge for WASM → JS events.
//!
//! Each callback is an optional `js_sys::Function` stored behind `RefCell`.
//! Rust code calls `invoke_*()` methods; the function is dispatched into JS.

use js_sys::Function;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;

/// Holds all JS callbacks for the presence-web module.
/// Each callback is set from JS and invoked from Rust.
#[derive(Default)]
pub struct Callbacks {
    /// Terminal ANSI data from server (base64-encoded).
    pub on_term: RefCell<Option<Function>>,
    /// Server connection state changed (boolean: connected/disconnected).
    pub on_server_state: RefCell<Option<Function>>,
    /// Bootstrap state_snapshot from server (JsValue with state object).
    pub on_state_snapshot: RefCell<Option<Function>>,
    /// Intendant event from server (JsValue with event object).
    pub on_server_event: RefCell<Option<Function>>,
    /// Voice model ready (connected + setup complete).
    pub on_voice_ready: RefCell<Option<Function>>,
    /// Voice audio chunk (base64 PCM).
    pub on_voice_audio: RefCell<Option<Function>>,
    /// Voice text response (thinking/reasoning — not what is spoken).
    pub on_voice_text: RefCell<Option<Function>>,
    /// Voice transcript (text of what the model actually spoke).
    pub on_voice_transcript: RefCell<Option<Function>>,
    /// Voice model tool call.
    pub on_voice_tool_call: RefCell<Option<Function>>,
    /// Voice model interrupted by user.
    pub on_voice_interrupted: RefCell<Option<Function>>,
    /// Error from any connection.
    pub on_error: RefCell<Option<Function>>,
    /// Diagnostic event (kind, detail) — for debug logging.
    pub on_diagnostic: RefCell<Option<Function>>,
    /// Inject system text into the active voice model (for async query results).
    pub on_inject_voice_text: RefCell<Option<Function>>,
    /// Inject an image into the active voice model (for inspect_frame results).
    /// Called with (base64_data: string, label: string).
    pub on_inject_voice_image: RefCell<Option<Function>>,
    /// Server session changed (binary restarted). Voice model should reconnect.
    pub on_session_changed: RefCell<Option<Function>>,
    /// Server tells this browser to disconnect its voice model (handover to another browser).
    pub on_force_disconnect: RefCell<Option<Function>>,
    /// Server confirms this browser is now the active voice owner.
    pub on_active_granted: RefCell<Option<Function>>,
    /// Raw server message (fired before internal routing, for dashboard interception).
    pub on_raw_message: RefCell<Option<Function>>,
    /// Live model token usage update (provider-agnostic normalized struct).
    pub on_live_usage: RefCell<Option<Function>>,
}

impl Callbacks {
    pub fn invoke_term(&self, base64_data: &str) {
        if let Some(ref f) = *self.on_term.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(base64_data));
        }
    }

    pub fn invoke_server_state(&self, connected: bool) {
        if let Some(ref f) = *self.on_server_state.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_bool(connected));
        }
    }

    pub fn invoke_state_snapshot(&self, state: &JsValue) {
        if let Some(ref f) = *self.on_state_snapshot.borrow() {
            let _ = f.call1(&JsValue::NULL, state);
        }
    }

    pub fn invoke_server_event(&self, event: &JsValue) {
        if let Some(ref f) = *self.on_server_event.borrow() {
            let _ = f.call1(&JsValue::NULL, event);
        }
    }

    pub fn invoke_voice_ready(&self) {
        if let Some(ref f) = *self.on_voice_ready.borrow() {
            let _ = f.call0(&JsValue::NULL);
        }
    }

    pub fn invoke_voice_audio(&self, base64_pcm: &str) {
        if let Some(ref f) = *self.on_voice_audio.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(base64_pcm));
        }
    }

    pub fn invoke_voice_text(&self, text: &str) {
        if let Some(ref f) = *self.on_voice_text.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(text));
        }
    }

    pub fn invoke_voice_transcript(&self, text: &str) {
        if let Some(ref f) = *self.on_voice_transcript.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(text));
        }
    }

    pub fn invoke_voice_tool_call(&self, call: &JsValue) {
        if let Some(ref f) = *self.on_voice_tool_call.borrow() {
            let _ = f.call1(&JsValue::NULL, call);
        }
    }

    pub fn invoke_voice_interrupted(&self) {
        if let Some(ref f) = *self.on_voice_interrupted.borrow() {
            let _ = f.call0(&JsValue::NULL);
        }
    }

    pub fn invoke_error(&self, msg: &str) {
        if let Some(ref f) = *self.on_error.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(msg));
        }
    }

    pub fn invoke_diagnostic(&self, kind: &str, detail: &str) {
        if let Some(ref f) = *self.on_diagnostic.borrow() {
            let _ = f.call2(
                &JsValue::NULL,
                &JsValue::from_str(kind),
                &JsValue::from_str(detail),
            );
        }
    }

    pub fn invoke_inject_voice_text(&self, text: &str) {
        if let Some(ref f) = *self.on_inject_voice_text.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(text));
        }
    }

    pub fn invoke_inject_voice_image(&self, base64_data: &str, label: &str) {
        if let Some(ref f) = *self.on_inject_voice_image.borrow() {
            let _ = f.call2(
                &JsValue::NULL,
                &JsValue::from_str(base64_data),
                &JsValue::from_str(label),
            );
        }
    }

    pub fn invoke_session_changed(&self) {
        if let Some(ref f) = *self.on_session_changed.borrow() {
            let _ = f.call0(&JsValue::NULL);
        }
    }

    pub fn invoke_force_disconnect(&self, reason: &str) {
        if let Some(ref f) = *self.on_force_disconnect.borrow() {
            let _ = f.call1(&JsValue::NULL, &JsValue::from_str(reason));
        }
    }

    pub fn invoke_active_granted(&self, handover_context: &str, conversation_context: &str) {
        if let Some(ref f) = *self.on_active_granted.borrow() {
            let _ = f.call2(
                &JsValue::NULL,
                &JsValue::from_str(handover_context),
                &JsValue::from_str(conversation_context),
            );
        }
    }

    pub fn invoke_raw_message(&self, msg: &JsValue) {
        if let Some(ref f) = *self.on_raw_message.borrow() {
            let _ = f.call1(&JsValue::NULL, msg);
        }
    }

    /// Invoke with a JsValue containing a normalized LiveUsage object.
    pub fn invoke_live_usage(&self, usage: &JsValue) {
        if let Some(ref f) = *self.on_live_usage.borrow() {
            let _ = f.call1(&JsValue::NULL, usage);
        }
    }
}
