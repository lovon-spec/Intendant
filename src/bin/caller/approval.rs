//! Shared approval types.
//!
//! Both the external agent layer (supervised coding-CLI subprocesses)
//! and the peer federation layer (federated autonomous daemons) need
//! to represent the same four-way user response to an approval
//! request. They arrive at the decision through different paths —
//! `external_agent` intercepts a subprocess's approval request and
//! surfaces it in the TUI / web dashboard; `peer` federates with a
//! remote daemon that has its own approval flow and forwards
//! requests across the connection — but the decision vocabulary is
//! the same, so the type lives here instead of being duplicated in
//! each consumer.
//!
//! Re-exported at the original locations:
//! [`crate::external_agent::ApprovalDecision`] and
//! [`crate::peer::event::ApprovalDecision`] both point to this type,
//! so existing call sites continue to work unchanged.

use serde::{Deserialize, Serialize};

/// Four-way user response to an approval request.
///
/// - `Accept` — approve this specific request. The next similar
///   request will prompt again.
/// - `AcceptForSession` — approve this request and any similar
///   request for the remainder of the session. Used when the user
///   trusts a class of operation ("yes, let the agent edit files in
///   this directory") but doesn't want to upgrade their global
///   autonomy level.
/// - `Decline` — reject this request but let the agent continue with
///   alternative approaches.
/// - `Cancel` — reject this request and signal the agent to stop the
///   current task entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}
