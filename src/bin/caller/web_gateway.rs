use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::presence::{self, AgentStateSnapshot};
use crate::types::LogLevel;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
// Phase 5a.1: the display input authority map is read from a synchronous
// `Fn() -> bool` closure on the WebRTC data-channel input hot path, so
// it can't live behind a `tokio::sync::RwLock` (no `.read().await` from
// sync code).  `StdRwLock` is the local alias to keep that map's type
// distinct at every callsite from the unrelated `tokio::sync::RwLock`
// uses in this file.  All access goes through `unwrap_or_else(|e| e.into_inner())`
// to remain poison-tolerant, matching the rest of the file's std-lock idiom.
use std::sync::RwLock as StdRwLock;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

/// Monotonically increasing counter for assigning unique peer IDs to WebSocket
/// connections.  Used for WebRTC signaling so that each browser tab gets a
/// stable identity within a display session.
static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);
static SESSION_SEARCH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static EXTERNAL_TRANSCRIPT_CACHE: OnceLock<Mutex<HashMap<String, ExternalTranscriptCacheEntry>>> =
    OnceLock::new();

const EXTERNAL_SESSION_SCAN_LIMIT: usize = 2_000;
const EXTERNAL_SESSION_READ_LIMIT: u64 = 512 * 1024;
const EXTERNAL_TRANSCRIPT_CACHE_LIMIT: usize = 32;
const SESSION_LIST_LIMIT: usize = 5_000;
const SESSION_SOURCE_FLOOR: usize = 100;
const SESSION_LOG_SEARCH_LIMIT: usize = 150;
const SESSION_LOG_SEARCH_READ_LIMIT: u64 = 2 * 1024 * 1024;
const SESSION_LOG_SEARCH_FIELD_CHARS: usize = 8 * 1024;
const SESSION_LOG_SEARCH_SNIPPETS_PER_SESSION: usize = 3;
const SESSION_LOG_SEARCH_SNIPPET_CHARS: usize = 220;
const FS_LIST_LIMIT: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternalTranscriptCacheKey {
    source: String,
    session_id: String,
    path: String,
    len: u64,
    mtime_nanos: u128,
}

#[derive(Clone, Debug)]
struct ExternalTranscriptCacheEntry {
    key: ExternalTranscriptCacheKey,
    entries: Vec<serde_json::Value>,
}

/// Tracks which WebSocket connection currently owns the voice model (is "active").
/// Only one connection can be active at a time; all others are "passive" (TUI-only).
struct ActivePresence {
    connection_id: String,
    direct_tx: mpsc::UnboundedSender<String>,
}

/// Identity of who currently holds input authority for one display.
///
/// Two provenance kinds, with explicit identity per kind so the
/// arbitration / gate / cleanup paths can match on the source of the
/// hold without resorting to string-shape inference:
///
/// - **`LocalWs`**: holder is a WebSocket connection on this gateway.
///   Carries the WS connection id (identity) plus the connection's
///   `direct_tx` for the local-only `display_input_authority_revoked`
///   confirmation that fires when this holder is preempted by another
///   grant. Federated holders do NOT get a direct revoke — federated
///   state always flows through the personalized authority-state
///   broadcast on each federated WebRtcPeer's `display_input_authority`
///   data channel.
///
/// - **`FederatedWebRtc`**: holder is a federated `PeerDisplayConnection`
///   on a peer primary. Identified by `(federation_connection_id,
///   session_id)`. `federation_connection_id` is the gateway-WS
///   `connection_id` of the federation transport (one per primary's
///   federation client); `session_id` distinguishes multiple
///   `PeerDisplayConnection` tabs from the same primary. Field name
///   spelled out so it's not confused with the local-browser
///   `LocalWs::connection_id`.
///
///   The design doc originally specified `peer_id: PeerId`, but the
///   stable federation `PeerId` isn't carried in the
///   `ControlMsg::WebRtcSignal` wire format — it's implicit in which
///   `/ws` connection delivered the message. F-1.3b uses the federation
///   WS `connection_id` as the holder identity instead; it's
///   authenticated by the federation WS connection, unique per
///   primary's federation transport, and already covered by WS-close
///   cleanup. `connection_id` changes across federation WS reconnect
///   (a stable `PeerId` would survive); WS-close cleanup releases any
///   held authority on each disconnect, so the trade-off is a UX
///   nicety, not correctness. See
///   `docs/design-federated-input-authority.md` for the full note.
///
/// The map is `HashMap<u32, DisplayInputHolder>` — no `Option`, no
/// wrapper struct. Entry absence = unclaimed; that's the pre-phase-5
/// backwards-compat state where every connection's input flowed
/// through (now: only the holder's input flows through; everyone
/// else's is dropped at the gate, federated input is dropped
/// unconditionally until F-2 lights up the federated input gate).
#[derive(Clone, Debug)]
enum DisplayInputHolder {
    LocalWs {
        connection_id: String,
        /// Outbound channel for sending this WS connection's
        /// `display_input_authority_revoked` confirmation when a
        /// later grant preempts this holder. Local-only — the
        /// federated path uses the personalized authority-state
        /// broadcast for the same notification.
        direct_tx: mpsc::UnboundedSender<String>,
    },
    FederatedWebRtc {
        federation_connection_id: String,
        session_id: String,
    },
}

impl DisplayInputHolder {
    /// True iff this holder is `LocalWs` with the given `connection_id`.
    /// Used by local gate / personalization sites; deliberately returns
    /// false for `FederatedWebRtc` rather than panicking, so a future
    /// caller that mistakenly passes a connection id from the federated
    /// side gets a silent drop rather than mis-authorization.
    fn matches_local_ws(&self, connection_id: &str) -> bool {
        match self {
            Self::LocalWs {
                connection_id: c, ..
            } => c == connection_id,
            Self::FederatedWebRtc { .. } => false,
        }
    }

    /// True iff this holder is `FederatedWebRtc` with the given
    /// `(federation_connection_id, session_id)` pair. Used by the
    /// federated input gate (in F-2) and the federated close-cleanup
    /// path.
    fn matches_federated(&self, federation_connection_id: &str, session_id: &str) -> bool {
        match self {
            Self::FederatedWebRtc {
                federation_connection_id: c,
                session_id: s,
            } => c == federation_connection_id && s == session_id,
            Self::LocalWs { .. } => false,
        }
    }

    /// True iff `self` and `other` identify the same holder
    /// (provenance + identity). Used by release / preempt sites where
    /// we need to compare the requesting holder against the current
    /// one without unwrapping the variant manually. Deliberately
    /// ignores `direct_tx` (which isn't equality-comparable and isn't
    /// part of identity — it's a notification handle that can change
    /// if the same WS connection rebuilds its outbound queue).
    ///
    /// Distinct from a `PartialEq` impl on purpose: spelled-out method
    /// at call sites makes intent explicit and prevents accidental
    /// equality-comparison pitfalls in collections / `.contains()` /
    /// pattern guards.
    ///
    /// Production callers don't need this yet — every F-1 / F-2
    /// release-or-preempt site already knows which provenance kind it's
    /// matching against and uses `matches_local_ws` /
    /// `matches_federated` directly. The method is pinned by unit
    /// tests as the documented identity-equality contract for future
    /// arbitration work (e.g. F-2's per-primary multi-operator
    /// scoping, where the comparison is against an opaque
    /// `DisplayInputHolder` snapshot).
    #[allow(dead_code)]
    fn same_identity(&self, other: &DisplayInputHolder) -> bool {
        match (self, other) {
            (
                Self::LocalWs {
                    connection_id: a, ..
                },
                Self::LocalWs {
                    connection_id: b, ..
                },
            ) => a == b,
            (
                Self::FederatedWebRtc {
                    federation_connection_id: ca,
                    session_id: sa,
                },
                Self::FederatedWebRtc {
                    federation_connection_id: cb,
                    session_id: sb,
                },
            ) => ca == cb && sa == sb,
            _ => false,
        }
    }
}

/// Phase 5a.1: dedicated internal broadcast event for display input
/// authority transitions.
///
/// Carries the holder's *server-internal* identity (or `None` for
/// unclaimed) so each WS outbound task can personalize this for its
/// own browser as `you | other | unclaimed` without ever shipping
/// holder IDs to browsers.  Personalization happens in the
/// per-connection outbound select arm where `connection_id_outbound`
/// is in scope.
///
/// Distinct from [`AppEvent`] on purpose: the generic outbound
/// broadcast carries already-serialized JSON strings, which would leak
/// holder IDs if we routed authority through it.  A dedicated typed
/// channel keeps the holder identity inside the gateway and forces
/// every per-connection consumer to compute its own personalized state.
#[derive(Clone, Debug)]
struct DisplayInputAuthorityChange {
    display_id: u32,
    holder: Option<DisplayInputHolder>,
}

/// Build the per-peer "may this connection inject input now?" closure
/// for the local `/ws` display-offer path (Phase 5a.1).
///
/// Returns a closure that consults the live authority map every time
/// it's called, so a grant or release elsewhere takes effect on the
/// very next data-channel input event without needing to reconstruct
/// the closure or rebuild the peer connection.
///
/// Semantics:
/// - `auth.get(display_id) == Some(entry)` and
///   `entry.matches_local_ws(this_id)` → `true`
///   (this WS connection holds authority)
/// - `auth.get(display_id) == Some(entry)` and
///   `!entry.matches_local_ws(this_id)` → `false`
///   (someone else — local or, once the variant lands, federated —
///   holds it; silent drop)
/// - `auth.get(display_id) == None`
///   → `true` (unclaimed = pre-phase-5 default; any connection can
///   input)
///
/// The federated path does NOT call this; it has its own deny-by-
/// default authorizer that becomes a `FederatedWebRtc` registry
/// lookup in F-1's later commits.
fn build_local_ws_input_authorizer(
    display_id: u32,
    connection_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) => entry.matches_local_ws(&connection_id),
            None => true,
        }
    })
}

/// Capacity of the [`DisplayInputAuthorityChange`] broadcast channel.
///
/// Sized to comfortably absorb a burst of grants/releases across a few
/// dozen connected browsers — typically 1-3 events per user action,
/// fanned out across all WS connections.  64 is plenty of headroom and
/// cheap; lagged subscribers fall back to a fresh personalized snapshot
/// path (see the `Lagged` arm in the outbound select).
const AUTHORITY_CHANGE_CAPACITY: usize = 64;

/// F-2: federated path's input-authorization closure. Returns `true`
/// iff the current holder for `display_id` is `FederatedWebRtc` matching
/// THIS peer's `(federation_connection_id, session_id)`. Anything else
/// — no holder, a `LocalWs` holder, a `FederatedWebRtc` with a different
/// session id (e.g. another tab from the same primary), or a different
/// connection — returns `false` and the federated input handler drops
/// the event silently.
///
/// Symmetric in shape to [`build_local_ws_input_authorizer`], but with
/// strict deny-by-default for the unclaimed case: local 5c treats `None`
/// as "anyone may input" for pre-phase-5 backwards compatibility, while
/// the federated path has no such legacy and treats `None` as "nobody
/// holds this — drop everything." A federated browser only sends input
/// when its chip is `'you'` (UX-side guard); receiving input here under
/// any other condition is a protocol bug or a stale post-release race
/// and silent drop is correct.
///
/// The closure is the entire boundary: `display/mod.rs` invokes it per
/// event and never sees the registry, the holder identity, or the
/// connection/session IDs. F-2's gate flip is the single semantic change
/// from F-1's `Arc::new(|| false)` deny-everything stub.
fn build_federated_input_authorizer(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) => entry.matches_federated(&federation_connection_id, &session_id),
            None => false,
        }
    })
}

/// Apply a `RequestDisplayInputAuthority`.  Inserts the new holder,
/// returns the prior holder if any, sends `display_input_authority_revoked`
/// to the prior holder (if displaced), and emits the personalized
/// authority change for fan-out.  Caller is responsible for the
/// `display_input_authority_granted` confirm to `requester_direct_tx`
/// and the bus log message — both stay at the call site to keep the
/// helper's surface narrow (no logging dependency, no second send to
/// the same channel).
///
/// Lock discipline: the `authority` write guard is dropped before any
/// `direct_tx.send` or `authority_change_tx.send` call.
fn apply_grant_input_authority(
    display_id: u32,
    requester_connection_id: String,
    requester_direct_tx: mpsc::UnboundedSender<String>,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::LocalWs {
        connection_id: requester_connection_id.clone(),
        direct_tx: requester_direct_tx,
    };
    // Clone for the broadcast — broadcast recipients personalize
    // from holder identity (the channel-clone in LocalWs is unused
    // downstream but cheap because mpsc::UnboundedSender is
    // Arc-backed).
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    // Only `LocalWs` prior holders get the direct revoke confirmation
    // — `direct_tx` is local-only by design (see `DisplayInputHolder`
    // doc). A `FederatedWebRtc` prior holder learns of the preempt
    // through the personalized authority-state broadcast on its own
    // `display_input_authority` data channel.
    if let Some(DisplayInputHolder::LocalWs {
        connection_id: prior_id,
        direct_tx: prior_tx,
    }) = prior.as_ref()
    {
        if prior_id != &requester_connection_id {
            let notify = serde_json::json!({
                "t": "display_input_authority_revoked",
                "display_id": display_id,
                "reason": "another connection requested control",
            })
            .to_string();
            let _ = prior_tx.send(notify);
        }
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

/// Apply a `ReleaseDisplayInputAuthority`.  No-op if the calling
/// connection isn't the holder (prevents A from unclaiming B's slot).
/// Returns `true` iff the slot was actually released.  Emits the
/// personalized authority change with `None` only when the release
/// took effect — a no-op release does not flip anyone's UI state.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
fn apply_release_input_authority(
    display_id: u32,
    releaser_connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_local_ws(releaser_connection_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

/// F-1.3b: federated grant. Constructs a `FederatedWebRtc` holder
/// from `(federation_connection_id, session_id)`, inserts it into
/// the registry, returns the prior holder if any, and emits the
/// personalized authority change for fan-out.
///
/// Mirrors [`apply_grant_input_authority`] for the local path but
/// is provenance-distinct: federated holders carry no `direct_tx`
/// (federated state always flows through the personalized
/// authority-state broadcast on the federated WebRtcPeer's
/// `display_input_authority` data channel — see the F-1 design
/// note in `DisplayInputHolder`).
///
/// Prior holder revocation:
/// - If prior is `LocalWs`, send the existing
///   `display_input_authority_revoked` notification on the prior
///   holder's `direct_tx`. Same protocol as a local→local handover.
/// - If prior is `FederatedWebRtc` with a different identity, no
///   direct revoke — the broadcast-driven personalized state
///   `"other"` reaches that prior federated holder via its own
///   authority data channel and updates its chip.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
fn apply_grant_input_authority_federated(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::FederatedWebRtc {
        federation_connection_id: federation_connection_id.clone(),
        session_id: session_id.clone(),
    };
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    // Prior LocalWs holder gets the legacy direct revoke; prior
    // FederatedWebRtc gets nothing here because the personalized
    // broadcast below carries `"other"` to it on its own data channel.
    if let Some(DisplayInputHolder::LocalWs {
        direct_tx: prior_tx,
        ..
    }) = prior.as_ref()
    {
        let notify = serde_json::json!({
            "t": "display_input_authority_revoked",
            "display_id": display_id,
            "reason": "another connection requested control",
        })
        .to_string();
        let _ = prior_tx.send(notify);
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

/// F-1.3b: federated release. No-op if the calling
/// `(federation_connection_id, session_id)` doesn't match the
/// current holder (prevents one federated session from unclaiming
/// another's slot — even from the same primary, distinct
/// `PeerDisplayConnection` tabs have distinct `session_id`s).
/// Returns `true` iff the slot was actually released.
///
/// Lock discipline: matches [`apply_grant_input_authority_federated`].
fn apply_release_input_authority_federated(
    display_id: u32,
    federation_connection_id: &str,
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_federated(federation_connection_id, session_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

/// F-1.3b: federated WS-close cleanup. Releases every
/// `FederatedWebRtc` entry whose `federation_connection_id` matches
/// the dropping federation transport, regardless of `session_id`
/// (the WS drop kills every `PeerDisplayConnection` session multiplexed
/// over that primary's federation transport). Emits one `None`-holder
/// authority change per affected display so other viewers' chips
/// flip back to `unclaimed`.
///
/// Distinct from [`apply_ws_close_input_authority`] which targets
/// `LocalWs` entries: a single `connection_id` is either acting as
/// a local browser or a federation transport but not both, so the
/// two cleanup paths address disjoint registry entries. Both fire
/// from the same WS-close hook (the gateway calls them in sequence).
///
/// Lock discipline: matches [`apply_grant_input_authority_federated`].
fn apply_federated_ws_close_input_authority(
    federation_connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| match entry {
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: c,
                ..
            } if c == federation_connection_id => {
                out.push(*did);
                false
            }
            _ => true,
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

// ---------------------------------------------------------------------------
// F-1.3b3: federated authority subscriber registry + helpers
//
// The federated counterpart to local 5c's per-WS subscriber model.
// Local 5c has no shared subscriber registry — each WS outbound task
// subscribes to `authority_change_tx` directly and personalizes for
// its own `connection_id`. Federated needs a registry because the
// send target is `WebRtcPeer::send_authority_state`, not a WS
// `direct_tx`: the gateway must hold an `Arc<WebRtcPeer>` to push to,
// and that handle isn't available until `handle_offer` returns and
// the peer is stored in the session.
//
// One entry per `(federation_connection_id, session_id, display_id)` —
// uniquely identifies one federated `PeerDisplayConnection`'s
// subscription to one display's authority state. Each entry owns a
// fanout task + a `CancellationToken` for clean teardown on the two
// distinct cleanup edges:
//
// 1. `WebRtcSignal::Close` / `DisplaySession::remove_peer(peer_id)`:
//    unregister this exact `(federation_connection_id, session_id,
//    display_id)` entry. Identity-matched authority release runs
//    alongside via `apply_release_input_authority_federated`.
// 2. Federation WS close: unregister all entries for that
//    `federation_connection_id`. Bulk authority release runs
//    alongside via `apply_federated_ws_close_input_authority`.
// ---------------------------------------------------------------------------

/// One federated authority subscriber. Holds the cancellation token
/// that terminates the per-subscriber fanout task on cleanup.
///
/// The `Arc<WebRtcPeer>` push target lives entirely inside the
/// fanout task spawned by [`register_federated_authority_subscriber`];
/// the registry doesn't carry a second copy because nothing reads
/// it back. The Drop chain is: cleanup edge calls `shutdown.cancel()`
/// → fanout task exits → its captured peer Arc drops → reference
/// count to the `WebRtcPeer` decrements. Any peer-teardown work that
/// the registry needs (e.g. tearing down WebRtcPeers on federation
/// WS-close) lives separately at the gateway level via
/// [`peer_id_for_federated_session`] + `DisplaySession::remove_peer`,
/// not by holding a duplicate Arc here.
struct FederatedAuthoritySubscriber {
    shutdown: tokio_util::sync::CancellationToken,
}

/// Stable mapping from a federated `session_id` (the
/// browser-supplied per-`PeerDisplayConnection` id round-tripped in
/// `ControlMsg::WebRtcSignal`) to the [`crate::display::PeerId`]
/// (`u64`) used as the `WebRtcPeer` key inside `DisplaySession`.
///
/// Used in two places that must agree exactly:
/// 1. [`handle_federated_webrtc_signal`] — derives the key on
///    Offer/IceCandidate/Close so subsequent signals route to the
///    same peer.
/// 2. WS-close cleanup — derives the key from each `(session_id,
///    display_id)` returned by
///    [`unregister_all_federated_subscribers_for_connection`] so
///    the federation WS-close can call `DisplaySession::remove_peer`
///    on every WebRtcPeer owned by the dropping connection.
///
/// A divergence between the two callers would leak peers (cleanup
/// would target a different key than was inserted on Offer), which
/// is exactly the bug fixed by extracting this helper.
fn peer_id_for_federated_session(session_id: &str) -> crate::display::PeerId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    session_id.hash(&mut h);
    h.finish()
}

/// Gateway-side registry of federated authority subscribers, keyed by
/// `(federation_connection_id, session_id, display_id)`. Owned by the
/// gateway listener task; cloned per-WS for the inbound handler so
/// every per-connection branch can register/unregister without
/// passing the registry through every helper signature.
type FederatedAuthoritySubscribers =
    Arc<StdRwLock<HashMap<(String, String, u32), FederatedAuthoritySubscriber>>>;

/// Compute the personalized authority state for one federated
/// subscriber from a `Option<&DisplayInputHolder>`. Returns `You` if
/// the holder is a `FederatedWebRtc` matching this subscriber's
/// `(federation_connection_id, session_id)`, `Other` if any other
/// holder exists, `Unclaimed` if no one holds. Mirrors the local 5c
/// outbound personalization logic at the per-WS subscriber loop.
fn personalize_authority_for_federated(
    holder: Option<&DisplayInputHolder>,
    federation_connection_id: &str,
    session_id: &str,
) -> crate::display::webrtc::DisplayInputAuthorityState {
    use crate::display::webrtc::DisplayInputAuthorityState;
    match holder {
        Some(h) if h.matches_federated(federation_connection_id, session_id) => {
            DisplayInputAuthorityState::You
        }
        Some(_) => DisplayInputAuthorityState::Other,
        None => DisplayInputAuthorityState::Unclaimed,
    }
}

/// Build the federated authority data-channel handler closure.
///
/// The handler is invoked by the WebRTC driver on every parsed
/// [`crate::display::webrtc::AuthorityChannelMessage`] received on the
/// `display_input_authority` channel. Identity is captured at
/// construction time, so messages from this peer always apply
/// authority changes against this peer's
/// `(federation_connection_id, session_id)` — there's no way for one
/// federated session to act on behalf of another, even from the same
/// primary.
///
/// Display-ID mismatches are silently dropped: the federated peer's
/// `PeerDisplayConnection` is bound to one display, so a request for
/// any other display is a protocol bug on the browser side rather
/// than a recoverable condition. Authority gating still applies on
/// the input-injection path (F-2's job), so a misdirected message
/// here can't bypass anything.
fn build_federated_authority_handler(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
) -> crate::display::webrtc::AuthorityChannelHandler {
    use crate::display::webrtc::AuthorityChannelMessage;
    Arc::new(move |msg| match msg {
        AuthorityChannelMessage::Request {
            display_id: req_did,
        } if req_did == display_id => {
            apply_grant_input_authority_federated(
                display_id,
                federation_connection_id.clone(),
                session_id.clone(),
                &authority,
                &authority_change_tx,
            );
        }
        AuthorityChannelMessage::Release {
            display_id: req_did,
        } if req_did == display_id => {
            apply_release_input_authority_federated(
                display_id,
                &federation_connection_id,
                &session_id,
                &authority,
                &authority_change_tx,
            );
        }
        AuthorityChannelMessage::Request { .. } | AuthorityChannelMessage::Release { .. } => {
            // Display-ID mismatch — drop silently. See doc comment.
        }
    })
}

/// Register a federated authority subscriber and start its fanout
/// task. Called from the federated `Offer` arm after a successful
/// `DisplaySession::handle_offer` and `get_peer` lookup.
///
/// Behavior, in order:
/// 1. Subscribe to `authority_change_tx` FIRST. Doing this before
///    the snapshot read closes the race where a holder change
///    arrives between the registry read and the subscribe — without
///    this ordering, that change would land on neither the snapshot
///    nor the fanout, and the chip would end up stale until the
///    next change.
/// 2. Compute the initial personalized snapshot from the current
///    registry state and send it via `peer.send_authority_state`.
///    F-1.2's pending-authority queue absorbs the case where the
///    `display_input_authority` data channel hasn't opened yet on
///    the federated browser side — the queued state flushes on
///    `OnDataChannel(OnOpen)` so the chip cannot start stuck on
///    `unknown`.
/// 3. Spawn the fanout task with the rx from step 1. It
///    personalizes each inbound change for this subscriber's
///    identity and pushes via `peer.send_authority_state`. Lagged
///    subscribers re-snapshot from the registry — same recovery
///    pattern as the local 5c lagged path so a momentary catch-up
///    cannot leave the chip on stale state.
/// 4. Insert the entry into `subscribers` keyed by
///    `(federation_connection_id, session_id, display_id)` so
///    cleanup edges can reach it.
///
/// Snapshot-vs-change ordering across the wire is FIFO via
/// `WebRtcPeer::send_authority_state`'s underlying `Command`
/// channel. If a change races the initial snapshot, both land on
/// the channel in the order they were enqueued; the more recent
/// one wins on the browser side, so the chip ends up correct
/// regardless of which arrives last.
fn register_federated_authority_subscriber(
    federation_connection_id: String,
    session_id: String,
    display_id: u32,
    peer: Arc<crate::display::webrtc::WebRtcPeer>,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    subscribers: FederatedAuthoritySubscribers,
) {
    // 1. Subscribe BEFORE snapshot — closes the race window where a
    //    change between snapshot read and subscribe lands on neither
    //    path.
    let mut auth_rx = authority_change_tx.subscribe();

    // 2. Initial snapshot. F-1.2's queue handles "channel not open yet."
    let initial_state = {
        let map = authority.read().unwrap_or_else(|e| e.into_inner());
        personalize_authority_for_federated(
            map.get(&display_id),
            &federation_connection_id,
            &session_id,
        )
    };
    let peer_for_initial = Arc::clone(&peer);
    tokio::spawn(async move {
        let _ = peer_for_initial
            .send_authority_state(display_id, initial_state)
            .await;
    });

    // 3. Fanout task.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task_authority = Arc::clone(&authority);
    let task_fcid = federation_connection_id.clone();
    let task_sid = session_id.clone();
    let task_did = display_id;
    let task_peer = peer; // moved — registry doesn't keep a copy.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_shutdown.cancelled() => break,
                msg = auth_rx.recv() => match msg {
                    Ok(change) if change.display_id == task_did => {
                        let state = personalize_authority_for_federated(
                            change.holder.as_ref(),
                            &task_fcid,
                            &task_sid,
                        );
                        let _ = task_peer
                            .send_authority_state(task_did, state)
                            .await;
                    }
                    Ok(_) => {} // change for a different display
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Re-snapshot from registry — same recovery
                        // pattern as the local 5c lagged subscriber so
                        // the chip is never left stuck on stale state.
                        let state = {
                            let map = task_authority
                                .read()
                                .unwrap_or_else(|e| e.into_inner());
                            personalize_authority_for_federated(
                                map.get(&task_did),
                                &task_fcid,
                                &task_sid,
                            )
                        };
                        let _ = task_peer
                            .send_authority_state(task_did, state)
                            .await;
                    }
                }
            }
        }
    });

    // 4. Insert into the registry. Replace-on-collision: a duplicate
    //    `(fcid, sid, did)` would mean a renegotiated peer for the
    //    same identity; cancel the prior shutdown to terminate its
    //    fanout task before the new entry takes over.
    if let Some(prior) = subscribers
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            (federation_connection_id, session_id, display_id),
            FederatedAuthoritySubscriber { shutdown },
        )
    {
        prior.shutdown.cancel();
    }
}

/// Unregister one federated authority subscriber by exact identity.
/// Called from the federated `Close` arm. Cancels the fanout task
/// and removes the entry. Returns `true` if an entry was removed.
///
/// Does NOT release authority — that's
/// `apply_release_input_authority_federated`'s responsibility, called
/// alongside this function. Splitting the two keeps each helper
/// single-purpose: this one manages subscriber lifecycle, the other
/// manages the holder map.
fn unregister_federated_authority_subscriber(
    federation_connection_id: &str,
    session_id: &str,
    display_id: u32,
    subscribers: &FederatedAuthoritySubscribers,
) -> bool {
    if let Some(sub) = subscribers
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(
            federation_connection_id.to_string(),
            session_id.to_string(),
            display_id,
        ))
    {
        sub.shutdown.cancel();
        true
    } else {
        false
    }
}

/// Tear down every federated `WebRtcPeer` listed in `released`.
/// Called from the federation WS-close cleanup hook AFTER
/// [`unregister_all_federated_subscribers_for_connection`] returns
/// the surviving entries' `(session_id, display_id)` pairs. Without
/// this, the WebRTC data channels on those peers would stay alive
/// past the federation WS drop and could keep dispatching
/// `display_input_authority_request` frames against the registry —
/// the authority handler closure captures the
/// `federation_connection_id` at construction time, so a request
/// arriving after the WS-close would re-grant the (already-released)
/// authority under a now-defunct identity.
///
/// Tearing the peers down here is the structural fix: the federation
/// WS identity is the only thing tying these peers to a real user;
/// once it's gone the peers must go too. `DisplaySession::remove_peer`
/// closes the underlying WebRTC peer connection cleanly, which causes
/// every data channel on it to close and the driver task to exit —
/// no further authority frames can be processed.
///
/// Returns the count of peers actually removed. Missing displays
/// (display session torn down between Offer and WS-close) and
/// missing peers (already removed by an earlier `WebRtcSignal::Close`
/// for the same session) both fall through silently as no-ops on
/// `remove_peer`.
async fn close_federated_peers_for_sessions(
    released: &[(String, u32)],
    session_registry: Option<&Arc<tokio::sync::RwLock<crate::display::SessionRegistry>>>,
) -> usize {
    if released.is_empty() {
        return 0;
    }
    let Some(sr) = session_registry else {
        return 0;
    };
    // Snapshot Arcs out of the read guard first so per-peer awaits
    // (remove_peer's `peer.close()` chain) don't hold the registry
    // lock — same lock-discipline rationale as the local
    // `display_ice` handler that fixed the original 5-20s mDNS
    // starvation. The registry's RwLock is read-only here so a
    // concurrent display deactivate isn't blocked by us either way,
    // but keeping the pattern consistent prevents future regressions
    // if the lock semantics change.
    let mut targets: Vec<(Arc<crate::display::DisplaySession>, crate::display::PeerId)> =
        Vec::with_capacity(released.len());
    {
        let reg = sr.read().await;
        for (sid, did) in released {
            if let Some(session) = reg.get(*did) {
                targets.push((session, peer_id_for_federated_session(sid)));
            }
        }
    }
    let count = targets.len();
    for (session, pid) in targets {
        session.remove_peer(pid).await;
    }
    count
}

/// Unregister every federated authority subscriber for a dropping
/// federation transport. Called from the WS-close cleanup hook
/// alongside [`apply_federated_ws_close_input_authority`]. Returns
/// the `(session_id, display_id)` pairs that were unregistered, for
/// caller logging and for the post-step
/// [`close_federated_peers_for_sessions`] which actually tears down
/// the WebRtcPeers.
fn unregister_all_federated_subscribers_for_connection(
    federation_connection_id: &str,
    subscribers: &FederatedAuthoritySubscribers,
) -> Vec<(String, u32)> {
    let mut released = Vec::new();
    let mut map = subscribers.write().unwrap_or_else(|e| e.into_inner());
    map.retain(|(fcid, sid, did), sub| {
        if fcid == federation_connection_id {
            released.push((sid.clone(), *did));
            sub.shutdown.cancel();
            false
        } else {
            true
        }
    });
    released
}

/// Apply WS-close cleanup for a dropping connection.  Removes every
/// authority entry held by `connection_id` and emits one `None`-holder
/// authority change per affected display so observers move from
/// `you/other` back to `unclaimed`.  Returns the list of released
/// display ids for caller logging / tests.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
fn apply_ws_close_input_authority(
    connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| {
            if entry.matches_local_ws(connection_id) {
                out.push(*did);
                false
            } else {
                true
            }
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

/// Phase 5a.1 / 5c.2: build the personalized
/// `display_input_authority_state` snapshot a freshly-connecting browser
/// needs to bootstrap its chip from `unknown` to the authoritative state.
///
/// One entry per active display id, with `state` resolved against this
/// connection's id:
/// - `"you"` if `connection_id` currently holds the slot;
/// - `"other"` if some other connection holds it;
/// - `"unclaimed"` if no one holds it.
///
/// Holder connection ids never leave the daemon — the caller serializes
/// only the resolved `&'static str` into the `display_input_authority_state`
/// frame.
///
/// The frames built from this snapshot must be sent to `direct_tx`
/// **after** the `log_replay` block: replayed historical `display_ready` /
/// `user_display_revoked` events re-trigger `addDisplaySlot` /
/// `removeDisplaySlot` on the browser, which destroys the bootstrap slot
/// and creates a fresh one whose chip starts at `unknown`. Sending the
/// authority snapshot after replay guarantees it lands on the *final*
/// slot, so a late-connecting browser never gets stranded at `unknown`
/// for a display that already exists. See the
/// `bootstrap_authority_snapshots_*` tests for the regression coverage.
fn compute_bootstrap_authority_snapshots(
    active_display_ids: impl IntoIterator<Item = u32>,
    authority: &HashMap<u32, DisplayInputHolder>,
    connection_id: &str,
) -> Vec<(u32, &'static str)> {
    active_display_ids
        .into_iter()
        .map(|did| {
            let state = match authority.get(&did) {
                Some(entry) if entry.matches_local_ws(connection_id) => "you",
                Some(_) => "other",
                None => "unclaimed",
            };
            (did, state)
        })
        .collect()
}

pub const DEFAULT_PORT: u16 = 8765;

/// Mint a short-lived vendor session token server-side so the browser
/// never handles (or stores) a long-lived API key.
async fn mint_session_token(provider: &str, model: &str) -> Result<String, String> {
    match provider {
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| "OPENAI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "model": model,
            });
            let resp = reqwest::Client::new()
                .post("https://api.openai.com/v1/realtime/sessions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("OpenAI request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("OpenAI HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("OpenAI parse failed: {}", e))?;
            // Response may have token at top level or nested under client_secret
            let token = data["client_secret"]["value"]
                .as_str()
                .or_else(|| data["value"].as_str())
                .ok_or_else(|| format!("No token in OpenAI response: {}", data))?;
            let expires_at = data["client_secret"]["expires_at"]
                .as_i64()
                .or_else(|| data["expires_at"].as_i64())
                .unwrap_or(0);
            Ok(serde_json::json!({
                "client_secret": { "value": token },
                "expires_at": expires_at
            })
            .to_string())
        }
        "gemini" => {
            let api_key = std::env::var("GEMINI_API_KEY")
                .map_err(|_| "GEMINI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "uses": 1,
                "bidi_generate_content_setup": {
                    "model": format!("models/{}", model),
                    "generation_config": {
                        "response_modalities": ["AUDIO"],
                        "speech_config": {
                            "voice_config": {
                                "prebuilt_voice_config": {
                                    "voice_name": "Aoede"
                                }
                            }
                        }
                    }
                }
            });
            let url = format!(
                "https://generativelanguage.googleapis.com/v1alpha/auth_tokens?key={}",
                api_key
            );
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Gemini request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("Gemini HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Gemini parse failed: {}", e))?;
            let token = data["name"]
                .as_str()
                .ok_or("No 'name' in Gemini response")?;
            Ok(serde_json::json!({ "token": token }).to_string())
        }
        _ => Err(format!("Unknown provider: {}", provider)),
    }
}

const APP_HTML: &str = include_str!("../../../static/app.html");
const AUDIO_PROCESSOR_JS: &str = include_str!("../../../static/audio-processor.js");
const ICON_128_PNG: &[u8] = include_bytes!("../../../static/icon-128.png");
const WASM_WEB_JS: &str = include_str!("../../../static/wasm-web/presence_web.js");
const WASM_WEB_BIN: &[u8] = include_bytes!("../../../static/wasm-web/presence_web_bg.wasm");
// 0 means replay every renderable entry from the external audit transcript.
// External activity replay intentionally includes only user/assistant messages
// and explicit context-rewind markers, not tool events or tool output.
const EXTERNAL_ACTIVITY_REPLAY_LIMIT: usize = 0;
const EXTERNAL_TRANSCRIPT_SEMANTICS: &str = "full_audit_transcript";

/// Session-specific state that changes when a new agent session starts.
/// Wrapped in `Arc<tokio::sync::RwLock<...>>` so the web gateway can observe
/// session changes without restarting.
pub struct ActiveSessionState {
    /// Stable identity for the long-lived Intendant process. This is distinct
    /// from `session_log`, which may point at a currently active worker session
    /// and may be cleared while the dashboard waits for new tasks.
    pub daemon_session_id: Option<String>,
    pub query_ctx: Option<WebQueryCtx>,
    pub frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    pub session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    pub recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    pub session_registry: Option<crate::display::SharedSessionRegistry>,
    pub snapshot_dir: Option<PathBuf>,
    pub project_root_for_changes: Option<PathBuf>,
    /// Shared handle to the live `FileWatcher`, used to serve the per-round
    /// history endpoints (GET history, POST rollback/redo/prune). The same
    /// mutex guards snapshot creation so concurrent rollback from the web
    /// gateway and snapshot-on-round-complete can't race.
    pub file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
}

impl ActiveSessionState {
    pub fn empty() -> SharedActiveSession {
        Arc::new(tokio::sync::RwLock::new(Self {
            daemon_session_id: None,
            query_ctx: None,
            frame_registry: None,
            session_log: None,
            recording_registry: None,
            session_registry: None,
            snapshot_dir: None,
            project_root_for_changes: None,
            file_watcher: None,
        }))
    }
}

pub type SharedActiveSession = Arc<tokio::sync::RwLock<ActiveSessionState>>;

/// Context for answering presence tool queries from browser-side live models.
/// Shared across all WebSocket connections (read-only for query tools).
#[derive(Clone)]
pub struct WebQueryCtx {
    pub agent_state: Arc<Mutex<AgentStateSnapshot>>,
    pub project_root: PathBuf,
    pub log_dir: PathBuf,
    pub knowledge_path: PathBuf,
    /// Server-authoritative presence session (event window + checkpoint state).
    pub presence_session: Option<Arc<Mutex<crate::presence::PresenceSession>>>,
    /// Shared context injection queue for mid-task interjections.
    pub context_injection: Option<crate::event::ContextInjectionQueue>,
}

#[derive(Debug, Serialize)]
struct FsPathStatus {
    input: String,
    path: String,
    exists: bool,
    is_dir: bool,
    is_file: bool,
    readable: bool,
    parent: Option<String>,
    parent_exists: bool,
    parent_is_dir: bool,
    nearest_existing_parent: Option<String>,
    can_create: bool,
}

#[derive(Debug, Serialize)]
struct FsListEntry {
    name: String,
    path: String,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
    hidden: bool,
}

#[derive(Debug, Deserialize)]
struct FsMkdirRequest {
    path: String,
}

/// Debug state for the voice model, tracked server-side from WebSocket messages.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceDebugState {
    pub connected: bool,
    pub voice_log_count: u32,
    pub last_voice_log: String,
}

/// Voice + WebRTC runtime config sent to the web frontend via `/config`.
///
/// Scoped to *runtime config only* — the voice provider, the active
/// model, audio sample rates, and WebRTC ICE servers. Identity-shaped
/// fields (host label, version, git sha) moved out of `/config` and
/// into the Agent Card served at `/.well-known/agent-card.json`: see
/// [`crate::peer::AgentCard`] and [`crate::peer::AgentCard::local_intendant`].
/// That's the single source of truth for who this daemon is and what
/// it can do, and keeping `/config` narrow makes it less likely that
/// future runtime config additions re-blur the boundary.
#[derive(Clone, Debug, Serialize)]
pub struct WebGatewayConfig {
    pub provider: String,
    pub model: String,
    pub input_sample_rate: u32,
    pub output_sample_rate: u32,
    /// Whether server-side transcription is enabled (browser should send user_audio).
    #[serde(default)]
    pub transcription_enabled: bool,
    /// ICE servers for WebRTC peer connections (STUN/TURN).
    /// Empty by default (local-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<crate::display::IceServer>,
}

impl Default for WebGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
        }
    }
}

/// Spawn the web gateway HTTP/WebSocket server.
///
/// - `GET /config` returns a JSON `WebGatewayConfig` (voice/runtime only).
/// - `GET /.well-known/agent-card.json` returns a JSON `AgentCard` with
///   this daemon's identity, capabilities, transports, and auth scheme.
/// - `GET /icon-128.png` and `GET /favicon.ico` return the dashboard icon.
/// - `GET /` (and any other path) returns the web TUI page.
/// - WebSocket connections are bridged to the EventBus (inbound control
///   messages) and broadcast channel (outbound events), mirroring the
///   Unix control socket in `control.rs`.
/// Scan session.jsonl for persisted provider/model/autonomy values.
///
/// The agent loop writes these as plain log entries at startup
/// (`Provider: X`, `Model: Y`, `Autonomy: Z`).  Today the writer uses
/// `l.debug(...)`, so event_type is `debug` for newer sessions and
/// `info` for older ones — scan both.  Replay uses the result to seed
/// the status bar before any events are rendered, replacing the old
/// prefix-based parsing inside `handle_log_replay`.
fn scan_replay_status(contents: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut autonomy: Option<String> = None;
    for line in contents.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ev = v.get("event").and_then(|x| x.as_str()).unwrap_or("");
        if !matches!(ev, "info" | "debug" | "warn" | "error") {
            continue;
        }
        let Some(msg) = v.get("message").and_then(|x| x.as_str()) else {
            continue;
        };
        if provider.is_none() {
            if let Some(rest) = msg.strip_prefix("Provider: ") {
                provider = Some(rest.split_whitespace().next().unwrap_or("").to_string());
            }
        }
        if model.is_none() {
            if let Some(rest) = msg.strip_prefix("Model: ") {
                model = Some(rest.to_string());
            }
        }
        if autonomy.is_none() {
            if let Some(rest) = msg.strip_prefix("Autonomy: ") {
                autonomy = Some(rest.to_string());
            }
        }
        if provider.is_some() && model.is_some() && autonomy.is_some() {
            break;
        }
    }
    (provider, model, autonomy)
}

/// Convert session.jsonl contents into a stream of OutboundEvent-shaped
/// JSON objects ready to be sent as a `log_replay` message.
///
/// The first entry is always a `replay_start` marker carrying
/// provider/model/autonomy so the WASM `handle_log_replay` can seed the
/// status bar.  Subsequent entries are the result of running each JSONL
/// row through `session_log_entry_to_app_event` → `app_event_to_outbound`
/// and injecting the original `ts` field, so replay drives the exact
/// same rendering path as live broadcast.
fn replay_jsonl_to_outbound_entries(
    contents: &str,
    log_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    let (provider, model, autonomy) = scan_replay_status(contents);
    let replay_session_id = replay_session_id_from_dir(log_dir);

    let mut entries: Vec<serde_json::Value> = Vec::new();
    entries.push(serde_json::json!({
        "event": "replay_start",
        "provider": provider,
        "model": model,
        "autonomy": autonomy,
    }));

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(app_event) =
            crate::session_log::session_log_entry_to_app_event(&entry_json, log_dir)
        else {
            continue;
        };
        let Some(outbound) = crate::event::app_event_to_outbound(&app_event) else {
            continue;
        };
        let Ok(mut value) = serde_json::to_value(&outbound) else {
            continue;
        };
        // Inject the historical timestamp so WASM's handle_event uses it
        // instead of wallclock when rendering log entries.
        if let Some(obj) = value.as_object_mut() {
            let ts = entry_json
                .get("ts")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            obj.insert("ts".to_string(), serde_json::Value::String(ts));
            if !obj.contains_key("session_id") {
                if let Some(session_id) = replay_session_id.as_deref() {
                    obj.insert(
                        "session_id".to_string(),
                        serde_json::Value::String(session_id.to_string()),
                    );
                }
            }
        }
        entries.push(value);
    }

    entries
}

fn replay_session_id_from_dir(log_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(log_dir.join("session_meta.json"))
        .ok()
        .and_then(|meta| serde_json::from_str::<crate::session_log::SessionMeta>(&meta).ok())
        .map(|meta| meta.session_id)
        .filter(|session_id| !session_id.trim().is_empty())
        .or_else(|| {
            log_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|session_id| !session_id.trim().is_empty())
        })
}

fn session_log_id(session_log: &Arc<Mutex<crate::session_log::SessionLog>>) -> Option<String> {
    session_log
        .lock()
        .ok()
        .map(|log| log.session_id().to_string())
        .filter(|id| !id.trim().is_empty())
}

fn session_log_replay_from_dir(log_dir: &std::path::Path) -> Option<String> {
    let session_jsonl = log_dir.join("session.jsonl");
    let contents = std::fs::read_to_string(&session_jsonl).ok()?;
    Some(
        serde_json::json!({
            "t": "log_replay",
            "entries": replay_jsonl_to_outbound_entries(&contents, log_dir),
        })
        .to_string(),
    )
}

fn agent_output_chunks_with_fallback(
    primary_log_dir: &Path,
    ids: &[String],
    fallback_logs_dir: Option<&Path>,
) -> Vec<crate::session_log::AgentOutputChunk> {
    let mut found: HashMap<String, crate::session_log::AgentOutputChunk> = HashMap::new();

    for chunk in crate::session_log::agent_output_chunks_by_id(primary_log_dir, ids) {
        found.entry(chunk.output_id.clone()).or_insert(chunk);
    }

    if found.len() < ids.len() {
        if let Some(logs_dir) = fallback_logs_dir {
            let mut dirs = Vec::new();
            if let Ok(entries) = std::fs::read_dir(logs_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir()
                        && path.join("session.jsonl").is_file()
                        && !same_path(&path, primary_log_dir)
                    {
                        dirs.push(path);
                    }
                }
            }
            dirs.sort_by(|a, b| session_log_mtime(b).cmp(&session_log_mtime(a)));

            for dir in dirs {
                let missing: Vec<String> = ids
                    .iter()
                    .filter(|id| !found.contains_key(id.as_str()))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    break;
                }
                for chunk in crate::session_log::agent_output_chunks_by_id(&dir, &missing) {
                    found.entry(chunk.output_id.clone()).or_insert(chunk);
                }
            }
        }
    }

    ids.iter().filter_map(|id| found.remove(id)).collect()
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn session_log_mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path.join("session.jsonl"))
        .or_else(|_| std::fs::metadata(path))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

fn current_agent_output_response(request_line: &str, log_dir: &Path) -> String {
    let ids_param = query_param(request_line, "ids").unwrap_or_default();
    let ids: Vec<String> = ids_param
        .split(',')
        .map(str::trim)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
        })
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        return upload_error_response("400 Bad Request", "missing output ids");
    }

    let fallback_logs_dir = std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".intendant").join("logs"));
    let chunks = agent_output_chunks_with_fallback(log_dir, &ids, fallback_logs_dir.as_deref());
    let found: HashSet<&str> = chunks
        .iter()
        .map(|chunk| chunk.output_id.as_str())
        .collect();
    let missing: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !found.contains(id))
        .collect();
    let body = serde_json::json!({
        "outputs": chunks,
        "missing": missing,
    })
    .to_string();
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    )
}

fn intendant_session_dir_from_home(home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.contains('/') {
        let dir = PathBuf::from(session_id);
        return dir.is_dir().then_some(dir);
    }

    let logs_dir = home.join(".intendant").join("logs");
    let direct = logs_dir.join(session_id);
    if direct.is_dir() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(session_id) {
            return Some(path);
        }
        let meta_path = path.join("session_meta.json");
        let Ok(meta_str) = std::fs::read_to_string(meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str) else {
            continue;
        };
        let Some(meta_id) = meta.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if meta_id == session_id || meta_id.starts_with(session_id) {
            return Some(path);
        }
    }

    None
}

#[derive(Debug, Clone, Default)]
struct ExternalSessionContext {
    project_root: Option<String>,
    cwd: Option<String>,
    source: Option<String>,
    source_label: Option<String>,
    name: Option<String>,
}

fn external_session_context_by_id(
    sessions: &[serde_json::Value],
) -> HashMap<String, ExternalSessionContext> {
    let mut out = HashMap::new();
    for session in sessions {
        let context = ExternalSessionContext {
            project_root: value_str(session, "project_root"),
            cwd: value_str(session, "cwd"),
            source: value_str(session, "source"),
            source_label: value_str(session, "source_label"),
            name: value_str(session, "name"),
        };
        if context.project_root.is_none()
            && context.cwd.is_none()
            && context.source.is_none()
            && context.source_label.is_none()
            && context.name.is_none()
        {
            continue;
        }
        for key in [
            value_str(session, "session_id"),
            value_str(session, "resume_id"),
        ]
        .into_iter()
        .flatten()
        {
            out.entry(key).or_insert_with(|| context.clone());
        }
    }
    out
}

fn session_value_matches_external_id(session: &serde_json::Value, external_id: &str) -> bool {
    ["session_id", "resume_id", "backend_session_id"]
        .into_iter()
        .any(|key| session.get(key).and_then(|v| v.as_str()) == Some(external_id))
}

fn external_session_row_matches(
    session: &serde_json::Value,
    source: &str,
    external_id: &str,
) -> bool {
    let source = crate::session_names::normalize_source(source);
    if !session_value_matches_external_id(session, external_id) {
        return false;
    }
    let row_source = session
        .get("source")
        .and_then(|v| v.as_str())
        .map(crate::session_names::normalize_source);
    let row_backend_source = session
        .get("backend_source")
        .and_then(|v| v.as_str())
        .map(crate::session_names::normalize_source);
    row_source.as_deref() == Some(source.as_str())
        || row_backend_source.as_deref() == Some(source.as_str())
}

fn merge_intendant_wrapper_into_external_session(
    external: &mut serde_json::Value,
    wrapper: &serde_json::Value,
) {
    let Some(obj) = external.as_object_mut() else {
        return;
    };
    let Some(wrapper_obj) = wrapper.as_object() else {
        return;
    };

    for (target_key, wrapper_key) in [
        ("intendant_session_id", "session_id"),
        ("intendant_session_path", "path"),
        ("backend_source", "backend_source"),
        ("backend_source_label", "backend_source_label"),
        ("backend_session_id", "backend_session_id"),
    ] {
        if let Some(value) = wrapper_obj.get(wrapper_key) {
            obj.insert(target_key.to_string(), value.clone());
        }
    }

    for key in ["name", "task", "project_root", "cwd", "provider", "model"] {
        let current_is_empty = obj
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::is_empty)
            .unwrap_or(true);
        if current_is_empty {
            if let Some(value) = wrapper_obj.get(key).filter(|v| !v.is_null()) {
                obj.insert(key.to_string(), value.clone());
            }
        }
    }

    for key in [
        "recordings",
        "recording_bytes",
        "annotations",
        "clips",
        "frames_bytes",
        "turns_bytes",
        "logs_bytes",
        "total_bytes",
    ] {
        if let Some(value) = wrapper_obj.get(key) {
            obj.insert(format!("intendant_{key}"), value.clone());
        }
    }
    if let Some(value) = wrapper_obj.get("status") {
        obj.insert("intendant_status".to_string(), value.clone());
    }
    obj.insert(
        "can_delete_intendant_log".to_string(),
        serde_json::json!(true),
    );

    if let (Some(current), Some(wrapper_updated)) = (
        obj.get("updated_at").and_then(|v| v.as_str()),
        wrapper_obj.get("updated_at").and_then(|v| v.as_str()),
    ) {
        if timestamp_sort_secs(wrapper_updated) > timestamp_sort_secs(current) {
            obj.insert(
                "updated_at".to_string(),
                serde_json::Value::String(wrapper_updated.to_string()),
            );
        }
    }
}

fn external_agent_thread_id_from_message(message: &str) -> Option<String> {
    if let Some(thread_id) = message.strip_prefix("External agent thread: ") {
        return clean_external_thread_id(thread_id);
    }
    if message.starts_with("Mode: external agent") {
        if let Some((_, thread_id)) = message.rsplit_once("thread: ") {
            return clean_external_thread_id(thread_id);
        }
    }
    None
}

fn external_agent_source_from_message(message: &str) -> Option<String> {
    let mode = message.strip_prefix("Mode: external agent (")?;
    let (source, _) = mode.split_once(')')?;
    let source = crate::session_names::normalize_source(source);
    (!source.is_empty()).then_some(source)
}

fn pretty_external_source_label(source: &str) -> String {
    match crate::session_names::normalize_source(source).as_str() {
        "codex" => "Codex".to_string(),
        "claude-code" => "Claude Code".to_string(),
        "gemini" => "Gemini CLI".to_string(),
        "intendant" => "Intendant".to_string(),
        other => other.to_string(),
    }
}

fn clean_external_thread_id(thread_id: &str) -> Option<String> {
    let thread_id = thread_id
        .trim()
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | ',' | ';'));
    if thread_id.is_empty() || thread_id.chars().any(char::is_whitespace) {
        None
    } else {
        Some(thread_id.to_string())
    }
}

fn resume_session_activity_replay(
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
    task: Option<&str>,
    limit: usize,
) -> Option<String> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    resume_session_activity_replay_from_home(&home, source, session_id, resume_id, task, limit)
}

fn resume_session_activity_replay_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
    task: Option<&str>,
    limit: usize,
) -> Option<String> {
    if task.map(str::trim).is_some_and(|task| !task.is_empty()) {
        return None;
    }

    let source_norm = source.trim().to_lowercase();
    if source_norm == "intendant" {
        let log_dir = intendant_session_dir_from_home(home, session_id)?;
        return session_log_replay_from_dir(&log_dir);
    }

    let replay_id = resume_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(session_id);
    external_session_activity_replay_from_home_with_attach(
        home,
        &source_norm,
        replay_id,
        limit,
        false,
    )
}

/// Compute a short content hash for cache-busting embedded static assets.
/// When the WASM, JS, or favicon changes (i.e. a new build), the hash changes,
/// the URL changes, and browsers fetch the new version.
fn asset_version_hash() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    WASM_WEB_BIN.hash(&mut hasher);
    WASM_WEB_JS.hash(&mut hasher);
    ICON_128_PNG.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Build a zip containing the current session's text artifacts for the
/// Settings → "Download session report" feature. Includes session.jsonl,
/// session_meta.json, transcript.jsonl, summary.json, daemon.log,
/// panic.log, and everything under `turns/`. Excludes `frames/` and
/// `recordings/` since those can be hundreds of megabytes and are not
/// needed to diagnose controller-side bugs.
fn build_session_report_zip(session_dir: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    let buf = Cursor::new(Vec::<u8>::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    const FLAT_FILES: &[&str] = &[
        "session.jsonl",
        "session_meta.json",
        "transcript.jsonl",
        "summary.json",
        "daemon.log",
        "panic.log",
    ];

    for name in FLAT_FILES {
        let path = session_dir.join(name);
        if path.is_file() {
            let data = std::fs::read(&path)?;
            zip.start_file(*name, options)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            zip.write_all(&data)?;
        }
    }

    let turns_dir = session_dir.join("turns");
    if turns_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&turns_dir) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            files.sort();
            for path in files {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    let zip_name = format!("turns/{}", fname);
                    let data = std::fs::read(&path)?;
                    zip.start_file(&zip_name, options)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                    zip.write_all(&data)?;
                }
            }
        }
    }

    let cursor = zip
        .finish()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(cursor.into_inner())
}

/// Parse a raw HTTP request blob for the `Host:` header and return its
/// hostname portion as an `IpAddr` if it's a literal IP (v4 or v6).
///
/// We need the address the browser is using to reach us — and the Host
/// header is the one piece of the HTTP handshake that actually contains
/// that. Loopback and unspecified addresses are rejected because they
/// don't survive Firefox's remote-candidate filter and wouldn't pair
/// anyway. Hostnames (like `localhost` or `dashboard.internal`) return
/// `None` — there's no ICE-TCP candidate we can usefully emit for those.
fn extract_host_header_ip(headers: &str) -> Option<std::net::IpAddr> {
    for line in headers.lines() {
        // Look for the Host: header line, case-insensitive. `strip_prefix`
        // returning None means "this isn't the Host line" — we must
        // continue the loop, not propagate with `?`.
        let Some(rest) = line
            .strip_prefix("Host: ")
            .or_else(|| line.strip_prefix("host: "))
            .or_else(|| line.strip_prefix("HOST: "))
        else {
            continue;
        };
        // `rest` is `host[:port]` where host can be:
        //   - IPv4 literal: 192.0.2.1
        //   - Bracketed IPv6 literal: [2001:db8::1]
        //   - Hostname: example.com
        let host_part = if let Some(inner) = rest.strip_prefix('[') {
            // IPv6 literal in brackets; chop at the closing bracket.
            match inner.split(']').next() {
                Some(s) => s,
                None => return None,
            }
        } else if let Some(colon) = rest.find(':') {
            &rest[..colon]
        } else {
            rest
        };
        let trimmed = host_part.trim();
        let ip = trimmed.parse::<std::net::IpAddr>().ok()?;
        if ip.is_loopback() || ip.is_unspecified() {
            return None;
        }
        return Some(ip);
    }
    None
}

#[cfg(test)]
mod host_header_tests {
    use super::extract_host_header_ip;
    use std::net::IpAddr;

    #[test]
    fn ipv4_with_port() {
        let headers = "GET / HTTP/1.1\r\nHost: 192.168.1.10:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("192.168.1.10".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn ipv6_bracketed() {
        let headers = "GET / HTTP/1.1\r\nHost: [2001:db8::1]:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("2001:db8::1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn hostname_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: dashboard.internal:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn localhost_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn loopback_ipv4_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn loopback_ipv6_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: [::1]:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn no_host_header() {
        let headers = "GET / HTTP/1.1\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn case_insensitive_header_name() {
        let headers = "GET / HTTP/1.1\r\nhost: 10.0.0.5:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("10.0.0.5".parse::<IpAddr>().unwrap())
        );
    }
}

/// List session directories from `~/.intendant/logs/`, returning JSON metadata
/// for each session (newest first, capped at 100).
/// Return session detail: replayed log entries + metadata for a single session.
/// Resolve a session directory by exact ID or prefix match.
fn resolve_session_dir_from_home(home: &Path, session_id: &str) -> Option<PathBuf> {
    let logs_dir = home.join(".intendant").join("logs");

    if logs_dir.join(session_id).is_dir() {
        return Some(logs_dir.join(session_id));
    }
    // Prefix match
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(session_id) {
                return Some(entry.path());
            }
        }
    }
    None
}

fn resolve_session_dir(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    resolve_session_dir_from_home(&PathBuf::from(home), session_id)
}

/// List recording streams from a recordings directory on disk.
fn list_recording_streams(recordings_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(recordings_dir) {
        for entry in dirs.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let stream_dir = entry.path();
            let manifest = std::fs::read_to_string(stream_dir.join("manifest.json"))
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .unwrap_or(serde_json::json!({}));
            let segments = crate::recording::parse_segment_csv_pub(
                &stream_dir.join("segments.csv"),
                &stream_dir,
            );
            let total_duration = segments.last().map(|s| s.end_secs).unwrap_or(0.0);
            let seg_json: Vec<serde_json::Value> = segments
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "filename": s.filename,
                        "start_secs": s.start_secs,
                        "end_secs": s.end_secs,
                    })
                })
                .collect();
            let mut e = manifest;
            e["stream_name"] = serde_json::json!(name);
            e["segments"] = serde_json::Value::Array(seg_json);
            e["total_duration_secs"] = serde_json::json!(total_duration);
            entries.push(e);
        }
    }
    entries.sort_by(|a, b| a["stream_name"].as_str().cmp(&b["stream_name"].as_str()));
    entries
}

fn get_session_detail(session_id: &str) -> String {
    let session_dir = match resolve_session_dir(session_id) {
        Some(d) => d,
        None => return serde_json::json!({"error": "session not found"}).to_string(),
    };

    let jsonl_path = session_dir.join("session.jsonl");
    let entries = if let Ok(contents) = std::fs::read_to_string(&jsonl_path) {
        replay_jsonl_to_outbound_entries(&contents, &session_dir)
    } else {
        Vec::new()
    };

    // Check for screenshot frames
    let frames_dir = session_dir.join("frames");
    let mut frames: Vec<String> = Vec::new();
    if frames_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&frames_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".png") || name.ends_with(".jpg") {
                    frames.push(name);
                }
            }
        }
        frames.sort();
    }

    serde_json::json!({
        "session_id": session_dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "entries": entries,
        "frames": frames,
    }).to_string()
}

fn session_log_search_from_request(request_line: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let home_path = PathBuf::from(&home);
    let query = query_param(request_line, "q").unwrap_or_default();
    let source_filter = query_param(request_line, "source").unwrap_or_else(|| "all".to_string());
    let mode = query_param(request_line, "mode").unwrap_or_default();
    session_log_search_from_home(&home_path, &query, &source_filter, &mode)
}

fn session_log_search_from_home(
    home: &Path,
    query: &str,
    source_filter: &str,
    mode: &str,
) -> String {
    let mode = SessionLogSearchMode::from_query(mode);
    let terms = session_log_search_terms(query);
    if !mode.has_search_input(query, &terms) {
        return serde_json::json!({
            "query": query,
            "mode": mode.as_str(),
            "source_filter": normalize_session_source_filter(source_filter),
            "searched": 0,
            "truncated": false,
            "limit": SESSION_LOG_SEARCH_LIMIT,
            "truncated_files": 0,
            "results": [],
        })
        .to_string();
    }

    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&list_sessions_from_home(home)).unwrap_or_else(|_| Vec::new());
    let source_filter = normalize_session_source_filter(source_filter);
    let mut results = Vec::new();
    let mut searched = 0usize;
    let mut truncated = false;
    let mut truncated_files = 0usize;

    for session in sessions {
        let source = session
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("intendant");
        if !session_source_matches_filter(source, &source_filter) {
            continue;
        }

        let Some(session_id) = session.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if searched >= SESSION_LOG_SEARCH_LIMIT {
            truncated = true;
            break;
        }
        searched += 1;

        let session_path = session
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let Some(search_path) =
            session_log_search_file_path(home, source, session_id, session_path.as_deref())
        else {
            continue;
        };
        let Some((matches, snippets, file_truncated)) =
            search_session_log_file(&search_path, query, &terms, mode)
        else {
            continue;
        };
        if file_truncated {
            truncated_files += 1;
        }
        if matches == 0 {
            continue;
        }
        results.push(serde_json::json!({
            "key": format!("{source}:{session_id}"),
            "source": source,
            "session_id": session_id,
            "matches": matches,
            "snippets": snippets,
        }));
    }

    serde_json::json!({
        "query": query,
        "mode": mode.as_str(),
        "source_filter": source_filter,
        "searched": searched,
        "truncated": truncated,
        "limit": SESSION_LOG_SEARCH_LIMIT,
        "truncated_files": truncated_files,
        "results": results,
    })
    .to_string()
}

fn session_log_search_file_path(
    home: &Path,
    source: &str,
    session_id: &str,
    session_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = session_path {
        if source == "intendant" && path.is_dir() {
            return Some(path.join("session.jsonl"));
        }
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }

    match source {
        "intendant" => Some(resolve_session_dir_from_home(home, session_id)?.join("session.jsonl")),
        "codex" => find_codex_session_file(home, session_id),
        "claude-code" => find_claude_session_file(home, session_id),
        "gemini" => find_gemini_session_file(home, session_id),
        _ => None,
    }
}

fn find_claude_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    collect_recent_files(
        &home.join(".claude").join("projects"),
        ".jsonl",
        EXTERNAL_SESSION_SCAN_LIMIT,
    )
    .into_iter()
    .find(|path| path.file_stem().and_then(|n| n.to_str()) == Some(session_id))
}

fn find_gemini_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    collect_recent_files(
        &home.join(".gemini").join("tmp"),
        ".json",
        EXTERNAL_SESSION_SCAN_LIMIT,
    )
    .into_iter()
    .filter(|path| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("chats")
    })
    .find(|path| {
        let Some((contents, _)) = read_text_prefix(path, SESSION_LOG_SEARCH_READ_LIMIT) else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&contents)
            .ok()
            .and_then(|obj| value_str(&obj, "sessionId"))
            .as_deref()
            == Some(session_id)
    })
}

fn read_text_prefix(path: &Path, max_bytes: u64) -> Option<(String, bool)> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    let mut limited = file.by_ref().take(max_bytes.saturating_add(1));
    limited.read_to_end(&mut buf).ok()?;
    let truncated = buf.len() as u64 > max_bytes;
    if truncated {
        buf.truncate(max_bytes as usize);
    }
    Some((String::from_utf8_lossy(&buf).to_string(), truncated))
}

fn search_session_log_file(
    path: &Path,
    query: &str,
    terms: &[String],
    mode: SessionLogSearchMode,
) -> Option<(usize, Vec<serde_json::Value>, bool)> {
    let (contents, truncated) = read_text_prefix(path, SESSION_LOG_SEARCH_READ_LIMIT)?;
    let (matches, snippets) = search_session_log_text(&contents, query, terms, mode);
    Some((matches, snippets, truncated))
}

fn normalize_session_source_filter(source_filter: &str) -> String {
    let value = source_filter.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "all" => "all".to_string(),
        "external" => "external".to_string(),
        "intendant" | "codex" | "claude-code" | "gemini" => value,
        "claude" => "claude-code".to_string(),
        _ => "all".to_string(),
    }
}

fn session_source_matches_filter(source: &str, source_filter: &str) -> bool {
    match source_filter {
        "all" => true,
        "external" => source != "intendant",
        "claude" | "claude-code" => source == "claude-code",
        other => source == other,
    }
}

fn session_log_search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| term.trim().to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionLogSearchMode {
    AllKeywords,
    ExactPhrase,
    AnyKeywordSession,
    UserMessageAllKeywords,
}

impl SessionLogSearchMode {
    fn from_query(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "exact" | "exact_phrase" | "phrase" => Self::ExactPhrase,
            "any" | "any_keyword" | "any_keyword_session" => Self::AnyKeywordSession,
            "user" | "user_message" | "user_message_all_keywords" => Self::UserMessageAllKeywords,
            _ => Self::AllKeywords,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AllKeywords => "all_keywords",
            Self::ExactPhrase => "exact_phrase",
            Self::AnyKeywordSession => "any_keyword_session",
            Self::UserMessageAllKeywords => "user_message_all_keywords",
        }
    }

    fn has_search_input(self, query: &str, terms: &[String]) -> bool {
        match self {
            Self::ExactPhrase => !query.trim().is_empty(),
            _ => !terms.is_empty(),
        }
    }
}

fn search_session_log_text(
    text: &str,
    query: &str,
    terms: &[String],
    mode: SessionLogSearchMode,
) -> (usize, Vec<serde_json::Value>) {
    let candidates: Vec<SessionLogSearchCandidate> = text
        .lines()
        .filter_map(session_log_search_candidate_from_line)
        .collect();
    if mode == SessionLogSearchMode::AnyKeywordSession
        && !candidates
            .iter()
            .any(|candidate| text_matches_any_session_term(&candidate.text, terms))
    {
        return (0, Vec::new());
    }

    let mut matches = 0usize;
    let mut snippets = Vec::new();
    let snippet_needles = if mode == SessionLogSearchMode::ExactPhrase {
        vec![query.trim().to_ascii_lowercase()]
    } else {
        terms.to_vec()
    };

    for candidate in candidates {
        if candidate.text.trim().is_empty()
            || !session_log_candidate_matches(&candidate, query, terms, mode)
        {
            continue;
        }
        matches += 1;
        if snippets.len() < SESSION_LOG_SEARCH_SNIPPETS_PER_SESSION {
            snippets.push(serde_json::json!({
                "ts": candidate.ts,
                "source": candidate.source,
                "level": candidate.level,
                "event": candidate.event,
                "content": session_log_match_snippet(
                    &candidate.text,
                    &snippet_needles,
                    SESSION_LOG_SEARCH_SNIPPET_CHARS
                ),
            }));
        }
    }

    (matches, snippets)
}

fn session_log_candidate_matches(
    candidate: &SessionLogSearchCandidate,
    query: &str,
    terms: &[String],
    mode: SessionLogSearchMode,
) -> bool {
    match mode {
        SessionLogSearchMode::AllKeywords => text_matches_session_terms(&candidate.text, terms),
        SessionLogSearchMode::ExactPhrase => text_contains_session_phrase(&candidate.text, query),
        SessionLogSearchMode::AnyKeywordSession => {
            text_matches_any_session_term(&candidate.text, terms)
        }
        SessionLogSearchMode::UserMessageAllKeywords => {
            candidate.is_user && text_matches_session_terms(&candidate.text, terms)
        }
    }
}

struct SessionLogSearchCandidate {
    ts: String,
    source: String,
    level: String,
    event: String,
    text: String,
    is_user: bool,
}

fn session_log_search_candidate_from_line(line: &str) -> Option<SessionLogSearchCandidate> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Some(SessionLogSearchCandidate {
            ts: String::new(),
            source: String::new(),
            level: String::new(),
            event: String::new(),
            text: trimmed.to_string(),
            is_user: false,
        });
    };

    let mut parts = Vec::new();
    collect_session_log_search_strings(&value, &mut parts);
    let text = if parts.is_empty() {
        trimmed.to_string()
    } else {
        parts.join("\n")
    };

    Some(SessionLogSearchCandidate {
        ts: value
            .get("ts")
            .or_else(|| value.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        source: value
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        level: value
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        event: value
            .get("event")
            .or_else(|| value.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        text,
        is_user: session_log_json_is_user_message(&value),
    })
}

fn session_log_json_is_user_message(value: &serde_json::Value) -> bool {
    [
        value.get("source"),
        value.get("role"),
        value.get("type"),
        value.pointer("/payload/source"),
        value.pointer("/payload/role"),
        value.pointer("/payload/type"),
        value.pointer("/message/role"),
        value.pointer("/message/type"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| v.as_str())
    .any(|value| matches!(value.to_ascii_lowercase().as_str(), "user" | "user_message"))
}

fn collect_session_log_search_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value) => {
            if value.trim().is_empty() {
                return;
            }
            out.push(compact_text(value, SESSION_LOG_SEARCH_FIELD_CHARS));
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_session_log_search_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_session_log_search_strings(value, out);
            }
        }
        _ => {}
    }
}

fn text_matches_session_terms(text: &str, terms: &[String]) -> bool {
    let haystack = text.to_ascii_lowercase();
    terms.iter().all(|term| haystack.contains(term))
}

fn text_matches_any_session_term(text: &str, terms: &[String]) -> bool {
    let haystack = text.to_ascii_lowercase();
    terms.iter().any(|term| haystack.contains(term))
}

fn text_contains_session_phrase(text: &str, phrase: &str) -> bool {
    let phrase = phrase.trim().to_ascii_lowercase();
    !phrase.is_empty() && text.to_ascii_lowercase().contains(&phrase)
}

fn session_log_match_snippet(text: &str, terms: &[String], max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let total_chars = compact.chars().count();
    if total_chars <= max_chars {
        return compact;
    }

    let lower = compact.to_ascii_lowercase();
    let match_byte = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let match_char = compact[..match_byte].chars().count();
    let start_char = match_char.saturating_sub(max_chars / 3);
    let end_char = (start_char + max_chars).min(total_chars);
    let mut snippet: String = compact
        .chars()
        .skip(start_char)
        .take(end_char - start_char)
        .collect();
    if start_char > 0 {
        snippet.insert_str(0, "...");
    }
    if end_char < total_chars {
        snippet.push_str("...");
    }
    snippet
}

fn value_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn codex_exec_command_workdir(payload: &serde_json::Value) -> Option<String> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call")
        || payload.get("name").and_then(|v| v.as_str()) != Some("exec_command")
    {
        return None;
    }

    let arguments = payload.get("arguments")?;
    let parsed_arguments;
    let arguments = if let Some(raw) = arguments.as_str() {
        parsed_arguments = serde_json::from_str::<serde_json::Value>(raw).ok()?;
        &parsed_arguments
    } else {
        arguments
    };

    value_str(arguments, "workdir")
        .or_else(|| value_str(arguments, "cwd"))
        .filter(|value| !value.trim().is_empty())
}

fn compact_text(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut out = one_line
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>();
        out.push_str("...");
        out
    }
}

fn preview_text(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

fn message_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("content").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct ExternalJsonLineKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    payload: Option<ExternalJsonPayloadKind<'a>>,
}

#[derive(Deserialize)]
struct ExternalJsonPayloadKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
}

fn codex_line_may_affect_replay(line: &str) -> bool {
    let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(line) else {
        return true;
    };
    let payload_kind = kind
        .payload
        .as_ref()
        .and_then(|payload| payload.kind.as_deref());
    match (kind.kind.as_deref(), payload_kind) {
        (_, Some("thread_rolled_back" | "user_message" | "agent_message" | "message")) => true,
        (Some("event_msg" | "response_item"), None) => true,
        (None, _) => true,
        _ => false,
    }
}

fn codex_payload_text(payload: &serde_json::Value) -> Option<(String, String)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    let role = payload
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("message")
        .to_string();
    let content = payload.get("content")?;
    message_content_text(content).map(|text| (role, text))
}

fn codex_event_message_text(payload: &serde_json::Value) -> Option<(String, String)> {
    match payload.get("type").and_then(|v| v.as_str())? {
        "user_message" => value_str(payload, "message").map(|text| ("user".to_string(), text)),
        "agent_message" => {
            value_str(payload, "message").map(|text| ("assistant".to_string(), text))
        }
        _ => None,
    }
}

fn is_codex_injected_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions for ") || trimmed.starts_with("<turn_aborted>")
}

fn codex_thread_display_name(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| !is_codex_injected_user_text(s))
        .map(|s| compact_text(&s, 180))
}

fn push_external_transcript_entry(
    entries: &mut Vec<serde_json::Value>,
    seen_messages: &mut HashSet<(String, String)>,
    provider_source: &str,
    ts: &str,
    role: &str,
    text: String,
) -> bool {
    let role = match role.trim().to_lowercase().as_str() {
        "model" => "assistant".to_string(),
        other => other.to_string(),
    };
    if role != "user" && role != "assistant" {
        return false;
    }
    if text.trim().is_empty() {
        return false;
    }
    if role == "user" && is_codex_injected_user_text(&text) {
        return false;
    }
    if !seen_messages.insert((role.clone(), text.clone())) {
        return false;
    }
    entries.push(serde_json::json!({
        "ts": ts,
        "level": if role == "assistant" || role == "model" { "model" } else { "info" },
        "source": external_transcript_source(provider_source, &role),
        "content": text,
    }));
    true
}

fn external_transcript_entry_role(entry: &serde_json::Value) -> Option<&'static str> {
    if entry.get("source").and_then(|v| v.as_str()) == Some("user") {
        Some("user")
    } else if entry.get("level").and_then(|v| v.as_str()) == Some("model") {
        Some("assistant")
    } else {
        None
    }
}

fn forget_external_seen_message(
    seen_messages: &mut HashSet<(String, String)>,
    entry: &serde_json::Value,
) {
    let Some(role) = external_transcript_entry_role(entry) else {
        return;
    };
    let Some(content) = entry.get("content").and_then(|v| v.as_str()) else {
        return;
    };
    seen_messages.remove(&(role.to_string(), content.to_string()));
}

fn mark_latest_external_turn_superseded(
    entries: &mut [serde_json::Value],
    seen_messages: &mut HashSet<(String, String)>,
    rollback_ts: &str,
) -> Option<u32> {
    for idx in (0..entries.len()).rev() {
        let entry = &entries[idx];
        if entry
            .get("superseded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if entry.get("kind").and_then(|v| v.as_str()) == Some("rollback_marker") {
            continue;
        }
        let Some(role) = external_transcript_entry_role(entry) else {
            continue;
        };
        let user_turn_index = entry
            .get("user_turn_index")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        forget_external_seen_message(seen_messages, entry);
        if let Some(obj) = entries[idx].as_object_mut() {
            obj.insert("superseded".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "superseded_at".to_string(),
                serde_json::Value::String(rollback_ts.to_string()),
            );
            obj.insert(
                "superseded_reason".to_string(),
                serde_json::Value::String("thread_rollback".to_string()),
            );
        }
        if role == "user" {
            return user_turn_index;
        }
    }
    None
}

fn collect_files(root: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(suffix))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn file_mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|m| metadata_mtime_secs(&m))
        .unwrap_or(0)
}

fn metadata_mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn metadata_mtime_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn collect_recent_files(root: &Path, suffix: &str, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, suffix, &mut files);
    let mut seen = HashSet::new();
    files.retain(|path| {
        std::fs::canonicalize(path)
            .map(|canonical| seen.insert(canonical))
            .unwrap_or(true)
    });
    files.sort_by(|a, b| file_mtime_secs(b).cmp(&file_mtime_secs(a)));
    files.truncate(limit);
    files
}

fn derive_project_root_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim();
    if cwd.is_empty() {
        return None;
    }

    let mut current = PathBuf::from(cwd);
    if !current.is_absolute() {
        return Some(cwd.to_string());
    }
    if current.is_file() {
        current.pop();
    }

    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().to_string());
        }
        if !current.pop() {
            break;
        }
    }

    Some(cwd.to_string())
}

fn read_text_head_tail(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok().map(|m| m.len()).unwrap_or(0);
    if len <= head_bytes.saturating_add(tail_bytes) {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        return Some(String::from_utf8_lossy(&buf).to_string());
    }

    let mut head = vec![0; head_bytes as usize];
    let head_len = file.read(&mut head).ok()?;
    head.truncate(head_len);

    file.seek(SeekFrom::End(-(tail_bytes as i64))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;

    let mut out = String::from_utf8_lossy(&head).to_string();
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&tail));
    Some(out)
}

fn file_mtime_string(path: &Path) -> Option<String> {
    mtime_secs_to_string(file_mtime_secs(path))
}

fn mtime_secs_to_string(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let dt: chrono::DateTime<chrono::Local> = t.into();
    Some(dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SessionUsage {
    total_tokens: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_creation_tokens: u64,
    cached_tokens: u64,
}

impl SessionUsage {
    fn is_empty(self) -> bool {
        self.total_tokens == 0
            && self.prompt_tokens == 0
            && self.completion_tokens == 0
            && self.cache_creation_tokens == 0
            && self.cached_tokens == 0
    }

    fn add(&mut self, other: SessionUsage) {
        self.total_tokens += other.total_tokens;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.cached_tokens += other.cached_tokens;
    }
}

fn value_u64_at(value: &serde_json::Value, paths: &[&str]) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| value.pointer(path).and_then(|v| v.as_u64()))
}

fn apply_session_usage(session: &mut serde_json::Value, usage: SessionUsage, model: Option<&str>) {
    if usage.is_empty() {
        return;
    }
    let estimated_cost = model.and_then(|m| {
        crate::app_state_pricing::estimate_session_cost(
            m,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cached_tokens,
            usage.cache_creation_tokens,
        )
    });
    if let Some(obj) = session.as_object_mut() {
        obj.insert(
            "total_tokens".to_string(),
            serde_json::json!(usage.total_tokens),
        );
        obj.insert(
            "prompt_tokens".to_string(),
            serde_json::json!(usage.prompt_tokens),
        );
        obj.insert(
            "completion_tokens".to_string(),
            serde_json::json!(usage.completion_tokens),
        );
        obj.insert(
            "cached_tokens".to_string(),
            serde_json::json!(usage.cached_tokens),
        );
        obj.insert(
            "cache_creation_tokens".to_string(),
            serde_json::json!(usage.cache_creation_tokens),
        );
        obj.insert(
            "estimated_cost".to_string(),
            serde_json::json!(estimated_cost.unwrap_or(0.0)),
        );
        obj.insert(
            "pricing_known".to_string(),
            serde_json::json!(estimated_cost.is_some()),
        );
    }
}

fn session_usage_from_json(session: &serde_json::Value) -> SessionUsage {
    SessionUsage {
        total_tokens: session
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_tokens: session
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        completion_tokens: session
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_tokens: session
            .get("cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_tokens: session
            .get("cache_creation_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}

fn apply_session_model_and_reprice(session: &mut serde_json::Value, model: &str) {
    if let Some(obj) = session.as_object_mut() {
        obj.insert("model".to_string(), serde_json::json!(model));
    }
    apply_session_usage(session, session_usage_from_json(session), Some(model));
}

fn external_session_json(
    source: &str,
    label: &str,
    session_id: String,
    resume_id: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    name: Option<String>,
    task: Option<String>,
    provider: &str,
    model: Option<String>,
    turns: u64,
    project_root: Option<String>,
    cwd: Option<String>,
    path: Option<String>,
    bytes: u64,
) -> serde_json::Value {
    let created_at = created_at.unwrap_or_default();
    let updated_at = updated_at.unwrap_or_else(|| created_at.clone());
    let cwd = cwd.or_else(|| project_root.clone());
    serde_json::json!({
        "source": source,
        "source_label": label,
        "session_id": session_id,
        "resume_id": resume_id,
        "created_at": created_at,
        "updated_at": updated_at,
        "name": name,
        "task": task,
        "provider": provider,
        "model": model,
        "turns": turns,
        "status": "external",
        "total_tokens": 0,
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "cached_tokens": 0,
        "cache_creation_tokens": 0,
        "estimated_cost": 0.0,
        "pricing_known": false,
        "role": null,
        "recordings": 0,
        "recording_bytes": 0,
        "annotations": 0,
        "clips": 0,
        "frames_bytes": 0,
        "turns_bytes": bytes,
        "logs_bytes": bytes,
        "total_bytes": bytes,
        "cwd": cwd,
        "project_root": project_root,
        "path": path,
        "can_delete": false,
        "can_resume": true,
    })
}

fn timestamp_sort_secs(value: &str) -> i64 {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return dt.timestamp();
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, format) {
            if let Some(dt) = dt.and_local_timezone(chrono::Local).single() {
                return dt.timestamp();
            }
        }
    }
    0
}

fn session_created_sort_key(session: &serde_json::Value) -> i64 {
    session
        .get("created_at")
        .and_then(|v| v.as_str())
        .map(timestamp_sort_secs)
        .unwrap_or(0)
}

fn session_changed_sort_key(session: &serde_json::Value) -> i64 {
    session
        .get("updated_at")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(timestamp_sort_secs)
        .unwrap_or_else(|| session_created_sort_key(session))
}

fn sort_sessions_newest_first(sessions: &mut Vec<serde_json::Value>) {
    sessions.sort_by(|a, b| session_changed_sort_key(b).cmp(&session_changed_sort_key(a)));
}

fn session_source(session: &serde_json::Value) -> &str {
    session
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("intendant")
}

fn session_unique_key(session: &serde_json::Value) -> String {
    let source = session_source(session);
    let session_id = session
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    format!("{source}:{session_id}")
}

fn push_unique_session(
    out: &mut Vec<serde_json::Value>,
    seen: &mut HashSet<String>,
    session: &serde_json::Value,
) {
    if seen.insert(session_unique_key(session)) {
        out.push(session.clone());
    }
}

fn truncate_sessions_preserving_sources(sessions: &mut Vec<serde_json::Value>) {
    if sessions.len() <= SESSION_LIST_LIMIT {
        return;
    }

    let mut out = Vec::with_capacity(SESSION_LIST_LIMIT);
    let mut seen = HashSet::new();
    for source in ["intendant", "codex", "claude-code", "gemini"] {
        for session in sessions
            .iter()
            .filter(|session| session_source(session) == source)
            .take(SESSION_SOURCE_FLOOR)
        {
            push_unique_session(&mut out, &mut seen, session);
        }
    }

    for session in sessions.iter() {
        if out.len() >= SESSION_LIST_LIMIT {
            break;
        }
        push_unique_session(&mut out, &mut seen, session);
    }

    sort_sessions_newest_first(&mut out);
    *sessions = out;
}

fn codex_usage_bucket<'a>(
    value: &'a serde_json::Value,
    names: &[&str],
) -> Option<&'a serde_json::Value> {
    for name in names {
        if let Some(v) = value.get(*name) {
            return Some(v);
        }
        if let Some(info) = value.get("info") {
            if let Some(v) = info.get(*name) {
                return Some(v);
            }
        }
    }
    None
}

fn codex_session_usage_from_payload(payload: &serde_json::Value) -> Option<SessionUsage> {
    let info = payload
        .get("info")
        .or_else(|| payload.get("tokenUsage"))
        .unwrap_or(payload);
    if info.is_null() {
        return None;
    }
    let total = codex_usage_bucket(info, &["total_token_usage", "total"]).unwrap_or(info);
    let prompt_tokens = value_u64_at(total, &["/input_tokens", "/inputTokens"])?;
    let completion_tokens = value_u64_at(total, &["/output_tokens", "/outputTokens"]).unwrap_or(0);
    let cached_tokens = value_u64_at(
        total,
        &[
            "/cached_input_tokens",
            "/cachedInputTokens",
            "/cached_tokens",
            "/cachedTokens",
        ],
    )
    .unwrap_or(0);
    let total_tokens = value_u64_at(total, &["/total_tokens", "/totalTokens"])
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    Some(SessionUsage {
        total_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: 0,
        cached_tokens,
    })
}

fn claude_usage_from_message_usage(usage: &serde_json::Value) -> Option<SessionUsage> {
    let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64())?;
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let prompt_tokens = input_tokens + cache_creation + cache_read;
    Some(SessionUsage {
        total_tokens: prompt_tokens + completion_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: cache_creation,
        cached_tokens: cache_read,
    })
}

fn gemini_usage_from_tokens(tokens: &serde_json::Value) -> Option<SessionUsage> {
    let prompt_tokens = value_u64_at(
        tokens,
        &[
            "/input",
            "/input_tokens",
            "/inputTokens",
            "/prompt",
            "/prompt_tokens",
            "/promptTokens",
        ],
    )?;
    let output_tokens = value_u64_at(
        tokens,
        &[
            "/output",
            "/output_tokens",
            "/outputTokens",
            "/completion",
            "/completion_tokens",
            "/completionTokens",
        ],
    )
    .unwrap_or(0);
    let thinking_tokens = value_u64_at(
        tokens,
        &[
            "/thoughts",
            "/thought_tokens",
            "/thoughtTokens",
            "/thinking",
            "/thinking_tokens",
            "/thinkingTokens",
        ],
    )
    .unwrap_or(0);
    let tool_tokens = value_u64_at(tokens, &["/tool", "/tool_tokens", "/toolTokens"]).unwrap_or(0);
    let cached_tokens = value_u64_at(
        tokens,
        &[
            "/cached",
            "/cached_tokens",
            "/cachedTokens",
            "/cached_input_tokens",
            "/cachedInputTokens",
        ],
    )
    .unwrap_or(0);
    let completion_tokens = output_tokens + thinking_tokens + tool_tokens;
    let total_tokens = value_u64_at(tokens, &["/total", "/total_tokens", "/totalTokens"])
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    Some(SessionUsage {
        total_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: 0,
        cached_tokens,
    })
}

fn resolve_codex_inherited_model(
    session_id: &str,
    model_by_id: &HashMap<String, String>,
    parent_by_id: &HashMap<String, String>,
) -> Option<String> {
    let mut seen = HashSet::new();
    let mut current = session_id.to_string();
    while seen.insert(current.clone()) {
        let parent = parent_by_id.get(&current)?;
        if let Some(model) = model_by_id.get(parent) {
            return Some(model.clone());
        }
        current = parent.clone();
    }
    None
}

fn list_codex_sessions(home: &Path) -> Vec<serde_json::Value> {
    let mut rows: HashMap<String, serde_json::Value> = HashMap::new();
    let mut model_by_id: HashMap<String, String> = HashMap::new();
    let mut parent_by_id: HashMap<String, String> = HashMap::new();
    let index_path = home.join(".codex").join("session_index.jsonl");
    if let Ok(contents) = std::fs::read_to_string(&index_path) {
        for line in contents.lines() {
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(id) = value_str(&obj, "id") else {
                continue;
            };
            let updated_at = value_str(&obj, "updated_at");
            let name = codex_thread_display_name(value_str(&obj, "thread_name"));
            rows.insert(
                id.clone(),
                external_session_json(
                    "codex",
                    "Codex",
                    id.clone(),
                    id,
                    None,
                    updated_at,
                    name,
                    None,
                    "Codex",
                    None,
                    0,
                    None,
                    None,
                    Some(index_path.to_string_lossy().to_string()),
                    file_size(&index_path),
                ),
            );
        }
    }

    let mut files = collect_recent_files(
        &home.join(".codex").join("sessions"),
        ".jsonl",
        EXTERNAL_SESSION_SCAN_LIMIT,
    );
    files.extend(collect_recent_files(
        &home.join(".codex").join("archived_sessions"),
        ".jsonl",
        EXTERNAL_SESSION_SCAN_LIMIT,
    ));
    files.sort_by(|a, b| file_mtime_secs(b).cmp(&file_mtime_secs(a)));
    files.truncate(EXTERNAL_SESSION_SCAN_LIMIT);
    for path in files {
        let Some(contents) = read_text_head_tail(
            &path,
            EXTERNAL_SESSION_READ_LIMIT,
            EXTERNAL_SESSION_READ_LIMIT,
        ) else {
            continue;
        };
        let mut id = None;
        let mut created_at = None;
        let mut session_cwd = None;
        let mut turn_cwd = None;
        let mut command_cwd = None;
        let mut model = None;
        let mut forked_from_id = None;
        let mut provider = Some("Codex".to_string());
        let mut usage = SessionUsage::default();
        let mut task_started_turns = 0u64;
        let mut saw_user_message_event = false;
        let mut event_user_turns: Vec<Option<String>> = Vec::new();
        let mut fallback_user_turns: Vec<Option<String>> = Vec::new();
        for line in contents.lines() {
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "session_meta" => {
                    if let Some(payload) = obj.get("payload") {
                        id = id.or_else(|| value_str(payload, "id"));
                        forked_from_id =
                            forked_from_id.or_else(|| value_str(payload, "forked_from_id"));
                        created_at = created_at.or_else(|| value_str(payload, "timestamp"));
                        if let Some(value) = value_str(payload, "cwd") {
                            if session_cwd.is_none() {
                                session_cwd = Some(value);
                            }
                        }
                        model = model.or_else(|| value_str(payload, "model"));
                        provider = value_str(payload, "model_provider").or(provider);
                    }
                }
                "turn_context" => {
                    if let Some(payload) = obj.get("payload") {
                        if let Some(value) = value_str(payload, "cwd") {
                            if session_cwd.is_none() {
                                session_cwd = Some(value.clone());
                            }
                            turn_cwd = Some(value);
                        }
                        model = model.or_else(|| value_str(payload, "model"));
                    }
                }
                "event_msg" => {
                    if let Some(payload) = obj.get("payload") {
                        let payload_type =
                            payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if payload_type.starts_with("exec_command") {
                            if let Some(value) = value_str(payload, "cwd") {
                                command_cwd = Some(value);
                            }
                        }
                        match payload_type {
                            "task_started" => {
                                task_started_turns += 1;
                            }
                            "token_count" => {
                                if let Some(parsed) = codex_session_usage_from_payload(payload) {
                                    usage = parsed;
                                }
                            }
                            "user_message" => {
                                saw_user_message_event = true;
                                let text = value_str(payload, "message")
                                    .filter(|s| !s.trim().is_empty())
                                    .map(|s| compact_text(&s, 180));
                                event_user_turns.push(text);
                            }
                            "thread_rolled_back" => {
                                let num_turns = payload
                                    .get("num_turns")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                for _ in 0..num_turns {
                                    let _ = event_user_turns.pop();
                                    let _ = fallback_user_turns.pop();
                                }
                                task_started_turns = task_started_turns.saturating_sub(num_turns);
                            }
                            _ => {}
                        }
                    }
                }
                "response_item" => {
                    if let Some(payload) = obj.get("payload") {
                        if let Some(value) = codex_exec_command_workdir(payload) {
                            command_cwd = Some(value);
                        }
                        if let Some((role, text)) = codex_payload_text(payload) {
                            if role == "user" && !is_codex_injected_user_text(&text) {
                                fallback_user_turns.push(Some(compact_text(&text, 180)));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(id) = id else {
            continue;
        };
        if let Some(model) = model.clone() {
            model_by_id.insert(id.clone(), model);
        }
        if let Some(parent_id) = forked_from_id {
            parent_by_id.insert(id.clone(), parent_id);
        }
        let existing = rows.get(&id);
        let existing_task = existing
            .and_then(|v| value_str(v, "task"))
            .filter(|s| !is_codex_injected_user_text(s));
        let existing_name = existing.and_then(|v| value_str(v, "name"));
        let existing_updated_at = existing.and_then(|v| value_str(v, "updated_at"));
        let file_updated_at = file_mtime_string(&path);
        let created_at = created_at.or_else(|| file_updated_at.clone());
        let updated_at = file_updated_at
            .or(existing_updated_at)
            .or_else(|| created_at.clone());
        let task = event_user_turns
            .iter()
            .find_map(|t| t.clone())
            .or_else(|| fallback_user_turns.iter().find_map(|t| t.clone()));
        let turns = if saw_user_message_event {
            event_user_turns.len() as u64
        } else if task_started_turns > 0 {
            task_started_turns
        } else if !fallback_user_turns.is_empty() {
            fallback_user_turns.len() as u64
        } else {
            0
        };
        let effective_cwd = command_cwd.or(turn_cwd).or_else(|| session_cwd.clone());
        let project_root =
            derive_project_root_from_cwd(session_cwd.as_deref().or(effective_cwd.as_deref()));
        let mut session = external_session_json(
            "codex",
            "Codex",
            id.clone(),
            id.clone(),
            created_at,
            updated_at,
            existing_name,
            task.or(existing_task),
            provider.as_deref().unwrap_or("Codex"),
            model.clone(),
            turns,
            project_root,
            effective_cwd,
            Some(path.to_string_lossy().to_string()),
            file_size(&path),
        );
        apply_session_usage(&mut session, usage, model.as_deref());
        rows.insert(id, session);
    }

    let ids_missing_model = rows
        .iter()
        .filter_map(|(id, session)| {
            if value_str(session, "model").is_none() {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for id in ids_missing_model {
        let Some(model) = resolve_codex_inherited_model(&id, &model_by_id, &parent_by_id) else {
            continue;
        };
        if let Some(session) = rows.get_mut(&id) {
            apply_session_model_and_reprice(session, &model);
        }
    }

    rows.into_values().collect()
}

fn list_claude_sessions(home: &Path) -> Vec<serde_json::Value> {
    let files = collect_recent_files(
        &home.join(".claude").join("projects"),
        ".jsonl",
        EXTERNAL_SESSION_SCAN_LIMIT,
    );
    let mut rows = Vec::new();
    for path in files {
        let session_id = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if session_id.is_empty() {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        let mut created_at = None;
        let mut updated_at = None;
        let mut session_cwd = None;
        let mut cwd = None;
        let mut task = None;
        let mut model = None;
        let mut usage = SessionUsage::default();
        let mut seen_usage = HashSet::new();
        let mut turns = 0u64;
        for (line_idx, line_result) in reader.lines().enumerate() {
            let Ok(line) = line_result else {
                continue;
            };
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            created_at = created_at.or_else(|| value_str(&obj, "timestamp"));
            updated_at = value_str(&obj, "timestamp").or(updated_at);
            if let Some(value) = value_str(&obj, "cwd") {
                if session_cwd.is_none() {
                    session_cwd = Some(value.clone());
                }
                cwd = Some(value);
            }
            if obj.get("type").and_then(|v| v.as_str()) == Some("user") {
                turns += 1;
                if task.is_none() {
                    if let Some(msg) = obj.get("message") {
                        if let Some(content) = msg.get("content").and_then(message_content_text) {
                            task = Some(compact_text(&content, 180));
                        }
                    }
                }
            }
            if let Some(msg) = obj.get("message") {
                if model.is_none() {
                    model = msg
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if let Some(parsed) = msg.get("usage").and_then(claude_usage_from_message_usage) {
                    let key = value_str(&obj, "requestId")
                        .or_else(|| value_str(msg, "id"))
                        .unwrap_or_else(|| format!("line-{line_idx}"));
                    if seen_usage.insert(key) {
                        usage.add(parsed);
                    }
                }
            }
        }
        let effective_cwd = cwd.or_else(|| session_cwd.clone());
        let project_root =
            derive_project_root_from_cwd(session_cwd.as_deref().or(effective_cwd.as_deref()));
        let mut session = external_session_json(
            "claude-code",
            "Claude Code",
            session_id.clone(),
            session_id,
            created_at
                .or_else(|| updated_at.clone())
                .or_else(|| file_mtime_string(&path)),
            file_mtime_string(&path).or(updated_at),
            None,
            task,
            "Claude Code",
            model.clone(),
            turns,
            project_root,
            effective_cwd,
            Some(path.to_string_lossy().to_string()),
            file_size(&path),
        );
        apply_session_usage(&mut session, usage, model.as_deref());
        rows.push(session);
    }
    rows
}

fn gemini_project_roots(home: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let path = home.join(".gemini").join("projects.json");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return out;
    };
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return out;
    };
    let Some(projects) = obj.get("projects").and_then(|v| v.as_object()) else {
        return out;
    };
    for (root, alias) in projects {
        if let Some(alias) = alias.as_str() {
            out.insert(alias.to_string(), root.to_string());
        }
    }
    out
}

fn list_gemini_sessions(home: &Path) -> Vec<serde_json::Value> {
    let roots = gemini_project_roots(home);
    let files = collect_recent_files(
        &home.join(".gemini").join("tmp"),
        ".json",
        EXTERNAL_SESSION_SCAN_LIMIT,
    );
    let mut rows = Vec::new();
    for path in files {
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            != Some("chats")
        {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(&contents) else {
            continue;
        };
        let Some(session_id) = value_str(&obj, "sessionId") else {
            continue;
        };
        let alias = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let mut task = None;
        let mut turns = 0u64;
        let mut model = value_str(&obj, "model");
        let mut usage = SessionUsage::default();
        if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
            for msg in messages {
                model = model.or_else(|| value_str(msg, "model"));
                if let Some(parsed) = msg.get("tokens").and_then(gemini_usage_from_tokens) {
                    usage.add(parsed);
                }
                let role = msg
                    .get("role")
                    .or_else(|| msg.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if role == "user" {
                    turns += 1;
                    if task.is_none() {
                        let text = msg
                            .get("text")
                            .or_else(|| msg.get("message"))
                            .or_else(|| msg.get("content"))
                            .and_then(message_content_text);
                        if let Some(text) = text {
                            task = Some(compact_text(&text, 180));
                        }
                    }
                }
            }
        }
        let project_root = alias.as_ref().and_then(|a| roots.get(a).cloned());
        let cwd = project_root.clone();
        let mut session = external_session_json(
            "gemini",
            "Gemini CLI",
            session_id.clone(),
            session_id,
            value_str(&obj, "startTime").or_else(|| file_mtime_string(&path)),
            file_mtime_string(&path),
            None,
            task,
            "Gemini CLI",
            model.clone(),
            turns,
            project_root,
            cwd,
            Some(path.to_string_lossy().to_string()),
            file_size(&path),
        );
        apply_session_usage(&mut session, usage, model.as_deref());
        rows.push(session);
    }
    rows
}

fn codex_session_file_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).ok()?;
        if bytes == 0 {
            return None;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return obj
                .get("payload")
                .and_then(|payload| value_str(payload, "id"));
        }
    }
}

fn find_codex_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(&home.join(".codex").join("sessions"), ".jsonl", &mut files);
    collect_files(
        &home.join(".codex").join("archived_sessions"),
        ".jsonl",
        &mut files,
    );

    files
        .into_iter()
        .find(|path| codex_session_file_id(path).as_deref() == Some(session_id))
}

fn external_session_detail(source: &str, session_id: &str) -> Option<String> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    external_session_detail_from_home(&home, source, session_id)
}

fn external_session_detail_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<String> {
    let entries = external_session_entries_from_home(home, source, session_id)?;

    Some(
        serde_json::json!({
            "session_id": session_id,
            "transcript_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "entries": entries,
            "frames": [],
        })
        .to_string(),
    )
}

fn external_transcript_source(provider_source: &str, role: &str) -> String {
    let role = role.trim().to_lowercase();
    if role == "user" {
        "user".to_string()
    } else {
        provider_source.to_string()
    }
}

fn external_transcript_cache() -> &'static Mutex<HashMap<String, ExternalTranscriptCacheEntry>> {
    EXTERNAL_TRANSCRIPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn external_transcript_path_key(path: &Path) -> String {
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalized.to_string_lossy().to_string()
}

fn external_transcript_cache_key(
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<ExternalTranscriptCacheKey> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(ExternalTranscriptCacheKey {
        source: source.to_string(),
        session_id: session_id.to_string(),
        path: external_transcript_path_key(path),
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(&metadata),
    })
}

fn external_transcript_cache_slot(key: &ExternalTranscriptCacheKey) -> String {
    format!("{}\0{}\0{}", key.source, key.session_id, key.path)
}

fn cached_external_transcript_entries(
    key: &ExternalTranscriptCacheKey,
) -> Option<Vec<serde_json::Value>> {
    let slot = external_transcript_cache_slot(key);
    let cache = external_transcript_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .get(&slot)
        .filter(|entry| &entry.key == key)
        .map(|entry| entry.entries.clone())
}

fn store_external_transcript_entries(
    key: ExternalTranscriptCacheKey,
    entries: &[serde_json::Value],
) {
    let slot = external_transcript_cache_slot(&key);
    let mut cache = external_transcript_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= EXTERNAL_TRANSCRIPT_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        ExternalTranscriptCacheEntry {
            key,
            entries: entries.to_vec(),
        },
    );
}

fn push_codex_transcript_message(
    entries: &mut Vec<serde_json::Value>,
    seen_messages: &mut HashSet<(String, String)>,
    user_turn_index: &mut u32,
    pending_replacement_for_user_turn: &mut Option<u32>,
    ts: &str,
    role: &str,
    text: String,
) {
    if push_external_transcript_entry(entries, seen_messages, "codex", ts, role, text)
        && role == "user"
    {
        *user_turn_index = user_turn_index.saturating_add(1);
        if let Some(entry) = entries.last_mut() {
            entry["user_turn_index"] = serde_json::json!(*user_turn_index);
            if let Some(turn) = pending_replacement_for_user_turn.take() {
                entry["replacement_for_user_turn_index"] = serde_json::json!(turn);
            }
        }
    }
}

fn parse_codex_session_entries(path: &Path) -> Option<Vec<serde_json::Value>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut user_turn_index = 0u32;
    let mut pending_replacement_for_user_turn: Option<u32> = None;
    let mut seen_messages = HashSet::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || !codex_line_may_affect_replay(trimmed) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
            if let Some(payload) = obj.get("payload") {
                if payload.get("type").and_then(|v| v.as_str()) == Some("thread_rolled_back") {
                    let turns = payload
                        .get("num_turns")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let ts = value_str(&obj, "timestamp").unwrap_or_default();
                    let mut superseded_user_turns = Vec::new();
                    for _ in 0..turns {
                        if let Some(turn) = mark_latest_external_turn_superseded(
                            &mut entries,
                            &mut seen_messages,
                            &ts,
                        ) {
                            superseded_user_turns.push(turn);
                            user_turn_index = user_turn_index.saturating_sub(1);
                        }
                    }
                    if let Some(replacement_turn) = superseded_user_turns.iter().copied().min() {
                        pending_replacement_for_user_turn = Some(replacement_turn);
                    }
                    if turns > 0 {
                        entries.push(serde_json::json!({
                            "ts": ts,
                            "level": "warn",
                            "source": "system",
                            "content": if turns == 1 {
                                "Rewound 1 user turn; overwritten entries are no longer active context.".to_string()
                            } else {
                                format!("Rewound {turns} user turns; overwritten entries are no longer active context.")
                            },
                            "kind": "rollback_marker",
                            "rollback_turns": turns,
                        }));
                    }
                    continue;
                }
            }
        }
        let ts = value_str(&obj, "timestamp").unwrap_or_default();
        if let Some(payload) = obj.get("payload") {
            if let Some((role, text)) = codex_event_message_text(payload) {
                push_codex_transcript_message(
                    &mut entries,
                    &mut seen_messages,
                    &mut user_turn_index,
                    &mut pending_replacement_for_user_turn,
                    &ts,
                    &role,
                    text,
                );
            }
            if let Some((role, text)) = codex_payload_text(payload) {
                push_codex_transcript_message(
                    &mut entries,
                    &mut seen_messages,
                    &mut user_turn_index,
                    &mut pending_replacement_for_user_turn,
                    &ts,
                    &role,
                    text,
                );
            }
        }
    }

    Some(entries)
}

fn parse_claude_session_entries(path: &Path) -> Option<Vec<serde_json::Value>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(trimmed) else {
            continue;
        };
        let typ = kind.kind.as_deref().unwrap_or("");
        if typ != "user" && typ != "assistant" {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let text = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(message_content_text)
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        entries.push(serde_json::json!({
            "ts": value_str(&obj, "timestamp").unwrap_or_default(),
            "level": if typ == "assistant" { "model" } else { "info" },
            "source": external_transcript_source("claude", typ),
            "content": text,
        }));
    }

    Some(entries)
}

fn parse_gemini_session_entries(path: &Path, session_id: &str) -> Option<Vec<serde_json::Value>> {
    let contents = std::fs::read_to_string(path).ok()?;
    let obj = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    if value_str(&obj, "sessionId").as_deref() != Some(session_id) {
        return None;
    }

    let mut entries = Vec::new();
    if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg
                .get("role")
                .or_else(|| msg.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("message");
            let text = msg
                .get("text")
                .or_else(|| msg.get("message"))
                .or_else(|| msg.get("content"))
                .and_then(message_content_text)
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            entries.push(serde_json::json!({
                "ts": value_str(msg, "timestamp").unwrap_or_default(),
                "level": if role == "assistant" || role == "model" { "model" } else { "info" },
                "source": external_transcript_source("gemini", role),
                "content": text,
            }));
        }
    }

    Some(entries)
}

fn find_claude_session_file_for_transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(&home.join(".claude").join("projects"), ".jsonl", &mut files);
    files
        .into_iter()
        .find(|path| path.file_stem().and_then(|n| n.to_str()) == Some(session_id))
}

fn find_gemini_session_file_for_transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(&home.join(".gemini").join("tmp"), ".json", &mut files);
    files.into_iter().find(|path| {
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            != Some("chats")
        {
            return false;
        }
        let Ok(contents) = std::fs::read_to_string(path) else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&contents)
            .ok()
            .and_then(|obj| value_str(&obj, "sessionId"))
            .as_deref()
            == Some(session_id)
    })
}

fn parse_external_session_entries_from_file(
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<Vec<serde_json::Value>> {
    match source {
        "codex" => parse_codex_session_entries(path),
        "claude-code" => parse_claude_session_entries(path),
        "gemini" => parse_gemini_session_entries(path, session_id),
        _ => None,
    }
}

fn external_session_entries_from_file(
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<Vec<serde_json::Value>> {
    let key = external_transcript_cache_key(source, session_id, path)?;
    if let Some(entries) = cached_external_transcript_entries(&key) {
        return Some(entries);
    }

    let entries = parse_external_session_entries_from_file(source, session_id, path)?;
    store_external_transcript_entries(key, &entries);
    Some(entries)
}

fn external_session_entries_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<Vec<serde_json::Value>> {
    let source = crate::session_names::normalize_source(source);
    let path = match source.as_str() {
        "codex" => find_codex_session_file(home, session_id),
        "claude-code" => find_claude_session_file_for_transcript(home, session_id),
        "gemini" => find_gemini_session_file_for_transcript(home, session_id),
        _ => None,
    }?;

    external_session_entries_from_file(&source, session_id, &path)
}

fn external_session_activity_replay(
    source: &str,
    session_id: &str,
    limit: usize,
) -> Option<String> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    external_session_activity_replay_from_home(&home, source, session_id, limit)
}

fn external_session_activity_replay_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: usize,
) -> Option<String> {
    external_session_activity_replay_from_home_with_attach(home, source, session_id, limit, true)
}

fn external_session_activity_replay_from_home_with_attach(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: usize,
    include_attached: bool,
) -> Option<String> {
    let source = crate::session_names::normalize_source(source);
    let mut transcript = external_session_entries_from_home(home, &source, session_id)?;
    if limit > 0 && transcript.len() > limit {
        transcript = transcript.split_off(transcript.len() - limit);
    }

    let mut entries = Vec::with_capacity(transcript.len() + 2);
    entries.push(serde_json::json!({
        "event": "replay_start",
        "session_id": session_id,
        "source": source,
        "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
    }));
    if include_attached {
        entries.push(serde_json::json!({
            "event": "session_attached",
            "session_id": session_id,
            "source": source,
            "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
        }));
    }

    for entry in transcript {
        let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if content.is_empty() {
            continue;
        }
        entries.push(serde_json::json!({
            "event": "log_entry",
            "session_id": session_id,
            "ts": entry.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
            "level": entry.get("level").and_then(|v| v.as_str()).unwrap_or("info"),
            "source": entry.get("source").and_then(|v| v.as_str()).unwrap_or(source.as_str()),
            "content": content,
            "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "user_turn_index": entry.get("user_turn_index").and_then(|v| v.as_u64()),
            "replacement_for_user_turn_index": entry
                .get("replacement_for_user_turn_index")
                .and_then(|v| v.as_u64()),
            "superseded": entry
                .get("superseded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "superseded_reason": entry
                .get("superseded_reason")
                .and_then(|v| v.as_str()),
            "kind": entry.get("kind").and_then(|v| v.as_str()),
        }));
    }

    Some(
        serde_json::json!({
            "t": "log_replay",
            "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "entries": entries,
        })
        .to_string(),
    )
}

fn external_attached_session_from_wire(line: &str) -> Option<(String, String)> {
    let parsed = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if parsed.get("event").and_then(|v| v.as_str()) != Some("session_attached") {
        return None;
    }
    let session_id = parsed.get("session_id").and_then(|v| v.as_str())?;
    let source = parsed.get("source").and_then(|v| v.as_str())?;
    if source == "intendant" {
        return None;
    }
    Some((session_id.to_string(), source.to_string()))
}

fn session_ended_id_from_wire(line: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if parsed.get("event").and_then(|v| v.as_str()) != Some("session_ended") {
        return None;
    }
    parsed
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn list_sessions() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let home_path = PathBuf::from(&home);
    list_sessions_from_home(&home_path)
}

fn empty_worktree_inventory_response() -> String {
    serde_json::to_string(&crate::worktree_inventory::empty_scan())
        .unwrap_or_else(|_| "{}".to_string())
}

fn scan_worktree_inventory_response(home: &Path, project_root: Option<&Path>) -> String {
    let hints = worktree_session_hints_from_home(home);
    let scan = crate::worktree_inventory::scan_worktrees(home, project_root, &hints);
    serde_json::to_string(&scan).unwrap_or_else(|_| "{}".to_string())
}

fn remove_worktree_inventory_response(home: &Path, body_text: &str) -> (&'static str, String) {
    let request =
        match serde_json::from_str::<crate::worktree_inventory::WorktreeRemoveRequest>(body_text) {
            Ok(request) => request,
            Err(e) => {
                return (
                    "400 Bad Request",
                    serde_json::json!({
                        "ok": false,
                        "error": format!("invalid worktree removal request: {e}")
                    })
                    .to_string(),
                );
            }
        };
    let hints = worktree_session_hints_from_home(home);
    match crate::worktree_inventory::remove_worktree_if_safe(request, &hints) {
        Ok(response) => (
            "200 OK",
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string()),
        ),
        Err(e) => (
            "409 Conflict",
            serde_json::json!({
                "ok": false,
                "error": e
            })
            .to_string(),
        ),
    }
}

fn worktree_session_hints_from_home(
    home: &Path,
) -> Vec<crate::worktree_inventory::WorktreeSessionHint> {
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&list_sessions_from_home(home)).unwrap_or_default();
    sessions
        .into_iter()
        .filter_map(|session| {
            let session_id = session.get("session_id")?.as_str()?.to_string();
            let source = session
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("intendant")
                .to_string();
            let status = session
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let project_root = session
                .get("project_root")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            let cwd = session
                .get("cwd")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            let updated_at = session
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            Some(crate::worktree_inventory::WorktreeSessionHint {
                session_id,
                source,
                status,
                project_root,
                cwd,
                updated_at,
            })
        })
        .collect()
}

fn list_sessions_from_home(home_path: &Path) -> String {
    let logs_dir = home_path.join(".intendant").join("logs");
    let mut external_sessions = Vec::new();
    external_sessions.extend(list_codex_sessions(home_path));
    external_sessions.extend(list_claude_sessions(home_path));
    external_sessions.extend(list_gemini_sessions(home_path));
    crate::session_names::apply_session_name_overlays(home_path, &mut external_sessions);
    if !logs_dir.is_dir() {
        sort_sessions_newest_first(&mut external_sessions);
        truncate_sessions_preserving_sources(&mut external_sessions);
        return serde_json::to_string(&external_sessions).unwrap_or_else(|_| "[]".to_string());
    }
    let external_context_by_id = external_session_context_by_id(&external_sessions);

    let mut sessions: Vec<serde_json::Value> = Vec::new();

    let entries = match std::fs::read_dir(&logs_dir) {
        Ok(e) => e,
        Err(_) => return "[]".to_string(),
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let session_id = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Try to read session_meta.json first (fast path)
        let meta_path = dir.join("session_meta.json");
        let mut name: Option<String> = None;
        let mut task: Option<String> = None;
        let mut created_at: Option<String> = None;
        let mut project_root: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut provider: Option<String> = None;
        let mut model: Option<String> = None;
        let mut status = "in_progress".to_string();
        let mut turns: u64 = 0;
        let mut total_tokens: u64 = 0;
        let mut prompt_tokens: u64 = 0;
        let mut completion_tokens: u64 = 0;
        let mut cached_tokens: u64 = 0;
        let mut role: Option<String> = None;
        let mut external_resume_id: Option<String> = None;
        let mut external_source: Option<String> = None;
        let mut updated_at_secs = file_mtime_secs(&dir);

        if let Ok(meta_str) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str) {
                task = meta
                    .get("task")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                created_at = meta
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                project_root = meta
                    .get("project_root")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                name = meta
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| compact_text(s, 180));
                if let Some(s) = meta.get("status").and_then(|v| v.as_str()) {
                    status = s.to_string();
                }
                if let Some(t) = meta.get("last_turn").and_then(|v| v.as_u64()) {
                    turns = t;
                }
                role = meta
                    .get("role")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }

        // Parse session.jsonl for provider, model, token totals, and any missing fields
        let jsonl_path = dir.join("session.jsonl");
        if let Ok(contents) = std::fs::read_to_string(&jsonl_path) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let event = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");

                match event {
                    "session_start" => {
                        if created_at.is_none() {
                            created_at = obj
                                .get("ts")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                        }
                    }
                    "info" | "debug" => {
                        if event == "info" {
                            if message.starts_with("Provider: ") && provider.is_none() {
                                provider =
                                    Some(message.trim_start_matches("Provider: ").to_string());
                            } else if message.starts_with("Model: ") && model.is_none() {
                                model = Some(message.trim_start_matches("Model: ").to_string());
                            } else if message.starts_with("Task: ") && task.is_none() {
                                task = Some(message.trim_start_matches("Task: ").to_string());
                            }
                        }
                        if external_resume_id.is_none() {
                            external_resume_id = external_agent_thread_id_from_message(message);
                        }
                        if external_source.is_none() {
                            external_source = external_agent_source_from_message(message);
                        }
                    }
                    "turn_start" => {
                        if let Some(t) = obj.get("turn").and_then(|v| v.as_u64()) {
                            if t > turns {
                                turns = t;
                            }
                        }
                    }
                    "model_response" => {
                        if let Some(tok) = obj.get("data").and_then(|d| d.get("tokens")) {
                            if let Some(t) = tok.get("total").and_then(|v| v.as_u64()) {
                                total_tokens += t;
                            }
                            if let Some(p) = tok.get("prompt").and_then(|v| v.as_u64()) {
                                prompt_tokens += p;
                            }
                            if let Some(c) = tok.get("completion").and_then(|v| v.as_u64()) {
                                completion_tokens += c;
                            }
                            if let Some(cached) = tok.get("cached").and_then(|v| v.as_u64()) {
                                cached_tokens += cached;
                            }
                        }
                    }
                    "task_complete" | "session_end" | "round_complete" => {
                        status = "completed".to_string();
                    }
                    "interrupted" => {
                        status = "interrupted".to_string();
                    }
                    _ => {}
                }
            }
        }

        if let Some(external_id) = external_resume_id.as_deref() {
            if let Some(context) = external_context_by_id.get(external_id) {
                if project_root.is_none() {
                    project_root = context.project_root.clone();
                }
                if cwd.is_none() {
                    cwd = context.cwd.clone().or_else(|| context.project_root.clone());
                }
                if name.is_none() {
                    name = context.name.clone();
                }
                if external_source.is_none() {
                    external_source = context.source.clone();
                }
            }
        }

        let backend_source_label = external_source.as_deref().and_then(|source| {
            external_resume_id
                .as_deref()
                .and_then(|external_id| external_context_by_id.get(external_id))
                .and_then(|context| context.source_label.clone())
                .or_else(|| Some(pretty_external_source_label(source)))
        });

        // Check for summary.json (written on clean exit)
        if status != "completed" && dir.join("summary.json").exists() {
            status = "completed".to_string();
        }

        // Recording / annotation / clip stats from disk
        let mut recording_count: u64 = 0;
        let mut recording_bytes: u64 = 0;
        let mut annotation_count: u64 = 0;
        let mut clip_count: u64 = 0;
        let mut frames_bytes: u64 = 0;
        let mut turns_bytes: u64 = 0;
        let mut logs_bytes: u64 = 0;

        let recordings_dir = dir.join("recordings");
        if recordings_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&recordings_dir) {
                for re in rd.flatten() {
                    if re.path().is_dir() {
                        recording_count += 1;
                        if let Ok(files) = std::fs::read_dir(re.path()) {
                            for f in files.flatten() {
                                let name = f.file_name().to_string_lossy().to_string();
                                if name.starts_with("seg_") {
                                    if let Ok(m) = f.metadata() {
                                        if m.is_file() {
                                            updated_at_secs =
                                                updated_at_secs.max(metadata_mtime_secs(&m));
                                            recording_bytes += m.len();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let frames_dir = dir.join("frames");
        if frames_dir.is_dir() {
            if let Ok(fd) = std::fs::read_dir(&frames_dir) {
                let mut clip_ids: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for fe in fd.flatten() {
                    let name = fe.file_name().to_string_lossy().to_string();
                    if name.starts_with("ann-") && name.ends_with(".jpg") {
                        annotation_count += 1;
                    } else if name.starts_with("clip-") && name.ends_with(".jpg") {
                        if let Some(pos) = name.rfind("-f") {
                            clip_ids.insert(name[..pos].to_string());
                        }
                    }
                    if let Ok(m) = fe.metadata() {
                        if m.is_file() {
                            updated_at_secs = updated_at_secs.max(metadata_mtime_secs(&m));
                            frames_bytes += m.len();
                        }
                    }
                }
                clip_count = clip_ids.len() as u64;
            }
        }

        // Turns directory size
        let turns_dir = dir.join("turns");
        if turns_dir.is_dir() {
            if let Ok(td) = std::fs::read_dir(&turns_dir) {
                for te in td.flatten() {
                    if let Ok(m) = te.metadata() {
                        if m.is_file() {
                            updated_at_secs = updated_at_secs.max(metadata_mtime_secs(&m));
                            turns_bytes += m.len();
                        }
                    }
                }
            }
        }

        // Root-level log files size
        for name in &[
            "session.jsonl",
            "session_meta.json",
            "summary.json",
            "conversation.jsonl",
        ] {
            if let Ok(m) = std::fs::metadata(dir.join(name)) {
                if m.is_file() {
                    updated_at_secs = updated_at_secs.max(metadata_mtime_secs(&m));
                    logs_bytes += m.len();
                }
            }
        }

        let total_bytes = recording_bytes + frames_bytes + turns_bytes + logs_bytes;

        // Refine status for sessions that never did model work:
        // - "idle": had some activity (recordings, display, task) but no model turns
        // - "abandoned": no turns, no task, no media — MCP probes, brief connections
        // Also override "interrupted" → "idle" when no model work happened
        // (process was killed before any model interaction — nothing was interrupted)
        if status != "completed" {
            let has_model_work = turns > 0 || total_tokens > 0;
            if !has_model_work {
                let has_media = recording_count > 0 || annotation_count > 0 || clip_count > 0;
                if task.is_some() || has_media {
                    status = "idle".to_string();
                } else {
                    status = "abandoned".to_string();
                }
            }
        }

        // Fall back to directory mtime for created_at
        if created_at.is_none() {
            created_at = mtime_secs_to_string(file_mtime_secs(&dir));
        }

        // Estimate cost using the model's pricing.
        let estimated_cost = model.as_deref().and_then(|m| {
            crate::app_state_pricing::estimate_session_cost(
                m,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                0,
            )
        });

        let created_at = created_at.unwrap_or_default();
        let updated_at =
            mtime_secs_to_string(updated_at_secs).unwrap_or_else(|| created_at.clone());

        let wrapper_session = serde_json::json!({
            "source": "intendant",
            "source_label": "Intendant",
            "session_id": session_id.clone(),
            "resume_id": session_id.clone(),
            "backend_source": external_source.clone(),
            "backend_source_label": backend_source_label,
            "backend_session_id": external_resume_id.clone(),
            "created_at": created_at,
            "updated_at": updated_at,
            "name": name,
            "task": task,
            "provider": provider,
            "model": model,
            "turns": turns,
            "status": status,
            "total_tokens": total_tokens,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "cached_tokens": cached_tokens,
            "cache_creation_tokens": 0,
            "estimated_cost": estimated_cost.unwrap_or(0.0),
            "pricing_known": estimated_cost.is_some(),
            "role": role,
            "recordings": recording_count,
            "recording_bytes": recording_bytes,
            "annotations": annotation_count,
            "clips": clip_count,
            "frames_bytes": frames_bytes,
            "turns_bytes": turns_bytes,
            "logs_bytes": logs_bytes,
            "total_bytes": total_bytes,
            "cwd": cwd.clone().or_else(|| project_root.clone()),
            "project_root": project_root.clone(),
            "path": dir.to_string_lossy().to_string(),
            "can_delete": true,
            "can_resume": true,
        });

        let merged_into_external = external_source
            .as_deref()
            .zip(external_resume_id.as_deref())
            .filter(|(source, external_id)| {
                crate::external_agent::source_session_id_is_canonical(source, external_id)
            })
            .and_then(|(source, external_id)| {
                external_sessions
                    .iter_mut()
                    .find(|session| external_session_row_matches(session, source, external_id))
            })
            .map(|external| {
                merge_intendant_wrapper_into_external_session(external, &wrapper_session);
            })
            .is_some();

        if !merged_into_external {
            sessions.push(wrapper_session);
        }
    }

    sessions.extend(external_sessions);

    sort_sessions_newest_first(&mut sessions);
    truncate_sessions_preserving_sources(&mut sessions);

    serde_json::to_string(&sessions).unwrap_or_else(|_| "[]".to_string())
}

/// Handle `/api/session/current/changes[/{path}]` requests.
///
/// - No path suffix: list all changed files (baseline vs current).
/// - With path suffix: return unified diff for a single file.
#[derive(Debug, Clone)]
enum ChangeFileState {
    Text { content: String, hash: String },
    Unsupported { hash: String, reason: String },
}

#[derive(Debug, Clone)]
struct ChangeRecord {
    path: String,
    kind: &'static str,
    lines_added: u32,
    lines_removed: u32,
    diff_available: bool,
    reason: Option<String>,
    diff: Option<String>,
}

fn handle_changes_request(
    request_line: &str,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
) -> (&'static str, String) {
    let (snapshot_dir, project_root) = match (snapshot_dir, project_root) {
        (Some(s), Some(p)) => (s, p),
        _ => {
            return (
                "503 Service Unavailable",
                serde_json::json!({"error": "file watcher not active"}).to_string(),
            );
        }
    };

    let baseline_dir = snapshot_dir.join("baseline");
    if !baseline_dir.exists() {
        return ("200 OK", serde_json::json!([]).to_string());
    }

    // Extract the request target from `GET <target> HTTP/1.1`, then trim the
    // endpoint prefix. The list endpoint has no path suffix.
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let file_path = target
        .strip_prefix("/api/session/current/changes")
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .trim_start_matches('/');

    if file_path.is_empty() {
        // List all changed files.
        ("200 OK", handle_changes_list(&baseline_dir, project_root))
    } else {
        // Single-file diff.
        handle_changes_file_diff(file_path, &baseline_dir, project_root)
    }
}

fn load_baseline_manifest(baseline_dir: &Path) -> crate::file_watcher::BaselineManifest {
    let Some(snapshot_dir) = baseline_dir.parent() else {
        return HashMap::new();
    };
    let path = snapshot_dir.join(crate::file_watcher::BASELINE_MANIFEST_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn collect_baseline_text_paths(baseline_dir: &Path) -> HashSet<String> {
    let mut paths = HashSet::new();
    let mut stack = vec![baseline_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(baseline_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if crate::file_watcher::should_ignore(rel) {
                continue;
            }
            paths.insert(crate::file_watcher::rel_path_key(rel));
        }
    }
    paths
}

fn collect_current_change_states(project_root: &Path) -> HashMap<String, ChangeFileState> {
    let mut states = HashMap::new();
    let mut stack = vec![project_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if let Ok(rel) = path.strip_prefix(project_root) {
                    if !crate::file_watcher::should_ignore(rel) {
                        stack.push(path);
                    }
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(project_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if crate::file_watcher::should_ignore(rel) {
                continue;
            }
            let key = crate::file_watcher::rel_path_key(rel);
            match crate::file_watcher::inspect_file(&path) {
                Ok(crate::file_watcher::InspectedFile::Text(snapshot)) => {
                    states.insert(
                        key,
                        ChangeFileState::Text {
                            content: snapshot.text,
                            hash: snapshot.hash_hex,
                        },
                    );
                }
                Ok(crate::file_watcher::InspectedFile::Unsupported(snapshot)) => {
                    states.insert(
                        key,
                        ChangeFileState::Unsupported {
                            hash: snapshot.hash_hex,
                            reason: snapshot.reason,
                        },
                    );
                }
                Err(_) => continue,
            }
        }
    }
    states
}

fn inspect_current_change_state(project_root: &Path, rel_key: &str) -> Option<ChangeFileState> {
    let path = project_root.join(Path::new(rel_key));
    if !path.exists() {
        return None;
    }
    match crate::file_watcher::inspect_file(&path) {
        Ok(crate::file_watcher::InspectedFile::Text(snapshot)) => Some(ChangeFileState::Text {
            content: snapshot.text,
            hash: snapshot.hash_hex,
        }),
        Ok(crate::file_watcher::InspectedFile::Unsupported(snapshot)) => {
            Some(ChangeFileState::Unsupported {
                hash: snapshot.hash_hex,
                reason: snapshot.reason,
            })
        }
        Err(_) => None,
    }
}

fn read_baseline_text(baseline_dir: &Path, rel_key: &str) -> Option<String> {
    std::fs::read_to_string(baseline_dir.join(Path::new(rel_key))).ok()
}

fn baseline_hash_for(
    baseline_text: Option<&str>,
    baseline_meta: Option<&crate::file_watcher::BaselineFileMeta>,
) -> Option<String> {
    baseline_meta.map(|m| m.hash.clone()).or_else(|| {
        baseline_text.map(|s| {
            crate::file_watcher::hex_encode(&crate::file_watcher::sha256_hash(s.as_bytes()))
        })
    })
}

fn diff_stat_pair(baseline: &str, current: &str) -> (u32, u32) {
    let diff = similar::TextDiff::from_lines(baseline, current);
    let mut added = 0u32;
    let mut removed = 0u32;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

fn unsupported_change_record(rel_key: &str, kind: &'static str, reason: String) -> ChangeRecord {
    ChangeRecord {
        path: rel_key.to_string(),
        kind,
        lines_added: 0,
        lines_removed: 0,
        diff_available: false,
        reason: Some(reason),
        diff: None,
    }
}

fn compute_change_record(
    rel_key: &str,
    baseline_dir: &Path,
    current: Option<&ChangeFileState>,
    baseline_manifest: &crate::file_watcher::BaselineManifest,
    include_diff: bool,
) -> Option<ChangeRecord> {
    let baseline_text = read_baseline_text(baseline_dir, rel_key);
    let baseline_meta = baseline_manifest.get(rel_key);
    let baseline_exists = baseline_text.is_some() || baseline_meta.is_some();
    let baseline_supported_text =
        baseline_text.is_some() && baseline_meta.map(|m| m.supported_text).unwrap_or(true);

    match (
        baseline_exists,
        baseline_supported_text,
        baseline_text.as_deref(),
        current,
    ) {
        (false, _, _, None) => None,
        (false, _, _, Some(ChangeFileState::Text { content, .. })) => {
            let (lines_added, lines_removed) = diff_stat_pair("", content);
            let diff = include_diff
                .then(|| crate::file_watcher::compute_unified_diff("", content, rel_key));
            Some(ChangeRecord {
                path: rel_key.to_string(),
                kind: "created",
                lines_added,
                lines_removed,
                diff_available: true,
                reason: None,
                diff,
            })
        }
        (false, _, _, Some(ChangeFileState::Unsupported { reason, .. })) => Some(
            unsupported_change_record(rel_key, "created", reason.clone()),
        ),
        (true, true, Some(base), None) => {
            let diff =
                include_diff.then(|| crate::file_watcher::compute_unified_diff(base, "", rel_key));
            Some(ChangeRecord {
                path: rel_key.to_string(),
                kind: "deleted",
                lines_added: 0,
                lines_removed: base.lines().count() as u32,
                diff_available: true,
                reason: None,
                diff,
            })
        }
        (true, false, _, None) => {
            let reason = baseline_meta
                .and_then(|m| m.reason.clone())
                .unwrap_or_else(|| "baseline file was not text-diffable".to_string());
            Some(unsupported_change_record(rel_key, "deleted", reason))
        }
        (true, true, Some(base), Some(ChangeFileState::Text { content, hash })) => {
            let baseline_hash = baseline_hash_for(Some(base), baseline_meta);
            if baseline_hash.as_ref() == Some(hash) || base == content {
                return None;
            }
            let (lines_added, lines_removed) = diff_stat_pair(base, content);
            let diff = include_diff
                .then(|| crate::file_watcher::compute_unified_diff(base, content, rel_key));
            Some(ChangeRecord {
                path: rel_key.to_string(),
                kind: "modified",
                lines_added,
                lines_removed,
                diff_available: true,
                reason: None,
                diff,
            })
        }
        (true, true, Some(base), Some(ChangeFileState::Unsupported { hash, reason })) => {
            let baseline_hash = baseline_hash_for(Some(base), baseline_meta);
            if baseline_hash.as_ref() == Some(hash) {
                return None;
            }
            Some(unsupported_change_record(
                rel_key,
                "modified",
                reason.clone(),
            ))
        }
        (true, false, _, Some(ChangeFileState::Text { hash, .. }))
        | (true, false, _, Some(ChangeFileState::Unsupported { hash, .. })) => {
            if baseline_meta.map(|m| &m.hash) == Some(hash) {
                return None;
            }
            let reason = baseline_meta
                .and_then(|m| m.reason.clone())
                .unwrap_or_else(|| "baseline file was not text-diffable".to_string());
            Some(unsupported_change_record(rel_key, "modified", reason))
        }
        _ => None,
    }
}

fn change_record_summary_json(record: &ChangeRecord) -> serde_json::Value {
    serde_json::json!({
        "path": record.path.clone(),
        "kind": record.kind,
        "lines_added": record.lines_added,
        "lines_removed": record.lines_removed,
        "diff_available": record.diff_available,
        "reason": record.reason.clone(),
    })
}

fn change_record_detail_json(record: &ChangeRecord) -> serde_json::Value {
    serde_json::json!({
        "path": record.path.clone(),
        "kind": record.kind,
        "diff": record.diff.clone().unwrap_or_default(),
        "lines_added": record.lines_added,
        "lines_removed": record.lines_removed,
        "diff_available": record.diff_available,
        "reason": record.reason.clone(),
    })
}

/// List all files that have changed since the session baseline.
fn handle_changes_list(baseline_dir: &Path, project_root: &Path) -> String {
    let baseline_manifest = load_baseline_manifest(baseline_dir);
    let baseline_paths = collect_baseline_text_paths(baseline_dir);
    let current_states = collect_current_change_states(project_root);
    let mut keys: HashSet<String> = baseline_manifest.keys().cloned().collect();
    keys.extend(baseline_paths);
    keys.extend(current_states.keys().cloned());

    let mut changes = Vec::new();
    let mut sorted_keys: Vec<String> = keys.into_iter().collect();
    sorted_keys.sort();
    for key in sorted_keys {
        if crate::file_watcher::should_ignore(Path::new(&key)) {
            continue;
        }
        if let Some(record) = compute_change_record(
            &key,
            baseline_dir,
            current_states.get(&key),
            &baseline_manifest,
            false,
        ) {
            changes.push(change_record_summary_json(&record));
        }
    }
    serde_json::to_string(&changes).unwrap_or_else(|_| "[]".to_string())
}

/// Return a unified diff for a single file.
fn handle_changes_file_diff(
    file_path: &str,
    baseline_dir: &Path,
    project_root: &Path,
) -> (&'static str, String) {
    let decoded = url_path_decode(file_path);
    // Reject path traversal.
    let rel = Path::new(&decoded);
    for component in rel.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid path"}).to_string(),
            );
        }
    }
    if crate::file_watcher::should_ignore(rel) {
        return (
            "404 Not Found",
            serde_json::json!({"error": "no changes for path"}).to_string(),
        );
    }

    let baseline_path = baseline_dir.join(rel);
    let current_path = project_root.join(rel);

    // Verify resolved paths stay within their roots.
    if let (Ok(resolved_baseline), Ok(resolved_root)) = (
        baseline_path
            .canonicalize()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(baseline_path.clone())),
        baseline_dir
            .canonicalize()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(baseline_dir.to_path_buf())),
    ) {
        if !resolved_baseline.starts_with(&resolved_root) {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid path"}).to_string(),
            );
        }
    }
    if let (Ok(resolved_current), Ok(resolved_root)) = (
        current_path
            .canonicalize()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(current_path.clone())),
        project_root
            .canonicalize()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(project_root.to_path_buf())),
    ) {
        if !resolved_current.starts_with(&resolved_root) {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid path"}).to_string(),
            );
        }
    }

    let baseline_manifest = load_baseline_manifest(baseline_dir);
    let current = inspect_current_change_state(project_root, &decoded);

    match compute_change_record(
        &decoded,
        baseline_dir,
        current.as_ref(),
        &baseline_manifest,
        true,
    ) {
        Some(record) => ("200 OK", change_record_detail_json(&record).to_string()),
        None => (
            "404 Not Found",
            serde_json::json!({"error": "no changes for path"}).to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Per-round file snapshot history endpoints
// ---------------------------------------------------------------------------

/// Read the full POST body (honoring Content-Length). Returns the peeked
/// prefix if the headers already carried the entire payload; otherwise reads
/// the remainder from the stream.
async fn read_post_body(header_text: &str, stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return peeked_body[..content_length].to_string();
    }
    let mut full = peeked_body.to_string();
    let remaining = content_length.saturating_sub(peeked_body.len());
    if remaining > 0 {
        let mut rest = vec![0u8; remaining];
        if stream.read_exact(&mut rest).await.is_ok() {
            full.push_str(&String::from_utf8_lossy(&rest));
        }
    }
    full
}

// ---------------------------------------------------------------------------
// File upload endpoints
// ---------------------------------------------------------------------------

/// Hard cap on individual uploaded file size. Prevents a rogue or mistaken
/// upload (e.g. someone dragging a multi-GB video file) from OOMing the
/// daemon or filling the session dir. Plumbed through the streaming reader
/// so we bail before reading the full body.
///
/// Picked to cover common real uploads (PDFs, CSVs, source archives,
/// annotated screenshots) without accepting arbitrary blobs. Can be made
/// configurable later via `[upload] max_size_mb` in intendant.toml.
const UPLOAD_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Stream the body of an HTTP request into a fresh tempfile, honouring
/// `Content-Length` and bailing out early if the body exceeds `max_bytes`.
///
/// Returns `(tempfile, size)` on success. Designed so the caller can then
/// commit the tempfile into the upload store via
/// [`crate::upload_store::commit_upload`], which atomically renames it
/// into place.
///
/// This is the binary counterpart to `read_post_body` — same peek-then-
/// stream pattern, but sinks to disk instead of a UTF-8 `String`.
async fn stream_body_to_tempfile(
    header_text: &str,
    initial_request_bytes: &[u8],
    stream: &mut tokio::net::TcpStream,
    max_bytes: usize,
) -> Result<(tempfile::NamedTempFile, usize), String> {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| "missing or invalid Content-Length".to_string())?;
    if content_length == 0 {
        return Err("empty body".to_string());
    }
    if content_length > max_bytes {
        return Err(format!(
            "body too large: {} bytes (cap is {})",
            content_length, max_bytes
        ));
    }

    let peeked_body = initial_body_bytes(initial_request_bytes)?;
    let mut tmp = tempfile::NamedTempFile::new().map_err(|e| format!("create tempfile: {e}"))?;

    // Write whatever body bytes we already have from the peek. These come
    // back through the same header_text split, so they're the leading
    // content_length bytes — truncate defensively in case the peek read
    // slightly more than the body.
    let peeked_n = peeked_body.len().min(content_length);
    tmp.write_all(&peeked_body[..peeked_n])
        .map_err(|e| format!("write tempfile: {e}"))?;
    let mut written = peeked_n;

    // Pull the rest from the socket in 64 KB chunks. The cap bails early;
    // the final total is asserted to equal Content-Length so we don't store
    // a truncated file.
    let mut buf = vec![0u8; 64 * 1024];
    while written < content_length {
        let want = (content_length - written).min(buf.len());
        match stream.read(&mut buf[..want]).await {
            Ok(0) => {
                return Err(format!(
                    "connection closed mid-upload at {} / {} bytes",
                    written, content_length
                ));
            }
            Ok(n) => {
                tmp.as_file_mut()
                    .write_all(&buf[..n])
                    .map_err(|e| format!("write tempfile: {e}"))?;
                written += n;
            }
            Err(e) => return Err(format!("socket read: {e}")),
        }
    }
    tmp.as_file_mut()
        .flush()
        .map_err(|e| format!("flush tempfile: {e}"))?;
    Ok((tmp, written))
}

fn initial_body_bytes(initial_request_bytes: &[u8]) -> Result<&[u8], String> {
    initial_request_bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|idx| &initial_request_bytes[idx + 4..])
        .ok_or_else(|| "incomplete HTTP headers".to_string())
}

fn pending_upload_session_dir(project_root: &std::path::Path) -> std::path::PathBuf {
    project_root.join(".intendant").join("pending_uploads")
}

fn json_response(status: &str, body: String) -> String {
    format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        body.len(),
        body
    )
}

fn json_ok(value: serde_json::Value) -> String {
    json_response("200 OK", value.to_string())
}

fn json_error(status: &str, message: impl AsRef<str>) -> String {
    json_response(
        status,
        serde_json::json!({ "error": message.as_ref() }).to_string(),
    )
}

fn effective_upload_destination(
    requested: crate::upload_store::UploadDestination,
    has_active_session: bool,
) -> crate::upload_store::UploadDestination {
    if has_active_session {
        requested
    } else {
        crate::upload_store::UploadDestination::Workspace
    }
}

/// Parse a query-string value by key out of a full `request_line`
/// (e.g. `POST /api/session/current/uploads?name=foo.pdf&destination=task HTTP/1.1`).
/// Returns the URL-decoded value, or `None` if the key isn't present.
fn query_param<'a>(request_line: &'a str, key: &str) -> Option<String> {
    let path_and_q = request_line.split_whitespace().nth(1)?;
    let query = path_and_q.splitn(2, '?').nth(1)?;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        if k == key {
            return Some(url_decode(v));
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` decoder: `%HH` → byte,
/// `+` → space. Good enough for filenames/destinations on the upload
/// path; we don't invite the full urlencoding crate just for this.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = &bytes[i + 1..i + 3];
                match std::str::from_utf8(h)
                    .ok()
                    .and_then(|hs| u8::from_str_radix(hs, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode percent escapes in an HTTP path segment. Unlike query-string
/// decoding, `+` is a literal plus in paths.
fn url_path_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = &bytes[i + 1..i + 3];
                match std::str::from_utf8(h)
                    .ok()
                    .and_then(|hs| u8::from_str_radix(hs, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn expand_dashboard_fs_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    let path = if trimmed.is_empty() || trimmed == "~" {
        dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| "could not resolve home directory".to_string())?
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return Err(format!(
            "path must be absolute or start with ~/ (got {})",
            trimmed
        ));
    }
    Ok(path)
}

fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn inspect_dashboard_fs_path(raw: &str) -> Result<FsPathStatus, String> {
    let path = expand_dashboard_fs_path(raw)?;
    let metadata = std::fs::metadata(&path).ok();
    let exists = metadata.is_some();
    let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let is_file = metadata.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let readable = if is_dir {
        std::fs::read_dir(&path).is_ok()
    } else if is_file {
        std::fs::File::open(&path).is_ok()
    } else {
        false
    };
    let display_path = if exists {
        std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())
    } else {
        path.clone()
    };
    let parent = path.parent().map(|p| p.to_string_lossy().to_string());
    let parent_metadata = path.parent().and_then(|p| std::fs::metadata(p).ok());
    let nearest = nearest_existing_parent(&path);
    let nearest_is_dir = nearest
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.is_dir())
        .unwrap_or(false);
    Ok(FsPathStatus {
        input: raw.trim().to_string(),
        path: display_path.to_string_lossy().to_string(),
        exists,
        is_dir,
        is_file,
        readable,
        parent,
        parent_exists: parent_metadata.is_some(),
        parent_is_dir: parent_metadata.map(|m| m.is_dir()).unwrap_or(false),
        nearest_existing_parent: nearest.map(|p| p.to_string_lossy().to_string()),
        can_create: !exists && nearest_is_dir,
    })
}

fn list_dashboard_fs_dir(raw: &str) -> Result<serde_json::Value, String> {
    let path = expand_dashboard_fs_path(raw)?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| format!("{} is not accessible: {}", path.display(), e))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    let read_dir = std::fs::read_dir(&canonical)
        .map_err(|e| format!("could not read {}: {}", canonical.display(), e))?;
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type().ok();
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let is_file = metadata.as_ref().map(|m| m.is_file()).unwrap_or(false);
        entries.push(FsListEntry {
            hidden: name.starts_with('.'),
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_dir,
            is_file,
            is_symlink: file_type.map(|t| t.is_symlink()).unwrap_or(false),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FS_LIST_LIMIT;
    entries.truncate(FS_LIST_LIMIT);
    let parent = canonical.parent().map(|p| p.to_string_lossy().to_string());
    Ok(serde_json::json!({
        "path": canonical.to_string_lossy().to_string(),
        "parent": parent,
        "home": dirs::home_dir().map(|p| p.to_string_lossy().to_string()),
        "entries": entries,
        "truncated": truncated,
    }))
}

fn mkdir_dashboard_fs_path(raw: &str) -> Result<serde_json::Value, (String, String)> {
    let path = expand_dashboard_fs_path(raw).map_err(|e| ("400 Bad Request".to_string(), e))?;
    if path.exists() {
        if path.is_dir() {
            let display = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            return Ok(serde_json::json!({
                "ok": true,
                "created": false,
                "already_exists": true,
                "path": display.to_string_lossy().to_string(),
                "notice": "Directory already exists"
            }));
        }
        return Err((
            "409 Conflict".to_string(),
            format!("{} already exists and is not a directory", path.display()),
        ));
    }
    std::fs::create_dir_all(&path).map_err(|e| {
        (
            "500 Internal Server Error".to_string(),
            format!("failed to create {}: {}", path.display(), e),
        )
    })?;
    let display = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(serde_json::json!({
        "ok": true,
        "created": true,
        "already_exists": false,
        "path": display.to_string_lossy().to_string()
    }))
}

/// Extract the `Content-Type` request header value, or a generic default.
fn content_type_header(header_text: &str) -> String {
    header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-type:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Build an HTTP response for an upload endpoint error.
fn upload_error_response(status: &str, message: &str) -> String {
    let body = serde_json::json!({"error": message}).to_string();
    format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        body.len(),
        body
    )
}

/// Check whether it is safe to mutate the project tree (rollback/redo) right
/// now. Returns `Ok(())` if idle, or an `(status_code, body_json)` pair to
/// send back as-is.
fn ensure_idle(
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> Result<(), (&'static str, String)> {
    if let Some(state) = agent_state {
        let phase = state.lock().map(|g| g.phase.clone()).unwrap_or_default();
        if !presence::is_agent_idle(&phase) {
            let body = serde_json::json!({
                "error": "agent is busy, stop the turn before rolling back",
                "phase": phase,
            })
            .to_string();
            return Err(("409 Conflict", body));
        }
    }
    Ok(())
}

/// GET /api/session/current/history — returns serialized `History` JSON.
async fn handle_history_get(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    let w = fw.lock().await;
    let body = serde_json::to_string(w.history()).unwrap_or_else(|_| "{}".to_string());
    ("200 OK", body)
}

/// POST /api/session/current/rollback — body:
/// ```json
/// { "round_id": N,
///   "revert_files": true,          // default true (backward-compat)
///   "revert_conversation": false   // default false
/// }
/// ```
///
/// Each boolean is independent. When both are false the endpoint is a
/// validation-only no-op (returns 400). Existing callers passing only
/// `round_id` get a file-only revert, matching prior behavior.
///
/// `revert_conversation` emits an `AppEvent::ConversationRollbackRequested`
/// on the shared bus. The active agent loop subscribes and either
/// truncates its native `Conversation` (native path), issues
/// `thread/rollback` (Codex), or shuts down and re-initializes
/// (session-reset for Claude Code / Gemini). A matching
/// `AppEvent::ConversationRolledBack` is emitted when the work
/// completes. The HTTP response does not wait for that completion —
/// the dashboard observes the event stream.
async fn handle_history_rollback(
    body_text: &str,
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
    bus: &EventBus,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    if let Err((status, body)) = ensure_idle(agent_state) {
        return (status, body);
    }
    let parsed: serde_json::Value = match serde_json::from_str(body_text) {
        Ok(v) => v,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("invalid body: {}", e)}).to_string(),
            );
        }
    };
    let round_id = match parsed.get("round_id").and_then(|v| v.as_u64()) {
        Some(id) => id,
        None => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "missing round_id"}).to_string(),
            );
        }
    };
    // Backward-compat: old callers pass only `round_id` and expect a
    // file-only revert. New callers supply both flags.
    let revert_files = parsed
        .get("revert_files")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let revert_conversation = parsed
        .get("revert_conversation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !revert_files && !revert_conversation {
        return (
            "400 Bad Request",
            serde_json::json!({
                "error": "at least one of revert_files / revert_conversation must be true"
            })
            .to_string(),
        );
    }

    // Resolve conversation-rollback parameters before we mutate any
    // state so a downstream failure doesn't leave files half-reverted
    // with no event emitted. Reading the history requires the same
    // mutex the rollback writes use, so we briefly acquire and release.
    let conv_params: Option<(Option<u32>, u32)> = if revert_conversation {
        let w = fw.lock().await;
        let hist = w.history();
        let target_idx = hist.rounds.iter().position(|r| r.id == round_id);
        let head_idx = hist
            .current_head_id
            .and_then(|hid| hist.rounds.iter().position(|r| r.id == hid));
        match (target_idx, head_idx) {
            (Some(t), Some(h)) => {
                // Compute turns to drop from the head turn-count sum
                // between (t, h]. This matches Codex's `numTurns`
                // semantics: the number of turns we want to undo.
                let turns_to_drop: u32 = if t < h {
                    hist.rounds[t + 1..=h]
                        .iter()
                        .map(|r| r.turn_count.unwrap_or(0))
                        .sum()
                } else {
                    0
                };
                let target_msg_count = hist.rounds[t].native_message_count;
                Some((target_msg_count, turns_to_drop))
            }
            (Some(_), None) => {
                // No head — rolling back with no active position is a
                // pure file-state restore; nothing to drop from the
                // conversation side.
                Some((hist.rounds[target_idx.unwrap()].native_message_count, 0))
            }
            _ => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": format!(
                        "round {} not found in active history", round_id
                    )})
                    .to_string(),
                );
            }
        }
    } else {
        None
    };

    // File rollback (may fail for reasons unrelated to the conversation
    // side; bail out before emitting the conversation event so both
    // halves stay consistent from the user's perspective).
    let file_result_json = if revert_files {
        let mut w = fw.lock().await;
        match w.rollback(round_id) {
            Ok(res) => serde_json::json!({
                "to_round_id": res.to_round_id,
                "files_reverted": res.files_reverted,
            }),
            Err(e) => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": e.to_string()}).to_string(),
                );
            }
        }
    } else {
        serde_json::json!({ "to_round_id": round_id, "files_reverted": 0 })
    };

    // Dispatch the conversation-rollback event; the agent loop picks it
    // up and emits `ConversationRolledBack` when done.
    if let Some((target_msg_count, turns_to_drop)) = conv_params {
        bus.send(AppEvent::ConversationRollbackRequested {
            round_id,
            target_native_message_count: target_msg_count,
            turns_to_drop,
        });
    }

    (
        "200 OK",
        serde_json::json!({
            "to_round_id": file_result_json["to_round_id"],
            "files_reverted": file_result_json["files_reverted"],
            "revert_files": revert_files,
            "revert_conversation": revert_conversation,
        })
        .to_string(),
    )
}

/// POST /api/session/current/redo — no body. Advances `current_head_id`.
async fn handle_history_redo(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    if let Err((status, body)) = ensure_idle(agent_state) {
        return (status, body);
    }
    let mut w = fw.lock().await;
    match w.redo() {
        Ok(res) => (
            "200 OK",
            serde_json::json!({
                "to_round_id": res.to_round_id,
                "files_reverted": res.files_reverted,
            })
            .to_string(),
        ),
        Err(e) => (
            "400 Bad Request",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// POST /api/session/current/prune — drop abandoned branches and GC orphaned
/// content-addressed blobs.
async fn handle_history_prune(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    let mut w = fw.lock().await;
    match w.prune_abandoned() {
        Ok(res) => (
            "200 OK",
            serde_json::json!({
                "branches_removed": res.branches_removed,
                "bytes_freed": res.bytes_freed,
            })
            .to_string(),
        ),
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// Delete session data: entire session, media, recordings, frames, or turns.
/// Returns a JSON result with `ok` and `bytes_freed`.
fn delete_session_data(session_id: &str, target: &str) -> String {
    // Path traversal protection
    if session_id.contains("..") || session_id.contains('/') || session_id.contains('\\') {
        return serde_json::json!({"ok": false, "error": "invalid session id"}).to_string();
    }

    let dir = match resolve_session_dir(session_id) {
        Some(d) => d,
        None => return serde_json::json!({"ok": false, "error": "session not found"}).to_string(),
    };

    let dir_byte_size = |path: &std::path::Path| -> u64 {
        let mut total = 0u64;
        if path.is_dir() {
            // On-disk allocation (512-byte blocks) with hardlinked inodes
            // counted once, matching `du` — so bytes_freed reflects the space
            // actually reclaimed, not apparent size.
            fn walk(dir: &std::path::Path, total: &mut u64, seen: &mut HashSet<(u64, u64)>) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for e in entries.flatten() {
                        let p = e.path();
                        if p.is_dir() {
                            walk(&p, total, seen);
                        } else if let Ok(m) = p.metadata() {
                            if m.nlink() > 1 && !seen.insert((m.dev(), m.ino())) {
                                continue;
                            }
                            *total = total.saturating_add(m.blocks().saturating_mul(512));
                        }
                    }
                }
            }
            let mut seen: HashSet<(u64, u64)> = HashSet::new();
            walk(path, &mut total, &mut seen);
        }
        total
    };

    match target {
        "session" => {
            let bytes = dir_byte_size(&dir);
            match std::fs::remove_dir_all(&dir) {
                Ok(_) => {
                    serde_json::json!({"ok": true, "deleted": "session", "bytes_freed": bytes})
                        .to_string()
                }
                Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
            }
        }
        "media" => {
            let rec_dir = dir.join("recordings");
            let frames_dir = dir.join("frames");
            let bytes = dir_byte_size(&rec_dir) + dir_byte_size(&frames_dir);
            let mut errors = Vec::new();
            if rec_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&rec_dir) {
                    errors.push(format!("recordings: {}", e));
                }
            }
            if frames_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&frames_dir) {
                    errors.push(format!("frames: {}", e));
                }
            }
            if errors.is_empty() {
                serde_json::json!({"ok": true, "deleted": "media", "bytes_freed": bytes})
                    .to_string()
            } else {
                serde_json::json!({"ok": false, "error": errors.join("; "), "bytes_freed": bytes})
                    .to_string()
            }
        }
        "recordings" | "frames" | "turns" => {
            let target_dir = dir.join(target);
            let bytes = dir_byte_size(&target_dir);
            if !target_dir.is_dir() {
                serde_json::json!({"ok": true, "deleted": target, "bytes_freed": 0}).to_string()
            } else {
                match std::fs::remove_dir_all(&target_dir) {
                    Ok(_) => {
                        serde_json::json!({"ok": true, "deleted": target, "bytes_freed": bytes})
                            .to_string()
                    }
                    Err(e) => {
                        serde_json::json!({"ok": false, "error": e.to_string(), "bytes_freed": 0})
                            .to_string()
                    }
                }
            }
        }
        _ => serde_json::json!({"ok": false, "error": "invalid target"}).to_string(),
    }
}

/// Settings payload for GET/POST /api/settings.
/// Flattened view of intendant.toml sections relevant to the web dashboard.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsPayload {
    // Computer Use
    pub cu_provider: Option<String>,
    pub cu_model: Option<String>,
    pub cu_backend: String,
    // Presence
    pub presence_enabled: bool,
    pub presence_provider: Option<String>,
    pub presence_model: Option<String>,
    pub presence_live_provider: Option<String>,
    pub presence_live_model: Option<String>,
    // Transcription
    pub transcription_enabled: bool,
    pub transcription_provider: String,
    pub transcription_model: String,
    pub transcription_endpoint: Option<String>,
    pub transcription_language: Option<String>,
    // Recording
    pub recording_enabled: bool,
    pub recording_framerate: u32,
    pub recording_quality: String,
    // Live Audio
    pub live_audio_enabled: bool,
    pub live_audio_timeout_secs: u64,
    // External agent default (persisted to `[agent] default_backend`).
    // Values: "codex" | "claude-code" | "gemini" | None (internal agent).
    #[serde(default)]
    pub external_agent: Option<String>,
    // Codex runtime config (persisted to `[agent.codex]`). Mirrored here so
    // the Activity → Control sub-tab can load in one fetch.
    #[serde(default)]
    pub codex_command: Option<String>,
    #[serde(default = "default_settings_codex_sandbox")]
    pub codex_sandbox: String,
    #[serde(default = "default_settings_codex_approval_policy")]
    pub codex_approval_policy: String,
    #[serde(default)]
    pub codex_model: Option<String>,
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,
    #[serde(default)]
    pub codex_web_search: bool,
    #[serde(default)]
    pub codex_network_access: bool,
    #[serde(default)]
    pub codex_writable_roots: Vec<String>,
    // Other external-agent executable commands. The Settings pane does not
    // edit these today, but the New Session pane uses them as per-launch
    // command/path defaults.
    #[serde(default)]
    pub claude_command: Option<String>,
    #[serde(default)]
    pub gemini_command: Option<String>,
    // Gemini runtime config (persisted to `[agent.gemini_cli]`). Mirrors
    // the Codex fields above for the Activity → Control sub-tab.
    #[serde(default)]
    pub gemini_model: Option<String>,
    #[serde(default = "default_settings_gemini_approval_mode")]
    pub gemini_approval_mode: String,
    #[serde(default)]
    pub gemini_sandbox: bool,
    #[serde(default)]
    pub gemini_extensions: Vec<String>,
    #[serde(default)]
    pub gemini_allowed_mcp_servers: Vec<String>,
    #[serde(default)]
    pub gemini_include_directories: Vec<String>,
    #[serde(default)]
    pub gemini_debug: bool,
    // Env var overrides (read-only, shown in UI)
    #[serde(default)]
    pub env_overrides: std::collections::HashMap<String, String>,
}

fn default_settings_codex_sandbox() -> String {
    crate::project::normalize_sandbox_mode("")
}

fn default_settings_codex_approval_policy() -> String {
    crate::project::normalize_approval_policy("")
}

fn normalize_settings_codex_command(input: Option<&str>) -> String {
    normalize_settings_agent_command(input, "codex")
}

fn normalize_settings_agent_command(input: Option<&str>, fallback: &str) -> String {
    let trimmed = input.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn default_settings_gemini_approval_mode() -> String {
    crate::project::normalize_gemini_approval_mode("")
}

fn settings_payload_from_config(config: &crate::project::ProjectConfig) -> SettingsPayload {
    let mut env_overrides = std::collections::HashMap::new();
    for (key, var) in [
        ("CU_PROVIDER", "CU_PROVIDER"),
        ("CU_MODEL", "CU_MODEL"),
        ("PRESENCE_PROVIDER", "PRESENCE_PROVIDER"),
        ("PRESENCE_MODEL", "PRESENCE_MODEL"),
        ("PROVIDER", "PROVIDER"),
        ("MODEL_NAME", "MODEL_NAME"),
    ] {
        if let Ok(val) = std::env::var(var) {
            env_overrides.insert(key.to_string(), val);
        }
    }
    SettingsPayload {
        cu_provider: config.computer_use.provider.clone(),
        cu_model: config.computer_use.model.clone(),
        cu_backend: config.computer_use.backend.clone(),
        presence_enabled: config.presence.enabled,
        presence_provider: config.presence.provider.clone(),
        presence_model: config.presence.model.clone(),
        presence_live_provider: config.presence.live_provider.clone(),
        presence_live_model: config.presence.live_model.clone(),
        transcription_enabled: config.transcription.enabled,
        transcription_provider: config.transcription.provider.clone(),
        transcription_model: config.transcription.model.clone(),
        transcription_endpoint: config.transcription.endpoint.clone(),
        transcription_language: config.transcription.language.clone(),
        recording_enabled: config.recording.enabled,
        recording_framerate: config.recording.framerate,
        recording_quality: config.recording.quality.clone(),
        live_audio_enabled: config.live_audio.enabled,
        live_audio_timeout_secs: config.live_audio.default_timeout_secs,
        external_agent: config.agent.default_backend.clone(),
        codex_command: Some(config.agent.codex.command.clone()),
        codex_sandbox: crate::project::normalize_sandbox_mode(&config.agent.codex.sandbox),
        codex_approval_policy: crate::project::normalize_approval_policy(
            &config.agent.codex.approval_policy,
        ),
        codex_model: config.agent.codex.model.clone(),
        codex_reasoning_effort: crate::project::normalize_reasoning_effort(
            config.agent.codex.reasoning_effort.as_deref(),
        ),
        codex_web_search: config.agent.codex.web_search,
        codex_network_access: config.agent.codex.network_access,
        codex_writable_roots: config.agent.codex.writable_roots.clone(),
        claude_command: Some(config.agent.claude_code.command.clone()),
        gemini_command: Some(config.agent.gemini_cli.command.clone()),
        gemini_model: config.agent.gemini_cli.model.clone(),
        gemini_approval_mode: crate::project::normalize_gemini_approval_mode(
            &config.agent.gemini_cli.approval_mode,
        ),
        gemini_sandbox: config.agent.gemini_cli.sandbox,
        gemini_extensions: config.agent.gemini_cli.extensions.clone(),
        gemini_allowed_mcp_servers: config.agent.gemini_cli.allowed_mcp_servers.clone(),
        gemini_include_directories: config.agent.gemini_cli.include_directories.clone(),
        gemini_debug: config.agent.gemini_cli.debug,
        env_overrides,
    }
}

fn apply_settings_payload(config: &mut crate::project::ProjectConfig, payload: &SettingsPayload) {
    config.computer_use.provider = payload.cu_provider.clone();
    config.computer_use.model = payload.cu_model.clone();
    config.computer_use.backend = payload.cu_backend.clone();
    config.presence.enabled = payload.presence_enabled;
    config.presence.provider = payload.presence_provider.clone();
    config.presence.model = payload.presence_model.clone();
    config.presence.live_provider = payload.presence_live_provider.clone();
    config.presence.live_model = payload.presence_live_model.clone();
    config.transcription.enabled = payload.transcription_enabled;
    config.transcription.provider = payload.transcription_provider.clone();
    config.transcription.model = payload.transcription_model.clone();
    config.transcription.endpoint = payload.transcription_endpoint.clone();
    config.transcription.language = payload.transcription_language.clone();
    config.recording.enabled = payload.recording_enabled;
    config.recording.framerate = payload.recording_framerate;
    config.recording.quality = payload.recording_quality.clone();
    config.live_audio.enabled = payload.live_audio_enabled;
    config.live_audio.default_timeout_secs = payload.live_audio_timeout_secs;
    // Normalize empty strings to None so the TOML doesn't end up with
    // `default_backend = ""` — the loader treats "" as a valid override
    // and would try to resolve it to a backend.
    config.agent.default_backend =
        payload
            .external_agent
            .as_ref()
            .and_then(|s| if s.is_empty() { None } else { Some(s.clone()) });
    if payload.codex_command.is_some() {
        config.agent.codex.command =
            normalize_settings_codex_command(payload.codex_command.as_deref());
    }
    if payload.claude_command.is_some() {
        config.agent.claude_code.command =
            normalize_settings_agent_command(payload.claude_command.as_deref(), "claude");
    }
    if payload.gemini_command.is_some() {
        config.agent.gemini_cli.command =
            normalize_settings_agent_command(payload.gemini_command.as_deref(), "gemini");
    }
}

/// Return JSON with boolean flags indicating which API keys are configured.
fn get_api_key_status_json() -> String {
    let openai = std::env::var("OPENAI_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let anthropic = std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let gemini = std::env::var("GEMINI_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    serde_json::json!({
        "openai": openai,
        "anthropic": anthropic,
        "gemini": gemini,
    })
    .to_string()
}

/// Payload for POST /api/api-keys.
#[derive(serde::Deserialize)]
struct SetApiKeysPayload {
    keys: std::collections::HashMap<String, String>,
}

/// Handle POST /api/api-keys: persist keys to ~/.config/intendant/.env and
/// set them in the current process.
fn handle_set_api_keys(body: &str) -> String {
    let payload: SetApiKeysPayload = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({"error": format!("Invalid payload: {}", e)}).to_string();
        }
    };

    // Only allow known key names.
    const ALLOWED: &[&str] = &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];
    for key in payload.keys.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return serde_json::json!({"error": format!("Unknown key: {}", key)}).to_string();
        }
    }

    // Resolve config dir.
    let config_dir = match dirs::config_dir() {
        Some(d) => d.join("intendant"),
        None => {
            return serde_json::json!({"error": "Cannot determine config directory"}).to_string();
        }
    };

    // Ensure the directory exists.
    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        return serde_json::json!({"error": format!("Cannot create config dir: {}", e)})
            .to_string();
    }

    let env_path = config_dir.join(".env");

    // Read existing content (may not exist yet).
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();

    // Build updated content: replace existing lines, append new ones.
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut written_keys = std::collections::HashSet::new();

    for line in &mut lines {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let var_name = trimmed[..eq_pos].trim().to_string();
            if let Some(new_val) = payload.keys.get(&var_name) {
                *line = format!("{}={}", var_name, new_val);
                written_keys.insert(var_name);
            }
        }
    }

    // Append keys that weren't already in the file.
    for (key, val) in &payload.keys {
        if !written_keys.contains(key.as_str()) {
            lines.push(format!("{}={}", key, val));
        }
    }

    let new_content = lines.join("\n") + "\n";

    // Atomic write: temp file + rename.
    let tmp_path = config_dir.join(".env.tmp");
    if let Err(e) = std::fs::write(&tmp_path, &new_content) {
        return serde_json::json!({"error": format!("Write failed: {}", e)}).to_string();
    }
    if let Err(e) = std::fs::rename(&tmp_path, &env_path) {
        return serde_json::json!({"error": format!("Rename failed: {}", e)}).to_string();
    }

    // Set env vars in the current process so future provider instantiations
    // pick them up without requiring a restart.
    for (key, val) in &payload.keys {
        std::env::set_var(key, val);
    }

    serde_json::json!({"ok": true}).to_string()
}

// ---------------------------------------------------------------------------
// MCP-over-HTTP (Streamable HTTP) types
// ---------------------------------------------------------------------------
//
// rmcp's Streamable HTTP transport expects:
//   - Requests (with `id`):   200 OK + application/json body
//   - Notifications (no `id`): 202 Accepted + empty body
//
// Returning 200+JSON for notifications causes rmcp to try deserializing the
// body as ServerJsonRpcMessage, which fails because there's no valid `id`.

#[derive(Deserialize)]
struct McpHttpRequest {
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct McpHttpResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpHttpError>,
}

#[derive(Serialize)]
struct McpHttpError {
    code: i64,
    message: String,
}

/// Result from handling an MCP-over-HTTP request.
enum McpHttpOutcome {
    /// JSON-RPC response (requests with `id`) -- return 200 OK + JSON body.
    Response(McpHttpResponse),
    /// Notification acknowledged -- return 202 Accepted with empty body.
    Accepted,
}

async fn handle_mcp_http_request(
    body: &str,
    server: &crate::mcp::IntendantServer,
) -> McpHttpOutcome {
    let request: McpHttpRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return McpHttpOutcome::Response(McpHttpResponse {
                jsonrpc: "2.0".into(),
                id: None,
                result: None,
                error: Some(McpHttpError {
                    code: -32700,
                    message: format!("Parse error: {}", e),
                }),
            });
        }
    };

    // JSON-RPC notifications have no `id` and expect no response body.
    // The MCP Streamable HTTP spec requires 202 Accepted for these.
    let is_notification = request.id.is_none();

    let result = match request.method.as_str() {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "intendant",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "notifications/initialized"
        | "notifications/cancelled"
        | "notifications/progress"
        | "notifications/roots/list_changed" => {
            // All notification methods: acknowledge and return 202.
            return McpHttpOutcome::Accepted;
        }
        "tools/list" => Ok(server.list_tools_json()),
        "tools/call" => {
            let params = request.params.unwrap_or_default();
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            match server.call_tool_by_name(name, args).await {
                Ok(result) => Ok(serde_json::to_value(result).unwrap_or_else(|e| {
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Failed to serialize MCP tool result: {}", e),
                        }],
                        "isError": true,
                    })
                })),
                Err(e) => Err(McpHttpError {
                    code: -32603,
                    message: e,
                }),
            }
        }
        other => {
            // Unknown notification (no id): accept silently per spec.
            if is_notification {
                return McpHttpOutcome::Accepted;
            }
            Err(McpHttpError {
                code: -32601,
                message: format!("Method not found: {}", other),
            })
        }
    };

    McpHttpOutcome::Response(McpHttpResponse {
        jsonrpc: "2.0".into(),
        id: request.id,
        result: result.as_ref().ok().cloned(),
        error: result.err(),
    })
}

pub fn spawn_web_gateway(
    listener: TcpListener,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    shared_session: SharedActiveSession,
    transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
    task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,
    project_root: Option<std::path::PathBuf>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    peer_registry: Option<crate::peer::PeerRegistry>,
    advertise_urls: Vec<String>,
    // Inbound bearer token enforcement. When `Some`, federation REST
    // endpoints (/api/peers*, /api/coordinator/*, /api/sessions)
    // require `Authorization: Bearer <token>` matching the configured
    // value; missing or wrong token returns 401. When `None`, no
    // application-layer auth is enforced — the operator's expected to
    // rely on transport security (mTLS proxy, tailnet, loopback).
    // Sourced from `[server.auth] bearer_token` in intendant.toml.
    //
    // /ws, /.well-known/agent-card.json, /config, the dashboard HTML,
    // and static assets are intentionally exempt in this slice — /ws
    // enforcement requires a parallel dashboard auth flow (browser
    // can't easily set headers on `WebSocket` opens) which lands in
    // slice 2d.
    inbound_bearer_token: Option<String>,
    // What to advertise in the local Agent Card's `auth` field —
    // tells connecting peers what wire-layer (transport) and
    // application-layer (bearer) auth they need to satisfy.
    // Built by `crate::main::build_local_advertised_auth` from
    // `[server.auth] advertised_transport` (`"none"` /
    // `"mutual-tls"` / `"pin-self-cert"`) and
    // `[server.auth] bearer_token`. The `pin-self-cert` path reads
    // the daemon's own `server.crt` from the LAN cert dir and
    // pre-fills the fingerprint so operators don't have to compute
    // it manually.
    //
    // Test call sites pass `AuthRequirements::none()` since they
    // don't exercise the advertise path; production call sites in
    // main.rs build the requirements from the project config.
    local_card_auth: crate::peer::AuthRequirements,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());

    // Build the local Agent Card from live runtime state so
    // `/.well-known/agent-card.json` can serve it. The transport URLs
    // come from [`resolve_advertise_urls`], which uses operator
    // overrides verbatim when provided and otherwise falls back to a
    // single auto-detected URL derived from the listener's bind
    // address. Multiple URLs let one daemon advertise itself reachable
    // via several paths (LAN IP, Tailscale, host port-forward, etc.)
    // — the connecting peer probes them in order.
    let advertise_urls = resolve_advertise_urls(listener.local_addr().ok(), &advertise_urls);
    let agent_card = build_local_agent_card(advertise_urls, local_card_auth);
    let agent_card_json = serde_json::to_string(&agent_card).unwrap_or_else(|_| "{}".to_string());

    // Pre-build ICE config for WebRTC display sessions from the gateway config.
    let ice_config = crate::display::IceConfig {
        ice_servers: config.ice_servers.clone(),
    };

    // Shared ICE-TCP peer registry + advertised TCP port.
    //
    // We multiplex ICE-TCP onto the HTTP listener port: the per-connection
    // accept handler (later in this function) peeks every accepted TCP
    // connection's first bytes to tell HTTP vs. WebSocket vs. STUN-framed
    // traffic apart. STUN traffic is read through one RFC 4571 frame and
    // handed to this registry, which demuxes to the matching peer by the
    // STUN USERNAME's local-ufrag half. The advertised TCP candidate port
    // is the HTTP port itself, so ICE-TCP flows through the exact same
    // tunnel/port-forward that already carries the dashboard — users
    // don't configure anything extra beyond what the dashboard already
    // requires.
    let http_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let tcp_peer_registry = crate::display::webrtc::TcpPeerRegistry::new();
    let tcp_advertised_port: Option<u16> = if http_port != 0 {
        Some(http_port)
    } else {
        None
    };

    // Slice 3b: TCP relay registry for primary-as-media-relay. When
    // a federated WebRTC `Answer` flows from a peer back to the
    // browser, the translator (below) extracts the peer's ICE ufrag
    // from the SDP, resolves the peer's outbound TCP address, and
    // registers the mapping here. The accept loop (below) then
    // dispatches incoming STUN-framed TCP connections whose ufrag
    // matches an entry to the relay byte-forwarding path instead of
    // the local WebRtcPeer path — the primary opens a fresh TCP
    // connection to the peer and shuttles bytes between browser and
    // peer until either side closes. Browser ICE treats this as a
    // TCP candidate alongside the peer's direct candidate; direct
    // wins on reachable topologies, relay covers the browser-can-
    // only-reach-primary case (e.g. hypervisor-isolated VMs).
    let tcp_relay_registry = crate::display::webrtc::TcpRelayRegistry::new();

    // Primary's relay TCP URL, used to inject a relay candidate into
    // forwarded `Answer` SDPs. Derived from the agent card's first
    // IntendantWs transport — that's the URL the primary advertises
    // to peers, which on most deployments is also what browsers use
    // to reach the primary. Stored as a string so DNS resolution
    // happens lazily at per-Answer rewrite time rather than once at
    // startup (hostnames may not resolve at boot for Tailscale /
    // mDNS / etc).
    let relay_advertise_url: Option<String> = agent_card.transports.iter().find_map(|t| match t {
        crate::peer::TransportSpec::IntendantWs { url } => Some(url.clone()),
        _ => None,
    });

    // Inject content-hash version into WASM/JS URLs for cache-busting.
    let v = asset_version_hash();
    let session_provider = config.provider.clone();
    let session_model = config.model.clone();
    let voice_debug = Arc::new(Mutex::new(VoiceDebugState::default()));
    let active_presence: Arc<Mutex<Option<ActivePresence>>> = Arc::new(Mutex::new(None));
    // Per-display input authority (phase 5).  Entry absence = unclaimed
    // (any connection can input — pre-phase-5 default); entry presence =
    // exclusive ownership by that one `connection_id`.
    //
    // Synchronous `StdRwLock` (5a.1): the WebRTC data-channel input
    // handler in `display/mod.rs::handle_offer_pool_mode` is an
    // `Arc<dyn Fn(InputEvent) + Send + Sync>` invoked from rtc's sync
    // receive context, and reads this map through the per-peer
    // `input_authorized` closure each time an event arrives.  Tokio's
    // RwLock can't be read from sync code without `block_on`; std's
    // can.  The map is small, write-rare (grant/release/WS-close only),
    // read-frequent on the hot input path; std::sync::RwLock is the
    // correct lock here.
    let display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>> =
        Arc::new(StdRwLock::new(HashMap::new()));

    // Phase 5a.1 authority transition channel.  Each per-connection
    // outbound task subscribes; emit sites are the Request/Release
    // ControlMsg handlers, the WS-close cleanup, and the DisplayReady
    // listener that fires `holder: None` for freshly
    // created display sessions so already-connected browsers move
    // from `unknown` to `unclaimed`.
    let (authority_change_tx, _authority_change_rx0) =
        broadcast::channel::<DisplayInputAuthorityChange>(AUTHORITY_CHANGE_CAPACITY);

    // F-1.3b3 federated authority subscribers. Federated counterpart
    // to local 5c's per-WS subscriber loop: federated browsers don't
    // share the local 5c WS path, so the gateway needs an explicit
    // registry of `(federation_connection_id, session_id, display_id)`
    // → `WebRtcPeer` to fan personalized state out to. Owned here at
    // gateway scope so cleanup edges (federated `Close`, federation
    // WS close) can locate entries by either single-identity or
    // bulk-by-connection key. See the F-1.3b3 helpers above.
    let federated_authority_subscribers: FederatedAuthoritySubscribers =
        Arc::new(StdRwLock::new(HashMap::new()));

    // Spawn a listener that fires an "unclaimed" authority change for
    // every newly-created display session so already-connected browsers'
    // chips flip from `unknown` to `unclaimed` without waiting for the
    // first Request/Release.  Subscribes to the broadcast_tx event
    // stream (already serialized JSON) and pattern-matches on
    // `display_ready` rather than the typed AppEvent — same source the
    // existing `display_ready_cache` task uses, keeps the dependency
    // surface small.
    {
        let authority_change_tx = authority_change_tx.clone();
        let mut display_events_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match display_events_rx.recv().await {
                    Ok(line) => {
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                let _ = authority_change_tx.send(DisplayInputAuthorityChange {
                                    display_id: did,
                                    holder: None,
                                });
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    // Process-wide registry of standalone shell PTY sessions, keyed by
    // (host_id, terminal_id). Lives as long as the web gateway task and
    // is cloned into each per-connection handler so WS reconnects reattach
    // to existing shells. Keyed on host_id even though there's only one
    // host today so multi-host phase 1 can add siblings without refactor.
    let terminal_registry: Arc<crate::terminal::TerminalRegistry> = Arc::new(
        crate::terminal::TerminalRegistry::new(project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })),
    );

    // Cache the latest usage_update JSON so late-connecting browsers get it
    // without sending ControlMsg (which would pollute the event log).
    let last_usage_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest live_usage_update JSON for late-connecting browsers.
    let last_live_usage_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest status event (has autonomy, session_id, task).
    let last_status_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache standalone autonomy changes so reconnecting dashboards do not
    // fall back to the stale autonomy value in the latest status event.
    let last_autonomy_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest external_agent_changed event so a refreshed
    // browser learns the current value without having to re-fetch
    // settings. Without this the dashboard dropdown snaps back to
    // "None (internal agent)" on every page refresh even though the
    // daemon still has the value in memory.
    let last_external_agent_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the current externally-attached session so refreshed browsers
    // can rehydrate Activity with the same compact transcript shown in the
    // Sessions tab instead of coming back empty.
    let last_session_attached_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache the latest user_display_granted event. The authoritative
    // state lives in AutonomyState.user_display_granted on the server,
    // but the dashboard only learns about it via the broadcast; without
    // this cache a refreshed browser shows "off" regardless of whether
    // the user has actually granted access. Cleared on user_display_revoked
    // so a stale grant doesn't get replayed after the user revokes.
    let last_user_display_json: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Cache display_ready JSON per display_id for late-connecting browsers.
    // Using a HashMap so multiple concurrent display sessions are all replayed.
    let display_ready_cache: Arc<Mutex<HashMap<u32, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Cache the most recent worktree inventory scan. Scanning can walk
    // large worktree directories for disk-size accounting, so the
    // dashboard explicitly triggers refreshes instead of doing it on
    // every GET.
    let worktree_inventory_cache: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let usage_cache = last_usage_json.clone();
        let live_usage_cache = last_live_usage_json.clone();
        let status_cache = last_status_json.clone();
        let autonomy_cache = last_autonomy_json.clone();
        let external_agent_cache = last_external_agent_json.clone();
        let session_attached_cache = last_session_attached_json.clone();
        let user_display_cache = last_user_display_json.clone();
        let display_cache = display_ready_cache.clone();
        let mut usage_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match usage_rx.recv().await {
                    Ok(line) => {
                        // Cache display_ready events per display_id for
                        // late-connecting browsers.
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.insert(did, line.clone());
                                }
                            }
                        }
                        // Evict display_ready cache when display is revoked.
                        if line.contains("\"event\":\"user_display_revoked\"")
                            || line.contains("\"event\":\"display_capture_lost\"")
                        {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.remove(&did);
                                }
                            }
                        }
                        // Cache user_display_granted for replay on reconnect.
                        // Clear the cache on user_display_revoked so a refreshed
                        // browser after a revoke doesn't re-enable the badge.
                        if line.contains("\"event\":\"user_display_granted\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"user_display_revoked\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = None;
                            }
                        }
                        if line.contains("\"event\":\"usage_update\"")
                            || line.contains("\"event\":\"usage\"")
                        {
                            if let Ok(mut guard) = usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"live_usage_update\"") {
                            if let Ok(mut guard) = live_usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"status\"") {
                            if let Ok(mut guard) = status_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"autonomy_changed\"") {
                            if let Ok(mut guard) = autonomy_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"external_agent_changed\"") {
                            if let Ok(mut guard) = external_agent_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"session_attached\"") {
                            if let Ok(mut guard) = session_attached_cache.lock() {
                                *guard = external_attached_session_from_wire(&line)
                                    .map(|_| line.clone());
                            }
                        }
                        if line.contains("\"event\":\"session_started\"") {
                            if let Ok(mut guard) = session_attached_cache.lock() {
                                *guard = None;
                            }
                        }
                        if line.contains("\"event\":\"session_ended\"") {
                            if let Some(ended_id) = session_ended_id_from_wire(&line) {
                                if let Ok(mut guard) = session_attached_cache.lock() {
                                    let should_clear = guard
                                        .as_deref()
                                        .and_then(external_attached_session_from_wire)
                                        .map(|(session_id, _)| session_id == ended_id)
                                        .unwrap_or(false);
                                    if should_clear {
                                        *guard = None;
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Peer registry → dashboard push translator.
    //
    // When the registry is wired (the daemon was started with
    // federation enabled), subscribe to its [`RegistryEvent`] stream
    // and translate each event into the matching wire-format
    // [`OutboundEvent`] variant, broadcast over the same channel as
    // every other dashboard event. The browser's existing primary
    // WebSocket pipeline picks them up and updates peer rows in-place
    // without polling `GET /api/peers`.
    //
    // Lagged events are skipped on purpose: the dashboard's recovery
    // path is to re-fetch `/api/peers`, which always returns ground
    // truth. Closed receiver = registry was dropped, exit cleanly.
    if let Some(reg) = peer_registry.as_ref() {
        let mut reg_rx = reg.subscribe();
        let push_tx = broadcast_tx.clone();
        let reg_for_task = reg.clone();
        let relay_registry_for_task = Arc::clone(&tcp_relay_registry);
        let relay_url_for_task = relay_advertise_url.clone();
        let bus_for_task = bus.clone();
        tokio::spawn(async move {
            loop {
                match reg_rx.recv().await {
                    Ok(event) => {
                        let outbound = match event {
                            crate::peer::RegistryEvent::PeerAdded(snap) => {
                                crate::types::OutboundEvent::PeerAdded { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerRemoved(id) => {
                                crate::types::OutboundEvent::PeerRemoved {
                                    id: id.as_str().to_string(),
                                }
                            }
                            crate::peer::RegistryEvent::PeerStateChanged(snap) => {
                                crate::types::OutboundEvent::PeerStateChanged { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerEventForwarded { peer, event } => {
                                // Slice 3b: when a federated Answer
                                // comes back toward the browser, rewrite
                                // the SDP to inject a TCP candidate
                                // pointing at the primary's own relay
                                // address, and register the peer's ufrag
                                // in the relay registry so incoming
                                // browser TCP connections with that
                                // ufrag get forwarded to the peer. Other
                                // event variants pass through verbatim.
                                let rewritten_event = maybe_rewrite_federated_answer(
                                    &peer,
                                    event,
                                    &reg_for_task,
                                    &relay_registry_for_task,
                                    relay_url_for_task.as_deref(),
                                    &bus_for_task,
                                )
                                .await;
                                crate::types::OutboundEvent::PeerEventForwarded {
                                    peer_id: peer.as_str().to_string(),
                                    payload: rewritten_event,
                                }
                            }
                        };
                        crate::control::broadcast_event(&push_tx, &outbound);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let app_html = Arc::new(
        APP_HTML
            .replace(
                "/wasm-web/presence_web.js",
                &format!("/wasm-web/presence_web.js?v={}", v),
            )
            .replace(
                "/wasm-web/presence_web_bg.wasm",
                &format!("/wasm-web/presence_web_bg.wasm?v={}", v),
            )
            .replace("/icon-128.png", &format!("/icon-128.png?v={}", v)),
    );

    tokio::spawn(async move {
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);

        if let Some(p) = tcp_advertised_port {
            eprintln!("[web_gateway] ICE-TCP candidates advertise port {p}");
        }

        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();
            let agent_card_json = agent_card_json.clone();
            let peer_registry = peer_registry.clone();
            let ice_config = ice_config.clone();
            let tcp_peer_registry = Arc::clone(&tcp_peer_registry);
            let tcp_relay_registry = Arc::clone(&tcp_relay_registry);
            let tcp_advertised_port = tcp_advertised_port;
            let shared_session = shared_session.clone();
            let voice_debug = voice_debug.clone();
            let session_provider = session_provider.clone();
            let session_model = session_model.clone();
            let app_html = app_html.clone();
            let transcriber = transcriber.clone();
            let active_presence = active_presence.clone();
            let display_input_authority = display_input_authority.clone();
            let authority_change_tx = authority_change_tx.clone();
            let federated_authority_subscribers = federated_authority_subscribers.clone();
            let last_usage_json = last_usage_json.clone();
            let last_live_usage_json = last_live_usage_json.clone();
            let last_status_json = last_status_json.clone();
            let last_autonomy_json = last_autonomy_json.clone();
            let last_external_agent_json = last_external_agent_json.clone();
            let last_session_attached_json = last_session_attached_json.clone();
            let last_user_display_json = last_user_display_json.clone();
            let display_ready_cache = display_ready_cache.clone();
            let web_tui_tx = web_tui_tx.clone();
            let task_tx = task_tx.clone();
            let project_root = project_root.clone();
            let mcp_server = mcp_server.clone();
            let terminal_registry = terminal_registry.clone();
            let inbound_bearer_token = inbound_bearer_token.clone();
            let worktree_inventory_cache = worktree_inventory_cache.clone();

            tokio::spawn(async move {
                // Snapshot session state at connection time
                let session_snap = shared_session.read().await;
                let daemon_session_id = session_snap.daemon_session_id.clone();
                let query_ctx = session_snap.query_ctx.clone();
                let frame_registry = session_snap.frame_registry.clone();
                let session_log = session_snap.session_log.clone();
                let recording_registry = session_snap.recording_registry.clone();
                let session_registry = session_snap.session_registry.clone();
                let snapshot_dir = session_snap.snapshot_dir.clone();
                let project_root_for_changes = session_snap.project_root_for_changes.clone();
                let file_watcher = session_snap.file_watcher.clone();
                drop(session_snap);
                // Peek at the first bytes to detect (in order):
                //  1. ICE-TCP STUN-framed traffic (RFC 4571 length prefix
                //     followed by a STUN message whose magic cookie
                //     0x2112A442 sits at payload offset 4 = peek offset 6)
                //  2. WebSocket upgrade (HTTP header containing
                //     "Upgrade: websocket")
                //  3. Plain HTTP (everything else)
                //
                // `peek()` does not consume the data, so both the
                // WebSocket handshake and the HTTP parser still get the
                // full request. Only the ICE-TCP branch actually reads
                // (and consumes) the first RFC 4571 frame, after which
                // the rest of the stream is handed to the WebRTC peer's
                // reader task.
                let mut buf = [0u8; 2048];
                let mut stream = stream;
                let n = match stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };

                // ICE-TCP detection: look for a STUN binding request
                // wrapped in an RFC 4571 2-byte BE length prefix. STUN
                // binding request type is 0x0001 (first payload byte < 2),
                // magic cookie is 0x2112A442 at payload offset 4, which
                // lives at peek offset 6..10 once we account for the
                // length prefix. A valid HTTP request never starts with
                // these bytes (method chars are ASCII >= 0x41).
                let looks_like_stun_tcp =
                    n >= 22 && buf[2] < 2 && buf[6..10] == [0x21, 0x12, 0xA4, 0x42];
                if looks_like_stun_tcp {
                    // Consume the first RFC 4571 frame from the stream
                    // (peek leaves it in the kernel buffer; we have to
                    // read it through to hand a clean stream to the peer
                    // reader task).
                    let first_frame =
                        match crate::display::webrtc::read_rfc4571_frame_pub(&mut stream).await {
                            Ok(f) => f,
                            Err(e) => {
                                eprintln!("[web_gateway] ICE-TCP first-frame read failed: {e}");
                                return;
                            }
                        };
                    let remote_addr = match stream.peer_addr() {
                        Ok(a) => a,
                        Err(_) => return,
                    };

                    // Slice 3b dispatch: parse the frame's ufrag once,
                    // then check the local `TcpPeerRegistry` first (for
                    // local WebRtcPeers the daemon owns) and fall
                    // through to the `TcpRelayRegistry` (federated
                    // peers the primary relays to). Unknown ufrag =
                    // close with a diagnostic log.
                    //
                    // Local first keeps the existing behavior
                    // unchanged for non-federated topologies;
                    // relay-as-fallback adds the federation relay
                    // path without touching the local fast path.
                    match crate::display::webrtc::parse_first_frame_ufrag(&first_frame) {
                        Some(ufrag) if tcp_peer_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_peer_registry
                                .route_accepted(stream, first_frame, remote_addr)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP local routing for {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) if tcp_relay_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) =
                                tcp_relay_registry.route_accepted(stream, first_frame).await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP relay routing for ufrag={ufrag} from {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: no route for ufrag {ufrag:?} from {remote_addr} \
                                 (neither local peer nor registered relay)"
                            );
                        }
                        None => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: first frame from {remote_addr} isn't a \
                                 STUN binding request with a parseable USERNAME"
                            );
                        }
                    }
                    return;
                }

                let header_text = String::from_utf8_lossy(&buf[..n]);
                let is_websocket = header_text
                    .lines()
                    .any(|l| l.to_lowercase().contains("upgrade: websocket"));

                // Parse the `Host:` header to learn what address the
                // browser thinks reaches us. We use this later as the IP
                // for ICE-TCP host candidates: Firefox refuses to pair
                // remote loopback candidates, so we need a non-loopback
                // address the browser can actually connect to. The only
                // one we know for sure the browser can reach is whatever
                // it just used to reach us for HTTP — which is exactly
                // what the Host header contains. If the user accessed
                // via a hostname (`localhost`, `myserver.local`) rather
                // than a literal IP, we get None here and skip the TCP
                // candidate entirely; those users can still use UDP if
                // their topology allows it.
                let browser_host_ip: Option<std::net::IpAddr> =
                    extract_host_header_ip(&header_text);

                if is_websocket {
                    // Bearer enforcement on /ws — dual-mode (Authorization
                    // header from daemons, ?token= query param from
                    // browsers). Reject with a plain HTTP 401 *before*
                    // the WebSocket handshake so the rejected client
                    // never sees a successful upgrade.
                    if let Err((status, body)) =
                        verify_bearer_for_ws(&header_text, inbound_bearer_token.as_deref())
                    {
                        use tokio::io::AsyncWriteExt;
                        let response = format!(
                            "HTTP/1.1 {status} Unauthorized\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             WWW-Authenticate: Bearer\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        return;
                    }
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (mut ws_tx, mut ws_rx) = ws_stream.split();
                    let mut outbound_rx = broadcast_tx.subscribe();

                    // Per-connection identity for active/passive tracking
                    let connection_id = uuid::Uuid::new_v4().to_string();

                    // Direct response channel: tool_response and state_snapshot
                    // messages for this specific connection (not broadcast).
                    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<String>();

                    // Register connection with WebTui for per-connection rendering
                    if let Some(ref tx) = web_tui_tx {
                        let _ = tx.send(crate::tui::web::WebTuiCommand::AddConnection {
                            id: connection_id.clone(),
                            direct_tx: direct_tx.clone(),
                            cols: 120,
                            rows: 40,
                        });
                    }

                    // Send bootstrap state snapshot on connect (with connection_id).
                    // Include config (provider/model) since AgentStateSnapshot
                    // doesn't carry those. The top-level `session_id` is the
                    // stable daemon/process session, not the active worker log.
                    let state = query_ctx
                        .as_ref()
                        .map(|ctx| {
                            ctx.agent_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        })
                        .unwrap_or_default();
                    let bootstrap_session_id = daemon_session_id
                        .clone()
                        .or_else(|| {
                            query_ctx
                                .as_ref()
                                .and_then(|ctx| replay_session_id_from_dir(&ctx.log_dir))
                        })
                        .or_else(|| session_log.as_ref().and_then(session_log_id));
                    if query_ctx.is_some() || bootstrap_session_id.is_some() {
                        let config: serde_json::Value =
                            serde_json::from_str(&config_json).unwrap_or_default();
                        let bootstrap = serde_json::json!({
                            "t": "state_snapshot",
                            "state": state,
                            "connection_id": connection_id,
                            "config": config,
                            "session_id": bootstrap_session_id.unwrap_or_default(),
                        });
                        let _ = direct_tx.send(bootstrap.to_string());
                    }

                    // Send cached usage data so late-connecting browsers
                    // populate the Usage tab without sending ControlMsg.
                    if let Ok(guard) = last_usage_json.lock() {
                        if let Some(ref usage_json) = *guard {
                            let _ = direct_tx.send(usage_json.clone());
                        }
                    }

                    // Send cached live usage data.
                    if let Ok(guard) = last_live_usage_json.lock() {
                        if let Some(ref live_json) = *guard {
                            let _ = direct_tx.send(live_json.clone());
                        }
                    }

                    // Send cached status (autonomy, session_id, task).
                    if let Ok(guard) = last_status_json.lock() {
                        if let Some(ref status_json) = *guard {
                            let _ = direct_tx.send(status_json.clone());
                        }
                    }

                    // Send cached autonomy after cached status so it wins
                    // when the latest status event is older than the user's
                    // most recent autonomy switch.
                    if let Ok(guard) = last_autonomy_json.lock() {
                        if let Some(ref autonomy_json) = *guard {
                            let _ = direct_tx.send(autonomy_json.clone());
                        }
                    }

                    // Send cached external_agent_changed so the dropdown
                    // and status badge reflect the current value on a
                    // fresh browser connection.
                    if let Ok(guard) = last_external_agent_json.lock() {
                        if let Some(ref ea_json) = *guard {
                            let _ = direct_tx.send(ea_json.clone());
                        }
                    }

                    // Send cached user_display_granted so the "your display"
                    // status bar toggle reflects the current grant state on
                    // a refreshed browser. Cache is cleared on revoke so
                    // a revoked state simply results in nothing being sent
                    // (the dashboard's HTML default is "off").
                    if let Ok(guard) = last_user_display_json.lock() {
                        if let Some(ref ud_json) = *guard {
                            let _ = direct_tx.send(ud_json.clone());
                        }
                    }

                    // Replay display_ready for every active display session so
                    // late-connecting browsers (including refreshes) recreate
                    // their DisplaySlots and initiate WebRTC.  Prefer the
                    // live session registry over the broadcast cache — it is
                    // authoritative and handles multiple concurrent displays.
                    //
                    // Phase 5a.1: alongside each display_ready, send a
                    // personalized `display_input_authority_state` so the
                    // browser starts at the authoritative state instead of
                    // `unknown`.  Without this snapshot the chip would only
                    // resolve on the next authority transition, which may
                    // be never if no one ever takes control.
                    //
                    // Frame ordering: `display_ready` goes out now (so the
                    // slot exists before any log replay happens); the
                    // per-display `display_input_authority_state` frame is
                    // deferred until *after* `log_replay` below. **#59**:
                    // browser-side `addDisplaySlot` is now idempotent for
                    // an existing live slot, so a replayed historical
                    // `display_ready` no longer destroys the bootstrap
                    // slot. The deferral here is therefore defense-in-
                    // depth against message ordering and late-replay
                    // state — for example a grant→revoke→grant cycle in
                    // session.jsonl whose intermediate `user_display_revoked`
                    // does tear the bootstrap slot down, after which the
                    // replayed re-grant `display_ready` creates a fresh
                    // slot that needs the authority frame to land on it
                    // rather than on the destroyed predecessor. Sending
                    // the authority frame after replay guarantees it lands
                    // on the *final* slot in every replay shape.
                    let bootstrap_authority_snapshots: Vec<(u32, &'static str)> =
                        if let Some(ref sr) = session_registry {
                            let reg = sr.read().await;
                            let active_ids: Vec<u32> = reg.display_ids();
                            // Snapshot resolutions + auth states under the
                            // std lock, then drop the guard before any
                            // direct_tx.send calls.
                            let resolutions: Vec<(u32, u32, u32)> = active_ids
                                .iter()
                                .filter_map(|&did| {
                                    reg.get(did).map(|session| {
                                        let (w, h) = session.resolution();
                                        (did, w, h)
                                    })
                                })
                                .collect();
                            let auth_snapshots = {
                                let auth = display_input_authority
                                    .read()
                                    .unwrap_or_else(|e| e.into_inner());
                                compute_bootstrap_authority_snapshots(
                                    resolutions.iter().map(|(did, _, _)| *did),
                                    &auth,
                                    &connection_id,
                                )
                            };
                            // Send the display_ready frames now; defer the
                            // authority frames until after log_replay.
                            for (did, w, h) in resolutions {
                                let ready = serde_json::json!({
                                    "event": "display_ready",
                                    "display_id": did,
                                    "width": w,
                                    "height": h,
                                });
                                let _ = direct_tx.send(ready.to_string());
                            }
                            auth_snapshots
                        } else {
                            // Fallback: use the broadcast-derived cache when
                            // no session registry is available (shouldn't
                            // happen in practice, but keeps the old
                            // behaviour as safety net).  No authority frame
                            // to send in this branch — the cache only holds
                            // display_ready JSON, no holder state.
                            if let Ok(guard) = display_ready_cache.lock() {
                                for display_json in guard.values() {
                                    let _ = direct_tx.send(display_json.clone());
                                }
                            }
                            Vec::new()
                        };

                    // Replay session log so late-connecting browsers see
                    // historical events (not just real-time from now on).
                    // Each JSONL entry is converted to an OutboundEvent via
                    // session_log_entry_to_app_event → app_event_to_outbound
                    // so replay drives the same rendering path as live.
                    let replay_log_dir =
                        query_ctx
                            .as_ref()
                            .map(|ctx| ctx.log_dir.clone())
                            .or_else(|| {
                                session_log.as_ref().and_then(|sl| {
                                    sl.lock().ok().map(|log| log.dir().to_path_buf())
                                })
                            });
                    if let Some(ref log_dir) = replay_log_dir {
                        if let Some(replay) = session_log_replay_from_dir(log_dir) {
                            let _ = direct_tx.send(replay);
                        }
                    }

                    let active_external_session = last_session_attached_json
                        .lock()
                        .ok()
                        .and_then(|guard| guard.clone())
                        .and_then(|line| external_attached_session_from_wire(&line));
                    if let Some((session_id, source)) = active_external_session {
                        if let Some(replay) = external_session_activity_replay(
                            &source,
                            &session_id,
                            EXTERNAL_ACTIVITY_REPLAY_LIMIT,
                        ) {
                            let _ = direct_tx.send(replay);
                        }
                    }

                    // Phase 5a.1: now that log_replay has finished
                    // recreating display slots from historical events,
                    // send the personalized `display_input_authority_state`
                    // for each currently-active display.  Sending these
                    // before log_replay would land the chip on a slot that
                    // log_replay then destroys (see the slot lifecycle
                    // bookkeeping in `addDisplaySlot` / `removeDisplaySlot`
                    // on the browser side).
                    for (did, state) in bootstrap_authority_snapshots {
                        let auth_msg = serde_json::json!({
                            "t": "display_input_authority_state",
                            "display_id": did,
                            "state": state,
                        });
                        let _ = direct_tx.send(auth_msg.to_string());
                    }

                    // Inbound: WebSocket → EventBus
                    // Handles message types:
                    //   {"t":"key", "key":"Enter", ...}  → AppEvent::Key
                    //   {"t":"resize", "cols":N, "rows":N} → AppEvent::Resize
                    //   {"t":"presence_connect",...}     → AppEvent::PresenceConnected
                    //   {"t":"presence_disconnect"}      → AppEvent::PresenceDisconnected
                    //   {"t":"voice_log",...}             → AppEvent::VoiceLog
                    //   {"t":"presence_checkpoint",...}   → AppEvent::PresenceCheckpointReceived
                    //   {"t":"voice_diagnostic",...}      → AppEvent::VoiceDiagnostic
                    //   {"t":"tool_request", "id":"...", "tool":"...", "args":{}} → tool_response
                    //   {"action":"status", ...}         → AppEvent::ControlCommand
                    // Assign a unique peer ID for WebRTC signaling
                    let peer_id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);

                    let bus_inbound = bus.clone();
                    let query_ctx_inbound = query_ctx.clone();
                    let direct_tx_inbound = direct_tx.clone();
                    let voice_debug_inbound = voice_debug.clone();
                    let live_provider = session_provider.clone();
                    let live_model = session_model.clone();
                    let transcriber_inbound = transcriber.clone();
                    let active_presence_inbound = active_presence.clone();
                    let display_input_authority_inbound = display_input_authority.clone();
                    let authority_change_tx_inbound = authority_change_tx.clone();
                    let federated_authority_subscribers_inbound =
                        federated_authority_subscribers.clone();
                    let connection_id_inbound = connection_id.clone();
                    let web_tui_tx_inbound = web_tui_tx.clone();
                    let frame_registry_inbound = frame_registry.clone();
                    let recording_registry_inbound = recording_registry.clone();
                    let session_log_inbound = session_log.clone();
                    let session_registry_inbound = session_registry.clone();
                    let task_tx_inbound = task_tx.clone();
                    let terminal_registry_inbound = terminal_registry.clone();
                    let inbound = tokio::spawn(async move {
                        // Track whether this connection has an active presence model,
                        // so we can auto-send PresenceDisconnected if the WebSocket drops
                        // without a clean presence_disconnect message (e.g. tab close
                        // before beforeunload fires, network failure).
                        let mut is_presence_connected = false;
                        // Whether this connection is the active voice owner
                        let mut is_active = false;

                        // Per-connection clip accumulators for batched clip_frame messages
                        struct ClipAccumulator {
                            stream: String,
                            note: String,
                            inject: bool,
                            in_secs: f64,
                            out_secs: f64,
                            fps: u32,
                            expected: usize,
                            frames: Vec<(String, String)>, // (frame_id, base64_data)
                        }
                        let mut clip_accumulators: std::collections::HashMap<
                            String,
                            ClipAccumulator,
                        > = std::collections::HashMap::new();

                        // Display IDs this peer has WebRTC connections to,
                        // used for cleanup when the WebSocket disconnects.
                        let mut peer_display_ids: Vec<u32> = Vec::new();

                        // Per-connection audio transcription buffer.
                        // PCM16 bytes are accumulated and drained every ~3s.
                        let mut audio_buf: Vec<u8> = Vec::new();
                        let mut audio_seq: u64 = 0;
                        // Input sample rate (known from config, default 16kHz)
                        let audio_sample_rate: u32 = 16000;

                        while let Some(Ok(msg)) = ws_rx.next().await {
                            if let Message::Text(text) = msg {
                                let trimmed = text.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // Try to parse as JSON for type-tagged messages
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
                                {
                                    match json.get("t").and_then(|v| v.as_str()) {
                                        Some("key") => {
                                            // Route key events to this connection's
                                            // ViewState via WebTuiCommand (not EventBus).
                                            if let Some(key_event) =
                                                crate::tui::web::parse_web_key(&json)
                                            {
                                                if let Some(ref tx) = web_tui_tx {
                                                    let _ = tx.send(
                                                        crate::tui::web::WebTuiCommand::Key {
                                                            id: connection_id_inbound.clone(),
                                                            key: key_event,
                                                        },
                                                    );
                                                } else if is_active {
                                                    // Fallback: no WebTui (headless web mode)
                                                    bus_inbound.send(AppEvent::Key(key_event));
                                                }
                                            }
                                        }
                                        Some("resize") => {
                                            // Route resize to this connection's terminal
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(
                                                    crate::tui::web::WebTuiCommand::Resize {
                                                        id: connection_id_inbound.clone(),
                                                        cols,
                                                        rows,
                                                    },
                                                );
                                            } else if is_active {
                                                bus_inbound.send(AppEvent::Resize(cols, rows));
                                            }
                                        }
                                        Some("term_subscribe") => {
                                            // Dashboard entered the Terminal tab. Start
                                            // emitting ratatui frames to this connection.
                                            // Every non-Terminal tab (Activity, Stats,
                                            // Video, Sessions, Network, Settings, Debug)
                                            // leaves us unsubscribed, which means WebTui
                                            // stays idle instead of flooding the socket
                                            // with frames nobody is watching.
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(
                                                    crate::tui::web::WebTuiCommand::Subscribe {
                                                        id: connection_id_inbound.clone(),
                                                    },
                                                );
                                            }
                                        }
                                        Some("term_unsubscribe") => {
                                            // Dashboard left the Terminal tab. Stop
                                            // emitting ratatui frames to this connection
                                            // until the next term_subscribe.
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(
                                                    crate::tui::web::WebTuiCommand::Unsubscribe {
                                                        id: connection_id_inbound.clone(),
                                                    },
                                                );
                                            }
                                        }
                                        Some("presence_connect") => {
                                            is_presence_connected = true;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = true;
                                            let server_session_id = json
                                                .get("server_session_id")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            let last_event_seq = json
                                                .get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);
                                            // Use provider/model from the browser if sent,
                                            // fall back to config defaults.
                                            let msg_provider = json
                                                .get("provider")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_provider.clone());
                                            let msg_model = json
                                                .get("model")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_model.clone());

                                            // Determine if this connection becomes active or passive.
                                            // Browsers can request always-passive mode (observer/follow-along).
                                            let force_passive = json
                                                .get("passive")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(false);
                                            let becomes_active = if force_passive {
                                                false
                                            } else {
                                                let slot = active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                // Empty slot → first connect wins.
                                                // Slot occupied by THIS connection → already active
                                                // (happens when active browser reconnects voice after handover).
                                                slot.is_none()
                                                    || slot
                                                        .as_ref()
                                                        .map(|a| {
                                                            a.connection_id == connection_id_inbound
                                                        })
                                                        .unwrap_or(false)
                                            };

                                            let was_already_active = is_active;
                                            if becomes_active {
                                                // First-connect wins (or re-confirm already-active)
                                                *active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner()) =
                                                    Some(ActivePresence {
                                                        connection_id: connection_id_inbound
                                                            .clone(),
                                                        direct_tx: direct_tx_inbound.clone(),
                                                    });
                                                is_active = true;
                                            }

                                            // Send welcome with replay window if presence session is available
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                // Build conversation context from recent voice transcripts
                                                let conversation_ctx =
                                                    presence::build_conversation_context(
                                                        &ctx.log_dir,
                                                        20,
                                                    );

                                                if let Some(ref ps) = ctx.presence_session {
                                                    let mut session = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    if becomes_active {
                                                        session.set_connected(true);
                                                    }
                                                    let state = ctx
                                                        .agent_state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone();
                                                    let welcome = session
                                                        .build_welcome(last_event_seq, &state);
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "session_id": welcome.session_id,
                                                        "state": welcome.state,
                                                        "events": welcome.events,
                                                        "last_checkpoint_summary": welcome.last_checkpoint_summary,
                                                        "current_seq": welcome.current_seq,
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound
                                                        .send(welcome_msg.to_string());
                                                } else {
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound
                                                        .send(welcome_msg.to_string());
                                                }
                                            } else {
                                                // No presence session — still send a minimal welcome with is_active
                                                let welcome_msg = serde_json::json!({
                                                    "t": "presence_welcome",
                                                    "is_active": becomes_active,
                                                });
                                                let _ =
                                                    direct_tx_inbound.send(welcome_msg.to_string());
                                            }

                                            // Only emit PresenceConnected for the active browser
                                            // (passive browsers don't pause server-side presence).
                                            // Skip if already active (e.g. voice reconnect after make_active
                                            // handover — PresenceConnected was already emitted by make_active).
                                            if becomes_active && !was_already_active {
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_connected(
                                                            Some(&msg_provider),
                                                            Some(&msg_model),
                                                        );
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceConnected {
                                                    server_session_id,
                                                    last_event_seq,
                                                    live_provider: Some(msg_provider),
                                                    live_model: Some(msg_model),
                                                });
                                            }
                                        }
                                        Some("presence_disconnect") => {
                                            is_presence_connected = false;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = false;
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    ps.lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .set_connected(false);
                                                }
                                            }
                                            // Only emit PresenceDisconnected if this was the active browser
                                            if is_active {
                                                // Clear the active slot
                                                let mut slot = active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                if slot
                                                    .as_ref()
                                                    .map(|a| {
                                                        a.connection_id == connection_id_inbound
                                                    })
                                                    .unwrap_or(false)
                                                {
                                                    *slot = None;
                                                }
                                                is_active = false;
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_disconnected();
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceDisconnected);
                                            }
                                        }
                                        Some("make_active") => {
                                            // Request to become the active voice owner
                                            let mut slot = active_presence_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            let previous_active = slot
                                                .as_ref()
                                                .map(|active| active.connection_id.clone());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_received_gateway",
                                                        &format!(
                                                            "request from connection={} previous_active={}",
                                                            connection_id_inbound,
                                                            previous_active.as_deref().unwrap_or("none"),
                                                        ),
                                                    );
                                                }
                                            }

                                            // Tell old active to disconnect voice
                                            if let Some(ref old) = *slot {
                                                if old.connection_id != connection_id_inbound {
                                                    let force_msg = serde_json::json!({
                                                        "t": "force_disconnect_voice",
                                                        "reason": "handover",
                                                    });
                                                    let _ =
                                                        old.direct_tx.send(force_msg.to_string());
                                                    if let Some(ref sl) = session_log_inbound {
                                                        if let Ok(mut l) = sl.lock() {
                                                            l.voice_diagnostic(
                                                                "make_active_force_disconnect_gateway",
                                                                &format!(
                                                                    "old_active={} new_active={}",
                                                                    old.connection_id, connection_id_inbound,
                                                                ),
                                                            );
                                                        }
                                                    }
                                                } else if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.voice_diagnostic(
                                                            "make_active_noop_gateway",
                                                            &format!(
                                                                "request from already-active connection={}",
                                                                connection_id_inbound,
                                                            ),
                                                        );
                                                    }
                                                }
                                            }

                                            // Install this connection as new active
                                            *slot = Some(ActivePresence {
                                                connection_id: connection_id_inbound.clone(),
                                                direct_tx: direct_tx_inbound.clone(),
                                            });
                                            drop(slot);

                                            is_active = true;
                                            is_presence_connected = true;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = true;

                                            // Build handover context from latest checkpoint
                                            let handover_context = query_ctx_inbound
                                                .as_ref()
                                                .and_then(|ctx| ctx.presence_session.as_ref())
                                                .and_then(|ps| {
                                                    let session = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    session.last_checkpoint_summary()
                                                })
                                                .unwrap_or_default();

                                            // Build conversation context from recent voice transcripts
                                            let conversation_ctx =
                                                query_ctx_inbound.as_ref().and_then(|ctx| {
                                                    presence::build_conversation_context(
                                                        &ctx.log_dir,
                                                        20,
                                                    )
                                                });
                                            let has_handover_context = !handover_context.is_empty();
                                            let has_conversation_context = conversation_ctx
                                                .as_deref()
                                                .map(|s| !s.is_empty())
                                                .unwrap_or(false);

                                            // Send active_granted to this connection
                                            let granted_msg = serde_json::json!({
                                                "t": "active_granted",
                                                "is_active": true,
                                                "handover_context": handover_context,
                                                "conversation_context": conversation_ctx,
                                            });
                                            let _ = direct_tx_inbound.send(granted_msg.to_string());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_granted_gateway",
                                                        &format!(
                                                            "connection={} handover_context={} conversation_context={}",
                                                            connection_id_inbound,
                                                            if has_handover_context { "yes" } else { "no" },
                                                            if has_conversation_context { "yes" } else { "no" },
                                                        ),
                                                    );
                                                }
                                            }

                                            // Emit PresenceConnected for the new active browser
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_connected(
                                                        Some(&live_provider),
                                                        Some(&live_model),
                                                    );
                                                }
                                            }
                                            bus_inbound.send(AppEvent::PresenceConnected {
                                                server_session_id: None,
                                                last_event_seq: 0,
                                                live_provider: Some(live_provider.clone()),
                                                live_model: Some(live_model.clone()),
                                            });
                                        }
                                        Some("voice_log") => {
                                            let text =
                                                json["text"].as_str().unwrap_or("").to_string();
                                            let seq = json["seq"].as_u64().unwrap_or(0);
                                            let tool_context = json
                                                .get("tool_context")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            {
                                                let mut vd = voice_debug_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                vd.voice_log_count += 1;
                                                vd.last_voice_log = text.clone();
                                            }
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_log(
                                                        &text,
                                                        seq,
                                                        tool_context.as_deref(),
                                                    );
                                                }
                                            }
                                            bus_inbound.send(AppEvent::VoiceLog {
                                                text,
                                                seq,
                                                tool_context,
                                            });
                                        }
                                        Some("live_usage_update") => {
                                            bus_inbound.send(AppEvent::LiveUsageUpdate {
                                                provider: json["provider"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                model: json["model"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                input_tokens: json["input_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_tokens: json["output_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_tokens: json["cached_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                total_tokens: json["total_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                thinking_tokens: json["thinking_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_text_tokens: json["input_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_audio_tokens: json["input_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_image_tokens: json["input_image_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_text_tokens: json["cached_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_audio_tokens: json["cached_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_image_tokens: json["cached_image_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_text_tokens: json["output_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_audio_tokens: json["output_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                            });
                                        }
                                        Some("presence_checkpoint") => {
                                            let summary =
                                                json["summary"].as_str().unwrap_or("").to_string();
                                            let last_event_seq = json
                                                .get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);

                                            // Record checkpoint and send ack
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    let checkpoint =
                                                        presence_core::PresenceCheckpoint {
                                                            summary: summary.clone(),
                                                            last_event_seq,
                                                        };
                                                    let ack = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .record_checkpoint(checkpoint);
                                                    let ack_msg = serde_json::json!({
                                                        "t": "presence_checkpoint_ack",
                                                        "seq": ack.seq,
                                                    });
                                                    let _ =
                                                        direct_tx_inbound.send(ack_msg.to_string());
                                                }
                                            }

                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_checkpoint(&summary, last_event_seq);
                                                }
                                            }
                                            bus_inbound.send(
                                                AppEvent::PresenceCheckpointReceived {
                                                    summary,
                                                    last_event_seq,
                                                },
                                            );
                                        }
                                        Some("voice_diagnostic") => {
                                            let kind = json["kind"]
                                                .as_str()
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let detail =
                                                json["detail"].as_str().unwrap_or("").to_string();
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(&kind, &detail);
                                                }
                                            }
                                            bus_inbound
                                                .send(AppEvent::VoiceDiagnostic { kind, detail });
                                        }
                                        Some("user_audio") => {
                                            // Browser sends base64-encoded PCM16 audio for server-side transcription.
                                            if let Some(ref transcriber) = transcriber_inbound {
                                                if let Some(data_b64) = json["data"].as_str() {
                                                    use base64::Engine;
                                                    if let Ok(pcm_bytes) =
                                                        base64::engine::general_purpose::STANDARD
                                                            .decode(data_b64)
                                                    {
                                                        audio_buf.extend_from_slice(&pcm_bytes);
                                                        // Drain at ~3s of audio (16kHz * 2 bytes/sample * 1 channel * 3s = 96000)
                                                        let threshold =
                                                            (audio_sample_rate as usize) * 2 * 3;
                                                        if audio_buf.len() >= threshold {
                                                            // Skip silent buffers — compute RMS energy of PCM16 samples.
                                                            // Whisper hallucinates on silence (outputs "you", ".", etc).
                                                            let rms = {
                                                                let samples = audio_buf
                                                                    .chunks_exact(2)
                                                                    .map(|c| {
                                                                        i16::from_le_bytes([
                                                                            c[0], c[1],
                                                                        ])
                                                                            as f64
                                                                    });
                                                                let sum_sq: f64 =
                                                                    samples.map(|s| s * s).sum();
                                                                let n = audio_buf.len() / 2;
                                                                if n > 0 {
                                                                    (sum_sq / n as f64).sqrt()
                                                                } else {
                                                                    0.0
                                                                }
                                                            };
                                                            if rms < 1000.0 {
                                                                // Below speech threshold — skip transcription.
                                                                // Whisper hallucinates aggressively on low-energy
                                                                // audio ("Thank you", "Bye bye", etc).
                                                                audio_buf.clear();
                                                                continue;
                                                            }
                                                            let wav =
                                                                crate::transcription::encode_wav(
                                                                    &audio_buf,
                                                                    audio_sample_rate,
                                                                    1,
                                                                );
                                                            audio_buf.clear();
                                                            audio_seq += 1;
                                                            let seq = audio_seq;
                                                            let t = transcriber.clone();
                                                            let bus_tx = bus_inbound.clone();
                                                            let session_log_tx =
                                                                session_log_inbound.clone();
                                                            tokio::spawn(async move {
                                                                match t.transcribe(&wav).await {
                                                                    Ok(segment) => {
                                                                        let text = segment
                                                                            .text
                                                                            .trim()
                                                                            .to_string();
                                                                        if !text.is_empty() {
                                                                            if let Some(ref sl) =
                                                                                session_log_tx
                                                                            {
                                                                                if let Ok(mut l) =
                                                                                    sl.lock()
                                                                                {
                                                                                    l.user_transcript(&text, seq);
                                                                                }
                                                                            }
                                                                            bus_tx.send(AppEvent::UserTranscript { text, seq });
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        eprintln!("transcription failed: {}", e);
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("video_frame") => {
                                            // Browser sends a video frame for HQ archival in the frame registry.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("cam0")
                                                .to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    // Register in frame registry
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: true,
                                                            live_resolution: Some(
                                                                "768x768".to_string(),
                                                            ),
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) =
                                                            reg.register(meta, &jpeg_bytes)
                                                        {
                                                            eprintln!(
                                                                "frame registry write failed: {}",
                                                                e
                                                            );
                                                        }
                                                    }
                                                    // Feed into recording pipeline (auto-starts on first frame)
                                                    if let Some(ref rec_reg) =
                                                        recording_registry_inbound
                                                    {
                                                        let mut rreg = rec_reg.write().await;
                                                        if rreg.is_enabled() {
                                                            if !rreg.is_recording(&stream) {
                                                                if crate::recording::is_ffmpeg_available() {
                                                                    if let Err(e) = rreg.start_stream(&stream).await {
                                                                        eprintln!("camera recording start failed: {}", e);
                                                                    } else {
                                                                        bus_inbound.send(AppEvent::RecordingStarted {
                                                                            stream_name: stream.clone(),
                                                                        });
                                                                    }
                                                                }
                                                            }
                                                            let _ = rreg
                                                                .feed_frame(&stream, &jpeg_bytes)
                                                                .await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("annotation_attach") => {
                                            // User clicked "Attach" on an annotation/frame: register
                                            // the JPEG in the frame registry but DO NOT inject into
                                            // the agent context. The browser tracks this frame ID as
                                            // a pending attachment and submits it with the next task.
                                            //
                                            // Works regardless of presence/agent state — attachments
                                            // are independent of any running task.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("annotation")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    let mut saved_path = String::new();
                                                    let mut registered = false;
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() {
                                                                None
                                                            } else {
                                                                Some(note.clone())
                                                            },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => {
                                                                saved_path = path.display().to_string();
                                                                registered = true;
                                                            }
                                                            Err(e) => eprintln!("annotation_attach frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    let _ = direct_tx_inbound.send(
                                                        serde_json::json!({
                                                            "t": "annotation_attached",
                                                            "frame_id": frame_id,
                                                            "stream": stream,
                                                            "path": saved_path,
                                                            "note": note,
                                                            "ok": registered,
                                                        })
                                                        .to_string(),
                                                    );
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} attached (pending)",
                                                            frame_id
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("annotation_submit") => {
                                            // User drew annotations on a frame and submitted it with a note.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("annotation")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    // Register in frame registry
                                                    let mut saved_path = String::new();
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() {
                                                                None
                                                            } else {
                                                                Some(note.clone())
                                                            },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => saved_path = path.display().to_string(),
                                                            Err(e) => eprintln!("annotation frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    // Optionally inject into agent conversation
                                                    let mut injected_to_queue = false;
                                                    if inject {
                                                        if let Some(ref ctx) = query_ctx_inbound {
                                                            if let Some(ref ciq) =
                                                                ctx.context_injection
                                                            {
                                                                if let Ok(mut q) = ciq.lock() {
                                                                    let label = if note.is_empty() {
                                                                        "[User Annotation] User highlighted something on the screen.".to_string()
                                                                    } else {
                                                                        format!(
                                                                            "[User Annotation] {}",
                                                                            note
                                                                        )
                                                                    };
                                                                    q.push(crate::event::ContextInjection {
                                                                        text: label,
                                                                        images: vec![crate::conversation::ImageData {
                                                                            media_type: "image/jpeg".to_string(),
                                                                            data: data_b64.to_string(),
                                                                        }],
                                                                        source: crate::event::InjectionSource::User,
                                                                        steer_id: None,
                                                                    });
                                                                    injected_to_queue = true;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    // Send path back to browser. Report whether the injection
                                                    // actually landed in the queue (not just whether the user
                                                    // pressed Send), so the UI doesn't lie when no presence is
                                                    // running.
                                                    let _ = direct_tx_inbound.send(
                                                        serde_json::json!({
                                                            "t": "annotation_saved",
                                                            "frame_id": frame_id,
                                                            "path": saved_path,
                                                            "injected": injected_to_queue,
                                                        })
                                                        .to_string(),
                                                    );
                                                    let status_label = if inject {
                                                        if injected_to_queue {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved — no agent connected)"
                                                        }
                                                    } else {
                                                        ""
                                                    };
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} on {}{}",
                                                            frame_id, stream, status_label
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("clip_start") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("recording")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            let in_secs = json["in_secs"].as_f64().unwrap_or(0.0);
                                            let out_secs = json["out_secs"].as_f64().unwrap_or(0.0);
                                            let fps = json["fps"].as_u64().unwrap_or(2) as u32;
                                            let total =
                                                json["total_frames"].as_u64().unwrap_or(0) as usize;
                                            clip_accumulators.insert(
                                                clip_id.clone(),
                                                ClipAccumulator {
                                                    stream,
                                                    note,
                                                    inject,
                                                    in_secs,
                                                    out_secs,
                                                    fps,
                                                    expected: total,
                                                    frames: Vec::with_capacity(total),
                                                },
                                            );
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[clip] started {} ({} frames, {}fps)",
                                                    clip_id, total, fps
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });
                                        }
                                        Some("clip_frame") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let timestamp_secs =
                                                json["timestamp_secs"].as_f64().unwrap_or(0.0);
                                            if let Some(data_b64) = json["data"].as_str() {
                                                // Register frame in frame registry
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: format!("clip:{}", clip_id),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) =
                                                            reg.register(meta, &jpeg_bytes)
                                                        {
                                                            eprintln!("clip frame registry write failed: {}", e);
                                                        }
                                                    }
                                                }
                                                // Accumulate for context injection
                                                if let Some(acc) =
                                                    clip_accumulators.get_mut(&clip_id)
                                                {
                                                    acc.frames
                                                        .push((frame_id, data_b64.to_string()));
                                                }
                                            }
                                        }
                                        Some("clip_end") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let frames_sent =
                                                json["frames_sent"].as_u64().unwrap_or(0) as usize;
                                            let mut injected = false;

                                            if let Some(acc) = clip_accumulators.remove(&clip_id) {
                                                let frames_registered = acc.frames.len();
                                                if acc.inject {
                                                    if let Some(ref ctx) = query_ctx_inbound {
                                                        if let Some(ref ciq) = ctx.context_injection
                                                        {
                                                            if let Ok(mut q) = ciq.lock() {
                                                                let label = if acc.note.is_empty() {
                                                                    format!(
                                                                        "[Video Clip] {} {}-{} ({} frames, {}fps)",
                                                                        acc.stream,
                                                                        format!("{:.1}s", acc.in_secs),
                                                                        format!("{:.1}s", acc.out_secs),
                                                                        frames_registered, acc.fps,
                                                                    )
                                                                } else {
                                                                    format!(
                                                                        "[Video Clip] {} {}-{} ({} frames, {}fps). {}",
                                                                        acc.stream,
                                                                        format!("{:.1}s", acc.in_secs),
                                                                        format!("{:.1}s", acc.out_secs),
                                                                        frames_registered, acc.fps, acc.note,
                                                                    )
                                                                };
                                                                let images: Vec<crate::conversation::ImageData> = acc.frames.iter().map(|(_, b64)| {
                                                                    crate::conversation::ImageData {
                                                                        media_type: "image/jpeg".to_string(),
                                                                        data: b64.clone(),
                                                                    }
                                                                }).collect();
                                                                q.push(crate::event::ContextInjection {
                                                                    text: label,
                                                                    images,
                                                                    source: crate::event::InjectionSource::User,
                                                                    steer_id: None,
                                                                });
                                                                injected = true;
                                                            }
                                                        }
                                                    }
                                                }

                                                let _ = direct_tx_inbound.send(
                                                    serde_json::json!({
                                                        "t": "clip_saved",
                                                        "clip_id": clip_id,
                                                        "frames_registered": frames_registered,
                                                        "injected": injected,
                                                    })
                                                    .to_string(),
                                                );

                                                bus_inbound.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "[clip] {} — {} frames{}",
                                                        clip_id,
                                                        frames_registered,
                                                        if injected {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved)"
                                                        }
                                                    ),
                                                    level: Some(LogLevel::Info),
                                                    turn: None,
                                                });
                                            }
                                        }
                                        Some("tool_request") => {
                                            let req_id =
                                                json["id"].as_str().unwrap_or("").to_string();
                                            let tool =
                                                json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned().unwrap_or(
                                                serde_json::Value::Object(Default::default()),
                                            );

                                            // Log the incoming tool request at Debug level
                                            let args_preview = {
                                                let s = serde_json::to_string(&args)
                                                    .unwrap_or_default();
                                                preview_text(&s, 200)
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[tool_request] {}({})",
                                                    tool, args_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            // Dispatch through presence-core (single canonical layer)
                                            let state = query_ctx_inbound
                                                .as_ref()
                                                .map(|ctx| {
                                                    ctx.agent_state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone()
                                                })
                                                .unwrap_or_default();
                                            let action =
                                                presence::dispatch_tool_call(&tool, &args, &state);

                                            // SubmitTask: send directly to task_tx (bypasses TUI)
                                            let query_result =
                                                if let presence::PresenceAction::SubmitTask(
                                                    envelope,
                                                ) = action
                                                {
                                                    let msg = format!(
                                                        "Task submitted: {}",
                                                        envelope.task
                                                    );
                                                    if let Some(ref tx) = task_tx_inbound {
                                                        let _ = tx.send(envelope).await;
                                                    } else {
                                                        // Fallback: dispatch via EventBus if no task_tx
                                                        let ctrl_action =
                                                            presence::PresenceAction::SubmitTask(
                                                                envelope,
                                                            );
                                                        if let Some((ctrl, _)) =
                                                            presence::action_to_control_msg(
                                                                &ctrl_action,
                                                            )
                                                        {
                                                            bus_inbound.send(
                                                                AppEvent::ControlCommand(ctrl),
                                                            );
                                                        }
                                                    }
                                                    presence::ToolQueryResult::text(msg)
                                                } else if let Some((ctrl, msg)) =
                                                    presence::action_to_control_msg(&action)
                                                {
                                                    // Other action tools: dispatch via EventBus
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                    presence::ToolQueryResult::text(msg)
                                                } else {
                                                    match action {
                                                        presence::PresenceAction::TextResult(
                                                            text,
                                                        ) => presence::ToolQueryResult::text(text),
                                                        presence::PresenceAction::NeedsIO {
                                                            tool_name,
                                                            args: io_args,
                                                        } => {
                                                            if let Some(ref ctx) = query_ctx_inbound
                                                            {
                                                                if let Some(result) =
                                                                    presence::handle_tool_query(
                                                                        &ctx.agent_state,
                                                                        &ctx.project_root,
                                                                        &ctx.log_dir,
                                                                        &ctx.knowledge_path,
                                                                        &tool_name,
                                                                        &io_args,
                                                                        frame_registry_inbound
                                                                            .as_ref(),
                                                                        ctx.context_injection
                                                                            .as_ref(),
                                                                    )
                                                                    .await
                                                                {
                                                                    result
                                                                } else {
                                                                    presence::ToolQueryResult::text(
                                                                        format!(
                                                                            "Unknown tool: {}",
                                                                            tool
                                                                        ),
                                                                    )
                                                                }
                                                            } else {
                                                                presence::ToolQueryResult::text("Presence query context not available".to_string())
                                                            }
                                                        }
                                                        _ => unreachable!(),
                                                    }
                                                };

                                            // Log the tool response at Debug level
                                            let result_preview =
                                                preview_text(&query_result.text, 200);
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[tool_response] {} → {}",
                                                    tool, result_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "tool_response",
                                                "id": req_id,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> =
                                                    query_result
                                                        .images
                                                        .iter()
                                                        .map(|img| {
                                                            serde_json::json!({
                                                                "mime_type": img.media_type,
                                                                "data": img.data,
                                                            })
                                                        })
                                                        .collect();
                                                response["images"] =
                                                    serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("async_query") => {
                                            // Async query from browser — same dispatch as tool_request
                                            // but result goes back as async_query_result (injected into
                                            // voice session as text, not as a tool response).
                                            let req_id =
                                                json["id"].as_str().unwrap_or("").to_string();
                                            let tool =
                                                json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned().unwrap_or(
                                                serde_json::Value::Object(Default::default()),
                                            );

                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[async_query] {}", tool),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let query_result = if let Some(ref ctx) =
                                                query_ctx_inbound
                                            {
                                                if let Some(result) = presence::handle_tool_query(
                                                    &ctx.agent_state,
                                                    &ctx.project_root,
                                                    &ctx.log_dir,
                                                    &ctx.knowledge_path,
                                                    &tool,
                                                    &args,
                                                    frame_registry_inbound.as_ref(),
                                                    ctx.context_injection.as_ref(),
                                                )
                                                .await
                                                {
                                                    result
                                                } else {
                                                    presence::ToolQueryResult::text(format!(
                                                        "Unknown query tool: {}",
                                                        tool
                                                    ))
                                                }
                                            } else {
                                                presence::ToolQueryResult::text(
                                                    "Presence query context not available"
                                                        .to_string(),
                                                )
                                            };

                                            let result_preview =
                                                preview_text(&query_result.text, 200);
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[async_query_result] {} → {}",
                                                    tool, result_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "async_query_result",
                                                "id": req_id,
                                                "tool": tool,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> =
                                                    query_result
                                                        .images
                                                        .iter()
                                                        .map(|img| {
                                                            serde_json::json!({
                                                                "mime_type": img.media_type,
                                                                "data": img.data,
                                                            })
                                                        })
                                                        .collect();
                                                response["images"] =
                                                    serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("display_offer") => {
                                            // WebRTC SDP offer from browser for a display session
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let sdp =
                                                json["sdp"].as_str().unwrap_or("").to_string();

                                            // Clone the Arc<DisplaySession> out of the read
                                            // lock before calling handle_offer. Holding the
                                            // guard across the await chokes any writer
                                            // (notably deactivate_user_display's
                                            // registry.write()) for as long as this block
                                            // runs. The Arc keeps the session alive
                                            // independently of the lock.
                                            let session: Option<
                                                Arc<crate::display::DisplaySession>,
                                            > = match session_registry_inbound.as_ref() {
                                                Some(sr) => sr.read().await.get(display_id),
                                                None => None,
                                            };
                                            if let Some(session) = session {
                                                let (ice_tx, mut ice_rx) = mpsc::channel::<(
                                                    crate::display::PeerId,
                                                    String,
                                                )>(
                                                    64
                                                );
                                                // Combine the Host-header IP with the
                                                // port we want to advertise (HTTP port
                                                // for Phase 3 multiplex, or standalone
                                                // Phase 2 port) to form the single TCP
                                                // candidate the peer will emit. None
                                                // if either piece is missing (typically
                                                // because the browser connected via
                                                // hostname).
                                                let tcp_advertised_addr: Option<
                                                    std::net::SocketAddr,
                                                > = match (browser_host_ip, tcp_advertised_port) {
                                                    (Some(ip), Some(port)) => {
                                                        Some(std::net::SocketAddr::new(ip, port))
                                                    }
                                                    _ => None,
                                                };
                                                // Phase 5a.1 input authority gate.  The closure
                                                // returns true when this connection is the
                                                // authority holder OR when the display has no
                                                // holder (unclaimed = pre-phase-5 default).
                                                // `display/mod.rs` only sees this boolean; it
                                                // never learns about DisplayInputHolder, the
                                                // map, or connection IDs.  See
                                                // [`build_local_ws_input_authorizer`] for the
                                                // closure semantics + tests.
                                                let input_authorized =
                                                    build_local_ws_input_authorizer(
                                                        display_id,
                                                        connection_id_inbound.clone(),
                                                        Arc::clone(
                                                            &display_input_authority_inbound,
                                                        ),
                                                    );
                                                // F-1.3b2 transport plumbing: local DisplaySlot's
                                                // browser doesn't create the
                                                // `display_input_authority` data channel
                                                // (5a/5c uses the WS path), so the handler is
                                                // never invoked here. The no-op keeps the
                                                // transport-layer signature uniform across
                                                // both peer kinds; the real federated handler
                                                // is wired by the federated path's caller in
                                                // a later slice.
                                                let authority_handler =
                                                    crate::display::webrtc::noop_authority_handler(
                                                    );
                                                match session
                                                    .handle_offer(
                                                        peer_id,
                                                        &sdp,
                                                        &ice_config,
                                                        Some(Arc::clone(&tcp_peer_registry)),
                                                        tcp_advertised_addr,
                                                        ice_tx,
                                                        input_authorized,
                                                        authority_handler,
                                                    )
                                                    .await
                                                {
                                                    Ok(answer_sdp) => {
                                                        peer_display_ids.push(display_id);
                                                        let answer = serde_json::json!({
                                                            "t": "display_answer",
                                                            "display_id": display_id,
                                                            "sdp": answer_sdp,
                                                        });
                                                        let _ = direct_tx_inbound
                                                            .send(answer.to_string());

                                                        // Forward server ICE candidates to browser
                                                        let ice_direct_tx =
                                                            direct_tx_inbound.clone();
                                                        tokio::spawn(async move {
                                                            while let Some((_pid, candidate_json)) =
                                                                ice_rx.recv().await
                                                            {
                                                                let msg = serde_json::json!({
                                                                    "t": "display_ice",
                                                                    "display_id": display_id,
                                                                    "candidate": serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default(),
                                                                });
                                                                if ice_direct_tx
                                                                    .send(msg.to_string())
                                                                    .is_err()
                                                                {
                                                                    break;
                                                                }
                                                            }
                                                        });
                                                    }
                                                    Err(e) => {
                                                        eprintln!("[web_gateway] WebRTC offer failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            }
                                        }
                                        Some("display_ice") => {
                                            // Trickle ICE candidate from browser. Spawn the
                                            // handling off the ws reader loop because
                                            // `add_ice_candidate` resolves mDNS hostnames
                                            // (browsers obfuscate host candidates as
                                            // `<uuid>.local`). On hosts without an mDNS
                                            // responder — every headless VM without Avahi,
                                            // which is the common deployment — each lookup
                                            // blocks on the system resolver's full timeout
                                            // (5-20s). With multiple candidates and ICE
                                            // retries, that piles 20-30s of blocking inside
                                            // this reader, stalling every other ws frame
                                            // behind it including grant/revoke — the root
                                            // cause of the "second ON takes 20+s" bug.
                                            //
                                            // Spawning decouples candidate processing from
                                            // frame intake. Failed lookups still log the
                                            // same "mdns resolve failed" diagnostic; losing
                                            // a candidate is survivable (ICE has others),
                                            // whereas blocking the reader is not.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let candidate = json["candidate"].to_string();
                                            let sr_clone = session_registry_inbound.clone();
                                            let pid = peer_id;
                                            tokio::spawn(async move {
                                                // Clone the session Arc out of the read
                                                // lock first. The previous spread-across-
                                                // `if let` form held the guard across
                                                // add_ice_candidate's mDNS resolution,
                                                // which on hosts without Avahi blocks for
                                                // 5-20s per candidate — starving any
                                                // concurrent writer (notably
                                                // deactivate_user_display's
                                                // registry.write()). Dropping the guard
                                                // first lets deactivate proceed
                                                // immediately; the session Arc keeps the
                                                // target alive while mDNS resolves.
                                                let session: Option<
                                                    Arc<crate::display::DisplaySession>,
                                                > = match sr_clone.as_ref() {
                                                    Some(sr) => sr.read().await.get(display_id),
                                                    None => None,
                                                };
                                                if let Some(session) = session {
                                                    if let Err(e) = session
                                                        .add_ice_candidate(pid, &candidate)
                                                        .await
                                                    {
                                                        eprintln!("[web_gateway] ICE candidate failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            });
                                        }
                                        Some("terminal_open") => {
                                            // {"t":"terminal_open","host_id":"local","terminal_id":"shell-0","cols":80,"rows":24}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey {
                                                host_id: host_id.clone(),
                                                terminal_id: terminal_id.clone(),
                                            };

                                            match terminal_registry_inbound
                                                .open_or_attach(key.clone(), cols, rows)
                                                .await
                                            {
                                                Ok(session) => {
                                                    // Spawn a forwarder task that drains the session's
                                                    // per-listener channel and sends base64-encoded
                                                    // output to this WS connection.
                                                    let (tx, mut rx) =
                                                        tokio::sync::mpsc::unbounded_channel();
                                                    session.attach(tx);

                                                    let forwarder_tx = direct_tx_inbound.clone();
                                                    let fwd_host = host_id.clone();
                                                    let fwd_term = terminal_id.clone();
                                                    tokio::spawn(async move {
                                                        use base64::Engine as _;
                                                        while let Some(event) = rx.recv().await {
                                                            let msg = match event {
                                                                crate::terminal::TerminalEvent::Output(bytes) => {
                                                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                                                    serde_json::json!({
                                                                        "t": "terminal_output",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "data": b64,
                                                                    })
                                                                }
                                                                crate::terminal::TerminalEvent::Exited { status } => {
                                                                    serde_json::json!({
                                                                        "t": "terminal_exited",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "status": status,
                                                                    })
                                                                }
                                                            };
                                                            if forwarder_tx
                                                                .send(msg.to_string())
                                                                .is_err()
                                                            {
                                                                break;
                                                            }
                                                        }
                                                    });

                                                    let ack = serde_json::json!({
                                                        "t": "terminal_opened",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                    });
                                                    let _ = direct_tx_inbound.send(ack.to_string());
                                                }
                                                Err(e) => {
                                                    let err = serde_json::json!({
                                                        "t": "terminal_error",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                        "error": e,
                                                    });
                                                    let _ = direct_tx_inbound.send(err.to_string());
                                                }
                                            }
                                        }
                                        Some("terminal_input") => {
                                            // {"t":"terminal_input","host_id":"local","terminal_id":"shell-0","data":"<base64>"}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let data_b64 = json["data"].as_str().unwrap_or("");
                                            use base64::Engine as _;
                                            if let Ok(data) =
                                                base64::engine::general_purpose::STANDARD
                                                    .decode(data_b64)
                                            {
                                                let key = crate::terminal::TerminalKey {
                                                    host_id,
                                                    terminal_id,
                                                };
                                                if let Some(session) =
                                                    terminal_registry_inbound.get(&key).await
                                                {
                                                    session.write_input(&data);
                                                }
                                            }
                                        }
                                        Some("terminal_resize") => {
                                            // {"t":"terminal_resize","host_id":"local","terminal_id":"shell-0","cols":N,"rows":N}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey {
                                                host_id,
                                                terminal_id,
                                            };
                                            if let Some(session) =
                                                terminal_registry_inbound.get(&key).await
                                            {
                                                session.resize(cols, rows);
                                            }
                                        }
                                        Some("terminal_close") => {
                                            // {"t":"terminal_close","host_id":"local","terminal_id":"shell-0"}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let key = crate::terminal::TerminalKey {
                                                host_id,
                                                terminal_id,
                                            };
                                            terminal_registry_inbound.close(&key).await;
                                        }
                                        Some("display_input") => {
                                            // Input event (keyboard/mouse) for a display session.
                                            // Drop the registry read lock before the inject
                                            // (which runs xdotool/cliclick subprocesses) so a
                                            // concurrent deactivate can take the write lock
                                            // without waiting on subprocess exits.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;

                                            // Phase 5 authority gate: if someone has claimed
                                            // input authority for this display, only that
                                            // connection's input flows through. Unclaimed
                                            // (no entry in the map) = pre-phase-5 default,
                                            // every connection can input. See the
                                            // `DisplayInputHolder` doc for the full
                                            // contract.
                                            let allowed = {
                                                let authority = display_input_authority_inbound
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                match authority.get(&display_id) {
                                                    Some(entry) => entry
                                                        .matches_local_ws(&connection_id_inbound),
                                                    None => true,
                                                }
                                            };
                                            if !allowed {
                                                // Silent drop — matches the "force_disconnect_voice"
                                                // convention where demoted connections don't get
                                                // per-message denial feedback; the browser already
                                                // knows it's passive from the authority_revoked
                                                // notification it received when it was demoted.
                                                continue;
                                            }

                                            if let Some(evt) = json.get("event") {
                                                if let Ok(input_event) = serde_json::from_value::<
                                                    crate::display::InputEvent,
                                                >(
                                                    evt.clone()
                                                ) {
                                                    let session: Option<
                                                        Arc<crate::display::DisplaySession>,
                                                    > = match session_registry_inbound.as_ref() {
                                                        Some(sr) => sr.read().await.get(display_id),
                                                        None => None,
                                                    };
                                                    if let Some(session) = session {
                                                        if let Err(e) =
                                                            session.inject_input(input_event).await
                                                        {
                                                            eprintln!("[web_gateway] display input injection failed: {}", e);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("set_diagnostics_visual_marker") => {
                                            // **Phase 0 visual-freshness diagnostic toggle**
                                            // (task #83). Inline rather than going through
                                            // the ControlMsg dispatch path because the
                                            // effect is a single atomic store on the
                                            // matching DisplaySession — no shared autonomy
                                            // state, no event-bus side effects, no listener
                                            // chain to wait on. Symmetric with the
                                            // `display_input` arm above for the same reason
                                            // (direct session access, no bus round-trip).
                                            //
                                            // No authority gate: diagnostics is operator-
                                            // initiated and the marker affects every viewer
                                            // of this display when on (it's stamped pre-
                                            // encoder, lands in every encoded layer). An
                                            // operator running a smoke run sets it, all
                                            // viewers see the marker until they unset it.
                                            // No covert-stamp scenario worth gating against.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let enabled =
                                                json["enabled"].as_bool().unwrap_or(false);
                                            match session_registry_inbound.as_ref() {
                                                Some(sr) => {
                                                    let applied = sr
                                                        .write()
                                                        .await
                                                        .set_diagnostics_visual_marker(
                                                            display_id, enabled,
                                                        );
                                                    eprintln!(
                                                        "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                                        display_id,
                                                        enabled,
                                                        if applied { "" } else { " (pending)" },
                                                    );
                                                }
                                                None => {
                                                    eprintln!(
                                                        "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                                        display_id, enabled,
                                                    );
                                                }
                                            }
                                        }
                                        _ => {
                                            // Fall through to ControlMsg parsing.
                                            // WebRtcSignal needs special handling because
                                            // it requires session_registry / direct_tx
                                            // access for the response leg; everything else
                                            // gets re-broadcast as AppEvent::ControlCommand
                                            // for the agent loop / TUI / MCP consumers.
                                            match serde_json::from_value::<ControlMsg>(json) {
                                                Ok(ControlMsg::WebRtcSignal {
                                                    display_id,
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    handle_federated_webrtc_signal(
                                                        display_id,
                                                        session_id,
                                                        signal,
                                                        session_registry_inbound.as_ref(),
                                                        &ice_config,
                                                        Arc::clone(&tcp_peer_registry),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                        // F-1.3b3 federated authority context.
                                                        // `connection_id_inbound` is this WS's
                                                        // id, which doubles as the federation
                                                        // transport's `federation_connection_id`
                                                        // when this connection is acting as a
                                                        // federation transport.
                                                        connection_id_inbound.clone(),
                                                        Arc::clone(&display_input_authority_inbound),
                                                        authority_change_tx_inbound.clone(),
                                                        Arc::clone(&federated_authority_subscribers_inbound),
                                                    ).await;
                                                }
                                                Ok(ControlMsg::RequestDisplayInputAuthority {
                                                    display_id,
                                                }) => {
                                                    // Phase 5a.1: handler body lives in
                                                    // `apply_grant_input_authority` so the
                                                    // authority-change emission is unit-testable
                                                    // without standing up a WS lifecycle.  This
                                                    // arm keeps the bus log + the per-connection
                                                    // confirm message at the call site to avoid
                                                    // baking logging dependencies into the helper.
                                                    apply_grant_input_authority(
                                                        display_id,
                                                        connection_id_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &display_input_authority_inbound,
                                                        &authority_change_tx_inbound,
                                                    );
                                                    // Confirm to the new holder (kept here so the
                                                    // helper has no dependency on the call site's
                                                    // direct_tx — and so the failure-to-send case
                                                    // doesn't bubble past the gate).
                                                    let granted = serde_json::json!({
                                                        "t": "display_input_authority_granted",
                                                        "display_id": display_id,
                                                    })
                                                    .to_string();
                                                    let _ = direct_tx_inbound.send(granted);
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] display_input_authority granted display={} holder={}",
                                                            display_id, connection_id_inbound,
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                }
                                                Ok(ControlMsg::ReleaseDisplayInputAuthority {
                                                    display_id,
                                                }) => {
                                                    let removed = apply_release_input_authority(
                                                        display_id,
                                                        connection_id_inbound.as_str(),
                                                        &display_input_authority_inbound,
                                                        &authority_change_tx_inbound,
                                                    );
                                                    if removed {
                                                        bus_inbound.send(AppEvent::PresenceLog {
                                                            message: format!(
                                                                "[ws] display_input_authority released display={} holder={}",
                                                                display_id, connection_id_inbound,
                                                            ),
                                                            level: Some(LogLevel::Debug),
                                                            turn: None,
                                                        });
                                                    }
                                                }
                                                Ok(ControlMsg::SetDiagnosticsVisualMarker {
                                                    display_id,
                                                    enabled,
                                                }) => {
                                                    // Accept the documented ControlMsg wire form
                                                    // (`{"action":"set_diagnostics_visual_marker", ...}`)
                                                    // in addition to the low-level `t` form
                                                    // handled above. The smoke script uses
                                                    // ControlMsg JSON so the toggle must be
                                                    // applied here instead of falling through to
                                                    // the generic bus path, where this variant is
                                                    // intentionally a no-op for TUI/MCP parity.
                                                    let display_id = display_id.unwrap_or(0);
                                                    match session_registry_inbound.as_ref() {
                                                        Some(sr) => {
                                                            let applied = sr
                                                                .write()
                                                                .await
                                                                .set_diagnostics_visual_marker(
                                                                    display_id, enabled,
                                                                );
                                                            eprintln!(
                                                                "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                                                display_id,
                                                                enabled,
                                                                if applied { "" } else { " (pending)" },
                                                            );
                                                        }
                                                        None => {
                                                            eprintln!(
                                                                "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                                                display_id, enabled,
                                                            );
                                                        }
                                                    }
                                                }
                                                Ok(ctrl @ ControlMsg::ResumeSession { .. }) => {
                                                    if let ControlMsg::ResumeSession {
                                                        source,
                                                        session_id,
                                                        resume_id,
                                                        task,
                                                        ..
                                                    } = &ctrl
                                                    {
                                                        if let Some(replay) =
                                                            resume_session_activity_replay(
                                                                source,
                                                                session_id,
                                                                resume_id.as_deref(),
                                                                task.as_deref(),
                                                                EXTERNAL_ACTIVITY_REPLAY_LIMIT,
                                                            )
                                                        {
                                                            let _ = direct_tx_inbound.send(replay);
                                                        }
                                                    }
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg: {:?}",
                                                            ctrl
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                }
                                                Ok(ctrl) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg: {:?}",
                                                            match &ctrl {
                                                                ControlMsg::StartTask {
                                                                    task,
                                                                    ..
                                                                } => format!(
                                                                    "StartTask({})",
                                                                    preview_text(task, 60)
                                                                ),
                                                                other => format!("{:?}", other),
                                                            }
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                }
                                                Err(e) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg parse failed: {}",
                                                            e
                                                        ),
                                                        level: Some(LogLevel::Warn),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // WebSocket closed — clean up active slot and auto-resume
                        // server presence if this was the active browser (covers tab
                        // close without beforeunload, network drops, etc.)
                        if is_active {
                            let mut slot = active_presence_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if slot
                                .as_ref()
                                .map(|a| a.connection_id == connection_id_inbound)
                                .unwrap_or(false)
                            {
                                *slot = None;
                            }
                        }
                        // Also release any display input authority this
                        // connection held (phase 5).  Without this, a
                        // dangling entry would block other browsers from
                        // claiming the display until someone explicitly
                        // sent RequestDisplayInputAuthority to force-take
                        // it — the `retain` below is the normal-drop
                        // cleanup that keeps the map consistent with
                        // live connections.
                        //
                        // Phase 5a.1: helper handles map mutation + per-
                        // display None-holder change emit so other
                        // browsers don't stay stuck on `other` after the
                        // holder's WS drops.  See
                        // `apply_ws_close_input_authority` for the
                        // semantics + tests.
                        apply_ws_close_input_authority(
                            connection_id_inbound.as_str(),
                            &display_input_authority_inbound,
                            &authority_change_tx_inbound,
                        );
                        // F-1.3b3: federation-transport WS-close
                        // cleanup. Two disjoint registry entries can
                        // belong to one connection_id — `LocalWs` from
                        // direct-browser use or `FederatedWebRtc` from
                        // federation-transport use — so both apply_*
                        // helpers fire from the same WS-close hook.
                        // The single WS in practice acts in only one
                        // role at a time, so the second helper is a
                        // no-op in the typical case; the cost of
                        // running both is the bookkeeping above.
                        //
                        // Order: unregister subscribers first (stops
                        // new fanout sends) → release authority (so
                        // observers see `unclaimed`) → close
                        // WebRtcPeers (so the data channels stop
                        // accepting incoming `display_input_authority_request`
                        // frames under the now-defunct federation
                        // identity). Without the peer-teardown step,
                        // the authority handler closure on each
                        // surviving peer would keep mutating the
                        // registry under an identity whose WS is
                        // gone — the structural bug F-1.3b3 fix #2
                        // closes.
                        let released_federated_subs =
                            unregister_all_federated_subscribers_for_connection(
                                connection_id_inbound.as_str(),
                                &federated_authority_subscribers_inbound,
                            );
                        apply_federated_ws_close_input_authority(
                            connection_id_inbound.as_str(),
                            &display_input_authority_inbound,
                            &authority_change_tx_inbound,
                        );
                        close_federated_peers_for_sessions(
                            &released_federated_subs,
                            session_registry_inbound.as_ref(),
                        )
                        .await;
                        if is_presence_connected && is_active {
                            bus_inbound.send(AppEvent::PresenceDisconnected);
                        }
                        // Remove this peer from display sessions it connected to
                        if !peer_display_ids.is_empty() {
                            if let Some(ref sr) = session_registry_inbound {
                                let reg = sr.read().await;
                                for did in &peer_display_ids {
                                    if let Some(session) = reg.get(*did) {
                                        session.remove_peer(peer_id).await;
                                    }
                                }
                            }
                        }
                        // Unregister from WebTui
                        if let Some(ref tx) = web_tui_tx_inbound {
                            let _ = tx.send(crate::tui::web::WebTuiCommand::RemoveConnection {
                                id: connection_id_inbound.clone(),
                            });
                        }
                    });

                    // Phase 5a.1 outbound personalization plumbing.  The
                    // authority change channel carries the holder's
                    // server-internal connection_id; this connection's
                    // outbound task converts each incoming change into a
                    // personalized `display_input_authority_state` wire
                    // message.  Connection IDs never leave the daemon —
                    // only the resolved `you|other|unclaimed` state does.
                    let mut authority_change_rx = authority_change_tx.subscribe();
                    let connection_id_outbound = connection_id.clone();
                    let display_input_authority_outbound = display_input_authority.clone();
                    let session_registry_outbound = session_registry.clone();

                    // Outbound: broadcast + direct responses → WebSocket
                    let outbound = tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                msg = outbound_rx.recv() => {
                                    match msg {
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
                                msg = direct_rx.recv() => {
                                    match msg {
                                        Some(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                msg = authority_change_rx.recv() => {
                                    match msg {
                                        Ok(change) => {
                                            // Personalize: never ship the holder's identity.
                                            let state = match &change.holder {
                                                Some(h) if h.matches_local_ws(&connection_id_outbound) => "you",
                                                Some(_) => "other",
                                                None => "unclaimed",
                                            };
                                            let frame = serde_json::json!({
                                                "t": "display_input_authority_state",
                                                "display_id": change.display_id,
                                                "state": state,
                                            }).to_string();
                                            if ws_tx
                                                .send(Message::Text(frame.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break,
                                        Err(broadcast::error::RecvError::Lagged(_)) => {
                                            // Phase 5a.1: a lagged subscriber missed at least one
                                            // authority transition.  Send a fresh personalized
                                            // snapshot for every currently-active display so the
                                            // browser's chip cannot be left stuck on stale state.
                                            // Snapshot is computed under the std lock (held briefly,
                                            // released before any send) plus the session registry's
                                            // tokio lock for the active-display list — order
                                            // matters: take the std lock LAST and drop it before
                                            // awaiting the send to avoid awaiting under a sync guard.
                                                            let display_ids: Vec<u32> = match session_registry_outbound.as_ref() {
                                                Some(sr) => sr.read().await.display_ids(),
                                                None => Vec::new(),
                                            };
                                            let snapshots: Vec<(u32, &'static str)> = {
                                                let auth = display_input_authority_outbound
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                display_ids.into_iter().map(|did| {
                                                    let state = match auth.get(&did) {
                                                        Some(entry) if entry.matches_local_ws(&connection_id_outbound) => "you",
                                                        Some(_) => "other",
                                                        None => "unclaimed",
                                                    };
                                                    (did, state)
                                                }).collect()
                                            };
                                            let mut send_failed = false;
                                            for (did, state) in snapshots {
                                                let frame = serde_json::json!({
                                                    "t": "display_input_authority_state",
                                                    "display_id": did,
                                                    "state": state,
                                                }).to_string();
                                                if ws_tx
                                                    .send(Message::Text(frame.into()))
                                                    .await
                                                    .is_err()
                                                {
                                                    send_failed = true;
                                                    break;
                                                }
                                            }
                                            if send_failed { break; }
                                        }
                                    }
                                }
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
                    let request_line = header_text.lines().next().unwrap_or("");

                    // CORS preflight: respond to OPTIONS with permissive headers.
                    // Needed when the page is served from a custom scheme (intendant://)
                    // in the macOS app bundle — API fetches become cross-origin.
                    if request_line.starts_with("OPTIONS") {
                        use tokio::io::AsyncWriteExt;
                        let response = "HTTP/1.1 204 No Content\r\n\
                            Access-Control-Allow-Origin: *\r\n\
                            Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n\
                            Access-Control-Allow-Headers: Content-Type\r\n\
                            Access-Control-Max-Age: 86400\r\n\
                            Connection: close\r\n\
                            \r\n";
                        let _ = stream.write_all(response.as_bytes()).await;
                        return;
                    }

                    // Federation auth enforcement. Applied before any
                    // federation API branch in the dispatch chain
                    // below; non-federation paths (WASM, frames,
                    // dashboard HTML, /config, /.well-known, /ws,
                    // /static/*) sail through unauthenticated. See
                    // `is_federation_path` for the exact set and the
                    // `inbound_bearer_token` docs on `spawn_web_gateway`
                    // for the design rationale.
                    if is_federation_path(request_line) {
                        if let Err((status, body)) =
                            verify_bearer_token(&header_text, inbound_bearer_token.as_deref())
                        {
                            use tokio::io::AsyncWriteExt;
                            let reason = match status {
                                401 => "Unauthorized",
                                _ => "Error",
                            };
                            let response = format!(
                                "HTTP/1.1 {status} {reason}\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: no-cache\r\n\
                                 WWW-Authenticate: Bearer\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {body}",
                                body.len(),
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            return;
                        }
                    }

                    // Route WASM binaries (need async write_all for large payloads)
                    let wasm_binary = if request_line.contains("/wasm-web/presence_web_bg.wasm") {
                        Some(WASM_WEB_BIN)
                    } else {
                        None
                    };

                    if let Some(wasm_data) = wasm_binary {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/wasm\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache, must-revalidate\r\n\
                             Connection: close\r\n\
                             \r\n",
                            wasm_data.len()
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(wasm_data).await;
                    } else if request_line.contains("/icon-128.png")
                        || request_line.contains("/favicon.ico")
                    {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: image/png\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n",
                            ICON_128_PNG.len()
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(ICON_128_PNG).await;
                    } else if request_line.contains(" /frames/") {
                        // Serve HQ frame images from the frame registry.
                        // URL format: /frames/<frame_id> (not /api/session/*/frames/*)
                        use tokio::io::AsyncWriteExt;
                        let frame_id = request_line
                            .split("/frames/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let data = if let Some(ref reg) = frame_registry {
                            let reg = reg.read().await;
                            reg.read_hq(frame_id).ok()
                        } else {
                            None
                        };
                        if let Some(jpeg_data) = data {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: image/jpeg\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: public, max-age=31536000, immutable\r\n\
                                 Connection: close\r\n\
                                 \r\n",
                                jpeg_data.len()
                            );
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&jpeg_data).await;
                        } else {
                            let body = "Frame not found";
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\n\
                                 Content-Type: text/plain\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.contains("/api/project-root") {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "project_root": project_root
                                .as_ref()
                                .map(|root| root.to_string_lossy().to_string())
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains(" /api/fs/stat")
                    {
                        use tokio::io::AsyncWriteExt;
                        let path = query_param(&request_line, "path").unwrap_or_default();
                        let response = match inspect_dashboard_fs_path(&path) {
                            Ok(status) => json_response(
                                "200 OK",
                                serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string()),
                            ),
                            Err(e) => json_error("400 Bad Request", e),
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains(" /api/fs/list")
                    {
                        use tokio::io::AsyncWriteExt;
                        let path = query_param(&request_line, "path").unwrap_or_default();
                        let response = match list_dashboard_fs_dir(&path) {
                            Ok(body) => json_ok(body),
                            Err(e) => json_error("400 Bad Request", e),
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains(" /api/fs/mkdir")
                    {
                        use tokio::io::AsyncWriteExt;
                        let body_text = read_post_body(&header_text, &mut stream).await;
                        let response = match serde_json::from_str::<FsMkdirRequest>(&body_text) {
                            Ok(req) => match mkdir_dashboard_fs_path(&req.path) {
                                Ok(body) => json_ok(body),
                                Err((status, message)) => json_error(&status, message),
                            },
                            Err(e) => json_error("400 Bad Request", format!("invalid JSON: {e}")),
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains("/api/settings")
                    {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        // Read POST body — may be partially or fully outside the peek buffer
                        let content_length: usize = header_text
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                        let body_owned;
                        let body_text = if peeked_body.len() >= content_length {
                            &peeked_body[..content_length]
                        } else {
                            let remaining = content_length.saturating_sub(peeked_body.len());
                            let mut full = peeked_body.to_string();
                            if remaining > 0 {
                                let mut rest = vec![0u8; remaining];
                                if stream.read_exact(&mut rest).await.is_ok() {
                                    full.push_str(&String::from_utf8_lossy(&rest));
                                }
                            }
                            body_owned = full;
                            &body_owned
                        };
                        let result = match &project_root {
                            Some(root) => match serde_json::from_str::<SettingsPayload>(body_text) {
                                Ok(payload) => {
                                    match crate::project::Project::from_root(root.clone()) {
                                        Ok(mut proj) => {
                                            apply_settings_payload(&mut proj.config, &payload);
                                            match proj.save_config() {
                                                Ok(()) => {
                                                    serde_json::json!({"ok": true}).to_string()
                                                }
                                                Err(e) => {
                                                    serde_json::json!({"error": e.to_string()})
                                                        .to_string()
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            serde_json::json!({"error": e.to_string()}).to_string()
                                        }
                                    }
                                }
                                Err(e) => {
                                    serde_json::json!({"error": format!("Invalid settings: {}", e)})
                                        .to_string()
                                }
                            },
                            None => serde_json::json!({"error": "No project root"}).to_string(),
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            result.len(),
                            result
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains("/api/diagnostics/visual-freshness")
                    {
                        // **Phase 0 visual-freshness transcript sink** (task #83).
                        // Body is browser-emitted NDJSON (one JSON record per
                        // `\n`-terminated line); server appends verbatim to
                        // `~/.intendant/diagnostics/visual-freshness/<session>.ndjson`.
                        // No parsing or schema validation here — that's
                        // browser-side or post-hoc analysis on the
                        // transcript. Session id arrives via `?session_id=…`
                        // query param; we sanitize aggressively (alnum + `-`
                        // + `_` only) and reject anything that collapses
                        // empty so a missing param can't accidentally
                        // produce a bare-`.ndjson` write.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let session_id_raw: String = request_line
                            .split('?')
                            .nth(1)
                            .and_then(|qs| qs.split_whitespace().next())
                            .map(|qs| {
                                qs.split('&')
                                    .find_map(|kv| {
                                        let mut parts = kv.splitn(2, '=');
                                        let k = parts.next()?;
                                        let v = parts.next()?;
                                        if k == "session_id" {
                                            Some(v.to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        let content_length: usize = header_text
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                        let body_owned;
                        let body_bytes: &[u8] = if peeked_body.len() >= content_length {
                            &peeked_body.as_bytes()[..content_length]
                        } else {
                            let remaining = content_length.saturating_sub(peeked_body.len());
                            let mut full: Vec<u8> = peeked_body.as_bytes().to_vec();
                            if remaining > 0 {
                                let mut rest = vec![0u8; remaining];
                                if stream.read_exact(&mut rest).await.is_ok() {
                                    full.extend_from_slice(&rest);
                                }
                            }
                            body_owned = full;
                            &body_owned
                        };
                        let (status_line, body) =
                            match crate::diagnostics::append_visual_freshness_record(
                                &session_id_raw,
                                body_bytes,
                            ) {
                                Ok(written) => (
                                    "HTTP/1.1 200 OK",
                                    serde_json::json!({"ok": true, "written": written}).to_string(),
                                ),
                                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => (
                                    "HTTP/1.1 400 Bad Request",
                                    serde_json::json!({"error": e.to_string()}).to_string(),
                                ),
                                Err(e) => (
                                    "HTTP/1.1 500 Internal Server Error",
                                    serde_json::json!({"error": e.to_string()}).to_string(),
                                ),
                            };
                        let response = format!(
                            "{status_line}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/settings") {
                        use tokio::io::AsyncWriteExt;
                        let body = match &project_root {
                            Some(root) => match crate::project::Project::from_root(root.clone()) {
                                Ok(proj) => {
                                    let payload = settings_payload_from_config(&proj.config);
                                    serde_json::to_string(&payload)
                                        .unwrap_or_else(|_| "{}".to_string())
                                }
                                Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                            },
                            None => serde_json::json!({"error": "No project root"}).to_string(),
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains("/api/api-keys")
                    {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let content_length: usize = header_text
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                        let body_owned;
                        let body_text = if peeked_body.len() >= content_length {
                            &peeked_body[..content_length]
                        } else {
                            let remaining = content_length.saturating_sub(peeked_body.len());
                            let mut full = peeked_body.to_string();
                            if remaining > 0 {
                                let mut rest = vec![0u8; remaining];
                                if stream.read_exact(&mut rest).await.is_ok() {
                                    full.push_str(&String::from_utf8_lossy(&rest));
                                }
                            }
                            body_owned = full;
                            &body_owned
                        };
                        let result = handle_set_api_keys(body_text);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            result.len(),
                            result
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/api-key-status") {
                        use tokio::io::AsyncWriteExt;
                        let body = get_api_key_status_json();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains(" /session")
                        && !request_line.contains("/api/session/")
                    {
                        let result = mint_session_token(&session_provider, &session_model).await;
                        let (status, body) = match result {
                            Ok(json) => ("200 OK", json),
                            Err(msg) => (
                                "502 Bad Gateway",
                                serde_json::json!({"error": msg}).to_string(),
                            ),
                        };
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/recordings/")
                        && !request_line.contains("/api/session/")
                    {
                        // Serve recording data: segment files and metadata.
                        use tokio::io::AsyncWriteExt;
                        let path_part = request_line
                            .split("/recordings/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let parts: Vec<&str> = path_part.split('/').collect();

                        if let Some(ref rec_reg) = recording_registry {
                            let reg = rec_reg.read().await;

                            if parts.len() == 2 && parts[1] == "segments" {
                                // GET /recordings/{stream}/segments — check session then daemon dir
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    // Fallback to daemon recordings dir
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let json: Vec<serde_json::Value> = segments
                                    .iter()
                                    .map(|s| {
                                        serde_json::json!({
                                            "filename": s.filename,
                                            "start_secs": s.start_secs,
                                            "end_secs": s.end_secs,
                                        })
                                    })
                                    .collect();
                                let body = serde_json::to_string(&json).unwrap_or("[]".to_string());
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 && parts[1] == "playlist.m3u8" {
                                // GET /recordings/{stream}/playlist.m3u8 — HLS playlist
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let mut m3u8 = String::from(
                                    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:0\n",
                                );
                                let max_dur = segments
                                    .iter()
                                    .map(|s| s.end_secs - s.start_secs)
                                    .fold(0.0f64, f64::max);
                                m3u8.push_str(&format!(
                                    "#EXT-X-TARGETDURATION:{}\n",
                                    max_dur.ceil() as u64
                                ));
                                for s in &segments {
                                    let dur = s.end_secs - s.start_secs;
                                    m3u8.push_str(&format!(
                                        "#EXTINF:{:.3},\n{}\n",
                                        dur, s.filename
                                    ));
                                }
                                m3u8.push_str("#EXT-X-ENDLIST\n");
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/vnd.apple.mpegurl\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    m3u8.len(),
                                    m3u8
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 {
                                // GET /recordings/{stream}/{filename} — serve segment file
                                let stream_name = parts[0];
                                let filename = parts[1];
                                // Validate filename to prevent path traversal
                                let valid = filename.starts_with("seg_")
                                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                                    && filename.len() < 30
                                    && !filename.contains("..");
                                if valid {
                                    // Check session dir first, then daemon dir
                                    let session_path = reg
                                        .session_dir()
                                        .join("recordings")
                                        .join(stream_name)
                                        .join(filename);
                                    let daemon_path = crate::debug::daemon_recordings_dir()
                                        .join(stream_name)
                                        .join(filename);
                                    let seg_path = if session_path.exists() {
                                        session_path
                                    } else {
                                        daemon_path
                                    };
                                    let content_type = if filename.ends_with(".ts") {
                                        "video/mp2t"
                                    } else {
                                        "video/mp4"
                                    };
                                    match tokio::fs::read(&seg_path).await {
                                        Ok(data) => {
                                            let header = format!(
                                                "HTTP/1.1 200 OK\r\n\
                                                 Content-Type: {}\r\n\
                                                 Content-Length: {}\r\n\
                                                 Cache-Control: public, max-age=3600\r\n\
                                                 Connection: close\r\n\
                                                 \r\n",
                                                content_type,
                                                data.len()
                                            );
                                            let _ = stream.write_all(header.as_bytes()).await;
                                            let _ = stream.write_all(&data).await;
                                        }
                                        Err(_) => {
                                            let body = "Segment not found";
                                            let response = format!(
                                                "HTTP/1.1 404 Not Found\r\n\
                                                 Content-Type: text/plain\r\n\
                                                 Content-Length: {}\r\n\
                                                 Connection: close\r\n\
                                                 \r\n\
                                                 {}",
                                                body.len(),
                                                body
                                            );
                                            let _ = stream.write_all(response.as_bytes()).await;
                                        }
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                let body = "Not found";
                                let response = format!(
                                    "HTTP/1.1 404 Not Found\r\n\
                                     Content-Type: text/plain\r\n\
                                     Content-Length: {}\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else {
                            let body = "Recording not available";
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\n\
                                 Content-Type: text/plain\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(),
                                body
                            );
                            use tokio::io::AsyncWriteExt;
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.contains("/recordings")
                        && !request_line.contains("/api/session/")
                    {
                        // GET /recordings — list all streams (session + daemon-scoped)
                        use tokio::io::AsyncWriteExt;

                        let mut all_entries = Vec::new();

                        // Session-scoped recordings (from RecordingRegistry)
                        if let Some(ref rec_reg) = recording_registry {
                            let reg = rec_reg.read().await;
                            let streams = reg.all_streams();
                            for name in &streams {
                                let manifest = reg.manifest(name).unwrap_or(serde_json::json!({}));
                                let segments = reg.segments(name);
                                let total_duration =
                                    segments.last().map(|s| s.end_secs).unwrap_or(0.0);
                                let seg_json: Vec<serde_json::Value> = segments
                                    .iter()
                                    .map(|s| {
                                        serde_json::json!({
                                            "filename": s.filename,
                                            "start_secs": s.start_secs,
                                            "end_secs": s.end_secs,
                                        })
                                    })
                                    .collect();
                                let mut entry = manifest;
                                entry["segments"] = serde_json::Value::Array(seg_json);
                                entry["total_duration_secs"] = serde_json::json!(total_duration);
                                all_entries.push(entry);
                            }
                        }

                        // Daemon-scoped recordings (from ~/.intendant/recordings/)
                        let daemon_dir = crate::debug::daemon_recordings_dir();
                        let mut daemon_streams: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for entry in list_recording_streams(&daemon_dir) {
                            if let Some(name) = entry["stream_name"].as_str() {
                                daemon_streams.insert(name.to_string());
                            }
                            all_entries.push(entry);
                        }

                        let body = serde_json::to_string(&all_entries).unwrap_or("[]".to_string());
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if (request_line.starts_with("DELETE")
                        || request_line.starts_with("POST"))
                        && request_line.contains("/api/session/")
                        && request_line.contains("/delete")
                    {
                        // DELETE /api/session/{id}[/{target}]  (native DELETE)
                        // POST  /api/session/{id}/delete[/{target}]  (WKWebView fallback)
                        use tokio::io::AsyncWriteExt;
                        let rest = request_line
                            .split("/api/session/")
                            .nth(1)
                            .and_then(|r| r.split_whitespace().next())
                            .unwrap_or("");
                        let rest_parts: Vec<&str> = rest
                            .split('/')
                            .filter(|s| !s.is_empty() && *s != "delete")
                            .collect();
                        let session_id = rest_parts.first().copied().unwrap_or("");
                        let target = rest_parts.get(1).copied().unwrap_or("session");
                        let body = delete_session_data(session_id, target);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("DELETE")
                        && request_line.contains("/api/session/")
                    {
                        // Plain DELETE without /delete in path (curl, regular browser)
                        use tokio::io::AsyncWriteExt;
                        let rest = request_line
                            .split("/api/session/")
                            .nth(1)
                            .and_then(|r| r.split_whitespace().next())
                            .unwrap_or("");
                        let rest_parts: Vec<&str> =
                            rest.split('/').filter(|s| !s.is_empty()).collect();
                        let session_id = rest_parts.first().copied().unwrap_or("");
                        let target = rest_parts.get(1).copied().unwrap_or("session");
                        let body = delete_session_data(session_id, target);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains(" /api/session/current/agent-output")
                    {
                        use tokio::io::AsyncWriteExt;
                        let log_dir = if let Some(ref slog) = session_log {
                            match slog.lock() {
                                Ok(l) => Some(l.dir().to_path_buf()),
                                Err(_) => None,
                            }
                        } else {
                            query_ctx.as_ref().map(|ctx| ctx.log_dir.clone())
                        };
                        let response = match log_dir {
                            Some(dir) => current_agent_output_response(&request_line, &dir),
                            None => upload_error_response("404 Not Found", "no active session log"),
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains(" /api/session/current/uploads")
                    {
                        // POST /api/session/current/uploads?name=<fn>&destination=task|workspace
                        //   Content-Type: <mime>
                        //   <raw bytes>
                        //
                        // Streams the body into a tempfile, commits it into
                        // the upload store (per-session `uploads/` or
                        // per-project `workspace_files/`), and broadcasts
                        // UploadReady so all connected browsers see it.
                        //
                        // Route sits in the `/api/session/current/*` family
                        // alongside `changes`, `history`, `rollback`, etc.
                        // That namespace is browser-session managed — not
                        // part of `is_federation_path`, so bearer-token auth
                        // doesn't apply. If a WAN-exposed deploy wants to
                        // protect uploads, gate the whole family at once.
                        use tokio::io::AsyncWriteExt;
                        let response = 'upload: {
                            let Some(ref root) = project_root_for_changes else {
                                break 'upload upload_error_response(
                                    "400 Bad Request",
                                    "no project root",
                                );
                            };

                            let name = query_param(&request_line, "name")
                                .unwrap_or_else(|| "upload.bin".to_string());
                            let requested_destination = query_param(&request_line, "destination")
                                .as_deref()
                                .and_then(crate::upload_store::UploadDestination::from_str)
                                .unwrap_or(crate::upload_store::UploadDestination::Task);
                            let mime = content_type_header(&header_text);
                            if header_text
                                .lines()
                                .any(|l| l.trim().eq_ignore_ascii_case("expect: 100-continue"))
                            {
                                let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
                            }

                            match stream_body_to_tempfile(
                                &header_text,
                                &discard,
                                &mut stream,
                                UPLOAD_MAX_BYTES,
                            )
                            .await
                            {
                                Err(e) => {
                                    let status = if e.contains("too large") {
                                        "413 Payload Too Large"
                                    } else {
                                        "400 Bad Request"
                                    };
                                    break 'upload upload_error_response(status, &e);
                                }
                                Ok((tmp, size)) => {
                                    let (session_dir, session_id) = {
                                        if let Some(ref slog) = session_log {
                                            match slog.lock() {
                                                Ok(l) => (
                                                    l.dir().to_path_buf(),
                                                    l.session_id().to_string(),
                                                ),
                                                Err(_) => {
                                                    break 'upload upload_error_response(
                                                        "500 Internal Server Error",
                                                        "session log lock poisoned",
                                                    );
                                                }
                                            }
                                        } else {
                                            (
                                                pending_upload_session_dir(root),
                                                "pending".to_string(),
                                            )
                                        }
                                    };
                                    let destination = effective_upload_destination(
                                        requested_destination,
                                        session_log.is_some(),
                                    );
                                    match crate::upload_store::commit_upload(
                                        tmp,
                                        &name,
                                        &mime,
                                        size as u64,
                                        destination,
                                        &session_dir,
                                        &session_id,
                                        root,
                                    ) {
                                        Ok(descriptor) => {
                                            bus.send(crate::event::AppEvent::UploadReady {
                                                descriptor: descriptor.clone(),
                                            });
                                            let body = serde_json::to_string(&descriptor)
                                                .unwrap_or_else(|_| "{}".to_string());
                                            format!(
                                                "HTTP/1.1 200 OK\r\n\
                                                 Content-Type: application/json\r\n\
                                                 Content-Length: {}\r\n\
                                                 Cache-Control: no-cache\r\n\
                                                 Access-Control-Allow-Origin: *\r\n\
                                                 Connection: close\r\n\
                                                 \r\n\
                                                 {}",
                                                body.len(),
                                                body
                                            )
                                        }
                                        Err(e) => upload_error_response(
                                            "500 Internal Server Error",
                                            &format!("commit upload: {e}"),
                                        ),
                                    }
                                }
                            }
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains(" /api/session/current/uploads")
                    {
                        // GET /api/session/current/uploads           — list uploads for the current session
                        // GET /api/session/current/uploads/<id>/raw  — stream bytes of one upload
                        use tokio::io::AsyncWriteExt;
                        let response = 'get_upload: {
                            let Some(ref root) = project_root_for_changes else {
                                break 'get_upload upload_error_response(
                                    "404 Not Found",
                                    "no project root",
                                );
                            };
                            let session_dir = if let Some(ref slog) = session_log {
                                match slog.lock() {
                                    Ok(l) => l.dir().to_path_buf(),
                                    Err(_) => {
                                        break 'get_upload upload_error_response(
                                            "500 Internal Server Error",
                                            "session log lock poisoned",
                                        );
                                    }
                                }
                            } else {
                                pending_upload_session_dir(root)
                            };
                            // Path after /api/session/current/uploads
                            let path_and_q = request_line.split_whitespace().nth(1).unwrap_or("");
                            let path = path_and_q.splitn(2, '?').next().unwrap_or("");
                            let suffix = path
                                .trim_start_matches("/api/session/current/uploads")
                                .trim_matches('/');
                            if suffix.is_empty() {
                                let uploads = crate::upload_store::list_uploads(&session_dir, root);
                                let body = serde_json::to_string(&uploads)
                                    .unwrap_or_else(|_| "[]".to_string());
                                format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Access-Control-Allow-Origin: *\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                )
                            } else if let Some(id) = suffix.strip_suffix("/raw") {
                                // GET raw bytes for one upload.
                                match crate::upload_store::find_upload(id, &session_dir, root) {
                                    None => {
                                        upload_error_response("404 Not Found", "upload not found")
                                    }
                                    Some(d) => {
                                        match std::fs::read(&d.path) {
                                            Ok(bytes) => {
                                                let header = format!(
                                                    "HTTP/1.1 200 OK\r\n\
                                                     Content-Type: {}\r\n\
                                                     Content-Length: {}\r\n\
                                                     Content-Disposition: inline; filename=\"{}\"\r\n\
                                                     Cache-Control: no-cache\r\n\
                                                     Access-Control-Allow-Origin: *\r\n\
                                                     Connection: close\r\n\
                                                     \r\n",
                                                    d.mime,
                                                    bytes.len(),
                                                    d.name.replace('"', ""),
                                                );
                                                let _ = stream.write_all(header.as_bytes()).await;
                                                let _ = stream.write_all(&bytes).await;
                                                // Skip the trailing write_all below.
                                                break 'get_upload String::new();
                                            }
                                            Err(e) => upload_error_response(
                                                "500 Internal Server Error",
                                                &format!("read upload: {e}"),
                                            ),
                                        }
                                    }
                                }
                            } else {
                                upload_error_response("404 Not Found", "unknown upload route")
                            }
                        };
                        if !response.is_empty() {
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.starts_with("DELETE")
                        && request_line.contains(" /api/session/current/uploads/")
                    {
                        // DELETE /api/session/current/uploads/<id> — remove the file + sidecar.
                        use tokio::io::AsyncWriteExt;
                        let response = 'del_upload: {
                            let Some(ref root) = project_root_for_changes else {
                                break 'del_upload upload_error_response(
                                    "404 Not Found",
                                    "no project root",
                                );
                            };
                            let session_dir = if let Some(ref slog) = session_log {
                                match slog.lock() {
                                    Ok(l) => l.dir().to_path_buf(),
                                    Err(_) => {
                                        break 'del_upload upload_error_response(
                                            "500 Internal Server Error",
                                            "session log lock poisoned",
                                        );
                                    }
                                }
                            } else {
                                pending_upload_session_dir(root)
                            };
                            let path_and_q = request_line.split_whitespace().nth(1).unwrap_or("");
                            let path = path_and_q.splitn(2, '?').next().unwrap_or("");
                            let id = path
                                .trim_start_matches("/api/session/current/uploads/")
                                .trim_matches('/');
                            if id.is_empty() {
                                break 'del_upload upload_error_response(
                                    "400 Bad Request",
                                    "missing upload id",
                                );
                            }
                            match crate::upload_store::delete_upload(id, &session_dir, root) {
                                Ok(_) => {
                                    bus.send(crate::event::AppEvent::UploadDeleted {
                                        id: id.to_string(),
                                    });
                                    let body = serde_json::json!({"ok": true}).to_string();
                                    format!(
                                        "HTTP/1.1 200 OK\r\n\
                                         Content-Type: application/json\r\n\
                                         Content-Length: {}\r\n\
                                         Cache-Control: no-cache\r\n\
                                         Access-Control-Allow-Origin: *\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    )
                                }
                                Err(e) => upload_error_response(
                                    "500 Internal Server Error",
                                    &format!("delete: {e}"),
                                ),
                            }
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains("/api/session/current/changes")
                    {
                        // File change tracking endpoints:
                        //   GET /api/session/current/changes        — list all changed files
                        //   GET /api/session/current/changes/{path} — unified diff for one file
                        use tokio::io::AsyncWriteExt;
                        let (status, body) = handle_changes_request(
                            &request_line,
                            snapshot_dir.as_deref(),
                            project_root_for_changes.as_deref(),
                        );
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains("/api/session/current/history")
                    {
                        // GET /api/session/current/history — serialized History.
                        use tokio::io::AsyncWriteExt;
                        let (status, body) = handle_history_get(file_watcher.as_ref()).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains("/api/session/current/rollback")
                    {
                        // POST /api/session/current/rollback body:
                        //   {"round_id": N,
                        //    "revert_files": bool (default true),
                        //    "revert_conversation": bool (default false)}
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let body_text = read_post_body(&header_text, &mut stream).await;
                        let agent_state = query_ctx.as_ref().map(|ctx| ctx.agent_state.clone());
                        let (status, body) = handle_history_rollback(
                            &body_text,
                            file_watcher.as_ref(),
                            agent_state.as_ref(),
                            &bus,
                        )
                        .await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains("/api/session/current/redo")
                    {
                        // POST /api/session/current/redo — no body required.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let _ = read_post_body(&header_text, &mut stream).await;
                        let agent_state = query_ctx.as_ref().map(|ctx| ctx.agent_state.clone());
                        let (status, body) =
                            handle_history_redo(file_watcher.as_ref(), agent_state.as_ref()).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains("/api/session/current/prune")
                    {
                        // POST /api/session/current/prune — no body required.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let _ = read_post_body(&header_text, &mut stream).await;
                        let (status, body) = handle_history_prune(file_watcher.as_ref()).await;
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/session/") {
                        use tokio::io::AsyncWriteExt;
                        // Extract the rest after /api/session/ and split into parts
                        let rest = request_line
                            .split("/api/session/")
                            .nth(1)
                            .and_then(|r| r.split_whitespace().next())
                            .unwrap_or("");
                        let rest_parts: Vec<&str> = rest.split('/').collect();

                        if rest_parts.len() >= 2 && rest_parts[1] == "recordings" {
                            // Session recording sub-routes: /api/session/{id}/recordings[/...]
                            let session_id = rest_parts[0];
                            let rec_rest = &rest_parts[2..]; // parts after "recordings"

                            if rec_rest.len() == 2 && rec_rest[1] == "segments" {
                                // GET /api/session/{id}/recordings/{stream}/segments
                                let stream_name = rec_rest[0];
                                let body =
                                    if let Some(session_dir) = resolve_session_dir(session_id) {
                                        let stream_dir =
                                            session_dir.join("recordings").join(stream_name);
                                        let segments = crate::recording::parse_segment_csv_pub(
                                            &stream_dir.join("segments.csv"),
                                            &stream_dir,
                                        );
                                        let seg_json: Vec<serde_json::Value> = segments
                                            .iter()
                                            .map(|s| {
                                                serde_json::json!({
                                                    "filename": s.filename,
                                                    "start_secs": s.start_secs,
                                                    "end_secs": s.end_secs,
                                                })
                                            })
                                            .collect();
                                        serde_json::to_string(&seg_json).unwrap_or("[]".to_string())
                                    } else {
                                        "[]".to_string()
                                    };
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if rec_rest.len() == 2 {
                                // GET /api/session/{id}/recordings/{stream}/{filename}
                                let stream_name = rec_rest[0];
                                let filename = rec_rest[1];
                                let valid = filename.starts_with("seg_")
                                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                                    && filename.len() < 30
                                    && !filename.contains("..");
                                if valid {
                                    let seg_ct = if filename.ends_with(".ts") {
                                        "video/mp2t"
                                    } else {
                                        "video/mp4"
                                    };
                                    let seg_path = resolve_session_dir(session_id).map(|d| {
                                        d.join("recordings").join(stream_name).join(filename)
                                    });
                                    if let Some(path) = seg_path.filter(|p| p.exists()) {
                                        match tokio::fs::read(&path).await {
                                            Ok(data) => {
                                                let header = format!(
                                                    "HTTP/1.1 200 OK\r\n\
                                                     Content-Type: {}\r\n\
                                                     Content-Length: {}\r\n\
                                                     Cache-Control: public, max-age=3600\r\n\
                                                     Connection: close\r\n\
                                                     \r\n",
                                                    seg_ct,
                                                    data.len()
                                                );
                                                let _ = stream.write_all(header.as_bytes()).await;
                                                let _ = stream.write_all(&data).await;
                                            }
                                            Err(_) => {
                                                let body = "Failed to read segment";
                                                let response = format!(
                                                    "HTTP/1.1 500 Internal Server Error\r\n\
                                                     Content-Type: text/plain\r\n\
                                                     Content-Length: {}\r\n\
                                                     Connection: close\r\n\
                                                     \r\n\
                                                     {}",
                                                    body.len(),
                                                    body
                                                );
                                                let _ = stream.write_all(response.as_bytes()).await;
                                            }
                                        }
                                    } else {
                                        let body = "Segment not found";
                                        let response = format!(
                                            "HTTP/1.1 404 Not Found\r\n\
                                             Content-Type: text/plain\r\n\
                                             Content-Length: {}\r\n\
                                             Connection: close\r\n\
                                             \r\n\
                                             {}",
                                            body.len(),
                                            body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                // GET /api/session/{id}/recordings — list streams
                                let body =
                                    if let Some(session_dir) = resolve_session_dir(session_id) {
                                        let recordings_dir = session_dir.join("recordings");
                                        let entries = list_recording_streams(&recordings_dir);
                                        serde_json::to_string(&entries).unwrap_or("[]".to_string())
                                    } else {
                                        "[]".to_string()
                                    };
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else if rest_parts.len() >= 2 && rest_parts[1] == "report" {
                            // GET /api/session/{id}/report — download a zip of
                            // the current session's text artifacts for sharing
                            // with the dev. Pass id="current" to target the
                            // live daemon's own session via WebQueryCtx.
                            use tokio::io::AsyncWriteExt;
                            let session_id = rest_parts[0];
                            let resolved_dir: Option<PathBuf> = if session_id == "current" {
                                query_ctx.as_ref().map(|ctx| ctx.log_dir.clone())
                            } else {
                                resolve_session_dir(session_id)
                            };
                            match resolved_dir {
                                Some(dir) => match build_session_report_zip(&dir) {
                                    Ok(bytes) => {
                                        let fname = dir
                                            .file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "session".to_string());
                                        let header = format!(
                                            "HTTP/1.1 200 OK\r\n\
                                             Content-Type: application/zip\r\n\
                                             Content-Length: {}\r\n\
                                             Content-Disposition: attachment; filename=\"intendant-session-{}.zip\"\r\n\
                                             Cache-Control: no-cache\r\n\
                                             Connection: close\r\n\
                                             \r\n",
                                            bytes.len(),
                                            fname
                                        );
                                        let _ = stream.write_all(header.as_bytes()).await;
                                        let _ = stream.write_all(&bytes).await;
                                    }
                                    Err(e) => {
                                        let body = format!("Failed to build report: {}", e);
                                        let response = format!(
                                            "HTTP/1.1 500 Internal Server Error\r\n\
                                             Content-Type: text/plain\r\n\
                                             Content-Length: {}\r\n\
                                             Connection: close\r\n\
                                             \r\n\
                                             {}",
                                            body.len(),
                                            body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                },
                                None => {
                                    let body = "Session not found";
                                    let response = format!(
                                        "HTTP/1.1 404 Not Found\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            }
                        } else if rest_parts.len() >= 2 && rest_parts[1] == "frames" {
                            // Session frame sub-routes: /api/session/{id}/frames[/{filename}]
                            use tokio::io::AsyncWriteExt;
                            let session_id = rest_parts[0];
                            let frame_rest = &rest_parts[2..];

                            if frame_rest.len() == 1 {
                                // GET /api/session/{id}/frames/{filename}
                                let filename = frame_rest[0];
                                let valid = (filename.ends_with(".jpg")
                                    || filename.ends_with(".png"))
                                    && filename.len() < 80
                                    && !filename.contains("..");
                                if valid {
                                    let ct = if filename.ends_with(".png") {
                                        "image/png"
                                    } else {
                                        "image/jpeg"
                                    };
                                    let frame_path = resolve_session_dir(session_id)
                                        .map(|d| d.join("frames").join(filename));
                                    if let Some(path) = frame_path.filter(|p| p.exists()) {
                                        match tokio::fs::read(&path).await {
                                            Ok(data) => {
                                                let header = format!(
                                                    "HTTP/1.1 200 OK\r\n\
                                                     Content-Type: {}\r\n\
                                                     Content-Length: {}\r\n\
                                                     Cache-Control: public, max-age=3600\r\n\
                                                     Connection: close\r\n\
                                                     \r\n",
                                                    ct,
                                                    data.len()
                                                );
                                                let _ = stream.write_all(header.as_bytes()).await;
                                                let _ = stream.write_all(&data).await;
                                            }
                                            Err(_) => {
                                                let body = "Failed to read frame";
                                                let response = format!(
                                                    "HTTP/1.1 500 Internal Server Error\r\n\
                                                     Content-Type: text/plain\r\n\
                                                     Content-Length: {}\r\n\
                                                     Connection: close\r\n\
                                                     \r\n\
                                                     {}",
                                                    body.len(),
                                                    body
                                                );
                                                let _ = stream.write_all(response.as_bytes()).await;
                                            }
                                        }
                                    } else {
                                        let body = "Frame not found";
                                        let response = format!(
                                            "HTTP/1.1 404 Not Found\r\n\
                                             Content-Type: text/plain\r\n\
                                             Content-Length: {}\r\n\
                                             Connection: close\r\n\
                                             \r\n\
                                             {}",
                                            body.len(),
                                            body
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                // GET /api/session/{id}/frames — list frame filenames
                                let body = if let Some(session_dir) =
                                    resolve_session_dir(session_id)
                                {
                                    let frames_dir = session_dir.join("frames");
                                    let mut names: Vec<String> = Vec::new();
                                    if frames_dir.is_dir() {
                                        if let Ok(entries) = std::fs::read_dir(&frames_dir) {
                                            for e in entries.flatten() {
                                                let n = e.file_name().to_string_lossy().to_string();
                                                if n.ends_with(".jpg") || n.ends_with(".png") {
                                                    names.push(n);
                                                }
                                            }
                                        }
                                        names.sort();
                                    }
                                    serde_json::to_string(&names).unwrap_or("[]".to_string())
                                } else {
                                    "[]".to_string()
                                };
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else {
                            // GET /api/session/{id} — session detail
                            let raw_id = rest_parts[0];
                            let session_id = raw_id.split('?').next().unwrap_or(raw_id);
                            let query = raw_id.split_once('?').map(|(_, q)| q).unwrap_or("");
                            let source = query
                                .split('&')
                                .find_map(|part| {
                                    let (k, v) = part.split_once('=')?;
                                    if k == "source" {
                                        Some(v)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or("intendant");
                            let body = if source == "intendant" {
                                get_session_detail(session_id)
                            } else {
                                external_session_detail(source, session_id).unwrap_or_else(|| {
                                    serde_json::json!({"error": "session not found"}).to_string()
                                })
                            };
                            let response = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: no-cache\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if request_line.contains("/api/displays") {
                        // Display enumeration endpoint
                        use tokio::io::AsyncWriteExt;
                        let displays =
                            crate::display::enumerate_displays_with_sessions(&session_registry)
                                .await;
                        let body =
                            serde_json::to_string(&displays).unwrap_or_else(|_| "[]".to_string());
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains(" /api/peers") {
                        // Peer registry endpoints. Dispatch:
                        //   GET    /api/peers                  → list
                        //   POST   /api/peers                  → add
                        //   DELETE /api/peers                  → remove
                        //   POST   /api/peers/{id}/message     → send message
                        //   POST   /api/peers/{id}/task        → delegate task
                        //   POST   /api/peers/{id}/approval    → resolve approval
                        //
                        // When no registry is wired in (test call sites
                        // that pass None), every request returns 503 so
                        // the dashboard can render "peers unavailable"
                        // instead of the empty list that a working-but-
                        // empty registry would produce.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};

                        // Extract subpath after `/api/peers`. The list/
                        // add/remove ops have an empty subpath; per-peer
                        // ops have `/{id}/{op}`. Extract the *path*
                        // token from the request line first (the second
                        // whitespace-separated word) — splitting on
                        // `/api/peers` directly would walk into the
                        // ` HTTP/1.1` suffix and mistake `HTTP` and `1.1`
                        // for path segments.
                        let path_token = request_line.split_whitespace().nth(1).unwrap_or("");
                        // Split path from query string. `/api/peers/eligible
                        // ?capability=display` needs the query stripped before
                        // we extract subpath segments.
                        let (path, query_str) = match path_token.find('?') {
                            Some(i) => (&path_token[..i], &path_token[i + 1..]),
                            None => (path_token, ""),
                        };
                        let subpath = path
                            .strip_prefix("/api/peers")
                            .unwrap_or("")
                            .trim_start_matches('/');
                        let segments: Vec<&str> =
                            subpath.split('/').filter(|s| !s.is_empty()).collect();

                        let (status, body) = match peer_registry.as_ref() {
                            None => (
                                503,
                                serde_json::json!({
                                    "error": "peer registry not configured"
                                })
                                .to_string(),
                            ),
                            Some(registry)
                                if segments.is_empty() && request_line.starts_with("GET") =>
                            {
                                (200, peers_list_response_body(registry))
                            }
                            Some(registry)
                                if segments.is_empty()
                                    && (request_line.starts_with("POST")
                                        || request_line.starts_with("DELETE")) =>
                            {
                                let body_text = read_request_body(&mut stream, &header_text).await;
                                if request_line.starts_with("POST") {
                                    peers_add(registry, &body_text).await
                                } else {
                                    peers_remove(registry, &body_text).await
                                }
                            }
                            Some(registry)
                                if segments == ["eligible"] && request_line.starts_with("GET") =>
                            {
                                // GET /api/peers/eligible?capability=display
                                // — list peers that satisfy all listed
                                // capabilities. The `eligible` segment is
                                // a reserved sub-path on /api/peers; an
                                // actual peer with that bare id would be
                                // shadowed here, but PeerId values always
                                // carry a `<kind>:` prefix so that's not
                                // a real collision.
                                peers_eligible(registry, query_str)
                            }
                            Some(registry)
                                if segments.len() == 2 && request_line.starts_with("POST") =>
                            {
                                let id = segments[0];
                                let op = segments[1];
                                let body_text = read_request_body(&mut stream, &header_text).await;
                                match op {
                                    "message" => peers_send_message(registry, id, &body_text).await,
                                    "task" => peers_delegate_task(registry, id, &body_text).await,
                                    "approval" => {
                                        peers_resolve_approval(registry, id, &body_text).await
                                    }
                                    "webrtc" => {
                                        peers_webrtc_signal(registry, id, &body_text, &bus).await
                                    }
                                    other => (
                                        404,
                                        serde_json::json!({
                                            "error": format!(
                                                "unknown peer op: {other}"
                                            )
                                        })
                                        .to_string(),
                                    ),
                                }
                            }
                            Some(_) => (
                                405,
                                serde_json::json!({
                                    "error": "method not allowed"
                                })
                                .to_string(),
                            ),
                        };
                        let reason = match status {
                            200 => "OK",
                            400 => "Bad Request",
                            404 => "Not Found",
                            405 => "Method Not Allowed",
                            500 => "Internal Server Error",
                            502 => "Bad Gateway",
                            503 => "Service Unavailable",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n\
                             Access-Control-Allow-Headers: Content-Type\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains(" /api/coordinator/route") {
                        // POST /api/coordinator/route — capability-based
                        // task routing through the Coordinator primitive.
                        // Body shape: {"required_capabilities": ["display",
                        // ...], "task": {"instructions": "...", "context":
                        // ..., "client_correlation_id": "..."}}.
                        // Response: {"peer_id": "...", "task_id": "..."}
                        // on success, structured error otherwise.
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        let (status, body) = match peer_registry.as_ref() {
                            None => (
                                503,
                                serde_json::json!({
                                    "error": "peer registry not configured"
                                })
                                .to_string(),
                            ),
                            Some(_) if !request_line.starts_with("POST") => (
                                405,
                                serde_json::json!({
                                    "error": "method not allowed"
                                })
                                .to_string(),
                            ),
                            Some(registry) => {
                                let body_text = read_request_body(&mut stream, &header_text).await;
                                coordinator_route(registry, &body_text).await
                            }
                        };
                        let reason = match status {
                            200 => "OK",
                            400 => "Bad Request",
                            404 => "Not Found",
                            405 => "Method Not Allowed",
                            500 => "Internal Server Error",
                            502 => "Bad Gateway",
                            503 => "Service Unavailable",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Access-Control-Allow-Methods: POST, OPTIONS\r\n\
                             Access-Control-Allow-Headers: Content-Type\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains(" /api/worktrees/remove")
                    {
                        let body_text = read_request_body(&mut stream, &header_text).await;
                        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
                        let cache = worktree_inventory_cache.clone();
                        let (status, body) = match tokio::task::spawn_blocking(move || {
                            let result = remove_worktree_inventory_response(&home, &body_text);
                            if result.0 == "200 OK" {
                                if let Ok(mut guard) = cache.lock() {
                                    *guard = None;
                                }
                            }
                            result
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(e) => (
                                "500 Internal Server Error",
                                serde_json::json!({
                                    "ok": false,
                                    "error": format!("worktree removal task failed: {e}")
                                })
                                .to_string(),
                            ),
                        };
                        let response = json_response(status, body);
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST")
                        && request_line.contains(" /api/worktrees/scan")
                    {
                        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
                        let project_root = project_root.clone();
                        let cache = worktree_inventory_cache.clone();
                        let body = match tokio::task::spawn_blocking(move || {
                            let body =
                                scan_worktree_inventory_response(&home, project_root.as_deref());
                            if let Ok(mut guard) = cache.lock() {
                                *guard = Some(body.clone());
                            }
                            body
                        })
                        .await
                        {
                            Ok(body) => body,
                            Err(e) => serde_json::json!({
                                "error": format!("worktree scan task failed: {e}")
                            })
                            .to_string(),
                        };
                        let response = json_response("200 OK", body);
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("GET")
                        && request_line.contains(" /api/worktrees")
                    {
                        let body = worktree_inventory_cache
                            .lock()
                            .ok()
                            .and_then(|guard| guard.clone())
                            .unwrap_or_else(empty_worktree_inventory_response);
                        let response = json_response("200 OK", body);
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/sessions/search") {
                        let body = if SESSION_SEARCH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
                            serde_json::json!({
                                "error": "Another deep session search is already running. Wait for it to finish before starting a new one.",
                                "busy": true,
                            })
                            .to_string()
                        } else {
                            let request_line_for_search = request_line.to_string();
                            let body = match tokio::task::spawn_blocking(move || {
                                session_log_search_from_request(&request_line_for_search)
                            })
                            .await
                            {
                                Ok(body) => body,
                                Err(e) => serde_json::json!({
                                    "error": format!("session search task failed: {e}")
                                })
                                .to_string(),
                            };
                            SESSION_SEARCH_IN_FLIGHT.store(false, Ordering::SeqCst);
                            body
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/api/sessions") {
                        // Session listing endpoint. CORS `*` so the
                        // multi-host Stats tab can fetch sibling
                        // daemons' session lists to populate its "All
                        // Sessions" and "Disk Usage" cards per host.
                        let body = list_sessions();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.contains("/debug") {
                        // Debug endpoint: returns agent state + voice connection info
                        let state = query_ctx.as_ref().map(|ctx| {
                            ctx.agent_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        });
                        let vd = voice_debug
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let active_id = active_presence
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                            .map(|a| a.connection_id.clone());
                        let debug_json = serde_json::json!({
                            "agent_state": state,
                            "voice": vd,
                            "active_connection_id": active_id,
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            debug_json.len(),
                            debug_json
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if request_line.starts_with("POST") && request_line.contains(" /mcp") {
                        // MCP Streamable HTTP endpoint.
                        //
                        // rmcp expects:
                        //   - Requests (has `id`):   200 OK + Content-Type: application/json
                        //   - Notifications (no `id`): 202 Accepted + empty body
                        //   - GET for SSE stream:    405 Method Not Allowed (we don't support SSE push)
                        //   - DELETE for session:    405 Method Not Allowed (stateless)
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
                        if let Some(ref mcp) = mcp_server {
                            let content_length: usize = header_text
                                .lines()
                                .find(|l| l.to_lowercase().starts_with("content-length:"))
                                .and_then(|l| l.split(':').nth(1))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                            let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
                            let body_owned;
                            let body_text = if peeked_body.len() >= content_length {
                                &peeked_body[..content_length]
                            } else {
                                let remaining = content_length.saturating_sub(peeked_body.len());
                                let mut full = peeked_body.to_string();
                                if remaining > 0 {
                                    let mut rest = vec![0u8; remaining];
                                    if stream.read_exact(&mut rest).await.is_ok() {
                                        full.push_str(&String::from_utf8_lossy(&rest));
                                    }
                                }
                                body_owned = full;
                                &body_owned
                            };
                            let outcome = handle_mcp_http_request(body_text, mcp).await;
                            let http_response = match outcome {
                                McpHttpOutcome::Response(resp) => {
                                    let json = serde_json::to_string(&resp).unwrap_or_default();
                                    format!(
                                        "HTTP/1.1 200 OK\r\n\
                                         Content-Type: application/json\r\n\
                                         Access-Control-Allow-Origin: *\r\n\
                                         Content-Length: {}\r\n\
                                         \r\n\
                                         {}",
                                        json.len(),
                                        json,
                                    )
                                }
                                McpHttpOutcome::Accepted => "HTTP/1.1 202 Accepted\r\n\
                                     Access-Control-Allow-Origin: *\r\n\
                                     Content-Length: 0\r\n\
                                     \r\n"
                                    .to_string(),
                            };
                            let _ = stream.write_all(http_response.as_bytes()).await;
                        } else {
                            let err = r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"MCP server not available"}}"#;
                            let http = format!(
                                "HTTP/1.1 503 Service Unavailable\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 \r\n\
                                 {}",
                                err.len(),
                                err
                            );
                            let _ = stream.write_all(http.as_bytes()).await;
                        }
                    } else if (request_line.starts_with("GET")
                        || request_line.starts_with("DELETE"))
                        && request_line.contains(" /mcp")
                    {
                        // MCP Streamable HTTP: GET (SSE stream) and DELETE (session cleanup)
                        // are not supported by our stateless endpoint.  Return 405 so rmcp
                        // gracefully falls back (skips SSE / ignores session delete).
                        use tokio::io::AsyncWriteExt;
                        let http = "HTTP/1.1 405 Method Not Allowed\r\n\
                                    Access-Control-Allow-Origin: *\r\n\
                                    Content-Length: 0\r\n\
                                    \r\n";
                        let _ = stream.write_all(http.as_bytes()).await;
                    } else {
                        let (content_type, body, cache) =
                            if request_line.contains("/wasm-web/presence_web.js") {
                                (
                                    "application/javascript",
                                    WASM_WEB_JS.to_string(),
                                    "no-cache, must-revalidate",
                                )
                            } else if request_line.contains("/audio-processor.js") {
                                (
                                    "application/javascript",
                                    AUDIO_PROCESSOR_JS.to_string(),
                                    "no-cache",
                                )
                            } else if request_line.contains("/.well-known/agent-card.json") {
                                // Canonical peer identity + capability surface.
                                // Served alongside /config so the browser and
                                // federated peers can discover who this daemon
                                // is without parsing the voice-runtime config.
                                ("application/json", agent_card_json.clone(), "no-cache")
                            } else if request_line.contains("/config") {
                                ("application/json", config_json.clone(), "no-cache")
                            } else {
                                // Default: serve app.html (also matches /app for backward compat)
                                ("text/html; charset=utf-8", app_html.to_string(), "no-cache")
                            };

                        // CORS: allow the multi-host dashboard to
                        // `fetch()` /config and /.well-known/agent-card.json
                        // on this daemon from a page served by a sibling
                        // daemon (cross-origin). `*` works because our
                        // fetches don't send credentials.
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: {}\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            content_type,
                            body.len(),
                            cache,
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                }
            });
        }
    })
}

/// Build a `WebGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
///
/// Returns voice/runtime fields only. Daemon identity (host label,
/// version, git sha) lives on the Agent Card at
/// `/.well-known/agent-card.json` and is assembled at gateway spawn
/// time via [`build_local_agent_card`].
pub fn build_config(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_config: crate::display::IceConfig,
) -> WebGatewayConfig {
    build_config_inner(
        live_provider,
        live_model,
        transcription_enabled,
        ice_config.ice_servers,
    )
}

// ---------------------------------------------------------------------------
// /api/peers helpers
// ---------------------------------------------------------------------------

/// Wrapper for the `GET /api/peers` JSON body.
///
/// Each entry is a [`crate::peer::PeerSnapshot`] — the same type the
/// registry's push events carry. One snapshot type means the dashboard
/// applies API entries and pushed deltas the same way; no parallel
/// schemas to drift apart.
#[derive(Serialize)]
struct PeerListResponse {
    peers: Vec<crate::peer::PeerSnapshot>,
}

#[derive(Deserialize)]
struct AddPeerRequest {
    card_url: String,
    /// Optional connecting-side override for the peer's transport
    /// URLs. When non-empty, the card's `transports` field is
    /// replaced with one `IntendantWs` entry per URL. Lets the
    /// operator route around topologies the advertising peer's card
    /// doesn't know about (port-forwards, proxies, named tunnels).
    /// `#[serde(default)]` so older clients without this field
    /// continue to work.
    #[serde(default)]
    via_urls: Vec<String>,
    /// Optional outbound bearer token sent to this peer (the
    /// `[[peer]] bearer_token` equivalent for dashboard-added
    /// peers). When set, sent on the agent-card fetch and the
    /// WebSocket upgrade. Required when the peer's card declares
    /// `auth.application = Some(Bearer)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer_token: Option<String>,
    /// Optional operator-supplied pinned cert fingerprints. When
    /// non-empty, REPLACES whatever the peer's card declares for
    /// `auth.transport` — eliminates the TOFU window when the
    /// operator got the fingerprint out-of-band. Same wire format
    /// as the card's: lowercase hex with optional `:` separators.
    #[serde(default)]
    pinned_fingerprints: Vec<String>,
    /// Explicit URL the **browser** uses to reach this peer's HTTP
    /// port for WebRTC ICE-TCP. When set, the dashboard uses this
    /// (not `d.ws_url`) as the `advertise_tcp_via_url` hint in the
    /// federated WebRTC offer. Decouples the browser-side URL from
    /// the via URL the primary uses for federation, which matters
    /// when the two network positions differ (primary-side localhost
    /// tunnel, browser on a different machine, etc.). `None` falls
    /// back to the slice 3a.2 behavior of using the primary's via URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser_tcp_via_url: Option<String>,
}

#[derive(Deserialize)]
struct RemovePeerRequest {
    peer_id: String,
}

/// Build the JSON body for `GET /api/peers`. Cheap — takes a
/// snapshot of the registry's handles and reads their current
/// watch-backed connection/status values. Handles are cloneable so
/// no lock is held across the serialization.
///
/// Each snapshot is built via [`crate::peer::PeerHandle::snapshot`], the
/// same constructor used by the registry's push event stream. The
/// dashboard applies an API entry and a pushed snapshot identically.
fn peers_list_response_body(registry: &crate::peer::PeerRegistry) -> String {
    let handles = registry.list();
    let peers: Vec<crate::peer::PeerSnapshot> = handles.iter().map(|h| h.snapshot()).collect();
    serde_json::to_string(&PeerListResponse { peers })
        .unwrap_or_else(|_| "{\"peers\":[]}".to_string())
}

/// Handle a `POST /api/peers` body: parse, call
/// `PeerRegistry::add_peer`, return `(status_code, body_json)`.
async fn peers_add(registry: &crate::peer::PeerRegistry, body_text: &str) -> (u16, String) {
    let req: AddPeerRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    match registry
        .add_peer_with_credentials(
            &req.card_url,
            req.via_urls,
            req.bearer_token,
            req.pinned_fingerprints,
            req.browser_tcp_via_url,
        )
        .await
    {
        Ok(peer_id) => (
            200,
            serde_json::json!({"peer_id": peer_id.as_str()}).to_string(),
        ),
        Err(e) => (502, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// Handle a `DELETE /api/peers` body: parse, call
/// `PeerRegistry::remove_peer`, return `(status_code, body_json)`.
async fn peers_remove(registry: &crate::peer::PeerRegistry, body_text: &str) -> (u16, String) {
    let req: RemovePeerRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let id = crate::peer::PeerId(req.peer_id);
    match registry.remove_peer(&id).await {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotFound(_)) => (
            404,
            serde_json::json!({"error": "peer not found"}).to_string(),
        ),
        Err(e) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// Read the body of an HTTP request from `stream`, given the already-
/// peeked `header_text` (which may include a partial body in its
/// trailing portion after the `\r\n\r\n` delimiter). Returns the body
/// as an owned `String`.
///
/// Reads exactly `Content-Length` bytes total — the prefix already
/// in `header_text` plus any remainder still in the socket. Returns
/// an empty string when no `Content-Length` header is present.
///
/// Factored out of the original inline body-reading block in the
/// `/api/peers` handler so the per-peer outbound op handlers below
/// can share it without duplicating the peek-then-stream pattern.
async fn read_request_body(stream: &mut tokio::net::TcpStream, header_text: &str) -> String {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if content_length == 0 {
        return String::new();
    }
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return peeked_body[..content_length].to_string();
    }
    let remaining = content_length.saturating_sub(peeked_body.len());
    let mut full = peeked_body.to_string();
    let mut rest = vec![0u8; remaining];
    if stream.read_exact(&mut rest).await.is_ok() {
        full.push_str(&String::from_utf8_lossy(&rest));
    }
    full
}

// ---------------------------------------------------------------------------
// Per-peer outbound op handlers
// ---------------------------------------------------------------------------
//
// These three endpoints let the dashboard drive the read-write peer
// transport directly. Each maps a JSON body to the matching
// [`crate::peer::PeerHandle`] method:
//
//   POST /api/peers/{id}/message  →  PeerHandle::send_message
//   POST /api/peers/{id}/task     →  PeerHandle::delegate_task
//   POST /api/peers/{id}/approval →  PeerHandle::resolve_approval
//
// Error model (uniform across the three):
//
//   400  bad JSON / missing required field
//   404  peer not in registry
//   405  peer's transport doesn't support this op (UnsupportedCapability)
//   502  transport-level failure (NotConnected, Transport, Auth, …)
//   500  catch-all for unexpected errors
//
// Status codes pick a meaningful HTTP semantic per [`PeerError`] variant
// rather than collapsing everything to 502 — the dashboard renders a
// different message for "wrong peer kind" vs "peer is offline".

/// Shared body for `POST /api/peers/{id}/message`.
///
/// Two equivalent shapes accepted:
///
/// 1. Shorthand: `{"text": "hello"}` — implicit user role + Text content.
/// 2. Full:     `{"role": "user", "content": {"type": "text", "text": "hello"}, "session": null}`.
///
/// The `content` field, when present, wins over `text`. Either `text`
/// or `content` is required; everything else is optional.
#[derive(Deserialize)]
struct SendMessageRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    role: Option<crate::peer::MessageRole>,
    #[serde(default)]
    content: Option<crate::peer::MessageContent>,
    #[serde(default)]
    session: Option<String>,
}

impl SendMessageRequest {
    fn into_message(self) -> Result<crate::peer::PeerMessage, String> {
        let role = self.role.unwrap_or(crate::peer::MessageRole::User);
        let content = match (self.content, self.text) {
            (Some(c), _) => c,
            (None, Some(t)) => crate::peer::MessageContent::Text { text: t },
            (None, None) => {
                return Err("either 'text' or 'content' is required".to_string());
            }
        };
        Ok(crate::peer::PeerMessage {
            session: self.session,
            role,
            content,
        })
    }
}

#[derive(Deserialize)]
struct DelegateTaskRequest {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

#[derive(Deserialize)]
struct ResolveApprovalRequest {
    request_id: String,
    decision: crate::peer::ApprovalDecision,
}

/// Convert a [`crate::peer::PeerError`] into the matching HTTP status +
/// JSON error body. Used by all three per-peer op handlers.
fn peer_error_response(err: crate::peer::PeerError) -> (u16, String) {
    use crate::peer::PeerError;
    let status = match &err {
        PeerError::NotFound(_) => 404,
        PeerError::UnsupportedCapability(_) => 405,
        PeerError::NotConnected
        | PeerError::Transport(_)
        | PeerError::Auth(_)
        | PeerError::CardFetch(_)
        | PeerError::Rejected { .. } => 502,
        _ => 500,
    };
    (
        status,
        serde_json::json!({"error": err.to_string()}).to_string(),
    )
}

/// Look up a peer by id; return 404 + body when absent.
fn peer_handle_or_404(
    registry: &crate::peer::PeerRegistry,
    id: &str,
) -> Result<crate::peer::PeerHandle, (u16, String)> {
    let peer_id = crate::peer::PeerId(id.to_string());
    registry.get(&peer_id).ok_or_else(|| {
        (
            404,
            serde_json::json!({"error": format!("peer not found: {id}")}).to_string(),
        )
    })
}

/// JSON body shape for `POST /api/peers/{id}/webrtc`.
///
/// Single endpoint, signal-discriminated. The dashboard's per-peer
/// `RTCPeerConnection` glue posts every leg of the signaling exchange
/// (Offer, IceCandidate, Close) through this one path, scoped by
/// `display_id` + `session_id`. The peer responds asynchronously
/// via `OutboundEvent::WebRtcSignal` events that the registry
/// forwards to the browser through the existing
/// `OutboundEvent::PeerEventForwarded` channel.
#[derive(Deserialize)]
struct PeerWebRtcSignalRequest {
    display_id: u32,
    session_id: String,
    signal: crate::peer::WebRtcSignal,
}

/// Handle `POST /api/peers/{id}/webrtc`. Routes a WebRTC signaling
/// frame from the browser to the named peer over the federation
/// transport. Returns `200 {"ok": true}` on accepted dispatch, or
/// the standard 4xx/5xx envelope used by the other peer ops.
///
/// The peer's response (Answer, ICE candidates) flows back
/// asynchronously via the registry's per-peer event forwarder —
/// callers don't get the answer in this HTTP response, they
/// observe it on the dashboard's primary `/ws` as a
/// `PeerEventForwarded` whose payload is `PeerEvent::WebRtcSignal`.
async fn peers_webrtc_signal(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
    bus: &EventBus,
) -> (u16, String) {
    // Same source tag as the peer-side handler (see
    // `handle_federated_webrtc_signal`), so filtering the session
    // log on `source == "webrtc-peer"` catches the full signaling
    // conversation across both primary (outbound forward) and peer
    // (inbound handle) — the wire is the same signal, the logs say
    // so.
    const LOG_SOURCE: &str = "webrtc-peer";
    let req: PeerWebRtcSignalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("rejecting webrtc signal from browser — invalid body: {e}"),
                turn: None,
            });
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let signal_kind = match &req.signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "forwarding {signal_kind} from browser to peer={id} display={} session={}",
            req.display_id, req.session_id
        ),
        turn: None,
    });
    let peer_id = crate::peer::PeerId(id.to_string());
    let handle = match registry.get(&peer_id) {
        Some(h) => h,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("peer {id} not in registry — dropping {signal_kind}"),
                turn: None,
            });
            return (
                404,
                serde_json::json!({"error": "peer not found"}).to_string(),
            );
        }
    };
    let display_id = req.display_id;
    let session_id_str = req.session_id.clone();
    match handle
        .webrtc_signal(
            req.display_id,
            crate::peer::WebRtcSessionId(req.session_id),
            req.signal,
        )
        .await
    {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotConnected) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} not connected — dropping {signal_kind} (display={display_id} session={session_id_str})"
                ),
                turn: None,
            });
            (
                502,
                serde_json::json!({"error": "peer is not connected"}).to_string(),
            )
        }
        Err(crate::peer::PeerError::UnsupportedCapability(_)) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} transport lacks webrtc_signal — dropping {signal_kind}"
                ),
                turn: None,
            });
            (
                502,
                serde_json::json!({
                    "error": "peer's transport does not support WebRTC signaling"
                })
                .to_string(),
            )
        }
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "error".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("webrtc_signal to peer {id} failed: {e}"),
                turn: None,
            });
            (500, serde_json::json!({"error": e.to_string()}).to_string())
        }
    }
}

/// Slice 3b: rewrite an outgoing federated `WebRtcSignal::Answer` to
/// (a) register the peer's ICE ufrag in the relay registry and
/// (b) inject a TCP candidate pointing at the primary's own address
/// alongside the peer's direct candidate.
///
/// After the rewrite, a browser receiving this Answer has two TCP
/// candidates: the peer's direct TCP candidate (if the peer provided
/// one via `advertise_tcp_via_url`) and the primary's relay
/// candidate. Browser ICE tries both and uses whichever forms first.
/// Because the relay candidate is emitted with a lower priority
/// (see `inject_relay_tcp_candidate`), direct wins on reachable
/// topologies and relay is the fallback.
///
/// Non-Answer events pass through verbatim. Events with malformed
/// SDPs, missing ufrags, or a peer URL that can't be resolved fall
/// through without rewriting — the browser still sees the original
/// Answer, just without the relay candidate.
async fn maybe_rewrite_federated_answer(
    peer: &crate::peer::PeerId,
    event: crate::peer::PeerEvent,
    registry: &crate::peer::PeerRegistry,
    relay_registry: &Arc<crate::display::webrtc::TcpRelayRegistry>,
    relay_advertise_url: Option<&str>,
    bus: &EventBus,
) -> crate::peer::PeerEvent {
    const LOG_SOURCE: &str = "webrtc-peer";

    // Match only the specific variant that carries an Answer SDP; all
    // other event variants (Log, Usage, ActivityStarted, IceCandidate,
    // etc.) pass through unchanged.
    let (display_id, session_id, sdp) = match &event {
        crate::peer::PeerEvent::WebRtcSignal {
            display_id,
            session_id,
            signal: crate::peer::WebRtcSignal::Answer { sdp },
        } => (*display_id, session_id.clone(), sdp.clone()),
        _ => return event,
    };

    // Extract the peer's ICE ufrag from the Answer SDP. Without it we
    // can't key the relay registry, so we skip rewriting and let the
    // browser try whatever direct candidate the peer advertised.
    let ufrag = match crate::display::webrtc::parse_sdp_ice_ufrag(&sdp) {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     Answer SDP missing a=ice-ufrag attribute"
                ),
                turn: None,
            });
            return event;
        }
    };

    // Resolve the peer's outbound TCP address — where the primary
    // will dial when it sees a relay-destined TCP connection. Prefer
    // `browser_tcp_via_url` (operator's split-browser-side URL) then
    // fall back to `ws_url` (primary-side via URL). In the typical
    // co-located case the two are the same; in split topologies the
    // operator uses browser_tcp_via_url to point at where they'd
    // like the BROWSER to reach the peer. Here we're dialing FROM
    // the primary, but the primary typically shares the LAN position
    // of the operator's browser-reachable URL when one is set.
    let outbound_url = registry.get(peer).and_then(|h| {
        let snap = h.snapshot();
        snap.browser_tcp_via_url.or(snap.ws_url)
    });
    let outbound_url = match outbound_url {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     no outbound URL on the peer's snapshot (peer removed mid-Answer?)"
                ),
                turn: None,
            });
            return event;
        }
    };
    let outbound_addr = match resolve_url_to_socket_addr(&outbound_url).await {
        Some(addr) => addr,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     outbound URL {outbound_url:?} didn't resolve to a SocketAddr"
                ),
                turn: None,
            });
            return event;
        }
    };
    relay_registry.register(ufrag.clone(), outbound_addr);

    // Resolve the primary's own relay URL into a SocketAddr we can
    // put in an SDP candidate line. When the primary has no
    // advertised URL we can work with (local_addr() was None at
    // spawn, headless mode, etc), skip injection and just forward
    // the Answer unchanged — the browser still has the peer's
    // direct candidate to try.
    let primary_relay_addr = match relay_advertise_url {
        Some(url) => match resolve_url_to_socket_addr(url).await {
            Some(addr) => addr,
            None => {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                         but can't inject relay candidate — primary's own URL {url:?} doesn't resolve"
                    ),
                    turn: None,
                });
                return event;
            }
        },
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                     but no primary relay URL configured — skipping candidate injection"
                ),
                turn: None,
            });
            return event;
        }
    };

    let rewritten_sdp =
        crate::display::webrtc::inject_relay_tcp_candidate(&sdp, primary_relay_addr);
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "info".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "relay registered ufrag={ufrag} peer={peer} session={session_id} \
             primary_relay={primary_relay_addr} outbound={outbound_addr}"
        ),
        turn: None,
    });

    crate::peer::PeerEvent::WebRtcSignal {
        display_id,
        session_id,
        signal: crate::peer::WebRtcSignal::Answer { sdp: rewritten_sdp },
    }
}

/// Parse a WebSocket / HTTP URL and resolve it to a [`SocketAddr`].
///
/// Used to convert the browser's view of a peer's HTTP port (the
/// `advertise_tcp_via_url` hint in a federated
/// [`crate::peer::WebRtcSignal::Offer`]) into the concrete address
/// the peer advertises in its ICE-TCP candidate.
///
/// Accepts `ws://` / `wss://` / `http://` / `https://` schemes (all
/// produce the same authority shape). The host can be an IPv4
/// literal, a bracketed IPv6 literal, or a hostname — hostnames are
/// resolved via [`tokio::net::lookup_host`] and the first returned
/// address is used. The port must be explicit; there's no default-
/// port fallback, because we can't know what the peer's HTTP
/// listener bound to without being told.
///
/// Returns `None` on any parse or resolution failure. Callers treat
/// that as "no TCP candidate, UDP-only path" — the same behavior as
/// slice 3a's pre-3a.2 baseline.
async fn resolve_url_to_socket_addr(url: &str) -> Option<std::net::SocketAddr> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))?;
    // Strip any path / query that follows the authority. Authority
    // for an IPv6 literal is `[::1]:8766`, which contains neither
    // `/` nor `?` inside the brackets, so split-on-first is safe.
    let authority = rest.split(|c| c == '/' || c == '?').next()?;
    // Fast path for `ipv4:port` or `[ipv6]:port`: parse directly.
    if let Ok(addr) = authority.parse::<std::net::SocketAddr>() {
        return Some(addr);
    }
    // Hostname:port — needs DNS. `lookup_host` accepts `host:port`
    // and returns the resolved SocketAddrs in OS-chosen order; first
    // is the winner (matches what the kernel would pick for a
    // regular connect()).
    tokio::net::lookup_host(authority).await.ok()?.next()
}

/// Handle a federation-driven WebRTC signal arriving on this peer's
/// WebSocket inside a [`crate::event::ControlMsg::WebRtcSignal`].
///
/// Routes the signal to the matching `DisplaySession` method and
/// emits responses back over the connection's `direct_tx` as
/// [`crate::types::OutboundEvent::WebRtcSignal`] frames:
///
/// - `Offer` → `DisplaySession::handle_offer` → emit `Answer` + drain
///   the per-session ICE channel emitting `IceCandidate`s as they arrive.
/// - `IceCandidate` → `DisplaySession::add_ice_candidate`. No response.
/// - `Close` → `DisplaySession::remove_peer`. No response.
/// - `Answer` → protocol error (this side is the offer-receiver, not
///   the offer-sender). Logged and ignored.
/// - `Unknown` → forward-compat fallback. Ignored.
///
/// Slice 3a.2 threads the browser's view of the peer's HTTP port
/// through as the ICE-TCP candidate the peer advertises, multiplexed
/// onto its own `TcpPeerRegistry` (same mechanism as the local
/// browser↔daemon display path). When the Offer carries an
/// `advertise_tcp_via_url`, the peer advertises both its UDP host
/// candidates and a TCP candidate at the resolved address — which
/// enables federation WebRTC through any tunnel / port-forward /
/// Tailscale path the operator has already made reachable from the
/// browser. Without the hint (or when the URL can't be resolved),
/// the peer falls back to UDP host candidates only — the 3a baseline.
/// Slice 3b layers primary-as-media-relay on top for the browser-
/// cannot-reach-peer-at-all case.
///
/// `session_id` is round-tripped verbatim into the response so the
/// browser's per-(peer, session_id) `RTCPeerConnection` map can match
/// the answer/candidates back to the right pending session. The local
/// [`crate::display::PeerId`] used as the `WebRtcPeer` key is derived
/// by hashing `session_id` — same string hashes to the same u64, so
/// later `IceCandidate` / `Close` signals route to the same peer.
async fn handle_federated_webrtc_signal(
    display_id: u32,
    session_id: String,
    signal: crate::peer::WebRtcSignal,
    session_registry: Option<&Arc<tokio::sync::RwLock<crate::display::SessionRegistry>>>,
    ice_config: &crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    direct_tx: tokio::sync::mpsc::UnboundedSender<String>,
    bus: &EventBus,
    // F-1.3b3 federated authority context. The caller's
    // `connection_id` is the federation transport's WS id, which the
    // peer-side authority registry uses as `federation_connection_id`
    // (see [`DisplayInputHolder::FederatedWebRtc`]). The remaining
    // refs route to the same shared registry + broadcast the local 5c
    // path uses, so cross-provenance arbitration (local takes from
    // federated and vice versa) goes through one source of truth.
    federation_connection_id: String,
    display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    federated_authority_subscribers: FederatedAuthoritySubscribers,
) {
    // Short tag used as the `source` on every log line this handler
    // emits, so the operator can filter the session log to just the
    // federated-WebRTC conversation: `grep 'source":"webrtc-peer"'`.
    // Distinct from the local-display `display_offer` flow (which
    // emits via different codepaths) so logs are unambiguous even
    // when both are active.
    const LOG_SOURCE: &str = "webrtc-peer";

    // Structured signal-kind tag for log messages. The inner
    // `WebRtcSignal` variant name would also work but `Offer`/`Answer`
    // etc. are clearer than the enum's Debug rendering with fields.
    let signal_kind = match &signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "received {signal_kind} from connector (display={display_id} session={session_id})"
        ),
        turn: None,
    });

    let registry = match session_registry {
        Some(r) => r,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dropping {signal_kind}: no session_registry (display={display_id} session={session_id})"
                ),
                turn: None,
            });
            return;
        }
    };
    let session = match registry.read().await.get(display_id) {
        Some(s) => s,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dropping {signal_kind}: unknown display {display_id} (session {session_id})"
                ),
                turn: None,
            });
            return;
        }
    };

    // Stable PeerId per session_id. Same string hashes to the same
    // u64, so subsequent IceCandidate / Close signals — and the
    // federation WS-close cleanup path — all route to the same
    // WebRtcPeer in the session's peer map. Centralized via
    // `peer_id_for_federated_session` so the cleanup path can't drift
    // from this derivation.
    let peer_id: crate::display::PeerId = peer_id_for_federated_session(&session_id);

    match signal {
        crate::peer::WebRtcSignal::Offer {
            sdp,
            advertise_tcp_via_url,
        } => {
            // Resolve the browser-supplied URL hint to a SocketAddr.
            // Unreachable hostnames / malformed URLs / missing hint
            // all collapse to `None` → UDP-only host candidates, same
            // behavior as pre-3a.2. Wrapped in a single lookup so we
            // don't block handle_offer on DNS per-session.
            let tcp_advertised_addr = match advertise_tcp_via_url.as_deref() {
                Some(url) if !url.is_empty() => resolve_url_to_socket_addr(url).await,
                _ => None,
            };
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "offer resolved advertise_tcp_via_url={:?} → tcp_candidate={:?}",
                    advertise_tcp_via_url.as_deref().unwrap_or(""),
                    tcp_advertised_addr
                ),
                turn: None,
            });
            // Loopback TCP candidates (127.0.0.1 / ::1) are silently
            // dropped by browsers as anti-rebinding mitigation (same
            // filter documented for the local path at
            // display/webrtc.rs:38-43; the federated path hits the
            // same trap when an operator configures a `localhost:NNNN`
            // tunnel on the primary side but the browser doesn't have
            // a matching loopback tunnel). No observable signaling
            // failure — ICE just silently never pairs. Emit a
            // prominent warn here so operators catch it at the first
            // Offer rather than debugging by inference through
            // "media never forms despite signaling completing."
            if let Some(addr) = tcp_advertised_addr {
                if addr.ip().is_loopback() {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "advertise_tcp_via_url resolved to loopback ({}) — \
                             browsers silently drop remote loopback ICE \
                             candidates (anti-rebinding mitigation), so ICE-TCP \
                             will never pair. Configure the peer's \
                             `browser_tcp_via_url` (slice 3a.4) with a \
                             non-loopback address the browser's machine can \
                             reach (LAN IP, port-forward on a real NIC, \
                             Tailscale URL, etc.) or wait for slice 3b's \
                             primary-as-media-relay fallback.",
                            addr
                        ),
                        turn: None,
                    });
                }
            }
            let (ice_tx, mut ice_rx) =
                tokio::sync::mpsc::channel::<(crate::display::PeerId, String)>(64);
            // F-2: federated input gate. Replaces F-1's deny-everything
            // stub with a registry lookup keyed on this peer's
            // `(federation_connection_id, session_id)`. Symmetric in
            // shape to the local 5c authorizer above — the closure is
            // the entire boundary, `display/mod.rs` doesn't see the
            // registry. Strict deny-by-default for unclaimed (no
            // holder); only the matching federated holder identity
            // returns true. See [`build_federated_input_authorizer`]
            // for the matching positive/negative test cases.
            let input_authorized = build_federated_input_authorizer(
                display_id,
                federation_connection_id.clone(),
                session_id.clone(),
                Arc::clone(&display_input_authority),
            );
            // F-1.3b3: real federated authority handler. Identity is
            // captured at construction so messages from this peer
            // always arbitrate against this peer's
            // `(federation_connection_id, session_id)`. Display-ID
            // mismatches drop silently (the federated peer is bound
            // to one display).
            let authority_handler = build_federated_authority_handler(
                display_id,
                federation_connection_id.clone(),
                session_id.clone(),
                Arc::clone(&display_input_authority),
                authority_change_tx.clone(),
            );
            let answer_result = session
                .handle_offer(
                    peer_id,
                    &sdp,
                    ice_config,
                    Some(tcp_peer_registry.clone()),
                    tcp_advertised_addr,
                    ice_tx,
                    input_authorized,
                    authority_handler,
                )
                .await;
            match answer_result {
                Ok(answer_sdp) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "info".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "offer handled, sending answer back to connector (display={display_id} session={session_id} answer_len={} bytes)",
                            answer_sdp.len()
                        ),
                        turn: None,
                    });
                    // F-1.3b3: register the federated peer as an
                    // authority subscriber. Sends the initial
                    // personalized snapshot (queue-or-send via
                    // F-1.2's pending_authority_state) and spawns
                    // the per-subscriber fanout task. If the peer
                    // was removed since handle_offer returned (race
                    // with a fast Close), `get_peer` returns None
                    // and we skip registration — the Close arm's
                    // unregister is a no-op for an entry that was
                    // never inserted, so the asymmetry is safe.
                    if let Some(peer_arc) = session.get_peer(peer_id).await {
                        register_federated_authority_subscriber(
                            federation_connection_id.clone(),
                            session_id.clone(),
                            display_id,
                            peer_arc,
                            Arc::clone(&display_input_authority),
                            authority_change_tx.clone(),
                            Arc::clone(&federated_authority_subscribers),
                        );
                    }
                    // D-3c: federated PeerDisplayConnection creates
                    // tile-stream data channels; local DisplaySlot
                    // peers do not. Register only this federated peer
                    // so snapshots/updates are not queued forever on
                    // local peers without tile channels.
                    session.register_tile_subscriber(peer_id).await;
                    let answer = crate::types::OutboundEvent::WebRtcSignal {
                        display_id,
                        session_id: session_id.clone(),
                        signal: crate::peer::WebRtcSignal::Answer { sdp: answer_sdp },
                    };
                    match serde_json::to_string(&answer) {
                        Ok(s) => {
                            if direct_tx.send(s).is_err() {
                                bus.send(AppEvent::LogEntry {
                                    session_id: None,
                                    level: "warn".to_string(),
                                    source: LOG_SOURCE.to_string(),
                                    content: format!(
                                        "failed to send answer to connector — direct_tx closed (display={display_id} session={session_id})"
                                    ),
                                    turn: None,
                                });
                            }
                        }
                        Err(e) => {
                            bus.send(AppEvent::LogEntry {
                                session_id: None,
                                level: "error".to_string(),
                                source: LOG_SOURCE.to_string(),
                                content: format!(
                                    "failed to serialize answer (display={display_id} session={session_id}): {e}"
                                ),
                                turn: None,
                            });
                        }
                    }

                    // Drain the per-session ICE channel and forward
                    // server-side trickle candidates as separate
                    // WebRtcSignal frames. Task exits when the
                    // session removes the peer (channel closes).
                    let direct_tx_ice = direct_tx.clone();
                    let session_id_ice = session_id;
                    let bus_ice = bus.clone();
                    tokio::spawn(async move {
                        let mut count: u32 = 0;
                        while let Some((_pid, candidate_json)) = ice_rx.recv().await {
                            count = count.saturating_add(1);
                            let evt = crate::types::OutboundEvent::WebRtcSignal {
                                display_id,
                                session_id: session_id_ice.clone(),
                                signal: crate::peer::WebRtcSignal::IceCandidate { candidate_json },
                            };
                            if let Ok(s) = serde_json::to_string(&evt) {
                                if direct_tx_ice.send(s).is_err() {
                                    bus_ice.send(AppEvent::LogEntry {
                                        session_id: None,
                                        level: "debug".to_string(),
                                        source: LOG_SOURCE.to_string(),
                                        content: format!(
                                            "ice forwarder exiting — direct_tx closed (display={display_id} session={session_id_ice}) after {count} candidates"
                                        ),
                                        turn: None,
                                    });
                                    break;
                                }
                            }
                        }
                        bus_ice.send(AppEvent::LogEntry {
                            session_id: None,
                            level: "debug".to_string(),
                            source: LOG_SOURCE.to_string(),
                            content: format!(
                                "ice forwarder finished — forwarded {count} candidates (display={display_id} session={session_id_ice})"
                            ),
                            turn: None,
                        });
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "handle_offer failed (display={display_id} session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::IceCandidate { candidate_json } => {
            match session.add_ice_candidate(peer_id, &candidate_json).await {
                Ok(()) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "debug".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "applied connector ICE candidate (display={display_id} session={session_id})"
                        ),
                        turn: None,
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "add_ice_candidate failed (display={display_id} session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::Answer { .. } => {
            // Protocol error: this side is the offer-receiver. Browsers
            // send Offers via the primary's federation transport;
            // peers reply with Answers via OutboundEvent::WebRtcSignal.
            // An incoming Answer here means a confused sender — log
            // and drop rather than silently mishandling.
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "unexpected Answer received on peer side (display={display_id} session={session_id}) — ignoring"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Close => {
            session.remove_peer(peer_id).await;
            // F-1.3b3: matched-identity authority release + matched
            // subscriber unregister. The release helper is a no-op
            // unless this exact `(federation_connection_id,
            // session_id)` currently holds the slot — distinct tabs
            // from the same primary have distinct session_ids and
            // can't unclaim each other (the F-1.3b1 helper enforces
            // this). The unregister tears down this peer's authority
            // fanout task; remaining federated subscribers and local
            // 5c subscribers see the (possible) `unclaimed` broadcast
            // through their own subscriber loops. Federation WS-close
            // does the bulk variant of both at the gateway WS-close
            // hook.
            apply_release_input_authority_federated(
                display_id,
                &federation_connection_id,
                &session_id,
                &display_input_authority,
                &authority_change_tx,
            );
            unregister_federated_authority_subscriber(
                &federation_connection_id,
                &session_id,
                display_id,
                &federated_authority_subscribers,
            );
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "removed per-session WebRtcPeer on Close (display={display_id} session={session_id})"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Unknown => {
            // Forward-compat fallback for signal kinds added by newer
            // builds. Older daemons silently ignore — but log at
            // debug so the operator can see unknown signal arrivals
            // when they're hunting wire-format issues.
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "ignoring unknown WebRtcSignal kind (display={display_id} session={session_id})"
                ),
                turn: None,
            });
        }
    }
}

/// Handle `POST /api/peers/{id}/message`.
async fn peers_send_message(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: SendMessageRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let msg = match req.into_message() {
        Ok(m) => m,
        Err(e) => {
            return (400, serde_json::json!({"error": e}).to_string());
        }
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.send_message(msg).await {
        Ok(message_id) => (
            200,
            serde_json::json!({"message_id": message_id.0}).to_string(),
        ),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `POST /api/peers/{id}/task`.
async fn peers_delegate_task(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: DelegateTaskRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let task = crate::peer::PeerTask {
        instructions: req.instructions,
        context: req.context,
        client_correlation_id: req.client_correlation_id,
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.delegate_task(task).await {
        Ok(task_id) => (200, serde_json::json!({"task_id": task_id.0}).to_string()),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `POST /api/peers/{id}/approval`.
async fn peers_resolve_approval(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: ResolveApprovalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.resolve_approval(&req.request_id, req.decision).await {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(e) => peer_error_response(e),
    }
}

// ---------------------------------------------------------------------------
// Coordinator endpoints — capability-based discovery + delegation
// ---------------------------------------------------------------------------

/// Parse `?capability=display&capability=custom:foo` into a typed
/// `Vec<Capability>` plus a list of unknown strings (for diagnostics).
/// Empty input returns `(vec![], vec![])` — empty-required-capabilities
/// matches every peer, which the handler rejects upstream.
fn parse_capability_query(query: &str) -> (Vec<crate::peer::Capability>, Vec<String>) {
    let mut caps = Vec::new();
    let mut unknown = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k != "capability" {
            continue;
        }
        match crate::peer::Capability::from_query_string(v) {
            Some(cap) => caps.push(cap),
            None => unknown.push(v.to_string()),
        }
    }
    (caps, unknown)
}

/// Handle `GET /api/peers/eligible?capability=...`. Returns the
/// connected peers whose Agent Card advertises every requested
/// capability. Each entry is a [`crate::peer::PeerSnapshot`] —
/// same shape as `/api/peers` so the dashboard can reuse rendering.
fn peers_eligible(registry: &crate::peer::PeerRegistry, query_str: &str) -> (u16, String) {
    let (caps, unknown) = parse_capability_query(query_str);
    if !unknown.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": format!(
                    "unrecognized capability values: {}",
                    unknown.join(", ")
                ),
                "hint": "use kebab-case kind names (display, computer-use, ...) or `custom:<name>`"
            })
            .to_string(),
        );
    }
    if caps.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": "at least one ?capability=... is required"
            })
            .to_string(),
        );
    }
    let coordinator = crate::peer::Coordinator::new(registry.clone());
    let peers: Vec<crate::peer::PeerSnapshot> = coordinator
        .eligible_peers(&caps)
        .iter()
        .map(|h| h.snapshot())
        .collect();
    let body = serde_json::json!({ "peers": peers }).to_string();
    (200, body)
}

/// JSON body shape for `POST /api/coordinator/route`.
#[derive(Deserialize)]
struct CoordinatorRouteRequest {
    /// Capabilities the executing peer must advertise. Each string is
    /// parsed via `Capability::from_query_string` for consistency with
    /// the eligible endpoint's URL query (kebab-case + `custom:<name>`).
    required_capabilities: Vec<String>,
    /// Wire-level task payload routed to the winning peer.
    task: CoordinatorRouteTask,
}

#[derive(Deserialize)]
struct CoordinatorRouteTask {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

/// Handle `POST /api/coordinator/route`. Routes the task to a
/// connected peer that satisfies all required capabilities,
/// returning the assigned task id on success or a structured error
/// on no-route / delegation failure.
async fn coordinator_route(registry: &crate::peer::PeerRegistry, body_text: &str) -> (u16, String) {
    let req: CoordinatorRouteRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };

    // Translate the wire capability strings into typed Capability
    // values. Same parser as the eligible endpoint — keeps the URL
    // and JSON surfaces consistent.
    let mut caps = Vec::with_capacity(req.required_capabilities.len());
    let mut unknown = Vec::new();
    for s in &req.required_capabilities {
        match crate::peer::Capability::from_query_string(s) {
            Some(c) => caps.push(c),
            None => unknown.push(s.clone()),
        }
    }
    if !unknown.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": format!(
                    "unrecognized capability values: {}",
                    unknown.join(", ")
                ),
                "hint": "use kebab-case kind names (display, computer-use, ...) or `custom:<name>`"
            })
            .to_string(),
        );
    }
    if caps.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": "required_capabilities must not be empty"
            })
            .to_string(),
        );
    }

    let task = crate::peer::PeerTask {
        instructions: req.task.instructions,
        context: req.task.context,
        client_correlation_id: req.task.client_correlation_id,
    };
    let coordinator = crate::peer::Coordinator::new(registry.clone());
    let request = crate::peer::TaskRequest {
        required_capabilities: caps,
        task,
    };
    match coordinator.route_task(request).await {
        Ok(routed) => (
            200,
            serde_json::json!({
                "peer_id": routed.peer_id.as_str(),
                "task_id": routed.task_id.0,
            })
            .to_string(),
        ),
        Err(crate::peer::CoordinatorError::NoRoute {
            required,
            considered,
        }) => (
            404,
            serde_json::json!({
                "error": "no route",
                "required_capabilities": required
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>(),
                "considered": considered.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            })
            .to_string(),
        ),
        Err(crate::peer::CoordinatorError::DelegationFailed { peer, error }) => (
            502,
            serde_json::json!({
                "error": format!("delegation to {peer} failed: {error}"),
                "peer_id": peer.as_str(),
            })
            .to_string(),
        ),
    }
}

/// True for HTTP requests that hit the federation REST surface:
/// `/api/peers*`, `/api/coordinator/*`, `/api/sessions`, and
/// `/api/worktrees`. These
/// are the endpoints the bearer-token enforcement layer protects
/// when `[server.auth] bearer_token` is set. Discovery
/// (`/.well-known/agent-card.json`), browser bootstrap (`/config`,
/// `/`, `/static/*`), and `/ws` are exempt — see
/// `spawn_web_gateway::inbound_bearer_token` docs for why.
fn is_federation_path(request_line: &str) -> bool {
    request_line.contains(" /api/peers")
        || request_line.contains(" /api/coordinator/")
        || request_line.contains(" /api/sessions")
        || request_line.contains(" /api/worktrees")
}

/// Extract a token from the `?token=...` query parameter of an HTTP
/// request line. Used by the WebSocket upgrade auth path because the
/// browser cannot set arbitrary headers on `WebSocket` opens — the
/// dashboard appends `?token=...` to the /ws URL instead.
///
/// `request_line` is the first line of the HTTP request, e.g.
/// `"GET /ws?token=abc HTTP/1.1"`. Returns the extracted token if
/// present, `None` if there's no `?token=` parameter.
pub(crate) fn extract_token_query_param(request_line: &str) -> Option<String> {
    let path_and_query = request_line.split_whitespace().nth(1)?;
    let query = path_and_query.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            // No URL-decoding: bearer tokens are typically URL-safe
            // (hex / base64-url). If a token contains characters that
            // require encoding, the operator can either pick a
            // different token or send via Authorization header (which
            // doesn't have the URL-encoding constraint).
            return Some(value.to_string());
        }
    }
    None
}

/// Verify a WebSocket upgrade request carries the expected bearer
/// token. Browser WebSocket clients cannot natively set custom
/// headers on `WebSocket` opens, so this accepts the token in EITHER
/// an `Authorization: Bearer <token>` header (sent by
/// `IntendantWsTransport` from the daemon side) OR a `?token=...`
/// URL query parameter (sent by the browser dashboard). The dual
/// path is the standard pragmatic workaround for the browser
/// limitation.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token via either method. Returns `Err((401, body))`
/// otherwise — the caller writes a plain HTTP 401 response *before*
/// the WebSocket handshake and returns, so the rejected client never
/// sees a successful upgrade.
pub(crate) fn verify_bearer_for_ws(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };

    // Try the Authorization header first (cheaper and the daemon-to-
    // daemon path uses it). On miss, fall back to the URL query.
    if verify_bearer_token(header_text, Some(expected)).is_ok() {
        return Ok(());
    }

    let request_line = header_text.lines().next().unwrap_or("");
    if extract_token_query_param(request_line).as_deref() == Some(expected) {
        return Ok(());
    }

    Err((
        401,
        serde_json::json!({
            "error": "missing or invalid bearer token (Authorization header or ?token=)"
        })
        .to_string(),
    ))
}

/// Verify a federation HTTP request carries the expected bearer
/// token in the `Authorization` header. Header name lookup is
/// case-insensitive per the HTTP spec; the `Bearer` scheme prefix
/// match accepts either case.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token. Returns `Err((401, body_json))` otherwise —
/// the caller writes that response and returns.
pub(crate) fn verify_bearer_token(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };
    let auth_header = header_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("authorization") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let auth = match auth_header {
        Some(v) => v,
        None => {
            return Err((
                401,
                serde_json::json!({"error": "missing Authorization header"}).to_string(),
            ));
        }
    };
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "));
    let token = match token {
        Some(t) => t.trim(),
        None => {
            return Err((
                401,
                serde_json::json!({
                    "error": "Authorization header must use Bearer scheme"
                })
                .to_string(),
            ));
        }
    };
    if token == expected {
        Ok(())
    } else {
        Err((
            401,
            serde_json::json!({"error": "invalid bearer token"}).to_string(),
        ))
    }
}

/// Resolve the list of WebSocket URLs to advertise in the Agent
/// Card for this daemon, in preference order.
///
/// **Additive auto-detection.** Mirrors WebRTC's host-candidate
/// gathering pattern: the daemon enumerates its own routable
/// interfaces via [`crate::lan::routable_local_addrs`] and emits one
/// URL per address by default, so the operator doesn't need to type
/// their own LAN IP into `--advertise-url`. The operator's overrides
/// (CLI `--advertise-url` or `[server.advertise]` in intendant.toml)
/// are *prepended* — they win on preference order, but the auto-
/// detected entries still ride along as fallbacks. The connecting
/// peer's `MultiTransport::connect` walks the merged list top-down
/// and picks the first that succeeds.
///
/// ## Bind-address rules
///
/// - **Specific bind** (e.g. `192.168.1.42:8765`): only that one IP
///   is auto-detected. The operator narrowed the listener for a
///   reason; we don't second-guess by also enumerating other
///   interfaces.
/// - **Wildcard bind** (`0.0.0.0` / `::`): every routable interface
///   becomes one URL. Loopback is excluded — advertising loopback to
///   remote peers is useless. If the operator wants to expose
///   loopback (e.g. for self-peering tests), they can pass it via
///   `--advertise-url`.
///
/// ## Fallbacks (in order, when auto-detection finds nothing)
///
/// 1. Resolved host label ([`crate::lan::resolve_host_label`]) —
///    works on a trusted LAN with mDNS, fragile elsewhere. Last-
///    ditch best-effort.
/// 2. `ws://localhost:0/ws` if there's no listener at all
///    (shouldn't happen in practice; the listener is always bound by
///    the time spawn is called). Card stays valid; URL won't work.
///
/// Dedup: exact-string match. If the operator's override happens to
/// match an auto-detected URL, only the operator's copy is kept.
pub(crate) fn resolve_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    overrides: &[String],
) -> Vec<String> {
    let port = local_addr.map(|a| a.port()).unwrap_or(0);

    // Auto-detect. Operator overrides come first; auto entries append.
    let auto = auto_detect_advertise_urls(local_addr, port);

    let mut out: Vec<String> = Vec::with_capacity(overrides.len() + auto.len());
    for url in overrides {
        if !out.contains(url) {
            out.push(url.clone());
        }
    }
    for url in auto {
        if !out.contains(&url) {
            out.push(url);
        }
    }

    if out.is_empty() {
        // No bind, no overrides, no interfaces. Card stays valid;
        // URL just won't work until the next daemon restart.
        out.push("ws://localhost:0/ws".to_string());
    }
    out
}

/// Build the auto-detected URL list from the listener bind address.
/// See [`resolve_advertise_urls`] for the full resolution rules.
fn auto_detect_advertise_urls(local_addr: Option<std::net::SocketAddr>, port: u16) -> Vec<String> {
    use std::net::IpAddr;
    let Some(addr) = local_addr else {
        return Vec::new();
    };

    // Specific bind: that one IP wins, no enumeration.
    match addr.ip() {
        IpAddr::V4(v4) if !v4.is_unspecified() => {
            return vec![format_ws_url(&v4.to_string(), port)];
        }
        IpAddr::V6(v6) if !v6.is_unspecified() => {
            return vec![format_ws_url(&format!("[{v6}]"), port)];
        }
        _ => {}
    }

    // Wildcard bind: enumerate every non-loopback routable interface.
    // IPv4 entries sort before IPv6 — WebRTC ICE-TCP in WebKit/WKWebView
    // silently drops IPv6 ULA candidates (seen empirically against
    // fdc2::/8 addresses on macOS 15), so the *first* URL in the list
    // — which slice 3b's `maybe_rewrite_federated_answer` takes as the
    // relay candidate verbatim — needs to be the one browsers actually
    // dial. Within each address family we preserve `getifaddrs` order
    // (`stable_sort_by`), so a multi-NIC host that already had a
    // preferred primary interface keeps it.
    let mut ips = crate::lan::routable_local_addrs(false);
    ips.sort_by(|a, b| match (a, b) {
        (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
        (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut urls: Vec<String> = ips
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V6(v6) => format_ws_url(&format!("[{v6}]"), port),
            ip => format_ws_url(&ip.to_string(), port),
        })
        .collect();

    // No interfaces found (unusual — host with no networking?). Fall
    // back to the resolved host label so the card carries *something*
    // dialable on a trusted LAN with mDNS.
    if urls.is_empty() {
        urls.push(format_ws_url(&crate::lan::resolve_host_label(), port));
    }
    urls
}

fn format_ws_url(host: &str, port: u16) -> String {
    format!("ws://{host}:{port}/ws")
}

/// Assemble the [`crate::peer::AgentCard`] for this daemon from live
/// runtime state.
///
/// Called once per `spawn_web_gateway` invocation, right after the
/// config is serialized — the result is cached as `agent_card_json`
/// and cloned into each per-connection handler, matching the pattern
/// used for `/config`.
///
/// Capabilities:
/// - `ComputerUse`, `Knowledge`, `Display` are always-on subsystems
///   compiled into every build and always able to service a federation
///   request (for `Display`, that's `DisplaySession::handle_offer`
///   against whatever the local dashboard has activated — returns
///   "no such display" if nothing is active, which is the correct
///   semantics for a peer trying to view a display the operator
///   hasn't opened yet).
/// - `Voice` / `Phone` / `Recording` are gated on runtime configuration
///   that isn't plumbed through here yet. Those become additive as
///   each subsystem teaches itself to advertise, likely via dynamic
///   `PeerEvent::CapabilityEngaged` once slice 3a.2 lands.
///
/// `advertise_urls` is the preference-ordered list of WebSocket URLs
/// peers should try when dialing this daemon. Each becomes a
/// [`crate::peer::TransportSpec::IntendantWs`] entry in the card.
/// Built by [`resolve_advertise_urls`], which merges operator
/// overrides (`--advertise-url`, `[server.advertise]`) with auto-
/// detected fallback. The list is non-empty by construction.
///
/// `auth` is the [`crate::peer::AuthRequirements`] to advertise —
/// what connecting peers should send. Built by
/// `crate::main::build_local_advertised_auth` from
/// `[server.auth]` (advertised_transport + bearer_token) and the
/// LAN cert dir (for `pin-self-cert` fingerprint). Phase 1 of slice
/// 2c always passed `AuthRequirements::none()`; this signature
/// change lets the operator advertise mTLS / pinned-mTLS / bearer
/// in the card so connecting peers know what to send.
pub fn build_local_agent_card(
    advertise_urls: Vec<String>,
    auth: crate::peer::AuthRequirements,
) -> crate::peer::AgentCard {
    use crate::peer::{Capability, TransportSpec};
    let transports: Vec<TransportSpec> = advertise_urls
        .into_iter()
        .map(|url| TransportSpec::IntendantWs { url })
        .collect();
    crate::peer::AgentCard::local_intendant(
        crate::lan::resolve_host_label(),
        env!("CARGO_PKG_VERSION").to_string(),
        Some(env!("INTENDANT_GIT_SHA").to_string()),
        transports,
        vec![
            Capability::ComputerUse,
            Capability::Knowledge,
            Capability::Display,
        ],
        auth,
    )
}

fn build_config_inner(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_servers: Vec<crate::display::IceServer>,
) -> WebGatewayConfig {
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
        return WebGatewayConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            input_sample_rate: input_rate,
            output_sample_rate: output_rate,
            transcription_enabled,
            ice_servers,
            ..Default::default()
        };
    }

    // If an explicit live model is given, detect provider from the model name.
    if let Some(model) = live_model {
        if model.starts_with("gpt")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
        {
            return WebGatewayConfig {
                provider: "openai".to_string(),
                model: model.to_string(),
                input_sample_rate: 24000,
                output_sample_rate: 24000,
                transcription_enabled,
                ice_servers,
                ..Default::default()
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            ..Default::default()
        };
    }

    // Fall back to env var detection
    if std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("GEMINI_API_KEY").is_err() {
        WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            ..Default::default()
        }
    } else {
        let mut cfg = WebGatewayConfig::default();
        cfg.transcription_enabled = transcription_enabled;
        cfg.ice_servers = ice_servers;
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutboundEvent;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn test_default_port() {
        assert_eq!(DEFAULT_PORT, 8765);
    }

    #[test]
    fn initial_body_bytes_preserves_non_utf8_upload_prefix() {
        let mut request =
            b"POST /api/session/current/uploads HTTP/1.1\r\nContent-Length: 4\r\n\r\n".to_vec();
        request.extend_from_slice(&[0xff, 0x00, 0x80, b'a']);

        assert_eq!(
            initial_body_bytes(&request).unwrap(),
            &[0xff, 0x00, 0x80, b'a']
        );
    }

    #[test]
    fn initial_body_bytes_rejects_incomplete_headers() {
        let request = b"POST /api/session/current/uploads HTTP/1.1\r\nContent-Length: 4\r\n";
        assert!(initial_body_bytes(request).is_err());
    }

    #[test]
    fn preview_text_truncates_on_char_boundary() {
        let text = "Wait, the `CONCURRENT AGENTS (n)` indicator is at the top — where";

        assert_eq!(
            preview_text(text, 60),
            "Wait, the `CONCURRENT AGENTS (n)` indicator is at the top — ..."
        );
    }

    #[test]
    fn preview_text_leaves_short_unicode_unchanged() {
        assert_eq!(preview_text("top — where", 60), "top — where");
    }

    #[test]
    fn upload_destination_falls_back_to_workspace_without_active_session() {
        assert_eq!(
            effective_upload_destination(crate::upload_store::UploadDestination::Task, false,),
            crate::upload_store::UploadDestination::Workspace
        );
        assert_eq!(
            effective_upload_destination(crate::upload_store::UploadDestination::Workspace, false,),
            crate::upload_store::UploadDestination::Workspace
        );
        assert_eq!(
            effective_upload_destination(crate::upload_store::UploadDestination::Task, true,),
            crate::upload_store::UploadDestination::Task
        );
    }

    #[test]
    fn pending_upload_session_dir_is_project_scoped() {
        let root = std::path::PathBuf::from("/tmp/project");
        assert_eq!(
            pending_upload_session_dir(&root),
            root.join(".intendant").join("pending_uploads")
        );
    }

    #[test]
    fn dashboard_fs_stat_reports_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let status = inspect_dashboard_fs_path(dir.path().to_str().unwrap()).unwrap();

        assert!(status.exists);
        assert!(status.is_dir);
        assert!(status.readable);
        assert!(!status.can_create);
    }

    #[test]
    fn dashboard_fs_stat_marks_missing_directory_creatable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("new").join("project");
        let status = inspect_dashboard_fs_path(missing.to_str().unwrap()).unwrap();

        assert!(!status.exists);
        assert!(status.can_create);
        assert_eq!(
            status.nearest_existing_parent.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn dashboard_fs_mkdir_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("new").join("project");
        let result = mkdir_dashboard_fs_path(missing.to_str().unwrap()).unwrap();

        assert_eq!(result["created"], true);
        assert_eq!(result["already_exists"], false);
        assert!(missing.is_dir());
    }

    #[test]
    fn dashboard_fs_mkdir_reports_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = mkdir_dashboard_fs_path(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(result["created"], false);
        assert_eq!(result["already_exists"], true);
    }

    #[test]
    fn external_session_json_falls_back_to_created_at_for_updated_at() {
        let session = external_session_json(
            "codex",
            "Codex",
            "session-1".to_string(),
            "session-1".to_string(),
            Some("2026-05-17T10:00:00Z".to_string()),
            None,
            Some("name".to_string()),
            Some("task".to_string()),
            "Codex",
            None,
            1,
            None,
            None,
            None,
            0,
        );

        assert_eq!(session["created_at"], "2026-05-17T10:00:00Z");
        assert_eq!(session["updated_at"], "2026-05-17T10:00:00Z");
        assert_eq!(session["name"], "name");
    }

    #[test]
    fn external_agent_thread_id_is_extracted_from_log_messages() {
        assert_eq!(
            external_agent_thread_id_from_message(
                "External agent thread: 019e41de-e785-7581-85dd-8e74bb464c6c"
            )
            .as_deref(),
            Some("019e41de-e785-7581-85dd-8e74bb464c6c")
        );
        assert_eq!(
            external_agent_thread_id_from_message(
                "Mode: external agent (Codex) via presence, thread: codex-session-1"
            )
            .as_deref(),
            Some("codex-session-1")
        );
        assert_eq!(
            external_agent_source_from_message(
                "Mode: external agent (Claude Code) via presence, thread: claude-session-1"
            )
            .as_deref(),
            Some("claude-code")
        );
    }

    #[test]
    fn external_session_context_indexes_session_and_resume_ids() {
        let sessions = vec![serde_json::json!({
            "session_id": "display-id",
            "resume_id": "resume-id",
            "project_root": "/repo",
            "cwd": "/repo/.worktrees/feature",
            "source": "codex",
            "source_label": "Codex",
            "name": "Dashboard task"
        })];

        let context = external_session_context_by_id(&sessions);
        assert_eq!(
            context
                .get("display-id")
                .and_then(|ctx| ctx.project_root.as_deref()),
            Some("/repo")
        );
        assert_eq!(
            context.get("resume-id").and_then(|ctx| ctx.cwd.as_deref()),
            Some("/repo/.worktrees/feature")
        );
        assert_eq!(
            context
                .get("resume-id")
                .and_then(|ctx| ctx.source.as_deref()),
            Some("codex")
        );
        assert_eq!(
            context
                .get("resume-id")
                .and_then(|ctx| ctx.source_label.as_deref()),
            Some("Codex")
        );
        assert_eq!(
            context.get("resume-id").and_then(|ctx| ctx.name.as_deref()),
            Some("Dashboard task")
        );
    }

    #[test]
    fn list_sessions_joins_external_context_from_debug_thread_log() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("feature");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let intendant_id = "intendant-wrapper-session";
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(intendant_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": intendant_id,
                "created_at": "2026-05-17T20:44:00",
                "project_root": repo.to_string_lossy(),
                "task": "Dashboard-started Codex task",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();

        let codex_id = "019e37ae-dashboard-started";
        let intendant_lines = [
            serde_json::json!({
                "ts": "2026-05-17T20:44:01",
                "event": "debug",
                "message": "Mode: external agent (Codex)"
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:02",
                "event": "debug",
                "message": format!("External agent thread: {codex_id}")
            }),
        ];
        std::fs::write(
            log_dir.join("session.jsonl"),
            intendant_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let codex_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": codex_id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "cwd": command_cwd.to_string_lossy()
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{codex_id}.jsonl")),
            codex_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        assert!(
            sessions.iter().all(|s| {
                !(s.get("source").and_then(|v| v.as_str()) == Some("intendant")
                    && s.get("session_id").and_then(|v| v.as_str()) == Some(intendant_id))
            }),
            "intendant wrapper should be merged into the native external session row"
        );
        let wrapped = sessions
            .iter()
            .find(|s| {
                s.get("source").and_then(|v| v.as_str()) == Some("codex")
                    && s.get("session_id").and_then(|v| v.as_str()) == Some(codex_id)
            })
            .expect("native Codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            wrapped.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            wrapped.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
        assert_eq!(
            wrapped.get("backend_source").and_then(|v| v.as_str()),
            Some("codex")
        );
        assert_eq!(
            wrapped.get("backend_source_label").and_then(|v| v.as_str()),
            Some("Codex")
        );
        assert_eq!(
            wrapped.get("backend_session_id").and_then(|v| v.as_str()),
            Some(codex_id)
        );
        assert_eq!(
            wrapped.get("intendant_session_id").and_then(|v| v.as_str()),
            Some(intendant_id)
        );
    }

    #[test]
    fn session_log_search_finds_intendant_log_content_not_summary() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "intendant-search-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "ordinary dashboard task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": "Detailed log contains alpha-search-token"
            })
            .to_string(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha-search-token",
            "all",
            "",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("session_id").and_then(|v| v.as_str()),
            Some(session_id)
        );
        assert_eq!(
            results[0].get("source").and_then(|v| v.as_str()),
            Some("intendant")
        );
    }

    #[test]
    fn session_log_search_can_filter_external_agent_sessions() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let id = "019e37ae-search-filter";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:40Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "ordinary request"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:50Z",
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "external-only beta-search-token"
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "beta-search-token",
            "external",
            "",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("source").and_then(|v| v.as_str()),
            Some("codex")
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "beta-search-token",
            "intendant",
            "",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_log_search_supports_exact_phrase_mode() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "exact-phrase-search-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "exact phrase task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": "Needle words appear as alpha phrase token"
            })
            .to_string(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha phrase",
            "all",
            "exact_phrase",
        ))
        .unwrap();
        assert_eq!(
            response
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha token",
            "all",
            "exact_phrase",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_log_search_supports_any_keyword_session_mode() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "any-keyword-search-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "any keyword task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": "This line contains only one-side-token"
            })
            .to_string(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "one-side-token absent-token",
            "all",
            "all_keywords",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "one-side-token absent-token",
            "all",
            "any_keyword_session",
        ))
        .unwrap();
        assert_eq!(
            response
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn session_log_search_supports_user_message_mode() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let id = "019e37ae-user-message-search";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:40Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "user-only alpha-token beta-token"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:50Z",
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "assistant-only gamma-token delta-token"
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha-token beta-token",
            "codex",
            "user_message_all_keywords",
        ))
        .unwrap();
        assert_eq!(
            response
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "gamma-token delta-token",
            "codex",
            "user_message_all_keywords",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn list_codex_sessions_uses_first_real_user_message() {
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-f523-73b0-8bb4-01be02f30ebd";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z",
                "thread_name": "# AGENTS.md instructions for /Users/vm/projects/intendant <INSTRUCTIONS>"
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/Users/vm/projects/intendant",
                    "model": "gpt-5.5",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /Users/vm/projects/intendant <INSTRUCTIONS>"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Fix the Sessions tab"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix the Sessions tab"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix the Sessions tab")
        );
        assert_eq!(session.get("name").and_then(|v| v.as_str()), None);
        assert_eq!(session.get("turns").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn list_codex_sessions_exposes_thread_name_separately_from_task() {
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-thread-name";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z",
                "thread_name": "Rehydration fix"
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/Users/vm/projects/intendant"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix activity replay"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("name").and_then(|v| v.as_str()),
            Some("Rehydration fix")
        );
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix activity replay")
        );
    }

    #[test]
    fn list_sessions_applies_external_session_name_overlay() {
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-overlay-name";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z"
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/Users/vm/projects/intendant"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix naming"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();
        crate::session_names::rename_session(home.path(), "codex", id, "Overlay name").unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("name").and_then(|v| v.as_str()),
            Some("Overlay name")
        );
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix naming")
        );
    }

    #[test]
    fn list_codex_sessions_separates_project_root_from_latest_command_cwd() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("feature");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-project-cwd-split";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "cwd": command_cwd.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:22Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Inspect cwd"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            session.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            session.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
    }

    #[test]
    fn list_codex_sessions_uses_function_call_workdir_as_latest_cwd() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("live-cwd");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-function-call-workdir";
        let arguments = serde_json::json!({
            "cmd": "pwd",
            "workdir": command_cwd.to_string_lossy()
        })
        .to_string();
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": arguments
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:22Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Inspect cwd"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            session.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            session.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
    }

    #[test]
    fn list_codex_sessions_applies_thread_rollback_to_turns_and_task() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37b2-e756-7461-9946-34b639448717";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:48:52Z",
                "type": "session_meta",
                "payload": {"id": id, "timestamp": "2026-05-17T20:48:52Z"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "old-turn"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Old prompt"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Old prompt"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "turn_aborted", "turn_id": "old-turn"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "thread_rolled_back", "num_turns": 1}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "new-turn"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "New prompt"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "New prompt"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-48-52-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("New prompt")
        );
        assert_eq!(session.get("turns").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn list_codex_sessions_parses_token_count_usage() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37c5-9d93-76f0-a395-f5b28bd54a74";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "cwd": "/Users/vm/projects/intendant",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:01Z",
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:02Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix stats usage"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:03Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        },
                        "last_token_usage": {
                            "input_tokens": 200,
                            "cached_input_tokens": 50,
                            "output_tokens": 25,
                            "total_tokens": 225
                        },
                        "model_context_window": 258400
                    }
                }
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(1000));
        assert_eq!(session["completion_tokens"].as_u64(), Some(250));
        assert_eq!(session["cached_tokens"].as_u64(), Some(400));
        assert_eq!(session["total_tokens"].as_u64(), Some(1250));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.00535).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn list_codex_sessions_inherits_model_from_parent_thread() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019e37c5-parent-model-thread";
        let child_id = "019e37c5-child-forked-thread";
        let parent_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:09:00Z",
                "type": "session_meta",
                "payload": {
                    "id": parent_id,
                    "timestamp": "2026-05-17T21:09:00Z",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:09:01Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5.5"}
            }),
        ];
        let child_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:01Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Use inherited model"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        }
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-09-00-{parent_id}.jsonl")),
            parent_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{child_id}.jsonl")),
            child_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(child_id))
            .expect("child codex session should be listed");
        assert_eq!(session["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(session["pricing_known"].as_bool(), Some(true));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.0107).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn list_claude_sessions_parses_and_deduplicates_usage() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-Users-vm-projects-intendant");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_id = "019e37cf-34ad-7b08-8a1e-7ad5086eb39f";
        let assistant = serde_json::json!({
            "timestamp": "2026-05-17T21:20:02Z",
            "type": "assistant",
            "cwd": "/Users/vm/projects/intendant",
            "requestId": "req-usage-1",
            "message": {
                "id": "msg-usage-1",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 10,
                    "cache_creation_input_tokens": 20,
                    "cache_read_input_tokens": 30,
                    "output_tokens": 40
                }
            }
        });
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:20:00Z",
                "type": "user",
                "cwd": "/Users/vm/projects/intendant",
                "message": {"content": "Fix stats usage"}
            }),
            assistant.clone(),
            assistant,
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), contents).unwrap();

        let sessions = list_claude_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .expect("claude session should be listed");
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix stats usage")
        );
        assert_eq!(session["prompt_tokens"].as_u64(), Some(60));
        assert_eq!(session["completion_tokens"].as_u64(), Some(40));
        assert_eq!(session["cached_tokens"].as_u64(), Some(30));
        assert_eq!(session["cache_creation_tokens"].as_u64(), Some(20));
        assert_eq!(session["total_tokens"].as_u64(), Some(100));
        assert_eq!(session["turns"].as_u64(), Some(1));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.000714).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn list_claude_sessions_counts_usage_in_large_file_middle() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-Users-vm-projects-intendant");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_id = "019e37cf-large-middle-usage";
        let user = serde_json::json!({
            "timestamp": "2026-05-17T21:20:00Z",
            "type": "user",
            "cwd": "/Users/vm/projects/intendant",
            "message": {"content": "Fix stats usage"}
        });
        let assistant = serde_json::json!({
            "timestamp": "2026-05-17T21:20:02Z",
            "type": "assistant",
            "cwd": "/Users/vm/projects/intendant",
            "requestId": "req-middle",
            "message": {
                "id": "msg-middle",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 1000,
                    "cache_creation_input_tokens": 2000,
                    "cache_read_input_tokens": 3000,
                    "output_tokens": 4000
                }
            }
        });
        let filler = "x".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 64);
        let contents = format!("{}\n{}\n{}\n{}\n", user, filler, assistant, filler);
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), contents).unwrap();

        let sessions = list_claude_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .expect("claude session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(6000));
        assert_eq!(session["completion_tokens"].as_u64(), Some(4000));
        assert_eq!(session["cached_tokens"].as_u64(), Some(3000));
        assert_eq!(session["cache_creation_tokens"].as_u64(), Some(2000));
        assert_eq!(session["total_tokens"].as_u64(), Some(10000));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.0714).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    #[cfg(unix)]
    fn list_claude_sessions_deduplicates_symlinked_project_dirs() {
        let home = tempfile::tempdir().unwrap();
        let projects_dir = home.path().join(".claude").join("projects");
        let project_dir = projects_dir.join("-Users-vm-projects-intendant");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_id = "019e37cf-symlink-dedupe";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:20:00Z",
                "type": "user",
                "cwd": "/Users/vm/projects/intendant",
                "message": {"content": "Fix stats usage"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:20:02Z",
                "type": "assistant",
                "cwd": "/Users/vm/projects/intendant",
                "requestId": "req-usage",
                "message": {
                    "id": "msg-usage",
                    "model": "claude-sonnet-4-6",
                    "usage": {"input_tokens": 10, "output_tokens": 20}
                }
            }),
        ];
        std::fs::write(
            project_dir.join(format!("{session_id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            &project_dir,
            projects_dir.join("-Volumes-Untitled-projects-intendant"),
        )
        .unwrap();

        let sessions = list_claude_sessions(home.path());
        let matching = sessions
            .iter()
            .filter(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .count();
        assert_eq!(matching, 1);
    }

    #[test]
    fn list_gemini_sessions_parses_token_usage() {
        let home = tempfile::tempdir().unwrap();
        let chats_dir = home
            .path()
            .join(".gemini")
            .join("tmp")
            .join("sample-project")
            .join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let session_id = "session-2026-05-18T09-30-gemini";
        let session = serde_json::json!({
            "sessionId": session_id,
            "startTime": "2026-05-18T09:30:00Z",
            "messages": [
                {
                    "type": "user",
                    "timestamp": "2026-05-18T09:30:01Z",
                    "content": "Fix stats usage"
                },
                {
                    "type": "assistant",
                    "timestamp": "2026-05-18T09:30:02Z",
                    "model": "gemini-2.5-flash",
                    "tokens": {
                        "input": 1000,
                        "cached": 100,
                        "output": 20,
                        "thoughts": 30,
                        "tool": 5,
                        "total": 1055
                    },
                    "content": "Done"
                }
            ]
        });
        std::fs::write(
            chats_dir.join(format!("{session_id}.json")),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        let sessions = list_gemini_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .expect("gemini session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(1000));
        assert_eq!(session["completion_tokens"].as_u64(), Some(55));
        assert_eq!(session["cached_tokens"].as_u64(), Some(100));
        assert_eq!(session["total_tokens"].as_u64(), Some(1055));
        assert_eq!(session["turns"].as_u64(), Some(1));
        assert_eq!(session["model"].as_str(), Some("gemini-2.5-flash"));
        assert_eq!(session["pricing_known"].as_bool(), Some(true));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.0004105).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn sort_sessions_newest_first_uses_updated_at() {
        let mut sessions = vec![
            serde_json::json!({
                "session_id": "newer-created",
                "created_at": "2026-05-17T11:00:00Z",
                "updated_at": "2026-05-17T11:00:00Z",
            }),
            serde_json::json!({
                "session_id": "recently-changed",
                "created_at": "2026-05-17T08:00:00Z",
                "updated_at": "2026-05-17T12:00:00Z",
            }),
            serde_json::json!({
                "session_id": "fallback-created",
                "created_at": "2026-05-17T10:30:00Z",
            }),
        ];

        sort_sessions_newest_first(&mut sessions);
        let ids: Vec<_> = sessions
            .iter()
            .filter_map(|s| s.get("session_id").and_then(|v| v.as_str()))
            .collect();

        assert_eq!(
            ids,
            vec!["recently-changed", "newer-created", "fallback-created"]
        );
    }

    #[test]
    fn codex_detail_uses_session_meta_id_not_substring_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let target_id = "019e36b9-fffa-7b42-9070-e06db38b2abd";
        let other_id = "019e37ea-1ace-7091-ad2a-7805190330fa";

        std::fs::write(
            sessions_dir.join("a-other-session.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "timestamp": "2026-05-17T21:49:12.197Z",
                    "type": "session_meta",
                    "payload": { "id": other_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T21:49:16.518Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": format!("mentions {target_id} but is the wrong file")
                            }
                        ]
                    }
                })
            ),
        )
        .unwrap();

        let target_path = sessions_dir.join("z-target-session.jsonl");
        std::fs::write(
            &target_path,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "timestamp": "2026-05-17T18:16:59.898Z",
                    "type": "session_meta",
                    "payload": { "id": target_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T18:17:01.000Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": "Implement a new subtab for the dashboard in the Activity tab"
                            }
                        ]
                    }
                })
            ),
        )
        .unwrap();

        assert_eq!(
            find_codex_session_file(dir.path(), target_id).as_deref(),
            Some(target_path.as_path())
        );

        let detail = external_session_detail_from_home(dir.path(), "codex", target_id)
            .expect("target session should resolve");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail
            .get("entries")
            .and_then(|v| v.as_array())
            .expect("entries should be present");
        let contents: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry.get("content").and_then(|v| v.as_str()))
            .collect();

        assert!(
            contents
                .iter()
                .any(|content| content.contains("Implement a new subtab")),
            "target session content missing: {contents:?}"
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("wrong file")),
            "detail included content from a substring match: {contents:?}"
        );
        assert!(entries.iter().any(|entry| {
            entry.get("source").and_then(|v| v.as_str()) == Some("user")
                && entry
                    .get("content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|content| content.contains("Implement a new subtab"))
        }));
    }

    #[test]
    fn codex_transcript_filters_and_deduplicates_human_assistant_messages() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-transcript-filter";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:53Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": "internal developer context" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:54Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "# AGENTS.md instructions for /Users/vm/projects/intendant\n<INSTRUCTIONS>"
                        }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Visible prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "Visible prompt" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "event_msg",
                    "payload": { "type": "agent_message", "message": "Visible answer" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Visible answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:05Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"echo hidden\"}"
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let detail = external_session_detail_from_home(dir.path(), "codex", session_id)
            .expect("codex session should resolve");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail["entries"].as_array().unwrap();
        let contents: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents, vec!["Visible prompt", "Visible answer"]);
        assert_eq!(entries[0]["source"], "user");
        assert_eq!(entries[1]["source"], "codex");
    }

    #[test]
    fn external_transcript_cache_invalidates_when_source_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-cache-invalidation";
        let path = sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl"));
        let session_meta = serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        });
        let first_message = serde_json::json!({
            "timestamp": "2026-05-17T16:49:00Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "first cached message" }]
            }
        });
        let second_message = serde_json::json!({
            "timestamp": "2026-05-17T16:49:01Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "second uncached message" }]
            }
        });

        std::fs::write(&path, format!("{session_meta}\n{first_message}\n")).unwrap();
        let first = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("first load should resolve");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["content"], "first cached message");

        std::fs::write(
            &path,
            format!("{session_meta}\n{first_message}\n{second_message}\n"),
        )
        .unwrap();
        let second = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("second load should resolve");
        let contents: Vec<_> = second
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(
            contents,
            vec!["first cached message", "second uncached message"]
        );
    }

    #[test]
    fn external_activity_replay_uses_compact_session_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "What happens on refresh?" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "The task keeps running." }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        assert_eq!(replay["t"], "log_replay");
        assert_eq!(replay["replay_semantics"], EXTERNAL_TRANSCRIPT_SEMANTICS);

        let entries = replay["entries"].as_array().unwrap();
        assert_eq!(entries[0]["event"], "replay_start");
        assert_eq!(
            entries[0]["replay_semantics"],
            EXTERNAL_TRANSCRIPT_SEMANTICS
        );
        assert_eq!(entries[1]["event"], "session_attached");
        assert_eq!(entries[1]["session_id"], session_id);
        assert_eq!(entries[1]["source"], "codex");

        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["level"] == "info"
                && entry["source"] == "user"
                && entry["content"] == "What happens on refresh?"
                && entry["user_turn_index"] == 1
        }));
        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["level"] == "model"
                && entry["source"] == "codex"
                && entry["content"] == "The task keeps running."
        }));
    }

    #[test]
    fn external_activity_replay_marks_rolled_back_context() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-overwritten-activity";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Old prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Old answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "event_msg",
                    "payload": { "type": "thread_rolled_back", "num_turns": 1 }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:03Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "New prompt" }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        let old_prompt = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Old prompt")
            .expect("old prompt should remain visible");
        assert_eq!(old_prompt["user_turn_index"], 1);
        assert_eq!(old_prompt["superseded"], true);

        let old_answer = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Old answer")
            .expect("old answer should remain visible");
        assert_eq!(old_answer["superseded"], true);

        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["kind"] == "rollback_marker"
                && entry["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Rewound 1 user turn"))
        }));

        let new_prompt = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "New prompt")
            .expect("replacement prompt should replay");
        assert_eq!(new_prompt["user_turn_index"], 1);
        assert_eq!(new_prompt["replacement_for_user_turn_index"], 1);
        assert_ne!(new_prompt["superseded"], true);
    }

    #[test]
    fn resume_session_open_replays_full_external_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-full-activity-replay";
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        })];
        for n in 1..=90 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-05-17T16:{:02}:00Z", 49 + (n / 60)),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": if n % 2 == 0 { "assistant" } else { "user" },
                    "content": [{ "type": "text", "text": format!("turn message {n}") }]
                }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            lines
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            session_id,
            None,
            None,
            EXTERNAL_ACTIVITY_REPLAY_LIMIT,
        )
        .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let contents: Vec<_> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "log_entry")
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents.len(), 90);
        assert_eq!(contents.first(), Some(&"turn message 1"));
        assert_eq!(contents.last(), Some(&"turn message 90"));
        assert!(replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "log_entry")
            .all(|entry| entry["session_id"] == session_id));
    }

    #[test]
    fn external_activity_replay_limits_transcript_entries() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        })];
        for n in 1..=3 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-05-17T16:49:0{n}Z"),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("message {n}") }]
                }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            lines
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let replay = external_session_activity_replay_from_home(dir.path(), "codex", session_id, 2)
            .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let contents: Vec<_> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "log_entry")
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents, vec!["message 2", "message 3"]);
    }

    #[test]
    fn resume_session_open_replays_external_transcript_without_attach_marker() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Open this from Sessions" }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            session_id,
            None,
            None,
            80,
        )
        .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        assert_eq!(entries[0]["event"], "replay_start");
        assert!(
            entries
                .iter()
                .all(|entry| entry["event"] != "session_attached"),
            "Sessions-tab open replay should let the live attach event render the attach line"
        );
        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["content"] == "Open this from Sessions"
        }));
    }

    #[test]
    fn resume_session_open_does_not_replay_when_task_is_submitted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            "session-1",
            None,
            Some("continue the task"),
            80,
        )
        .is_none());
    }

    #[test]
    fn resume_session_open_replays_intendant_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("session-1");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.model_response("internal history", 0, 0, 0, 0, None);
        drop(log);

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "intendant",
            "session-1",
            None,
            None,
            80,
        )
        .expect("intendant session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();

        assert!(replay["entries"].as_array().unwrap().iter().any(|entry| {
            entry["event"] == "model_response" && entry["summary"] == "internal history"
        }));
    }

    #[test]
    fn external_attached_session_cache_ignores_internal_sessions() {
        assert_eq!(
            external_attached_session_from_wire(
                &serde_json::json!({
                    "event": "session_attached",
                    "session_id": "internal",
                    "source": "intendant"
                })
                .to_string()
            ),
            None
        );
        assert_eq!(
            external_attached_session_from_wire(
                &serde_json::json!({
                    "event": "session_attached",
                    "session_id": "external",
                    "source": "codex"
                })
                .to_string()
            ),
            Some(("external".to_string(), "codex".to_string()))
        );
    }

    #[test]
    fn settings_payload_accepts_settings_tab_save_without_agent_runtime_fields() {
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        assert_eq!(payload.external_agent.as_deref(), Some("codex"));
        assert_eq!(payload.codex_sandbox, "workspace-write");
        assert_eq!(payload.codex_approval_policy, "on-request");
        assert_eq!(payload.gemini_approval_mode, "default");

        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/opt/codex/bin/codex".to_string();
        config.agent.codex.sandbox = "danger-full-access".to_string();
        config.agent.gemini_cli.approval_mode = "yolo".to_string();
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.default_backend.as_deref(), Some("codex"));
        assert_eq!(config.agent.codex.command, "/opt/codex/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.gemini_cli.approval_mode, "yolo");
    }

    #[test]
    fn settings_payload_round_trips_codex_command() {
        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/usr/local/bin/codex".to_string();
        config.agent.claude_code.command = "/usr/local/bin/claude".to_string();
        config.agent.gemini_cli.command = "/usr/local/bin/gemini".to_string();

        let payload = settings_payload_from_config(&config);
        assert_eq!(
            payload.codex_command.as_deref(),
            Some("/usr/local/bin/codex")
        );
        assert_eq!(
            payload.claude_command.as_deref(),
            Some("/usr/local/bin/claude")
        );
        assert_eq!(
            payload.gemini_command.as_deref(),
            Some("/usr/local/bin/gemini")
        );

        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "  /opt/homebrew/bin/codex  ",
            "claude_command": "  /opt/claude/bin/claude  ",
            "gemini_command": "  /opt/gemini/bin/gemini  "
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.codex.command, "/opt/homebrew/bin/codex");
        assert_eq!(config.agent.claude_code.command, "/opt/claude/bin/claude");
        assert_eq!(config.agent.gemini_cli.command, "/opt/gemini/bin/gemini");
    }

    /// A specific bind address is preserved verbatim in the
    /// advertised URL. The operator chose it; we trust them.
    #[test]
    fn advertise_url_preserves_specific_bind_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[]),
            vec!["ws://127.0.0.1:8765/ws".to_string()]
        );
        let lan_ip = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(lan_ip), &[]),
            vec!["ws://192.168.1.42:8765/ws".to_string()]
        );
    }

    /// Wildcard bind (0.0.0.0) gets replaced with one URL per routable
    /// interface (auto-detection), never the literal wildcard. This
    /// is the guard against the production case where main.rs binds
    /// to 0.0.0.0:8765 and an earlier implementation was handing out
    /// `ws://0.0.0.0:8765/ws` in the Agent Card — an unusable URL
    /// that the transport-url-is-the-listener-addr assumption let
    /// slip through localhost-only tests.
    ///
    /// The exact set of interfaces is environment-dependent so we
    /// can't pin specific addresses; we only assert that no entry is
    /// the wildcard literal and the port is preserved everywhere.
    #[test]
    fn advertise_url_replaces_ipv4_wildcard_with_interface_urls() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[]);
        assert!(
            !urls.is_empty(),
            "auto-detect should produce at least one URL"
        );
        for url in &urls {
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.starts_with("ws://"), "scheme preserved: {url}");
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
            let host = url
                .strip_prefix("ws://")
                .and_then(|rest| rest.strip_suffix(":8765/ws"))
                .expect("url has expected prefix/suffix");
            assert!(
                !host.is_empty(),
                "host must resolve to something non-empty: {url}"
            );
        }
    }

    /// Same guard for IPv6 wildcards (::), which have the same
    /// unreachability problem as 0.0.0.0. Auto-detected v6 entries
    /// are bracketed per RFC 3986; we don't pin which interfaces are
    /// found because that's environment-dependent.
    #[test]
    fn advertise_url_replaces_ipv6_wildcard_with_interface_urls() {
        use std::net::{Ipv6Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[]);
        assert!(
            !urls.is_empty(),
            "wildcard v6 bind should still produce some auto-detected URLs"
        );
        for url in &urls {
            assert!(
                !url.contains("[::]"),
                "ipv6 wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// IPv6 specific addresses are bracketed in the URL per RFC 3986
    /// so a literal address like `::1` doesn't collide with the
    /// `:port` separator.
    #[test]
    fn advertise_url_brackets_specific_ipv6_address() {
        use std::net::{Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(specific), &[]);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].contains("[::1]"),
            "ipv6 literal must be bracketed: {}",
            urls[0]
        );
    }

    // -----------------------------------------------------------------
    // resolve_url_to_socket_addr (slice 3a.2 — URL hint parsing)
    // -----------------------------------------------------------------

    /// Directly-parseable `ipv4:port` authorities are returned
    /// without any DNS round-trip.
    #[tokio::test]
    async fn resolve_url_parses_ipv4_literal_url() {
        let addr = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws")
            .await
            .expect("parses");
        assert_eq!(addr.to_string(), "127.0.0.1:8766");
    }

    /// Bracketed IPv6 literals round-trip through the parser; the
    /// `/ws` path suffix is stripped before the SocketAddr parse.
    #[tokio::test]
    async fn resolve_url_parses_ipv6_literal_url() {
        let addr = resolve_url_to_socket_addr("wss://[::1]:8443/ws")
            .await
            .expect("parses");
        assert_eq!(addr.port(), 8443);
        assert!(addr.is_ipv6(), "expected IPv6, got {addr}");
    }

    /// `http://` and `https://` are accepted alongside the WebSocket
    /// schemes so the same URL form works whether the operator types
    /// the dashboard URL or the /ws URL.
    #[tokio::test]
    async fn resolve_url_accepts_http_and_https_schemes() {
        let a = resolve_url_to_socket_addr("http://127.0.0.1:8000/")
            .await
            .expect("http parses");
        assert_eq!(a.port(), 8000);
        let b = resolve_url_to_socket_addr("https://127.0.0.1:8443")
            .await
            .expect("https parses");
        assert_eq!(b.port(), 8443);
    }

    /// Hostnames route through `tokio::net::lookup_host`. `localhost`
    /// is the one name we can rely on across every test environment.
    #[tokio::test]
    async fn resolve_url_resolves_localhost_via_dns() {
        let addr = resolve_url_to_socket_addr("ws://localhost:8766/ws")
            .await
            .expect("resolves");
        assert_eq!(addr.port(), 8766);
        assert!(
            addr.ip().is_loopback(),
            "localhost must resolve to a loopback address: {addr}"
        );
    }

    /// URLs with a path + query string strip cleanly: the authority
    /// is everything up to the first `/` or `?`.
    #[tokio::test]
    async fn resolve_url_strips_path_and_query() {
        let a = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws/path?foo=bar")
            .await
            .expect("parses");
        assert_eq!(a.to_string(), "127.0.0.1:8766");
    }

    /// Unknown schemes, missing ports, and unresolvable hostnames
    /// all return `None` — caller falls back to UDP-only path.
    #[tokio::test]
    async fn resolve_url_returns_none_on_malformed_inputs() {
        // Unknown scheme
        assert!(resolve_url_to_socket_addr("foo://127.0.0.1:8766")
            .await
            .is_none());
        // Empty authority
        assert!(resolve_url_to_socket_addr("ws:///path").await.is_none());
        // No port (authority parses as IP but not SocketAddr; lookup_host
        // rejects a bare host with no port).
        assert!(resolve_url_to_socket_addr("ws://127.0.0.1/ws")
            .await
            .is_none());
    }

    /// Operator overrides come first in the merged list (preference
    /// order), but auto-detected entries are appended as fallbacks.
    /// The connecting peer's `MultiTransport::connect` walks the list
    /// top-down and uses the first that succeeds, so overrides win on
    /// preference while auto entries provide redundancy.
    #[test]
    fn advertise_overrides_prepend_to_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Specific bind so we can assert exactly one auto-detected entry
        // (wildcard bind would enumerate every host interface — non-
        // deterministic in CI). Specific-bind also covers the
        // intentionally-narrowed-listener case.
        let bind = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let overrides = vec![
            "ws://192.168.1.42:8765/ws".to_string(),
            "wss://laptop.tail-abcd.ts.net:8443/ws".to_string(),
        ];
        let urls = resolve_advertise_urls(Some(bind), &overrides);
        // Overrides come first, auto-detected entry appended.
        assert_eq!(urls.len(), 3, "got: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
        assert_eq!(urls[1], "wss://laptop.tail-abcd.ts.net:8443/ws");
        assert_eq!(urls[2], "ws://127.0.0.1:8765/ws");
    }

    /// An empty overrides list relies entirely on auto-detection.
    /// With a specific bind the result is exactly that one URL.
    #[test]
    fn empty_overrides_use_only_auto_detected_url() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan), &[]);
        assert_eq!(urls, vec!["ws://192.168.1.42:8765/ws".to_string()]);
    }

    /// Dedup: an operator URL that happens to match an auto-detected
    /// entry is kept exactly once (in operator position, since
    /// overrides are processed first). Avoids advertising the same
    /// URL twice when the operator types out their LAN IP that the
    /// daemon would have auto-detected anyway.
    #[test]
    fn advertise_dedupes_overrides_matching_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let overrides = vec!["ws://192.168.1.42:8765/ws".to_string()];
        let urls = resolve_advertise_urls(Some(lan), &overrides);
        assert_eq!(urls.len(), 1, "duplicate suppressed: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    /// A wildcard bind enumerates every routable non-loopback
    /// interface. We can't pin exact addresses (CI hosts vary) but
    /// can assert: (a) at least one URL is produced, (b) loopback is
    /// excluded (advertising loopback to remote peers is useless),
    /// (c) the port matches the bind port.
    #[test]
    fn advertise_wildcard_bind_enumerates_interfaces_excluding_loopback() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[]);
        assert!(
            !urls.is_empty(),
            "expected at least one auto-detected URL, got: {urls:?}"
        );
        for url in &urls {
            assert!(
                !url.contains("127.0.0.1"),
                "loopback must not appear in auto-detected federation URLs: {url}"
            );
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in auto-detected URLs: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// When operator wants to override completely (e.g. for security
    /// reasons — only advertise the Tailscale URL even though the
    /// daemon binds wildcard), they bind to a specific interface
    /// instead of wildcard. Specific bind narrows auto-detection to
    /// just that interface, so combined with operator override the
    /// effective list is `[override..., that_one_interface]`.
    #[test]
    fn specific_bind_narrows_auto_detection_to_one_interface() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan_only = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan_only), &[]);
        assert_eq!(urls.len(), 1, "specific bind = exactly one auto entry");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    #[test]
    fn test_app_html_embedded() {
        assert!(!APP_HTML.is_empty());
        assert!(APP_HTML.contains("<!DOCTYPE html>"));
        assert!(APP_HTML.contains("tab-activity"));
        assert!(APP_HTML.contains("tab-stats"));
        assert!(APP_HTML.contains("tab-terminal"));
        assert!(APP_HTML.contains("tab-displays"));
    }

    #[test]
    fn changes_request_decodes_nested_file_path() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/main.rs");
        let current_path = project.path().join("src/main.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "old\nsame\n").unwrap();
        std::fs::write(&current_path, "new\nsame\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/src%2Fmain.rs HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "src/main.rs");
        assert!(json["diff"].as_str().unwrap().contains("-old"));
        assert!(json["diff"].as_str().unwrap().contains("+new"));
    }

    #[test]
    fn changes_request_without_path_lists_files() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/main.rs");
        let current_path = project.path().join("src/main.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "old\n").unwrap();
        std::fs::write(&current_path, "new\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert!(
            json.as_array().is_some(),
            "list endpoint should return an array"
        );
        assert_eq!(json[0]["path"], "src/main.rs");
    }

    #[test]
    fn changes_request_lists_current_only_created_file() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();
        let current_path = project.path().join("src/new.rs");
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&current_path, "new\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["path"], "src/new.rs");
        assert_eq!(json[0]["kind"], "created");
        assert_eq!(json[0]["lines_added"], 1);
        assert_eq!(json[0]["diff_available"], true);
    }

    #[test]
    fn changes_request_lists_created_empty_file() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();
        std::fs::write(project.path().join("empty.txt"), "").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["path"], "empty.txt");
        assert_eq!(json[0]["kind"], "created");
        assert_eq!(json[0]["lines_added"], 0);
        assert_eq!(json[0]["lines_removed"], 0);
    }

    #[test]
    fn changes_request_empty_baseline_file_modified_is_not_created() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/empty.txt");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "").unwrap();
        std::fs::write(project.path().join("empty.txt"), "now has text\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json[0]["path"], "empty.txt");
        assert_eq!(json[0]["kind"], "modified");
        assert_eq!(json[0]["lines_added"], 1);
    }

    #[test]
    fn changes_request_created_then_deleted_net_zero_is_absent() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn changes_request_ignores_nested_worktrees() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();
        let worktree_file = project.path().join(".worktrees/feature/src/main.rs");
        std::fs::create_dir_all(worktree_file.parent().unwrap()).unwrap();
        std::fs::write(worktree_file, "fn main() {}\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn changes_request_reports_unsupported_current_for_text_baseline() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/main.rs");
        let current_path = project.path().join("src/main.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "fn main() {}\n").unwrap();
        std::fs::write(&current_path, b"fn\0main").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json[0]["path"], "src/main.rs");
        assert_eq!(json[0]["kind"], "modified");
        assert_eq!(json[0]["diff_available"], false);
        assert_eq!(json[0]["reason"], "binary file");

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/src/main.rs HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, "200 OK");
        assert_eq!(json["diff_available"], false);
        assert_eq!(json["diff"], "");
    }

    #[test]
    fn changes_request_decodes_segment_escaped_file_path() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/file name.rs");
        let current_path = project.path().join("src/file name.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "before\n").unwrap();
        std::fs::write(&current_path, "after\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/src/file%20name.rs HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "src/file name.rs");
        assert!(json["diff"].as_str().unwrap().contains("-before"));
        assert!(json["diff"].as_str().unwrap().contains("+after"));
    }

    #[test]
    fn changes_request_rejects_decoded_path_traversal() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/%2E%2E/Cargo.toml HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "400 Bad Request");
        assert_eq!(json["error"], "invalid path");
    }

    #[test]
    fn test_web_gateway_config_default() {
        let config = WebGatewayConfig::default();
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
        assert_eq!(config.output_sample_rate, 24000);
    }

    #[test]
    fn test_web_gateway_config_serialize() {
        let config = WebGatewayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"provider\":\"gemini\""));
        assert!(json.contains("\"input_sample_rate\":16000"));
    }

    #[test]
    fn test_build_config_gemini_model() {
        let config = build_config(
            None,
            Some("gemini-2.5-flash-native-audio-preview-12-2025"),
            false,
            crate::display::IceConfig::default(),
        );
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(
            None,
            Some("gpt-4o-realtime-preview"),
            false,
            crate::display::IceConfig::default(),
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(
            Some("openai"),
            None,
            false,
            crate::display::IceConfig::default(),
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(None, None, false, crate::display::IceConfig::default());
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    #[test]
    fn test_scan_replay_status_extracts_provider_model_autonomy() {
        let contents = concat!(
            r#"{"ts":"10:00:00","event":"session_start","level":"info"}"#,
            "\n",
            r#"{"ts":"10:00:01","event":"info","level":"info","message":"Provider: openai"}"#,
            "\n",
            r#"{"ts":"10:00:02","event":"info","level":"info","message":"Model: gpt-5"}"#,
            "\n",
            r#"{"ts":"10:00:03","event":"info","level":"info","message":"Autonomy: High"}"#,
            "\n",
        );
        let (p, m, a) = scan_replay_status(contents);
        assert_eq!(p.as_deref(), Some("openai"));
        assert_eq!(m.as_deref(), Some("gpt-5"));
        assert_eq!(a.as_deref(), Some("High"));
    }

    #[test]
    fn test_scan_replay_status_reads_debug_level_entries() {
        // Newer sessions write Provider/Model/Autonomy as `l.debug(...)`
        // so the event_type is "debug", not "info".  scan_replay_status
        // must pick those up too.
        let contents = concat!(
            r#"{"ts":"10:00:00","event":"debug","level":"debug","message":"Provider: anthropic"}"#,
            "\n",
            r#"{"ts":"10:00:01","event":"debug","level":"debug","message":"Model: claude-sonnet-4-6"}"#,
            "\n",
            r#"{"ts":"10:00:02","event":"debug","level":"debug","message":"Autonomy: Medium"}"#,
            "\n",
        );
        let (p, m, a) = scan_replay_status(contents);
        assert_eq!(p.as_deref(), Some("anthropic"));
        assert_eq!(m.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(a.as_deref(), Some("Medium"));
    }

    #[test]
    fn test_replay_jsonl_produces_replay_start_marker_first() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("Provider: openai");
        log.info("Model: gpt-5");
        log.info("Autonomy: Medium");
        log.turn_start(1, 0.0, 100_000);
        log.auto_approved("exec: ls");
        log.round_complete(1, 3);
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        // First entry is the replay_start marker.
        assert_eq!(
            entries[0].get("event").and_then(|v| v.as_str()),
            Some("replay_start")
        );
        assert_eq!(
            entries[0].get("provider").and_then(|v| v.as_str()),
            Some("openai")
        );

        // Each OutboundEvent entry has its historical `ts` injected.
        // Find the turn_started entry and verify it carries the original ts.
        let turn_started = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("turn_started"))
            .expect("turn_started should be present");
        assert!(
            turn_started.get("ts").is_some(),
            "ts should be injected into each outbound entry"
        );
        assert_eq!(
            turn_started.get("session_id").and_then(|v| v.as_str()),
            Some("session")
        );

        // auto_approved preview preserved.
        let auto_approved = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("auto_approved"))
            .expect("auto_approved should be present");
        assert_eq!(
            auto_approved.get("preview").and_then(|v| v.as_str()),
            Some("exec: ls")
        );
        assert_eq!(
            auto_approved.get("session_id").and_then(|v| v.as_str()),
            Some("session")
        );

        // round_complete fields propagated.
        let round = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("round_complete"))
            .expect("round_complete should be present");
        assert_eq!(round.get("round").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            round.get("turns_in_round").and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    #[test]
    fn test_session_log_replay_from_dir_reads_active_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("Provider: openai");
        log.model_response("still here after refresh", 0, 0, 0, 0, None);
        drop(log);

        let replay = session_log_replay_from_dir(&log_dir).expect("session log should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();

        assert_eq!(replay["t"], "log_replay");
        assert!(replay["entries"].as_array().unwrap().iter().any(|entry| {
            entry["event"] == "model_response" && entry["summary"] == "still here after refresh"
        }));
    }

    #[test]
    fn agent_output_chunks_falls_back_to_other_logs_by_output_id() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        let primary_dir = logs_dir.join("primary");
        let fallback_dir = logs_dir.join("fallback");

        let mut primary = crate::session_log::SessionLog::open(primary_dir.clone()).unwrap();
        primary.agent_output_with_id("primary output", "", Some("Codex"), Some("primary-out"));
        drop(primary);

        let mut fallback = crate::session_log::SessionLog::open(fallback_dir.clone()).unwrap();
        fallback.agent_output_with_id("fallback output", "", Some("Codex"), Some("fallback-out"));
        drop(fallback);

        let chunks = agent_output_chunks_with_fallback(
            &primary_dir,
            &["fallback-out".to_string(), "primary-out".to_string()],
            Some(&logs_dir),
        );

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].output_id, "fallback-out");
        assert_eq!(chunks[0].stdout, "fallback output");
        assert_eq!(chunks[1].output_id, "primary-out");
        assert_eq!(chunks[1].stdout, "primary output");
    }

    #[test]
    fn test_replay_jsonl_skips_internal_events() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.messages_input(r#"[{"role":"user","content":"hi"}]"#); // -> skip
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#); // -> skip
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        // Entries are: [replay_start, turn_started].  messages_input,
        // agent_input, and session_start all return None.
        assert_eq!(entries.len(), 2, "unexpected entries: {:#?}", entries);
        assert_eq!(
            entries[0].get("event").and_then(|v| v.as_str()),
            Some("replay_start")
        );
        assert_eq!(
            entries[1].get("event").and_then(|v| v.as_str()),
            Some("turn_started")
        );
    }

    #[test]
    fn test_replay_jsonl_includes_context_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.context_snapshot(
            "native",
            "Internal agent messages",
            Some(1),
            "intendant.conversation.messages.v1",
            None,
            Some(200_000),
            Some(1),
            &serde_json::json!([{"role": "user", "content": "hi"}]),
        );
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        let context = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("context_snapshot"))
            .expect("context_snapshot should replay");
        assert_eq!(
            context.get("format").and_then(|v| v.as_str()),
            Some("intendant.conversation.messages.v1")
        );
        assert_eq!(
            context.pointer("/raw/0/role").and_then(|v| v.as_str()),
            Some("user")
        );
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_websocket_echo() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        // Bind to port 0 for a random free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a Status control message
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Verify the EventBus receives the ControlCommand
        // (may be preceded by a PresenceLog debug event from the diagnostic logging)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");

            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status { .. })) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        handle.abort();
    }

    #[tokio::test]
    async fn test_broadcast_to_websocket() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx.clone(),
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
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
            external_agent: None,
        };
        crate::control::broadcast_event(&broadcast_tx, &event);

        // Verify the WebSocket client receives it
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
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

    // ---- /api/peers endpoint tests ----

    /// Spawn a test gateway with the given peer registry option and
    /// return (port, gateway handle). Condensed helper to keep the
    /// /api/peers tests below compact.
    async fn spawn_test_gateway_with_registry(
        peer_registry: Option<crate::peer::PeerRegistry>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Fire a raw HTTP request and read the response bytes.
    async fn http_request_bytes(port: u16, request: &str) -> Vec<u8> {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;
        response
    }

    /// Fire a raw HTTP request and read the response. Small helper
    /// because the /api/peers tests all make a handful of these.
    async fn http_request(port: u16, request: &str) -> String {
        String::from_utf8_lossy(&http_request_bytes(port, request).await).into_owned()
    }

    /// Same as `spawn_test_gateway_with_registry` but also wires an
    /// inbound bearer token. Used by the federation auth tests.
    async fn spawn_test_gateway_with_auth(
        peer_registry: Option<crate::peer::PeerRegistry>,
        bearer_token: Option<String>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            bearer_token,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    // -----------------------------------------------------------------
    // verify_bearer_token + is_federation_path unit tests
    // -----------------------------------------------------------------

    #[test]
    fn verify_bearer_token_passes_when_no_token_configured() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_token(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_missing_header_when_required() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("missing Authorization"));
    }

    #[test]
    fn verify_bearer_token_rejects_wrong_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("invalid bearer"));
    }

    #[test]
    fn verify_bearer_token_accepts_correct_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_header_name_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nauthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_scheme_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_non_bearer_scheme() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Basic Zm9vOmJhcg==\r\n\r\n";
        let err = verify_bearer_token(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("Bearer scheme"));
    }

    #[test]
    fn is_federation_path_recognizes_federation_endpoints() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers HTTP/1.1"));
        assert!(is_federation_path("DELETE /api/peers HTTP/1.1"));
        assert!(is_federation_path("GET /api/peers/eligible HTTP/1.1"));
        assert!(is_federation_path(
            "POST /api/peers/intendant:foo/message HTTP/1.1"
        ));
        assert!(is_federation_path("POST /api/coordinator/route HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions HTTP/1.1"));
    }

    #[test]
    fn is_federation_path_excludes_unauthenticated_endpoints() {
        // Discovery, dashboard bootstrap, and `/ws` must NOT be
        // mistaken for federation paths — they're intentionally
        // exempt from bearer enforcement.
        assert!(!is_federation_path(
            "GET /.well-known/agent-card.json HTTP/1.1"
        ));
        assert!(!is_federation_path("GET /config HTTP/1.1"));
        assert!(!is_federation_path("GET / HTTP/1.1"));
        assert!(!is_federation_path("GET /static/app.js HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /ws HTTP/1.1\r\nUpgrade: websocket"
        ));
        assert!(!is_federation_path("GET /api/settings HTTP/1.1"));
        assert!(!is_federation_path("POST /api/api-keys HTTP/1.1"));
    }

    // -----------------------------------------------------------------
    // End-to-end: federation REST auth enforcement
    // -----------------------------------------------------------------

    /// With `inbound_bearer_token` configured, a federation request
    /// without an Authorization header is rejected 401.
    #[tokio::test]
    async fn test_federation_endpoint_rejects_missing_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        // Request without auth — should 401, NOT pass through to the
        // 503-no-registry response that would happen otherwise.
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("missing Authorization"));
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate header signals the auth scheme"
        );
        handle.abort();
    }

    /// Wrong bearer token → 401 with "invalid bearer token".
    #[tokio::test]
    async fn test_federation_endpoint_rejects_wrong_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("invalid bearer"));
        handle.abort();
    }

    /// Correct bearer token → request flows through to the normal
    /// handler (which then returns 503 because no registry was
    /// configured — proves auth passed and dispatch ran).
    #[tokio::test]
    async fn test_federation_endpoint_accepts_correct_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-token\r\n\r\n",
        )
        .await;
        // Auth passed; handler returned its 503 (no registry).
        assert!(
            resp.contains("503"),
            "expected 503 (auth passed, registry missing), got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /config is exempt — even when bearer is required for
    /// federation endpoints, the dashboard bootstrap continues to work
    /// without auth. This is how the dashboard remains usable on the
    /// loopback / trusted-network case where the operator has set a
    /// bearer for WAN federation.
    #[tokio::test]
    async fn test_config_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(port, "GET /config HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("200 OK"),
            "config should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    #[tokio::test]
    async fn test_favicon_routes_serve_png() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;

        for path in ["/icon-128.png", "/favicon.ico"] {
            let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
            let resp = http_request_bytes(port, &request).await;
            let response_str = String::from_utf8_lossy(&resp);
            assert!(
                response_str.starts_with("HTTP/1.1 200 OK"),
                "expected 200 for {path}, got: {response_str}"
            );
            assert!(
                response_str.contains("Content-Type: image/png"),
                "expected PNG content type for {path}, got: {response_str}"
            );
            assert!(
                !response_str.contains("<!DOCTYPE html>"),
                "{path} should not fall through to app HTML"
            );

            let body_offset = resp
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|idx| idx + 4)
                .expect("HTTP response should contain a body separator");
            assert!(
                resp[body_offset..].starts_with(b"\x89PNG\r\n\x1a\n"),
                "{path} should serve PNG bytes"
            );
        }

        handle.abort();
    }

    // -----------------------------------------------------------------
    // /ws bearer enforcement (slice 2d)
    // -----------------------------------------------------------------

    #[test]
    fn extract_token_query_param_finds_token() {
        assert_eq!(
            extract_token_query_param("GET /ws?token=abc HTTP/1.1"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_query_param_finds_token_among_others() {
        assert_eq!(
            extract_token_query_param("GET /ws?other=x&token=abc&more=y HTTP/1.1"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_query_param_returns_none_when_absent() {
        assert_eq!(extract_token_query_param("GET /ws HTTP/1.1"), None);
        assert_eq!(extract_token_query_param("GET /ws?other=x HTTP/1.1"), None);
    }

    #[test]
    fn extract_token_query_param_handles_no_request_line() {
        assert_eq!(extract_token_query_param(""), None);
    }

    #[test]
    fn verify_bearer_for_ws_passes_when_no_token_configured() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n";
        assert!(verify_bearer_for_ws(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_authorization_header() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_token_query_param() {
        // The dashboard browser path: no Authorization header (browsers
        // can't easily set headers on WebSocket opens), token rides on
        // the URL.
        let header = "GET /ws?token=right HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_rejects_when_neither_present() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    #[test]
    fn verify_bearer_for_ws_rejects_wrong_query_token() {
        let header = "GET /ws?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    /// Header AND query both present — header wins (matches first).
    /// Mismatched header with matching query: header check fails, query
    /// check passes, overall accepted. Documents the fallback behavior.
    #[test]
    fn verify_bearer_for_ws_header_wrong_falls_back_to_query() {
        let header = "GET /ws?token=right HTTP/1.1\r\n\
                      Host: x\r\n\
                      Authorization: Bearer wrong\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    /// Real /ws upgrade through `spawn_test_gateway_with_auth`:
    /// connecting without a token gets a plain HTTP 401 *before* the
    /// WebSocket handshake completes — the dashboard sees a 401 page,
    /// not a successful upgrade then immediate close.
    #[tokio::test]
    async fn test_ws_upgrade_rejects_missing_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate signals scheme"
        );
        // Critically, the upgrade did NOT complete.
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must reject before WS handshake completes"
        );
        handle.abort();
    }

    /// /ws with a matching Authorization header completes the upgrade
    /// (101 Switching Protocols). This is the daemon-to-daemon path
    /// that IntendantWsTransport uses.
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_authorization_header() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Bearer ws-token\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /ws with `?token=` query parameter completes the upgrade. This
    /// is the dashboard-browser path (browsers can't set arbitrary
    /// headers on `WebSocket` opens, so the token rides on the URL).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_query_token() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws?token=ws-token HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /ws with no token still works when the gateway has no bearer
    /// configured (the common case for trusted-LAN deployments).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_when_no_bearer_configured() {
        let (port, handle) = spawn_test_gateway_with_auth(None, None).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /.well-known/agent-card.json is exempt — discovery must work
    /// before any auth handshake. Connecting peers fetch the card to
    /// see what auth they need to satisfy.
    #[tokio::test]
    async fn test_agent_card_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("200 OK"),
            "agent card should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// `GET /api/peers` returns 503 when the web gateway was spawned
    /// without a peer registry. This lets the dashboard distinguish
    /// "peers not configured" from "no peers yet" and render
    /// differently.
    #[tokio::test]
    async fn test_api_peers_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        assert!(resp.contains("peer registry not configured"));
        handle.abort();
    }

    /// `GET /api/peers` on a registry with no peers returns
    /// `{"peers":[]}`. Baseline for the list endpoint shape.
    #[tokio::test]
    async fn test_api_peers_list_empty_registry() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("200 OK"));
        // Split body from headers.
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(body.trim(), r#"{"peers":[]}"#);
        handle.abort();
    }

    /// End-to-end: spawn a "target" gateway (gateway A) and a
    /// "dashboard" gateway (gateway B) with a peer registry. POST
    /// A's card URL to B's /api/peers. Assert the peer is added,
    /// GET /api/peers shows it, DELETE removes it. This exercises
    /// the full path from HTTP request through PeerRegistry,
    /// IntendantWsTransport, the Agent Card fetch, WebSocket
    /// connect, and event drain.
    #[tokio::test]
    async fn test_api_peers_add_list_remove_end_to_end() {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) = spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (dash_port, dash_handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!("http://127.0.0.1:{target_port}/.well-known/agent-card.json");
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "add failed: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peer_id = parsed["peer_id"]
            .as_str()
            .expect("peer_id missing")
            .to_string();
        assert!(peer_id.starts_with("intendant:"));

        // GET /api/peers should now show the added peer.
        let list_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(list_resp.contains("200 OK"));
        let list_body = list_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let list: serde_json::Value = serde_json::from_str(list_body).unwrap();
        let peers_arr = list["peers"].as_array().unwrap();
        assert_eq!(peers_arr.len(), 1);
        assert_eq!(peers_arr[0]["id"].as_str().unwrap(), peer_id);
        // The "id" field should match the peer_id returned from POST.
        // The "version" should be the local build's version.
        assert_eq!(
            peers_arr[0]["version"].as_str().unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        // The dashboard panel rebuild relies on `ws_url` being
        // present so the browser can open a secondary WASM
        // connection without re-fetching the card. Guard against
        // the field being dropped or renamed.
        let ws_url = peers_arr[0]["ws_url"]
            .as_str()
            .expect("ws_url field must be present in the API response");
        assert!(
            ws_url.starts_with("ws://") && ws_url.ends_with("/ws"),
            "ws_url should be a native Intendant WebSocket URL: {ws_url}"
        );
        // The dashboard renders capability badges from this list,
        // so it must be present and contain the always-on phase 1
        // capabilities the test peer advertises.
        let caps = peers_arr[0]["capabilities"]
            .as_array()
            .expect("capabilities must be a JSON array");
        assert!(!caps.is_empty(), "expected at least one capability");

        // DELETE /api/peers with the peer_id.
        let del_body = serde_json::json!({"peer_id": peer_id}).to_string();
        let del_req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            del_body.len(),
            del_body
        );
        let del_resp = http_request(dash_port, &del_req).await;
        assert!(del_resp.contains("200 OK"), "delete failed: {del_resp}");

        // GET should now be empty.
        let empty_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let empty_body = empty_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(empty_body.trim(), r#"{"peers":[]}"#);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers` with an invalid body returns 400 with a
    /// diagnostic error message.
    #[tokio::test]
    async fn test_api_peers_post_invalid_body() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `DELETE /api/peers` for an unknown peer id returns 404.
    #[tokio::test]
    async fn test_api_peers_delete_unknown_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = r#"{"peer_id":"intendant:ghost"}"#;
        let req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Per-peer outbound op endpoints — `/api/peers/{id}/{op}`
    // -----------------------------------------------------------------

    /// Poll the registry until the peer transitions to
    /// `ConnectionState::Connected`, or `timeout` elapses. Returns
    /// whether the peer connected in time. Used by the routing tests
    /// below to avoid sending ops at a peer whose transport is still
    /// in handshake (which would bounce off as `NotConnected` → 502
    /// and obscure the actual code path under test).
    async fn wait_for_connected(
        registry: &crate::peer::PeerRegistry,
        peer_id: &crate::peer::PeerId,
        timeout: tokio::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Some(h) = registry.get(peer_id) {
                if h.is_connected() {
                    return true;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }
        false
    }

    /// Boilerplate: spawn target gateway A, register it as a peer on
    /// dashboard gateway B via HTTP, wait for the transport to connect,
    /// return everything the per-peer op tests need: the dashboard's
    /// port (where ops are POSTed) plus the peer id (the path
    /// parameter for every op endpoint) plus all four task handles to
    /// abort at end of test. Cuts ~30 lines of setup per test.
    async fn setup_peer_op_test() -> (
        u16,
        String,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) = spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let registry_for_wait = registry.clone();
        let (dash_port, dash_handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!("http://127.0.0.1:{target_port}/.well-known/agent-card.json");
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "register failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        let peer_id = parsed["peer_id"].as_str().unwrap().to_string();

        // Wait for the IntendantWsTransport to finish its handshake so
        // the op ack distinguishes "handler+routing works" from
        // "transport not ready yet".
        let pid = crate::peer::PeerId(peer_id.clone());
        assert!(
            wait_for_connected(
                &registry_for_wait,
                &pid,
                tokio::time::Duration::from_secs(3),
            )
            .await,
            "peer never reached Connected"
        );

        (dash_port, peer_id, target_handle, dash_handle)
    }

    /// `POST /api/peers/{id}/message` with a `{text}` shorthand body
    /// returns 200 + a `message_id`. Verifies the path-parameter
    /// routing, the JSON shorthand parsing, and the dispatch into
    /// `PeerHandle::send_message`. The wire-level encoding (this
    /// becomes a `ControlMsg::FollowUp` over the WebSocket) is covered
    /// by `peer::transport::intendant::tests::send_message_writes_followup_control_msg`.
    #[tokio::test]
    async fn test_api_peers_send_message_text_shorthand_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({"text": "hello peer"}).to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["message_id"].as_str().is_some(),
            "expected message_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/message` with a full `{role, content,
    /// session}` body works the same. Verifies the full-control shape
    /// path through `SendMessageRequest::into_message` (where `content`
    /// wins over `text` when both are present).
    #[tokio::test]
    async fn test_api_peers_send_message_full_shape_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/task` with `{instructions}` returns 200 +
    /// `task_id`. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::delegate_task_writes_start_task_control_msg`.
    #[tokio::test]
    async fn test_api_peers_delegate_task_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "instructions": "do the thing",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/task HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "delegate_task failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["task_id"].as_str().is_some(),
            "expected task_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/approval` with `{request_id, decision}`
    /// returns 200. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::resolve_approval_maps_each_decision_to_its_control_msg`.
    #[tokio::test]
    async fn test_api_peers_resolve_approval_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "request_id": "42",
            "decision": "accept",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/approval HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "resolve_approval failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{unknown}/message` returns 404 with a
    /// diagnostic body. Doesn't require setup — exercises only the
    /// peer lookup path before any transport interaction.
    #[tokio::test]
    async fn test_api_peers_op_unknown_peer_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"text": "hi"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:ghost/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("intendant:ghost"),
            "404 body should mention the missing id: {resp_body}"
        );
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with malformed JSON returns 400.
    #[tokio::test]
    async fn test_api_peers_send_message_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with neither `text` nor
    /// `content` returns 400. Verifies the `into_message` validation
    /// rejects empty bodies before the peer lookup runs.
    #[tokio::test]
    async fn test_api_peers_send_message_requires_text_or_content() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"session": "scratch"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("text") && resp_body.contains("content"),
            "error body should mention the missing fields: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown sub-op (e.g. `/api/peers/{id}/bogus`) returns 404 with
    /// a diagnostic body. Guards the dispatch arm that distinguishes
    /// "supported op" from "unrecognized verb".
    #[tokio::test]
    async fn test_api_peers_unknown_op_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "{}";
        let req = format!(
            "POST /api/peers/intendant:any/bogus HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("bogus"),
            "404 body should name the unknown op: {resp_body}"
        );
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Coordinator endpoints — capability discovery + delegation
    // -----------------------------------------------------------------

    /// `GET /api/peers/eligible` returns 503 with no registry,
    /// matching the rest of /api/peers.
    #[tokio::test]
    async fn test_api_peers_eligible_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        handle.abort();
    }

    /// Missing `?capability=...` query param returns 400 with a
    /// hint that at least one is required.
    #[tokio::test]
    async fn test_api_peers_eligible_requires_capability_param() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("capability"),
            "400 body should mention capability: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown capability strings return 400 with the offending
    /// values surfaced (not silently dropped, which would let an
    /// /api/peers/eligible?capability=typo through and return all
    /// peers).
    #[tokio::test]
    async fn test_api_peers_eligible_rejects_unknown_capability() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display&capability=nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("nope"),
            "400 body should name the unknown capability: {resp_body}"
        );
        handle.abort();
    }

    /// With one connected peer that advertises both ComputerUse and
    /// Knowledge (the test fixture's defaults), `?capability=computer-use`
    /// returns the peer; `?capability=display` returns an empty list
    /// (the fixture doesn't advertise Display).
    #[tokio::test]
    async fn test_api_peers_eligible_returns_matching_peers() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Hits: the test peer's card advertises ComputerUse.
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=computer-use HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peers = parsed["peers"].as_array().expect("peers array");
        assert_eq!(peers.len(), 1, "expected one matching peer");
        assert_eq!(peers[0]["id"].as_str().unwrap(), peer_id);

        // Misses: the fixture doesn't advertise Voice (build_local_agent_card
        // advertises ComputerUse + Knowledge + Display; Voice / Phone /
        // Recording are gated on runtime configuration that isn't plumbed
        // through yet).
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=voice HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["peers"].as_array().unwrap().len(), 0);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/coordinator/route` with required_capabilities the
    /// connected peer satisfies returns 200 + peer_id + task_id.
    /// Wire encoding to ControlMsg::StartTask is covered by
    /// peer::transport::intendant::tests.
    #[tokio::test]
    async fn test_api_coordinator_route_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "required_capabilities": ["computer-use"],
            "task": {
                "instructions": "do the thing",
                "context": {"file": "src/main.rs"},
            },
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(
            parsed["peer_id"].as_str().expect("peer_id present"),
            peer_id
        );
        assert!(
            parsed["task_id"].as_str().is_some(),
            "task_id should be present in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Routing a capability no connected peer satisfies returns 404
    /// with the considered peer ids surfaced for diagnostics.
    #[tokio::test]
    async fn test_api_coordinator_route_no_match_returns_404() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Voice is the "gated, not-advertised-by-default" capability
        // that the stock build_local_agent_card fixture doesn't claim
        // — so routing by it hits no-route and surfaces the considered
        // list. Display moved to always-on in the 3a.1 fix, so it can
        // no longer serve as the deliberately-unsatisfied capability.
        let body = serde_json::json!({
            "required_capabilities": ["voice"],
            "task": {"instructions": "needs voice"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), "no route");
        let considered = parsed["considered"].as_array().expect("considered array");
        assert!(
            considered.iter().any(|v| v.as_str() == Some(&peer_id)),
            "considered list should include the peer that didn't match"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Bad JSON body returns 400.
    #[tokio::test]
    async fn test_api_coordinator_route_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// Empty `required_capabilities` returns 400 — would otherwise
    /// match every peer and route to the first lexicographically,
    /// which is almost never what the caller meant.
    #[tokio::test]
    async fn test_api_coordinator_route_rejects_empty_capabilities() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({
            "required_capabilities": [],
            "task": {"instructions": "anything"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("required_capabilities"),
            "400 body should mention required_capabilities: {resp_body}"
        );
        handle.abort();
    }

    /// GET on the route endpoint returns 405 — only POST is allowed.
    #[tokio::test]
    async fn test_api_coordinator_route_get_returns_405() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("405"), "expected 405, got: {resp}");
        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_html() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
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
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
            ..Default::default()
        };
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
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

    /// `/config` is scoped to voice/runtime config only after the
    /// AgentCard split. Identity fields (host_label, version, git_sha)
    /// moved to /.well-known/agent-card.json. This test enforces the
    /// boundary so a future code change can't reintroduce drift
    /// between the two by sneaking identity fields back into
    /// WebGatewayConfig.
    #[tokio::test]
    async fn test_config_endpoint_has_no_identity_fields() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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

        // Extract the JSON body (after the header terminator).
        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
        let obj = parsed.as_object().expect("body is an object");

        assert!(
            obj.contains_key("provider"),
            "should still have runtime fields"
        );
        assert!(obj.contains_key("model"));
        assert!(
            !obj.contains_key("host_label"),
            "host_label must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("version"),
            "version must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("git_sha"),
            "git_sha must live on the agent card, not /config: {obj:?}"
        );

        handle.abort();
    }

    /// `/.well-known/agent-card.json` reflects live daemon state and
    /// deserializes into an [`crate::peer::AgentCard`] with the
    /// expected shape. This is the server-side guardrail the user
    /// asked for — if someone breaks the assembly in
    /// `build_local_agent_card`, the endpoint round-trip fails here
    /// before anyone hits it in the browser.
    #[tokio::test]
    async fn test_agent_card_endpoint_reflects_live_state() {
        use crate::peer::{AgentCard, AuthRequirements, Capability, TransportAuth, TransportSpec};

        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "agent card endpoint should return 200: {response_str}"
        );
        assert!(response_str.contains("application/json"));

        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let card: AgentCard = serde_json::from_str(body).expect("body deserializes as AgentCard");

        // Identity fields must be populated from live state.
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::PeerKind::Intendant),
            "local daemon must identify as Intendant kind: id = {:?}",
            card.id
        );
        assert!(
            card.id.as_str().starts_with("intendant:"),
            "PeerId must have intendant prefix: {}",
            card.id.as_str()
        );
        assert!(
            !card.label.is_empty(),
            "label must be resolved from lan::resolve_host_label"
        );
        assert_eq!(
            card.version,
            env!("CARGO_PKG_VERSION"),
            "version must come from CARGO_PKG_VERSION"
        );
        assert_eq!(
            card.git_sha.as_deref(),
            Some(env!("INTENDANT_GIT_SHA")),
            "git_sha must come from INTENDANT_GIT_SHA"
        );

        // Transports must advertise at least the native Intendant WS
        // transport, with a URL that points back at this listener.
        assert_eq!(card.transports.len(), 1, "expected one transport");
        let expected_url_prefix = format!("ws://127.0.0.1:{port}");
        match &card.transports[0] {
            TransportSpec::IntendantWs { url } => {
                assert!(
                    url.starts_with(&expected_url_prefix) && url.ends_with("/ws"),
                    "transport URL {url} should start with {expected_url_prefix} and end with /ws"
                );
            }
            other => panic!("expected IntendantWs transport, got {other:?}"),
        }

        // Phase 1 conservative capability set.
        assert!(
            card.capabilities.contains(&Capability::ComputerUse),
            "card should advertise ComputerUse capability: {:?}",
            card.capabilities
        );
        assert!(
            card.capabilities.contains(&Capability::Knowledge),
            "card should advertise Knowledge capability: {:?}",
            card.capabilities
        );

        // Auth defaults to None in phase 1 (trust the network layer).
        assert!(
            matches!(card.auth.transport, TransportAuth::None) && card.auth.application.is_none(),
            "expected AuthRequirements::none() in phase 1, got {:?}",
            card.auth
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_presence_connect_disconnect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect (new protocol)
        ws.send(Message::Text(
            r#"{"t":"presence_connect","server_session_id":"sess-1","last_event_seq":5}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        match event {
            AppEvent::PresenceConnected {
                server_session_id,
                last_event_seq,
                ..
            } => {
                assert_eq!(server_session_id.as_deref(), Some("sess-1"));
                assert_eq!(last_event_seq, 5);
            }
            _ => panic!("expected PresenceConnected, got {:?}", event),
        }

        // Send presence_disconnect
        ws.send(Message::Text(r#"{"t":"presence_disconnect"}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    #[tokio::test]
    async fn test_voice_log_forwarding() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        ws.send(Message::Text(
            r#"{"t":"voice_log","text":"hello","seq":3,"tool_context":"check_status"}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        match event {
            AppEvent::VoiceLog {
                text,
                seq,
                tool_context,
            } => {
                assert_eq!(text, "hello");
                assert_eq!(seq, 3);
                assert_eq!(tool_context.as_deref(), Some("check_status"));
            }
            _ => panic!("expected VoiceLog"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_check_status() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Create a query context with a known agent state
        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "thinking".to_string(),
            turn: 3,
            budget_pct: 0.15,
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx_split, mut ws_rx) = ws.split();

        // First message should be the bootstrap state_snapshot
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["state"]["phase"], "thinking");
            assert_eq!(json["state"]["turn"], 3);
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_state_snapshot_uses_daemon_session_without_active_session() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.daemon_session_id = Some("daemon-session".to_string());
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (_ws, mut ws_rx) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap()
            .0
            .split();

        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["session_id"], "daemon-session");
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_state_snapshot_prefers_daemon_over_active_session_log() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let active_log = Arc::new(Mutex::new(
            crate::session_log::SessionLog::open(dir.path().join("active-worker")).unwrap(),
        ));

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            {
                let mut state = ss.write().await;
                state.daemon_session_id = Some("daemon-session".to_string());
                state.session_log = Some(active_log);
            }
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (_ws, mut ws_rx) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap()
            .0
            .split();

        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["session_id"], "daemon-session");
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_response_roundtrip() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "running_agent".to_string(),
            turn: 5,
            budget_pct: 0.42,
            last_command_preview: "cargo test".to_string(),
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Drain the bootstrap message
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            futures_util::StreamExt::next(&mut ws),
        )
        .await;

        // Send a check_status tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_1","tool":"check_status","args":{}}"#.into(),
        ))
        .await
        .unwrap();

        // Read the tool_response
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "tool_response");
            assert_eq!(json["id"], "req_1");
            let result = json["result"].as_str().unwrap();
            assert!(result.contains("running_agent"), "result: {}", result);
            assert!(result.contains("Turn: 5"), "result: {}", result);
        } else {
            panic!("expected text message for tool_response");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_action_dispatches_control() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot::default()));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send an approve_action tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_2","tool":"approve_action","args":{"id":42}}"#.into(),
        ))
        .await
        .unwrap();

        // Should emit a ControlCommand(Approve) on the EventBus
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            if let AppEvent::ControlCommand(ControlMsg::Approve { id, .. }) = event {
                assert_eq!(id, 42);
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Approve)");

        // Should also get a tool_response back
        // Drain bootstrap first
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            futures_util::StreamExt::next(&mut ws),
        )
        .await;

        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "tool_response");
            assert_eq!(json["id"], "req_2");
            assert!(json["result"].as_str().unwrap().contains("Approved"));
        } else {
            panic!("expected text message");
        }

        handle.abort();
    }

    /// When a WebSocket client that sent `presence_connect` drops without
    /// sending `presence_disconnect`, the server should auto-emit
    /// `PresenceDisconnected` to resume server-side presence.
    #[tokio::test]
    async fn test_ws_drop_auto_sends_presence_disconnected() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the WebSocket WITHOUT sending presence_disconnect
        ws.close(None).await.unwrap();

        // Server should auto-send PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for auto PresenceDisconnected")
            .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    /// When a client that never sent `presence_connect` drops, no
    /// `PresenceDisconnected` should be emitted.
    #[tokio::test]
    async fn test_ws_drop_no_auto_disconnect_without_presence() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a control action (routes through EventBus regardless of active state)
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Drain events until we see the Status control event
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status { .. })) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        // Drop the WebSocket
        ws.close(None).await.unwrap();

        // Should NOT receive PresenceDisconnected — only a timeout
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(result.is_err(), "expected timeout, got {:?}", result);

        handle.abort();
    }

    /// POST /session returns 502 when no API key is configured.
    #[tokio::test]
    async fn test_post_session_no_api_key() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // POST /session without any API key env var set
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("502 Bad Gateway"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("not set on server"),
            "response: {}",
            response_str
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_audio_processor_js() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /audio-processor.js HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("application/javascript"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("AudioCaptureProcessor"),
            "response: {}",
            response_str
        );

        handle.abort();
    }

    /// First browser to send presence_connect should become active.
    #[tokio::test]
    async fn test_first_browser_becomes_active() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should get PresenceConnected on the bus (active browser emits it)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive a presence_welcome with is_active: true via direct channel
        // (We need to read WS messages to find it)
        let (_ws_tx_split, mut ws_rx) = ws.split();
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "presence_welcome");
            assert_eq!(json["is_active"], true);
        } else {
            panic!("expected text message");
        }

        handle.abort();
    }

    /// Second browser to send presence_connect should be passive (no PresenceConnected emitted).
    #[tokio::test]
    async fn test_second_browser_is_passive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects — becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Drain PresenceConnected from first browser
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Second browser connects — should be passive
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should NOT receive PresenceConnected on bus (passive)
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "passive browser should not emit PresenceConnected"
        );

        // Second browser should receive welcome with is_active: false
        // Drain bootstrap state_snapshot first
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(
                                json["is_active"], false,
                                "second browser should be passive"
                            );
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_welcome,
            "second browser should receive presence_welcome"
        );

        handle.abort();
    }

    /// When second browser sends make_active, the first should receive force_disconnect_voice.
    #[tokio::test]
    async fn test_make_active_handover() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // Browser 1 connects and becomes active
        let (ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws1_tx, mut ws1_rx) = ws1.split();
        ws1_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain ws1's bootstrap + welcome messages
        for _ in 0..3 {
            let _ =
                tokio::time::timeout(tokio::time::Duration::from_millis(300), ws1_rx.next()).await;
        }

        // Browser 2 connects (passive — no presence_connect yet, just make_active)
        let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws2_tx, mut ws2_rx) = ws2.split();

        // Drain ws2's bootstrap state_snapshot
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(300), ws2_rx.next()).await;

        // Browser 2 sends make_active
        ws2_tx
            .send(Message::Text(r#"{"t":"make_active"}"#.into()))
            .await
            .unwrap();

        // Browser 1 should receive force_disconnect_voice
        let mut found_force_disconnect = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws1_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "force_disconnect_voice" {
                            assert_eq!(json["reason"], "handover");
                            found_force_disconnect = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_force_disconnect,
            "browser 1 should receive force_disconnect_voice"
        );

        // Browser 2 should receive active_granted
        let mut found_active_granted = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "active_granted" {
                            assert_eq!(json["is_active"], true);
                            found_active_granted = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_active_granted,
            "browser 2 should receive active_granted"
        );

        // EventBus should have received a new PresenceConnected for browser 2
        let mut found_connected = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(AppEvent::PresenceConnected { .. })) => {
                    found_connected = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(found_connected, "make_active should emit PresenceConnected");

        handle.abort();
    }

    /// When the active browser drops, the next browser to connect should get active.
    #[tokio::test]
    async fn test_active_drop_clears_slot() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects and becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the active browser
        ws1.close(None).await.unwrap();

        // Should get PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceDisconnected));

        // Give server a moment to process the drop
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second browser connects — should now become active
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should get PresenceConnected (new active)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive welcome with is_active: true
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(json["is_active"], true);
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_welcome,
            "new browser should be active after old one dropped"
        );

        handle.abort();
    }

    /// An already-active browser re-sending presence_connect (e.g. after voice reconnect)
    /// should receive is_active: true and NOT emit a duplicate PresenceConnected.
    #[tokio::test]
    async fn test_active_browser_resend_presence_connect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws_tx, mut ws_rx) = ws.split();

        // First presence_connect — becomes active
        ws_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Drain PresenceConnected from first connect
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain welcome + bootstrap messages
        for _ in 0..5 {
            let _ =
                tokio::time::timeout(tokio::time::Duration::from_millis(200), ws_rx.next()).await;
        }

        // Re-send presence_connect (simulates voice reconnect after handover)
        ws_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Should receive welcome with is_active: true (still active)
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(
                                json["is_active"], true,
                                "already-active browser should still be active on re-connect"
                            );
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_welcome, "should receive presence_welcome");

        // Should NOT get a duplicate PresenceConnected on the bus
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "should not emit duplicate PresenceConnected for already-active browser"
        );

        handle.abort();
    }

    // ---------------------------------------------------------------
    // Phase 5a.1: input-authority closure semantics + emission tests
    // ---------------------------------------------------------------

    /// Build an empty `display_input_authority` map of the production
    /// shape, for the helper-shape tests below.
    fn empty_authority_map() -> Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>> {
        Arc::new(StdRwLock::new(HashMap::new()))
    }

    /// Insert a `DisplayInputHolder` directly into the map for tests
    /// that need to seed a holder without going through the full
    /// `apply_grant_input_authority` flow.  The inserted holder owns
    /// a fresh dummy `direct_tx` whose receiver is dropped — sends to
    /// it return `Err`, which the production code already tolerates
    /// (the WS-close path would have cleared this entry in real life).
    fn seed_holder(
        map: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
        display_id: u32,
        connection_id: &str,
    ) {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            display_id,
            DisplayInputHolder::LocalWs {
                connection_id: connection_id.to_string(),
                direct_tx: tx,
            },
        );
    }

    /// Closure semantics: unclaimed map → authorized.  Matches the
    /// pre-phase-5 backwards-compat default; without this, the gate
    /// would block input on a fresh display that no one has claimed
    /// yet (regression hazard).
    #[test]
    fn local_ws_authorizer_returns_true_when_unclaimed() {
        let map = empty_authority_map();
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), map);
        assert!(authz(), "unclaimed display should authorize any connection");
    }

    /// Closure semantics: holder asks → authorized.  The on-going
    /// holder's input keeps flowing without re-asking.
    #[test]
    fn local_ws_authorizer_returns_true_for_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), map);
        assert!(authz(), "holder must remain authorized");
    }

    /// Closure semantics: non-holder asks → denied.  This is the
    /// silent-drop case — the closure returns false; the gate in
    /// `display/mod.rs::gated_input_handler` then drops the event.
    #[test]
    fn local_ws_authorizer_returns_false_for_non_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let authz = build_local_ws_input_authorizer(0, "conn-B".to_string(), map);
        assert!(
            !authz(),
            "non-holder must be denied even though display is claimed"
        );
    }

    /// Closure re-evaluates on every call — the gate must observe
    /// live grant/release transitions for a long-lived `WebRtcPeer`.
    /// Captured-snapshot semantics would freeze the gate at the value
    /// at construction time, breaking the take-control flow mid-session.
    #[test]
    fn local_ws_authorizer_re_evaluates_on_each_call() {
        let map = empty_authority_map();
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), Arc::clone(&map));
        assert!(authz(), "starts unclaimed → authorized");
        seed_holder(&map, 0, "conn-B");
        assert!(!authz(), "after seeding conn-B as holder → denied");
        // Replace holder with self.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::LocalWs {
                connection_id: "conn-A".to_string(),
                direct_tx: mpsc::unbounded_channel().0,
            },
        );
        assert!(authz(), "after taking holder → re-authorized");
        // Release.
        map.write().unwrap_or_else(|e| e.into_inner()).remove(&0);
        assert!(authz(), "after release back to unclaimed → authorized");
    }

    /// `apply_grant_input_authority` emits a personalized authority
    /// change carrying `Some(holder)`.  The change flows through the
    /// broadcast channel; per-connection outbound tasks resolve the
    /// holder against their own id (via `matches_local_ws`) to produce
    /// `you|other|unclaimed` for browsers — the authoritative state
    /// the dashboard chip binds against.
    #[test]
    fn apply_grant_emits_authority_change_with_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (direct_tx, _direct_rx) = mpsc::unbounded_channel::<String>();
        let prior = apply_grant_input_authority(7, "conn-A".to_string(), direct_tx, &map, &auth_tx);
        assert!(prior.is_none(), "no prior holder on first grant");
        let change = auth_rx.try_recv().expect("authority change emitted");
        assert_eq!(change.display_id, 7);
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-A"))
                .unwrap_or(false),
            "broadcast holder must identify conn-A as the LocalWs holder"
        );
        // And the map records the new holder.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "registry entry must identify conn-A as LocalWs holder"
        );
    }

    /// A second grant from a different connection must auto-revoke
    /// the prior holder (matches Zoom's "granting auto-revokes prior"
    /// UX).  The prior holder receives a `display_input_authority_revoked`
    /// notification on its own direct_tx; the personalized change
    /// emits with the new holder's id.
    #[test]
    fn apply_grant_auto_revokes_prior_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (direct_tx_a, mut direct_rx_a) = mpsc::unbounded_channel::<String>();
        let (direct_tx_b, _direct_rx_b) = mpsc::unbounded_channel::<String>();

        // First grant to A.
        apply_grant_input_authority(7, "conn-A".to_string(), direct_tx_a.clone(), &map, &auth_tx);
        // Drain the first authority change.
        let _ = auth_rx.try_recv().expect("first grant emitted");

        // Second grant to B → A is auto-revoked.
        let prior =
            apply_grant_input_authority(7, "conn-B".to_string(), direct_tx_b, &map, &auth_tx);
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_local_ws("conn-A"),
            "prior holder must be conn-A"
        );

        // Authority change shows new holder.
        let change = auth_rx.try_recv().expect("second grant emitted");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-B"))
                .unwrap_or(false),
            "broadcast holder must identify conn-B"
        );

        // A receives a revoked notification on its direct_tx.
        let notify = direct_rx_a
            .try_recv()
            .expect("prior holder gets display_input_authority_revoked");
        assert!(notify.contains("display_input_authority_revoked"));
        assert!(notify.contains("\"display_id\":7"));
    }

    /// `apply_release_input_authority` emits a `None`-holder change
    /// only when the release actually took effect (caller is the
    /// current holder).  No-op release does not emit.
    #[test]
    fn apply_release_emits_authority_change_with_none() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed = apply_release_input_authority(7, "conn-A", &map, &auth_tx);
        assert!(removed, "holder's release should succeed");
        let change = auth_rx.try_recv().expect("authority change emitted");
        assert_eq!(change.display_id, 7);
        assert!(change.holder.is_none(), "release emits None holder");
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&7)
            .is_none());
    }

    /// Release attempted by a non-holder is a no-op — prevents A from
    /// unclaiming B's slot.  No authority change is emitted.
    #[test]
    fn apply_release_is_noop_for_non_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed = apply_release_input_authority(7, "conn-B", &map, &auth_tx);
        assert!(!removed, "non-holder cannot unclaim");
        // No change emitted.
        assert!(
            auth_rx.try_recv().is_err(),
            "no authority change for no-op release"
        );
        // Original holder still in map.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "original holder conn-A must still be in registry after no-op release"
        );
    }

    /// WS-close cleanup releases every entry held by the dropping
    /// connection and emits one `None`-holder change per affected
    /// display.  Without this fan-out, browsers in `other` state
    /// after the dropping connection had taken control would stay
    /// stuck on stale UI.
    #[test]
    fn apply_ws_close_emits_authority_change_with_none_for_each_held_display() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "conn-A");
        seed_holder(&map, 2, "conn-A");
        seed_holder(&map, 3, "conn-B");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_ws_close_input_authority("conn-A", &map, &auth_tx);
        // A's two holdings released; B untouched.
        let mut released_sorted = released.clone();
        released_sorted.sort();
        assert_eq!(released_sorted, vec![1, 2]);
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&1)
            .is_none());
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&2)
            .is_none());
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&3).unwrap().matches_local_ws("conn-B"),
            "other connections' holdings preserved",
        );
        drop(map_guard);
        // One change emitted per released display, both with None.
        let mut events: Vec<DisplayInputAuthorityChange> = Vec::new();
        while let Ok(change) = auth_rx.try_recv() {
            events.push(change);
        }
        assert_eq!(events.len(), 2);
        for change in &events {
            assert!(change.holder.is_none());
            assert!(change.display_id == 1 || change.display_id == 2);
        }
    }

    /// WS-close for a connection that holds no slots → no events,
    /// empty release list.  Common case (non-controller dropping out).
    #[test]
    fn apply_ws_close_is_noop_when_no_slots_held() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "conn-other");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_ws_close_input_authority("conn-A", &map, &auth_tx);
        assert!(released.is_empty(), "no slots held → no releases");
        assert!(auth_rx.try_recv().is_err(), "no authority changes emitted");
        // Other holder untouched.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&1).unwrap().matches_local_ws("conn-other"),
            "other holder untouched after no-op close",
        );
    }

    // ===================================================================
    // F-1.3b: federated authority registry helpers
    // ===================================================================

    /// Seed a `FederatedWebRtc` holder directly into the map for tests
    /// that need to set up cross-provenance scenarios. Mirrors
    /// `seed_holder` for `LocalWs`.
    fn seed_federated_holder(
        map: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
        display_id: u32,
        federation_connection_id: &str,
        session_id: &str,
    ) {
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            display_id,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: federation_connection_id.to_string(),
                session_id: session_id.to_string(),
            },
        );
    }

    /// `matches_federated`: same `(federation_connection_id, session_id)`
    /// pair matches; mismatched connection or mismatched session does
    /// not. Pins the F-1 identity rule that one federation tab can't
    /// pose as another (even from the same primary).
    #[test]
    fn matches_federated_identity_check() {
        let h = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-conn-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        assert!(h.matches_federated("fed-conn-1", "sess-A"));
        assert!(
            !h.matches_federated("fed-conn-1", "sess-B"),
            "same connection + different session must not match"
        );
        assert!(
            !h.matches_federated("fed-conn-2", "sess-A"),
            "different connection + same session must not match"
        );
        assert!(
            !h.matches_federated("fed-conn-2", "sess-B"),
            "fully-different identity must not match"
        );
    }

    /// `matches_federated` returns false for a `LocalWs` holder
    /// regardless of inputs. Cross-provenance equality is impossible.
    #[test]
    fn matches_federated_false_for_local_ws() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let h = DisplayInputHolder::LocalWs {
            connection_id: "conn-A".to_string(),
            direct_tx: tx,
        };
        assert!(!h.matches_federated("conn-A", "sess-A"));
    }

    /// `matches_local_ws` returns false for a `FederatedWebRtc`
    /// holder regardless of inputs. Symmetric with the test above.
    #[test]
    fn matches_local_ws_false_for_federated() {
        let h = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-conn-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        assert!(!h.matches_local_ws("fed-conn-1"));
    }

    /// `same_identity` distinguishes provenance even when string
    /// values collide. A `LocalWs { connection_id: "x" }` is NOT
    /// `same_identity` as `FederatedWebRtc { federation_connection_id:
    /// "x", session_id: "x" }` even though all the strings happen to
    /// match.
    #[test]
    fn same_identity_does_not_cross_provenance() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let local = DisplayInputHolder::LocalWs {
            connection_id: "x".to_string(),
            direct_tx: tx,
        };
        let federated = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "x".to_string(),
            session_id: "x".to_string(),
        };
        assert!(!local.same_identity(&federated));
        assert!(!federated.same_identity(&local));
    }

    /// `apply_grant_input_authority_federated` first call inserts a
    /// `FederatedWebRtc` holder, returns no prior, emits the change
    /// with the new holder.
    #[test]
    fn apply_grant_federated_first_grant_no_prior() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let prior = apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        assert!(prior.is_none(), "no prior on first grant");
        let change = auth_rx.try_recv().expect("change emitted");
        assert_eq!(change.display_id, 7);
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_federated("fed-conn-1", "sess-A"))
                .unwrap_or(false),
            "broadcast holder must be FederatedWebRtc(fed-conn-1, sess-A)"
        );
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&7)
                .unwrap()
                .matches_federated("fed-conn-1", "sess-A"),
            "registry must record the federated holder"
        );
    }

    /// **Cross-provenance handover**: a federated grant takes from a
    /// local holder. The local holder's `direct_tx` receives the
    /// `display_input_authority_revoked` notification (legacy local
    /// protocol); the broadcast change carries the new federated
    /// holder so other viewers personalize to "other".
    #[test]
    fn apply_grant_federated_takes_from_local_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (local_tx, mut local_rx) = mpsc::unbounded_channel::<String>();
        // Local holder.
        apply_grant_input_authority(7, "conn-LOCAL".to_string(), local_tx, &map, &auth_tx);
        let _ = auth_rx.try_recv().expect("local grant change");

        // Federated takes.
        let prior = apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_local_ws("conn-LOCAL"),
            "prior holder must be the local one"
        );

        // Local holder gets the legacy direct revoke.
        let revoke = local_rx
            .try_recv()
            .expect("local prior holder must receive direct revoke");
        assert!(revoke.contains("display_input_authority_revoked"));
        assert!(revoke.contains("\"display_id\":7"));

        // Broadcast carries the new federated holder.
        let change = auth_rx.try_recv().expect("broadcast change after handover");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_federated("fed-conn-1", "sess-A"))
                .unwrap_or(false),
            "broadcast holder after handover must be the federated one"
        );
    }

    /// **Cross-provenance handover (other direction)**: a local grant
    /// takes from a federated holder. The federated holder gets NO
    /// direct revoke (federated state always flows through the
    /// personalized broadcast — see `DisplayInputHolder` doc).
    #[test]
    fn apply_grant_local_takes_from_federated_holder_no_direct_revoke() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Federated holder.
        apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        let _ = auth_rx.try_recv().expect("federated grant change");

        // Local takes.
        let (local_tx, _local_rx) = mpsc::unbounded_channel::<String>();
        let prior =
            apply_grant_input_authority(7, "conn-LOCAL".to_string(), local_tx, &map, &auth_tx);
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_federated("fed-conn-1", "sess-A"),
            "prior holder must be the federated one"
        );

        // Federated holder is informed via the broadcast (handler
        // would compute "other" for this federated subscriber). The
        // direct-revoke path is not used for federated prior holders.
        let change = auth_rx.try_recv().expect("broadcast change after handover");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-LOCAL"))
                .unwrap_or(false),
            "broadcast holder after handover must be the local one"
        );
    }

    /// Federated release succeeds only when the calling
    /// `(federation_connection_id, session_id)` matches the current
    /// holder. A different session on the same federation connection
    /// cannot unclaim.
    #[test]
    fn apply_release_federated_only_on_matching_identity() {
        let map = empty_authority_map();
        seed_federated_holder(&map, 7, "fed-conn-1", "sess-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);

        // Wrong session — no-op.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-1", "sess-B", &map, &auth_tx);
        assert!(!removed, "wrong session must not unclaim");
        assert!(auth_rx.try_recv().is_err(), "no change for no-op release");

        // Wrong connection — no-op.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-2", "sess-A", &map, &auth_tx);
        assert!(!removed, "wrong connection must not unclaim");
        assert!(auth_rx.try_recv().is_err(), "no change for no-op release");

        // Original holder still in map.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&7)
                .unwrap()
                .matches_federated("fed-conn-1", "sess-A"),
            "original federated holder still in registry"
        );
        drop(map_guard);

        // Correct identity — releases.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-1", "sess-A", &map, &auth_tx);
        assert!(removed, "matching identity must release");
        let change = auth_rx.try_recv().expect("change emitted on release");
        assert!(change.holder.is_none(), "release emits None");
        assert!(
            map.read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&7)
                .is_none(),
            "registry empty after release"
        );
    }

    /// Federated release is also no-op against a `LocalWs` holder —
    /// federated session can't unclaim a local one even if the IDs
    /// happen to collide.
    #[test]
    fn apply_release_federated_noop_on_local_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed =
            apply_release_input_authority_federated(7, "conn-A", "sess-X", &map, &auth_tx);
        assert!(
            !removed,
            "federated release must not unclaim a LocalWs holder"
        );
        assert!(auth_rx.try_recv().is_err(), "no change emitted");
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "local holder still in registry"
        );
    }

    /// Federated WS-close releases ALL `FederatedWebRtc` entries with
    /// matching `federation_connection_id`, regardless of `session_id`
    /// (the WS drop kills every session multiplexed over that primary's
    /// federation transport). Other federation connections' entries
    /// AND any local entries are untouched.
    #[test]
    fn apply_federated_ws_close_releases_all_sessions_on_dropping_connection() {
        let map = empty_authority_map();
        seed_federated_holder(&map, 1, "fed-conn-1", "sess-A");
        seed_federated_holder(&map, 2, "fed-conn-1", "sess-B");
        seed_federated_holder(&map, 3, "fed-conn-2", "sess-C");
        seed_holder(&map, 4, "conn-LOCAL");

        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(16);
        let released = apply_federated_ws_close_input_authority("fed-conn-1", &map, &auth_tx);
        let mut released_sorted = released.clone();
        released_sorted.sort();
        assert_eq!(
            released_sorted,
            vec![1, 2],
            "both sessions on fed-conn-1 must be released"
        );

        // Other federation connection's entry untouched.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&3)
                .unwrap()
                .matches_federated("fed-conn-2", "sess-C"),
            "other federation connection's entry untouched"
        );
        // Local entry untouched.
        assert!(
            map_guard.get(&4).unwrap().matches_local_ws("conn-LOCAL"),
            "local holder untouched"
        );
        drop(map_guard);

        // One change emitted per affected display, all with None.
        let mut events = Vec::new();
        while let Ok(change) = auth_rx.try_recv() {
            events.push(change);
        }
        assert_eq!(events.len(), 2);
        for change in &events {
            assert!(change.holder.is_none());
            assert!(change.display_id == 1 || change.display_id == 2);
        }
    }

    /// Federated WS-close with no matching entries → empty list, no
    /// events. Local entries with the same `connection_id` value are
    /// not touched (the function is provenance-scoped).
    #[test]
    fn apply_federated_ws_close_is_noop_with_no_matching_entries() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "fed-conn-1");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_federated_ws_close_input_authority("fed-conn-1", &map, &auth_tx);
        assert!(
            released.is_empty(),
            "no FederatedWebRtc entries with this connection — no releases"
        );
        assert!(auth_rx.try_recv().is_err(), "no change emitted");
        // Local entry with the same connection_id (rare but possible
        // if a single connection_id value is reused across phases) is
        // untouched by the federated cleanup.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&1).unwrap().matches_local_ws("fed-conn-1"),
            "LocalWs entry with same id value untouched by federated cleanup"
        );
    }

    /// **F-1 invariant pin**: federated input authorizer is
    /// deny-by-default in F-1.3b. F-2 will replace this with a real
    /// registry-backed predicate; this test exists so any premature
    /// flip fires loudly.
    #[test]
    /// F-2: positive — an authority entry of `FederatedWebRtc` matching
    /// this closure's `(federation_connection_id, session_id)`
    /// authorizes input. Mirrors the local 5c
    /// `local_ws_authorizer_returns_true_for_holder` shape.
    #[test]
    fn federated_input_authorizer_returns_true_for_matching_holder() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-A".to_string(),
            },
        );
        let authz = build_federated_input_authorizer(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
        );
        assert!(authz(), "matching identity must authorize input");
    }

    /// F-2: negative — unclaimed (`None`) is strict deny on the
    /// federated path. Different from local 5c (which treats `None`
    /// as "anyone may input" for backwards compat); federated has no
    /// such legacy.
    #[test]
    fn federated_input_authorizer_returns_false_when_no_holder() {
        let map = empty_authority_map();
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "unclaimed display must drop federated input — different \
             from local 5c's pre-phase-5 default-allow"
        );
    }

    /// F-2: negative — a `LocalWs` holder denies federated input.
    /// Mixed cross-provenance hold: local browser drives input; the
    /// federated browser's events are dropped at the gate.
    #[test]
    fn federated_input_authorizer_returns_false_when_local_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "local-conn-A");
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "LocalWs holder must drop federated input even though the \
             registry is non-empty"
        );
    }

    /// F-2: negative — same `federation_connection_id`, different
    /// `session_id`. Two tabs from the same primary; only one holds.
    /// The non-holding tab's events drop.
    #[test]
    fn federated_input_authorizer_returns_false_when_wrong_session() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
        );
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "same connection + different session must deny — distinct \
             tabs from the same primary don't share input authority"
        );
    }

    /// F-2: negative — different `federation_connection_id` (different
    /// primary). The federated holder belongs to a different primary's
    /// transport; this primary's federated browser must not be able to
    /// drive input on behalf of the other primary's session.
    #[test]
    fn federated_input_authorizer_returns_false_when_wrong_connection() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-OTHER".to_string(),
                session_id: "sess-A".to_string(),
            },
        );
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "different federation_connection_id must deny even when \
             session_id matches — distinct primaries are distinct \
             security boundaries"
        );
    }

    // ---------------------------------------------------------------
    // F-1.3b3: federated authority handler + subscriber registry
    // ---------------------------------------------------------------

    /// Test helper: build a stub `WebRtcPeer` via the existing
    /// `new_for_test` constructor. Send-authority-state calls against
    /// the returned peer will fail (its command_rx is dropped) but
    /// the registry-level tests below only inspect the subscriber
    /// map, never await on delivery.
    fn make_test_peer(peer_id: u64) -> Arc<crate::display::webrtc::WebRtcPeer> {
        use crate::display::encode::pool::SimulcastRid;
        use crate::display::webrtc::WebRtcPeer;
        Arc::new(WebRtcPeer::new_for_test(
            peer_id,
            vec![SimulcastRid::full()],
        ))
    }

    /// Build an empty subscriber registry of the production shape.
    fn empty_subscribers() -> FederatedAuthoritySubscribers {
        Arc::new(StdRwLock::new(HashMap::new()))
    }

    /// `personalize_authority_for_federated` returns `You` when the
    /// holder's identity matches this subscriber's
    /// `(federation_connection_id, session_id)`. Mirrors the local
    /// 5c outbound personalization at the per-WS subscriber loop.
    #[test]
    fn personalize_authority_for_federated_returns_you_on_match() {
        let holder = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        let state = personalize_authority_for_federated(Some(&holder), "fed-1", "sess-A");
        assert_eq!(
            state,
            crate::display::webrtc::DisplayInputAuthorityState::You
        );
    }

    /// `personalize_authority_for_federated` returns `Other` when
    /// any holder exists that isn't this subscriber's identity. The
    /// "wrong session, same connection" case (two tabs from one
    /// primary) also resolves to `Other` — distinct session IDs
    /// don't collapse.
    #[test]
    fn personalize_authority_for_federated_returns_other_when_someone_else_holds() {
        let other_federated = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-1".to_string(),
            session_id: "sess-B".to_string(),
        };
        assert_eq!(
            personalize_authority_for_federated(Some(&other_federated), "fed-1", "sess-A"),
            crate::display::webrtc::DisplayInputAuthorityState::Other,
            "same connection, different session must be 'other'",
        );
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let local = DisplayInputHolder::LocalWs {
            connection_id: "local-conn".to_string(),
            direct_tx: tx,
        };
        assert_eq!(
            personalize_authority_for_federated(Some(&local), "fed-1", "sess-A"),
            crate::display::webrtc::DisplayInputAuthorityState::Other,
            "LocalWs holder must surface as 'other' to a federated subscriber",
        );
    }

    /// `personalize_authority_for_federated` returns `Unclaimed` when
    /// no holder is in the registry. Map absence is the canonical
    /// "no one holds" signal — no `Option` in the value type.
    #[test]
    fn personalize_authority_for_federated_returns_unclaimed_when_no_holder() {
        let state = personalize_authority_for_federated(None, "fed-1", "sess-A");
        assert_eq!(
            state,
            crate::display::webrtc::DisplayInputAuthorityState::Unclaimed
        );
    }

    /// The handler closure built by `build_federated_authority_handler`
    /// dispatches a `Request` to `apply_grant_input_authority_federated`,
    /// resulting in a holder bound to this peer's identity in the
    /// registry. Pins that the handler closure carries the right
    /// identity and that the dispatch shape is correct.
    #[test]
    fn build_federated_authority_handler_dispatches_request_to_grant() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
        );

        handler(AuthorityChannelMessage::Request { display_id: 0 });

        let guard = map.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&0) {
            Some(DisplayInputHolder::FederatedWebRtc {
                federation_connection_id,
                session_id,
            }) => {
                assert_eq!(federation_connection_id, "fed-1");
                assert_eq!(session_id, "sess-A");
            }
            other => panic!("expected FederatedWebRtc holder, got {other:?}"),
        }
    }

    /// `Release` against a holder of this same identity removes the
    /// entry from the registry. Pins the wire→registry round-trip.
    #[test]
    fn build_federated_authority_handler_dispatches_release_to_apply_release() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Seed a federated holder with the identity the handler was
        // built for.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-A".to_string(),
            },
        );

        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
        );
        handler(AuthorityChannelMessage::Release { display_id: 0 });

        assert!(
            map.read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&0)
                .is_none(),
            "release with matching identity must remove the holder"
        );
    }

    /// `Release` on a holder of a DIFFERENT identity is a silent
    /// no-op — the F-1.3b1 helper enforces identity matching at the
    /// registry layer, and the handler can't bypass it. Two tabs from
    /// the same primary can't unclaim each other.
    #[test]
    fn build_federated_authority_handler_release_noop_on_wrong_identity() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Seed with a holder of a DIFFERENT session id.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
        );

        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
        );
        handler(AuthorityChannelMessage::Release { display_id: 0 });

        let guard = map.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&0) {
            Some(DisplayInputHolder::FederatedWebRtc { session_id, .. }) => {
                assert_eq!(
                    session_id, "sess-OTHER",
                    "wrong-identity release must not remove the slot"
                );
            }
            other => panic!("expected slot to remain held by sess-OTHER, got {other:?}"),
        }
    }

    /// Display-ID mismatches drop silently. The federated peer's
    /// `PeerDisplayConnection` is bound to one display; a `Request`
    /// targeting any other display must not mutate the registry.
    #[test]
    fn build_federated_authority_handler_ignores_display_id_mismatch() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
        );

        handler(AuthorityChannelMessage::Request { display_id: 99 });
        handler(AuthorityChannelMessage::Release { display_id: 99 });

        assert!(
            map.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "display-id mismatch must not mutate the registry"
        );
    }

    /// `unregister_federated_authority_subscriber` removes the entry
    /// when the identity tuple matches and returns true. Cancellation
    /// of the spawned fanout task is a side effect of the cancel call
    /// on the stored token; not directly observable in this test, but
    /// the broadcast channel close on test exit reaps any orphaned
    /// task cleanly.
    #[tokio::test]
    async fn unregister_federated_authority_subscriber_removes_matching() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "register must insert one entry"
        );

        let removed = unregister_federated_authority_subscriber("fed-1", "sess-A", 0, &subscribers);

        assert!(removed, "matching unregister returns true");
        assert!(
            subscribers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "registry must be empty after unregister"
        );
    }

    /// `unregister_federated_authority_subscriber` returns false (and
    /// leaves the registry untouched) when no entry matches.
    #[tokio::test]
    async fn unregister_federated_authority_subscriber_noop_on_miss() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );

        let removed =
            unregister_federated_authority_subscriber("fed-1", "sess-OTHER", 0, &subscribers);
        assert!(!removed, "non-matching unregister returns false");
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "registry must be unchanged after non-matching unregister"
        );
    }

    /// Federation WS-close cleanup releases every subscriber whose
    /// `federation_connection_id` matches the dropping connection,
    /// regardless of `session_id` or `display_id`. Counterpart to
    /// `apply_federated_ws_close_input_authority`.
    #[tokio::test]
    async fn unregister_all_federated_subscribers_for_connection_releases_matching() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Three subscribers: two on fed-1 (different sessions, same
        // display), one on fed-2 (the survivor).
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-B".to_string(),
            0,
            make_test_peer(2),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-2".to_string(),
            "sess-C".to_string(),
            0,
            make_test_peer(3),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            3
        );

        let released = unregister_all_federated_subscribers_for_connection("fed-1", &subscribers);

        assert_eq!(released.len(), 2, "two fed-1 entries released");
        let remaining: Vec<(String, String, u32)> = subscribers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            remaining,
            vec![("fed-2".to_string(), "sess-C".to_string(), 0)],
            "only fed-2 entry must remain"
        );
    }

    /// `unregister_all_federated_subscribers_for_connection` returns
    /// an empty vec and leaves the registry untouched when no entries
    /// match the dropping connection.
    #[tokio::test]
    async fn unregister_all_federated_subscribers_for_connection_noop_on_no_match() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-2".to_string(),
            "sess-C".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );

        let released = unregister_all_federated_subscribers_for_connection("fed-1", &subscribers);
        assert!(
            released.is_empty(),
            "no matching entries → empty release list"
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "registry unchanged"
        );
    }

    /// `register_federated_authority_subscriber` replaces an existing
    /// entry with the same `(fcid, sid, did)` key (renegotiated peer
    /// for the same identity). Map size stays at 1; the prior entry's
    /// shutdown token fires via the in-helper cancel path.
    #[tokio::test]
    async fn register_federated_authority_subscriber_replaces_on_collision() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(2),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "duplicate-key registration must replace, not append"
        );
    }

    // ---------------------------------------------------------------
    // F-1.3b3 fix #2: WS-close peer teardown — peer_id helper
    // determinism + close-helper edge cases. The actual
    // session.remove_peer side effect requires a real
    // DisplaySession (which needs a real backend) and is exercised
    // by the F-3 smoke; these unit tests pin the contract that
    // must hold for the smoke to be meaningful: the same session_id
    // hashes to the same PeerId on both the Offer (insert) and the
    // WS-close (cleanup) sides.
    // ---------------------------------------------------------------

    /// Same `session_id` → same `PeerId` on every call. The Offer
    /// arm in `handle_federated_webrtc_signal` derives the
    /// `WebRtcPeer` key from this; WS-close cleanup must derive
    /// the same key to find the inserted peer. A divergence here
    /// would leak peers (cleanup would target a different key than
    /// the one Offer inserted), which is exactly the bug the helper
    /// extraction prevents.
    #[test]
    fn peer_id_for_federated_session_is_deterministic() {
        let a = peer_id_for_federated_session("sess-A");
        let b = peer_id_for_federated_session("sess-A");
        assert_eq!(a, b, "the same session id must hash to the same peer id");
    }

    /// Distinct `session_id`s map to distinct `PeerId`s in
    /// practice. (`u64` hash collisions are theoretically possible
    /// but vanishingly unlikely between any two real session ids
    /// generated by the browser.) Without this property, two
    /// federated tabs from one primary would alias to the same
    /// `WebRtcPeer` slot — cleanup of one tab would tear down the
    /// other.
    #[test]
    fn peer_id_for_federated_session_distinct_for_distinct_sessions() {
        let a = peer_id_for_federated_session("sess-A");
        let b = peer_id_for_federated_session("sess-B");
        assert_ne!(
            a, b,
            "distinct session ids should produce distinct peer ids"
        );
    }

    /// `close_federated_peers_for_sessions` short-circuits to 0 on
    /// empty release input — covers the "WS-close fired but the
    /// connection had no federated subscribers" no-op path
    /// (typical: the connection was a local browser, not a
    /// federation transport).
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_on_empty_release() {
        let reg = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let count = close_federated_peers_for_sessions(&[], Some(&reg)).await;
        assert_eq!(count, 0, "empty release must short-circuit");
    }

    /// `close_federated_peers_for_sessions` short-circuits on a
    /// `None` session_registry — the daemon may run without one
    /// (e.g. presence-disabled startup), and the WS-close path
    /// must not panic in that mode.
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_on_no_registry() {
        let count = close_federated_peers_for_sessions(&[("sess-A".to_string(), 0)], None).await;
        assert_eq!(count, 0, "missing registry must short-circuit");
    }

    /// `close_federated_peers_for_sessions` returns 0 (and runs no
    /// `remove_peer` calls) when the listed displays aren't in the
    /// registry — covers the race where a display session gets
    /// deactivated between Offer-time and WS-close.
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_when_displays_missing() {
        let reg = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let count = close_federated_peers_for_sessions(
            &[("sess-A".to_string(), 0), ("sess-B".to_string(), 1)],
            Some(&reg),
        )
        .await;
        assert_eq!(
            count, 0,
            "missing displays in the registry must fall through silently",
        );
    }

    // ---------------------------------------------------------------
    // Phase 5c.2: bootstrap snapshot regression — late-second browser
    // joining a daemon that already has an active display must end up
    // with its chip resolved to `you`/`other`/`unclaimed`, never stuck
    // at `unknown`.  The snapshot computation is the per-connection
    // personalization pass (the holder-id never reaches the wire).
    // ---------------------------------------------------------------

    /// Active display, no holder → `unclaimed` for the connecting browser.
    /// Covers the "fresh display granted before browser B connects, no one
    /// has clicked Take Control yet" case.
    #[test]
    fn bootstrap_authority_snapshots_unclaimed_when_no_holder() {
        let map = empty_authority_map();
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-B");
        assert_eq!(snaps, vec![(0, "unclaimed")]);
    }

    /// Active display, browser A holds → connecting browser B sees `other`.
    /// This is the exact regression that left B's chip at `unknown`
    /// before slice 5c.2 — the bootstrap was sent but landed on the
    /// wrong slot, so this test pins the snapshot resolution.
    #[test]
    fn bootstrap_authority_snapshots_other_for_late_second_browser() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-B");
        assert_eq!(
            snaps,
            vec![(0, "other")],
            "browser B (different connection_id) must see `other` while A holds",
        );
    }

    /// Active display, this connection IS the holder → `you`.
    /// Covers a holder browser refresh: same `connection_id` (or
    /// equivalent) reconnecting must see `you` so the chip stays
    /// consistent with the server-side gate.
    #[test]
    fn bootstrap_authority_snapshots_you_when_self_is_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-A");
        assert_eq!(snaps, vec![(0, "you")]);
    }

    /// Multiple active displays, mixed holders → per-display
    /// personalization is independent.  The connecting browser sees
    /// `you` for its own holdings and `other`/`unclaimed` for the rest.
    /// Locks in that the snapshot iterates per display, not per holder.
    #[test]
    fn bootstrap_authority_snapshots_resolve_per_display_independently() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A"); // you
        seed_holder(&map, 1, "conn-B"); // other
                                        // display 2 unclaimed
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32, 1, 2], &auth, "conn-A");
        assert_eq!(
            snaps,
            vec![(0, "you"), (1, "other"), (2, "unclaimed")],
            "each display's state resolves independently against this connection",
        );
    }

    /// Empty session registry → no snapshots, no frames to send.
    /// Matches the "browser connects to a daemon with no granted
    /// display" path; bootstrap loop is a no-op.
    #[test]
    fn bootstrap_authority_snapshots_empty_when_no_active_displays() {
        let map = empty_authority_map();
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([] as [u32; 0], &auth, "conn-A");
        assert!(snaps.is_empty());
    }
}
