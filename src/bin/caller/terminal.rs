//! Standalone shell sessions for the web dashboard's Terminal tab.
//!
//! The existing Terminal tab shows the intendant TUI over xterm.js; this
//! module adds a parallel path for real shell PTYs so users can run ad-hoc
//! commands on the daemon host without leaving the dashboard.
//!
//! Architecture:
//!
//! - A global [`TerminalRegistry`] (held by the web gateway) maps session
//!   keys to live [`PtySession`]s. Sessions survive WebSocket reconnects —
//!   when a client drops and reopens the page, it reattaches to the same
//!   session key and replays the scrollback ring.
//!
//! - Each [`PtySession`] owns a master PTY, a writer into the shell's
//!   stdin, a reader task that copies stdout to every attached listener,
//!   and a small ring buffer for scrollback replay.
//!
//! - Session keys are `(HostId, TerminalId)`. `HostId` is always `"local"`
//!   for now but is threaded through everywhere so multi-host phase 1 can
//!   add sibling daemons without a refactor.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex as StdMutex};

use portable_pty::{native_pty_system, CommandBuilder as PtyCommandBuilder, MasterPty, PtySize};
use tokio::sync::{mpsc, RwLock};

/// Max scrollback retained per session, in bytes. 32 KB is enough to
/// replay a few screens of recent output on reconnect without holding a
/// whole terminal history in memory.
const SCROLLBACK_LIMIT: usize = 32 * 1024;

/// Device Status Report (cursor position) query / reply.
///
/// Windows ConPTY emits `ESC[6n` when a console app starts and blocks until
/// the terminal replies before processing stdin, so a shell would hang at
/// startup if nobody answers. In production the browser's xterm.js answers,
/// but we also answer server-side: the reply is consumed by conhost (the
/// component that issued the query) rather than delivered to the shell as
/// input, so it's safe even alongside the client's reply, and it keeps the
/// shell usable before any client has attached. On Unix the query doesn't fire
/// at startup, so the scan is a no-op.
#[cfg(windows)]
const DSR_CPR_QUERY: &[u8] = b"\x1b[6n";
#[cfg(windows)]
const DSR_CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// Composite session identifier. `host_id` is always `"local"` today but
/// keys the map so that multi-host phase 1 can add sibling daemons
/// without retrofitting the single-host assumption.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalKey {
    pub host_id: String,
    pub terminal_id: String,
}

impl TerminalKey {
    pub fn local(terminal_id: impl Into<String>) -> Self {
        Self {
            host_id: "local".to_string(),
            terminal_id: terminal_id.into(),
        }
    }
}

/// Event broadcast to every listener attached to a session. Encoded as
/// base64 on the wire to survive JSON transport.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    Exited { status: i32 },
}

/// Fixed-capacity byte ring used for reconnect scrollback replay.
struct Scrollback {
    buf: Vec<u8>,
    capacity: usize,
}

impl Scrollback {
    fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity.min(4096)),
            capacity,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.capacity {
            let drop = self.buf.len() - self.capacity;
            self.buf.drain(..drop);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }
}

/// A single live PTY-backed shell session. Internally shared via `Arc` so
/// the reader task and any number of attached listeners can hold a
/// reference without lifetime gymnastics.
pub struct PtySession {
    master: StdMutex<Box<dyn MasterPty + Send>>,
    writer: StdMutex<Box<dyn Write + Send>>,
    listeners: StdMutex<Vec<mpsc::UnboundedSender<TerminalEvent>>>,
    scrollback: StdMutex<Scrollback>,
    alive: StdMutex<bool>,
}

impl PtySession {
    /// Spawn a new shell under a fresh PTY. The shell defaults to
    /// `$SHELL`, falling back to `/bin/bash`.
    fn spawn(cols: u16, rows: u16, cwd: Option<std::path::PathBuf>) -> Result<Arc<Self>, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;

        // Platform shell: `$SHELL -l` (login env) on Unix — unchanged;
        // `powershell.exe -NoLogo` on Windows with a `cmd.exe` fallback. The
        // builder is consumed by `spawn_command`, so build a fresh one per
        // spawn attempt.
        let (shell, shell_args) = crate::platform::interactive_pty_shell();
        let build_cmd = |program: &str, args: &[String]| {
            let mut cmd = PtyCommandBuilder::new(program);
            cmd.args(args);
            if let Some(ref dir) = cwd {
                cmd.cwd(dir);
            }
            // Seed TERM so xterm.js gets colors and cursor sequences.
            cmd.env("TERM", "xterm-256color");
            cmd
        };
        let child = match pair.slave.spawn_command(build_cmd(&shell, &shell_args)) {
            Ok(child) => child,
            Err(primary_err) => match crate::platform::interactive_pty_shell_fallback() {
                Some((fb_shell, fb_args)) => pair
                    .slave
                    .spawn_command(build_cmd(&fb_shell, &fb_args))
                    .map_err(|fb_err| {
                        format!("spawn {shell} ({primary_err}) and fallback {fb_shell} ({fb_err})")
                    })?,
                None => return Err(format!("spawn {shell}: {primary_err}")),
            },
        };

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take writer: {e}"))?;

        let session = Arc::new(Self {
            master: StdMutex::new(pair.master),
            writer: StdMutex::new(writer),
            listeners: StdMutex::new(Vec::new()),
            scrollback: StdMutex::new(Scrollback::new(SCROLLBACK_LIMIT)),
            alive: StdMutex::new(true),
        });

        // Reader: dedicated OS thread (portable_pty's reader is blocking).
        // Copies bytes into scrollback and fans out to listeners.
        let session_clone = session.clone();
        std::thread::spawn(move || {
            Self::reader_loop(session_clone, reader, child);
        });

        Ok(session)
    }

    fn reader_loop(
        session: Arc<Self>,
        mut reader: Box<dyn Read + Send>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    // Answer ConPTY's startup cursor-position query so the shell
                    // doesn't block waiting for it (Windows only; no-op on Unix
                    // where the slice is never present).
                    #[cfg(windows)]
                    if chunk.windows(DSR_CPR_QUERY.len()).any(|w| w == DSR_CPR_QUERY) {
                        if let Ok(mut w) = session.writer.lock() {
                            let _ = w.write_all(DSR_CPR_REPLY);
                            let _ = w.flush();
                        }
                    }
                    if let Ok(mut sb) = session.scrollback.lock() {
                        sb.push(&chunk);
                    }
                    session.broadcast(TerminalEvent::Output(chunk));
                }
                Err(_) => break,
            }
        }

        // Shell exited. Capture exit status if available and notify
        // listeners so the UI can mark the session as closed.
        let status = match child.wait() {
            Ok(s) => s.exit_code() as i32,
            Err(_) => -1,
        };
        if let Ok(mut alive) = session.alive.lock() {
            *alive = false;
        }
        session.broadcast(TerminalEvent::Exited { status });
    }

    fn broadcast(&self, event: TerminalEvent) {
        if let Ok(mut listeners) = self.listeners.lock() {
            // Prune any senders whose receivers have been dropped.
            listeners.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }

    /// Write bytes to the PTY stdin. Silently drops if the writer has
    /// been closed (shell already exited).
    pub fn write_input(&self, data: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Attach a new listener. Immediately replays the scrollback buffer
    /// to the listener before any live bytes arrive, so a reconnecting
    /// client sees what it missed.
    pub fn attach(&self, tx: mpsc::UnboundedSender<TerminalEvent>) {
        // Replay scrollback first.
        let snapshot = self
            .scrollback
            .lock()
            .map(|sb| sb.snapshot())
            .unwrap_or_default();
        if !snapshot.is_empty() {
            let _ = tx.send(TerminalEvent::Output(snapshot));
        }
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.push(tx);
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.lock().map(|g| *g).unwrap_or(false)
    }
}

/// Process-wide registry of live shell sessions, keyed by
/// `(host_id, terminal_id)`. Held by the web gateway inside an `Arc` so
/// every WS connection can reach the same pool.
pub struct TerminalRegistry {
    sessions: RwLock<HashMap<TerminalKey, Arc<PtySession>>>,
    project_root: std::path::PathBuf,
}

impl TerminalRegistry {
    pub fn new(project_root: std::path::PathBuf) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            project_root,
        }
    }

    /// Returns the session for `key`, spawning a new shell if it doesn't
    /// exist yet. Dead sessions (child has exited) are replaced on the
    /// next open so the user can type `exit` and get a fresh shell.
    pub async fn open_or_attach(
        &self,
        key: TerminalKey,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<PtySession>, String> {
        {
            let guard = self.sessions.read().await;
            if let Some(existing) = guard.get(&key) {
                if existing.is_alive() {
                    return Ok(existing.clone());
                }
            }
        }

        let mut guard = self.sessions.write().await;
        // Re-check after acquiring the write lock in case another task
        // spawned the session concurrently.
        if let Some(existing) = guard.get(&key) {
            if existing.is_alive() {
                return Ok(existing.clone());
            }
        }

        let session = PtySession::spawn(cols, rows, Some(self.project_root.clone()))?;
        guard.insert(key, session.clone());
        Ok(session)
    }

    pub async fn get(&self, key: &TerminalKey) -> Option<Arc<PtySession>> {
        self.sessions.read().await.get(key).cloned()
    }

    pub async fn close(&self, key: &TerminalKey) {
        if let Some(session) = self.sessions.write().await.remove(key) {
            // Writing EOF (Ctrl-D) to the shell's stdin tells it to exit
            // cleanly; if it ignores, the session is simply dropped and
            // the reader thread hits read error → broadcasts Exited.
            session.write_input(&[0x04]);
        }
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn open_attach_write_and_receive_output() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let key = TerminalKey::local("test-0");
        let session = registry.open_or_attach(key.clone(), 80, 24).await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        session.attach(tx);

        // A terminal client sends CR (the Enter key), not LF — required for
        // ConPTY to submit the line on Windows; harmless on Unix.
        session.write_input(b"echo hello_from_pty\r");

        // Drain events until we see the expected echo, with a bounded wait.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw_echo = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Some(TerminalEvent::Output(bytes))) => {
                    if String::from_utf8_lossy(&bytes).contains("hello_from_pty") {
                        saw_echo = true;
                        break;
                    }
                }
                Ok(Some(TerminalEvent::Exited { .. })) => break,
                Ok(None) => break,
                Err(_) => {}
            }
        }
        assert!(saw_echo, "did not see echoed output from PTY");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_replays_scrollback() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let key = TerminalKey::local("test-1");
        let session = registry.open_or_attach(key, 80, 24).await.unwrap();

        // Drive a command through the first listener, then detach.
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        session.attach(tx1);
        // CR (Enter), not LF — see open_attach_write_and_receive_output.
        session.write_input(b"echo scroll_token_abc\r");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv()).await {
                Ok(Some(TerminalEvent::Output(bytes))) => {
                    if String::from_utf8_lossy(&bytes).contains("scroll_token_abc") {
                        break;
                    }
                }
                _ => {}
            }
        }
        drop(rx1);

        // Reattach with a fresh listener and expect the scrollback replay
        // to contain the token — no additional commands driven.
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        session.attach(tx2);
        match tokio::time::timeout(std::time::Duration::from_millis(500), rx2.recv()).await {
            Ok(Some(TerminalEvent::Output(bytes))) => {
                assert!(
                    String::from_utf8_lossy(&bytes).contains("scroll_token_abc"),
                    "replayed scrollback missing token"
                );
            }
            other => panic!("expected replay event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_or_attach_reuses_live_session() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let key = TerminalKey::local("test-2");
        let a = registry.open_or_attach(key.clone(), 80, 24).await.unwrap();
        let b = registry.open_or_attach(key, 80, 24).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "expected same Arc on re-open");
        assert_eq!(registry.len().await, 1);
    }
}
