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
use presence_core::PresenceAction;
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Serialize a serde_json::Value to JsValue with maps as plain JS objects.
fn to_js(val: &serde_json::Value) -> JsValue {
    val.serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .unwrap_or(JsValue::NULL)
}

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
    pub fn set_on_diagnostic(&self, f: Function) {
        *self.callbacks.on_diagnostic.borrow_mut() = Some(f);
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
                    Some("presence_welcome") => {
                        // Update presence state from welcome
                        if let Some(state) = msg.get("state") {
                            let state_js =
                                serde_wasm_bindgen::to_value(state).unwrap_or(JsValue::NULL);
                            presence.borrow_mut().set_state(state_js);
                        }
                        // Replay events from the window
                        if let Some(events) = msg.get("events").and_then(|v| v.as_array()) {
                            for event in events {
                                if let Some(inner) = event.get("event") {
                                    let event_js =
                                        serde_wasm_bindgen::to_value(inner).unwrap_or(JsValue::NULL);
                                    presence.borrow_mut().update_from_event(event_js);
                                }
                            }
                        }
                        // Notify JS (same callback as state_snapshot)
                        let welcome_js =
                            serde_wasm_bindgen::to_value(&msg).unwrap_or(JsValue::NULL);
                        cb.invoke_state_snapshot(&welcome_js);
                    }
                    Some("presence_checkpoint_ack") => {
                        // Acknowledged — no action needed on browser side
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

        // Tell server which voice model we connected, then mark live
        let actual_model = model.as_deref().unwrap_or(match provider {
            "gemini" => "gemini-2.5-flash-native-audio-preview-12-2025",
            "openai" => "gpt-4o-realtime-preview",
            _ => "unknown",
        });
        {
            let mut srv = self.server.borrow_mut();
            srv.set_active_voice(provider, actual_model);
            srv.set_voice_live(true);
        }
        self.server.borrow().send_live_connected();
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

        // Notify server and clear voice state
        self.server.borrow().send_live_disconnected();
        let mut srv = self.server.borrow_mut();
        srv.set_voice_live(false);
        srv.set_active_voice("", "");
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

    /// Send a voice transcript log entry to the server.
    #[wasm_bindgen]
    pub fn send_voice_log(&self, text: &str, tool_context: Option<String>) {
        self.server.borrow_mut().send_voice_log(text, tool_context.as_deref());
    }

    /// Send a presence checkpoint to the server.
    #[wasm_bindgen]
    pub fn send_presence_checkpoint(&self, summary: &str) {
        self.server.borrow().send_presence_checkpoint(summary);
    }

    /// Send a voice diagnostic to the server (errors, silence, disconnects).
    #[wasm_bindgen]
    pub fn send_voice_diagnostic(&self, kind: &str, detail: &str) {
        self.server.borrow().send_voice_diagnostic(kind, detail);
    }

    // --- High-level handlers (consolidate JS logic into WASM) ---

    /// Handle a voice model tool call end-to-end.
    ///
    /// - Dispatches the tool via presence-core
    /// - Sends voice log to server
    /// - For `TextResult` and action types: sends voice tool response, dispatches
    ///   server action if needed, returns `JsValue::NULL`
    /// - For `NeedsIO`: returns `{ needs_io: true, tool_name, args }` so JS can
    ///   do the async server roundtrip and call `send_voice_tool_response` itself
    #[wasm_bindgen]
    pub fn handle_voice_tool_call(&self, call: JsValue) -> JsValue {
        let call_val: serde_json::Value =
            serde_wasm_bindgen::from_value(call.clone()).unwrap_or_default();
        let name = call_val["name"].as_str().unwrap_or("").to_string();
        let args_val = call_val.get("args").cloned().unwrap_or(serde_json::Value::Object(Default::default()));

        // Dispatch tool
        let args_js = to_js(&args_val);
        let action_js = self.presence.borrow().dispatch(&name, args_js);
        let action: PresenceAction =
            serde_wasm_bindgen::from_value(action_js).unwrap_or(PresenceAction::TextResult(
                format!("dispatch error for {}", name),
            ));

        // Log
        let args_str = serde_json::to_string(&args_val).unwrap_or_default();
        let log_text = format!("[tool] {}({})", name, args_str);
        self.send_voice_log(&log_text, Some(name));

        match &action {
            PresenceAction::TextResult(text) => {
                let result = serde_json::json!({"result": text});
                self.send_voice_tool_response(call, to_js(&result));
                JsValue::NULL
            }
            PresenceAction::NeedsIO { tool_name, args } => {
                let ret = serde_json::json!({
                    "needs_io": true,
                    "tool_name": tool_name,
                    "args": args,
                });
                to_js(&ret)
            }
            _ => {
                // Action type (Approve, Deny, Skip, SubmitTask, etc.)
                let confirmation = presence_core::action_confirmation(&action);
                let msg = self.action_to_server_msg(&action);
                if !msg.is_null() {
                    self.send_server_action(msg);
                }
                let result = serde_json::json!({"result": confirmation});
                self.send_voice_tool_response(call, to_js(&result));
                JsValue::NULL
            }
        }
    }

    /// Convert a PresenceAction to a server control message (JSON).
    /// Returns JsValue::NULL for TextResult/NeedsIO.
    fn action_to_server_msg(&self, action: &PresenceAction) -> JsValue {
        let msg = match action {
            PresenceAction::SubmitTask(envelope) => {
                let mut obj = serde_json::json!({
                    "action": "start_task",
                    "task": envelope.task,
                });
                if envelope.force_direct {
                    obj["orchestrate"] = serde_json::Value::Bool(false);
                }
                Some(obj)
            }
            PresenceAction::Approve { id } => {
                Some(serde_json::json!({"action": "approve", "id": id}))
            }
            PresenceAction::Deny { id } => {
                Some(serde_json::json!({"action": "deny", "id": id}))
            }
            PresenceAction::Skip { id } => {
                Some(serde_json::json!({"action": "skip", "id": id}))
            }
            PresenceAction::Respond { text } => {
                Some(serde_json::json!({"action": "input", "text": text}))
            }
            PresenceAction::SetAutonomy { level } => {
                Some(serde_json::json!({"action": "set_autonomy", "level": level}))
            }
            PresenceAction::TextResult(_) | PresenceAction::NeedsIO { .. } => None,
        };
        match msg {
            Some(val) => to_js(&val),
            None => JsValue::NULL,
        }
    }

    /// Handle a server event by injecting system text into the voice model.
    /// Returns true if a message was sent to the voice model.
    #[wasm_bindgen]
    pub fn handle_server_event(&self, evt: JsValue) -> bool {
        let Ok(evt_val) = serde_wasm_bindgen::from_value::<serde_json::Value>(evt) else {
            return false;
        };
        let event_type = evt_val
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let text = match event_type {
            "approval_required" => {
                let cmd = evt_val["command"].as_str().unwrap_or("");
                let id = &evt_val["id"];
                Some(format!(
                    "[System: approval needed] You want to run: \"{}\" (id: {}). Ask the user.",
                    cmd, id
                ))
            }
            "ask_human" => {
                let q = evt_val["question"].as_str().unwrap_or("");
                Some(format!(
                    "[System: question] \"{}\". Ask the user naturally.",
                    q
                ))
            }
            "task_complete" => {
                let reason = evt_val["reason"].as_str().unwrap_or("");
                Some(format!(
                    "[System: done] {}. Tell the user briefly.",
                    reason
                ))
            }
            "round_complete" => {
                let round = evt_val["round"].as_u64().unwrap_or(0);
                let turns = evt_val["turns_in_round"].as_u64().unwrap_or(0);
                Some(format!(
                    "[System: round {} complete ({} turns)] The task finished. \
                     Summarize what was done and ask the user if they need anything else. \
                     If they give you a new task, use submit_task.",
                    round, turns
                ))
            }
            "status" => {
                let phase = evt_val["phase"].as_str().unwrap_or("");
                match phase {
                    "running_agent" => {
                        Some("[System: phase] Now running commands.".to_string())
                    }
                    "thinking" => Some("[System: phase] Now thinking.".to_string()),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(text) = text {
            self.send_text(&text);
            true
        } else {
            false
        }
    }

    /// If the agent has a pending approval, inject it into the voice model.
    /// Returns true if a message was sent.
    #[wasm_bindgen]
    pub fn inject_pending_approval_if_any(&self) -> bool {
        if !self.has_pending_approval() {
            return false;
        }
        let state_js = self.presence.borrow().get_state();
        let Ok(state) = serde_wasm_bindgen::from_value::<serde_json::Value>(state_js) else {
            return false;
        };
        if let Some(pa) = state.get("pending_approval") {
            let cmd = pa["command_preview"].as_str().unwrap_or("");
            let id = &pa["id"];
            let cat = pa["category"].as_str().unwrap_or("");
            self.send_text(&format!(
                "[System: approval needed] You want to run: \"{}\" (id: {}, category: {}). Ask the user if this is okay.",
                cmd, id, cat
            ));
            true
        } else {
            false
        }
    }
}
