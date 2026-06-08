use crate::autonomy::ApprovalConfig;
use crate::error::CallerError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct ModelConfig {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct OrchestratorConfig {
    pub max_parallel_agents: Option<usize>,
    pub sub_agent_dir: Option<String>,
}

/// Configuration for an external MCP server to connect to as a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// WebRTC configuration: ICE servers for STUN/TURN.
/// Configured via `[webrtc]` in intendant.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebRtcConfig {
    /// ICE servers (STUN/TURN) for WebRTC peer connections.
    /// Empty by default (local-only, no STUN/TURN).
    #[serde(default)]
    pub ice_servers: Vec<WebRtcIceServerConfig>,
    /// Whether the federated (peer-to-peer) display path may negotiate
    /// H.264. Default false ⇒ federation pins VP8 in the browser (the safe
    /// default for lossy TURN-relayed paths). Set true to let federation
    /// negotiate the peer's intra-refresh H.264 (libx264 / NVENC). Threaded
    /// into the `/config` payload alongside `ice_servers`; the local
    /// (same-machine) display path is unaffected.
    #[serde(default)]
    pub federation_allow_h264: bool,
}

/// A single ICE server entry in intendant.toml `[webrtc]` configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRtcIceServerConfig {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl WebRtcConfig {
    /// Convert to the display module's `IceConfig`.
    pub fn to_ice_config(&self) -> crate::display::IceConfig {
        crate::display::IceConfig {
            ice_servers: self
                .ice_servers
                .iter()
                .map(|s| crate::display::IceServer {
                    urls: s.urls.clone(),
                    username: s.username.clone(),
                    credential: s.credential.clone(),
                })
                .collect(),
        }
    }
}

/// Computer use configuration: provider/model overrides for tasks that involve
/// visual grounding (reference frames). Configured via `[computer_use]` in
/// intendant.toml or `CU_PROVIDER`/`CU_MODEL` env vars.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComputerUseConfig {
    /// Provider name (e.g. "anthropic", "gemini").
    #[serde(default)]
    pub provider: Option<String>,
    /// Model name (e.g. "claude-haiku-4-5-20251001", "gemini-2.5-flash").
    #[serde(default)]
    pub model: Option<String>,
    /// Display backend for input/screenshot. Default: "auto" (detect from env).
    /// Values: "x11", "wayland", "macos", "auto".
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_backend() -> String {
    "auto".to_string()
}

/// Configuration for external agent backends.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExternalAgentConfig {
    /// Default backend when --agent is not specified. None means use native agent.
    #[serde(default)]
    pub default_backend: Option<String>,
    /// Codex app-server settings.
    #[serde(default)]
    pub codex: CodexConfig,
    /// Claude Code settings.
    #[serde(default)]
    pub claude_code: ClaudeCodeConfig,
    /// Gemini CLI settings.
    #[serde(default)]
    pub gemini_cli: GeminiCliConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexConfig {
    /// Path or command name for the codex binary.
    #[serde(default = "default_codex_command")]
    pub command: String,
    /// Model to use (e.g. "o4-mini", "codex-mini-latest").
    #[serde(default)]
    pub model: Option<String>,
    /// Approval policy: "never", "on-request", "on-failure", "untrusted", "granular".
    #[serde(default = "default_codex_approval_policy")]
    pub approval_policy: String,
    /// Sandbox mode within Codex.
    #[serde(default = "default_codex_sandbox")]
    pub sandbox: String,
    /// Reasoning effort passed to Codex for reasoning-capable models.
    /// Codex's `-c model_reasoning_effort=...` — accepted values:
    /// `"minimal" | "low" | "medium" | "high" | "xhigh"`. Empty = Codex default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Optional Codex service-tier default for Intendant-managed Codex
    /// sessions. Empty / omitted inherits Codex's own config and account
    /// defaults. `"priority"` enables Fast, `"flex"` requests Flex, and
    /// `"standard"` sends an explicit `serviceTier: null` to opt out of Fast
    /// for managed sessions.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Whether to enable the Responses API `web_search` tool for this
    /// session. Maps to `codex --search` / `-c tool_suggest.web_search=true`.
    #[serde(default)]
    pub web_search: bool,
    /// Allow outbound network inside the `workspace-write` sandbox. Maps to
    /// `-c sandbox_workspace_write.network_access=true`. Ignored when
    /// sandbox is `read-only` or `danger-full-access`.
    #[serde(default)]
    pub network_access: bool,
    /// Extra writable roots beyond the project root. Each entry is passed
    /// as `--add-dir <PATH>` to Codex. Absolute paths only; relative paths
    /// are resolved against the project root at dispatch time.
    #[serde(default)]
    pub writable_roots: Vec<String>,
    /// Managed Codex context capability. `vanilla` is vanilla/fork-safe:
    /// Codex keeps its normal compaction behavior and Intendant does not
    /// advertise or enforce same-thread context rewinds. `managed` enables
    /// Intendant's managed Codex protocol: proactive rewinds/fissions,
    /// disabled auto-compaction, item-anchor rollback, developer-primer
    /// injection, and same-thread restore/backout. This currently requires
    /// the Intendant-aware Codex fork.
    #[serde(default = "default_codex_managed_context", alias = "context_recovery")]
    pub managed_context: String,
    /// Context snapshot archive mode for the Activity -> Context tab.
    /// `summary` records compact per-request visualization data and uses
    /// temporary provider traces while the session is live. `exact` persists
    /// full provider request payloads for exact raw replay. `off` disables
    /// context snapshot capture.
    #[serde(default = "default_codex_context_archive")]
    pub context_archive: String,
}

fn default_codex_command() -> String {
    "codex".to_string()
}

fn default_codex_approval_policy() -> String {
    "on-request".to_string()
}

fn default_codex_sandbox() -> String {
    "workspace-write".to_string()
}

fn default_codex_managed_context() -> String {
    "vanilla".to_string()
}

fn default_codex_context_archive() -> String {
    "summary".to_string()
}

/// Valid Codex sandbox modes, in the order we present them in the UI.
/// Matches `codex --sandbox <MODE>` exactly — the string flows through the
/// stack unchanged and is sent verbatim to `thread/start`.
pub const CODEX_SANDBOX_MODES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];

/// Valid Codex approval policies, in the order we present them.
/// Matches `codex --ask-for-approval <POLICY>`.
/// `"on-failure"` is deprecated upstream so we leave it out of the UI set.
pub const CODEX_APPROVAL_POLICIES: &[&str] = &["untrusted", "on-request", "never"];

/// Valid Codex reasoning-effort values, in the order we present them.
/// `""` is a sentinel for "use the model's default" so the UI can offer
/// "default" as a menu choice without introducing a separate Option<String>
/// juggling layer. All other values map straight to
/// `-c model_reasoning_effort=...`.
pub const CODEX_REASONING_EFFORTS: &[&str] = &["", "minimal", "low", "medium", "high", "xhigh"];
pub const CODEX_STANDARD_SERVICE_TIER: &str = "standard";

/// Normalize a user-supplied sandbox value to one of `CODEX_SANDBOX_MODES`.
/// Unknown or empty values fall back to the safest real policy
/// (`workspace-write`) so a config typo can't silently escalate privileges.
pub fn normalize_sandbox_mode(input: &str) -> String {
    let trimmed = input.trim();
    if CODEX_SANDBOX_MODES.iter().any(|m| *m == trimmed) {
        trimmed.to_string()
    } else {
        default_codex_sandbox()
    }
}

/// Normalize a user-supplied approval policy to one of
/// `CODEX_APPROVAL_POLICIES`. Unknown values fall back to `on-request`
/// (the project default) rather than silently disabling approvals.
pub fn normalize_approval_policy(input: &str) -> String {
    let trimmed = input.trim();
    if CODEX_APPROVAL_POLICIES.iter().any(|p| *p == trimmed) {
        trimmed.to_string()
    } else {
        default_codex_approval_policy()
    }
}

/// Normalize a user-supplied reasoning effort. Empty / unknown values map
/// to `None` (use the model default). Known values map to `Some(value)`
/// so the caller can drop the key entirely when Codex should pick.
pub fn normalize_reasoning_effort(input: Option<&str>) -> Option<String> {
    let s = input.map(|v| v.trim()).unwrap_or("");
    if s.is_empty() {
        return None;
    }
    if CODEX_REASONING_EFFORTS
        .iter()
        .any(|e| !e.is_empty() && *e == s)
    {
        Some(s.to_string())
    } else {
        None
    }
}

pub fn normalize_codex_service_tier(input: Option<&str>) -> Option<String> {
    let s = input.map(str::trim).unwrap_or("");
    if s.is_empty() {
        return None;
    }
    match s.to_ascii_lowercase().as_str() {
        "inherit" | "default" | "auto" | "codex" => None,
        "fast" | "priority" => Some("priority".to_string()),
        "standard" | "normal" | "none" | "off" | "clear" | "disabled" | "false" | "0" => {
            Some(CODEX_STANDARD_SERVICE_TIER.to_string())
        }
        "flex" => Some("flex".to_string()),
        _ => Some(s.to_string()),
    }
}

pub fn codex_service_tier_is_standard_clear(tier: &str) -> bool {
    normalize_codex_service_tier(Some(tier)).as_deref() == Some(CODEX_STANDARD_SERVICE_TIER)
}

pub fn normalize_codex_managed_context(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "managed" | "patched" | "intendant" | "on" | "true" | "enabled" => "managed".to_string(),
        _ => "vanilla".to_string(),
    }
}

pub fn codex_managed_context_enabled(mode: &str) -> bool {
    normalize_codex_managed_context(mode) == "managed"
}

pub fn normalize_codex_context_archive(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "exact" | "full" | "raw" | "on" | "true" | "1" => "exact".to_string(),
        "off" | "none" | "disabled" | "false" | "0" => "off".to_string(),
        _ => "summary".to_string(),
    }
}

pub fn codex_context_archive_exact(mode: &str) -> bool {
    normalize_codex_context_archive(mode) == "exact"
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            command: default_codex_command(),
            model: None,
            approval_policy: default_codex_approval_policy(),
            sandbox: default_codex_sandbox(),
            reasoning_effort: None,
            service_tier: None,
            web_search: false,
            network_access: false,
            writable_roots: Vec::new(),
            managed_context: default_codex_managed_context(),
            context_archive: default_codex_context_archive(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCodeConfig {
    /// Path or command name for the claude binary.
    #[serde(default = "default_claude_code_command")]
    pub command: String,
    /// Model to use.
    #[serde(default)]
    pub model: Option<String>,
    /// Permission mode: "default", "acceptEdits", "plan", "auto", "bypassPermissions".
    #[serde(default = "default_claude_code_permission_mode")]
    pub permission_mode: String,
    /// Allowed tools list (empty = all).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

fn default_claude_code_command() -> String {
    "claude".to_string()
}

fn default_claude_code_permission_mode() -> String {
    "auto".to_string()
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            command: default_claude_code_command(),
            model: None,
            permission_mode: default_claude_code_permission_mode(),
            allowed_tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCliConfig {
    /// Command to run Gemini CLI. Default: "gemini".
    #[serde(default = "default_gemini_cli_command")]
    pub command: String,
    /// Model to use (e.g. "gemini-2.5-pro", "gemini-2.5-flash").
    #[serde(default)]
    pub model: Option<String>,
    /// Approval mode matching `gemini --approval-mode`: "default" (prompt
    /// for every tool), "auto_edit" (auto-approve edits, prompt for exec),
    /// "yolo" (auto-approve everything), "plan" (read-only, no writes).
    #[serde(default = "default_gemini_approval_mode")]
    pub approval_mode: String,
    /// Whether to pass `--sandbox` when spawning Gemini.
    #[serde(default)]
    pub sandbox: bool,
    /// List of extension names to enable (passed as `--extensions`). Empty
    /// means "use all installed extensions" — Gemini's default.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Allowlist of MCP server names (from Gemini's config) this session
    /// may use. Passed as `--allowed-mcp-server-names`. Empty means "all".
    /// Note: Intendant always merges its own `intendant` entry into
    /// `$HOME/.gemini/settings.json`; if you set an allowlist here, remember
    /// to include `intendant` or the Intendant MCP tools won't be reachable.
    #[serde(default)]
    pub allowed_mcp_servers: Vec<String>,
    /// Extra directories that Gemini should treat as workspace roots.
    /// Passed as `--include-directories`. Absolute paths only.
    #[serde(default)]
    pub include_directories: Vec<String>,
    /// Open Gemini's DevTools console alongside the session. Maps to
    /// `--debug`. Mostly useful for tracking down Gemini CLI bugs.
    #[serde(default)]
    pub debug: bool,
}

fn default_gemini_cli_command() -> String {
    "gemini".to_string()
}

fn default_gemini_approval_mode() -> String {
    "default".to_string()
}

/// Valid Gemini approval modes, in UI order. Matches
/// `gemini --approval-mode <MODE>` exactly; the string flows through the
/// stack unchanged and is passed verbatim as a CLI arg.
pub const GEMINI_APPROVAL_MODES: &[&str] = &["default", "auto_edit", "yolo", "plan"];

/// Normalize a user-supplied Gemini approval mode. Unknown or empty values
/// fall back to `"default"` so a config typo can't silently escalate to
/// `yolo`.
pub fn normalize_gemini_approval_mode(input: &str) -> String {
    let trimmed = input.trim();
    if GEMINI_APPROVAL_MODES.iter().any(|m| *m == trimmed) {
        trimmed.to_string()
    } else {
        default_gemini_approval_mode()
    }
}

impl Default for GeminiCliConfig {
    fn default() -> Self {
        Self {
            command: default_gemini_cli_command(),
            model: None,
            approval_mode: default_gemini_approval_mode(),
            sandbox: false,
            extensions: Vec::new(),
            allowed_mcp_servers: Vec::new(),
            include_directories: Vec::new(),
            debug: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub model: ModelConfig,
    #[serde(default)]
    pub orchestrator: OrchestratorConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    #[allow(dead_code)]
    pub sandbox: SandboxProjectConfig,
    #[serde(default)]
    pub presence: crate::presence::PresenceConfig,
    #[serde(default)]
    pub transcription: crate::transcription::TranscriptionConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub computer_use: ComputerUseConfig,
    #[serde(default)]
    pub agent: ExternalAgentConfig,
    #[serde(default)]
    pub live_audio: LiveAudioConfig,
    #[serde(default)]
    pub webrtc: WebRtcConfig,
    /// `[server]` section in intendant.toml — daemon-level settings
    /// for what this Intendant advertises to peers. See [`ServerConfig`].
    #[serde(default)]
    pub server: ServerConfig,
    /// Federated peer daemons to auto-register at startup.
    ///
    /// Each `[[peer]]` section in `intendant.toml` becomes one
    /// [`PeerConfig`] entry; the daemon hydrates them into the
    /// [`crate::peer::PeerRegistry`] after the web gateway comes up,
    /// so the dashboard shows them as known peers from first load
    /// without the user having to add each one through the UI.
    /// Peers added via the dashboard at runtime live only in the
    /// registry (and the browser's localStorage mirror) — they're
    /// not written back to `intendant.toml` automatically.
    #[serde(default, rename = "peer")]
    pub peers: Vec<PeerConfig>,
}

/// Daemon-level settings for what this Intendant advertises to peers
/// and what it requires of inbound peer connections.
/// Lives under `[server]` in intendant.toml.
///
/// The CLI flag `--advertise-url` (repeatable) is *additive* over the
/// `advertise` field via `web_gateway::resolve_advertise_urls`:
/// operator overrides come first, auto-detected interface URLs append
/// behind them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    /// IP address the web dashboard listens on. Empty/default means the
    /// current wildcard behavior: bind dual-stack `[::]` when available
    /// (accepting IPv4 too), then fall back to `0.0.0.0`.
    ///
    /// Operators should set this to `127.0.0.1` or another specific
    /// interface when using plaintext `--no-tls` for local automation.
    ///
    /// Example:
    /// ```toml
    /// [server]
    /// bind = "127.0.0.1"
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<std::net::IpAddr>,

    /// WebSocket URLs to advertise in this daemon's Agent Card,
    /// in preference order (most-preferred first). Each becomes
    /// an `IntendantWs` transport entry. Empty (the default) means
    /// "rely entirely on auto-detection" — the daemon enumerates
    /// its routable interfaces and emits one URL each.
    ///
    /// Use this when the daemon's local view of its own address
    /// doesn't match how peers reach it: NAT'd VMs reachable via
    /// a host port-forward, Tailscale tailnet URLs, named tunnels,
    /// mTLS proxy URLs, dual-stack IPv4+IPv6, etc.
    ///
    /// Example:
    /// ```toml
    /// [server]
    /// advertise = [
    ///   "wss://192.168.1.42:8765/ws",           # LAN with access certs
    ///   "wss://laptop.tail-abcd.ts.net:8443/ws" # Tailscale fallback
    /// ]
    /// ```
    #[serde(default)]
    pub advertise: Vec<String>,

    /// Inbound auth requirements for federation peers connecting to
    /// this daemon. See [`ServerAuthConfig`].
    #[serde(default)]
    pub auth: ServerAuthConfig,

    /// Native TLS-only mode for the `--web` dashboard. See
    /// [`ServerTlsConfig`]. Off by default because the gateway defaults to
    /// mTLS; enable this to serve HTTPS/WSS without client certificates.
    #[serde(default)]
    pub tls: ServerTlsConfig,

    /// Native client-certificate auth for the `--web` dashboard. See
    /// [`ServerMutualTlsConfig`].
    #[serde(default)]
    pub mtls: ServerMutualTlsConfig,
}

/// Native TLS-only HTTPS/WSS for the `--web` dashboard, lives under
/// `[server.tls]` in intendant.toml. The dashboard defaults to mTLS; enabling
/// this section intentionally disables browser client-certificate auth while
/// keeping HTTPS/WSS.
///
/// When enabled, the gateway's per-connection demux gains a TLS branch:
/// an accepted connection whose first bytes are a TLS ClientHello
/// (record type `0x16`) is wrapped in a `tokio_rustls::TlsAcceptor`, and
/// the decrypted stream then flows through the existing HTTP/WebSocket
/// handling. Raw ICE-TCP (STUN-framed, RFC 4571 length-prefixed) and UDP
/// media are untouched — the first-byte check distinguishes `0x16` (TLS)
/// from the STUN length-prefix/magic-cookie pattern.
///
/// This is the pure-Rust (`rustls` + `rcgen`) path to encrypted serving,
/// available on every platform including Windows — no nginx or OpenSSL
/// dependency. It is independent of
/// [`ServerAuthConfig::advertised_transport`]'s mTLS pinning, which
/// concerns federation peer auth at a proxy layer.
///
/// Example:
/// ```toml
/// [server.tls]
/// enabled = true
/// # optional explicit cert/key (PEM); omit for installed access certs or self-signed fallback
/// # cert = "/etc/intendant/server.crt"
/// # key  = "/etc/intendant/server.key"
/// # extra SAN hostname beyond the bind IP + localhost
/// # hostname = "intendant.local"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerTlsConfig {
    /// Master switch for TLS-only mode. `false` leaves the default mTLS
    /// behavior in place. The CLI `--tls` flag ORs into this at runtime.
    #[serde(default)]
    pub enabled: bool,

    /// Optional path to a PEM-encoded certificate (chain) overriding the
    /// default cert selection. Must be paired with `key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,

    /// Optional path to the PEM-encoded private key matching `cert`.
    /// PKCS#8, PKCS#1 (RSA), or SEC1 (EC) are all accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Optional extra hostname to add to the self-signed cert's SAN list
    /// (in addition to the bind IP and `localhost`). Ignored when an
    /// explicit `cert`/`key` pair is supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// Native browser/client certificate verification for the `--web` dashboard,
/// lives under `[server.mtls]` in intendant.toml. This is the default dashboard
/// transport; the config section remains useful when an operator wants the
/// intent written explicitly or wants to configure a custom CA.
///
/// This is intentionally separate from [`ServerTlsConfig`]: TLS controls
/// encryption and browser secure-context behavior, while mTLS controls client
/// authentication. Enabling this section implies native TLS. When `ca` is not
/// configured, Intendant uses the installed access CA from the platform-specific
/// `intendant access` cert directory.
///
/// Example:
/// ```toml
/// [server.mtls]
/// enabled = true
/// # optional CA override; omit to use the installed Intendant access CA
/// # ca = "~/.intendant/access-certs/ca.crt"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerMutualTlsConfig {
    /// Explicitly require clients to present a certificate signed by the
    /// configured client CA. The CLI `--mtls` flag ORs into this at runtime.
    #[serde(default)]
    pub enabled: bool,

    /// Optional PEM CA bundle for verifying client certificates. When absent,
    /// native mTLS uses the installed Intendant access CA (`ca.crt`) if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
}

/// Advanced compatibility auth this daemon enforces on inbound peer connections.
/// Lives under `[server.auth]` in intendant.toml.
///
/// What this configures: what *peers* must present when connecting
/// to *this* daemon. The advertised counterpart is
/// [`crate::peer::AgentCard.auth`], which tells connecting peers what
/// to send. The two are normally kept consistent by the operator — if
/// `[server.auth] bearer_token` is set, the daemon's Agent Card should
/// advertise `application: Some(Bearer)` so peers know to send it.
/// Prefer dashboard/server mTLS for normal operator-facing access; bearer
/// tokens remain here for legacy deployments and non-browser clients that
/// cannot present a client certificate yet.
///
/// What it does NOT configure: outbound legacy credentials this daemon
/// sends to other peers. Those live on each `[[peer]]` block as
/// `bearer_token = "..."`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerAuthConfig {
    /// When set, inbound HTTP and WebSocket requests must carry
    /// `Authorization: Bearer <token>` matching this exact value.
    /// Missing or wrong token returns 401.
    ///
    /// `None` (the default) means no application-layer requirement.
    /// Prefer mTLS/keycard access for the dashboard and federation. This
    /// field is intentionally kept as an advanced compatibility escape
    /// hatch rather than a first-class UX path.
    ///
    /// Example:
    /// ```toml
    /// [server.auth]
    /// bearer_token = "legacy-advanced-only"
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// What this daemon advertises in its Agent Card's
    /// `auth.transport` field — tells connecting peers what wire-
    /// layer auth to expect. Distinct from `bearer_token` which
    /// covers application-layer auth.
    ///
    /// Accepted values:
    ///
    /// - `"none"` (default) — advertise no transport-layer
    ///   requirement. Right for trusted-LAN federation behind a
    ///   firewall.
    /// - `"mutual-tls"` — advertise plain mTLS. Operator must enable
    ///   native `--mtls` / `[server.mtls]` so the dashboard gateway
    ///   actually enforces client certificates. The card just announces
    ///   the requirement.
    /// - `"pin-self-cert"` — advertise mTLS PLUS pin this daemon's
    ///   server cert by SHA-256 fingerprint. Daemon reads its own
    ///   `server.crt` from the access cert dir at startup, computes
    ///   the fingerprint, and embeds it in the local card's
    ///   `auth.transport = PinnedMutualTls` so connecting peers can
    ///   verify against it without the operator having to compute
    ///   and paste the fingerprint by hand. Fails startup if no
    ///   `server.crt` exists (run `intendant access setup` first).
    ///
    /// Example:
    /// ```toml
    /// [server.auth]
    /// advertised_transport = "pin-self-cert"
    /// ```
    #[serde(default = "default_advertised_transport")]
    pub advertised_transport: String,
}

/// Default value for [`ServerAuthConfig::advertised_transport`] —
/// the no-transport-requirement advertise.
fn default_advertised_transport() -> String {
    "none".to_string()
}

impl Default for ServerAuthConfig {
    /// Manual `Default` impl because the derived one would produce
    /// `advertised_transport = ""` (via `String::default()`), which
    /// `build_local_advertised_auth` rejects as invalid. The serde
    /// `default = "default_advertised_transport"` attribute only
    /// covers deserialization; programmatic construction (in tests
    /// and elsewhere) uses this impl.
    fn default() -> Self {
        Self {
            bearer_token: None,
            advertised_transport: default_advertised_transport(),
        }
    }
}

/// A federated peer daemon advertised via `intendant.toml [[peer]]`.
///
/// `card_url` is the only required field — the registry fetches the
/// peer's Agent Card from that URL at startup, picks a supported
/// transport, and spawns the actor. `label` is an optional display
/// override; when absent the card's own `label` field is used.
/// `bearer_token` is an advanced compatibility credential this daemon
/// sends when connecting out to legacy peers that still require
/// `[server.auth] bearer_token`.
/// `client_cert` / `client_key` are the normal explicit mTLS path for
/// daemon-to-daemon peers when the peer issued this daemon a client identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// URL of the peer's Agent Card. Typically
    /// `https://<host>:<port>/.well-known/agent-card.json` or
    /// `http://<host>:<port>/.well-known/agent-card.json` for
    /// non-TLS local testing.
    pub card_url: String,
    /// Optional display label override. Rendered in the dashboard
    /// Daemons panel instead of `card.label` when set. Does not
    /// affect routing — the registry still keys on `card.id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Outbound bearer token sent to this peer in the
    /// `Authorization: Bearer <token>` header on every HTTP and
    /// WebSocket request. The peer enforces it via its own
    /// `[server.auth] bearer_token`.
    ///
    /// Only set this when the peer's Agent Card advertises
    /// `auth.application = Some(Bearer)`. Normal dashboard and
    /// federation access should use TLS/mTLS client certificates.
    ///
    /// Example:
    /// ```toml
    /// [[peer]]
    /// card_url = "https://wan-peer.example.com/.well-known/agent-card.json"
    /// bearer_token = "legacy-token-from-the-peer-side"
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Peer-issued mTLS client certificate PEM presented when connecting to
    /// this peer over HTTPS/WSS. Must be paired with `client_key`.
    ///
    /// The certificate must chain to the CA the peer configured for
    /// `[server.mtls]` / `--mtls-ca`; a local daemon's own access client cert
    /// only works when the peer trusts that issuing CA. When absent, TLS peer
    /// connections fall back to the installed access `client.crt` / `client.key`
    /// if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,
    /// Private key PEM for `client_cert`. Must be supplied together with
    /// `client_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// Operator-supplied pinned SHA-256 cert fingerprints. When
    /// non-empty, REPLACES whatever the peer's own card declares for
    /// `auth.transport` — eliminates the TOFU window for operators
    /// who got the fingerprint out-of-band (a side channel they
    /// trust more than the card itself, e.g. printed on the peer
    /// machine, sent over Signal, distributed via configuration
    /// management).
    ///
    /// Empty (the default) means "trust whatever the card claims" —
    /// follow-up B's auto-advertise feature on the peer side then
    /// pre-populates the card with the right fingerprint, which
    /// covers most cases. Override here is the explicit "I don't
    /// trust the card's claim, pin against this exact set instead."
    ///
    /// Format: same as the card's wire form — lowercase hex with
    /// optional `:` separators (OpenSSL-style). Parse failures
    /// surface as a `PeerError::Auth` at peer-registration time.
    ///
    /// Example:
    /// ```toml
    /// [[peer]]
    /// card_url = "https://wan-peer.example.com/.well-known/agent-card.json"
    /// pinned_fingerprints = [
    ///   "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
    /// ]
    /// ```
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_fingerprints: Vec<String>,
    /// Explicit URL the **browser** uses to reach this peer's HTTP
    /// port for WebRTC ICE-TCP — decoupled from the via URL the
    /// primary uses for federation.
    ///
    /// Slice 3a.2 wired `advertise_tcp_via_url` from `d.ws_url` (the
    /// primary-side via URL). That works when the browser shares
    /// the primary's network position, but breaks when the primary
    /// reaches the peer via a tunnel endpoint the browser can't use
    /// — most commonly, a `localhost:NNNN` tunnel that's bound on
    /// the primary VM's loopback and invisible to the browser's
    /// machine, or that resolves to `[::1]` on the peer and hits
    /// WKWebView's anti-rebinding filter. This field is the escape
    /// hatch: when set, it replaces `d.ws_url` as the
    /// `advertise_tcp_via_url` sent in the federated WebRTC offer.
    ///
    /// Example (browser on the operator's Mac, primary on a
    /// hypervisor VM, peer reachable from the Mac via a non-loopback
    /// tunnel endpoint):
    /// ```toml
    /// [[peer]]
    /// card_url = "http://localhost:8766/.well-known/agent-card.json"
    /// # Primary reaches the peer via this (loopback on the primary VM):
    /// # — via_urls are CLI / dashboard-add-time only; this config
    /// #   key is for the browser-side URL specifically.
    /// browser_tcp_via_url = "ws://192.168.1.42:8766/ws"
    /// ```
    ///
    /// `None` (the default) means "use the primary-side via URL" —
    /// identical behavior to slice 3a.2 before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_tcp_via_url: Option<String>,
}

/// Recording configuration in intendant.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_framerate")]
    pub framerate: u32,
    #[serde(default = "default_segment_duration")]
    pub segment_duration_secs: u32,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default)]
    pub max_retention_hours: Option<u32>,
}

fn default_framerate() -> u32 {
    15
}
fn default_segment_duration() -> u32 {
    60
}
fn default_quality() -> String {
    "medium".to_string()
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            framerate: default_framerate(),
            segment_duration_secs: default_segment_duration(),
            quality: default_quality(),
            max_retention_hours: None,
        }
    }
}

impl RecordingConfig {
    /// Map quality name to ffmpeg CRF value (lower = higher quality).
    pub fn crf(&self) -> u32 {
        match self.quality.as_str() {
            "low" => 35,
            "high" => 20,
            _ => 28, // medium
        }
    }
}

/// Live audio sub-agent configuration in intendant.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAudioConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_live_timeout")]
    pub default_timeout_secs: u64,
    #[serde(default)]
    pub gemini_model: Option<String>,
    #[serde(default)]
    pub openai_model: Option<String>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
}

fn default_live_timeout() -> u64 {
    300
}
fn default_sample_rate() -> u32 {
    24000
}

impl Default for LiveAudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_timeout_secs: default_live_timeout(),
            gemini_model: None,
            openai_model: None,
            sample_rate: default_sample_rate(),
        }
    }
}

/// Sandbox configuration in intendant.toml.
#[derive(Debug, Default, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SandboxProjectConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub extra_write_paths: Vec<String>,
}

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    pub fn detect() -> Result<Self, CallerError> {
        let root = detect_project_root()?;
        Self::from_root(root)
    }

    /// Build a Project from an explicit root path, loading intendant.toml if present.
    pub fn from_root(root: PathBuf) -> Result<Self, CallerError> {
        let config_path = root.join("intendant.toml");
        let config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).map_err(|e| {
                CallerError::Config(format!("Failed to read intendant.toml: {}", e))
            })?;
            toml::from_str(&content)
                .map_err(|e| CallerError::Toml(format!("Failed to parse intendant.toml: {}", e)))?
        } else {
            ProjectConfig::default()
        };
        Ok(Self { root, config })
    }

    /// Write the current config back to intendant.toml.
    /// Creates the file if it doesn't exist.
    pub fn save_config(&self) -> Result<(), CallerError> {
        let config_path = self.root.join("intendant.toml");
        let content = toml::to_string_pretty(&self.config)
            .map_err(|e| CallerError::Config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(&config_path, content)
            .map_err(|e| CallerError::Config(format!("Failed to write intendant.toml: {}", e)))?;
        Ok(())
    }

    pub fn memory_path(&self) -> PathBuf {
        self.root.join(".intendant").join("memory.json")
    }

    #[allow(dead_code)]
    pub fn agent_dir(&self) -> PathBuf {
        self.root.join(".intendant")
    }

    pub fn sub_agent_dir(&self) -> PathBuf {
        match &self.config.orchestrator.sub_agent_dir {
            Some(dir) => self.root.join(dir),
            None => self.root.join(".intendant").join("subagents"),
        }
    }
}

fn detect_project_root() -> Result<PathBuf, CallerError> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    std::env::current_dir()
        .map_err(|e| CallerError::Config(format!("Failed to get current directory: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_project_config() {
        let config = ProjectConfig::default();
        assert!(config.memory.enabled);
        assert!(config.model.context_window.is_none());
        assert!(config.model.max_output_tokens.is_none());
        assert!(config.orchestrator.max_parallel_agents.is_none());
        assert!(config.orchestrator.sub_agent_dir.is_none());
        assert!(config.server.bind.is_none());
        assert!(config.peers.is_empty());
    }

    #[test]
    fn parse_server_bind_ip() {
        let toml_str = r#"
[server]
bind = "127.0.0.1"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.bind, Some("127.0.0.1".parse().unwrap()));
    }

    /// `[[peer]]` sections parse into `ProjectConfig.peers` via the
    /// `#[serde(rename = "peer")]` attribute on the field. A config
    /// with no peer sections leaves the vec empty (covered by
    /// default_project_config).
    #[test]
    fn parse_peer_sections() {
        let toml_str = r#"
[[peer]]
card_url = "https://nicks-mac.local:8443/.well-known/agent-card.json"
label = "Nick's Mac"

[[peer]]
card_url = "http://127.0.0.1:9000/.well-known/agent-card.json"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.peers.len(), 2);
        assert_eq!(
            config.peers[0].card_url,
            "https://nicks-mac.local:8443/.well-known/agent-card.json"
        );
        assert_eq!(config.peers[0].label.as_deref(), Some("Nick's Mac"));
        assert_eq!(
            config.peers[1].card_url,
            "http://127.0.0.1:9000/.well-known/agent-card.json"
        );
        assert!(config.peers[1].label.is_none());
    }

    /// Round-trip: serializing a config with peer entries back to
    /// TOML produces a string that parses to the same values.
    /// Guards against a future rename or field change breaking the
    /// save path that's used by `Project::save_config`.
    #[test]
    fn peer_config_round_trip_through_toml() {
        let original = ProjectConfig {
            peers: vec![
                PeerConfig {
                    card_url: "http://a.local/.well-known/agent-card.json".into(),
                    label: Some("A".into()),
                    bearer_token: None,
                    client_cert: None,
                    client_key: None,
                    pinned_fingerprints: Vec::new(),
                    browser_tcp_via_url: None,
                },
                PeerConfig {
                    card_url: "http://b.local/.well-known/agent-card.json".into(),
                    label: None,
                    bearer_token: Some("secret-for-b".into()),
                    client_cert: Some("/secrets/b-client.crt".into()),
                    client_key: Some("/secrets/b-client.key".into()),
                    pinned_fingerprints: vec![
                        "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
                    ],
                    browser_tcp_via_url: Some("ws://192.168.1.42:8766/ws".into()),
                },
            ],
            ..ProjectConfig::default()
        };
        let serialized = toml::to_string(&original).unwrap();
        let parsed: ProjectConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.peers.len(), 2);
        assert_eq!(parsed.peers[0].card_url, original.peers[0].card_url);
        assert_eq!(parsed.peers[0].label, original.peers[0].label);
        assert_eq!(parsed.peers[0].bearer_token, original.peers[0].bearer_token);
        assert_eq!(parsed.peers[1].card_url, original.peers[1].card_url);
        assert!(parsed.peers[1].label.is_none());
        assert_eq!(parsed.peers[1].bearer_token, original.peers[1].bearer_token);
        assert_eq!(parsed.peers[1].client_cert, original.peers[1].client_cert);
        assert_eq!(parsed.peers[1].client_key, original.peers[1].client_key);
        assert_eq!(
            parsed.peers[1].pinned_fingerprints,
            original.peers[1].pinned_fingerprints
        );
        assert!(
            parsed.peers[0].pinned_fingerprints.is_empty(),
            "empty pinning preserved across round-trip"
        );
        // Slice 3a.4: browser_tcp_via_url round-trips verbatim.
        assert_eq!(
            parsed.peers[0].browser_tcp_via_url,
            original.peers[0].browser_tcp_via_url
        );
        assert_eq!(
            parsed.peers[1].browser_tcp_via_url,
            original.peers[1].browser_tcp_via_url,
        );
        assert!(
            parsed.peers[0].browser_tcp_via_url.is_none(),
            "None is serialized-then-deserialized as None"
        );
        assert_eq!(
            parsed.peers[1].browser_tcp_via_url.as_deref(),
            Some("ws://192.168.1.42:8766/ws"),
        );
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
[memory]
enabled = true

[model]
context_window = 200000
max_output_tokens = 16384

[orchestrator]
max_parallel_agents = 4
sub_agent_dir = ".custom/agents"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.memory.enabled);
        assert_eq!(config.model.context_window, Some(200_000));
        assert_eq!(config.model.max_output_tokens, Some(16_384));
        assert_eq!(config.orchestrator.max_parallel_agents, Some(4));
        assert_eq!(
            config.orchestrator.sub_agent_dir.as_deref(),
            Some(".custom/agents")
        );
    }

    #[test]
    fn parse_empty_config() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.memory.enabled); // default_true
        assert!(config.model.context_window.is_none());
    }

    #[test]
    fn parse_partial_config() {
        let toml_str = r#"
[memory]
enabled = false
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.memory.enabled);
        assert!(config.model.context_window.is_none());
    }

    #[test]
    fn parse_model_config_only() {
        let toml_str = r#"
[model]
context_window = 128000
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.memory.enabled); // default
        assert_eq!(config.model.context_window, Some(128_000));
        assert!(config.model.max_output_tokens.is_none());
    }

    #[test]
    fn project_paths() {
        let project = Project {
            root: PathBuf::from("/tmp/myproject"),
            config: ProjectConfig::default(),
        };
        assert_eq!(
            project.memory_path(),
            PathBuf::from("/tmp/myproject/.intendant/memory.json")
        );
        assert_eq!(
            project.agent_dir(),
            PathBuf::from("/tmp/myproject/.intendant")
        );
        assert_eq!(
            project.sub_agent_dir(),
            PathBuf::from("/tmp/myproject/.intendant/subagents")
        );
    }

    #[test]
    fn sub_agent_dir_custom() {
        let toml_str = r#"
[orchestrator]
sub_agent_dir = ".custom/agents"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let project = Project {
            root: PathBuf::from("/tmp/myproject"),
            config,
        };
        assert_eq!(
            project.sub_agent_dir(),
            PathBuf::from("/tmp/myproject/.custom/agents")
        );
    }

    #[test]
    fn parse_orchestrator_config() {
        let toml_str = r#"
[orchestrator]
max_parallel_agents = 8
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.orchestrator.max_parallel_agents, Some(8));
        assert!(config.orchestrator.sub_agent_dir.is_none());
    }

    #[test]
    fn parse_approval_config() {
        let toml_str = r#"
[approval]
file_read = "auto"
file_write = "ask"
file_delete = "deny"
command_exec = "auto"
network = "ask"
destructive = "deny"
tool_call = "ask"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.approval.file_read,
            crate::autonomy::ApprovalRule::Auto
        );
        assert_eq!(
            config.approval.file_write,
            crate::autonomy::ApprovalRule::Ask
        );
        assert_eq!(
            config.approval.file_delete,
            crate::autonomy::ApprovalRule::Deny
        );
        assert_eq!(
            config.approval.destructive,
            crate::autonomy::ApprovalRule::Deny
        );
        assert_eq!(
            config.approval.tool_call,
            crate::autonomy::ApprovalRule::Ask
        );
    }

    #[test]
    fn parse_mcp_servers_empty() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn parse_mcp_servers_single() {
        let toml_str = r#"
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "filesystem");
        assert_eq!(config.mcp_servers[0].command, "npx");
        assert_eq!(config.mcp_servers[0].args.len(), 3);
        assert!(config.mcp_servers[0].env.is_empty());
    }

    #[test]
    fn parse_mcp_servers_multiple_with_env() {
        let toml_str = r#"
[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_test123"

[[mcp_servers]]
name = "sqlite"
command = "uvx"
args = ["mcp-server-sqlite", "--db-path", "/tmp/test.db"]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);
        assert_eq!(config.mcp_servers[0].name, "github");
        assert_eq!(
            config.mcp_servers[0].env.get("GITHUB_TOKEN").unwrap(),
            "ghp_test123"
        );
        assert_eq!(config.mcp_servers[1].name, "sqlite");
    }

    #[test]
    fn parse_approval_config_defaults() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert_eq!(
            config.approval.file_read,
            crate::autonomy::ApprovalRule::Auto
        );
        assert_eq!(
            config.approval.file_write,
            crate::autonomy::ApprovalRule::Ask
        );
        assert_eq!(
            config.approval.command_exec,
            crate::autonomy::ApprovalRule::Auto
        );
        assert_eq!(
            config.approval.tool_call,
            crate::autonomy::ApprovalRule::Auto
        );
    }

    #[test]
    fn parse_presence_config_defaults() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.presence.enabled);
        assert!(config.presence.provider.is_none());
        assert!(config.presence.model.is_none());
        assert!(config.presence.live_provider.is_none());
        assert!(config.presence.live_model.is_none());
        assert_eq!(config.presence.context_window, 1_048_576);
        assert_eq!(config.presence.live_context_window, 32_768);
    }

    #[test]
    fn parse_transcription_config_defaults() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.transcription.enabled);
        assert_eq!(config.transcription.provider, "openai");
        assert_eq!(config.transcription.model, "whisper-1");
        assert!(config.transcription.endpoint.is_none());
        assert!(config.transcription.language.is_none());
    }

    #[test]
    fn parse_transcription_config_full() {
        let toml_str = r#"
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
endpoint = "http://localhost:8080/v1/audio/transcriptions"
language = "en"
buffer_secs = 5.0
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.transcription.enabled);
        assert_eq!(config.transcription.model, "whisper-1");
        assert_eq!(
            config.transcription.endpoint.as_deref(),
            Some("http://localhost:8080/v1/audio/transcriptions")
        );
        assert_eq!(config.transcription.language.as_deref(), Some("en"));
    }

    #[test]
    fn parse_presence_config_full() {
        let toml_str = r#"
[presence]
enabled = false
provider = "gemini"
model = "gemini-3-flash-preview"
context_window = 1048576
live_provider = "openai"
live_model = "gpt-4o-realtime-preview"
live_context_window = 65536
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.presence.enabled);
        assert_eq!(config.presence.provider.as_deref(), Some("gemini"));
        assert_eq!(
            config.presence.model.as_deref(),
            Some("gemini-3-flash-preview")
        );
        assert_eq!(config.presence.context_window, 1_048_576);
        assert_eq!(config.presence.live_provider.as_deref(), Some("openai"));
        assert_eq!(
            config.presence.live_model.as_deref(),
            Some("gpt-4o-realtime-preview")
        );
        assert_eq!(config.presence.live_context_window, 65_536);
    }

    #[test]
    fn parse_recording_config_defaults() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(!config.recording.enabled);
        assert_eq!(config.recording.framerate, 15);
        assert_eq!(config.recording.segment_duration_secs, 60);
        assert_eq!(config.recording.quality, "medium");
        assert!(config.recording.max_retention_hours.is_none());
    }

    #[test]
    fn parse_recording_config_full() {
        let toml_str = r#"
[recording]
enabled = true
framerate = 15
segment_duration_secs = 120
quality = "high"
max_retention_hours = 48
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.recording.enabled);
        assert_eq!(config.recording.framerate, 15);
        assert_eq!(config.recording.segment_duration_secs, 120);
        assert_eq!(config.recording.quality, "high");
        assert_eq!(config.recording.max_retention_hours, Some(48));
        assert_eq!(config.recording.crf(), 20);
    }

    #[test]
    fn parse_webrtc_config_defaults() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.webrtc.ice_servers.is_empty());
    }

    #[test]
    fn parse_webrtc_config_stun_only() {
        let toml_str = r#"
[webrtc]
ice_servers = [
    { urls = ["stun:stun.l.google.com:19302"] },
]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.webrtc.ice_servers.len(), 1);
        assert_eq!(
            config.webrtc.ice_servers[0].urls,
            vec!["stun:stun.l.google.com:19302"]
        );
        assert!(config.webrtc.ice_servers[0].username.is_none());
        assert!(config.webrtc.ice_servers[0].credential.is_none());
    }

    #[test]
    fn parse_webrtc_config_stun_and_turn() {
        let toml_str = r#"
[webrtc]
ice_servers = [
    { urls = ["stun:stun.l.google.com:19302"] },
    { urls = ["turn:turn.example.com:3478"], username = "user", credential = "pass" },
]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.webrtc.ice_servers.len(), 2);
        assert_eq!(
            config.webrtc.ice_servers[0].urls,
            vec!["stun:stun.l.google.com:19302"]
        );
        assert_eq!(
            config.webrtc.ice_servers[1].urls,
            vec!["turn:turn.example.com:3478"]
        );
        assert_eq!(
            config.webrtc.ice_servers[1].username.as_deref(),
            Some("user")
        );
        assert_eq!(
            config.webrtc.ice_servers[1].credential.as_deref(),
            Some("pass")
        );
    }

    #[test]
    fn webrtc_config_to_ice_config() {
        let toml_str = r#"
[webrtc]
ice_servers = [
    { urls = ["stun:stun.l.google.com:19302"] },
    { urls = ["turn:turn.example.com:3478"], username = "u", credential = "p" },
]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let ice = config.webrtc.to_ice_config();
        assert_eq!(ice.ice_servers.len(), 2);
        assert_eq!(
            ice.ice_servers[0].urls,
            vec!["stun:stun.l.google.com:19302"]
        );
        assert!(ice.ice_servers[0].username.is_none());
        assert_eq!(ice.ice_servers[1].username.as_deref(), Some("u"));
        assert_eq!(ice.ice_servers[1].credential.as_deref(), Some("p"));
    }

    #[test]
    fn parse_agent_config_backward_compat() {
        let toml_str = r#"
[memory]
enabled = true

[model]
context_window = 200000
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.agent.default_backend.is_none());
        assert_eq!(config.agent.codex.command, "codex");
        assert_eq!(config.agent.codex.approval_policy, "on-request");
        assert_eq!(config.agent.codex.sandbox, "workspace-write");
        assert!(config.agent.codex.model.is_none());
        assert_eq!(config.agent.claude_code.command, "claude");
        assert_eq!(config.agent.claude_code.permission_mode, "auto");
        assert!(config.agent.claude_code.model.is_none());
        assert!(config.agent.claude_code.allowed_tools.is_empty());
    }

    #[test]
    fn parse_agent_config_full() {
        let toml_str = r#"
[agent]
default_backend = "codex"

[agent.codex]
command = "/usr/local/bin/codex"
model = "o4-mini"
approval_policy = "never"
sandbox = "workspace-write"

[agent.claude_code]
command = "/usr/local/bin/claude"
model = "claude-sonnet-4-20250514"
permission_mode = "acceptEdits"
allowed_tools = ["Read", "Edit", "Bash"]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.default_backend.as_deref(), Some("codex"));
        assert_eq!(config.agent.codex.command, "/usr/local/bin/codex");
        assert_eq!(config.agent.codex.model.as_deref(), Some("o4-mini"));
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.sandbox, "workspace-write");
        assert!(config.agent.codex.service_tier.is_none());
        assert_eq!(config.agent.claude_code.command, "/usr/local/bin/claude");
        assert_eq!(
            config.agent.claude_code.model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(config.agent.claude_code.permission_mode, "acceptEdits");
        assert_eq!(
            config.agent.claude_code.allowed_tools,
            vec!["Read", "Edit", "Bash"]
        );
    }

    #[test]
    fn parse_agent_config_minimal_defaults() {
        let toml_str = r#"
[agent]
default_backend = "codex"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.default_backend.as_deref(), Some("codex"));
        assert_eq!(config.agent.codex.command, "codex");
        assert!(config.agent.codex.model.is_none());
        assert_eq!(config.agent.codex.approval_policy, "on-request");
        assert_eq!(config.agent.codex.sandbox, "workspace-write");
        assert_eq!(config.agent.codex.context_archive, "summary");
        assert_eq!(config.agent.claude_code.command, "claude");
        assert!(config.agent.claude_code.model.is_none());
        assert_eq!(config.agent.claude_code.permission_mode, "auto");
        assert!(config.agent.claude_code.allowed_tools.is_empty());
    }

    #[test]
    fn codex_config_defaults() {
        let config = CodexConfig::default();
        assert_eq!(config.command, "codex");
        assert!(config.model.is_none());
        assert_eq!(config.approval_policy, "on-request");
        assert_eq!(config.sandbox, "workspace-write");
        assert_eq!(config.context_archive, "summary");
        assert!(config.service_tier.is_none());
        assert_eq!(normalize_codex_context_archive("raw"), "exact");
        assert_eq!(normalize_codex_context_archive("disabled"), "off");
        assert_eq!(
            normalize_codex_service_tier(Some("fast")).as_deref(),
            Some("priority")
        );
        assert_eq!(
            normalize_codex_service_tier(Some("normal")).as_deref(),
            Some(CODEX_STANDARD_SERVICE_TIER)
        );
        assert_eq!(normalize_codex_service_tier(Some("inherit")), None);
    }
}
