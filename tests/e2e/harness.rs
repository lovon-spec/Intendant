use serde_json::Value;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// A JSONL event emitted by intendant in --json mode.
#[derive(Debug, Clone)]
pub struct JsonEvent {
    pub event_type: String,
    pub data: Value,
}

/// Spawns an intendant binary, reads JSONL events from stdout, sends commands on stdin.
pub struct IntendantProcess {
    child: Child,
    stdout_reader: BufReader<ChildStdout>,
    stdin: ChildStdin,
    /// All events collected so far (for debugging on failure)
    pub events: Vec<JsonEvent>,
}

impl IntendantProcess {
    /// Start intendant with --json --direct and given args.
    pub fn spawn(task: &str, autonomy: &str, extra_args: &[&str]) -> Self {
        let binary = Self::binary_path();

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.arg("--json")
            .arg("--direct")
            .arg("--autonomy")
            .arg(autonomy)
            .arg(task);

        for arg in extra_args {
            cmd.arg(arg);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RUST_LOG", "warn");

        let mut child = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "Failed to spawn intendant binary at {}: {}",
                binary.display(),
                e
            )
        });

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");

        IntendantProcess {
            child,
            stdout_reader: BufReader::new(stdout),
            stdin,
            events: Vec::new(),
        }
    }

    /// Start intendant with --web, --json, --direct.
    pub fn spawn_web(task: &str, autonomy: &str, port: u16, extra_args: &[&str]) -> Self {
        let binary = Self::binary_path();

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.arg("--json")
            .arg("--direct")
            .arg("--web")
            .arg(port.to_string())
            .arg("--autonomy")
            .arg(autonomy)
            .arg(task);

        for arg in extra_args {
            cmd.arg(arg);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RUST_LOG", "warn");

        let mut child = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "Failed to spawn intendant binary at {}: {}",
                binary.display(),
                e
            )
        });

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");

        IntendantProcess {
            child,
            stdout_reader: BufReader::new(stdout),
            stdin,
            events: Vec::new(),
        }
    }

    /// Start intendant in TUI mode with --control-socket.
    pub fn spawn_tui(task: &str, autonomy: &str, extra_args: &[&str]) -> Self {
        let binary = Self::binary_path();

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.arg("--control-socket")
            .arg("--direct")
            .arg("--autonomy")
            .arg(autonomy)
            .arg(task);

        for arg in extra_args {
            cmd.arg(arg);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RUST_LOG", "warn");

        let mut child = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "Failed to spawn intendant binary at {}: {}",
                binary.display(),
                e
            )
        });

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");

        IntendantProcess {
            child,
            stdout_reader: BufReader::new(stdout),
            stdin,
            events: Vec::new(),
        }
    }

    /// Get the PID of the child process.
    pub fn pid(&self) -> u32 {
        self.child.id().expect("child PID")
    }

    /// Read next JSONL event from stdout, with timeout.
    pub async fn read_event(&mut self, timeout: Duration) -> Option<JsonEvent> {
        let mut line = String::new();
        match tokio::time::timeout(timeout, self.stdout_reader.read_line(&mut line)).await {
            Ok(Ok(0)) => None, // EOF
            Ok(Ok(_)) => {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(val) => {
                        let event = JsonEvent {
                            event_type: val
                                .get("type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            data: val.get("data").cloned().unwrap_or(Value::Null),
                        };
                        self.events.push(event.clone());
                        Some(event)
                    }
                    Err(_) => None, // non-JSON line (provider info, etc.)
                }
            }
            Ok(Err(_)) => None, // read error
            Err(_) => None,     // timeout
        }
    }

    /// Wait for a specific event type, collecting all events along the way.
    pub async fn wait_for(&mut self, event_type: &str, timeout: Duration) -> Option<JsonEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.read_event(remaining).await {
                Some(event) if event.event_type == event_type => return Some(event),
                Some(_) => continue,
                None => return None,
            }
        }
    }

    /// Send a JSON command on stdin (approve, deny, input, etc.).
    pub async fn send_command(&mut self, msg: &Value) {
        let line = format!("{}\n", serde_json::to_string(msg).unwrap());
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to stdin");
        self.stdin.flush().await.expect("flush stdin");
    }

    /// Send follow-up text on stdin (non-JSON line).
    pub async fn send_follow_up(&mut self, text: &str) {
        let line = format!("{}\n", text);
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to stdin");
        self.stdin.flush().await.expect("flush stdin");
    }

    /// GET /debug endpoint (web mode only).
    pub async fn debug_snapshot(&self, port: u16) -> Option<Value> {
        let url = format!("http://127.0.0.1:{}/debug", port);
        reqwest::get(&url)
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    }

    /// Wait for process exit with timeout.
    pub async fn wait(mut self, timeout: Duration) -> Option<ExitStatus> {
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => {
                let _ = self.child.kill().await;
                None
            }
        }
    }

    /// Kill the child process.
    pub async fn kill(mut self) {
        let _ = self.child.kill().await;
    }

    fn binary_path() -> PathBuf {
        // Look for release build first, then debug
        let release = PathBuf::from("target/release/intendant");
        if release.exists() {
            return release;
        }
        let debug = PathBuf::from("target/debug/intendant");
        if debug.exists() {
            return debug;
        }
        panic!("intendant binary not found — run `cargo build` or `cargo build --release` first");
    }
}

/// Connect to a control socket and exchange JSON-line messages.
pub struct ControlSocketClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl ControlSocketClient {
    /// Connect to the control socket for the given PID.
    pub async fn connect(pid: u32, timeout: Duration) -> Option<Self> {
        let path = format!("/tmp/intendant-{}.sock", pid);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::net::UnixStream::connect(&path).await {
                Ok(stream) => {
                    let (read_half, write_half) = stream.into_split();
                    return Some(ControlSocketClient {
                        reader: BufReader::new(read_half),
                        writer: write_half,
                    });
                }
                Err(_) => {
                    if tokio::time::Instant::now() >= deadline {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Send a JSON command.
    pub async fn send(&mut self, msg: &Value) {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(msg).unwrap();
        self.writer
            .write_all(format!("{}\n", line).as_bytes())
            .await
            .expect("write to socket");
    }

    /// Read next JSON event, with timeout.
    pub async fn recv(&mut self, timeout: Duration) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(timeout, self.reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => serde_json::from_str(line.trim()).ok(),
            _ => None,
        }
    }

    /// Wait for a specific event type from the control socket.
    pub async fn wait_for_event(&mut self, event_name: &str, timeout: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.recv(remaining).await {
                Some(val) if val.get("event").and_then(|v| v.as_str()) == Some(event_name) => {
                    return Some(val)
                }
                Some(_) => continue,
                None => return None,
            }
        }
    }
}

/// Connect WebSocket and exchange messages.
pub struct WsClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsClient {
    /// Connect to the WebSocket endpoint, retrying until timeout.
    pub async fn connect(port: u16, timeout: Duration) -> Option<Self> {
        let url = format!("ws://127.0.0.1:{}/ws", port);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((ws, _)) => return Some(WsClient { ws }),
                Err(_) => {
                    if tokio::time::Instant::now() >= deadline {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    /// Read next JSON message from WebSocket, with timeout.
    pub async fn recv(&mut self, timeout: Duration) -> Option<Value> {
        use futures_util::StreamExt;
        match tokio::time::timeout(timeout, self.ws.next()).await {
            Ok(Some(Ok(msg))) => {
                let text = msg.into_text().ok()?;
                serde_json::from_str(&text).ok()
            }
            _ => None,
        }
    }

    /// Send a JSON message on the WebSocket.
    pub async fn send(&mut self, msg: &Value) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let text = serde_json::to_string(msg).unwrap();
        let _ = self.ws.send(Message::Text(text.into())).await;
    }

    /// Send a tool_request and wait for the matching tool_response.
    pub async fn tool_request(
        &mut self,
        tool: &str,
        args: &Value,
        timeout: Duration,
    ) -> Option<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.send(&serde_json::json!({
            "t": "tool_request",
            "id": id,
            "tool": tool,
            "args": args,
        }))
        .await;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if let Some(val) = self.recv(remaining).await {
                if val.get("t").and_then(|v| v.as_str()) == Some("tool_response") {
                    if val.get("id").and_then(|v| v.as_str()) == Some(&id) {
                        return val
                            .get("result")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                }
            } else {
                return None;
            }
        }
    }

    /// Wait for a message with a specific `t` field value.
    pub async fn wait_for_type(&mut self, msg_type: &str, timeout: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.recv(remaining).await {
                Some(val) if val.get("t").and_then(|v| v.as_str()) == Some(msg_type) => {
                    return Some(val)
                }
                Some(_) => continue,
                None => return None,
            }
        }
    }

    /// Collect `term` frames for a duration, return decoded text.
    pub async fn collect_term_frames(&mut self, duration: Duration) -> Vec<String> {
        use base64::Engine;
        let mut frames = Vec::new();
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.recv(remaining).await {
                Some(val) if val.get("t").and_then(|v| v.as_str()) == Some("term") => {
                    if let Some(encoded) = val.get("d").and_then(|v| v.as_str()) {
                        if let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(encoded)
                        {
                            if let Ok(text) = String::from_utf8(bytes) {
                                frames.push(text);
                            }
                        }
                    }
                }
                Some(_) => continue,
                None => break,
            }
        }
        frames
    }
}

/// Voice helpers — shell out to espeak-ng/ffmpeg/paplay for audio.

/// Speak text via espeak-ng piped through ffmpeg to paplay on PulseAudio virtual sink.
pub fn say(text: &str, speed: u32) {
    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!(
            "espeak-ng -s {} --stdout '{}' | ffmpeg -y -i pipe:0 -ar 48000 -ac 1 -f s16le pipe:1 2>/dev/null | \
             PULSE_SINK=virtual_mic paplay --raw --rate=48000 --channels=1 --format=s16le",
            speed,
            text.replace('\'', "'\\''")
        ))
        .status()
        .expect("espeak-ng pipeline");
    assert!(status.success(), "espeak-ng pipeline failed");
}

/// Set up PulseAudio virtual microphone source.
pub fn setup_virtual_mic() {
    // Create a null sink that acts as a virtual mic
    let _ = std::process::Command::new("pactl")
        .args(["load-module", "module-null-sink", "sink_name=virtual_mic"])
        .output();
    // Set its monitor as the default source
    let _ = std::process::Command::new("pactl")
        .args(["set-default-source", "virtual_mic.monitor"])
        .output();
}

/// Clean up PulseAudio virtual mic.
pub fn cleanup_virtual_mic() {
    let _ = std::process::Command::new("bash")
        .arg("-c")
        .arg("pactl list short modules | grep virtual_mic | awk '{print $1}' | xargs -r pactl unload-module")
        .output();
}

/// Poll an async predicate until it returns true or timeout.
pub async fn poll_until<F, Fut>(predicate: F, timeout: Duration) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
