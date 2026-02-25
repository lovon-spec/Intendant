use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use std::sync::Arc;

/// Entry tracking a single command's process state across turns.
#[derive(Debug, Clone)]
struct ProcessEntry {
    pid: i32,
    status: String, // "r", "c", "f", "s", "w"
    exit_code: i32,
}

/// Caller-side process state, accumulated across turns within a task.
/// The runtime connects to query cross-turn state (PIDs from prior turns).
pub struct ProcessStateStore {
    entries: Arc<RwLock<HashMap<u64, ProcessEntry>>>,
    socket_path: PathBuf,
}

impl ProcessStateStore {
    /// Create a new store with a unique socket path based on the caller PID.
    pub fn new() -> Self {
        let socket_path =
            PathBuf::from(format!("/tmp/intendant-state-{}.sock", std::process::id()));
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            socket_path,
        }
    }

    /// Create a store with a custom socket path (for testing).
    #[cfg(test)]
    fn with_path(socket_path: PathBuf) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            socket_path,
        }
    }

    /// Path to the Unix socket.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Ingest JSON status lines from runtime stdout, extracting nonce/pid/status/exit_code.
    pub async fn ingest_stdout(&self, stdout: &str) {
        let mut entries = self.entries.write().await;
        for line in stdout.lines() {
            let trimmed = line.trim();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if parsed.get("type").and_then(|t| t.as_str()) == Some("status") {
                    if let (Some(nonce), Some(status)) = (
                        parsed.get("nonce").and_then(|n| n.as_u64()),
                        parsed.get("status").and_then(|s| s.as_str()),
                    ) {
                        let pid = parsed
                            .get("pid")
                            .and_then(|p| p.as_i64())
                            .unwrap_or(0) as i32;
                        let exit_code = parsed
                            .get("exit_code")
                            .and_then(|e| e.as_i64())
                            .unwrap_or(0) as i32;
                        entries.insert(
                            nonce,
                            ProcessEntry {
                                pid,
                                status: status.to_string(),
                                exit_code,
                            },
                        );
                    }
                }
            }
        }
    }

    /// Start the Unix socket server in the background. Returns a join handle.
    pub fn start_server(&self) -> tokio::task::JoinHandle<()> {
        let entries = self.entries.clone();
        let socket_path = self.socket_path.clone();

        // Remove stale socket file
        let _ = std::fs::remove_file(&socket_path);

        tokio::spawn(async move {
            let listener = match UnixListener::bind(&socket_path) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("Failed to bind state socket at {:?}: {}", socket_path, e);
                    return;
                }
            };

            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let entries = entries.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut line = String::new();

                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break, // EOF
                            Ok(_) => {}
                            Err(_) => break,
                        }

                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        let response =
                            if let Ok(req) = serde_json::from_str::<serde_json::Value>(trimmed) {
                                if req.get("query").and_then(|q| q.as_str()) == Some("get_pid") {
                                    if let Some(nonce) =
                                        req.get("nonce").and_then(|n| n.as_u64())
                                    {
                                        let map = entries.read().await;
                                        if let Some(entry) = map.get(&nonce) {
                                            serde_json::json!({"pid": entry.pid}).to_string()
                                        } else {
                                            serde_json::json!({"error": "unknown nonce"})
                                                .to_string()
                                        }
                                    } else {
                                        serde_json::json!({"error": "missing nonce"}).to_string()
                                    }
                                } else {
                                    serde_json::json!({"error": "unknown query"}).to_string()
                                }
                            } else {
                                serde_json::json!({"error": "invalid json"}).to_string()
                            };

                        if writer
                            .write_all(format!("{}\n", response).as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        })
    }

    /// Clean up the socket file.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for ProcessStateStore {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ingest_status_lines() {
        let store = ProcessStateStore::new();
        let stdout = r#"{"type":"status","nonce":1,"status":"r","pid":1234,"exit_code":0}
{"type":"status","nonce":1,"status":"c","pid":1234,"exit_code":0}
{"type":"status","nonce":2,"status":"f","pid":5678,"exit_code":1}
some non-json line
"#;
        store.ingest_stdout(stdout).await;
        let entries = store.entries.read().await;
        // Nonce 1 should have latest status (completed)
        let e1 = entries.get(&1).unwrap();
        assert_eq!(e1.pid, 1234);
        assert_eq!(e1.status, "c");
        assert_eq!(e1.exit_code, 0);
        // Nonce 2 should be failed
        let e2 = entries.get(&2).unwrap();
        assert_eq!(e2.pid, 5678);
        assert_eq!(e2.status, "f");
        assert_eq!(e2.exit_code, 1);
    }

    #[tokio::test]
    async fn ingest_ignores_non_status() {
        let store = ProcessStateStore::new();
        let stdout = r#"{"type":"result","nonce":1,"data":"hello"}
plain text
"#;
        store.ingest_stdout(stdout).await;
        let entries = store.entries.read().await;
        assert!(entries.is_empty());
    }

    fn unique_socket_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!(
            "/tmp/intendant-test-{}-{}.sock",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn socket_server_responds_to_get_pid() {
        let store = ProcessStateStore::with_path(unique_socket_path("get_pid"));
        // Pre-populate
        {
            let mut entries = store.entries.write().await;
            entries.insert(
                42,
                ProcessEntry {
                    pid: 9999,
                    status: "c".to_string(),
                    exit_code: 0,
                },
            );
        }

        let _handle = store.start_server();
        // Give server time to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect and query
        let stream = tokio::net::UnixStream::connect(store.socket_path()).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Query known nonce
        writer
            .write_all(b"{\"query\":\"get_pid\",\"nonce\":42}\n")
            .await
            .unwrap();
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(parsed["pid"], 9999);

        // Query unknown nonce
        writer
            .write_all(b"{\"query\":\"get_pid\",\"nonce\":999}\n")
            .await
            .unwrap();
        response.clear();
        reader.read_line(&mut response).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
        assert!(parsed.get("error").is_some());

        store.cleanup();
    }

    #[tokio::test]
    async fn socket_path_contains_pid() {
        let store = ProcessStateStore::new();
        let path = store.socket_path().to_string_lossy().to_string();
        assert!(path.contains(&std::process::id().to_string()));
    }

    #[tokio::test]
    async fn cleanup_removes_socket() {
        let store = ProcessStateStore::with_path(unique_socket_path("cleanup"));
        let _handle = store.start_server();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(store.socket_path().exists());
        store.cleanup();
        assert!(!store.socket_path().exists());
    }
}
