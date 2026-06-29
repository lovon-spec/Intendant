//! CLI-first peer pairing.
//!
//! `intendant peer invite` runs on the daemon that will accept inbound
//! federation connections. It issues a fresh client certificate from that
//! daemon's access CA and packages it with the daemon's Agent Card URL plus
//! server-cert fingerprint.
//!
//! `intendant peer join <invite>` runs on the daemon that will connect out. It
//! stores the peer-issued client identity in the local per-user access cert
//! store and writes/updates a `[[peer]]` block in `intendant.toml`.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::access;
use crate::error::CallerError;
use crate::project::{PeerConfig, Project};

const INVITE_PREFIX: &str = "intendant-peer-v1.";
pub(crate) const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";
const DEFAULT_WEB_PORT: u16 = crate::web_gateway::DEFAULT_PORT;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInvite {
    pub version: u8,
    pub card_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_cert_fingerprint: Option<String>,
    pub client_cert_pem: String,
    pub client_key_pem: String,
    pub issued_at_unix: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct InviteOptions {
    pub card_url: Option<String>,
    pub label: Option<String>,
    pub client_name: Option<String>,
    pub port: u16,
}

impl Default for InviteOptions {
    fn default() -> Self {
        Self {
            card_url: None,
            label: None,
            client_name: None,
            port: DEFAULT_WEB_PORT,
        }
    }
}

#[derive(Debug)]
pub(crate) struct InviteOutcome {
    pub invite: PeerInvite,
    pub encoded: String,
    pub server_cert_fingerprint: String,
}

#[derive(Debug, Default)]
struct PeerArgs {
    action: PeerAction,
    card_url: Option<String>,
    label: Option<String>,
    client_name: Option<String>,
    port: u16,
    invite: Option<String>,
    target_url: Option<String>,
    code_or_id: Option<String>,
    profile: Option<String>,
    requester_card_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerAction {
    Invite,
    Join,
    Request,
    Requests,
    Approve,
    Deny,
    Complete,
    Identities,
    Revoke,
    Help,
}

impl Default for PeerAction {
    fn default() -> Self {
        Self::Help
    }
}

#[derive(Debug)]
pub struct JoinOutcome {
    pub card_url: String,
    pub config_path: PathBuf,
    pub client_cert_path: PathBuf,
    pub client_key_path: PathBuf,
    pub updated_existing: bool,
}

/// Top-level entry invoked from `main()` when argv[1] == "peer".
pub async fn run(argv: Vec<String>) -> Result<(), CallerError> {
    let args = parse_args(&argv)?;
    match args.action {
        PeerAction::Help => {
            print_help();
            Ok(())
        }
        PeerAction::Invite => cmd_invite(args),
        PeerAction::Join => cmd_join(args),
        PeerAction::Request => cmd_request(args).await,
        PeerAction::Requests => cmd_requests(),
        PeerAction::Approve => cmd_approve(args),
        PeerAction::Deny => cmd_deny(args),
        PeerAction::Complete => cmd_complete(args).await,
        PeerAction::Identities => cmd_identities(),
        PeerAction::Revoke => cmd_revoke(args),
    }
}

fn parse_args(argv: &[String]) -> Result<PeerArgs, CallerError> {
    let mut args = PeerArgs {
        port: DEFAULT_WEB_PORT,
        ..PeerArgs::default()
    };

    let Some(first) = argv.first() else {
        return Ok(args);
    };

    args.action = match first.as_str() {
        "invite" => PeerAction::Invite,
        "join" => PeerAction::Join,
        "request" => PeerAction::Request,
        "requests" => PeerAction::Requests,
        "approve" => PeerAction::Approve,
        "deny" => PeerAction::Deny,
        "complete" | "poll" => PeerAction::Complete,
        "identities" | "identity" => PeerAction::Identities,
        "revoke" => PeerAction::Revoke,
        "help" | "-h" | "--help" => return Ok(args),
        other => {
            return Err(CallerError::Config(format!(
                "unknown peer subcommand '{other}' (expected invite/join/request/requests/approve/deny/complete/identities/revoke)"
            )));
        }
    };

    let mut iter = argv.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--card-url" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CallerError::Config("missing value for --card-url".into()))?;
                args.card_url = Some(value.clone());
            }
            "--label" | "--name" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CallerError::Config("missing value for --label".into()))?;
                args.label = Some(value.clone());
            }
            "--client-name" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CallerError::Config("missing value for --client-name".into()))?;
                args.client_name = Some(value.clone());
            }
            "--profile" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CallerError::Config("missing value for --profile".into()))?;
                args.profile = Some(value.clone());
            }
            "--requester-card-url" => {
                let value = iter.next().ok_or_else(|| {
                    CallerError::Config("missing value for --requester-card-url".into())
                })?;
                args.requester_card_url = Some(value.clone());
            }
            "--port" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CallerError::Config("missing value for --port".into()))?;
                args.port = value
                    .parse()
                    .map_err(|_| CallerError::Config(format!("invalid --port value '{value}'")))?;
            }
            "-h" | "--help" => {
                args.action = PeerAction::Help;
                return Ok(args);
            }
            other if other.starts_with('-') => {
                return Err(CallerError::Config(format!("unknown peer flag '{other}'")));
            }
            other => match args.action {
                PeerAction::Join if args.invite.is_none() => {
                    args.invite = Some(other.to_string());
                }
                PeerAction::Request if args.target_url.is_none() => {
                    args.target_url = Some(other.to_string());
                }
                PeerAction::Approve
                | PeerAction::Deny
                | PeerAction::Complete
                | PeerAction::Revoke
                    if args.code_or_id.is_none() =>
                {
                    args.code_or_id = Some(other.to_string());
                }
                PeerAction::Invite => {
                    return Err(CallerError::Config(format!(
                        "unexpected argument '{other}' for `intendant peer invite`"
                    )));
                }
                PeerAction::Join => {
                    return Err(CallerError::Config(format!(
                        "unexpected extra invite argument '{other}'"
                    )));
                }
                PeerAction::Request => {
                    return Err(CallerError::Config(format!(
                        "unexpected extra target argument '{other}'"
                    )));
                }
                PeerAction::Approve | PeerAction::Deny | PeerAction::Complete => {
                    return Err(CallerError::Config(format!(
                        "unexpected extra request argument '{other}'"
                    )));
                }
                PeerAction::Revoke => {
                    return Err(CallerError::Config(format!(
                        "unexpected extra identity argument '{other}'"
                    )));
                }
                PeerAction::Requests | PeerAction::Identities => {
                    return Err(CallerError::Config(format!(
                        "unexpected argument '{other}' for this peer subcommand"
                    )));
                }
                PeerAction::Help => {}
            },
        }
    }

    Ok(args)
}

fn print_help() {
    println!("Intendant peer pairing");
    println!();
    println!("USAGE:");
    println!("    intendant peer invite [--card-url URL] [--label NAME] [--client-name NAME]");
    println!("    intendant peer join <INVITE> [--label NAME]");
    println!("    intendant peer request <TARGET_URL> [--label NAME] [--profile PROFILE]");
    println!("    intendant peer requests");
    println!("    intendant peer approve <CODE_OR_ID> [--profile PROFILE]");
    println!("    intendant peer deny <CODE_OR_ID>");
    println!("    intendant peer complete <REQUEST_ID> [--label NAME]");
    println!("    intendant peer identities");
    println!("    intendant peer revoke <FINGERPRINT_OR_LABEL>");
    println!();
    println!("ACTIONS:");
    println!("    invite        Issue a secret peer invite from this daemon");
    println!("    join          Import an invite and write/update [[peer]] in intendant.toml");
    println!("    request       Ask a target daemon for peer access");
    println!("    requests      List pending incoming peer access requests");
    println!("    approve       Approve an incoming access request and issue its cert");
    println!("    deny          Deny an incoming access request");
    println!("    complete      Poll an outgoing access request and install it if approved");
    println!("    identities    List approved/revoked inbound peer identities");
    println!("    revoke        Revoke an inbound peer identity certificate");
    println!();
    println!("FLAGS:");
    println!("    --card-url <URL>     Agent Card URL to store in the invite");
    println!("                         (also accepts a base http(s) URL or ws(s) /ws URL)");
    println!("    --port <N>           Port for the default Agent Card URL (default 8765)");
    println!("    --label <NAME>       Display label for this peer in the joining daemon");
    println!("    --client-name <NAME> Common name hint for the issued client certificate");
    println!("    --profile <NAME>     Requested or approved peer profile (default peer-operator; use peer-root for full peer access)");
    println!("    --requester-card-url <URL>  Optional Agent Card URL for the requesting daemon");
    println!();
    println!("NOTES:");
    println!("    Run invite on the daemon that will accept inbound peer connections.");
    println!("    Run join on the daemon that should connect to it.");
    println!("    The invite contains a client private key; treat it as a secret.");
}

fn cmd_invite(args: PeerArgs) -> Result<(), CallerError> {
    let outcome = create_invite(InviteOptions {
        card_url: args.card_url,
        label: args.label,
        client_name: args.client_name,
        port: args.port,
    })?;
    let invite = outcome.invite;
    let encoded = outcome.encoded;
    let server_cert_fingerprint = outcome.server_cert_fingerprint;

    println!(":: issued peer invite for {}", invite.card_url);
    println!(":: pinned server cert fingerprint: {server_cert_fingerprint}");
    println!(":: this invite contains a client private key; paste it only to the daemon that should connect");
    println!("{encoded}");
    Ok(())
}

fn cmd_join(args: PeerArgs) -> Result<(), CallerError> {
    let invite_text = match args.invite {
        Some(invite) => invite,
        None if !io::stdin().is_terminal() => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input)?;
            input
        }
        None => {
            return Err(CallerError::Config(
                "`intendant peer join` requires an invite argument or stdin".into(),
            ));
        }
    };
    let invite = decode_invite(invite_text.trim())?;
    let mut project = Project::detect()?;
    let cert_dir = access::backend::select_backend().cert_dir();
    let outcome = join_peer_invite(&mut project, &cert_dir, invite, args.label.as_deref())?;

    let action = if outcome.updated_existing {
        "updated"
    } else {
        "added"
    };
    println!(":: {action} peer {}", outcome.card_url);
    println!(":: wrote {}", outcome.config_path.display());
    println!(":: client cert: {}", outcome.client_cert_path.display());
    println!(":: client key:  {}", outcome.client_key_path.display());
    println!(":: restart or reload the daemon to connect from startup config");
    Ok(())
}

async fn cmd_request(args: PeerArgs) -> Result<(), CallerError> {
    let target_url = args.target_url.ok_or_else(|| {
        CallerError::Config("`intendant peer request` requires a target URL".into())
    })?;
    let cert_dir = access::backend::select_backend().cert_dir();
    let outgoing = crate::peer::access_request::initiate_access_request(
        &cert_dir,
        crate::peer::access_request::InitiateAccessRequestOptions {
            target_url,
            requester_label: args.label,
            requested_profile: args.profile,
            requester_card_url: args.requester_card_url,
        },
    )
    .await?;
    println!(":: access request sent to {}", outgoing.target_card_url);
    println!(":: request id: {}", outgoing.request_id);
    println!(":: approval code: {}", outgoing.code);
    println!(
        ":: approve on the target with: intendant peer approve {}",
        outgoing.code
    );
    println!(
        ":: after approval run: intendant peer complete {}",
        outgoing.request_id
    );
    Ok(())
}

fn cmd_requests() -> Result<(), CallerError> {
    let cert_dir = access::backend::select_backend().cert_dir();
    let requests = crate::peer::access_request::list_requests(&cert_dir)?;
    if requests.is_empty() {
        println!(":: no peer access requests");
        return Ok(());
    }
    for request in requests {
        println!(
            "{}  {:?}  {}  profile={}  expires={}",
            request.code,
            request.status,
            request.requester_label,
            request
                .requested_profile
                .as_deref()
                .unwrap_or(crate::peer::access_policy::DEFAULT_PROFILE),
            request.expires_at_unix
        );
    }
    Ok(())
}

fn cmd_approve(args: PeerArgs) -> Result<(), CallerError> {
    let code = args.code_or_id.ok_or_else(|| {
        CallerError::Config("`intendant peer approve` requires a code or request id".into())
    })?;
    let cert_dir = access::backend::select_backend().cert_dir();
    let request =
        crate::peer::access_request::approve_request(&cert_dir, &code, args.profile.as_deref())?;
    println!(
        ":: approved peer access request {} for {}",
        request.code, request.requester_label
    );
    println!(
        ":: approved profile: {}",
        request
            .approved_profile
            .as_deref()
            .unwrap_or(crate::peer::access_policy::DEFAULT_PROFILE)
    );
    Ok(())
}

fn cmd_deny(args: PeerArgs) -> Result<(), CallerError> {
    let code = args.code_or_id.ok_or_else(|| {
        CallerError::Config("`intendant peer deny` requires a code or request id".into())
    })?;
    let cert_dir = access::backend::select_backend().cert_dir();
    let request = crate::peer::access_request::deny_request(&cert_dir, &code)?;
    println!(
        ":: denied peer access request {} for {}",
        request.code, request.requester_label
    );
    Ok(())
}

async fn cmd_complete(args: PeerArgs) -> Result<(), CallerError> {
    let request_id = args.code_or_id.ok_or_else(|| {
        CallerError::Config("`intendant peer complete` requires a request id".into())
    })?;
    let cert_dir = access::backend::select_backend().cert_dir();
    let mut project = Project::detect()?;
    let outcome = crate::peer::access_request::poll_access_request(
        &mut project,
        &cert_dir,
        &request_id,
        args.label.as_deref(),
    )
    .await?;
    match outcome.install {
        Some(install) => {
            let action = if install.updated_existing {
                "updated"
            } else {
                "added"
            };
            println!(":: {action} peer {}", install.card_url);
            println!(":: wrote {}", install.config_path.display());
            println!(":: client cert: {}", install.client_cert_path.display());
            println!(":: client key:  {}", install.client_key_path.display());
        }
        None => {
            println!(
                ":: request {} is {:?}; approve it on the target first",
                outcome.code, outcome.status
            );
        }
    }
    Ok(())
}

fn cmd_identities() -> Result<(), CallerError> {
    let cert_dir = access::backend::select_backend().cert_dir();
    let identities = crate::peer::access_policy::list_identities(&cert_dir)?;
    if identities.is_empty() {
        println!(":: no inbound peer identities");
        return Ok(());
    }
    for identity in identities {
        println!(
            "{}  {:?}  profile={}  label={}{}{}",
            identity.fingerprint,
            identity.status,
            identity.profile,
            identity.label,
            identity
                .request_id
                .as_deref()
                .map(|id| format!("  request_id={id}"))
                .unwrap_or_default(),
            identity
                .card_url
                .as_deref()
                .map(|url| format!("  card_url={url}"))
                .unwrap_or_default(),
        );
    }
    Ok(())
}

fn cmd_revoke(args: PeerArgs) -> Result<(), CallerError> {
    let identity = args.code_or_id.ok_or_else(|| {
        CallerError::Config("`intendant peer revoke` requires a fingerprint or label".into())
    })?;
    let cert_dir = access::backend::select_backend().cert_dir();
    let record = crate::peer::access_policy::revoke_identity(&cert_dir, &identity)?;
    println!(
        ":: revoked peer identity {} for {}",
        record.fingerprint, record.label
    );
    println!(":: profile was {}", record.profile);
    Ok(())
}

pub(crate) fn create_invite(options: InviteOptions) -> Result<InviteOutcome, CallerError> {
    let cert_dir = access::backend::select_backend().cert_dir();
    create_invite_from_cert_dir(&cert_dir, options)
}

pub(crate) fn create_invite_from_cert_dir(
    cert_dir: &Path,
    options: InviteOptions,
) -> Result<InviteOutcome, CallerError> {
    let label = options.label.unwrap_or_else(access::resolve_host_label);
    let client_name = options
        .client_name
        .unwrap_or_else(|| "Unnamed Intendant daemon".to_string());
    let card_url = match options.card_url {
        Some(url) => normalize_card_url(&url)?,
        None => default_card_url(cert_dir, options.port)?,
    };
    let identity =
        access::certs::issue_client_identity(cert_dir, &client_name).map_err(access_error)?;
    let client_fingerprint = crate::peer::access_policy::fingerprint_pem(&identity.cert_pem)?;
    let server_cert_fingerprint = access::certs::read_server_cert_fingerprint(cert_dir)
        .ok_or_else(|| {
            CallerError::Config(format!(
                "no server.crt found in {} — run `intendant access setup` first",
                cert_dir.display()
            ))
        })?;

    crate::peer::transport::pinning::parse_fingerprint(&server_cert_fingerprint).map_err(|e| {
        CallerError::Config(format!("local server cert fingerprint is invalid: {e}"))
    })?;
    crate::peer::access_policy::write_approved_identity(
        cert_dir,
        &client_fingerprint,
        &client_name,
        crate::peer::access_policy::DEFAULT_PROFILE,
        Some(&card_url),
        None,
    )?;

    let invite = PeerInvite {
        version: 1,
        card_url,
        label: Some(label),
        server_cert_fingerprint: Some(server_cert_fingerprint.clone()),
        client_cert_pem: identity.cert_pem,
        client_key_pem: identity.key_pem,
        issued_at_unix: unix_timestamp(),
    };
    let encoded = encode_invite(&invite)?;
    Ok(InviteOutcome {
        invite,
        encoded,
        server_cert_fingerprint,
    })
}

pub fn encode_invite(invite: &PeerInvite) -> Result<String, CallerError> {
    let json = serde_json::to_vec(invite)?;
    Ok(format!("{}{}", INVITE_PREFIX, URL_SAFE_NO_PAD.encode(json)))
}

pub fn decode_invite(input: &str) -> Result<PeerInvite, CallerError> {
    let trimmed = input.trim();
    let payload = trimmed.strip_prefix(INVITE_PREFIX).unwrap_or(trimmed);
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| CallerError::Config(format!("invalid peer invite encoding: {e}")))?;
    let invite: PeerInvite = serde_json::from_slice(&bytes)?;
    validate_invite(&invite)?;
    Ok(invite)
}

fn validate_invite(invite: &PeerInvite) -> Result<(), CallerError> {
    if invite.version != 1 {
        return Err(CallerError::Config(format!(
            "unsupported peer invite version {}",
            invite.version
        )));
    }
    if invite.card_url.trim().is_empty() {
        return Err(CallerError::Config(
            "peer invite has an empty card_url".into(),
        ));
    }
    if invite.client_cert_pem.trim().is_empty() || invite.client_key_pem.trim().is_empty() {
        return Err(CallerError::Config(
            "peer invite is missing the client certificate or key".into(),
        ));
    }
    if let Some(fp) = &invite.server_cert_fingerprint {
        crate::peer::transport::pinning::parse_fingerprint(fp).map_err(|e| {
            CallerError::Config(format!("peer invite has invalid server fingerprint: {e}"))
        })?;
    }
    Ok(())
}

pub(crate) fn join_peer_invite(
    project: &mut Project,
    cert_dir: &Path,
    invite: PeerInvite,
    label_override: Option<&str>,
) -> Result<JoinOutcome, CallerError> {
    validate_invite(&invite)?;

    let peer_dir = cert_dir
        .join("peers")
        .join(storage_slug(invite.label.as_deref(), &invite.card_url));
    std::fs::create_dir_all(&peer_dir)?;

    let cert_path = peer_dir.join("client.crt");
    let key_path = peer_dir.join("client.key");
    std::fs::write(&cert_path, invite.client_cert_pem.as_bytes())?;
    write_secret_file(&key_path, &invite.client_key_pem)?;

    let label = label_override
        .map(str::to_string)
        .or_else(|| invite.label.clone());
    let pins = invite
        .server_cert_fingerprint
        .clone()
        .map(|fp| vec![fp])
        .unwrap_or_default();

    let existing = project
        .config
        .peers
        .iter_mut()
        .find(|peer| peer.card_url == invite.card_url);
    let updated_existing = existing.is_some();
    match existing {
        Some(peer) => {
            if label.is_some() {
                peer.label = label;
            }
            peer.client_cert = Some(cert_path.to_string_lossy().into_owned());
            peer.client_key = Some(key_path.to_string_lossy().into_owned());
            if !pins.is_empty() {
                peer.pinned_fingerprints = pins;
            }
        }
        None => {
            project.config.peers.push(PeerConfig {
                card_url: invite.card_url.clone(),
                label,
                bearer_token: None,
                via_urls: Vec::new(),
                client_cert: Some(cert_path.to_string_lossy().into_owned()),
                client_key: Some(key_path.to_string_lossy().into_owned()),
                pinned_fingerprints: pins,
                browser_tcp_via_url: None,
            });
        }
    }

    project.save_config()?;
    Ok(JoinOutcome {
        card_url: invite.card_url,
        config_path: project.root.join("intendant.toml"),
        client_cert_path: cert_path,
        client_key_path: key_path,
        updated_existing,
    })
}

fn default_card_url(cert_dir: &Path, port: u16) -> Result<String, CallerError> {
    let host = access::certs::current_cert_ip(cert_dir).map_err(access_error)?;
    Ok(format!(
        "https://{}:{}{}",
        url_host(&host),
        port,
        AGENT_CARD_PATH
    ))
}

pub(crate) fn normalize_card_url(raw: &str) -> Result<String, CallerError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(CallerError::Config("--card-url cannot be empty".into()));
    }

    let http = if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{}", trim_ws_path(rest))
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{}", trim_ws_path(rest))
    } else if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        trimmed.to_string()
    } else {
        return Err(CallerError::Config(format!(
            "--card-url must start with http://, https://, ws://, or wss:// (got {raw:?})"
        )));
    };

    if http.ends_with(AGENT_CARD_PATH) {
        return Ok(http);
    }
    let base = http
        .strip_suffix("/ws")
        .unwrap_or(&http)
        .trim_end_matches('/');
    Ok(format!("{base}{AGENT_CARD_PATH}"))
}

fn trim_ws_path(rest: &str) -> &str {
    rest.strip_suffix("/ws")
        .unwrap_or(rest)
        .trim_end_matches('/')
}

fn url_host(host: &str) -> String {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_string(),
    }
}

pub(crate) fn storage_slug(label: Option<&str>, card_url: &str) -> String {
    let raw = label
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| storage_name_from_card_url(card_url));
    let mut sanitized = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
        if sanitized.len() >= 40 {
            break;
        }
    }
    let sanitized = sanitized.trim_matches('-');
    let base = if sanitized.is_empty() {
        "peer"
    } else {
        sanitized
    };
    format!("{base}-{}", &sha256_hex(card_url.as_bytes())[..12])
}

fn storage_name_from_card_url(card_url: &str) -> String {
    let without_scheme = card_url
        .strip_prefix("https://")
        .or_else(|| card_url.strip_prefix("http://"))
        .unwrap_or(card_url);
    without_scheme
        .split('/')
        .next()
        .unwrap_or("peer")
        .trim_matches(|c| c == '[' || c == ']')
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(crate) fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) fn write_secret_file(path: &Path, contents: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents.as_bytes())?;
    }
    Ok(())
}

fn access_error(err: access::AccessError) -> CallerError {
    CallerError::Config(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::certs::{ensure_certs, read_server_cert_fingerprint, ServerNames};
    use crate::project::ProjectConfig;
    use tempfile::TempDir;

    fn invite() -> PeerInvite {
        PeerInvite {
            version: 1,
            card_url: "https://peer.example/.well-known/agent-card.json".into(),
            label: Some("Peer Example".into()),
            server_cert_fingerprint: Some(
                "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
            ),
            client_cert_pem: "-----BEGIN CERTIFICATE-----\npeer\n-----END CERTIFICATE-----\n"
                .into(),
            client_key_pem: "-----BEGIN PRIVATE KEY-----\npeer\n-----END PRIVATE KEY-----\n".into(),
            issued_at_unix: 1,
        }
    }

    fn names(ip: &str) -> ServerNames {
        ServerNames::new(
            ip.parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap()
    }

    #[test]
    fn invite_round_trips_with_prefix() {
        let original = invite();
        let encoded = encode_invite(&original).unwrap();
        assert!(encoded.starts_with(INVITE_PREFIX));
        assert_eq!(decode_invite(&encoded).unwrap(), original);
    }

    #[test]
    fn normalize_card_url_accepts_base_and_ws_forms() {
        assert_eq!(
            normalize_card_url("https://host.test:8765").unwrap(),
            "https://host.test:8765/.well-known/agent-card.json"
        );
        assert_eq!(
            normalize_card_url("wss://host.test:8765/ws").unwrap(),
            "https://host.test:8765/.well-known/agent-card.json"
        );
        assert_eq!(
            normalize_card_url("http://host.test:8765/.well-known/agent-card.json").unwrap(),
            "http://host.test:8765/.well-known/agent-card.json"
        );
    }

    #[test]
    fn default_card_url_uses_access_cert_primary_ip() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.9"), "peer", false).unwrap();
        assert_eq!(
            default_card_url(tmp.path(), 8766).unwrap(),
            "https://10.0.0.9:8766/.well-known/agent-card.json"
        );
    }

    #[test]
    fn create_invite_issues_client_identity_and_server_pin() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.9"), "peer", false).unwrap();

        let outcome = create_invite_from_cert_dir(
            tmp.path(),
            InviteOptions {
                card_url: Some("wss://peer.example:8765/ws".into()),
                label: Some("Peer Example".into()),
                client_name: Some("dashboard pairing".into()),
                ..InviteOptions::default()
            },
        )
        .unwrap();
        let decoded = decode_invite(&outcome.encoded).unwrap();

        assert_eq!(decoded, outcome.invite);
        assert_eq!(
            decoded.card_url,
            "https://peer.example:8765/.well-known/agent-card.json"
        );
        assert_eq!(decoded.label.as_deref(), Some("Peer Example"));
        assert!(decoded.client_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(decoded.client_key_pem.contains("BEGIN PRIVATE KEY"));
        assert_eq!(
            decoded.server_cert_fingerprint.as_deref(),
            Some(outcome.server_cert_fingerprint.as_str())
        );
    }

    #[test]
    fn invite_fingerprint_matches_access_server_cert() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.9"), "peer", false).unwrap();
        let fp = read_server_cert_fingerprint(tmp.path()).unwrap();
        crate::peer::transport::pinning::parse_fingerprint(&fp).unwrap();
    }

    #[test]
    fn join_invite_adds_peer_config_and_writes_identity_files() {
        let root = TempDir::new().unwrap();
        let certs = TempDir::new().unwrap();
        let mut project = Project {
            root: root.path().to_path_buf(),
            config: ProjectConfig::default(),
        };

        let outcome =
            join_peer_invite(&mut project, certs.path(), invite(), Some("Override")).unwrap();

        assert!(!outcome.updated_existing);
        assert!(outcome.client_cert_path.exists());
        assert!(outcome.client_key_path.exists());
        assert!(root.path().join("intendant.toml").exists());
        assert_eq!(project.config.peers.len(), 1);
        let peer = &project.config.peers[0];
        assert_eq!(
            peer.card_url,
            "https://peer.example/.well-known/agent-card.json"
        );
        assert_eq!(peer.label.as_deref(), Some("Override"));
        assert_eq!(
            peer.client_cert.as_deref(),
            Some(outcome.client_cert_path.to_str().unwrap())
        );
        assert_eq!(
            peer.client_key.as_deref(),
            Some(outcome.client_key_path.to_str().unwrap())
        );
        assert_eq!(peer.pinned_fingerprints.len(), 1);
    }

    #[test]
    fn join_invite_updates_existing_peer_without_losing_legacy_fields() {
        let root = TempDir::new().unwrap();
        let certs = TempDir::new().unwrap();
        let mut project = Project {
            root: root.path().to_path_buf(),
            config: ProjectConfig {
                peers: vec![PeerConfig {
                    card_url: "https://peer.example/.well-known/agent-card.json".into(),
                    label: Some("Old".into()),
                    bearer_token: Some("legacy".into()),
                    via_urls: Vec::new(),
                    client_cert: None,
                    client_key: None,
                    pinned_fingerprints: Vec::new(),
                    browser_tcp_via_url: Some("ws://browser-via/ws".into()),
                }],
                ..ProjectConfig::default()
            },
        };

        let outcome = join_peer_invite(&mut project, certs.path(), invite(), None).unwrap();

        assert!(outcome.updated_existing);
        assert_eq!(project.config.peers.len(), 1);
        let peer = &project.config.peers[0];
        assert_eq!(peer.label.as_deref(), Some("Peer Example"));
        assert_eq!(peer.bearer_token.as_deref(), Some("legacy"));
        assert_eq!(
            peer.browser_tcp_via_url.as_deref(),
            Some("ws://browser-via/ws")
        );
        assert!(peer.client_cert.is_some());
        assert!(peer.client_key.is_some());
        assert_eq!(peer.pinned_fingerprints.len(), 1);
    }
}
