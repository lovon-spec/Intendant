//! Multi-transport wrapper: walks a list of candidate transports and
//! uses the first one whose `connect()` succeeds.
//!
//! Why this exists: an Agent Card may advertise several reachable
//! addresses for the same peer (LAN IP, Tailscale tailnet IP,
//! port-forwarded WAN URL, etc.) in preference order. The connecting
//! daemon should probe them in order and use the first that works,
//! instead of requiring the operator to know which URL is reachable
//! from where. See `web_gateway::resolve_advertise_urls` for the
//! advertise side and `peer/registry::pick_supported_transports` for
//! how the candidate list is filtered before reaching this wrapper.
//!
//! ## Connect behavior
//!
//! `connect()` walks `candidates` in card order, attempting each
//! one's `connect()` until one succeeds. The successful candidate
//! becomes the active transport; subsequent `send()` and
//! `disconnect()` calls delegate to it. If all candidates fail, the
//! error from the *last* attempt is returned and earlier errors are
//! logged at warning level so the operator can see why each path
//! failed.
//!
//! ## Re-probe on reconnect
//!
//! Every call to `connect()` (including the actor's reconnect path
//! after a transport drop) re-walks the full list from the top. This
//! is intentional — if a more-preferred path comes back online while
//! we were running on a fallback, the next reconnect picks it up.
//! The cost is small because failing transports fail fast (connect
//! timeout is short and there is no per-transport retry at this
//! layer; the actor handles retries).
//!
//! ## Capability reporting
//!
//! Before any candidate has connected, `features()` returns the
//! union of all candidates' features — assume the best case until
//! we know which path will win, so coordinator-level capability
//! checks don't prematurely reject ops a candidate could support
//! once it connects. After connect, it returns the active
//! transport's actual features.
//!
//! ## Lifecycle
//!
//! All candidates are constructed up front (each gets its own clone
//! of the per-peer events sender). Only the active candidate is
//! connected at any given time; non-active ones sit idle and emit
//! nothing. `disconnect` asks the active one to disconnect and
//! clears `active`, so the next `connect()` starts fresh from the
//! top of the list.

use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::traits::{PeerOp, PeerOpAck, PeerTransport, TransportFeatures};
use crate::peer::PeerError;
use async_trait::async_trait;

pub struct MultiTransport {
    candidates: Vec<Box<dyn PeerTransport>>,
    /// Index into `candidates` of the currently-connected candidate.
    /// `None` before the first successful connect or after `disconnect`.
    active: Option<usize>,
}

impl MultiTransport {
    /// Construct from a non-empty list of candidate transports. List
    /// order is preference order — the first successful `connect()`
    /// wins and becomes the active transport for the next
    /// connect→disconnect cycle.
    pub fn new(candidates: Vec<Box<dyn PeerTransport>>) -> Self {
        debug_assert!(
            !candidates.is_empty(),
            "MultiTransport requires at least one candidate"
        );
        Self {
            candidates,
            active: None,
        }
    }

    /// Index of the currently-active candidate, if any. Exposed for
    /// tests; production code should use [`PeerTransport::is_connected`].
    #[cfg(test)]
    pub(crate) fn active_index(&self) -> Option<usize> {
        self.active
    }
}

#[async_trait]
impl PeerTransport for MultiTransport {
    fn spec(&self) -> &TransportSpec {
        // Active when connected, first candidate's spec otherwise.
        // Both are stable enough for log/UI use; the field's main
        // consumer is the actor's reconnect logging.
        let i = self.active.unwrap_or(0);
        self.candidates[i].spec()
    }

    fn features(&self) -> TransportFeatures {
        match self.active {
            Some(i) => self.candidates[i].features(),
            None => union_features(&self.candidates),
        }
    }

    async fn connect(&mut self) -> Result<AgentCard, PeerError> {
        let mut last_error: Option<PeerError> = None;
        for (i, candidate) in self.candidates.iter_mut().enumerate() {
            match candidate.connect().await {
                Ok(card) => {
                    self.active = Some(i);
                    return Ok(card);
                }
                Err(e) => {
                    eprintln!(
                        "multi-transport: candidate {i} ({:?}) connect failed: {e}",
                        candidate.spec()
                    );
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PeerError::Transport("multi-transport: no candidates available".into())
        }))
    }

    async fn disconnect(&mut self) -> Result<(), PeerError> {
        let result = match self.active {
            Some(i) => self.candidates[i].disconnect().await,
            None => Ok(()),
        };
        self.active = None;
        result
    }

    fn is_connected(&self) -> bool {
        match self.active {
            Some(i) => self.candidates[i].is_connected(),
            None => false,
        }
    }

    async fn send(&mut self, op: PeerOp) -> Result<PeerOpAck, PeerError> {
        match self.active {
            Some(i) => self.candidates[i].send(op).await,
            None => Err(PeerError::NotConnected),
        }
    }
}

/// Union of all candidates' features. Used by `MultiTransport::features`
/// before any candidate has connected — reports the most permissive
/// view so coordinator-level capability checks don't reject ops that
/// *could* be supported once a candidate actually connects.
fn union_features(candidates: &[Box<dyn PeerTransport>]) -> TransportFeatures {
    let mut u = TransportFeatures::default();
    for c in candidates {
        let f = c.features();
        u.bidirectional |= f.bidirectional;
        u.streaming_events |= f.streaming_events;
        u.send_message |= f.send_message;
        u.task_delegation |= f.task_delegation;
        u.task_cancel |= f.task_cancel;
        u.task_query |= f.task_query;
        u.invoke_capability |= f.invoke_capability;
        u.resolve_approval |= f.resolve_approval;
        u.webrtc_signal |= f.webrtc_signal;
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::card::{AuthRequirements, Capability};
    use crate::peer::event::{MessageContent, MessageRole, PeerMessage};
    use crate::peer::id::{PeerId, PeerKind};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Minimal in-process transport for testing the MultiTransport
    /// dispatch logic. `connect_succeeds` controls whether `connect()`
    /// returns Ok or a Transport error; `connected` reflects current
    /// state.
    struct StubTransport {
        spec: TransportSpec,
        connect_succeeds: bool,
        connected: AtomicBool,
        connect_count: Arc<AtomicUsize>,
        send_count: Arc<AtomicUsize>,
    }

    impl StubTransport {
        fn new(url: &str, connect_succeeds: bool) -> Self {
            Self {
                spec: TransportSpec::IntendantWs { url: url.into() },
                connect_succeeds,
                connected: AtomicBool::new(false),
                connect_count: Arc::new(AtomicUsize::new(0)),
                send_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    fn dummy_card() -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::Intendant, "stub"),
            label: "stub".into(),
            version: "test".into(),
            git_sha: None,
            transports: vec![],
            capabilities: vec![Capability::ComputerUse],
            auth: AuthRequirements::none(),
        }
    }

    #[async_trait]
    impl PeerTransport for StubTransport {
        fn spec(&self) -> &TransportSpec {
            &self.spec
        }
        fn features(&self) -> TransportFeatures {
            TransportFeatures {
                bidirectional: true,
                streaming_events: true,
                send_message: true,
                ..Default::default()
            }
        }
        async fn connect(&mut self) -> Result<AgentCard, PeerError> {
            self.connect_count.fetch_add(1, Ordering::SeqCst);
            if self.connect_succeeds {
                self.connected.store(true, Ordering::SeqCst);
                Ok(dummy_card())
            } else {
                let url = match &self.spec {
                    TransportSpec::IntendantWs { url } => url.clone(),
                    _ => "?".into(),
                };
                Err(PeerError::Transport(format!("stub {url} fail")))
            }
        }
        async fn disconnect(&mut self) -> Result<(), PeerError> {
            self.connected.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }
        async fn send(&mut self, _op: PeerOp) -> Result<PeerOpAck, PeerError> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(PeerOpAck::Ok)
        }
    }

    fn dummy_send_op() -> PeerOp {
        PeerOp::SendMessage {
            message: PeerMessage {
                session: None,
                role: MessageRole::User,
                content: MessageContent::Text { text: "hi".into() },
            },
        }
    }

    #[tokio::test]
    async fn first_candidate_wins() {
        let mut multi = MultiTransport::new(vec![
            Box::new(StubTransport::new("ws://a", true)),
            Box::new(StubTransport::new("ws://b", true)),
        ]);
        multi.connect().await.unwrap();
        assert!(multi.is_connected());
        assert_eq!(multi.active_index(), Some(0));
        match multi.spec() {
            TransportSpec::IntendantWs { url } => assert_eq!(url, "ws://a"),
            _ => panic!("wrong spec variant"),
        }
    }

    #[tokio::test]
    async fn falls_through_to_second_when_first_fails() {
        let mut multi = MultiTransport::new(vec![
            Box::new(StubTransport::new("ws://a", false)),
            Box::new(StubTransport::new("ws://b", true)),
        ]);
        multi.connect().await.unwrap();
        assert_eq!(multi.active_index(), Some(1));
        match multi.spec() {
            TransportSpec::IntendantWs { url } => assert_eq!(url, "ws://b"),
            _ => panic!("wrong spec variant"),
        }
    }

    #[tokio::test]
    async fn returns_last_error_when_all_fail() {
        let mut multi = MultiTransport::new(vec![
            Box::new(StubTransport::new("ws://a", false)),
            Box::new(StubTransport::new("ws://b", false)),
        ]);
        let err = multi.connect().await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("ws://b"),
            "expected the last error from ws://b, got: {msg}"
        );
        assert_eq!(multi.active_index(), None);
    }

    #[tokio::test]
    async fn disconnect_clears_active() {
        let mut multi = MultiTransport::new(vec![Box::new(StubTransport::new("ws://a", true))]);
        multi.connect().await.unwrap();
        assert!(multi.is_connected());
        multi.disconnect().await.unwrap();
        assert!(!multi.is_connected());
        assert_eq!(multi.active_index(), None);
    }

    #[tokio::test]
    async fn reconnect_re_probes_from_top() {
        // Both candidates succeed. After disconnect+reconnect, the
        // first candidate should be probed again — ensures a more-
        // preferred path is reused once the actor cycles.
        let stub_a = StubTransport::new("ws://a", true);
        let count_a = stub_a.connect_count.clone();
        let stub_b = StubTransport::new("ws://b", true);
        let count_b = stub_b.connect_count.clone();
        let mut multi = MultiTransport::new(vec![Box::new(stub_a), Box::new(stub_b)]);
        multi.connect().await.unwrap();
        multi.disconnect().await.unwrap();
        multi.connect().await.unwrap();
        assert_eq!(
            count_a.load(Ordering::SeqCst),
            2,
            "first candidate should be probed on every reconnect"
        );
        assert_eq!(
            count_b.load(Ordering::SeqCst),
            0,
            "second candidate should never be probed when first succeeds"
        );
    }

    #[tokio::test]
    async fn send_when_not_connected_errors() {
        let mut multi = MultiTransport::new(vec![Box::new(StubTransport::new("ws://a", true))]);
        let err = multi.send(dummy_send_op()).await.unwrap_err();
        assert!(matches!(err, PeerError::NotConnected));
    }

    #[tokio::test]
    async fn send_routes_to_active() {
        let stub_a = StubTransport::new("ws://a", false);
        let count_a_send = stub_a.send_count.clone();
        let stub_b = StubTransport::new("ws://b", true);
        let count_b_send = stub_b.send_count.clone();
        let mut multi = MultiTransport::new(vec![Box::new(stub_a), Box::new(stub_b)]);
        multi.connect().await.unwrap();
        multi.send(dummy_send_op()).await.unwrap();
        assert_eq!(count_a_send.load(Ordering::SeqCst), 0);
        assert_eq!(count_b_send.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn features_pre_connect_is_union() {
        let multi = MultiTransport::new(vec![
            Box::new(StubTransport::new("ws://a", true)),
            Box::new(StubTransport::new("ws://b", true)),
        ]);
        let f = multi.features();
        assert!(f.bidirectional);
        assert!(f.send_message);
        assert!(!f.task_delegation, "stub doesn't claim task delegation");
    }
}
