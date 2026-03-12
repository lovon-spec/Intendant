//! Gemini Live (BidiGenerateContent) WebSocket voice provider.

use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use serde::Serialize;

use crate::callbacks::Callbacks;

/// Serialize a serde_json::Value into a JsValue with maps as plain JS objects
/// (not ES6 Map). This is required because serde-wasm-bindgen 0.6+ defaults
/// to Map, which makes Object.keys() return [] and property access fail.
fn to_js_object(val: &serde_json::Value) -> JsValue {
    val.serialize(
        &serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true),
    )
    .unwrap_or(JsValue::NULL)
}

const DEFAULT_MODEL: &str = "gemini-2.5-flash-native-audio-preview-12-2025";
/// Constrained endpoint — used with ephemeral tokens (baked model+config).
const API_BASE_CONSTRAINED: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContentConstrained";
/// Non-constrained endpoint — used with API keys (supports tool calling).
const API_BASE: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContent";

pub struct GeminiProvider {
    ws: Option<WebSocket>,
    pub connected: bool,
    model: String,
    input_sample_rate: u32,
    callbacks: Rc<Callbacks>,
    _onopen: Option<Closure<dyn FnMut()>>,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
    _onerror: Option<Closure<dyn FnMut()>>,
    /// Audio send counter for diagnostics.
    audio_send_count: Cell<u64>,
    /// Pauses audio sending while a blocking tool call is pending.
    /// Set to true on toolCall, cleared on tool_response send.
    tool_call_pending: Rc<Cell<bool>>,
}

impl GeminiProvider {
    pub fn new(callbacks: Rc<Callbacks>) -> Self {
        Self {
            ws: None,
            connected: false,
            model: DEFAULT_MODEL.to_string(),
            input_sample_rate: 16000,
            callbacks,
            _onopen: None,
            _onmessage: None,
            _onclose: None,
            _onerror: None,
            audio_send_count: Cell::new(0),
            tool_call_pending: Rc::new(Cell::new(false)),
        }
    }

    pub fn connect(
        &mut self,
        token: &str,
        model: Option<&str>,
        input_sample_rate: Option<u32>,
        system_prompt: &str,
        tools: &JsValue,
    ) {
        if let Some(m) = model {
            self.model = m.to_string();
        }
        if let Some(r) = input_sample_rate {
            self.input_sample_rate = r;
        }

        self.disconnect();

        // Detect auth mode: ephemeral tokens start with "auth_tokens/",
        // everything else is treated as an API key.
        let is_ephemeral = token.starts_with("auth_tokens/");
        let url = if is_ephemeral {
            // Constrained endpoint — model+config baked in token, binary frames, no tool calls
            format!("{}?access_token={}", API_BASE_CONSTRAINED, token)
        } else {
            // Non-constrained endpoint — API key auth, text frames, tool calls work
            format!("{}?key={}", API_BASE, token)
        };
        let ws = match WebSocket::new(&url) {
            Ok(ws) => ws,
            Err(e) => {
                self.callbacks
                    .invoke_error(&format!("Gemini connect failed: {:?}", e));
                return;
            }
        };

        // BidiGenerateContentConstrained sends binary frames; non-constrained sends text.
        // Set ArrayBuffer type for both — text frames still work as string.
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // Build setup message. With ephemeral tokens (constrained), model+generation_config
        // are baked into the token, so we only send tools + system_instruction.
        // With API keys (non-constrained), we send the full setup including model+config.
        let setup_msg = if is_ephemeral {
            self.build_setup_message(system_prompt, tools)
        } else {
            self.build_full_setup_message(system_prompt, tools)
        };

        // onopen — send setup
        let ws_setup = ws.clone();
        let onopen = Closure::wrap(Box::new(move || {
            let _ = ws_setup.send_with_str(&setup_msg);
        }) as Box<dyn FnMut()>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        // onmessage — parse Gemini protocol
        // BidiGenerateContentConstrained sends binary (ArrayBuffer) frames,
        // while BidiGenerateContent sends text frames. Handle both.
        let callbacks = self.callbacks.clone();
        let pending = self.tool_call_pending.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            let text = if let Some(s) = e.data().as_string() {
                Some(s)
            } else if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                let bytes = arr.to_vec();
                String::from_utf8(bytes).ok()
            } else {
                None
            };
            if let Some(text) = text {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                    Self::handle_message_static(&callbacks, &pending, &msg);
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose
        let callbacks_close = self.callbacks.clone();
        let onclose = Closure::wrap(Box::new(move |e: CloseEvent| {
            let reason = e.reason();
            let detail = if reason.is_empty() {
                format!("Gemini disconnected (code={})", e.code())
            } else {
                format!("Gemini disconnected (code={}, reason={})", e.code(), reason)
            };
            callbacks_close.invoke_diagnostic("gemini_close", &detail);
            callbacks_close.invoke_error(&detail);
        }) as Box<dyn FnMut(CloseEvent)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // onerror
        let callbacks_err = self.callbacks.clone();
        let onerror = Closure::wrap(Box::new(move || {
            callbacks_err.invoke_error("Gemini WebSocket error");
        }) as Box<dyn FnMut()>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        self.ws = Some(ws);
        self._onopen = Some(onopen);
        self._onmessage = Some(onmessage);
        self._onclose = Some(onclose);
        self._onerror = Some(onerror);
    }

    /// Setup message for BidiGenerateContentConstrained (ephemeral tokens).
    /// Model + generation_config are baked into the token; only send tools + system prompt.
    fn build_setup_message(&self, system_prompt: &str, tools: &JsValue) -> String {
        let tools_val: serde_json::Value =
            serde_wasm_bindgen::from_value(tools.clone()).unwrap_or(serde_json::Value::Array(vec![]));

        let setup = serde_json::json!({
            "setup": {
                "system_instruction": {
                    "parts": [{ "text": system_prompt }]
                },
                "tools": [{
                    "function_declarations": tools_val
                }]
            }
        });
        setup.to_string()
    }

    /// Full setup message for BidiGenerateContent (API key auth).
    /// Includes model, generation_config, tools, and system prompt.
    fn build_full_setup_message(&self, system_prompt: &str, tools: &JsValue) -> String {
        let tools_val: serde_json::Value =
            serde_wasm_bindgen::from_value(tools.clone()).unwrap_or(serde_json::Value::Array(vec![]));

        let setup = serde_json::json!({
            "setup": {
                "model": format!("models/{}", self.model),
                "generation_config": {
                    "response_modalities": ["AUDIO"],
                    "speech_config": {
                        "voice_config": {
                            "prebuilt_voice_config": {
                                "voice_name": "Aoede"
                            }
                        }
                    }
                },
                "system_instruction": {
                    "parts": [{ "text": system_prompt }]
                },
                "tools": [{
                    "function_declarations": tools_val
                }]
            }
        });
        setup.to_string()
    }

    fn handle_message_static(callbacks: &Callbacks, tool_call_pending: &Rc<Cell<bool>>, msg: &serde_json::Value) {
        // setupComplete
        if msg.get("setupComplete").is_some() {
            callbacks.invoke_diagnostic("gemini_msg", "setupComplete");
            callbacks.invoke_voice_ready();
            return;
        }

        // toolCall — pause audio until tool_response is sent
        if let Some(tool_call) = msg.get("toolCall") {
            if let Some(fcs) = tool_call.get("functionCalls").and_then(|v| v.as_array()) {
                tool_call_pending.set(true);
                let names: Vec<&str> = fcs.iter()
                    .filter_map(|fc| fc.get("name").and_then(|v| v.as_str()))
                    .collect();
                callbacks.invoke_diagnostic("gemini_msg", &format!("toolCall: {} (audio paused)", names.join(", ")));
                for fc in fcs {
                    let call_js = to_js_object(fc);
                    callbacks.invoke_voice_tool_call(&call_js);
                }
            }
            return;
        }

        // toolCallCancellation — resume audio
        if msg.get("toolCallCancellation").is_some() {
            tool_call_pending.set(false);
            callbacks.invoke_diagnostic("gemini_msg", "toolCallCancellation (audio resumed)");
            return;
        }

        // serverContent
        if let Some(response) = msg.get("serverContent") {
            if response.get("turnComplete").is_some() {
                callbacks.invoke_diagnostic("gemini_msg", "turnComplete");
                return;
            }
            if response.get("interrupted").is_some() {
                callbacks.invoke_diagnostic("gemini_msg", "interrupted");
                callbacks.invoke_voice_interrupted();
                return;
            }
            if let Some(model_turn) = response.get("modelTurn") {
                if let Some(parts) = model_turn.get("parts").and_then(|v| v.as_array()) {
                    let mut has_audio = false;
                    let mut has_text = false;
                    let mut has_tool = false;
                    for part in parts {
                        // Audio data
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(mime) = inline.get("mimeType").and_then(|v| v.as_str()) {
                                if mime.starts_with("audio/") {
                                    has_audio = true;
                                    if let Some(data) = inline.get("data").and_then(|v| v.as_str())
                                    {
                                        callbacks.invoke_voice_audio(data);
                                    }
                                }
                            }
                        }
                        // Text
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            has_text = true;
                            callbacks.invoke_voice_text(text);
                        }
                        // Function call in model turn — pause audio
                        if part.get("functionCall").is_some() {
                            has_tool = true;
                            tool_call_pending.set(true);
                            let call_js = to_js_object(part.get("functionCall").unwrap());
                            callbacks.invoke_voice_tool_call(&call_js);
                        }
                    }
                    let mut kinds = Vec::new();
                    if has_audio { kinds.push("audio"); }
                    if has_text { kinds.push("text"); }
                    if has_tool { kinds.push("functionCall"); }
                    callbacks.invoke_diagnostic(
                        "gemini_msg",
                        &format!("serverContent({})", kinds.join("+")),
                    );
                }
            }
        }
    }

    pub fn send_audio(&self, base64_pcm: &str) {
        // Gemini Live API requires audio to stop while a blocking tool call
        // is pending. Sending audio during a tool call causes 1008 disconnect.
        if self.tool_call_pending.get() {
            return;
        }
        if let Some(ref ws) = self.ws {
            let msg = serde_json::json!({
                "realtime_input": {
                    "media_chunks": [{
                        "mime_type": format!("audio/pcm;rate={}", self.input_sample_rate),
                        "data": base64_pcm
                    }]
                }
            });
            let _ = ws.send_with_str(&msg.to_string());
            let count = self.audio_send_count.get() + 1;
            self.audio_send_count.set(count);
            self.callbacks.invoke_diagnostic(
                "audio_send",
                &format!("chunk #{} ({}B)", count, base64_pcm.len()),
            );
        } else {
            self.callbacks.invoke_diagnostic("audio_send", "DROPPED — no WebSocket");
        }
    }

    pub fn send_text(&self, text: &str) {
        if let Some(ref ws) = self.ws {
            let msg = serde_json::json!({
                "client_content": {
                    "turns": [{"role": "user", "parts": [{"text": text}]}],
                    "turn_complete": true
                }
            });
            let _ = ws.send_with_str(&msg.to_string());
        }
    }

    pub fn send_tool_response(&self, call: &JsValue, result: &JsValue) {
        if let Some(ref ws) = self.ws {
            let call_val: serde_json::Value =
                serde_wasm_bindgen::from_value(call.clone()).unwrap_or_default();
            let result_val: serde_json::Value =
                serde_wasm_bindgen::from_value(result.clone()).unwrap_or_default();

            let name = call_val["name"].as_str().unwrap_or("");
            let id = call_val["id"]
                .as_str()
                .or_else(|| call_val["name"].as_str())
                .unwrap_or("");

            let msg = serde_json::json!({
                "tool_response": {
                    "function_responses": [{
                        "name": name,
                        "id": id,
                        "response": result_val
                    }]
                }
            });
            let _ = ws.send_with_str(&msg.to_string());
            // Resume audio after tool response is sent
            self.tool_call_pending.set(false);
        }
    }

    pub fn disconnect(&mut self) {
        if let Some(ref ws) = self.ws {
            let _ = ws.close();
        }
        self.ws = None;
        self.connected = false;
        self._onopen = None;
        self._onmessage = None;
        self._onclose = None;
        self._onerror = None;
    }
}
