//! Read-only WebSocket connection to a secondary intendant daemon.
//!
//! The primary daemon (the one serving `app.html`) continues to use
//! [`crate::server::ServerConnection`] with its full voice/approval/tool
//! request surface. Secondaries are observers — they forward every
//! received message through a single JS callback so the dashboard can
//! interleave activity, aggregate stats, etc. Secondaries never send
//! anything except an initial `presence_connect` for bootstrap state.
//!
//! Each secondary is keyed by a `host_id` — the JS layer derives it
//! from the remote's Agent Card at `/.well-known/agent-card.json`
//! (typically the card's `label` field) so the JS side can route
//! events back to per-host DOM elements without a protocol change.

use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use crate::callbacks::Callbacks;

const RECONNECT_DELAY_MS: i32 = 3000;

pub struct SecondaryConnection {
    host_id: String,
    label: String,
    url: String,
    ws: Option<WebSocket>,
    connected: Rc<RefCell<bool>>,
    callbacks: Rc<Callbacks>,
    _onopen: Option<Closure<dyn FnMut()>>,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
    _onerror: Option<Closure<dyn FnMut()>>,
}

impl SecondaryConnection {
    pub fn new(host_id: &str, label: &str, callbacks: Rc<Callbacks>) -> Self {
        Self {
            host_id: host_id.to_string(),
            label: label.to_string(),
            url: String::new(),
            ws: None,
            connected: Rc::new(RefCell::new(false)),
            callbacks,
            _onopen: None,
            _onmessage: None,
            _onclose: None,
            _onerror: None,
        }
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    pub fn connect(&mut self, url: &str) {
        self.disconnect();
        self.url = url.to_string();

        let ws = match WebSocket::new(url) {
            Ok(ws) => ws,
            Err(e) => {
                self.callbacks.invoke_error(&format!(
                    "secondary {} WebSocket connect failed: {:?}",
                    self.host_id, e
                ));
                return;
            }
        };

        // onopen
        let host_id = self.host_id.clone();
        let connected_flag = self.connected.clone();
        let cb_open = self.callbacks.clone();
        let onopen = Closure::wrap(Box::new(move || {
            *connected_flag.borrow_mut() = true;
            cb_open.invoke_secondary_state(&host_id, true);
        }) as Box<dyn FnMut()>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        // onmessage
        let host_id_msg = self.host_id.clone();
        let cb_msg = self.callbacks.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            let Some(text) = e.data().as_string() else { return };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return };
            // Forward every message to JS as-is. The JS side picks out
            // the event types it cares about (logs, usage, status, …).
            let js = serde_wasm_bindgen::Serializer::new()
                .serialize_maps_as_objects(true);
            if let Ok(value) = json.serialize(&js) {
                cb_msg.invoke_secondary_event(&host_id_msg, &value);
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose — schedule a reconnect via window.__presenceWeb
        let host_id_close = self.host_id.clone();
        let connected_close = self.connected.clone();
        let cb_close = self.callbacks.clone();
        let url_close = url.to_string();
        let onclose = Closure::wrap(Box::new(move |_e: CloseEvent| {
            *connected_close.borrow_mut() = false;
            cb_close.invoke_secondary_state(&host_id_close, false);
            let _ = web_sys::window().map(|w| {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    &js_sys::Function::new_no_args(&format!(
                        "if (window.__presenceWeb) window.__presenceWeb.reconnect_secondary_host('{}','{}')",
                        host_id_close.replace('\'', "\\'"),
                        url_close.replace('\'', "\\'"),
                    )),
                    RECONNECT_DELAY_MS,
                );
            });
        }) as Box<dyn FnMut(CloseEvent)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // onerror
        let host_id_err = self.host_id.clone();
        let cb_err = self.callbacks.clone();
        let onerror = Closure::wrap(Box::new(move || {
            cb_err.invoke_error(&format!("secondary {} WebSocket error", host_id_err));
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
        *self.connected.borrow_mut() = false;
        self._onopen = None;
        self._onmessage = None;
        self._onclose = None;
        self._onerror = None;
    }
}

// Pull Serialize into scope for the onmessage closure.
use serde::Serialize;
