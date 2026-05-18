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
///
/// Subscription model: connections start silent. A connection must send
/// `Subscribe` before any ratatui frames (`{"t":"term",...}`) are emitted
/// to it; `Unsubscribe` stops emission without tearing the connection
/// down. The dashboard subscribes only when the Terminal tab is visible,
/// which keeps WebTui idle while the user is on any other tab.
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
    /// Start emitting terminal frames for this connection.  Emits an
    /// alternate-screen-enter sequence and an immediate full draw so the
    /// browser sees the current UI state, not a blank screen.
    Subscribe {
        id: String,
    },
    /// Stop emitting terminal frames for this connection.  The underlying
    /// ratatui `Terminal` is retained so a subsequent `Subscribe` can
    /// resume cleanly with the latest geometry.
    Unsubscribe {
        id: String,
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
    /// Whether this connection wants terminal frames. Set to `true` when
    /// the dashboard is showing the Terminal tab; `false` otherwise.
    /// No draws and no `{"t":"term",...}` emissions happen when this is
    /// `false` — the whole render path is gated on it.
    subscribed: bool,
}

impl WebConnection {
    fn new(cols: u16, rows: u16, direct_tx: mpsc::UnboundedSender<String>) -> io::Result<Self> {
        let writer = SharedWriter::new();
        let backend = CrosstermBackend::new(writer.clone());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;

        Ok(Self {
            terminal,
            writer,
            view: ViewState::default(),
            direct_tx,
            subscribed: false,
        })
    }

    /// Begin emitting term frames to this connection.  Sends the
    /// alternate-screen-enter sequence plus a fresh full draw so the
    /// browser sees the current UI state immediately; returns `Ok(())`
    /// without touching the terminal if this connection is already
    /// subscribed.
    fn subscribe(&mut self, app: &mut App) -> io::Result<()> {
        if self.subscribed {
            return Ok(());
        }
        self.subscribed = true;

        // Send alternate-screen enter sequence, then clear so the next
        // draw paints the whole viewport (ratatui otherwise diffs against
        // its previous buffer and may skip cells that actually need
        // re-painting on the browser).
        let mut init_writer = self.writer.clone();
        execute!(init_writer, EnterAlternateScreen)?;
        let _ = self.terminal.clear();
        let init_data = self.writer.take();
        if !init_data.is_empty() {
            let _ = self.direct_tx.send(encode_term(&init_data));
        }
        self.draw(app)
    }

    /// Stop emitting term frames to this connection.  The terminal is
    /// kept intact so a subsequent `subscribe` resumes cleanly.
    fn unsubscribe(&mut self) {
        self.subscribed = false;
        // Discard any buffered output — we already skipped sending it
        // during the draw gate, but a stray resize/clear could have left
        // bytes queued; dropping them keeps state clean for the next
        // subscribe.
        let _ = self.writer.take();
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.terminal.resize(Rect::new(0, 0, cols, rows));
        let _ = self.terminal.clear();
        if self.subscribed {
            let data = self.writer.take();
            if !data.is_empty() {
                let _ = self.direct_tx.send(encode_term(&data));
            }
        } else {
            // Discard: the next subscribe will redraw from scratch
            // against the new geometry.
            let _ = self.writer.take();
        }
    }

    fn draw(&mut self, app: &mut App) -> io::Result<()> {
        if !self.subscribed {
            return Ok(());
        }
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
            // Apply any pending verbosity override from control socket.
            // Apply it to every connection's view (even unsubscribed ones)
            // so re-subscribing picks up the latest setting immediately.
            if let Some(v) = app.pending_verbosity.take() {
                for conn in self.connections.values_mut() {
                    conn.view.verbosity = v;
                }
            }

            // Render only subscribed connections. When nobody is
            // subscribed, this loop is effectively idle — no draws, no
            // emitted frames, no CPU burn — and we wait for the next
            // AppEvent or WebTuiCommand. This is the payoff: every tab
            // other than Terminal leaves the browser quiet.
            if self.has_subscribers() {
                for conn in self.connections.values_mut() {
                    if conn.subscribed {
                        let _ = conn.draw(app);
                    }
                }
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
                            // Connections start silent. The browser is
                            // responsible for sending Subscribe when it
                            // wants frames (i.e. the Terminal tab is
                            // active).  No initial draw here; that would
                            // leak term frames to connections that never
                            // intend to look at the Terminal tab.
                            match WebConnection::new(cols, rows, direct_tx) {
                                Ok(conn) => {
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
                        Some(WebTuiCommand::Subscribe { id }) => {
                            if let Some(conn) = self.connections.get_mut(&id) {
                                if let Err(e) = conn.subscribe(app) {
                                    eprintln!("WebTui: subscribe failed for {}: {}", id, e);
                                }
                            }
                        }
                        Some(WebTuiCommand::Unsubscribe { id }) => {
                            if let Some(conn) = self.connections.get_mut(&id) {
                                conn.unsubscribe();
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

    /// Returns true when at least one connection is currently subscribed
    /// to terminal frames.  When false, the render loop is fully idle.
    fn has_subscribers(&self) -> bool {
        self.connections.values().any(|c| c.subscribed)
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

    // ------------------------------------------------------------------
    // Subscription-gated rendering tests
    //
    // These cover the invariant documented on `WebTuiCommand`:
    // connections start silent, only subscribed connections receive
    // `{"t":"term",...}` frames, and unsubscribe stops the stream while
    // leaving the underlying terminal intact for a later re-subscribe.
    // ------------------------------------------------------------------

    fn test_app() -> App {
        let autonomy = crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default());
        App::new(
            "openai".to_string(),
            "gpt-5".to_string(),
            autonomy,
            std::path::PathBuf::from("/tmp/test_webtui_session"),
        )
    }

    /// Drain an mpsc receiver non-blockingly. Returns every message
    /// currently queued.
    fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    fn count_term_frames(msgs: &[String]) -> usize {
        msgs.iter()
            .filter(|m| {
                serde_json::from_str::<serde_json::Value>(m)
                    .ok()
                    .and_then(|v| v.get("t").and_then(|t| t.as_str()).map(str::to_string))
                    .as_deref()
                    == Some("term")
            })
            .count()
    }

    #[test]
    fn new_connection_emits_nothing() {
        // A fresh WebConnection must not push anything to its direct_tx
        // until Subscribe lands. Previously `new()` sent an alternate-
        // screen-enter sequence, which is exactly the kind of per-open
        // chatter that adds up when nobody is watching the Terminal tab.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let _conn = WebConnection::new(80, 24, tx).expect("new");
        assert!(drain(&mut rx).is_empty(), "fresh connection must be silent");
    }

    #[test]
    fn unsubscribed_draw_is_a_noop() {
        // Even when `draw()` is called explicitly, the writer must not
        // produce a term frame while the connection is unsubscribed. This
        // is the subscription gate enforced at the draw site.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut conn = WebConnection::new(80, 24, tx).expect("new");
        let mut app = test_app();
        conn.draw(&mut app).expect("draw");
        assert_eq!(count_term_frames(&drain(&mut rx)), 0);
    }

    #[test]
    fn subscribe_then_unsubscribe_stops_frames() {
        // Subscribe must produce at least one term frame (the initial
        // full draw). After unsubscribe, subsequent draws must not emit
        // any frames. This is the gate the whole change exists for.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut conn = WebConnection::new(80, 24, tx).expect("new");
        let mut app = test_app();

        conn.subscribe(&mut app).expect("subscribe");
        let after_subscribe = drain(&mut rx);
        assert!(
            count_term_frames(&after_subscribe) >= 1,
            "subscribe should push at least one frame, got {:?}",
            after_subscribe
        );

        // A few more draws while subscribed — each should produce frames
        // (contents may be identical, but ratatui writes at least some
        // cursor-position sequence).
        for _ in 0..3 {
            conn.draw(&mut app).expect("draw");
        }
        assert!(count_term_frames(&drain(&mut rx)) >= 1);

        // Now unsubscribe and draw repeatedly — zero frames should
        // reach the wire.
        conn.unsubscribe();
        for _ in 0..5 {
            conn.draw(&mut app).expect("draw");
        }
        assert_eq!(
            count_term_frames(&drain(&mut rx)),
            0,
            "no frames should flow to an unsubscribed connection"
        );
    }

    #[test]
    fn resubscribe_after_unsubscribe_redraws() {
        // Re-subscribing has to restart the frame stream (otherwise the
        // dashboard would show a stale view after tab switching).
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut conn = WebConnection::new(80, 24, tx).expect("new");
        let mut app = test_app();

        conn.subscribe(&mut app).expect("initial subscribe");
        let _ = drain(&mut rx); // discard bootstrap frames

        conn.unsubscribe();
        assert_eq!(count_term_frames(&drain(&mut rx)), 0);

        conn.subscribe(&mut app).expect("resubscribe");
        assert!(
            count_term_frames(&drain(&mut rx)) >= 1,
            "re-subscribe must push a fresh draw"
        );
    }

    #[test]
    fn resize_while_unsubscribed_does_not_leak() {
        // A browser that's sitting on (say) the Stats tab can still emit
        // resize messages when the window reflows. Those must not
        // produce term frames while unsubscribed.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut conn = WebConnection::new(80, 24, tx).expect("new");
        conn.resize(100, 30);
        assert_eq!(count_term_frames(&drain(&mut rx)), 0);
    }

    #[test]
    fn has_subscribers_reflects_connection_state() {
        // The WebTui run loop skips its draw pass entirely when
        // `has_subscribers()` is false. This test pins that predicate so
        // the optimization can't regress silently.
        let (broadcast_tx, _) = broadcast::channel::<String>(4);
        let mut tui = WebTui::new(80, 24, broadcast_tx).expect("new");
        assert!(!tui.has_subscribers(), "empty WebTui has no subscribers");

        let (tx_a, _rx_a) = mpsc::unbounded_channel::<String>();
        let conn_a = WebConnection::new(80, 24, tx_a).expect("conn a");
        tui.connections.insert("a".into(), conn_a);
        assert!(
            !tui.has_subscribers(),
            "adding an unsubscribed connection doesn't make the loop active"
        );

        // Flip connection A to subscribed (bypassing the side-effect of
        // subscribe() so we don't need to drive an App here).
        tui.connections.get_mut("a").unwrap().subscribed = true;
        assert!(tui.has_subscribers());

        tui.connections.get_mut("a").unwrap().subscribed = false;
        assert!(!tui.has_subscribers());
    }
}
