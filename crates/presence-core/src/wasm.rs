//! WASM exports for browser-side presence logic.
//!
//! Provides a stateful `WasmPresence` object and freestanding helpers.
//! All data crosses the WASM boundary via `serde-wasm-bindgen` (JSON-compatible).

use wasm_bindgen::prelude::*;

use crate::dispatch::dispatch_tool_call;
use crate::format::format_event;
use crate::prompt::DEFAULT_PRESENCE_PROMPT;
use crate::tools::presence_tools;
use crate::types::AgentStateSnapshot;

// ---------------------------------------------------------------------------
// Freestanding helpers
// ---------------------------------------------------------------------------

/// Return all presence tool definitions as a JS array.
#[wasm_bindgen]
pub fn get_presence_tools() -> JsValue {
    serde_wasm_bindgen::to_value(&presence_tools()).unwrap_or(JsValue::NULL)
}

/// Return the compiled-in presence system prompt.
#[wasm_bindgen]
pub fn get_presence_prompt() -> String {
    DEFAULT_PRESENCE_PROMPT.to_string()
}

/// Unicode-safe string truncation (appends "..." if truncated).
#[wasm_bindgen]
pub fn wasm_truncate(s: &str, max: usize) -> String {
    crate::truncate(s, max)
}

// ---------------------------------------------------------------------------
// Stateful presence object
// ---------------------------------------------------------------------------

/// Browser-side presence state.
///
/// Wraps `AgentStateSnapshot` and exposes tool dispatch, event formatting,
/// and state queries to JavaScript.
#[wasm_bindgen]
pub struct WasmPresence {
    state: AgentStateSnapshot,
}

#[wasm_bindgen]
impl WasmPresence {
    /// Create a new presence instance with default (empty) agent state.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            state: AgentStateSnapshot::default(),
        }
    }

    /// Replace the entire agent state (e.g. from a bootstrap `state_snapshot`).
    #[wasm_bindgen]
    pub fn set_state(&mut self, state: JsValue) {
        if let Ok(s) = serde_wasm_bindgen::from_value::<AgentStateSnapshot>(state) {
            self.state = s;
        }
    }

    /// Get the current agent state as a JS object.
    #[wasm_bindgen]
    pub fn get_state(&self) -> JsValue {
        serde_wasm_bindgen::to_value(&self.state).unwrap_or(JsValue::NULL)
    }

    /// Update state from a server-sent event (OutboundEvent JSON).
    ///
    /// Returns a formatted narration string if the event should be narrated
    /// to the live model, or `null` if the event is not narration-worthy.
    #[wasm_bindgen]
    pub fn update_from_event(&mut self, event: JsValue) -> JsValue {
        let Ok(event_json) = serde_wasm_bindgen::from_value::<serde_json::Value>(event) else {
            return JsValue::NULL;
        };
        match self.state.update_from_server_event(&event_json) {
            Some(pe) => JsValue::from_str(&format_event(&pe)),
            None => JsValue::NULL,
        }
    }

    /// Dispatch a tool call using local agent state.
    ///
    /// Returns a `PresenceAction` JS object:
    /// - `{ type: "TextResult", data: "..." }` — resolved locally
    /// - `{ type: "SubmitTask", data: { task, force_direct, context_hints } }`
    /// - `{ type: "Approve", data: { id } }`
    /// - `{ type: "Deny", data: { id } }`
    /// - `{ type: "Skip", data: { id } }`
    /// - `{ type: "Respond", data: { text } }`
    /// - `{ type: "SetAutonomy", data: { level } }`
    /// - `{ type: "NeedsIO", data: { tool_name, args } }` — needs server round-trip
    #[wasm_bindgen]
    pub fn dispatch(&self, tool_name: &str, args: JsValue) -> JsValue {
        let args_val: serde_json::Value = serde_wasm_bindgen::from_value(args)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let action = dispatch_tool_call(tool_name, &args_val, &self.state);
        serde_wasm_bindgen::to_value(&action).unwrap_or(JsValue::NULL)
    }

    /// Check if there is a pending approval.
    #[wasm_bindgen]
    pub fn has_pending_approval(&self) -> bool {
        self.state.pending_approval.is_some()
    }

    /// Get the current phase.
    #[wasm_bindgen]
    pub fn phase(&self) -> String {
        self.state.phase.clone()
    }
}
