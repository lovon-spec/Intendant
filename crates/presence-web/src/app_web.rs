//! WASM entry point for app.html.
//!
//! Wraps `PresenceWeb` (for voice/server connection) and `AppState`
//! (for log/usage/status logic). Returns `Vec<UiCommand>` as JSON arrays
//! to a thin JS rendering layer.

use std::cell::RefCell;

use js_sys::Function;
use wasm_bindgen::prelude::*;

use crate::app_state::AppState;
use crate::{to_js, PresenceWeb};

/// App dashboard backed by WASM.
///
/// - All event routing, state, and cost calculation in Rust (`AppState`)
/// - Voice/server connection delegated to `PresenceWeb`
/// - JS only processes `UiCommand[]` for DOM updates
#[wasm_bindgen]
pub struct AppWeb {
    inner: PresenceWeb,
    state: RefCell<AppState>,
}

#[wasm_bindgen]
impl AppWeb {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: PresenceWeb::new(),
            state: RefCell::new(AppState::new()),
        }
    }

    // ── Server connection ──────────────────────────────────────────

    /// Connect to the intendant web gateway WebSocket.
    /// Sets up raw_message interception for AppState routing.
    #[wasm_bindgen]
    pub fn connect_server(&self, url: &str) {
        self.inner.connect_server(url);
    }

    #[wasm_bindgen]
    pub fn reconnect_server(&self, url: &str) {
        self.inner.reconnect_server(url);
    }

    // ── Message handling (called from JS on_raw_message callback) ──

    /// Route a raw server message through AppState. Returns `UiCommand[]` as JSON.
    #[wasm_bindgen]
    pub fn handle_server_message(&self, msg: JsValue) -> JsValue {
        let Ok(val) = serde_wasm_bindgen::from_value::<serde_json::Value>(msg) else {
            return JsValue::NULL;
        };
        let cmds = self.state.borrow_mut().handle_message(&val);
        to_js(&cmds)
    }

    // ── Live usage ────────────────────────────────────────────────

    /// Handle live model usage from Gemini Live / OpenAI Realtime.
    /// Updates AppState, sends to server, returns `UiCommand[]`.
    #[wasm_bindgen]
    pub fn handle_live_usage(&self, usage: JsValue) -> JsValue {
        let Ok(val) = serde_wasm_bindgen::from_value::<serde_json::Value>(usage) else {
            return JsValue::NULL;
        };
        let provider = self.inner.active_voice_provider();
        let model = self.inner.active_voice_model();
        let input_tokens = val["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = val["output_tokens"].as_u64().unwrap_or(0);
        let cached_tokens = val["cached_tokens"].as_u64().unwrap_or(0);
        let total_tokens = val["total_tokens"].as_u64().unwrap_or(0);
        let thinking_tokens = val["thinking_tokens"].as_u64().unwrap_or(0);

        // Update WASM state for immediate rendering
        let cmds = self.state.borrow_mut().update_live_usage(
            &provider, &model,
            input_tokens, output_tokens, cached_tokens, total_tokens, thinking_tokens,
        );
        // Notify server for caching/broadcast to other connections
        self.inner.send_live_usage(input_tokens, output_tokens, cached_tokens, total_tokens, thinking_tokens);

        to_js(&cmds)
    }

    // ── Verbosity ──────────────────────────────────────────────────

    /// Change log verbosity and return commands to re-filter.
    #[wasm_bindgen]
    pub fn set_verbosity(&self, level: &str) -> JsValue {
        let cmds = self.state.borrow_mut().set_verbosity(level);
        to_js(&cmds)
    }

    /// Notify which tab is active (for badge logic).
    #[wasm_bindgen]
    pub fn set_active_tab(&self, tab: &str) -> JsValue {
        let cmds = self.state.borrow_mut().set_active_tab(tab);
        to_js(&cmds)
    }

    // ── Actions (sends ControlMsg to server) ───────────────────────

    /// Approve/skip/deny/approve_all a pending action.
    /// Returns `UiCommand[]` for UI updates. Sends the action to the server.
    #[wasm_bindgen]
    pub fn send_approval(&self, action: &str) -> JsValue {
        let result = self.state.borrow_mut().approve_action(action);
        match result {
            Some((id, cmds)) => {
                // Send to server
                let msg = serde_json::json!({"action": action, "id": id});
                self.inner.send_server_action(to_js(&msg));
                to_js(&cmds)
            }
            None => JsValue::NULL,
        }
    }

    /// Send a human response (askHuman).
    #[wasm_bindgen]
    pub fn send_human_response(&self, text: &str) -> JsValue {
        let cmds = self.state.borrow_mut().human_response(text);
        let msg = serde_json::json!({"action": "input", "text": text});
        self.inner.send_server_action(to_js(&msg));
        to_js(&cmds)
    }

    /// Send a follow-up message.
    #[wasm_bindgen]
    pub fn send_follow_up(&self, text: &str) -> JsValue {
        let cmds = self.state.borrow_mut().follow_up(text);
        let msg = serde_json::json!({"action": "follow_up", "text": text});
        self.inner.send_server_action(to_js(&msg));
        to_js(&cmds)
    }

    /// Take control of a display.
    #[wasm_bindgen]
    pub fn take_display(&self, display_id: u64) {
        let msg = serde_json::json!({"action": "take_display", "display_id": display_id});
        self.inner.send_server_action(to_js(&msg));
    }

    /// Release control of a display.
    #[wasm_bindgen]
    pub fn release_display(&self, display_id: u64, note: Option<String>) {
        let mut msg = serde_json::json!({"action": "release_display", "display_id": display_id});
        if let Some(n) = note {
            if !n.is_empty() {
                msg["note"] = serde_json::Value::String(n);
            }
        }
        self.inner.send_server_action(to_js(&msg));
    }

    /// Get pending approval ID (for keyboard shortcut routing).
    #[wasm_bindgen]
    pub fn pending_approval_id(&self) -> JsValue {
        match self.state.borrow().pending_approval_id() {
            Some(id) => JsValue::from_f64(id as f64),
            None => JsValue::NULL,
        }
    }

    // ── Voice (delegates to PresenceWeb) ───────────────────────────

    #[wasm_bindgen]
    pub fn connect_voice(
        &self,
        provider: &str,
        token: &str,
        model: Option<String>,
        input_sample_rate: Option<u32>,
    ) {
        self.inner.connect_voice(provider, token, model, input_sample_rate);
    }

    #[wasm_bindgen]
    pub fn disconnect_voice(&self) {
        self.inner.disconnect_voice();
    }

    #[wasm_bindgen]
    pub fn send_audio(&self, base64_pcm: &str) {
        self.inner.send_audio(base64_pcm);
    }

    #[wasm_bindgen]
    pub fn send_text(&self, text: &str) {
        self.inner.send_text(text);
    }

    #[wasm_bindgen]
    pub fn send_voice_tool_response(&self, call: JsValue, result: JsValue) {
        self.inner.send_voice_tool_response(call, result);
    }

    #[wasm_bindgen]
    pub fn handle_voice_tool_call(&self, call: JsValue) -> JsValue {
        self.inner.handle_voice_tool_call(call)
    }

    #[wasm_bindgen]
    pub fn handle_server_event(&self, evt: JsValue) -> bool {
        self.inner.handle_server_event(evt)
    }

    #[wasm_bindgen]
    pub fn inject_pending_approval_if_any(&self) -> bool {
        self.inner.inject_pending_approval_if_any()
    }

    #[wasm_bindgen]
    pub fn send_make_active(&self) {
        self.inner.send_make_active();
    }

    #[wasm_bindgen]
    pub fn set_passive_mode(&self, passive: bool) {
        self.inner.set_passive_mode(passive);
    }

    // ── Presence state (delegates) ─────────────────────────────────

    #[wasm_bindgen]
    pub fn get_state(&self) -> JsValue {
        self.inner.get_state()
    }

    #[wasm_bindgen]
    pub fn has_pending_approval(&self) -> bool {
        self.inner.has_pending_approval()
    }

    #[wasm_bindgen]
    pub fn phase(&self) -> String {
        self.inner.phase()
    }

    #[wasm_bindgen]
    pub fn get_tools(&self) -> JsValue {
        self.inner.get_tools()
    }

    #[wasm_bindgen]
    pub fn get_prompt(&self) -> String {
        self.inner.get_prompt()
    }

    // ── Voice log / diagnostics (delegates) ────────────────────────

    #[wasm_bindgen]
    pub fn send_voice_log(&self, text: &str, tool_context: Option<String>) {
        self.inner.send_voice_log(text, tool_context);
    }

    #[wasm_bindgen]
    pub fn send_presence_checkpoint(&self, summary: &str) {
        self.inner.send_presence_checkpoint(summary);
    }

    #[wasm_bindgen]
    pub fn send_user_audio(&self, base64_pcm: &str) {
        self.inner.send_user_audio(base64_pcm);
    }

    #[wasm_bindgen]
    pub fn send_voice_diagnostic(&self, kind: &str, detail: &str) {
        self.inner.send_voice_diagnostic(kind, detail);
    }

    // ── Callback setters (delegates to PresenceWeb) ────────────────

    #[wasm_bindgen]
    pub fn set_on_term(&self, f: Function) { self.inner.set_on_term(f); }
    #[wasm_bindgen]
    pub fn set_on_server_state(&self, f: Function) { self.inner.set_on_server_state(f); }
    #[wasm_bindgen]
    pub fn set_on_voice_ready(&self, f: Function) { self.inner.set_on_voice_ready(f); }
    #[wasm_bindgen]
    pub fn set_on_voice_audio(&self, f: Function) { self.inner.set_on_voice_audio(f); }
    #[wasm_bindgen]
    pub fn set_on_voice_text(&self, f: Function) { self.inner.set_on_voice_text(f); }
    #[wasm_bindgen]
    pub fn set_on_voice_transcript(&self, f: Function) { self.inner.set_on_voice_transcript(f); }
    #[wasm_bindgen]
    pub fn set_on_voice_tool_call(&self, f: Function) { self.inner.set_on_voice_tool_call(f); }
    #[wasm_bindgen]
    pub fn set_on_voice_interrupted(&self, f: Function) { self.inner.set_on_voice_interrupted(f); }
    #[wasm_bindgen]
    pub fn set_on_live_usage(&self, f: Function) { self.inner.set_on_live_usage(f); }
    #[wasm_bindgen]
    pub fn set_on_error(&self, f: Function) { self.inner.set_on_error(f); }
    #[wasm_bindgen]
    pub fn set_on_diagnostic(&self, f: Function) { self.inner.set_on_diagnostic(f); }
    #[wasm_bindgen]
    pub fn set_on_inject_voice_text(&self, f: Function) { self.inner.set_on_inject_voice_text(f); }
    #[wasm_bindgen]
    pub fn set_on_session_changed(&self, f: Function) { self.inner.set_on_session_changed(f); }
    #[wasm_bindgen]
    pub fn set_on_state_snapshot(&self, f: Function) { self.inner.set_on_state_snapshot(f); }
    #[wasm_bindgen]
    pub fn set_on_server_event(&self, f: Function) { self.inner.set_on_server_event(f); }
    #[wasm_bindgen]
    pub fn set_on_force_disconnect(&self, f: Function) { self.inner.set_on_force_disconnect(f); }
    #[wasm_bindgen]
    pub fn set_on_active_granted(&self, f: Function) { self.inner.set_on_active_granted(f); }
    #[wasm_bindgen]
    pub fn set_on_raw_message(&self, f: Function) { self.inner.set_on_raw_message(f); }
}
