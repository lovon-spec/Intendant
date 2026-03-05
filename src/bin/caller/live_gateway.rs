use crate::tui::event::{AppEvent, ControlMsg, EventBus};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_PORT: u16 = 8765;

const LIVE_HTML: &str = include_str!("../../../static/live.html");

/// Configuration sent to the live frontend via `/config`.
#[derive(Clone, Debug, Serialize)]
pub struct LiveGatewayConfig {
    pub provider: String,
    pub model: String,
    pub input_sample_rate: u32,
    pub output_sample_rate: u32,
}

impl Default for LiveGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
        }
    }
}

/// Spawn the live gateway HTTP/WebSocket server.
///
/// - `GET /config` returns a JSON `LiveGatewayConfig`.
/// - `GET /` (and any other path) returns `live.html`.
/// - WebSocket connections are bridged to the EventBus (inbound control
///   messages) and broadcast channel (outbound events), mirroring the
///   Unix control socket in `control.rs`.
pub fn spawn_live_gateway(
    port: u16,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: LiveGatewayConfig,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());

    tokio::spawn(async move {
        let addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Live gateway bind failed on {}: {}", addr, e);
                return;
            }
        };

        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();

            tokio::spawn(async move {
                // Peek at the first bytes to detect WebSocket upgrade.
                // peek() does not consume the data, so tokio_tungstenite
                // can still read the full handshake.
                let mut buf = [0u8; 2048];
                let mut stream = stream;
                let n = match stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let header_text = String::from_utf8_lossy(&buf[..n]);
                let is_websocket = header_text
                    .lines()
                    .any(|l| l.to_lowercase().contains("upgrade: websocket"));

                if is_websocket {
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (mut ws_tx, mut ws_rx) = ws_stream.split();
                    let mut outbound_rx = broadcast_tx.subscribe();

                    // Inbound: WebSocket → EventBus
                    let bus_inbound = bus.clone();
                    let inbound = tokio::spawn(async move {
                        while let Some(Ok(msg)) = ws_rx.next().await {
                            if let Message::Text(text) = msg {
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    if let Ok(ctrl) =
                                        serde_json::from_str::<ControlMsg>(trimmed)
                                    {
                                        bus_inbound
                                            .send(AppEvent::ControlCommand(ctrl));
                                    }
                                }
                            }
                        }
                    });

                    // Outbound: broadcast → WebSocket
                    let outbound = tokio::spawn(async move {
                        loop {
                            match outbound_rx.recv().await {
                                Ok(line) => {
                                    if ws_tx
                                        .send(Message::Text(line.into()))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            }
                        }
                    });

                    let _ = tokio::join!(inbound, outbound);
                } else {
                    // Plain HTTP: consume the peeked request bytes, then send response.
                    let mut discard = vec![0u8; n];
                    use tokio::io::AsyncReadExt;
                    let _ = stream.read_exact(&mut discard).await;

                    // Route by request path
                    let is_config = header_text
                        .lines()
                        .next()
                        .map(|l| l.contains("/config"))
                        .unwrap_or(false);

                    let (content_type, body) = if is_config {
                        ("application/json", config_json.as_str())
                    } else {
                        ("text/html; charset=utf-8", LIVE_HTML)
                    };

                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: {}\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {}",
                        content_type,
                        body.len(),
                        body
                    );
                    let _ = stream.try_write(response.as_bytes());
                    // Give the client time to receive before dropping.
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            });
        }
    })
}

/// Build a `LiveGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
pub fn build_config(live_provider: Option<&str>, live_model: Option<&str>) -> LiveGatewayConfig {
    // If an explicit provider is given, use it directly.
    if let Some(provider) = live_provider {
        let model = live_model.unwrap_or_else(|| match provider {
            "openai" => "gpt-4o-realtime-preview",
            _ => "gemini-2.5-flash-native-audio-preview-12-2025",
        });
        let (input_rate, output_rate) = if provider == "openai" {
            (24000, 24000)
        } else {
            (16000, 24000)
        };
        return LiveGatewayConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            input_sample_rate: input_rate,
            output_sample_rate: output_rate,
        };
    }

    // If an explicit live model is given, detect provider from the model name.
    if let Some(model) = live_model {
        if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
            return LiveGatewayConfig {
                provider: "openai".to_string(),
                model: model.to_string(),
                input_sample_rate: 24000,
                output_sample_rate: 24000,
            };
        }
        return LiveGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
        };
    }

    // Fall back to env var detection
    if std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("GEMINI_API_KEY").is_err() {
        LiveGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
        }
    } else {
        LiveGatewayConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::OutboundEvent;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn test_default_port() {
        assert_eq!(DEFAULT_PORT, 8765);
    }

    #[test]
    fn test_live_html_embedded() {
        assert!(!LIVE_HTML.is_empty());
        assert!(LIVE_HTML.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn test_live_gateway_config_default() {
        let config = LiveGatewayConfig::default();
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
        assert_eq!(config.output_sample_rate, 24000);
    }

    #[test]
    fn test_live_gateway_config_serialize() {
        let config = LiveGatewayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"provider\":\"gemini\""));
        assert!(json.contains("\"input_sample_rate\":16000"));
    }

    #[test]
    fn test_build_config_gemini_model() {
        let config = build_config(None, Some("gemini-2.5-flash-native-audio-preview-12-2025"));
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(None, Some("gpt-4o-realtime-preview"));
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(Some("openai"), None);
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(None, None);
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    #[tokio::test]
    async fn test_spawn_live_gateway_lifecycle() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let config = LiveGatewayConfig::default();
        let handle = spawn_live_gateway(0, bus, broadcast_tx, config);

        // Give it a moment to bind
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_websocket_echo() {
        let (bus, mut rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        // Bind to port 0 for a random free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = LiveGatewayConfig::default();
        let handle = spawn_live_gateway(port, bus, broadcast_tx, config);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a Status control message
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Verify the EventBus receives the ControlCommand
        let event = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel closed");

        match event {
            AppEvent::ControlCommand(ControlMsg::Status) => {}
            _ => panic!("expected ControlCommand(Status), got {:?}", event),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_broadcast_to_websocket() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = LiveGatewayConfig::default();
        let handle = spawn_live_gateway(port, bus, broadcast_tx.clone(), config);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx, mut ws_rx) = ws.split();

        // Give the subscription a moment to register
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Broadcast an event
        let event = OutboundEvent::Status {
            turn: 1,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
            session_id: "test-session".to_string(),
            task: "test task".to_string(),
        };
        crate::control::broadcast_event(&broadcast_tx, &event);

        // Verify the WebSocket client receives it
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            ws_rx.next(),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            assert!(text.contains("\"event\":\"status\""));
            assert!(text.contains("\"turn\":1"));
        } else {
            panic!("expected text message");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_html() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = LiveGatewayConfig::default();
        let handle = spawn_live_gateway(port, bus, broadcast_tx, config);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Plain HTTP GET
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        // Read with timeout
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("<!DOCTYPE html>"));

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_config() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = LiveGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
        };
        let handle = spawn_live_gateway(port, bus, broadcast_tx, config);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // GET /config
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("application/json"));
        assert!(response_str.contains("\"provider\":\"openai\""));

        handle.abort();
    }
}
