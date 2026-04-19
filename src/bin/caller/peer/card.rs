//! Agent Card — the canonical identity and capability descriptor for a peer.
//!
//! Served at `/.well-known/agent-card.json` by every Intendant daemon, and
//! fetched from non-Intendant peers via the same path (A2A-style discovery).
//! The card is the single source of truth for: who this peer is, what it
//! can do, how to reach it, and how to authenticate against it. Replaces
//! the host_label/version/git_sha fields of [`crate::web_gateway::WebGatewayConfig`],
//! which now carries only voice runtime config.
//!
//! ## Forward-compat fallback variants
//!
//! Every wire-format enum in this module has an `Unknown` variant —
//! either marked `#[serde(other)]` for internally / adjacently tagged
//! enums, or handled through a custom `from_wire` helper for plain
//! unit enums that externally serialize to a bare string. Deserializing
//! a card that advertises a future transport, capability, or auth
//! scheme we don't recognize silently produces `Unknown` for that
//! position rather than failing the whole card parse — the registry
//! then filters `Unknown` out when picking a transport or auth method.
//!
//! Note that `#[serde(other)]` variants cannot be *serialized* (serde
//! rejects that at runtime). This is fine in practice because we never
//! round-trip cards that came from the wire: locally constructed cards
//! never contain `Unknown` variants, and wire-originated cards are
//! consumed, not re-emitted.

use crate::peer::id::PeerId;
use serde::{Deserialize, Serialize};

impl AgentCard {
    /// Construct an `AgentCard` for the local Intendant daemon from
    /// resolved runtime state. Centralizes the "this is me" assembly
    /// so `web_gateway::spawn_web_gateway` doesn't reinvent the
    /// shape of the card at its only call site.
    ///
    /// `label` should come from [`crate::lan::resolve_host_label`],
    /// `version` from `env!("CARGO_PKG_VERSION")`, and `git_sha` from
    /// `env!("INTENDANT_GIT_SHA")` (wrapped in `Some` — it's a
    /// build-time constant in Intendant). `transport_url` is the URL
    /// peers should connect to for the native Intendant WebSocket
    /// transport (e.g. `ws://127.0.0.1:8765/ws`). `capabilities` is
    /// the set of services this daemon actually exposes at runtime —
    /// compute it from feature flags and configured subsystems, not
    /// as a static maximum.
    ///
    /// Auth defaults to [`AuthScheme::None`] (trust-the-network) unless
    /// the caller opts into a stricter scheme; the LAN mTLS case is
    /// handled at the nginx proxy layer above the gateway and doesn't
    /// need a runtime flag here yet.
    pub fn local_intendant(
        label: String,
        version: String,
        git_sha: Option<String>,
        transport_url: String,
        capabilities: Vec<Capability>,
        auth: AuthScheme,
    ) -> Self {
        Self {
            id: PeerId::new(crate::peer::id::PeerKind::Intendant, &label),
            label,
            version,
            git_sha,
            transports: vec![TransportSpec::IntendantWs { url: transport_url }],
            capabilities,
            auth,
        }
    }
}

/// Identity + capability + transport descriptor for one peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Stable opaque ID. The peer identifies itself with this in every
    /// event and request. `id.kind()` is the source of truth for the
    /// peer's daemon kind — there is no separate `kind` field on the
    /// card, by design (one source of truth).
    pub id: PeerId,

    /// Human-readable display name. May change without affecting `id`.
    pub label: String,

    /// Cargo package version (or equivalent) of the daemon binary.
    pub version: String,

    /// Short git commit SHA the binary was built from, if known.
    /// `None` for non-Intendant peers that don't expose build metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,

    /// One or more transports this peer can be reached on. Listed in
    /// preference order — the registry picks the first one whose type
    /// is supported and reachable. A peer may offer several (e.g. an
    /// Intendant daemon will expose its native WebSocket *and* an MCP
    /// server *and*, once shipped, an A2A endpoint, all in one card).
    /// Unknown transports deserialize as [`TransportSpec::Unknown`]
    /// so older builds can still pick a known transport from the list.
    pub transports: Vec<TransportSpec>,

    /// What this peer can do. The federation coordinator routes work
    /// by matching required capabilities against this list.
    pub capabilities: Vec<Capability>,

    /// How to authenticate against this peer.
    pub auth: AuthScheme,
}

/// One way to reach a peer.
///
/// Serde renames: `A2A` and `OpenClawWs` have explicit `#[serde(rename)]`
/// attributes because the default kebab-case conversion mangles them
/// (`a2-a` and `open-claw-ws`) — the canonical spellings are `a2a`
/// and `openclaw-ws`, matching how their respective ecosystems name
/// themselves. The `transport_spec_canonical_wire_strings` test is
/// the invariant guard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TransportSpec {
    /// Native Intendant↔Intendant WebSocket. Carries the full `AppEvent`
    /// stream, mapped through the upcaster into `PeerEvent` variants.
    /// This is the highest-fidelity transport between Intendants.
    IntendantWs { url: String },

    /// Linux Foundation Agent2Agent — JSON-RPC over HTTPS + SSE.
    /// The standardizing bet for cross-daemon-kind federation.
    #[serde(rename = "a2a")]
    A2A { url: String },

    /// MCP server (any transport variant). Used for peers that expose
    /// themselves as MCP servers — Hermes Agent's `hermes mcp serve`,
    /// Intendant's own MCP server, etc. Lossy compared to native or A2A
    /// because MCP is structurally vertical (agent→tool) rather than
    /// peer-symmetric, but covers a lot of ground cheaply.
    Mcp {
        url: String,
        transport: McpTransportKind,
    },

    /// OpenClaw Gateway WebSocket. Intendant connects as an `operator`
    /// (drive sessions) and/or a `node` (lend capabilities back to the
    /// gateway). One peer entry corresponds to one role; a daemon that
    /// wants both registers two peers with the same underlying URL.
    #[serde(rename = "openclaw-ws")]
    OpenClawWs { url: String, role: OpenClawRole },

    /// Forward-compat fallback. Unknown transport types on the wire
    /// deserialize to `Unknown` so the card still parses; the registry
    /// filters these out when picking a transport. Cannot be
    /// serialized — do not construct this variant from local code.
    #[serde(other)]
    Unknown,
}

/// MCP wire transport variant — nested inside [`TransportSpec::Mcp`].
///
/// Uses custom Serialize/Deserialize impls so unknown values fall
/// through to [`McpTransportKind::Unknown`] rather than failing the
/// parent transport parse. (Internally tagged enums can use
/// `#[serde(other)]`, but this one is externally tagged as a bare
/// string — no `tag = "..."` attribute — so the custom impls are the
/// idiomatic way to get forward-compat fallback here.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpTransportKind {
    Stdio,
    Sse,
    StreamableHttp,
    /// Forward-compat fallback for MCP transport kinds we don't recognize.
    Unknown,
}

impl McpTransportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Sse => "sse",
            Self::StreamableHttp => "streamable-http",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "stdio" => Self::Stdio,
            "sse" => Self::Sse,
            "streamable-http" => Self::StreamableHttp,
            _ => Self::Unknown,
        }
    }
}

impl Serialize for McpTransportKind {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for McpTransportKind {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String>::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

/// Role Intendant takes when connecting to an OpenClaw Gateway.
///
/// Custom Serialize/Deserialize for the same reason as
/// [`McpTransportKind`] — bare-string external form, no `#[serde(tag)]`,
/// so we hand-roll the fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenClawRole {
    /// Control-plane client — drive OpenClaw sessions, send chat,
    /// invoke nodes, resolve approvals.
    Operator,
    /// Capability host — OpenClaw routes `node.invoke` calls to us so
    /// we can offer screen, voice, computer-use back to the gateway.
    Node,
    /// Forward-compat fallback for OpenClaw roles we don't recognize.
    Unknown,
}

impl OpenClawRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Node => "node",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "operator" => Self::Operator,
            "node" => Self::Node,
            _ => Self::Unknown,
        }
    }
}

impl Serialize for OpenClawRole {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OpenClawRole {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String>::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

/// Capabilities advertised by a peer. The coordinator routes work by
/// matching required capabilities against this list.
///
/// Note the two escape hatches: `Custom(String)` is for *named*
/// capabilities the peer wants to advertise explicitly (e.g. a
/// `"vortex-audio"` capability specific to Intendant's audio stack).
/// `Unknown` is for capability kinds the peer sends that we don't
/// recognize at all — forward-compat catch that keeps the card
/// parseable when a new known-name capability lands in a future version.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "kebab-case")]
pub enum Capability {
    /// Has a graphical display the peer can show / control / share.
    Display,
    /// Has a live voice / audio session.
    Voice,
    /// Can place phone calls (e.g. Intendant's phone-call skill).
    Phone,
    /// Has computer-use (screen + keyboard + mouse) on its own host.
    ComputerUse,
    /// Has a tagged knowledge store the peer can be queried against.
    Knowledge,
    /// Has display / session recording.
    Recording,
    /// Accepts task delegation from peers (implements `PeerDelegator`).
    TaskDelegation,
    /// Forwards messages to / from external channels (chat, sms, email,
    /// WhatsApp, Telegram). The OpenClaw category, basically.
    MessageRelay,
    /// Named custom capability — string-tagged for explicit extensions.
    Custom(String),
    /// Forward-compat fallback for capability kinds we don't recognize
    /// at all. Distinct from `Custom` — `Custom` is a peer explicitly
    /// saying "here's a named extension"; `Unknown` is us saying "this
    /// peer advertised something the parser doesn't know yet." Cannot
    /// be serialized.
    #[serde(other)]
    Unknown,
}

impl Capability {
    /// Parse a `Capability` from a URL-friendly string.
    ///
    /// Accepts the kebab-case kind names that match the JSON wire
    /// format (`"display"`, `"computer-use"`, `"task-delegation"`,
    /// etc.) plus a `"custom:<name>"` form for the `Custom` variant.
    /// Returns `None` for unrecognized values so callers can return
    /// a clean 400 instead of silently routing through `Unknown`
    /// (which would never match any real peer's advertised list).
    ///
    /// Used by the `/api/peers/eligible?capability=...` and
    /// `/api/coordinator/route` endpoints to keep the URL/JSON
    /// surface free of nested object syntax.
    pub fn from_query_string(s: &str) -> Option<Self> {
        if let Some(rest) = s.strip_prefix("custom:") {
            if rest.is_empty() {
                return None;
            }
            return Some(Self::Custom(rest.to_string()));
        }
        match s {
            "display" => Some(Self::Display),
            "voice" => Some(Self::Voice),
            "phone" => Some(Self::Phone),
            "computer-use" => Some(Self::ComputerUse),
            "knowledge" => Some(Self::Knowledge),
            "recording" => Some(Self::Recording),
            "task-delegation" => Some(Self::TaskDelegation),
            "message-relay" => Some(Self::MessageRelay),
            _ => None,
        }
    }
}

/// How a peer authenticates inbound connections.
///
/// Each transport understands the `AuthScheme`s relevant to it. The
/// federation coordinator does not need to interpret them — it forwards
/// the scheme + any local credentials to the transport at connect time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "kebab-case")]
pub enum AuthScheme {
    /// No authentication. Trust on the network layer (LAN, Tailscale,
    /// Unix socket). Default for phase-1 multi-host between Intendants
    /// on a trusted LAN.
    None,
    /// Static bearer token in `Authorization: Bearer <token>` header.
    /// `hint` is an optional human-readable credential reference like
    /// `"intendant.toml [peer.foo] token"` so the registry can locate
    /// the actual secret without leaking it into the card.
    Bearer { hint: Option<String> },
    /// Device keypair challenge/response, OpenClaw-style. The `nonce_url`
    /// is where the challenge is fetched; clients sign it with a per-device
    /// key registered via a pairing flow.
    DeviceKeypair { nonce_url: String },
    /// mTLS — the TLS layer authenticates the peer via a client cert
    /// signed by a CA both sides trust. Reuses the `intendant lan` CA
    /// infrastructure when both peers are Intendants on the same LAN.
    MutualTls,
    /// Forward-compat fallback for auth schemes we don't recognize.
    /// Connecting to a peer whose auth is `Unknown` fails at connect
    /// time with a clear error — the card still parses so other
    /// information in it remains usable. Cannot be serialized.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::id::PeerKind;

    #[test]
    fn card_serde_round_trip() {
        let card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "nicks-mac"),
            label: "Nick's Mac".to_string(),
            version: "0.42.0".to_string(),
            git_sha: Some("deadbeef".to_string()),
            transports: vec![
                TransportSpec::IntendantWs {
                    url: "wss://nicks-mac.local:8443/ws".to_string(),
                },
                TransportSpec::Mcp {
                    url: "https://nicks-mac.local:8443/mcp".to_string(),
                    transport: McpTransportKind::StreamableHttp,
                },
            ],
            capabilities: vec![
                Capability::Display,
                Capability::Voice,
                Capability::ComputerUse,
                Capability::Custom("vortex-audio".to_string()),
            ],
            auth: AuthScheme::MutualTls,
        };
        let json = serde_json::to_string(&card).unwrap();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, card);
    }

    #[test]
    fn capability_serde_kebab_case() {
        let json = serde_json::to_string(&Capability::ComputerUse).unwrap();
        assert!(json.contains("computer-use"), "got: {json}");
    }

    #[test]
    fn auth_scheme_serde_round_trip() {
        for scheme in [
            AuthScheme::None,
            AuthScheme::Bearer { hint: None },
            AuthScheme::Bearer {
                hint: Some("env:INTENDANT_PEER_TOKEN".to_string()),
            },
            AuthScheme::DeviceKeypair {
                nonce_url: "https://example.test/auth/nonce".to_string(),
            },
            AuthScheme::MutualTls,
        ] {
            let json = serde_json::to_string(&scheme).unwrap();
            let parsed: AuthScheme = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, scheme);
        }
    }

    /// Canonical wire spellings for `TransportSpec` variants. Same
    /// policy as `peer::id::tests::canonical_wire_strings` — any
    /// future rename that mangles an acronym will fire this test.
    #[test]
    fn transport_spec_canonical_wire_strings() {
        let cases: &[(TransportSpec, &str)] = &[
            (
                TransportSpec::IntendantWs {
                    url: "wss://x".into(),
                },
                "intendant-ws",
            ),
            (
                TransportSpec::A2A {
                    url: "https://x".into(),
                },
                "a2a",
            ),
            (
                TransportSpec::Mcp {
                    url: "https://x".into(),
                    transport: McpTransportKind::Stdio,
                },
                "mcp",
            ),
            (
                TransportSpec::OpenClawWs {
                    url: "wss://x".into(),
                    role: OpenClawRole::Operator,
                },
                "openclaw-ws",
            ),
        ];
        for (spec, expected_type) in cases {
            let json = serde_json::to_string(spec).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed["type"].as_str().unwrap(),
                *expected_type,
                "TransportSpec {spec:?}: expected type = {expected_type:?}"
            );
        }
    }

    /// Forward-compat: an agent card that advertises one transport we
    /// don't recognize alongside one we do must still parse, and the
    /// known transport must still be usable.
    #[test]
    fn card_with_unknown_transport_parses_and_preserves_known() {
        let json = r#"{
            "id": "intendant:nicks-mac",
            "label": "Nick's Mac",
            "version": "0.42.0",
            "transports": [
                { "type": "wasm-runtime", "url": "ws://nicks-mac:9000/wasm", "flavor": "wasi-preview2" },
                { "type": "intendant-ws", "url": "wss://nicks-mac:8443/ws" }
            ],
            "capabilities": [],
            "auth": { "scheme": "none" }
        }"#;
        let card: AgentCard = serde_json::from_str(json).unwrap();
        assert_eq!(card.transports.len(), 2);
        assert!(matches!(card.transports[0], TransportSpec::Unknown));
        assert!(matches!(
            card.transports[1],
            TransportSpec::IntendantWs { .. }
        ));
    }

    /// Forward-compat: an unknown auth scheme doesn't break the card
    /// parse — the caller discovers the problem at connect time, not
    /// at parse time, so other info from the card is still usable.
    #[test]
    fn card_with_unknown_auth_parses() {
        let json = r#"{
            "id": "intendant:future-peer",
            "label": "Future Peer",
            "version": "9.0.0",
            "transports": [{ "type": "intendant-ws", "url": "wss://x/ws" }],
            "capabilities": [],
            "auth": { "scheme": "webauthn-passkey", "rp_id": "future-peer.local" }
        }"#;
        let card: AgentCard = serde_json::from_str(json).unwrap();
        assert!(matches!(card.auth, AuthScheme::Unknown));
    }

    /// Forward-compat: an unknown capability kind doesn't break the
    /// card parse — it becomes `Capability::Unknown` and is filtered
    /// out when matching required capabilities.
    #[test]
    fn card_with_unknown_capability_parses() {
        let json = r#"{
            "id": "intendant:future-peer",
            "label": "Future",
            "version": "9.0.0",
            "transports": [{ "type": "intendant-ws", "url": "wss://x/ws" }],
            "capabilities": [
                { "kind": "display" },
                { "kind": "holographic-projection" },
                { "kind": "custom", "name": "vortex-audio" }
            ],
            "auth": { "scheme": "none" }
        }"#;
        let card: AgentCard = serde_json::from_str(json).unwrap();
        assert_eq!(card.capabilities.len(), 3);
        assert!(matches!(card.capabilities[0], Capability::Display));
        assert!(matches!(card.capabilities[1], Capability::Unknown));
        assert!(matches!(&card.capabilities[2], Capability::Custom(n) if n == "vortex-audio"));
    }

    /// Forward-compat: an unknown MCP transport inside a `Mcp` variant
    /// parses to `McpTransportKind::Unknown`.
    #[test]
    fn mcp_unknown_transport_kind_parses() {
        let json = r#"{ "type": "mcp", "url": "https://x/mcp", "transport": "websocket" }"#;
        let spec: TransportSpec = serde_json::from_str(json).unwrap();
        match spec {
            TransportSpec::Mcp { transport, .. } => {
                assert_eq!(transport, McpTransportKind::Unknown);
            }
            _ => panic!("expected Mcp variant"),
        }
    }

    /// Forward-compat: an unknown OpenClaw role falls through to Unknown.
    #[test]
    fn openclaw_unknown_role_parses() {
        let json = r#"{ "type": "openclaw-ws", "url": "ws://x:18789", "role": "supervisor" }"#;
        let spec: TransportSpec = serde_json::from_str(json).unwrap();
        match spec {
            TransportSpec::OpenClawWs { role, .. } => {
                assert_eq!(role, OpenClawRole::Unknown);
            }
            _ => panic!("expected OpenClawWs variant"),
        }
    }

    /// Wire-format consistency for the two unit enums with custom
    /// Serialize/Deserialize impls: their `as_str()` must match what
    /// serde produces. If this test fires, someone changed one source
    /// of truth without updating the other.
    #[test]
    fn unit_enums_as_str_matches_serde_wire_format() {
        for k in [
            McpTransportKind::Stdio,
            McpTransportKind::Sse,
            McpTransportKind::StreamableHttp,
            McpTransportKind::Unknown,
        ] {
            let wire = serde_json::to_string(&k).unwrap();
            assert_eq!(wire, format!("\"{}\"", k.as_str()));
        }
        for r in [
            OpenClawRole::Operator,
            OpenClawRole::Node,
            OpenClawRole::Unknown,
        ] {
            let wire = serde_json::to_string(&r).unwrap();
            assert_eq!(wire, format!("\"{}\"", r.as_str()));
        }
    }

    /// Capability::from_query_string round-trips kebab-case kinds and
    /// parses `"custom:<name>"` into the Custom variant. Unrecognized
    /// strings return None so the API surface can return a clean 400.
    #[test]
    fn capability_from_query_string_parses_kinds() {
        assert_eq!(Capability::from_query_string("display"), Some(Capability::Display));
        assert_eq!(Capability::from_query_string("voice"), Some(Capability::Voice));
        assert_eq!(Capability::from_query_string("phone"), Some(Capability::Phone));
        assert_eq!(
            Capability::from_query_string("computer-use"),
            Some(Capability::ComputerUse)
        );
        assert_eq!(Capability::from_query_string("knowledge"), Some(Capability::Knowledge));
        assert_eq!(Capability::from_query_string("recording"), Some(Capability::Recording));
        assert_eq!(
            Capability::from_query_string("task-delegation"),
            Some(Capability::TaskDelegation)
        );
        assert_eq!(
            Capability::from_query_string("message-relay"),
            Some(Capability::MessageRelay)
        );
    }

    #[test]
    fn capability_from_query_string_parses_custom() {
        assert_eq!(
            Capability::from_query_string("custom:vortex-audio"),
            Some(Capability::Custom("vortex-audio".to_string()))
        );
        // Empty custom name rejected — would otherwise let `?capability=custom:`
        // through and produce a Custom("") that matches nothing useful.
        assert_eq!(Capability::from_query_string("custom:"), None);
    }

    #[test]
    fn capability_from_query_string_rejects_unknown() {
        assert_eq!(Capability::from_query_string("unknown"), None);
        assert_eq!(Capability::from_query_string(""), None);
        // snake_case is not the wire format — only kebab-case is accepted.
        assert_eq!(Capability::from_query_string("computer_use"), None);
    }
}
