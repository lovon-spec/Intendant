//! Browser-side presence layer in Rust+WASM.
//!
//! Wraps `presence-core` (pure logic) with browser WebSocket transport for:
//! - Server connection (TUI frames, control messages, tool requests)
//! - Gemini Live voice model
//! - OpenAI Realtime voice model
//!
//! JavaScript surface shrinks to: xterm.js, DOM updates, audio capture/playback.

mod callbacks;
mod gemini;
mod openai;
mod server;

use std::cell::RefCell;
use std::rc::Rc;

use callbacks::Callbacks;
use js_sys::Function;
use presence_core::wasm::WasmPresence;
use wasm_bindgen::prelude::*;

/// Main entry point for the browser presence layer.
///
/// Manages server connection, voice model, and presence state.
/// All WebSocket protocols are handled in Rust; JS only handles
/// DOM updates and audio I/O.
#[wasm_bindgen]
pub struct PresenceWeb {
    callbacks: Rc<Callbacks>,
    server: RefCell<server::ServerConnection>,
    gemini: RefCell<Option<gemini::GeminiProvider>>,
    openai: Rc<RefCell<Option<openai::OpenAIProvider>>>,
    presence: Rc<RefCell<WasmPresence>>,
    active_provider: RefCell<String>, // "gemini" or "openai" or ""
    pending_tool_requests:
        Rc<RefCell<std::collections::HashMap<String, js_sys::Function>>>,
    tool_request_counter: RefCell<u32>,
}

#[wasm_bindgen]
impl PresenceWeb {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        let callbacks = Rc::new(Callbacks::default());
        let presence = Rc::new(RefCell::new(WasmPresence::new()));
        let pending = Rc::new(RefCell::new(std::collections::HashMap::new()));

        let mut server = server::ServerConnection::new(callbacks.clone());

        // Set up server message handler that routes to presence + callbacks
        // We use a raw message handler here. The actual routing to presence
        // and voice model will be done via connect_server().
        let handler: Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>> =
            Rc::new(RefCell::new(Box::new(move |_msg: serde_json::Value| {
                // Will be replaced in connect_server
            })));
        server.set_message_handler(handler);

        Self {
            callbacks,
            server: RefCell::new(server),
            gemini: RefCell::new(None),
            openai: Rc::new(RefCell::new(None)),
            presence,
            active_provider: RefCell::new(String::new()),
            pending_tool_requests: pending,
            tool_request_counter: RefCell::new(0),
        }
    }

    // --- Callback setters (called from JS) ---

    #[wasm_bindgen]
    pub fn set_on_term(&self, f: Function) {
        *self.callbacks.on_term.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_server_state(&self, f: Function) {
        *self.callbacks.on_server_state.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_voice_ready(&self, f: Function) {
        *self.callbacks.on_voice_ready.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_voice_audio(&self, f: Function) {
        *self.callbacks.on_voice_audio.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_voice_text(&self, f: Function) {
        *self.callbacks.on_voice_text.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_voice_tool_call(&self, f: Function) {
        *self.callbacks.on_voice_tool_call.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_voice_interrupted(&self, f: Function) {
        *self.callbacks.on_voice_interrupted.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_error(&self, f: Function) {
        *self.callbacks.on_error.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_state_snapshot(&self, f: Function) {
        *self.callbacks.on_state_snapshot.borrow_mut() = Some(f);
    }

    #[wasm_bindgen]
    pub fn set_on_server_event(&self, f: Function) {
        *self.callbacks.on_server_event.borrow_mut() = Some(f);
    }

    // --- Server connection ---

    #[wasm_bindgen]
    pub fn connect_server(&self, url: &str) {
        let pending = self.pending_tool_requests.clone();
        let presence = self.presence.clone();

        // Create the message handler
        let handler: Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>> = Rc::new(RefCell::new({
            let cb = self.callbacks.clone();
            let pending = pending;
            let presence = presence;
            Box::new(move |msg: serde_json::Value| {
                // Route by message type
                let t = msg.get("t").and_then(|v| v.as_str());

                match t {
                    Some("term") => {
                        if let Some(d) = msg["d"].as_str() {
                            cb.invoke_term(d);
                        }
                    }
                    Some("state_snapshot") => {
                        // Update presence state from bootstrap/reconnect
                        if let Some(state) = msg.get("state") {
                            let state_js =
                                serde_wasm_bindgen::to_value(state).unwrap_or(JsValue::NULL);
                            presence.borrow_mut().set_state(state_js);
                        }
                        // Notify JS for voice model narration
                        let snapshot_js =
                            serde_wasm_bindgen::to_value(&msg).unwrap_or(JsValue::NULL);
                        cb.invoke_state_snapshot(&snapshot_js);
                    }
                    Some("tool_response") => {
                        if let Some(id) = msg["id"].as_str() {
                            let resolver = pending.borrow_mut().remove(id);
                            if let Some(f) = resolver {
                                let result_js = serde_wasm_bindgen::to_value(
                                    &msg.get("result").unwrap_or(&serde_json::Value::Null),
                                )
                                .unwrap_or(JsValue::NULL);
                                let _ = f.call1(&JsValue::NULL, &result_js);
                            }
                        }
                    }
                    _ => {
                        // Event messages (have "event" field)
                        if msg.get("event").is_some() {
                            let event_js =
                                serde_wasm_bindgen::to_value(&msg).unwrap_or(JsValue::NULL);
                            // Update presence state (drop borrow before callback)
                            presence.borrow_mut().update_from_event(event_js.clone());
                            // Notify JS for voice model narration
                            cb.invoke_server_event(&event_js);
                        }
                    }
                }
            })
        }));

        let mut server = self.server.borrow_mut();
        server.set_message_handler(handler);
        server.connect(url);
    }

    #[wasm_bindgen]
    pub fn reconnect_server(&self, url: &str) {
        self.server.borrow_mut().connect(url);
    }

    #[wasm_bindgen]
    pub fn send_key(&self, key: &str, ctrl: bool, alt: bool, shift: bool) {
        self.server.borrow().send_key(key, ctrl, alt, shift);
    }

    #[wasm_bindgen]
    pub fn send_resize(&self, cols: u16, rows: u16) {
        self.server.borrow().send_resize(cols, rows);
    }

    // --- Voice model ---

    #[wasm_bindgen]
    pub fn connect_voice(
        &self,
        provider: &str,
        token: &str,
        model: Option<String>,
        input_sample_rate: Option<u32>,
    ) {
        let tools_val = presence_core::presence_tools();
        let tools_js =
            serde_wasm_bindgen::to_value(&tools_val).unwrap_or(JsValue::NULL);
        let prompt = presence_core::DEFAULT_PRESENCE_PROMPT;

        *self.active_provider.borrow_mut() = provider.to_string();

        match provider {
            "gemini" => {
                let mut gemini = gemini::GeminiProvider::new(self.callbacks.clone());
                gemini.connect(
                    token,
                    model.as_deref(),
                    input_sample_rate,
                    prompt,
                    &tools_js,
                );
                *self.gemini.borrow_mut() = Some(gemini);
            }
            "openai" => {
                let mut openai = openai::OpenAIProvider::new(self.callbacks.clone());
                openai.connect(token, model.as_deref(), prompt, &tools_js);
                *self.openai.borrow_mut() = Some(openai);
            }
            _ => {
                self.callbacks
                    .invoke_error(&format!("Unknown voice provider: {}", provider));
            }
        }

        // Notify server of live model connection
        self.server.borrow().send_live_connected();
        self.server.borrow_mut().set_voice_live(true);
    }

    #[wasm_bindgen]
    pub fn disconnect_voice(&self) {
        if let Some(ref mut g) = *self.gemini.borrow_mut() {
            g.disconnect();
        }
        if let Some(ref mut o) = *self.openai.borrow_mut() {
            o.disconnect();
        }
        *self.active_provider.borrow_mut() = String::new();

        // Notify server
        self.server.borrow().send_live_disconnected();
        self.server.borrow_mut().set_voice_live(false);
    }

    #[wasm_bindgen]
    pub fn send_audio(&self, base64_pcm: &str) {
        match self.active_provider.borrow().as_str() {
            "gemini" => {
                if let Some(ref g) = *self.gemini.borrow() {
                    g.send_audio(base64_pcm);
                }
            }
            "openai" => {
                if let Some(ref o) = *self.openai.borrow() {
                    o.send_audio(base64_pcm);
                }
            }
            _ => {}
        }
    }

    #[wasm_bindgen]
    pub fn send_text(&self, text: &str) {
        match self.active_provider.borrow().as_str() {
            "gemini" => {
                if let Some(ref g) = *self.gemini.borrow() {
                    g.send_text(text);
                }
            }
            "openai" => {
                if let Some(ref o) = *self.openai.borrow() {
                    o.send_text(text);
                }
            }
            _ => {}
        }
    }

    #[wasm_bindgen]
    pub fn send_voice_tool_response(&self, call: JsValue, result: JsValue) {
        match self.active_provider.borrow().as_str() {
            "gemini" => {
                if let Some(ref g) = *self.gemini.borrow() {
                    g.send_tool_response(&call, &result);
                }
            }
            "openai" => {
                if let Some(ref o) = *self.openai.borrow() {
                    o.send_tool_response(&call, &result);
                }
            }
            _ => {}
        }
    }

    // --- Presence state (delegates to presence-core WASM) ---

    #[wasm_bindgen]
    pub fn set_state(&self, state: JsValue) {
        self.presence.borrow_mut().set_state(state);
    }

    #[wasm_bindgen]
    pub fn get_state(&self) -> JsValue {
        self.presence.borrow().get_state()
    }

    #[wasm_bindgen]
    pub fn update_from_event(&self, event: JsValue) -> JsValue {
        self.presence.borrow_mut().update_from_event(event)
    }

    #[wasm_bindgen]
    pub fn dispatch_tool(&self, tool_name: &str, args: JsValue) -> JsValue {
        self.presence.borrow().dispatch(tool_name, args)
    }

    #[wasm_bindgen]
    pub fn has_pending_approval(&self) -> bool {
        self.presence.borrow().has_pending_approval()
    }

    #[wasm_bindgen]
    pub fn phase(&self) -> String {
        self.presence.borrow().phase()
    }

    // --- Server actions (sends ControlMsg via server WebSocket) ---

    #[wasm_bindgen]
    pub fn send_server_action(&self, action: JsValue) {
        if let Ok(val) = serde_wasm_bindgen::from_value::<serde_json::Value>(action) {
            self.server.borrow().send_action(&val);
        }
    }

    /// Send a tool_request to the server, with a JS callback for the response.
    #[wasm_bindgen]
    pub fn send_tool_request(&self, tool: &str, args: JsValue, on_result: Function) {
        let mut counter = self.tool_request_counter.borrow_mut();
        *counter += 1;
        let id = format!("req_{}", *counter);

        self.pending_tool_requests
            .borrow_mut()
            .insert(id.clone(), on_result);

        let args_val: serde_json::Value =
            serde_wasm_bindgen::from_value(args).unwrap_or_default();
        self.server.borrow().send_tool_request(&id, tool, &args_val);
    }

    /// Get presence tools as JS array (from presence-core).
    #[wasm_bindgen]
    pub fn get_tools(&self) -> JsValue {
        serde_wasm_bindgen::to_value(&presence_core::presence_tools()).unwrap_or(JsValue::NULL)
    }

    /// Get presence system prompt (from presence-core).
    #[wasm_bindgen]
    pub fn get_prompt(&self) -> String {
        presence_core::DEFAULT_PRESENCE_PROMPT.to_string()
    }
}
