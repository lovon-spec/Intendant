//! Gemini Live (BidiGenerateContent) WebSocket voice provider.

use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use crate::callbacks::Callbacks;

const DEFAULT_MODEL: &str = "gemini-2.5-flash-native-audio-preview-12-2025";
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

        let url = format!("{}?access_token={}", API_BASE, token);
        let ws = match WebSocket::new(&url) {
            Ok(ws) => ws,
            Err(e) => {
                self.callbacks
                    .invoke_error(&format!("Gemini connect failed: {:?}", e));
                return;
            }
        };

        // Build setup message
        let setup_msg = self.build_setup_message(system_prompt, tools);

        // onopen — send setup
        let ws_setup = ws.clone();
        let onopen = Closure::wrap(Box::new(move || {
            let _ = ws_setup.send_with_str(&setup_msg);
        }) as Box<dyn FnMut()>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        // onmessage — parse Gemini protocol
        let callbacks = self.callbacks.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                    Self::handle_message_static(&callbacks, &msg);
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose
        let callbacks_close = self.callbacks.clone();
        let onclose = Closure::wrap(Box::new(move |e: CloseEvent| {
            callbacks_close.invoke_error(&format!("Gemini disconnected ({})", e.code()));
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

    fn build_setup_message(&self, system_prompt: &str, tools: &JsValue) -> String {
        // Convert tools JsValue to serde_json::Value
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

    fn handle_message_static(callbacks: &Callbacks, msg: &serde_json::Value) {
        // setupComplete
        if msg.get("setupComplete").is_some() {
            callbacks.invoke_voice_ready();
            return;
        }

        // toolCall
        if let Some(tool_call) = msg.get("toolCall") {
            if let Some(fcs) = tool_call.get("functionCalls").and_then(|v| v.as_array()) {
                for fc in fcs {
                    let call_js = serde_wasm_bindgen::to_value(fc).unwrap_or(JsValue::NULL);
                    callbacks.invoke_voice_tool_call(&call_js);
                }
            }
            return;
        }

        // toolCallCancellation — ignore
        if msg.get("toolCallCancellation").is_some() {
            return;
        }

        // serverContent
        if let Some(response) = msg.get("serverContent") {
            if response.get("turnComplete").is_some() {
                return;
            }
            if response.get("interrupted").is_some() {
                callbacks.invoke_voice_interrupted();
                return;
            }
            if let Some(model_turn) = response.get("modelTurn") {
                if let Some(parts) = model_turn.get("parts").and_then(|v| v.as_array()) {
                    for part in parts {
                        // Audio data
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(mime) = inline.get("mimeType").and_then(|v| v.as_str()) {
                                if mime.starts_with("audio/") {
                                    if let Some(data) = inline.get("data").and_then(|v| v.as_str())
                                    {
                                        callbacks.invoke_voice_audio(data);
                                    }
                                }
                            }
                        }
                        // Text
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            callbacks.invoke_voice_text(text);
                        }
                        // Function call in model turn
                        if part.get("functionCall").is_some() {
                            let call_js =
                                serde_wasm_bindgen::to_value(part.get("functionCall").unwrap())
                                    .unwrap_or(JsValue::NULL);
                            callbacks.invoke_voice_tool_call(&call_js);
                        }
                    }
                }
            }
        }
    }

    pub fn send_audio(&self, base64_pcm: &str) {
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
