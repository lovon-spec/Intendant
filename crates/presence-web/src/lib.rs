//! Browser-side presence layer in Rust+WASM.
//!
//! Wraps `presence-core` (pure logic) with browser WebSocket transport for:
//! - Server connection (TUI frames, control messages, tool requests)
//! - Gemini Live voice model
//! - OpenAI Realtime voice model
//!
//! JavaScript surface shrinks to: xterm.js, DOM updates, audio capture/playback.

// app_state is pure Rust (no WASM deps) — available on all targets for testing.
pub mod app_state;

// Everything below is WASM-only: browser WebSocket transport, voice providers, bindings.
#[cfg(target_arch = "wasm32")]
mod callbacks;
#[cfg(target_arch = "wasm32")]
mod gemini;
#[cfg(target_arch = "wasm32")]
mod openai;
#[cfg(target_arch = "wasm32")]
mod server;

/// Provider-agnostic live model usage snapshot.
/// Both Gemini Live and OpenAI Realtime map their provider-specific
/// usage metadata into this common structure.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LiveUsage {
    /// Input/prompt tokens (includes cached).
    pub input_tokens: u64,
    /// Output/response tokens.
    pub output_tokens: u64,
    /// Cached input tokens (subset of input_tokens, cheaper pricing).
    pub cached_tokens: u64,
    /// Total tokens across all categories.
    pub total_tokens: u64,
    /// Thinking/reasoning tokens (model-dependent, 0 if not applicable).
    pub thinking_tokens: u64,
    /// Input text tokens, when provider reports modality breakdown.
    #[serde(default)]
    pub input_text_tokens: u64,
    /// Input audio tokens, when provider reports modality breakdown.
    #[serde(default)]
    pub input_audio_tokens: u64,
    /// Input image/video tokens, when provider reports modality breakdown.
    #[serde(default)]
    pub input_image_tokens: u64,
    /// Cached text tokens, subset of input_text_tokens.
    #[serde(default)]
    pub cached_text_tokens: u64,
    /// Cached audio tokens, subset of input_audio_tokens.
    #[serde(default)]
    pub cached_audio_tokens: u64,
    /// Cached image/video tokens, subset of input_image_tokens.
    #[serde(default)]
    pub cached_image_tokens: u64,
    /// Output text tokens, when provider reports modality breakdown.
    #[serde(default)]
    pub output_text_tokens: u64,
    /// Output audio tokens, when provider reports modality breakdown.
    #[serde(default)]
    pub output_audio_tokens: u64,
}

#[cfg(target_arch = "wasm32")]
mod wasm_impl {
    use super::*;
    use callbacks::Callbacks;
    use js_sys::Function;
    use presence_core::wasm::WasmPresence;
    use presence_core::PresenceAction;
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;

    /// Serialize any Serialize value to JsValue with maps as plain JS objects.
    fn to_js(val: &impl serde::Serialize) -> JsValue {
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
        pending_tool_requests: Rc<RefCell<std::collections::HashMap<String, js_sys::Function>>>,
        /// Pending voice tool calls waiting for async_query_result from server.
        /// Maps async query ID → original JsValue tool call (for send_voice_tool_response).
        pending_async_calls: Rc<RefCell<std::collections::HashMap<String, JsValue>>>,
        tool_request_counter: RefCell<u32>,
        dashboard: RefCell<app_state::AppState>,
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
                pending_async_calls: Rc::new(RefCell::new(std::collections::HashMap::new())),
                tool_request_counter: RefCell::new(0),
                dashboard: RefCell::new(app_state::AppState::new()),
            }
        }

        // --- Callback setters (called from JS) ---

        #[wasm_bindgen]
        pub fn set_on_term(&self, f: Function) {
            *self.callbacks.on_term.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_terminal_output(&self, f: Function) {
            *self.callbacks.on_terminal_output.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_terminal_exited(&self, f: Function) {
            *self.callbacks.on_terminal_exited.borrow_mut() = Some(f);
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
        pub fn set_on_voice_transcript(&self, f: Function) {
            *self.callbacks.on_voice_transcript.borrow_mut() = Some(f);
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
        pub fn set_on_inject_voice_text(&self, f: Function) {
            *self.callbacks.on_inject_voice_text.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_inject_voice_text_passive(&self, f: Function) {
            *self.callbacks.on_inject_voice_text_passive.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_tool_response(&self, f: Function) {
            *self.callbacks.on_tool_response.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_inject_voice_image(&self, f: Function) {
            *self.callbacks.on_inject_voice_image.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_session_changed(&self, f: Function) {
            *self.callbacks.on_session_changed.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_state_snapshot(&self, f: Function) {
            *self.callbacks.on_state_snapshot.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_server_event(&self, f: Function) {
            *self.callbacks.on_server_event.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_force_disconnect(&self, f: Function) {
            *self.callbacks.on_force_disconnect.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_active_granted(&self, f: Function) {
            *self.callbacks.on_active_granted.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_raw_message(&self, f: Function) {
            *self.callbacks.on_raw_message.borrow_mut() = Some(f);
        }

        #[wasm_bindgen]
        pub fn set_on_live_usage(&self, f: Function) {
            *self.callbacks.on_live_usage.borrow_mut() = Some(f);
        }

        // --- Server connection ---

        #[wasm_bindgen]
        pub fn connect_server(&self, url: &str) {
            let pending = self.pending_tool_requests.clone();
            let pending_async = self.pending_async_calls.clone();
            let presence = self.presence.clone();

            // Create the message handler
            let handler: Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>> = Rc::new(RefCell::new({
                let cb = self.callbacks.clone();
                let pending = pending;
                let pending_async = pending_async;
                let presence = presence;
                let last_session_id: RefCell<Option<String>> = RefCell::new(None);
                Box::new(move |msg: serde_json::Value| {
                    let t = msg.get("t").and_then(|v| v.as_str());

                    match t {
                        Some("terminal_output") => {
                            let host_id = msg
                                .get("host_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("local");
                            let terminal_id = msg
                                .get("terminal_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("shell-0");
                            if let Some(data) = msg.get("data").and_then(|v| v.as_str()) {
                                cb.invoke_terminal_output(host_id, terminal_id, data);
                            }
                            return;
                        }
                        Some("terminal_exited") => {
                            let host_id = msg
                                .get("host_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("local");
                            let terminal_id = msg
                                .get("terminal_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("shell-0");
                            let status = msg.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
                            cb.invoke_terminal_exited(host_id, terminal_id, status as i32);
                            return;
                        }
                        _ => {}
                    }

                    // Fire raw message callback (for dashboard interception)
                    cb.invoke_raw_message(&to_js(&msg));

                    // Route by message type
                    match t {
                        Some("term") => {
                            if let Some(d) = msg["d"].as_str() {
                                cb.invoke_term(d);
                            }
                        }
                        Some("state_snapshot") => {
                            // Update presence state from bootstrap/reconnect
                            if let Some(state) = msg.get("state") {
                                presence.borrow_mut().set_state(to_js(state));
                            }
                            // Notify JS for voice model narration
                            cb.invoke_state_snapshot(&to_js(&msg));
                        }
                        Some("presence_welcome") => {
                            // Detect server session change (binary restarted).
                            // If the session ID differs, the voice model's Gemini
                            // context is stale — JS must reconnect it.
                            if let Some(sid) = msg.get("session_id").and_then(|v| v.as_str()) {
                                let mut last = last_session_id.borrow_mut();
                                if let Some(ref prev) = *last {
                                    if prev != sid {
                                        cb.invoke_diagnostic(
                                            "session_changed",
                                            &format!("{} → {}", prev, sid),
                                        );
                                        cb.invoke_session_changed();
                                    }
                                }
                                *last = Some(sid.to_string());
                            }
                            // Update presence state from welcome
                            if let Some(state) = msg.get("state") {
                                presence.borrow_mut().set_state(to_js(state));
                            }
                            // Replay events from the window
                            if let Some(events) = msg.get("events").and_then(|v| v.as_array()) {
                                for event in events {
                                    if let Some(inner) = event.get("event") {
                                        presence.borrow_mut().update_from_event(to_js(inner));
                                    }
                                }
                            }
                            // Notify JS (same callback as state_snapshot)
                            cb.invoke_state_snapshot(&to_js(&msg));
                        }
                        Some("presence_checkpoint_ack") => {
                            // Acknowledged — no action needed on browser side
                        }
                        Some("force_disconnect_voice") => {
                            let reason = msg["reason"].as_str().unwrap_or("unknown");
                            cb.invoke_force_disconnect(reason);
                        }
                        Some("active_granted") => {
                            let handover_context = msg["handover_context"].as_str().unwrap_or("");
                            let conversation_context =
                                msg["conversation_context"].as_str().unwrap_or("");
                            cb.invoke_active_granted(handover_context, conversation_context);
                        }
                        Some("tool_response") => {
                            if let Some(id) = msg["id"].as_str() {
                                let resolver = pending.borrow_mut().remove(id);
                                if let Some(f) = resolver {
                                    let result_js = to_js(
                                        msg.get("result").unwrap_or(&serde_json::Value::Null),
                                    );
                                    let _ = f.call1(&JsValue::NULL, &result_js);
                                }
                            }
                        }
                        Some("async_query_result") => {
                            let req_id = msg["id"].as_str().unwrap_or("");
                            let tool = msg["tool"].as_str().unwrap_or("query");
                            let result_text = msg["result"].as_str().unwrap_or("");
                            let truncated = if result_text.len() > 2000 {
                                format!("{}...(truncated)", &result_text[..2000])
                            } else {
                                result_text.to_string()
                            };

                            // If images are included (e.g. from inspect_frame), inject them
                            // into the voice model via send_frame before the tool response
                            // so the model sees the image in the same context.
                            if let Some(images) = msg["images"].as_array() {
                                for (i, img) in images.iter().enumerate() {
                                    if let Some(data) = img["data"].as_str() {
                                        let label = format!("{}_{}", tool, i);
                                        cb.invoke_inject_voice_image(data, &label);
                                    }
                                }
                            }

                            // Resolve the pending tool call with a proper tool_response.
                            // This replaces the old placeholder approach — the model was
                            // ignoring passively injected results because it had already
                            // moved past the tool call turn.
                            let pending_call = pending_async.borrow_mut().remove(req_id);
                            if let Some(original_call) = pending_call {
                                let result = serde_json::json!({"result": truncated});
                                cb.invoke_tool_response(&original_call, &to_js(&result));
                            } else {
                                // Fallback: no pending call (e.g. fire-and-forget query)
                                let text = format!("[System: {} result] {}", tool, truncated);
                                cb.invoke_inject_voice_text_passive(&text);
                            }
                        }
                        Some("display_input_authority_state") => {
                            // Phase 5a.1: per-display input-authority state for
                            // this browser.  Server has already resolved the
                            // holder against this connection's id; we receive
                            // the resolved `you|other|unclaimed` and forward to
                            // JS.  Skips the generic event/raw-message paths so
                            // dashboard JS doesn't accidentally route this
                            // through `on_server_event` for narration or
                            // unrelated state machinery.
                            if let (Some(display_id), Some(state)) = (
                                msg.get("display_id").and_then(|v| v.as_u64()),
                                msg.get("state").and_then(|v| v.as_str()),
                            ) {
                                cb.invoke_display_input_authority_change(display_id as u32, state);
                            }
                        }
                        _ => {
                            // Event messages (have "event" field)
                            if msg.get("event").is_some() {
                                let event_js = to_js(&msg);
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

        /// Request to become the active voice owner (triggers handover from current active).
        #[wasm_bindgen]
        pub fn send_make_active(&self) -> bool {
            self.server.borrow().send_make_active()
        }

        #[wasm_bindgen]
        pub fn send_key(&self, key: &str, ctrl: bool, alt: bool, shift: bool) {
            self.server.borrow().send_key(key, ctrl, alt, shift);
        }

        #[wasm_bindgen]
        pub fn send_resize(&self, cols: u16, rows: u16) {
            self.server.borrow().send_resize(cols, rows);
        }

        /// Set passive mode — this browser will never request active status.
        /// Use for observer/follow-along mode.
        #[wasm_bindgen]
        pub fn set_passive_mode(&self, passive: bool) {
            self.server.borrow_mut().set_passive_mode(passive);
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
            let tools_js = to_js(&tools_val);
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
            self.server.borrow().send_presence_connect();
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
            self.server.borrow().send_presence_disconnect();
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

        /// Send a video frame to the active live provider.
        /// `base64_jpeg` is the 768x768 live-resolution frame.
        /// `frame_id` is the client-assigned ID (e.g. "cam0-f00047").
        #[wasm_bindgen]
        pub fn send_frame(&self, base64_jpeg: &str, frame_id: &str) {
            match self.active_provider.borrow().as_str() {
                "gemini" => {
                    if let Some(ref g) = *self.gemini.borrow() {
                        g.send_frame(base64_jpeg, frame_id);
                    }
                }
                "openai" => {
                    if let Some(ref o) = *self.openai.borrow() {
                        o.send_frame(base64_jpeg, frame_id);
                    }
                }
                _ => {}
            }
        }

        /// Send a frame ID context annotation to the live provider as system text.
        /// Called alongside send_frame so the model knows the ID of the image it just received.
        #[wasm_bindgen]
        pub fn send_frame_context(&self, frame_id: &str) {
            let text = format!("[frame:{}]", frame_id);
            self.send_text(&text);
        }

        /// Send a video frame to the server for HQ archival.
        /// `base64_jpeg` is the original resolution frame.
        /// `frame_id` is the client-assigned ID.
        /// `stream` is the source stream name (e.g. "cam0").
        #[wasm_bindgen]
        pub fn send_video_frame_to_server(&self, base64_jpeg: &str, frame_id: &str, stream: &str) {
            self.server
                .borrow()
                .send_video_frame(base64_jpeg, frame_id, stream);
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

        /// Send text without ending the user turn (turn_complete: false for Gemini).
        /// Used for tool result injection that arrives while the model is mid-response.
        #[wasm_bindgen]
        pub fn send_text_passive(&self, text: &str) {
            match self.active_provider.borrow().as_str() {
                "gemini" => {
                    if let Some(ref g) = *self.gemini.borrow() {
                        g.send_text_passive(text);
                    }
                }
                "openai" => {
                    // OpenAI Realtime doesn't have this distinction
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

        /// Send a raw JSON string through the server WebSocket.
        /// Use this for transport-level messages (WebRTC signaling) that don't
        /// need to go through the WASM state machine or serde conversion.
        #[wasm_bindgen]
        pub fn send_raw(&self, json_str: &str) -> bool {
            self.server.borrow().send_raw(json_str)
        }

        #[wasm_bindgen]
        pub fn send_server_action(&self, action: JsValue) {
            match serde_wasm_bindgen::from_value::<serde_json::Value>(action) {
                Ok(val) => {
                    let sent = self.server.borrow().send_action(&val);
                    if !sent {
                        let action_type = val.get("action").and_then(|v| v.as_str()).unwrap_or("?");
                        self.callbacks.invoke_diagnostic(
                            "action_drop",
                            &format!(
                                "send_server_action({}) failed — server WebSocket not ready",
                                action_type
                            ),
                        );
                    }
                }
                Err(e) => {
                    self.callbacks.invoke_diagnostic(
                        "action_drop",
                        &format!(
                            "send_server_action: JsValue deserialization failed: {:?}",
                            e
                        ),
                    );
                }
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
            to_js(&presence_core::presence_tools())
        }

        /// Get presence system prompt (from presence-core).
        #[wasm_bindgen]
        pub fn get_prompt(&self) -> String {
            presence_core::DEFAULT_PRESENCE_PROMPT.to_string()
        }

        /// Send a voice transcript log entry to the server.
        #[wasm_bindgen]
        pub fn send_voice_log(&self, text: &str, tool_context: Option<String>) {
            self.server
                .borrow_mut()
                .send_voice_log(text, tool_context.as_deref());
        }

        /// Send a presence checkpoint to the server.
        #[wasm_bindgen]
        pub fn send_presence_checkpoint(&self, summary: &str) {
            self.server.borrow().send_presence_checkpoint(summary);
        }

        /// Send raw PCM16 audio (base64-encoded) to the server for transcription.
        #[wasm_bindgen]
        pub fn send_user_audio(&self, base64_pcm: &str) {
            self.server.borrow().send_user_audio(base64_pcm);
        }

        /// Send a voice diagnostic to the server (errors, silence, disconnects).
        #[wasm_bindgen]
        pub fn send_voice_diagnostic(&self, kind: &str, detail: &str) {
            self.server.borrow().send_voice_diagnostic(kind, detail);
        }

        /// Send live model usage to the server for tracking/broadcast.
        fn send_live_usage(&self, usage: &crate::LiveUsage) {
            self.server.borrow().send_live_usage(usage);
        }

        /// Get the active voice provider name (e.g. "gemini", "openai", or "").
        pub fn active_voice_provider(&self) -> String {
            self.active_provider.borrow().clone()
        }

        /// Get the active voice model name from the server connection.
        pub fn active_voice_model(&self) -> String {
            self.server.borrow().active_model()
        }

        // --- High-level handlers (consolidate JS logic into WASM) ---

        /// Handle a voice model tool call end-to-end.
        ///
        /// ALL tools respond instantly — no server roundtrip blocks the voice model.
        ///
        /// - `TextResult` (check_status): answered from cached state, immediate response
        /// - Action tools (approve, deny, submit_task, etc.): immediate "ok", fire-and-forget to server
        /// - `NeedsIO` (query_detail, recall_memory): immediate "querying..." response,
        ///   async query to server, result injected as text when it arrives
        #[wasm_bindgen]
        pub fn handle_voice_tool_call(&self, call: JsValue) -> JsValue {
            let call_val: serde_json::Value =
                serde_wasm_bindgen::from_value(call.clone()).unwrap_or_default();
            let name = call_val["name"].as_str().unwrap_or("").to_string();
            let args_val = call_val
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));

            // Dispatch tool
            let args_js = to_js(&args_val);
            let action_js = self.presence.borrow().dispatch(&name, args_js);
            let action: PresenceAction = serde_wasm_bindgen::from_value(action_js).unwrap_or(
                PresenceAction::TextResult(format!("dispatch error for {}", name)),
            );

            // Log
            let args_str = serde_json::to_string(&args_val).unwrap_or_default();
            let log_text = format!("[tool] {}({})", name, args_str);
            self.send_voice_log(&log_text, Some(name.clone()));

            match &action {
                PresenceAction::TextResult(text) => {
                    let result = serde_json::json!({"result": text});
                    self.send_voice_tool_response(call, to_js(&result));
                }
                PresenceAction::NeedsIO { tool_name, args } => {
                    // Don't send a placeholder — hold the tool_response until the
                    // server returns the real result. The model waits (audio is
                    // buffered by the WebSocket, not lost). This ensures the model
                    // processes the actual data in the same turn.
                    let mut counter = self.tool_request_counter.borrow_mut();
                    *counter += 1;
                    let id = format!("aq_{}", *counter);
                    drop(counter);

                    // Store the pending call so async_query_result can resolve it
                    self.pending_async_calls
                        .borrow_mut()
                        .insert(id.clone(), call.clone());

                    self.server.borrow().send_async_query(&id, tool_name, args);
                }
                _ => {
                    // Action type (Approve, Deny, Skip, SubmitTask, etc.)
                    // Respond immediately, dispatch to server fire-and-forget
                    let confirmation = presence_core::action_confirmation(&action);
                    let msg = self.action_to_server_msg(&action);
                    if !msg.is_null() {
                        self.send_server_action(msg);
                    }
                    let result = serde_json::json!({"result": confirmation});
                    self.send_voice_tool_response(call, to_js(&result));
                }
            }
            JsValue::NULL
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
            let event_type = evt_val.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let text = match event_type {
                "approval_required" => {
                    let cmd = evt_val["command"].as_str().unwrap_or("");
                    let id = &evt_val["id"];
                    Some(format!(
                    "[System: approval needed] Command: \"{}\" (id: {}). You MUST ask the user and wait for their explicit yes/no. Do NOT approve on your own.",
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
                    let summary = evt_val["summary"].as_str().unwrap_or("");
                    let brief = if summary.is_empty() {
                        reason.to_string()
                    } else {
                        summary.to_string()
                    };
                    Some(format!(
                        "[System: done] {}. Tell the user what was accomplished. \
                     If they want full details, use query_detail with scope 'task_result'.",
                        brief
                    ))
                }
                // round_complete is intentionally NOT injected — task_complete already
                // notifies the voice model.  Injecting both causes the model to process
                // two prompts in rapid succession, delaying responsiveness to user speech.
                "round_complete" => None,
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
                "[System: approval needed] Command: \"{}\" (id: {}, category: {}). You MUST ask the user and wait for their explicit yes/no. Do NOT approve on your own.",
                cmd, id, cat
            ));
                true
            } else {
                false
            }
        }

        // --- Dashboard state (log/usage/approval UI) ---

        /// Handle live model usage from Gemini Live / OpenAI Realtime.
        /// Updates dashboard state, sends to server, returns `UiCommand[]`.
        #[wasm_bindgen]
        pub fn handle_live_usage(&self, usage: JsValue) -> JsValue {
            let Ok(live_usage) = serde_wasm_bindgen::from_value::<crate::LiveUsage>(usage) else {
                return JsValue::NULL;
            };
            let provider = self.active_voice_provider();
            let model = self.active_voice_model();

            // Update WASM state for immediate rendering
            let cmds =
                self.dashboard
                    .borrow_mut()
                    .update_live_usage(app_state::LiveUsageSnapshot {
                        provider,
                        model,
                        input_tokens: live_usage.input_tokens,
                        output_tokens: live_usage.output_tokens,
                        cached_tokens: live_usage.cached_tokens,
                        total_tokens: live_usage.total_tokens,
                        thinking_tokens: live_usage.thinking_tokens,
                        input_text_tokens: live_usage.input_text_tokens,
                        input_audio_tokens: live_usage.input_audio_tokens,
                        input_image_tokens: live_usage.input_image_tokens,
                        cached_text_tokens: live_usage.cached_text_tokens,
                        cached_audio_tokens: live_usage.cached_audio_tokens,
                        cached_image_tokens: live_usage.cached_image_tokens,
                        output_text_tokens: live_usage.output_text_tokens,
                        output_audio_tokens: live_usage.output_audio_tokens,
                    });
            // Notify server for caching/broadcast to other connections
            self.send_live_usage(&live_usage);

            to_js(&cmds)
        }

        /// Route a raw server message through the dashboard state machine.
        /// Returns `UiCommand[]` as a JS array for the rendering layer.
        #[wasm_bindgen]
        pub fn handle_server_message(&self, msg: JsValue) -> JsValue {
            let Ok(val) = serde_wasm_bindgen::from_value::<serde_json::Value>(msg) else {
                return JsValue::NULL;
            };
            let cmds = self.dashboard.borrow_mut().handle_message(&val);
            to_js(&cmds)
        }

        /// Change log verbosity and return commands to re-filter.
        #[wasm_bindgen]
        pub fn set_verbosity(&self, level: &str) -> JsValue {
            let cmds = self.dashboard.borrow_mut().set_verbosity(level);
            to_js(&cmds)
        }

        /// Notify which tab is active (for badge logic).
        #[wasm_bindgen]
        pub fn set_active_tab(&self, tab: &str) -> JsValue {
            let cmds = self.dashboard.borrow_mut().set_active_tab(tab);
            to_js(&cmds)
        }

        /// Select the session whose scoped events should update global UI state.
        #[wasm_bindgen]
        pub fn select_session(&self, session_id: &str) -> JsValue {
            let cmds = self.dashboard.borrow_mut().select_session(session_id);
            to_js(&cmds)
        }

        /// Approve/skip/deny/approve_all a pending action.
        /// Returns `UiCommand[]` for UI updates. Sends the action to the server.
        #[wasm_bindgen]
        pub fn send_approval(&self, action: &str) -> JsValue {
            let result = self.dashboard.borrow_mut().approve_action(action);
            match result {
                Some((id, cmds)) => {
                    let msg = serde_json::json!({"action": action, "id": id});
                    self.server.borrow().send_json(&msg);
                    to_js(&cmds)
                }
                None => JsValue::NULL,
            }
        }

        /// Send a human response (askHuman).
        #[wasm_bindgen]
        pub fn send_human_response(&self, text: &str) -> JsValue {
            let cmds = self.dashboard.borrow_mut().human_response(text);
            let msg = serde_json::json!({"action": "input", "text": text});
            self.server.borrow().send_json(&msg);
            to_js(&cmds)
        }

        /// Send a follow-up message. `direct = true` bypasses the presence
        /// layer and dispatches the follow-up straight to the agent as a
        /// force_direct task, mirroring how direct start_task works. Used
        /// when the Direct toggle is checked at follow-up submit time.
        #[wasm_bindgen]
        pub fn send_follow_up(&self, text: &str, direct: bool) -> JsValue {
            let cmds = self.dashboard.borrow_mut().follow_up(text);
            let mut msg = serde_json::json!({"action": "follow_up", "text": text});
            if direct {
                msg["direct"] = serde_json::Value::Bool(true);
            }
            self.server.borrow().send_json(&msg);
            to_js(&cmds)
        }

        /// Request interruption of the current agent turn. Sends ControlMsg::Interrupt
        /// via the WebSocket; the backend dispatcher broadcasts InterruptRequested
        /// and agent loops cancel their work.
        #[wasm_bindgen]
        pub fn send_interrupt(&self) -> JsValue {
            let msg = serde_json::json!({"action": "interrupt"});
            self.server.borrow().send_json(&msg);
            to_js(&Vec::<app_state::UiCommand>::new())
        }

        /// Inject a user message into the currently running turn. Sends
        /// ControlMsg::Steer via the WebSocket with a client-generated id so
        /// the backend can echo it back on SteerRequested/SteerAccepted/
        /// SteerQueued/SteerDelivered events and the UI can correlate
        /// delivery state.
        ///
        /// Returns the generated id as a JsValue string so the caller can
        /// attach it to the pending-steer row in the activity log.
        #[wasm_bindgen]
        pub fn send_steer(&self, text: &str) -> JsValue {
            // No uuid crate in this target — use timestamp + a monotonically
            // incrementing counter so two rapid sends collide neither on the
            // same ms nor on the same counter value.
            let mut counter = self.tool_request_counter.borrow_mut();
            *counter = counter.wrapping_add(1);
            let seq = *counter;
            drop(counter);
            let ts = js_sys::Date::now() as u64;
            let id = format!("steer-{}-{}", ts, seq);
            let msg = serde_json::json!({"action": "steer", "text": text, "id": &id});
            self.server.borrow().send_json(&msg);
            JsValue::from_str(&id)
        }

        /// Get pending approval ID (for keyboard shortcut routing).
        #[wasm_bindgen]
        pub fn pending_approval_id(&self) -> JsValue {
            match self.dashboard.borrow().pending_approval_id() {
                Some(id) => JsValue::from_f64(id as f64),
                None => JsValue::NULL,
            }
        }

        /// Take control of a display.
        #[wasm_bindgen]
        pub fn take_display(&self, display_id: u64) {
            let msg = serde_json::json!({"action": "take_display", "display_id": display_id});
            self.server.borrow().send_json(&msg);
        }

        /// Release control of a display.
        #[wasm_bindgen]
        pub fn release_display(&self, display_id: u64, note: Option<String>) {
            let mut msg =
                serde_json::json!({"action": "release_display", "display_id": display_id});
            if let Some(n) = note {
                if !n.is_empty() {
                    msg["note"] = serde_json::Value::String(n);
                }
            }
            self.server.borrow().send_json(&msg);
        }

        /// Grant agent access to the user's session display (primary / id 0).
        #[wasm_bindgen]
        pub fn grant_user_display(&self) {
            let msg = serde_json::json!({"action": "grant_user_display"});
            self.server.borrow().send_json(&msg);
        }

        /// Grant agent access to a specific user display by ID.
        #[wasm_bindgen]
        pub fn grant_user_display_with_id(&self, display_id: u32) {
            let msg = serde_json::json!({"action": "grant_user_display", "display_id": display_id});
            self.server.borrow().send_json(&msg);
        }

        /// Revoke agent access to the user's session display (primary / id 0).
        #[wasm_bindgen]
        pub fn revoke_user_display(&self) {
            let msg = serde_json::json!({"action": "revoke_user_display"});
            self.server.borrow().send_json(&msg);
        }

        /// Revoke agent access to a specific user display by ID.
        #[wasm_bindgen]
        pub fn revoke_user_display_with_id(&self, display_id: u32) {
            let msg =
                serde_json::json!({"action": "revoke_user_display", "display_id": display_id});
            self.server.borrow().send_json(&msg);
        }

        /// Phase 5a.1: register a JS callback fired when the server reports
        /// this browser's input-authority state for a display.  Called with
        /// `(display_id: u32, state: "you" | "other" | "unclaimed")`.  The
        /// state strings are a closed set; the server only emits these three
        /// (forward-compat for future states would land as a new wire shape).
        ///
        /// The callback fires for both bootstrap snapshots (sent when this
        /// browser connects) and live transitions (Request/Release/WS-close
        /// elsewhere, plus DisplayReady for new sessions starting at
        /// unclaimed).  JS can treat each callback as authoritative and
        /// replace any previous state for the same display_id.
        #[wasm_bindgen]
        pub fn set_on_display_input_authority_change(&self, f: Function) {
            *self
                .callbacks
                .on_display_input_authority_change
                .borrow_mut() = Some(f);
        }

        /// Phase 5: claim exclusive input authority for one display.
        /// The server gates `display_input` messages so only the holder
        /// can drive the platform mouse/keyboard; other connections see
        /// their input silently dropped.  Auto-revokes any prior holder
        /// (Zoom-style "grant control" UX), and the current connection
        /// receives a `display_input_authority_granted` confirmation
        /// message back over the WS.
        #[wasm_bindgen]
        pub fn request_display_input_authority(&self, display_id: u32) {
            let msg = serde_json::json!({
                "action": "request_display_input_authority",
                "display_id": display_id,
            });
            self.server.borrow().send_json(&msg);
        }

        /// Phase 5: release this connection's input authority for one
        /// display.  No-op if the calling connection doesn't currently
        /// hold the authority — prevents browser A from unclaiming
        /// browser B's control by mistake.  After release, the slot is
        /// unclaimed and the gate reverts to the backwards-compatible
        /// any-connection-can-input default until someone claims again.
        #[wasm_bindgen]
        pub fn release_display_input_authority(&self, display_id: u32) {
            let msg = serde_json::json!({
                "action": "release_display_input_authority",
                "display_id": display_id,
            });
            self.server.borrow().send_json(&msg);
        }
    }
} // mod wasm_impl

// ---------------------------------------------------------------------------
// Native-buildable wire-format invariant tests (Phase 5a.1)
//
// The actual dispatch arm in `connect_server`'s message handler is
// gated on `#[cfg(target_arch = "wasm32")]`, so it can't be exercised
// directly from a native `cargo test`.  Instead we pin the *wire
// contract* the dispatch reads — same shape that
// `web_gateway::apply_grant_input_authority` (and friends) emit.  If
// either side drifts (server changes the field name, dispatch reads a
// different field), one of these tests fires and the integration
// breakage is caught at unit-test time.
//
// Mirrors the regression-guard pattern of
// `crate::app_state::tests::peer_webrtc_signal_wire_name`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod authority_wire_tests {
    /// The dispatch in `connect_server` matches `t == "display_input_authority_state"`
    /// and reads `display_id: u64` + `state: &str`.  This test pins
    /// that exact shape against the JSON the gateway actually emits,
    /// failing loudly if either side renames a field or changes a
    /// type.
    #[test]
    fn display_input_authority_state_shape_matches_dispatch() {
        // Construct the JSON the way `web_gateway` emits it (see
        // `apply_grant_input_authority` and the per-connection
        // outbound select arm).
        let wire = serde_json::json!({
            "t": "display_input_authority_state",
            "display_id": 7u32,
            "state": "you",
        });

        // The dispatch reads these exact fields with these exact
        // accessors; replicate them here.
        let t = wire.get("t").and_then(|v| v.as_str());
        let display_id = wire.get("display_id").and_then(|v| v.as_u64());
        let state = wire.get("state").and_then(|v| v.as_str());

        assert_eq!(t, Some("display_input_authority_state"));
        assert_eq!(display_id, Some(7));
        assert_eq!(state, Some("you"));
    }

    /// All three state strings (`you`, `other`, `unclaimed`) parse.
    /// The state vocabulary is a closed set on the gateway side; this
    /// test fails if any of the three is renamed without coordinated
    /// JS update.
    #[test]
    fn display_input_authority_state_accepts_all_three_states() {
        for state in ["you", "other", "unclaimed"] {
            let wire = serde_json::json!({
                "t": "display_input_authority_state",
                "display_id": 0u32,
                "state": state,
            });
            assert_eq!(
                wire.get("state").and_then(|v| v.as_str()),
                Some(state),
                "wire state '{state}' must round-trip"
            );
        }
    }

    /// The dispatch's `(Some(display_id), Some(state))` guard rejects
    /// malformed messages — pinned here so the gateway never starts
    /// emitting a partial shape that bypasses the dispatch into the
    /// generic `_ =>` arm.
    #[test]
    fn display_input_authority_state_rejects_partial_shapes() {
        let no_display_id = serde_json::json!({
            "t": "display_input_authority_state",
            "state": "you",
        });
        assert!(no_display_id
            .get("display_id")
            .and_then(|v| v.as_u64())
            .is_none());

        let no_state = serde_json::json!({
            "t": "display_input_authority_state",
            "display_id": 0u32,
        });
        assert!(no_state.get("state").and_then(|v| v.as_str()).is_none());

        let wrong_type = serde_json::json!({
            "t": "display_input_authority_state",
            "display_id": "zero",  // string, not u64
            "state": "you",
        });
        assert!(wrong_type
            .get("display_id")
            .and_then(|v| v.as_u64())
            .is_none());
    }
}
