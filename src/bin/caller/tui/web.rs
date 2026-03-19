//! Web-based TUI backend: renders ratatui to a buffer and streams ANSI
//! output over WebSocket via a broadcast channel.  Key events and resize
//! messages arrive from the browser and are injected as `AppEvent`s.

use super::app::{App, ViewState};
use crate::event::AppEvent;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use ratatui::prelude::*;
use ratatui::{TerminalOptions, Viewport};
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

// ---------------------------------------------------------------------------
// SharedWriter — captures ANSI output in a thread-safe buffer
// ---------------------------------------------------------------------------

/// A thread-safe writer that captures terminal output for later retrieval.
#[derive(Clone)]
pub struct SharedWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::with_capacity(16384))),
        }
    }

    /// Take and return accumulated bytes, leaving the buffer empty.
    pub fn take(&self) -> Vec<u8> {
        let mut buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *buf)
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WebTuiCommand — channel for per-connection events from the web gateway
// ---------------------------------------------------------------------------

/// Commands from the web gateway to the WebTui event loop.
/// Key/resize events are routed per-connection instead of through EventBus.
pub enum WebTuiCommand {
    AddConnection {
        id: String,
        direct_tx: mpsc::UnboundedSender<String>,
        cols: u16,
        rows: u16,
    },
    RemoveConnection {
        id: String,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    Key {
        id: String,
        key: crossterm::event::KeyEvent,
    },
}

// ---------------------------------------------------------------------------
// WebConnection — per-connection terminal + view state
// ---------------------------------------------------------------------------

struct WebConnection {
    terminal: Terminal<CrosstermBackend<SharedWriter>>,
    writer: SharedWriter,
    view: ViewState,
    direct_tx: mpsc::UnboundedSender<String>,
}

impl WebConnection {
    fn new(
        cols: u16,
        rows: u16,
        direct_tx: mpsc::UnboundedSender<String>,
    ) -> io::Result<Self> {
        let writer = SharedWriter::new();
        let backend = CrosstermBackend::new(writer.clone());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;

        // Send alternate-screen enter sequence to this connection
        let mut init_writer = writer.clone();
        execute!(init_writer, EnterAlternateScreen)?;
        let init_data = writer.take();
        if !init_data.is_empty() {
            let b64 = encode_term(&init_data);
            let _ = direct_tx.send(b64);
        }

        Ok(Self {
            terminal,
            writer,
            view: ViewState::default(),
            direct_tx,
        })
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.terminal.resize(Rect::new(0, 0, cols, rows));
        let _ = self.terminal.clear();
        let data = self.writer.take();
        if !data.is_empty() {
            let _ = self.direct_tx.send(encode_term(&data));
        }
    }

    fn draw(&mut self, app: &mut App) -> io::Result<()> {
        let view = &self.view;
        self.terminal.draw(|f| {
            super::render_frame(f, app, view);
        })?;
        let data = self.writer.take();
        if !data.is_empty() {
            let _ = self.direct_tx.send(encode_term(&data));
        }
        Ok(())
    }
}

fn encode_term(data: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    json!({"t": "term", "d": b64}).to_string()
}

// ---------------------------------------------------------------------------
// WebTui — manages N per-connection terminals with independent view state
// ---------------------------------------------------------------------------

/// Web TUI: renders the ratatui interface per-connection, each with its own
/// terminal buffer and view state (scroll, verbosity, expanded turns).
pub struct WebTui {
    connections: HashMap<String, WebConnection>,
    /// Broadcast channel for non-term events (OutboundEvents).
    broadcast_tx: broadcast::Sender<String>,
}

impl WebTui {
    /// Create a new web TUI (no initial connections — they register via WebTuiCommand).
    pub fn new(
        _cols: u16,
        _rows: u16,
        broadcast_tx: broadcast::Sender<String>,
    ) -> io::Result<Self> {
        Ok(Self {
            connections: HashMap::new(),
            broadcast_tx,
        })
    }

    fn broadcast_term(tx: &broadcast::Sender<String>, data: &[u8]) {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let msg = json!({"t": "term", "d": b64}).to_string();
        let _ = tx.send(msg);
    }

    /// Run the main web TUI event loop until quit.
    pub async fn run(
        &mut self,
        app: &mut App,
        mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
        mut cmd_rx: mpsc::UnboundedReceiver<WebTuiCommand>,
        bus: crate::event::EventBus,
    ) -> io::Result<()> {
        loop {
            // Apply any pending verbosity override from control socket
            if let Some(v) = app.pending_verbosity.take() {
                for conn in self.connections.values_mut() {
                    conn.view.verbosity = v;
                }
            }

            // Render each connection independently
            for conn in self.connections.values_mut() {
                let _ = conn.draw(app);
            }

            // Wait for next event (AppEvent or WebTuiCommand)
            tokio::select! {
                result = event_rx.recv() => {
                    match result {
                        Ok(ev @ AppEvent::Key(_)) | Ok(ev @ AppEvent::Resize(_, _)) => {
                            // Key/Resize from EventBus (e.g. crossterm native terminal)
                            // are ignored in web mode — per-connection keys come via
                            // WebTuiCommand. But still forward to App for shared state.
                            for d in app.handle_event(ev) { bus.send(d); }
                        }
                        Ok(ev) => {
                            for d in app.handle_event(ev) { bus.send(d); }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                    if app.should_quit {
                        break;
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(WebTuiCommand::AddConnection { id, direct_tx, cols, rows }) => {
                            match WebConnection::new(cols, rows, direct_tx) {
                                Ok(mut conn) => {
                                    // Immediately render so the browser doesn't see a blank screen
                                    let _ = conn.draw(app);
                                    self.connections.insert(id, conn);
                                }
                                Err(e) => eprintln!("WebTui: failed to create connection: {}", e),
                            }
                        }
                        Some(WebTuiCommand::RemoveConnection { id }) => {
                            self.connections.remove(&id);
                        }
                        Some(WebTuiCommand::Resize { id, cols, rows }) => {
                            if let Some(conn) = self.connections.get_mut(&id) {
                                conn.resize(cols, rows);
                                let _ = conn.draw(app);
                            }
                        }
                        Some(WebTuiCommand::Key { id, key }) => {
                            if let Some(conn) = self.connections.get_mut(&id) {
                                // Try view-only key handling first
                                if !conn.view.handle_key(key, app) {
                                    // Fall through to shared state
                                    app.handle_key(key);
                                }
                            }
                        }
                        None => break,
                    }
                    if app.should_quit {
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Key event parsing — converts JSON from xterm.js into crossterm KeyEvent
// ---------------------------------------------------------------------------

/// Parse a JSON key event from the web client into a crossterm KeyEvent.
///
/// Expected format:
/// ```json
/// {"t":"key","key":"Enter","ctrl":false,"alt":false,"shift":false}
/// ```
pub fn parse_web_key(json: &serde_json::Value) -> Option<crossterm::event::KeyEvent> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let key = json["key"].as_str()?;
    let ctrl = json["ctrl"].as_bool().unwrap_or(false);
    let alt = json["alt"].as_bool().unwrap_or(false);
    let shift = json["shift"].as_bool().unwrap_or(false);

    let code = match key {
        "Enter" => KeyCode::Enter,
        "Backspace" => KeyCode::Backspace,
        "Tab" => KeyCode::Tab,
        "Escape" => KeyCode::Esc,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Delete" => KeyCode::Delete,
        " " => KeyCode::Char(' '),
        s if s.len() == 1 => {
            let ch = s.chars().next().unwrap();
            // ctrl+letter comes as the letter itself with ctrl=true
            KeyCode::Char(if ctrl { ch.to_ascii_lowercase() } else { ch })
        }
        s if s.starts_with('F') && s.len() > 1 => {
            let n: u8 = s[1..].parse().ok()?;
            KeyCode::F(n)
        }
        _ => return None,
    };

    let mut modifiers = KeyModifiers::empty();
    if ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if alt {
        modifiers |= KeyModifiers::ALT;
    }
    if shift {
        modifiers |= KeyModifiers::SHIFT;
    }

    Some(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn shared_writer_write_and_take() {
        let mut w = SharedWriter::new();
        w.write_all(b"hello").unwrap();
        w.write_all(b" world").unwrap();
        let data = w.take();
        assert_eq!(data, b"hello world");
        // Second take returns empty
        assert!(w.take().is_empty());
    }

    #[test]
    fn shared_writer_clone_shares_buffer() {
        let mut w1 = SharedWriter::new();
        let w2 = w1.clone();
        w1.write_all(b"abc").unwrap();
        let data = w2.take();
        assert_eq!(data, b"abc");
    }

    #[test]
    fn parse_web_key_enter() {
        let json = json!({"t":"key","key":"Enter","ctrl":false,"alt":false,"shift":false});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::Enter);
        assert_eq!(ev.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn parse_web_key_ctrl_c() {
        let json = json!({"t":"key","key":"c","ctrl":true,"alt":false,"shift":false});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::Char('c'));
        assert!(ev.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn parse_web_key_arrow() {
        let json = json!({"t":"key","key":"ArrowUp"});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::Up);
    }

    #[test]
    fn parse_web_key_char() {
        let json = json!({"t":"key","key":"a"});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::Char('a'));
    }

    #[test]
    fn parse_web_key_f_key() {
        let json = json!({"t":"key","key":"F5"});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::F(5));
    }

    #[test]
    fn parse_web_key_space() {
        let json = json!({"t":"key","key":" "});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::Char(' '));
    }

    #[test]
    fn parse_web_key_unknown() {
        let json = json!({"t":"key","key":"Meta"});
        assert!(parse_web_key(&json).is_none());
    }

    #[test]
    fn parse_web_key_escape() {
        let json = json!({"t":"key","key":"Escape"});
        let ev = parse_web_key(&json).unwrap();
        assert_eq!(ev.code, KeyCode::Esc);
    }

    #[test]
    fn parse_web_key_combined_modifiers() {
        let json = json!({"t":"key","key":"a","ctrl":true,"alt":true,"shift":true});
        let ev = parse_web_key(&json).unwrap();
        assert!(ev.modifiers.contains(KeyModifiers::CONTROL));
        assert!(ev.modifiers.contains(KeyModifiers::ALT));
        assert!(ev.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn parse_web_key_missing_key_field() {
        let json = json!({"t":"key"});
        assert!(parse_web_key(&json).is_none());
    }

    #[test]
    fn broadcast_term_format() {
        let (tx, mut rx) = broadcast::channel::<String>(4);
        WebTui::broadcast_term(&tx, b"\x1b[2J");
        let msg = rx.try_recv().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["t"], "term");
        // Verify base64 decodes back
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(parsed["d"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"\x1b[2J");
    }
}
