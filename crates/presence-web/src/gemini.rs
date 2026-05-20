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
    val.serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .unwrap_or(JsValue::NULL)
}

fn modality_tokens(usage: &serde_json::Value, field: &str, modality: &str) -> u64 {
    usage
        .get(field)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("modality")
                        .and_then(|v| v.as_str())
                        .map(|m| m.eq_ignore_ascii_case(modality))
                        .unwrap_or(false)
                })
                .filter_map(|item| item.get("tokenCount").and_then(|v| v.as_u64()))
                .sum()
        })
        .unwrap_or(0)
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
        // Per-turn counters for hallucination detection diagnostics.
        let turn_tool_calls = Rc::new(Cell::new(0u32));
        let turn_has_speech = Rc::new(Cell::new(false));
        let turn_tool_calls_inner = turn_tool_calls.clone();
        let turn_has_speech_inner = turn_has_speech.clone();
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
                    Self::handle_message_static(
                        &callbacks,
                        &msg,
                        &turn_tool_calls_inner,
                        &turn_has_speech_inner,
                    );
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

    /// Deserialize tools JsValue to serde_json::Value, logging the result.
    fn deserialize_tools(callbacks: &Callbacks, tools: &JsValue) -> serde_json::Value {
        match serde_wasm_bindgen::from_value::<serde_json::Value>(tools.clone()) {
            Ok(val) => {
                let count = val.as_array().map(|a| a.len()).unwrap_or(0);
                callbacks.invoke_diagnostic(
                    "gemini_setup",
                    &format!("tools deserialized: {} function declarations", count),
                );
                if count == 0 {
                    callbacks.invoke_diagnostic(
                        "gemini_setup",
                        "WARNING: zero tools — model will not be able to call functions",
                    );
                }
                val
            }
            Err(e) => {
                callbacks.invoke_diagnostic(
                    "gemini_setup",
                    &format!(
                        "ERROR: tools deserialization failed: {:?} — sending empty tools",
                        e
                    ),
                );
                serde_json::Value::Array(vec![])
            }
        }
    }

    /// Setup message for BidiGenerateContentConstrained (ephemeral tokens).
    /// Model + generation_config are baked into the token; only send tools + system prompt.
    fn build_setup_message(&self, system_prompt: &str, tools: &JsValue) -> String {
        let tools_val = Self::deserialize_tools(&self.callbacks, tools);

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
        let tools_val = Self::deserialize_tools(&self.callbacks, tools);

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
                "output_audio_transcription": {},
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

    fn handle_message_static(
        callbacks: &Callbacks,
        msg: &serde_json::Value,
        turn_tool_calls: &Cell<u32>,
        turn_has_speech: &Cell<bool>,
    ) {
        // usageMetadata can appear alongside any server message.
        // Normalize Gemini-specific fields into provider-agnostic LiveUsage.
        if let Some(usage) = msg.get("usageMetadata") {
            let total = usage["totalTokenCount"].as_u64().unwrap_or(0);
            callbacks.invoke_diagnostic(
                "gemini_usage",
                &format!(
                    "tokens: total={} prompt={} response={} thinking={}",
                    total,
                    usage["promptTokenCount"].as_u64().unwrap_or(0),
                    usage["responseTokenCount"].as_u64().unwrap_or(0),
                    usage["thoughtsTokenCount"].as_u64().unwrap_or(0),
                ),
            );
            let live_usage = crate::LiveUsage {
                input_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0),
                output_tokens: usage["responseTokenCount"].as_u64().unwrap_or(0),
                cached_tokens: usage["cachedContentTokenCount"].as_u64().unwrap_or(0),
                total_tokens: usage["totalTokenCount"].as_u64().unwrap_or(0),
                thinking_tokens: usage["thoughtsTokenCount"].as_u64().unwrap_or(0),
                input_text_tokens: modality_tokens(usage, "promptTokensDetails", "TEXT"),
                input_audio_tokens: modality_tokens(usage, "promptTokensDetails", "AUDIO"),
                input_image_tokens: modality_tokens(usage, "promptTokensDetails", "IMAGE")
                    + modality_tokens(usage, "promptTokensDetails", "VIDEO"),
                cached_text_tokens: modality_tokens(usage, "cacheTokensDetails", "TEXT")
                    + modality_tokens(usage, "cachedContentTokensDetails", "TEXT"),
                cached_audio_tokens: modality_tokens(usage, "cacheTokensDetails", "AUDIO")
                    + modality_tokens(usage, "cachedContentTokensDetails", "AUDIO"),
                cached_image_tokens: modality_tokens(usage, "cacheTokensDetails", "IMAGE")
                    + modality_tokens(usage, "cacheTokensDetails", "VIDEO")
                    + modality_tokens(usage, "cachedContentTokensDetails", "IMAGE")
                    + modality_tokens(usage, "cachedContentTokensDetails", "VIDEO"),
                output_text_tokens: modality_tokens(usage, "responseTokensDetails", "TEXT"),
                output_audio_tokens: modality_tokens(usage, "responseTokensDetails", "AUDIO"),
            };
            callbacks.invoke_live_usage(&to_js_object(
                &serde_json::to_value(&live_usage).unwrap_or_default(),
            ));
        }

        // setupComplete
        if msg.get("setupComplete").is_some() {
            callbacks.invoke_diagnostic("gemini_msg", "setupComplete");
            callbacks.invoke_voice_ready();
            return;
        }

        // toolCall
        if let Some(tool_call) = msg.get("toolCall") {
            if let Some(fcs) = tool_call.get("functionCalls").and_then(|v| v.as_array()) {
                turn_tool_calls.set(turn_tool_calls.get() + fcs.len() as u32);
                let names: Vec<&str> = fcs
                    .iter()
                    .filter_map(|fc| fc.get("name").and_then(|v| v.as_str()))
                    .collect();
                callbacks
                    .invoke_diagnostic("gemini_msg", &format!("toolCall: {}", names.join(", ")));
                for fc in fcs {
                    let call_js = to_js_object(fc);
                    callbacks.invoke_voice_tool_call(&call_js);
                }
            }
            return;
        }

        // toolCallCancellation — ignore
        if msg.get("toolCallCancellation").is_some() {
            callbacks.invoke_diagnostic("gemini_msg", "toolCallCancellation");
            return;
        }

        // serverContent
        if let Some(response) = msg.get("serverContent") {
            // Output audio transcription (text of what was spoken)
            if let Some(transcript) = response.get("outputTranscription") {
                if let Some(text) = transcript.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        turn_has_speech.set(true);
                        callbacks.invoke_voice_transcript(text);
                    }
                }
                return;
            }
            if response.get("turnComplete").is_some() {
                let tools = turn_tool_calls.get();
                let spoke = turn_has_speech.get();
                if spoke && tools == 0 {
                    callbacks.invoke_diagnostic(
                        "no_tool_turn",
                        "Model spoke without calling any tool — possible hallucination",
                    );
                }
                callbacks.invoke_diagnostic(
                    "gemini_msg",
                    &format!("turnComplete (tools={}, spoke={})", tools, spoke),
                );
                // Reset for next turn
                turn_tool_calls.set(0);
                turn_has_speech.set(false);
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
                        // Function call in model turn
                        if part.get("functionCall").is_some() {
                            has_tool = true;
                            turn_tool_calls.set(turn_tool_calls.get() + 1);
                            let call_js = to_js_object(part.get("functionCall").unwrap());
                            callbacks.invoke_voice_tool_call(&call_js);
                        }
                    }
                    let mut kinds = Vec::new();
                    if has_audio {
                        kinds.push("audio");
                    }
                    if has_text {
                        kinds.push("text");
                    }
                    if has_tool {
                        kinds.push("functionCall");
                    }
                    callbacks.invoke_diagnostic(
                        "gemini_msg",
                        &format!("serverContent({})", kinds.join("+")),
                    );
                }
            }
        }
    }

    /// Send a video frame to Gemini Live with its frame ID annotation.
    ///
    /// Uses `client_content` with `turn_complete: false` to deliver both the
    /// image and its ID label atomically without interrupting the model's
    /// current response. This avoids a race condition where a separate
    /// `turn_complete: true` annotation could cancel an in-progress tool call.
    pub fn send_frame(&self, base64_jpeg: &str, frame_id: &str) {
        if let Some(ref ws) = self.ws {
            let msg = serde_json::json!({
                "client_content": {
                    "turns": [{
                        "role": "user",
                        "parts": [
                            {
                                "inlineData": {
                                    "mimeType": "image/jpeg",
                                    "data": base64_jpeg
                                }
                            },
                            {
                                "text": format!("[frame:{}]", frame_id)
                            }
                        ]
                    }],
                    "turn_complete": false
                }
            });
            let _ = ws.send_with_str(&msg.to_string());
            self.callbacks.invoke_diagnostic(
                "video_send",
                &format!("frame {} ({}B)", frame_id, base64_jpeg.len()),
            );
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
            let count = self.audio_send_count.get() + 1;
            self.audio_send_count.set(count);
            self.callbacks.invoke_diagnostic(
                "audio_send",
                &format!("chunk #{} ({}B)", count, base64_pcm.len()),
            );
        } else {
            self.callbacks
                .invoke_diagnostic("audio_send", "DROPPED — no WebSocket");
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

    /// Send text without ending the user turn. Used for injecting tool results
    /// and system messages that should not interrupt the model's current response.
    pub fn send_text_passive(&self, text: &str) {
        if let Some(ref ws) = self.ws {
            let msg = serde_json::json!({
                "client_content": {
                    "turns": [{"role": "user", "parts": [{"text": text}]}],
                    "turn_complete": false
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
