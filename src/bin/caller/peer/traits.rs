//! Transport trait, operation envelope, and transport features.
//!
//! [`PeerTransport`] is the only trait in this module. Higher-level
//! ergonomics live on [`crate::peer::handle::PeerHandle`], which is a
//! concrete struct that owns an actor task that in turn owns the
//! transport. This avoids the trait-object-discovery problems of
//! sub-trait splits: `Box<dyn Peer>` can't downcast to specific role
//! traits without `Any` machinery, so the role split is kept entirely
//! on the handle side.
//!
//! ## Event delivery
//!
//! Transports do not expose an event stream — they accept the sender
//! side of an `mpsc::Sender<PeerEvent>` at construction time (via a
//! transport-specific `new()`) and push [`PeerEvent`]s to it as they
//! arrive off the wire. The per-peer actor owns the receiver side
//! from the moment the channel is created, which removes the awkward
//! "take the stream once" semantics entirely.
//!
//! ## Mutable state
//!
//! `&mut self` on [`PeerTransport::connect`], [`PeerTransport::disconnect`],
//! and [`PeerTransport::send`] reflects the fact that wire state is
//! inherently connection-local and mutable (JSON-RPC id counter,
//! pending-request map, etc.). The per-peer actor task is the sole
//! owner; there is no need to hide this behind interior mutability.

use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::event::{ApprovalDecision, MessageId, PeerMessage, TaskId, TaskUpdate};
use crate::peer::PeerError;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// TransportFeatures
// ---------------------------------------------------------------------------

/// Which [`PeerOp`] variants a transport supports.
///
/// Documented contract: this is **static wire-verb support**, a property
/// of the transport implementation class, not a per-session negotiation
/// result. A transport whose protocol has per-connection feature
/// variance should reflect the *union* of possible features here and
/// return [`PeerError::UnsupportedCapability`] from
/// [`PeerTransport::send`] when asked to perform a feature that wasn't
/// negotiated in the current session.
///
/// Disambiguation: `TransportFeatures` is the set of wire verbs the
/// transport can issue; [`crate::peer::card::AgentCard`]'s `capabilities`
/// field is the set of *services* the peer agent offers. Different axes
/// — a transport supports `send_message` (the verb), while the peer's
/// card claims `Capability::Display` (the service).
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportFeatures {
    /// Transport can push events back without polling (WebSocket, SSE,
    /// stdio). False for pure HTTP REST with no subscription mechanism.
    pub bidirectional: bool,
    /// Transport offers a long-lived event stream we can subscribe to.
    /// False if events must be synthesized from polling at a higher layer.
    pub streaming_events: bool,
    /// Transport supports [`PeerOp::SendMessage`].
    pub send_message: bool,
    /// Transport supports [`PeerOp::DelegateTask`].
    pub task_delegation: bool,
    /// Transport supports [`PeerOp::CancelTask`].
    pub task_cancel: bool,
    /// Transport supports [`PeerOp::QueryTaskStatus`].
    pub task_query: bool,
    /// Transport supports [`PeerOp::InvokeCapability`] (OpenClaw
    /// node-style capability invocation).
    pub invoke_capability: bool,
    /// Transport supports [`PeerOp::ResolveApproval`].
    pub resolve_approval: bool,
}

// ---------------------------------------------------------------------------
// Operation envelope
// ---------------------------------------------------------------------------

/// Outbound operation envelope. Transport-neutral; each concrete
/// transport maps variants onto its wire protocol.
#[derive(Clone, Debug)]
pub enum PeerOp {
    SendMessage { message: PeerMessage },
    DelegateTask { task: PeerTask },
    CancelTask { task: TaskId },
    QueryTaskStatus { task: TaskId },
    InvokeCapability {
        name: String,
        args: serde_json::Value,
    },
    ResolveApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
}

impl PeerOp {
    /// Short name for error messages and logging.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SendMessage { .. } => "send_message",
            Self::DelegateTask { .. } => "delegate_task",
            Self::CancelTask { .. } => "cancel_task",
            Self::QueryTaskStatus { .. } => "query_task_status",
            Self::InvokeCapability { .. } => "invoke_capability",
            Self::ResolveApproval { .. } => "resolve_approval",
        }
    }
}

/// Acknowledgment for a [`PeerOp`]. The variant returned matches the op.
#[derive(Clone, Debug)]
pub enum PeerOpAck {
    /// Generic ack — operation accepted, no payload.
    Ok,
    /// Response to `SendMessage` — the peer-assigned message id.
    MessageId(MessageId),
    /// Response to `DelegateTask` — the peer-assigned task id.
    TaskId(TaskId),
    /// Response to `QueryTaskStatus`.
    TaskStatus(TaskUpdate),
    /// Response to `InvokeCapability`.
    Value(serde_json::Value),
}

impl PeerOpAck {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::MessageId(_) => "MessageId",
            Self::TaskId(_) => "TaskId",
            Self::TaskStatus(_) => "TaskStatus",
            Self::Value(_) => "Value",
        }
    }
}

/// Wire-level task payload. Distinct from a coordinator-level task
/// request (which carries routing metadata like required capabilities);
/// the peer only ever sees what's in this struct.
#[derive(Clone, Debug)]
pub struct PeerTask {
    /// Free-form natural-language instructions for the peer's agent.
    pub instructions: String,

    /// Structured context to pass through (file paths, prior state,
    /// anything not expressible in natural language).
    pub context: serde_json::Value,

    /// Optional caller-supplied correlation id. Unused in phase 1.
    /// Reserved for idempotent retries after transport timeouts: a
    /// coordinator that retries a delegation after a timeout passes
    /// the same id so the peer can deduplicate. Adding the field now
    /// so the wire type is stable before retry logic lands.
    pub client_correlation_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Feature enforcement helper
// ---------------------------------------------------------------------------

/// Check that a transport supports the given op. Called by every
/// transport implementation at the top of its [`PeerTransport::send`]
/// as the invariant guard.
///
/// The [`crate::peer::handle::PeerHandle`] layer also checks features
/// up front as an early reject — this helper is the second line of
/// defense so transports can't be driven out of spec by callers that
/// bypass the handle. Adding a new [`PeerOp`] variant forces a new
/// arm here, which compile-errors until the corresponding
/// [`TransportFeatures`] flag is added.
pub fn check_feature(features: &TransportFeatures, op: &PeerOp) -> Result<(), PeerError> {
    let supported = match op {
        PeerOp::SendMessage { .. } => features.send_message,
        PeerOp::DelegateTask { .. } => features.task_delegation,
        PeerOp::CancelTask { .. } => features.task_cancel,
        PeerOp::QueryTaskStatus { .. } => features.task_query,
        PeerOp::InvokeCapability { .. } => features.invoke_capability,
        PeerOp::ResolveApproval { .. } => features.resolve_approval,
    };
    if !supported {
        return Err(PeerError::UnsupportedCapability(op.name().to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The transport trait
// ---------------------------------------------------------------------------

/// Low-level wire handler for one peer connection.
///
/// Single trait, six methods. All role-specific ergonomics live on
/// [`crate::peer::handle::PeerHandle`] — transports just move bytes.
#[async_trait]
pub trait PeerTransport: Send + Sync {
    /// The transport spec this instance was constructed from.
    fn spec(&self) -> &TransportSpec;

    /// Static wire-verb support. See [`TransportFeatures`].
    fn features(&self) -> TransportFeatures;

    /// Connect and complete the auth handshake. Returns the peer's
    /// most recently advertised [`AgentCard`], which may differ from
    /// the card used to discover the peer if the peer has re-issued.
    async fn connect(&mut self) -> Result<AgentCard, PeerError>;

    /// Disconnect cleanly. Idempotent — disconnecting an already-closed
    /// transport is not an error.
    async fn disconnect(&mut self) -> Result<(), PeerError>;

    fn is_connected(&self) -> bool;

    /// Execute one operation. Must call [`check_feature`] at the top
    /// as the invariant guard.
    async fn send(&mut self, op: PeerOp) -> Result<PeerOpAck, PeerError>;
}
