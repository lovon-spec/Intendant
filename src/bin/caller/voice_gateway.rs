use crate::tui::event::{AppEvent, ControlMsg, EventBus};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_PORT: u16 = 8765;

const VOICE_HTML: &str = include_str!("../../../static/voice.html");

/// Spawn the voice gateway HTTP/WebSocket server.
///
/// - Plain HTTP GET requests receive `voice.html`.
/// - WebSocket connections are bridged to the EventBus (inbound control
///   messages) and broadcast channel (outbound events), mirroring the
///   Unix control socket in `control.rs`.
pub fn spawn_voice_gateway(
    port: u16,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Voice gateway bind failed on {}: {}", addr, e);
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

                    let body = VOICE_HTML;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {}",
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
    fn test_voice_html_embedded() {
        assert!(!VOICE_HTML.is_empty());
        assert!(VOICE_HTML.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn test_spawn_voice_gateway_lifecycle() {
        let (bus, _rx) = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let handle = spawn_voice_gateway(0, bus, broadcast_tx);

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

        let handle = spawn_voice_gateway(port, bus, broadcast_tx);
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

        let handle = spawn_voice_gateway(port, bus, broadcast_tx.clone());
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

        let handle = spawn_voice_gateway(port, bus, broadcast_tx);
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
}
