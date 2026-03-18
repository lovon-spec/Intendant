//! Server WebSocket connection to the intendant web gateway.
//!
//! Handles: TUI ANSI frames, state snapshots, presence_welcome, tool requests/responses,
//! outbound events, keyboard/resize input, presence_connect/disconnect, voice_log.

use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use crate::callbacks::Callbacks;

/// Reconnect delay in milliseconds.
const RECONNECT_DELAY_MS: i32 = 3000;

/// Server connection state.
pub struct ServerConnection {
    ws: Option<WebSocket>,
    url: String,
    connected: bool,
    /// Whether the voice model is live (for re-sending presence_connect on reconnect).
    /// Shared via Rc so the onopen closure sees updates from set_voice_live() even
    /// when called after connect() but before the WebSocket opens.
    voice_live: Rc<RefCell<bool>>,
    /// Active voice provider name (e.g. "gemini", "openai") — sent in presence_connect.
    /// Shared via Rc so the onopen closure sees updates from set_active_voice().
    active_provider: Rc<RefCell<String>>,
    /// Active voice model name — sent in presence_connect.
    /// Shared via Rc so the onopen closure sees updates from set_active_voice().
    active_model: Rc<RefCell<String>>,
    /// When true, this browser never requests active status (observer mode).
    /// Shared via Rc so the onopen closure sees updates from set_passive_mode().
    passive_mode: Rc<RefCell<bool>>,
    callbacks: Rc<Callbacks>,
    /// Closures must be stored to prevent drop while WebSocket holds references.
    _onopen: Option<Closure<dyn FnMut()>>,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
    _onerror: Option<Closure<dyn FnMut()>>,
    /// Handles server messages (term, state_snapshot, presence_welcome, tool_response, events).
    /// Stored as a shared handler so the main module can process messages.
    on_message_handler: Option<Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>>>,
    /// Voice log sequence counter (monotonic).
    voice_log_seq: u64,
    /// Last event sequence number from the server.
    last_event_seq: u64,
    /// Server session ID (from presence_welcome, sent on reconnect).
    server_session_id: Option<String>,
}

impl ServerConnection {
    pub fn new(callbacks: Rc<Callbacks>) -> Self {
        Self {
            ws: None,
            url: String::new(),
            connected: false,
            voice_live: Rc::new(RefCell::new(false)),
            active_provider: Rc::new(RefCell::new(String::new())),
            active_model: Rc::new(RefCell::new(String::new())),
            passive_mode: Rc::new(RefCell::new(false)),
            callbacks,
            _onopen: None,
            _onmessage: None,
            _onclose: None,
            _onerror: None,
            on_message_handler: None,
            voice_log_seq: 0,
            last_event_seq: 0,
            server_session_id: None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn set_voice_live(&mut self, live: bool) {
        *self.voice_live.borrow_mut() = live;
    }

    /// Set the active voice provider and model (sent in presence_connect messages).
    pub fn set_active_voice(&mut self, provider: &str, model: &str) {
        *self.active_provider.borrow_mut() = provider.to_string();
        *self.active_model.borrow_mut() = model.to_string();
    }

    /// Set passive mode — this browser will never request active status.
    pub fn set_passive_mode(&mut self, passive: bool) {
        *self.passive_mode.borrow_mut() = passive;
    }

    /// Set a handler for parsed server messages.
    pub fn set_message_handler(
        &mut self,
        handler: Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>>,
    ) {
        self.on_message_handler = Some(handler);
    }

    /// Connect to the server WebSocket.
    pub fn connect(&mut self, url: &str) {
        // Close any existing connection
        self.disconnect();
        self.url = url.to_string();

        let ws = match WebSocket::new(url) {
            Ok(ws) => ws,
            Err(e) => {
                self.callbacks
                    .invoke_error(&format!("WebSocket connect failed: {:?}", e));
                return;
            }
        };

        // Set up event handlers using closures stored in self
        let callbacks = self.callbacks.clone();
        let url_clone = url.to_string();

        // onopen
        let callbacks_open = callbacks.clone();
        // We need shared mutable state for the connection flag and voice_live.
        // Since WASM is single-threaded, Rc<RefCell<>> is safe.
        let connected_flag = Rc::new(RefCell::new(false));
        let ws_clone = ws.clone();

        let connected_open = connected_flag.clone();
        // Clone the shared Rcs — not snapshots, so set_voice_live/set_active_voice
        // changes are visible to the onopen closure even if called after connect().
        let voice_open = self.voice_live.clone();
        let session_id_open = Rc::new(RefCell::new(self.server_session_id.clone()));
        let last_seq_open = Rc::new(RefCell::new(self.last_event_seq));
        let provider_open = self.active_provider.clone();
        let model_open = self.active_model.clone();
        let passive_open = self.passive_mode.clone();
        let onopen = Closure::wrap(Box::new(move || {
            *connected_open.borrow_mut() = true;
            callbacks_open.invoke_server_state(true);
            // Re-send presence_connect if voice model was active before reconnect
            if *voice_open.borrow() {
                let mut msg = serde_json::json!({
                    "t": "presence_connect",
                    "server_session_id": *session_id_open.borrow(),
                    "last_event_seq": *last_seq_open.borrow(),
                });
                let p = provider_open.borrow();
                if !p.is_empty() {
                    msg["provider"] = serde_json::Value::String(p.clone());
                }
                let m = model_open.borrow();
                if !m.is_empty() {
                    msg["model"] = serde_json::Value::String(m.clone());
                }
                if *passive_open.borrow() {
                    msg["passive"] = serde_json::Value::Bool(true);
                }
                let _ = ws_clone.send_with_str(&msg.to_string());
            }
        }) as Box<dyn FnMut()>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        // onmessage
        let handler = self.on_message_handler.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(ref h) = handler {
                        (h.borrow_mut())(json);
                    }
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose — reconnect after delay
        let callbacks_close = callbacks.clone();
        let connected_close = connected_flag.clone();
        let onclose = Closure::wrap(Box::new(move |_e: CloseEvent| {
            *connected_close.borrow_mut() = false;
            callbacks_close.invoke_server_state(false);
            // Schedule reconnect
            let url_rc = url_clone.clone();
            let _ = web_sys::window().map(|w| {
                // We can't call self.connect() from a closure, so we just
                // signal the disconnection. The main module handles reconnect.
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    &js_sys::Function::new_no_args(&format!(
                        "if (window.__presenceWeb) window.__presenceWeb.reconnect_server('{}')",
                        url_rc.replace('\'', "\\'")
                    )),
                    RECONNECT_DELAY_MS,
                );
            });
        }) as Box<dyn FnMut(CloseEvent)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // onerror
        let callbacks_err = callbacks;
        let onerror = Closure::wrap(Box::new(move || {
            callbacks_err.invoke_error("Server WebSocket error");
        }) as Box<dyn FnMut()>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        self.ws = Some(ws);
        self._onopen = Some(onopen);
        self._onmessage = Some(onmessage);
        self._onclose = Some(onclose);
        self._onerror = Some(onerror);
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

    /// Send a JSON message to the server.
    pub fn send_json(&self, msg: &serde_json::Value) -> bool {
        if let Some(ref ws) = self.ws {
            if ws.ready_state() != 1 {
                // WebSocket not in OPEN state — log and drop
                web_sys::console::warn_1(
                    &format!("[presence-web] send_json dropped (readyState={}): {}",
                        ws.ready_state(),
                        msg.get("t").and_then(|v| v.as_str()).or_else(|| msg.get("action").and_then(|v| v.as_str())).unwrap_or("?")
                    ).into(),
                );
                return false;
            }
            ws.send_with_str(&msg.to_string()).is_ok()
        } else {
            false
        }
    }

    /// Send a keyboard event.
    pub fn send_key(&self, key: &str, ctrl: bool, alt: bool, shift: bool) {
        let msg = serde_json::json!({
            "t": "key",
            "key": key,
            "ctrl": ctrl,
            "alt": alt,
            "shift": shift,
        });
        self.send_json(&msg);
    }

    /// Send a resize event.
    pub fn send_resize(&self, cols: u16, rows: u16) {
        let msg = serde_json::json!({
            "t": "resize",
            "cols": cols,
            "rows": rows,
        });
        self.send_json(&msg);
    }

    /// Send presence_connect notification (replaces live_connected).
    /// Includes the active voice provider/model so the server can display the correct name.
    pub fn send_presence_connect(&self) {
        let mut msg = serde_json::json!({
            "t": "presence_connect",
            "server_session_id": self.server_session_id,
            "last_event_seq": self.last_event_seq,
        });
        let p = self.active_provider.borrow();
        if !p.is_empty() {
            msg["provider"] = serde_json::Value::String(p.clone());
        }
        let m = self.active_model.borrow();
        if !m.is_empty() {
            msg["model"] = serde_json::Value::String(m.clone());
        }
        if *self.passive_mode.borrow() {
            msg["passive"] = serde_json::Value::Bool(true);
        }
        self.send_json(&msg);
    }

    /// Send presence_disconnect notification (replaces live_disconnected).
    pub fn send_presence_disconnect(&self) {
        self.send_json(&serde_json::json!({"t": "presence_disconnect"}));
    }

    /// Send live_connected notification (legacy, kept for backward compatibility).
    pub fn send_live_connected(&self) {
        self.send_presence_connect();
    }

    /// Send live_disconnected notification (legacy, kept for backward compatibility).
    pub fn send_live_disconnected(&self) {
        self.send_presence_disconnect();
    }

    /// Send a voice transcript log entry.
    pub fn send_voice_log(&mut self, text: &str, tool_context: Option<&str>) {
        self.voice_log_seq += 1;
        let msg = serde_json::json!({
            "t": "voice_log",
            "text": text,
            "seq": self.voice_log_seq,
            "tool_context": tool_context,
        });
        self.send_json(&msg);
    }

    /// Send a presence checkpoint.
    pub fn send_presence_checkpoint(&self, summary: &str) {
        let msg = serde_json::json!({
            "t": "presence_checkpoint",
            "summary": summary,
            "last_event_seq": self.last_event_seq,
        });
        self.send_json(&msg);
    }

    /// Send raw PCM16 audio (base64-encoded) to the server for transcription.
    pub fn send_user_audio(&self, base64_pcm: &str) {
        let msg = serde_json::json!({
            "t": "user_audio",
            "data": base64_pcm,
        });
        self.send_json(&msg);
    }

    /// Send a voice diagnostic to the server (errors, silence, disconnects).
    pub fn send_voice_diagnostic(&self, kind: &str, detail: &str) {
        let msg = serde_json::json!({
            "t": "voice_diagnostic",
            "kind": kind,
            "detail": detail,
        });
        self.send_json(&msg);
    }

    /// Update the last event sequence number (call when receiving server events).
    pub fn set_last_event_seq(&mut self, seq: u64) {
        self.last_event_seq = seq;
    }

    /// Set the server session ID (from presence_welcome).
    pub fn set_server_session_id(&mut self, id: Option<String>) {
        self.server_session_id = id;
    }

    /// Send a tool_request to the server.
    pub fn send_tool_request(&self, id: &str, tool: &str, args: &serde_json::Value) {
        let msg = serde_json::json!({
            "t": "tool_request",
            "id": id,
            "tool": tool,
            "args": args,
        });
        self.send_json(&msg);
    }

    /// Send an async_query to the server (fire-and-forget for NeedsIO tools).
    /// Result arrives later as an `async_query_result` message.
    pub fn send_async_query(&self, id: &str, tool: &str, args: &serde_json::Value) {
        let msg = serde_json::json!({
            "t": "async_query",
            "id": id,
            "tool": tool,
            "args": args,
        });
        self.send_json(&msg);
    }

    /// Send a ControlMsg action to the server. Returns true if sent.
    pub fn send_action(&self, action: &serde_json::Value) -> bool {
        self.send_json(action)
    }

    /// Request to become the active voice owner (triggers handover from current active).
    pub fn send_make_active(&self) {
        self.send_json(&serde_json::json!({"t": "make_active"}));
    }
}
