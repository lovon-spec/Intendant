//! Peer identity.
//!
//! A `PeerId` is a stable opaque token of the form `"<kind>:<label>"`, e.g.
//! `"intendant:nicks-mac"` or `"openclaw:home-server"`. The kind prefix lets
//! the registry de-collide peers across daemon types that might otherwise
//! share a label, and lets readers route by kind without needing the full
//! `AgentCard` in hand.
//!
//! ## Wire format discipline
//!
//! `PeerKind` uses **custom** `Serialize` / `Deserialize` impls rather
//! than `#[derive(..)]` + `#[serde(rename_all = "snake_case")]`. The
//! derive-based path mangles acronyms — `A2A` becomes `"a2_a"` and
//! `OpenClaw` becomes `"open_claw"`, neither of which matches the
//! canonical strings used in documentation and by every external
//! ecosystem tool. The custom impls delegate to [`PeerKind::as_str`]
//! for serialize and [`PeerKind::from_wire`] for deserialize, giving
//! exactly one source of truth for the wire vocabulary. The
//! `as_str_matches_serde_wire_format` test is the invariant guard.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable opaque identifier for a peer agent daemon.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

impl PeerId {
    pub fn new(kind: PeerKind, label: &str) -> Self {
        Self(format!("{}:{}", kind.as_str(), label))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse the kind prefix from the id.
    pub fn kind(&self) -> Option<PeerKind> {
        let prefix = self.0.split(':').next()?;
        PeerKind::from_str(prefix)
    }

    /// Return the label portion (everything after the first `:`).
    /// If the id has no colon at all, the whole id is the label.
    pub fn label(&self) -> &str {
        match self.0.split_once(':') {
            Some((_, rest)) => rest,
            None => &self.0,
        }
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of agent daemon a peer is.
///
/// Used by the registry to dispatch to the right transport: an `Intendant`
/// peer uses the native Intendant WebSocket transport; an `OpenClaw` peer
/// uses the operator+node Gateway protocol; etc.
///
/// `Other` is the forward-compat fallback: unknown kind strings on the
/// wire deserialize to `Other` rather than failing, so a daemon that
/// advertises `"voice_clone_agent"` in the future still parses cleanly
/// on older builds (which then can't route work to it, but at least
/// see it exists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeerKind {
    Intendant,
    OpenClaw,
    Hermes,
    Letta,
    /// Generic A2A-speaking peer (Linux Foundation Agent2Agent protocol).
    A2A,
    /// Generic MCP-server-shaped peer.
    Mcp,
    /// Unknown kind — forward-compat fallback.
    Other,
}

impl PeerKind {
    /// Canonical wire string. This is the single source of truth for
    /// how a `PeerKind` serializes — the custom `Serialize` impl
    /// delegates here and the `as_str_matches_serde_wire_format` test
    /// enforces the invariant.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intendant => "intendant",
            Self::OpenClaw => "openclaw",
            Self::Hermes => "hermes",
            Self::Letta => "letta",
            Self::A2A => "a2a",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }

    /// Strict parse — returns `Some` only for known kinds. Use when
    /// you want to distinguish "unrecognized" from "explicit Other"
    /// (which [`from_wire`](Self::from_wire) collapses).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "intendant" => Some(Self::Intendant),
            "openclaw" => Some(Self::OpenClaw),
            "hermes" => Some(Self::Hermes),
            "letta" => Some(Self::Letta),
            "a2a" => Some(Self::A2A),
            "mcp" => Some(Self::Mcp),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Parse with forward-compat fallback: unrecognized wire strings
    /// collapse to [`PeerKind::Other`] rather than returning `None`.
    /// This is what the custom `Deserialize` impl calls.
    pub fn from_wire(s: &str) -> Self {
        Self::from_str(s).unwrap_or(Self::Other)
    }
}

impl Serialize for PeerKind {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PeerKind {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String>::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trip() {
        let id = PeerId::new(PeerKind::Intendant, "nicks-mac");
        assert_eq!(id.as_str(), "intendant:nicks-mac");
        assert_eq!(id.kind(), Some(PeerKind::Intendant));
        assert_eq!(id.label(), "nicks-mac");
    }

    #[test]
    fn id_with_colon_in_label() {
        // Labels are allowed to contain colons; only the first colon is
        // the kind separator. Useful for `tcp:host:port`-style labels.
        let id = PeerId::new(PeerKind::OpenClaw, "tcp:host:8080");
        assert_eq!(id.kind(), Some(PeerKind::OpenClaw));
        assert_eq!(id.label(), "tcp:host:8080");
    }

    #[test]
    fn id_with_unknown_prefix() {
        let id = PeerId("zzz:foo".into());
        assert_eq!(id.kind(), None);
        assert_eq!(id.label(), "foo");
    }

    #[test]
    fn id_without_prefix() {
        let id = PeerId("just-a-label".into());
        assert_eq!(id.kind(), None);
        assert_eq!(id.label(), "just-a-label");
    }

    /// Every variant of `PeerKind` must serialize to exactly `as_str()`.
    /// This test is the forward-compat policy enforcement for this
    /// module — if anyone reintroduces `#[serde(rename_all = ..)]` on
    /// this enum, acronyms like A2A will get mangled and this test
    /// fires.
    #[test]
    fn as_str_matches_serde_wire_format() {
        for k in [
            PeerKind::Intendant,
            PeerKind::OpenClaw,
            PeerKind::Hermes,
            PeerKind::Letta,
            PeerKind::A2A,
            PeerKind::Mcp,
            PeerKind::Other,
        ] {
            let wire = serde_json::to_value(k).unwrap();
            let wire_str = wire.as_str().unwrap_or_else(|| {
                panic!("PeerKind::{k:?} did not serialize to a JSON string: {wire}")
            });
            assert_eq!(
                wire_str,
                k.as_str(),
                "PeerKind::{k:?}: as_str() = {:?}, serde wire = {:?}",
                k.as_str(),
                wire_str
            );
            // Round-trip back through deserialize.
            let parsed: PeerKind = serde_json::from_str(&wire.to_string()).unwrap();
            assert_eq!(parsed, k);
        }
    }

    /// Canonical spellings we promise to preserve. This is the
    /// user-facing contract the previous `#[serde(rename_all)]`
    /// silently violated — `A2A` was serializing as `"a2_a"`.
    #[test]
    fn canonical_wire_strings() {
        let cases: &[(PeerKind, &str)] = &[
            (PeerKind::Intendant, "intendant"),
            (PeerKind::OpenClaw, "openclaw"),
            (PeerKind::Hermes, "hermes"),
            (PeerKind::Letta, "letta"),
            (PeerKind::A2A, "a2a"),
            (PeerKind::Mcp, "mcp"),
            (PeerKind::Other, "other"),
        ];
        for (k, expected) in cases {
            let json = serde_json::to_string(k).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "PeerKind::{k:?}");
        }
    }

    /// Forward-compat: unknown kind strings deserialize to `Other`,
    /// not to an error.
    #[test]
    fn unknown_kind_falls_through_to_other() {
        let parsed: PeerKind = serde_json::from_str("\"voice_clone_agent\"").unwrap();
        assert_eq!(parsed, PeerKind::Other);
        let parsed: PeerKind = serde_json::from_str("\"some-future-kind\"").unwrap();
        assert_eq!(parsed, PeerKind::Other);
    }

    /// PeerId construction uses `as_str()`, so the ID prefix must also
    /// match the wire format. This catches any future divergence
    /// between the two.
    #[test]
    fn peer_id_prefix_matches_wire_format() {
        let id = PeerId::new(PeerKind::A2A, "foo");
        assert_eq!(id.as_str(), "a2a:foo");
        // And the same id round-trips through serde via the PeerKind
        // wire format.
        let kind_wire = serde_json::to_string(&PeerKind::A2A).unwrap();
        assert_eq!(kind_wire, "\"a2a\"");
    }
}
