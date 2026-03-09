//! Web-based TUI backend: renders ratatui to a buffer and streams ANSI
//! output over WebSocket via a broadcast channel.  Key events and resize
//! messages arrive from the browser and are injected as `AppEvent`s.

use super::app::App;
use crate::event::AppEvent;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use ratatui::prelude::*;
use ratatui::{TerminalOptions, Viewport};
use serde_json::json;
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
// WebTui — ratatui terminal that broadcasts ANSI to WebSocket clients
// ---------------------------------------------------------------------------

/// Web TUI: renders the ratatui interface into a buffer and broadcasts
/// ANSI escape sequences over a broadcast channel for xterm.js clients.
pub struct WebTui {
    terminal: Terminal<CrosstermBackend<SharedWriter>>,
    writer: SharedWriter,
    broadcast_tx: broadcast::Sender<String>,
}

impl WebTui {
    /// Create a new web TUI with the given initial terminal dimensions.
    pub fn new(
        cols: u16,
        rows: u16,
        broadcast_tx: broadcast::Sender<String>,
    ) -> io::Result<Self> {
        let writer = SharedWriter::new();
        let backend = CrosstermBackend::new(writer.clone());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;

        // Write the initial alternate-screen enter sequence so xterm.js
        // switches to the alternate buffer.
        let mut init_writer = writer.clone();
        execute!(init_writer, EnterAlternateScreen)?;
        let init_data = writer.take();
        if !init_data.is_empty() {
            Self::broadcast_term(&broadcast_tx, &init_data);
        }

        Ok(Self {
            terminal,
            writer,
            broadcast_tx,
        })
    }

    /// Render one frame and broadcast the ANSI diff to connected clients.
    pub fn draw(&mut self, app: &mut App) -> io::Result<()> {
        self.terminal.draw(|f| {
            super::render_frame(f, app);
        })?;

        // Capture the ANSI output written during draw and broadcast it
        let data = self.writer.take();
        if !data.is_empty() {
            Self::broadcast_term(&self.broadcast_tx, &data);
        }

        Ok(())
    }

    /// Resize the virtual terminal and force a full redraw.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self
            .terminal
            .resize(Rect::new(0, 0, cols, rows));
        // Force a full redraw by clearing the internal diff buffer
        let _ = self.terminal.clear();
        let data = self.writer.take();
        if !data.is_empty() {
            Self::broadcast_term(&self.broadcast_tx, &data);
        }
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
        mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    ) -> io::Result<()> {
        loop {
            self.draw(app)?;

            if let Some(event) = event_rx.recv().await {
                // Handle resize events from the browser
                if let AppEvent::Resize(w, h) = &event {
                    if *w > 0 && *h > 0 {
                        self.resize(*w, *h);
                    }
                }
                app.handle_event(event);
                if app.should_quit {
                    break;
                }
            } else {
                break;
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
