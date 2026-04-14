//! Concrete [`PeerTransport`](crate::peer::traits::PeerTransport)
//! implementations.
//!
//! Transports translate between the transport-neutral
//! [`PeerOp`](crate::peer::traits::PeerOp)/
//! [`PeerEvent`](crate::peer::event::PeerEvent) abstractions and a
//! specific wire protocol. They're constructed once per peer by the
//! registry, with the sender side of an mpsc channel injected via
//! their constructors — the per-peer actor owns the receiver side
//! and drains events off it.
//!
//! Phase 1 ships one transport, [`intendant::IntendantWsTransport`],
//! which speaks Intendant's own `/ws` wire protocol and is the
//! canonical path for Intendant↔Intendant federation. Follow-up
//! commits add A2A, OpenClaw Gateway, and generic MCP-as-peer
//! transports as sibling modules alongside it.

pub mod intendant;

pub use intendant::IntendantWsTransport;

/// Derive the HTTP(S) base URL for Agent Card discovery from a
/// native Intendant WebSocket URL. `ws://` becomes `http://`,
/// `wss://` becomes `https://`, and a trailing `/ws` is stripped
/// so the caller can append any `.well-known` path.
///
/// Handles the common cases cleanly; URLs that don't match either
/// scheme are returned unchanged on the assumption the caller
/// knows what they're doing. Not exhaustive — a URL with a weird
/// port layout or query string still flows through, and the
/// Agent Card fetch will fail cleanly at HTTP time with a
/// diagnostic error rather than silently 404ing.
pub(crate) fn ws_url_to_http_base(ws_url: &str) -> String {
    let (scheme, rest) = if let Some(rest) = ws_url.strip_prefix("wss://") {
        ("https://", rest)
    } else if let Some(rest) = ws_url.strip_prefix("ws://") {
        ("http://", rest)
    } else {
        return ws_url.to_string();
    };
    let base = format!("{scheme}{rest}");
    base.trim_end_matches("/ws")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn ws_to_http_base() {
        assert_eq!(
            ws_url_to_http_base("ws://127.0.0.1:8765/ws"),
            "http://127.0.0.1:8765"
        );
        assert_eq!(
            ws_url_to_http_base("wss://nicks-mac.local:8443/ws"),
            "https://nicks-mac.local:8443"
        );
        assert_eq!(
            ws_url_to_http_base("ws://127.0.0.1:8765"),
            "http://127.0.0.1:8765"
        );
        assert_eq!(
            ws_url_to_http_base("ws://127.0.0.1:8765/"),
            "http://127.0.0.1:8765"
        );
        // Non-ws schemes pass through unchanged — the fetch step
        // will diagnose at HTTP time.
        assert_eq!(
            ws_url_to_http_base("https://example.com/agent"),
            "https://example.com/agent"
        );
    }
}
